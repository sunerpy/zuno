//! Explicit, evidenced resolution of uncertain durable jobs.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use uuid::Uuid;
use zuno_db::Pool;
use zuno_db::inbox::{InputDelivery, NewSessionInput};
use zuno_db::job::{
    AgentJob, AgentJobStore, JobReconciliation, JobStatus, JobSubject, ReportDelivery,
};
use zuno_error::{DbError, ToolError};
use zuno_tool::{ToolContext, ToolOutput, ToolReplayPolicy, TypedTool};

/// Model-visible reconciliation tool id.
pub const WIRE_ID: &str = "job_reconcile";

/// The description exposed to models.
pub const DESCRIPTION: &str = include_str!("description/job_reconcile.txt");

/// Authoritative terminal outcome observed outside Zuno.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ReconciledOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl ReconciledOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Evidence required to replace one uncertain durable outcome.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobReconcileParams {
    /// Job identifier returned by a background operation.
    #[serde(rename = "jobID")]
    pub job_id: String,
    /// Authoritative terminal state.
    pub outcome: ReconciledOutcome,
    /// Final user-facing text when the operation completed.
    #[serde(default, rename = "finalText")]
    pub final_text: Option<String>,
    /// Final error when the operation failed or was cancelled.
    #[serde(default)]
    pub error: Option<String>,
    /// External system, API, process supervisor, or operator record inspected.
    pub authority: String,
    /// Concrete observation from that authority.
    pub evidence: String,
}

/// Reconciles only uncertain jobs owned by the current session.
#[derive(Clone)]
pub struct JobReconcileTool {
    store: AgentJobStore,
}

impl JobReconcileTool {
    /// Bind reconciliation to the harness database.
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self {
            store: AgentJobStore::new(pool),
        }
    }
}

#[async_trait]
impl TypedTool for JobReconcileTool {
    type Params = JobReconcileParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    async fn run(
        &self,
        params: JobReconcileParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let job = match self.store.get(&params.job_id) {
            Ok(job) if job.parent_session_id == ctx.session_id => job,
            Ok(_) | Err(DbError::NotFound { .. }) => return Err(not_found(&params.job_id)),
            Err(source) => return Err(failed(source)),
        };
        if job.status != JobStatus::Uncertain {
            return Err(invalid(format!(
                "job `{}` is {}, not uncertain",
                job.id,
                job.status.as_str()
            )));
        }
        if params.authority.trim().is_empty() {
            return Err(invalid("`authority` must not be empty"));
        }
        if params.evidence.trim().is_empty() {
            return Err(invalid("`evidence` must not be empty"));
        }

        let (final_text, error) = resolved_text(&params)?;
        let metadata = reconciled_metadata(&job, &params, &final_text);
        let report = (job.report_delivery == ReportDelivery::NextStep)
            .then(|| reconciliation_report(&job, &params, &final_text, &metadata));
        let reconciliation = match params.outcome {
            ReconciledOutcome::Completed => JobReconciliation::completed(
                metadata.clone(),
                params.authority.clone(),
                params.evidence.clone(),
                zuno_db::message::now_millis(),
                report,
            ),
            ReconciledOutcome::Failed => JobReconciliation::failed(
                error.expect("failed outcome was validated"),
                params.authority.clone(),
                params.evidence.clone(),
                zuno_db::message::now_millis(),
                report,
            )
            .with_result(metadata.clone()),
            ReconciledOutcome::Cancelled => JobReconciliation::cancelled(
                error.expect("cancelled outcome was validated"),
                params.authority.clone(),
                params.evidence.clone(),
                zuno_db::message::now_millis(),
                report,
            )
            .with_result(metadata.clone()),
        };
        let settled = self
            .store
            .reconcile_uncertain(&job.id, reconciliation)
            .map_err(failed)?;
        let body = json!({
            "jobID": settled.job.id,
            "status": params.outcome.as_str(),
            "authority": params.authority,
            "evidence": params.evidence,
            "reportInputID": settled.job.report_input_id,
            "result": metadata,
        });
        Ok(ToolOutput::text(
            format!("{}: reconciled", job.id),
            serde_json::to_string_pretty(&body).expect("JSON value serializes"),
        )
        .with_metadata("job", body))
    }
}

fn resolved_text(params: &JobReconcileParams) -> Result<(String, Option<String>), ToolError> {
    match params.outcome {
        ReconciledOutcome::Completed => {
            let text = params
                .final_text
                .as_deref()
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| {
                    invalid("completed reconciliation requires non-empty `finalText`")
                })?;
            if params.error.is_some() {
                return Err(invalid("completed reconciliation must not include `error`"));
            }
            Ok((text.to_owned(), None))
        }
        ReconciledOutcome::Failed | ReconciledOutcome::Cancelled => {
            let error = params
                .error
                .as_deref()
                .filter(|error| !error.trim().is_empty())
                .ok_or_else(|| {
                    invalid("failed or cancelled reconciliation requires non-empty `error`")
                })?
                .to_owned();
            Ok((
                params.final_text.clone().unwrap_or_else(|| error.clone()),
                Some(error),
            ))
        }
    }
}

fn reconciled_metadata(job: &AgentJob, params: &JobReconcileParams, final_text: &str) -> Value {
    let mut metadata = job
        .result
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert(
        "schemaVersion".to_owned(),
        metadata
            .get("schemaVersion")
            .cloned()
            .unwrap_or_else(|| Value::Number(1_u64.into())),
    );
    metadata.insert("jobId".to_owned(), Value::String(job.id.clone()));
    metadata.insert(
        "parentSessionId".to_owned(),
        Value::String(job.parent_session_id.clone()),
    );
    metadata.insert(
        "status".to_owned(),
        Value::String(params.outcome.as_str().to_owned()),
    );
    metadata.insert("finalText".to_owned(), Value::String(final_text.to_owned()));
    metadata.insert(
        "reconciliation".to_owned(),
        json!({
            "authority": params.authority,
            "evidence": params.evidence,
        }),
    );
    if let JobSubject::ChildSession { session_id } = &job.subject {
        metadata.insert("sessionId".to_owned(), Value::String(session_id.clone()));
    }
    Value::Object(metadata)
}

fn reconciliation_report(
    job: &AgentJob,
    params: &JobReconcileParams,
    final_text: &str,
    metadata: &Value,
) -> NewSessionInput {
    let mut prompt = Map::from_iter([
        (
            "kind".to_owned(),
            Value::String("subagentReport".to_owned()),
        ),
        ("jobID".to_owned(), Value::String(job.id.clone())),
        (
            "status".to_owned(),
            Value::String(params.outcome.as_str().to_owned()),
        ),
        ("text".to_owned(), Value::String(final_text.to_owned())),
        ("metadata".to_owned(), metadata.clone()),
        ("subject".to_owned(), job.subject.as_json()),
    ]);
    if let JobSubject::ChildSession { session_id } = &job.subject {
        prompt.insert(
            "childSessionID".to_owned(),
            Value::String(session_id.clone()),
        );
    }
    NewSessionInput::new(
        format!("input_{}", Uuid::new_v4().simple()),
        job.parent_session_id.clone(),
        Value::Object(prompt),
        InputDelivery::Queue,
        zuno_db::message::now_millis(),
    )
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

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::other(message.into())),
    }
}

fn failed(source: impl std::error::Error + Send + Sync + 'static) -> ToolError {
    ToolError::Failed {
        tool: WIRE_ID.to_owned(),
        source: Box::new(source),
    }
}
