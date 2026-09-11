//! CLI adapters for bounded, project-owned learning work.
//!
//! These commands only inspect durable state or admit background jobs. They do
//! not invoke the foreground agent, create session inputs, or select a new model.

use super::{SessionCommandError, TurnHost, redact_learning_text};
use serde_json::{Value, json};
use std::sync::Arc;
use zuno_db::learning_job::{LearningJobRecord, LearningJobStatus};
use zuno_learning::{
    LearningHistoryRepair, LearningIngestion, LearningScheduleOutcome, LearningScheduler,
};

impl TurnHost {
    pub(super) fn learn_reprocess(&mut self, value: &str) -> Result<Value, SessionCommandError> {
        validate_message_id(value)?;
        self.refresh_memory_policy()
            .map_err(SessionCommandError::internal)?;
        self.require_learning_generation()
            .map_err(SessionCommandError::invalid_arguments)?;
        let scheduler = self
            .learning_runtime()
            .map_err(SessionCommandError::invalid_arguments)?
            .scheduler
            .clone();
        let outcome = reprocess(
            &LearningIngestion::new(Arc::clone(&self.database)),
            &scheduler,
            &self.project_id,
            value,
            &|text| redact_learning_text(text, self.credential.as_deref()),
        )?;
        if runnable(&outcome) {
            self.start_learning_maintenance();
            self.learning_supervisor.wake_project(&self.project_id);
            self.work_changes.changed();
        }
        Ok(admission_value(outcome, &|text| {
            redact_learning_text(text, self.credential.as_deref())
        }))
    }

    pub(super) fn learn_repair_history(
        &mut self,
        value: &str,
    ) -> Result<Value, SessionCommandError> {
        let dry_run = parse_dry_run(value)?;
        self.refresh_memory_policy()
            .map_err(SessionCommandError::internal)?;
        if !dry_run {
            self.require_learning_generation()
                .map_err(SessionCommandError::invalid_arguments)?;
        }
        let scheduler = self
            .learning_runtime()
            .map_err(SessionCommandError::invalid_arguments)?
            .scheduler
            .clone();
        let report = repair_history(
            &LearningIngestion::new(Arc::clone(&self.database)),
            &scheduler,
            &self.project_id,
            dry_run,
            &|text| redact_learning_text(text, self.credential.as_deref()),
        )?;
        if !dry_run {
            if report.queued > 0 || report.revalidated_experiences > 0 {
                self.start_learning_maintenance();
                self.learning_supervisor.wake_project(&self.project_id);
            }
            if report.examined > 0 {
                self.work_changes.changed();
            }
        }
        serde_json::to_value(report).map_err(SessionCommandError::internal)
    }

    /// Reading status remains available when model construction or generation is disabled.
    pub(super) fn learn_runtime_status(&self) -> Result<Value, SessionCommandError> {
        let mut value = serde_json::to_value(
            self.learning_projection
                .snapshot(&self.session_id, &self.project_id)
                .map_err(SessionCommandError::internal)?,
        )
        .map_err(SessionCommandError::internal)?;
        let learning = self.learning.as_ref();
        let generation = learning.and_then(|learning| learning.generation.as_ref());
        value["runtime"] = json!({
            "useExisting":learning.is_some_and(|learning|learning.scheduler.use_existing()),
            "generationEnabled":learning.is_some_and(|learning|learning.scheduler.generates()),
            "generationAvailable":generation.is_some(),
            "extractorVersion":generation.map(|generation|generation.extractor.version()),
            "sessionGeneration":self.memory_policy.generation.as_str(),
            "historyBatchLimit":zuno_db::learning_job::MAX_LEARNING_HISTORY_BATCH,
        });
        if let Some(jobs) = value["queue"]["jobs"].as_array_mut() {
            for job in jobs {
                if let Some(error) = job["error"].as_str() {
                    job["error"] = json!(redact_learning_text(error, self.credential.as_deref()));
                }
            }
        }
        Ok(value)
    }
}

fn validate_message_id(value: &str) -> Result<&str, SessionCommandError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 256
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(SessionCommandError::invalid_arguments(
            "usage: /learn reprocess <assistant-message-id>",
        ));
    }
    Ok(value)
}

fn parse_dry_run(value: &str) -> Result<bool, SessionCommandError> {
    match value.trim() {
        "" => Ok(false),
        "--dry-run" => Ok(true),
        _ => Err(SessionCommandError::invalid_arguments(
            "usage: /learn repair-history [--dry-run]",
        )),
    }
}

fn reprocess(
    ingestion: &LearningIngestion,
    scheduler: &LearningScheduler,
    project_id: &str,
    value: &str,
    redact: &dyn Fn(&str) -> String,
) -> Result<LearningScheduleOutcome, SessionCommandError> {
    let message = validate_message_id(value)?;
    ingestion
        .reprocess(
            project_id,
            message,
            scheduler,
            zuno_db::message::now_millis(),
            redact,
        )
        .map_err(|error| SessionCommandError::internal(redact(&error.diagnostic())))
}

fn repair_history(
    ingestion: &LearningIngestion,
    scheduler: &LearningScheduler,
    project_id: &str,
    dry_run: bool,
    redact: &dyn Fn(&str) -> String,
) -> Result<LearningHistoryRepair, SessionCommandError> {
    ingestion
        .repair_history(
            project_id,
            scheduler,
            zuno_db::message::now_millis(),
            dry_run,
            redact,
        )
        .map_err(|error| SessionCommandError::internal(redact(&error.diagnostic())))
}

fn runnable(outcome: &LearningScheduleOutcome) -> bool {
    match outcome {
        LearningScheduleOutcome::Queued(_) => true,
        LearningScheduleOutcome::Existing(job) => job.status == LearningJobStatus::Queued,
        _ => false,
    }
}

fn admission_value(outcome: LearningScheduleOutcome, redact: &dyn Fn(&str) -> String) -> Value {
    let (admission, job): (&str, LearningJobRecord) = match outcome {
        LearningScheduleOutcome::Queued(job) => ("queued", job),
        LearningScheduleOutcome::Existing(job) => ("existing", job),
        LearningScheduleOutcome::Disabled => return json!({"admission":"disabled"}),
        LearningScheduleOutcome::Excluded => return json!({"admission":"excluded"}),
        LearningScheduleOutcome::Ineligible => return json!({"admission":"ineligible"}),
        LearningScheduleOutcome::SkippedInsufficientRecords { observed, required } => {
            return json!({"admission":"insufficientRecords","observed":observed,"required":required});
        }
    };
    json!({
        "admission":admission,"jobID":job.id,"status":job.status.as_str(),
        "sourceMessageID":job.source_message_id,"extractorVersion":job.extractor_version,
        "attempt":job.attempt,"scheduledAt":job.scheduled_at,
        "error":job.error.as_deref().map(redact),
    })
}

#[cfg(test)]
#[path = "learn_runtime_tests.rs"]
mod tests;
