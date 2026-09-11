use crate::{LearningIngestion, LearningScheduleOutcome, LearningScheduler};
use serde::Serialize;
use zuno_db::learning_job::{LearningJobStatus, MAX_LEARNING_HISTORY_BATCH};
use zuno_error::DbError;
use zuno_types::SessionMemoryGeneration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningHistoryAction {
    Revalidated,
    WouldRevalidate,
    Queued,
    WouldQueue,
    Existing,
    Excluded,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningHistoryItem {
    pub job_id: String,
    pub source_message_id: Option<String>,
    pub action: LearningHistoryAction,
    pub revalidated_experiences: usize,
    pub unverified_experiences: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningHistoryRepair {
    pub dry_run: bool,
    pub examined: usize,
    pub revalidated_experiences: usize,
    pub queued: usize,
    pub would_queue: usize,
    pub unavailable: usize,
    pub excluded: usize,
    pub has_more: bool,
    pub items: Vec<LearningHistoryItem>,
}

impl LearningIngestion {
    /// Request a bounded version upgrade for an exact assistant message in this
    /// project. A current job is returned unchanged, even if it failed.
    pub fn reprocess(
        &self,
        project_id: &str,
        assistant_message_id: &str,
        scheduler: &LearningScheduler,
        now: i64,
        redact: &dyn Fn(&str) -> String,
    ) -> crate::Result<LearningScheduleOutcome> {
        if !scheduler.generates() {
            return Ok(LearningScheduleOutcome::Disabled);
        }
        let session_id = self
            .sources
            .session_for_message(project_id, assistant_message_id)?
            .ok_or_else(|| {
                crate::model::invalid("assistant message is missing from this project")
            })?;
        match self.jobs.generation_for_session(&session_id)? {
            SessionMemoryGeneration::Disabled => return Ok(LearningScheduleOutcome::Disabled),
            SessionMemoryGeneration::Excluded => return Ok(LearningScheduleOutcome::Excluded),
            SessionMemoryGeneration::Enabled => {}
        }
        if self
            .jobs
            .source_was_forgotten(&session_id, assistant_message_id)?
        {
            return Ok(LearningScheduleOutcome::Excluded);
        }
        if let Some(job) = self.jobs.extraction_for_source(
            project_id,
            assistant_message_id,
            scheduler.extractor_version(),
        )? {
            return Ok(LearningScheduleOutcome::Existing(job));
        }
        let (request, signals) =
            self.request(project_id, &session_id, assistant_message_id, false, redact)?;
        if request.sources.is_empty() {
            return Ok(LearningScheduleOutcome::Ineligible);
        }
        if signals.external_context && scheduler.excludes_external_context() {
            return Ok(LearningScheduleOutcome::Excluded);
        }
        scheduler.schedule_reprocess(request, now)
    }

    /// Revalidate and, where needed, queue at most 32 legacy jobs. Dry-run follows
    /// the same evidence checks without changing flags, payloads, queues or files.
    pub fn repair_history(
        &self,
        project_id: &str,
        scheduler: &LearningScheduler,
        now: i64,
        dry_run: bool,
        redact: &dyn Fn(&str) -> String,
    ) -> crate::Result<LearningHistoryRepair> {
        let mut report = LearningHistoryRepair {
            dry_run,
            ..Default::default()
        };
        if !scheduler.generates() {
            return Ok(report);
        }
        let batch = self.jobs.legacy_for_project(
            project_id,
            scheduler.extractor_version(),
            MAX_LEARNING_HISTORY_BATCH,
        )?;
        report.has_more = batch.has_more;
        for job in batch.jobs {
            report.examined += 1;
            let mut item = LearningHistoryItem {
                job_id: job.id.clone(),
                source_message_id: job.source_message_id.clone(),
                action: LearningHistoryAction::Unavailable,
                revalidated_experiences: 0,
                unverified_experiences: 0,
            };
            let mut expected_updated = job.time_updated;
            let identities = job
                .session_id
                .as_deref()
                .zip(job.source_message_id.as_deref());
            let (session, message) = match identities {
                Some(ids) => ids,
                None => {
                    report.unavailable += 1;
                    if !dry_run {
                        self.jobs.record_history_repair(
                            &job.id,
                            expected_updated,
                            scheduler.extractor_version(),
                            "unavailable",
                            now,
                        )?;
                    }
                    report.items.push(item);
                    continue;
                }
            };
            if self.jobs.generation_for_session(session)? != SessionMemoryGeneration::Enabled
                || self.jobs.source_was_forgotten(session, message)?
            {
                item.action = LearningHistoryAction::Excluded;
                report.excluded += 1;
                report.items.push(item);
                continue;
            }
            let (request, signals) = match self.request(project_id, session, message, false, redact)
            {
                Ok(input) => input,
                Err(crate::LearningServiceError::Database(
                    DbError::Conflict { .. } | DbError::NotFound { .. },
                )) => {
                    report.unavailable += 1;
                    if !dry_run {
                        self.jobs.record_history_repair(
                            &job.id,
                            expected_updated,
                            scheduler.extractor_version(),
                            "unavailable",
                            now,
                        )?;
                    }
                    report.items.push(item);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if signals.external_context && scheduler.excludes_external_context() {
                item.action = LearningHistoryAction::Excluded;
                report.excluded += 1;
                if !dry_run {
                    self.jobs.record_history_repair(
                        &job.id,
                        expected_updated,
                        scheduler.extractor_version(),
                        "excluded",
                        now,
                    )?;
                }
                report.items.push(item);
                continue;
            }
            if request.sources.is_empty() {
                report.unavailable += 1;
                if !dry_run {
                    self.jobs.record_history_repair(
                        &job.id,
                        expected_updated,
                        scheduler.extractor_version(),
                        "unavailable",
                        now,
                    )?;
                }
                report.items.push(item);
                continue;
            }
            let mut needs_extraction = true;
            if job.status == LearningJobStatus::Completed {
                let snapshot = self.sources.snapshot(
                    project_id,
                    session,
                    message,
                    &request.sources,
                    false,
                    scheduler.allows_closed_turn_while_busy(),
                )?;
                let repair = self.evidence.revalidate_legacy_job(
                    &job.id,
                    &snapshot,
                    &request.sources,
                    dry_run,
                    now,
                )?;
                item.revalidated_experiences = repair.revalidated;
                item.unverified_experiences = repair.unverified;
                report.revalidated_experiences += repair.revalidated;
                needs_extraction = repair.examined == 0 || repair.unverified > 0;
                if !dry_run {
                    expected_updated = now;
                }
                if !needs_extraction {
                    item.action = if dry_run {
                        LearningHistoryAction::WouldRevalidate
                    } else {
                        LearningHistoryAction::Revalidated
                    };
                }
            }
            if needs_extraction {
                if self
                    .jobs
                    .extraction_for_source(project_id, message, scheduler.extractor_version())?
                    .is_some()
                {
                    item.action = LearningHistoryAction::Existing;
                } else if dry_run {
                    item.action = LearningHistoryAction::WouldQueue;
                    report.would_queue += 1;
                } else {
                    match scheduler.schedule_reprocess(request, now)? {
                        LearningScheduleOutcome::Queued(_) => {
                            item.action = LearningHistoryAction::Queued;
                            report.queued += 1;
                            if job.status == LearningJobStatus::Queued {
                                expected_updated = now;
                            }
                        }
                        LearningScheduleOutcome::Existing(_) => {
                            item.action = LearningHistoryAction::Existing
                        }
                        _ => {
                            item.action = LearningHistoryAction::Excluded;
                            report.excluded += 1;
                        }
                    }
                }
            }
            if !dry_run {
                let outcome = match item.action {
                    LearningHistoryAction::Revalidated => "revalidated",
                    LearningHistoryAction::Queued => "queued",
                    LearningHistoryAction::Existing => "existing",
                    LearningHistoryAction::Excluded => "excluded",
                    LearningHistoryAction::Unavailable => "unavailable",
                    LearningHistoryAction::WouldRevalidate | LearningHistoryAction::WouldQueue => {
                        unreachable!("dry-run action")
                    }
                };
                self.jobs.record_history_repair(
                    &job.id,
                    expected_updated,
                    scheduler.extractor_version(),
                    outcome,
                    now,
                )?;
            }
            report.items.push(item);
        }
        Ok(report)
    }
}
