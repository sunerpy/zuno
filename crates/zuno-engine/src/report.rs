//! Render-time projection of one batch of settled reports.
//!
//! A parent commonly holds several settled reports at once: a fan-out settles together,
//! and restart recovery re-admits reports for work that already settled. Whichever path
//! delivers that batch — the idle wake that drives it as one turn, or the wake that
//! offers it to a running turn — the model must be able to tell which report is the
//! current state of its work. That decision lives here so every delivery path reaches
//! the same conclusion from the same durable rows instead of each surface inventing one.
//!
//! Grouping is a projection, not a durable claim: no inbox row is merged, dropped,
//! reordered, or given a new state.

use std::collections::BTreeMap;

use serde_json::Value;
use zuno_db::inbox::{DurableInputKind, SessionInput};

use crate::planning::PlanningInputSource;

/// One settled report as a batched delivery renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedReport {
    /// Durable inbox id, reused as this report's user message id.
    pub input_id: String,
    /// Exact model-visible text, including its batch annotation when one applies.
    pub text: String,
    /// Which planning origin this report seeds.
    pub source: PlanningInputSource,
    /// Whether this report is the newest terminal state the whole batch carries.
    ///
    /// Plan reconciliation is seeded from it, so a superseded state cannot reopen work
    /// the newest report already finished.
    pub newest: bool,
}

/// The projection of one batch of settled reports onto what the model reads.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReportBatch {
    reports: Vec<ProjectedReport>,
    undecodable: Vec<String>,
}

impl ReportBatch {
    /// Project durable rows, in admission order, onto what the model will read.
    ///
    /// Rows that are not settled reports belong to another driver and are left out of
    /// the batch entirely. A settled report whose durable prompt carries no
    /// model-visible text cannot become a user message and is reported through
    /// [`Self::undecodable`] instead.
    ///
    /// Rows describing the same work are compared by the instant that work completed,
    /// and only the newest of each group is presented as current, so a batch carrying
    /// three snapshots of one job stops reading as three live states the parent must
    /// chase.
    #[must_use]
    pub fn project(rows: &[SessionInput]) -> Self {
        let mut reports = Vec::new();
        let mut undecodable = Vec::new();
        let mut identities = Vec::new();
        let mut completions = Vec::new();
        for input in rows {
            let Some(kind) = DurableInputKind::classify(&input.prompt) else {
                continue;
            };
            if !kind.is_asynchronous_report() {
                continue;
            }
            let Some(text) = kind.plain_text(&input.prompt) else {
                undecodable.push(input.id.clone());
                continue;
            };
            identities.push(report_work_identity(&input.prompt));
            completions.push((input.time_created, input.admitted_sequence));
            reports.push(ProjectedReport {
                input_id: input.id.clone(),
                text: text.to_owned(),
                source: report_planning_source(kind),
                newest: false,
            });
        }
        let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (index, identity) in identities.iter().enumerate() {
            if let Some(identity) = identity {
                groups.entry(identity.as_str()).or_default().push(index);
            }
        }
        for (work, members) in &groups {
            if members.len() < 2 {
                continue;
            }
            let newest = members
                .iter()
                .copied()
                .max_by_key(|index| completions[*index])
                .expect("a grouped batch member exists");
            let size = members.len();
            for index in members.iter().copied() {
                let annotation = if index == newest {
                    format!(
                        "[current report] Newest of {size} reports for work `{work}` in this delivery."
                    )
                } else {
                    format!(
                        "[superseded report] Work `{work}` reported again later in this same \
                         delivery; the report marked current for `{work}` is its state now."
                    )
                };
                reports[index].text = format!("{annotation}\n\n{}", reports[index].text);
            }
        }
        if let Some(seed) = (0..reports.len()).max_by_key(|index| completions[*index]) {
            reports[seed].newest = true;
        }
        Self {
            reports,
            undecodable,
        }
    }

    /// The batch in admission order, as the delivery persists and drives it.
    #[must_use]
    pub fn reports(&self) -> &[ProjectedReport] {
        &self.reports
    }

    /// Rows that cannot become user messages and must be settled failed.
    #[must_use]
    pub fn undecodable(&self) -> &[String] {
        &self.undecodable
    }

    /// Whether this batch has nothing the model can read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reports.is_empty()
    }
}

/// The work one settled report describes, when its durable prompt names it.
///
/// Two rows sharing this identity are the same work at different instants: settlement
/// and restart recovery each admit a report for one job, so a parent can hold several
/// rows whose newest member is the only current state. A row that names no work is its
/// own group and is rendered exactly as its writer wrote it.
fn report_work_identity(prompt: &Value) -> Option<String> {
    ["jobID", "executionID"]
        .into_iter()
        .find_map(|field| prompt.get(field).and_then(Value::as_str))
        .map(str::to_owned)
}

/// Which planning origin a settled report of this kind seeds.
///
/// A process-owned background execution is the parent's own work continuing; every other
/// settled report describes a delegated child. Neither may create a Plan.
const fn report_planning_source(kind: DurableInputKind) -> PlanningInputSource {
    if matches!(kind, DurableInputKind::BackgroundExecutionReport) {
        PlanningInputSource::BackgroundReport
    } else {
        PlanningInputSource::ChildReport
    }
}
