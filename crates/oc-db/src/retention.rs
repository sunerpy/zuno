//! Age-based session retention selection without mutation.
//!
//! This module only decides which session rows form safe, descendant-closed
//! candidate sets. It deliberately performs no deletion: mutation, artifact
//! cleanup, confirmation, and preview rendering belong to the service layer.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use oc_error::DbError;
use rusqlite::Connection;

use crate::open;

/// Milliseconds in one day.
pub const DAY_MILLIS: i64 = 86_400_000;

/// The no-server grace period applied to recently touched sessions.
pub const DEFAULT_RECENCY_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Which projects may provide age-eligible roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionScope {
    /// The project resolved from the caller's current directory.
    CurrentProject(String),
    /// One explicitly named and resolved project.
    Project(String),
    /// Every project in the database.
    AllProjects,
}

/// Which timestamp decides whether a session is old enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetentionKey {
    /// Last activity, the safe default.
    #[default]
    Updated,
    /// Creation time, regardless of later activity.
    Created,
}

/// One selector invocation.
#[derive(Debug, Clone)]
pub struct RetentionRequest {
    /// Sessions strictly older than this many whole days are age-eligible.
    pub older_than_days: u64,
    /// Which projects may provide age-eligible roots.
    pub scope: RetentionScope,
    /// Timestamp used for the age predicate.
    pub key: RetentionKey,
    /// Clock value used for deterministic cutoffs, in Unix milliseconds.
    pub now_ms: i64,
    /// Grace period used only when no server can report active sessions.
    pub recency_window: Duration,
    /// Permit sessions carrying a public share URL.
    pub include_shared: bool,
    /// Permit sessions whose compaction marker is set.
    pub include_compacting: bool,
    /// Permit recently touched sessions when no server is reachable.
    pub include_recent: bool,
}

impl RetentionRequest {
    /// Build a request with every protection enabled.
    #[must_use]
    pub fn new(older_than_days: u64, scope: RetentionScope, now_ms: i64) -> Self {
        Self {
            older_than_days,
            scope,
            key: RetentionKey::Updated,
            now_ms,
            recency_window: DEFAULT_RECENCY_WINDOW,
            include_shared: false,
            include_compacting: false,
            include_recent: false,
        }
    }

    /// Use creation time for the age predicate.
    #[must_use]
    pub fn created(mut self) -> Self {
        self.key = RetentionKey::Created;
        self
    }

    /// Allow published sessions to be selected.
    #[must_use]
    pub fn including_shared(mut self) -> Self {
        self.include_shared = true;
        self
    }

    /// Allow sessions currently marked as compacting to be selected.
    #[must_use]
    pub fn including_compacting(mut self) -> Self {
        self.include_compacting = true;
        self
    }

    /// Cross the no-server recency guard.
    #[must_use]
    pub fn including_recent(mut self) -> Self {
        self.include_recent = true;
        self
    }
}

/// What a local-server probe could establish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// At least one server answered; these ids are active in the responding
    /// process or processes.
    Reachable {
        /// Session ids reported by `/api/session/active`.
        active_session_ids: BTreeSet<String>,
    },
    /// No server answered, so the selector must fall back to recency.
    Unreachable,
}

/// Injectable boundary for `/api/session/active` discovery.
///
/// The database cannot answer whether a session is running. Implementations at
/// the process edge may probe one or more local servers and aggregate their ids;
/// tests can supply a deterministic fake.
pub trait LivenessProbe {
    /// Probe local servers once for this selection pass.
    fn probe(&self) -> Liveness;
}

/// Why a row is protected from selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtectionReason {
    /// `share_url` is set; crossable with `--include-shared`.
    Shared,
    /// `time_compacting` is set; crossable with `--include-compacting`.
    Compacting,
    /// A reachable server reported the id active.
    Active,
    /// No server was reachable and the row was touched inside the grace period.
    Recent {
        /// The row's `time_updated` value.
        time_updated: i64,
        /// Inclusive lower bound of the protected recency window.
        cutoff_ms: i64,
    },
}

/// Why an age-eligible row was excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusionReason {
    /// The candidate row itself is protected.
    Protected(ProtectionReason),
    /// Selecting the candidate would strand a protected descendant.
    ProtectedDescendant {
        /// Descendant that vetoed the candidate subtree.
        descendant_id: String,
        /// Every protection applying to that descendant.
        protections: Vec<ProtectionReason>,
    },
}

/// One age-eligible row refused by the selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionExclusion {
    /// Candidate id.
    pub id: String,
    /// Direct or descendant protections that vetoed it.
    pub reasons: Vec<ExclusionReason>,
}

/// Why a row belongs to the selected set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionReason {
    /// The row independently passed the age predicate.
    AgeThreshold {
        /// Timestamp key used by the request.
        key: RetentionKey,
        /// Value read from that key.
        timestamp: i64,
        /// Exclusive upper bound used by the age predicate.
        cutoff_ms: i64,
    },
    /// The row is required to close another candidate's subtree.
    DescendantOf {
        /// Age-eligible subtree root that pulled this row in.
        candidate_id: String,
    },
}

/// One row in the descendant-closed selected set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionCandidate {
    /// Session id.
    pub id: String,
    /// Every reason accumulated across overlapping candidate subtrees.
    pub reasons: Vec<SelectionReason>,
}

/// Complete selector output suitable for a preview surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionReport {
    /// Exclusive upper bound used by the age predicate.
    pub age_cutoff_ms: i64,
    /// Liveness evidence used for this pass.
    pub liveness: Liveness,
    /// Safe rows, closed under transitive descendants.
    pub selected: Vec<RetentionCandidate>,
    /// Age-eligible roots refused by direct or descendant protection.
    pub excluded: Vec<RetentionExclusion>,
}

/// Select safe retention candidates without mutating the database.
///
/// # Errors
///
/// [`DbError::Query`] if session rows cannot be read.
pub fn select(
    connection: &Connection,
    request: &RetentionRequest,
    probe: &impl LivenessProbe,
) -> Result<RetentionReport, DbError> {
    let age_ms = i64::try_from(request.older_than_days)
        .unwrap_or(i64::MAX)
        .saturating_mul(DAY_MILLIS);
    let age_cutoff_ms = request.now_ms.saturating_sub(age_ms);
    let liveness = probe.probe();
    let rows = read_rows(connection)?;
    let children = child_index(&rows);
    let eligible = age_eligible(&rows, request, age_cutoff_ms);
    let mut selected: BTreeMap<String, RetentionCandidate> = BTreeMap::new();
    let mut excluded = Vec::new();

    for candidate_id in eligible {
        let subtree = descendant_ids(&candidate_id, &children);
        let mut reasons = protection_reasons(rows.get(&candidate_id), request, &liveness)
            .into_iter()
            .map(ExclusionReason::Protected)
            .collect::<Vec<_>>();

        for descendant_id in subtree.iter().filter(|id| *id != &candidate_id) {
            let protections = protection_reasons(rows.get(descendant_id), request, &liveness);
            if !protections.is_empty() {
                reasons.push(ExclusionReason::ProtectedDescendant {
                    descendant_id: descendant_id.clone(),
                    protections,
                });
            }
        }

        if !reasons.is_empty() {
            excluded.push(RetentionExclusion {
                id: candidate_id,
                reasons,
            });
            continue;
        }

        for id in subtree {
            let candidate = selected
                .entry(id.clone())
                .or_insert_with(|| RetentionCandidate {
                    id: id.clone(),
                    reasons: Vec::new(),
                });
            let reason = if id == candidate_id {
                let row = &rows[&id];
                SelectionReason::AgeThreshold {
                    key: request.key,
                    timestamp: row.retention_timestamp(request.key),
                    cutoff_ms: age_cutoff_ms,
                }
            } else {
                SelectionReason::DescendantOf {
                    candidate_id: candidate_id.clone(),
                }
            };
            if !candidate.reasons.contains(&reason) {
                candidate.reasons.push(reason);
            }
        }
    }

    Ok(RetentionReport {
        age_cutoff_ms,
        liveness,
        selected: selected.into_values().collect(),
        excluded,
    })
}

#[derive(Debug)]
struct RetentionRow {
    id: String,
    project_id: String,
    parent_id: Option<String>,
    share_url: Option<String>,
    time_created: i64,
    time_updated: i64,
    time_compacting: Option<i64>,
}

impl RetentionRow {
    fn retention_timestamp(&self, key: RetentionKey) -> i64 {
        match key {
            RetentionKey::Updated => self.time_updated,
            RetentionKey::Created => self.time_created,
        }
    }
}

fn read_rows(connection: &Connection) -> Result<BTreeMap<String, RetentionRow>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT id, project_id, parent_id, share_url, time_created, time_updated, \
             time_compacting FROM session ORDER BY id ASC",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok(RetentionRow {
                id: row.get(0)?,
                project_id: row.get(1)?,
                parent_id: row.get(2)?,
                share_url: row.get(3)?,
                time_created: row.get(4)?,
                time_updated: row.get(5)?,
                time_compacting: row.get(6)?,
            })
        })
        .map_err(open::map_error)?;
    let mut result = BTreeMap::new();
    for row in rows {
        let row = row.map_err(open::map_error)?;
        result.insert(row.id.clone(), row);
    }
    Ok(result)
}

fn child_index(rows: &BTreeMap<String, RetentionRow>) -> BTreeMap<String, Vec<String>> {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows.values() {
        if let Some(parent_id) = &row.parent_id {
            children
                .entry(parent_id.clone())
                .or_default()
                .push(row.id.clone());
        }
    }
    children
}

fn age_eligible(
    rows: &BTreeMap<String, RetentionRow>,
    request: &RetentionRequest,
    cutoff_ms: i64,
) -> Vec<String> {
    rows.values()
        .filter(|row| scope_contains(&request.scope, &row.project_id))
        .filter(|row| row.retention_timestamp(request.key) < cutoff_ms)
        .map(|row| row.id.clone())
        .collect()
}

fn scope_contains(scope: &RetentionScope, project_id: &str) -> bool {
    match scope {
        RetentionScope::CurrentProject(expected) | RetentionScope::Project(expected) => {
            expected == project_id
        }
        RetentionScope::AllProjects => true,
    }
}

fn descendant_ids(root: &str, children: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.to_owned()];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(descendants) = children.get(&id) {
            stack.extend(descendants.iter().rev().cloned());
        }
    }
    seen.into_iter().collect()
}

fn protection_reasons(
    row: Option<&RetentionRow>,
    request: &RetentionRequest,
    liveness: &Liveness,
) -> Vec<ProtectionReason> {
    let Some(row) = row else {
        return Vec::new();
    };
    let mut reasons = Vec::new();
    if row.share_url.is_some() && !request.include_shared {
        reasons.push(ProtectionReason::Shared);
    }
    if row.time_compacting.is_some() && !request.include_compacting {
        reasons.push(ProtectionReason::Compacting);
    }
    match liveness {
        Liveness::Reachable { active_session_ids } => {
            if active_session_ids.contains(&row.id) {
                reasons.push(ProtectionReason::Active);
            }
        }
        Liveness::Unreachable if !request.include_recent => {
            let window_ms = i64::try_from(request.recency_window.as_millis()).unwrap_or(i64::MAX);
            let cutoff_ms = request.now_ms.saturating_sub(window_ms);
            if row.time_updated >= cutoff_ms {
                reasons.push(ProtectionReason::Recent {
                    time_updated: row.time_updated,
                    cutoff_ms,
                });
            }
        }
        Liveness::Unreachable => {}
    }
    reasons
}
