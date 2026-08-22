//! Non-replayable cancellation of a durable background job.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use zuno_db::Pool;
use zuno_db::job::{AgentJobStore, JobStatus};
use zuno_error::{DbError, ToolError};
use zuno_tool::{ToolContext, ToolOutput, ToolReplayPolicy, TypedTool};

/// Model-visible cancellation tool id.
pub const WIRE_ID: &str = "job_cancel";

/// Arguments for one cancellation request.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobCancelParams {
    /// Job identifier returned by a background task or product-agent call.
    #[serde(rename = "jobID")]
    pub job_id: String,
}

/// Result of asking the live executor to cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelOutcome {
    /// Whether a live executor accepted the request.
    pub requested: bool,
    /// Human-readable safety diagnostic.
    pub message: String,
}

/// Live cancellation seam owned by the process supervisor.
#[async_trait]
pub trait JobController: Send + Sync + 'static {
    /// Request cancellation of one running job.
    async fn cancel(&self, parent_session_id: &str, job_id: &str) -> Result<CancelOutcome, String>;
}

/// Cancels only jobs owned by the current session.
pub struct JobCancelTool {
    store: AgentJobStore,
    controller: Arc<dyn JobController>,
}

impl JobCancelTool {
    /// Bind durable ownership checks to the live process controller.
    #[must_use]
    pub fn new(pool: Arc<Pool>, controller: Arc<dyn JobController>) -> Self {
        Self {
            store: AgentJobStore::new(pool),
            controller,
        }
    }
}

#[async_trait]
impl TypedTool for JobCancelTool {
    type Params = JobCancelParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        "Cancel a running background job owned by this session. Cancellation is a side effect \
         and is never automatically replayed. Terminal jobs are reported unchanged."
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    async fn run(
        &self,
        params: JobCancelParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let job = match self.store.get(&params.job_id) {
            Ok(job) if job.parent_session_id == ctx.session_id => job,
            Ok(_) | Err(DbError::NotFound { .. }) => return Err(not_found(&params.job_id)),
            Err(source) => {
                return Err(ToolError::Failed {
                    tool: WIRE_ID.to_owned(),
                    source: Box::new(source),
                });
            }
        };
        if job.status != JobStatus::Running {
            let status = status_name(job.status);
            let body = json!({
                "jobID": job.id,
                "status": status,
                "cancellationRequested": false,
                "message": format!("job is already {status}")
            });
            return Ok(ToolOutput::text(
                format!("{}: {status}", body["jobID"].as_str().unwrap_or_default()),
                serde_json::to_string_pretty(&body).expect("JSON value serializes"),
            )
            .with_metadata("job", body));
        }

        let outcome = self
            .controller
            .cancel(&ctx.session_id, &params.job_id)
            .await
            .map_err(|message| ToolError::Failed {
                tool: WIRE_ID.to_owned(),
                source: Box::new(std::io::Error::other(message)),
            })?;
        let body = json!({
            "jobID": params.job_id,
            "status": "running",
            "cancellationRequested": outcome.requested,
            "message": outcome.message,
        });
        Ok(ToolOutput::text(
            format!(
                "{}: {}",
                body["jobID"].as_str().unwrap_or_default(),
                if outcome.requested {
                    "cancellation requested"
                } else {
                    "not running in this process"
                }
            ),
            serde_json::to_string_pretty(&body).expect("JSON value serializes"),
        )
        .with_metadata("job", body))
    }
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

fn not_found(job_id: &str) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("job `{job_id}` was not found for this session"),
        )),
    }
}
