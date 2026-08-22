//! Durable background-job inspection.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use zuno_db::Pool;
use zuno_db::job::{AgentJob, AgentJobStore, JobStatus, ReportDelivery};
use zuno_error::{DbError, ToolError};
use zuno_tool::{ToolConcurrencyPolicy, ToolContext, ToolOutput, ToolReplayPolicy, TypedTool};

/// The tool identifier exposed to models.
pub const WIRE_ID: &str = "job";

/// The description exposed to models.
pub const DESCRIPTION: &str = include_str!("description/job.txt");

/// Selects one durable background job.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobParams {
    /// Job identifier returned by a background task call.
    #[serde(rename = "jobID")]
    pub job_id: String,
}

/// Reads background-job state from the session database.
#[derive(Clone)]
pub struct JobTool {
    store: AgentJobStore,
}

impl JobTool {
    /// Bind the tool to the database shared by the current harness.
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self {
            store: AgentJobStore::new(pool),
        }
    }
}

#[async_trait]
impl TypedTool for JobTool {
    type Params = JobParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::ParallelSafe
    }

    async fn run(&self, params: JobParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let job = match self.store.get(&params.job_id) {
            Ok(job) if job.parent_session_id == ctx.session_id => job,
            Ok(_) | Err(DbError::NotFound { .. }) => {
                return Err(not_found(&params.job_id));
            }
            Err(source) => {
                return Err(ToolError::Failed {
                    tool: WIRE_ID.to_owned(),
                    source: Box::new(source),
                });
            }
        };
        render(job)
    }
}

fn render(job: AgentJob) -> Result<ToolOutput, ToolError> {
    let status = status_name(job.status);
    let body = json!({
        "jobID": job.id,
        "parentSessionID": job.parent_session_id,
        "subject": job.subject.as_json(),
        "status": status,
        "reportDelivery": delivery_name(job.report_delivery),
        "result": job.result,
        "error": job.error,
        "reportInputID": job.report_input_id,
        "createdSequence": job.created_sequence,
        "settledSequence": job.settled_sequence,
        "timeCreated": job.time_created,
        "timeUpdated": job.time_updated,
        "timeCompleted": job.time_completed,
    });
    let output = serde_json::to_string_pretty(&body).map_err(|source| ToolError::Failed {
        tool: WIRE_ID.to_owned(),
        source: Box::new(source),
    })?;
    Ok(ToolOutput::text(
        format!("{}: {status}", body["jobID"].as_str().unwrap_or_default()),
        output,
    )
    .with_metadata("job", body))
}

fn status_name(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
        JobStatus::Uncertain => "uncertain",
    }
}

fn delivery_name(delivery: ReportDelivery) -> &'static str {
    match delivery {
        ReportDelivery::NextStep => "nextStep",
        ReportDelivery::Quiet => "quiet",
    }
}

fn not_found(job_id: &str) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("job `{job_id}` was not found for this session"),
        )),
    }
}

#[cfg(test)]
mod replay_policy_tests {
    use super::*;

    #[test]
    fn durable_job_inspection_is_safe_to_repeat() {
        let pool = Arc::new(Pool::open(&zuno_paths::DbLocation::Memory).expect("job database"));
        assert_eq!(JobTool::new(pool).replay_policy(), ToolReplayPolicy::Safe);
    }
}
