use crate::WorkflowCallIdentity;
use crate::WorkflowEngine;
use crate::WorkflowError;
use crate::WorkflowRunId;
use crate::WorkflowSourceIdentity;
use crate::sha256_hex;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use thiserror::Error;

pub const WORKFLOW_LEDGER_DB_FILENAME: &str = "zuno_workflows_1.sqlite";
const WORKFLOW_BINDINGS_SCHEMA: &str = "zuno.workflow-bindings/v1";
const WORKFLOW_BINDINGS_SCHEMA_VERSION: u64 = 1;
const MAX_WORKFLOW_BINDINGS_BYTES: usize = 256 * 1024;

#[derive(Debug, Error)]
pub enum WorkflowLedgerError {
    #[error("workflow ledger database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("workflow ledger JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid workflow ledger value: {0}")]
    InvalidValue(String),
    #[error("workflow run `{run_id}` was not found")]
    RunNotFound { run_id: String },
    #[error("workflow run `{run_id}` already has a cancellation request")]
    RunCancellationRequested { run_id: String },
    #[error("workflow call `{call_id}` was not found in run `{run_id}`")]
    CallNotFound { run_id: String, call_id: String },
    #[error(
        "workflow run `{run_id}` replay diverged: recorded request digest {recorded}, actual {actual}"
    )]
    RunReplayDiverged {
        run_id: String,
        recorded: String,
        actual: String,
    },
    #[error(
        "workflow call `{call_id}` in run `{run_id}` replay diverged: recorded request digest {recorded}, actual {actual}"
    )]
    CallReplayDiverged {
        run_id: String,
        call_id: String,
        recorded: String,
        actual: String,
    },
    #[error("workflow run `{run_id}` cannot transition from {from} to {to}")]
    InvalidRunTransition {
        run_id: String,
        from: WorkflowRunStatus,
        to: WorkflowRunStatus,
    },
    #[error(
        "workflow call `{call_id}` in run `{run_id}` is already terminal as {recorded}; refusing divergent completion as {actual}"
    )]
    CallCompletionConflict {
        run_id: String,
        call_id: String,
        recorded: WorkflowCallStatus,
        actual: WorkflowCallStatus,
    },
    #[error(
        "workflow ledger schema marker is missing while {table_count} application tables already exist"
    )]
    MissingSchemaMarker { table_count: i64 },
    #[error("workflow ledger schema marker is corrupt: {0}")]
    CorruptSchemaMarker(String),
    #[error("workflow ledger schema version {found} is newer than supported version {supported}")]
    FutureSchema { found: i64, supported: i64 },
    #[error(
        "workflow ledger schema version {found} is older than supported version {supported}; no migration is registered"
    )]
    UnsupportedSchema { found: i64, supported: i64 },
}

impl From<WorkflowError> for WorkflowLedgerError {
    fn from(error: WorkflowError) -> Self {
        Self::InvalidValue(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    PendingApproval,
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}

impl WorkflowRunStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Uncertain
        )
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::PendingApproval => "pending_approval",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Uncertain => "uncertain",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, WorkflowLedgerError> {
        match value {
            "pending_approval" => Ok(Self::PendingApproval),
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "uncertain" => Ok(Self::Uncertain),
            value => Err(WorkflowLedgerError::InvalidValue(format!(
                "unknown workflow run status `{value}`"
            ))),
        }
    }
}

impl std::fmt::Display for WorkflowRunStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCallStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}

impl WorkflowCallStatus {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Uncertain => "uncertain",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, WorkflowLedgerError> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "uncertain" => Ok(Self::Uncertain),
            value => Err(WorkflowLedgerError::InvalidValue(format!(
                "unknown workflow call status `{value}`"
            ))),
        }
    }
}

impl std::fmt::Display for WorkflowCallStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLedgerRun {
    pub run_id: WorkflowRunId,
    pub workflow: WorkflowSourceIdentity,
    pub executable_digest: String,
    pub engine: WorkflowEngine,
    pub engine_revision: String,
    /// Digest of the complete route-to-backend binding set admitted for this run.
    ///
    /// The corresponding snapshot is retained so a host can verify the exact
    /// route binding again immediately before dispatch without consulting a
    /// mutable workflow or profile document.
    pub binding_digest: String,
    pub bindings: JsonValue,
    pub parent_thread_id: String,
    pub request_digest: String,
    pub args: JsonValue,
    pub status: WorkflowRunStatus,
    pub result: Option<JsonValue>,
    pub error: Option<String>,
    pub agents_started: u32,
    pub cancel_requested_at: Option<i64>,
    pub cancel_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub calls: Vec<WorkflowLedgerCall>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLedgerCall {
    pub run_id: WorkflowRunId,
    pub call_id: String,
    pub request_digest: String,
    pub operation: String,
    pub request: JsonValue,
    pub status: WorkflowCallStatus,
    pub result: Option<JsonValue>,
    pub error: Option<String>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct AcceptWorkflowRun {
    pub run_id: WorkflowRunId,
    pub workflow: WorkflowSourceIdentity,
    pub executable_digest: String,
    pub engine: WorkflowEngine,
    pub engine_revision: String,
    pub bindings: JsonValue,
    pub parent_thread_id: String,
    pub args: JsonValue,
    pub pending_approval: bool,
}

impl AcceptWorkflowRun {
    pub fn request_digest(&self) -> Result<String, WorkflowLedgerError> {
        let binding_digest = self.binding_digest()?;
        digest_json(&serde_json::json!({
            "workflow": self.workflow,
            "executableDigest": self.executable_digest,
            "engine": self.engine,
            "engineRevision": self.engine_revision,
            "bindingDigest": binding_digest,
            "parentThreadId": self.parent_thread_id,
            "args": self.args,
        }))
    }

    pub fn binding_digest(&self) -> Result<String, WorkflowLedgerError> {
        Ok(sha256_hex(&self.canonical_bindings_bytes()?))
    }

    pub fn canonical_bindings_json(&self) -> Result<String, WorkflowLedgerError> {
        String::from_utf8(self.canonical_bindings_bytes()?).map_err(|error| {
            WorkflowLedgerError::InvalidValue(format!(
                "canonical workflow bindings are not UTF-8: {error}"
            ))
        })
    }

    fn canonical_bindings_bytes(&self) -> Result<Vec<u8>, WorkflowLedgerError> {
        let object = self.bindings.as_object().ok_or_else(|| {
            WorkflowLedgerError::InvalidValue("workflow bindings must be a JSON object".to_string())
        })?;
        if object.get("schemaVersion").and_then(JsonValue::as_u64)
            != Some(WORKFLOW_BINDINGS_SCHEMA_VERSION)
        {
            return Err(WorkflowLedgerError::InvalidValue(format!(
                "workflow bindings schemaVersion must be {WORKFLOW_BINDINGS_SCHEMA_VERSION}"
            )));
        }
        if object.get("schema").and_then(JsonValue::as_str) != Some(WORKFLOW_BINDINGS_SCHEMA) {
            return Err(WorkflowLedgerError::InvalidValue(format!(
                "workflow bindings schema must be {WORKFLOW_BINDINGS_SCHEMA:?}"
            )));
        }
        if !object.get("routes").is_some_and(JsonValue::is_object) {
            return Err(WorkflowLedgerError::InvalidValue(
                "workflow bindings routes must be a JSON object".to_string(),
            ));
        }
        let encoded = serde_json::to_vec(&canonical_json(&self.bindings))?;
        if encoded.len() > MAX_WORKFLOW_BINDINGS_BYTES {
            return Err(WorkflowLedgerError::InvalidValue(format!(
                "workflow bindings exceed the {MAX_WORKFLOW_BINDINGS_BYTES}-byte limit"
            )));
        }
        Ok(encoded)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AcceptRunOutcome {
    Created(WorkflowLedgerRun),
    Existing(WorkflowLedgerRun),
}

impl AcceptRunOutcome {
    pub fn record(&self) -> &WorkflowLedgerRun {
        match self {
            Self::Created(record) | Self::Existing(record) => record,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AcceptWorkflowCall {
    pub run_id: WorkflowRunId,
    pub identity: WorkflowCallIdentity,
    pub operation: String,
    pub request: JsonValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AcceptCallOutcome {
    /// The call was durably marked running. The caller may dispatch it exactly once.
    Dispatch(WorkflowLedgerCall),
    /// The call id already exists. The caller must not dispatch it again.
    Existing(WorkflowLedgerCall),
}

impl AcceptCallOutcome {
    pub fn record(&self) -> &WorkflowLedgerCall {
        match self {
            Self::Dispatch(record) | Self::Existing(record) => record,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowCallCompletion {
    Completed(JsonValue),
    Failed(String),
    Cancelled(String),
    Uncertain(String),
}

impl WorkflowCallCompletion {
    pub(super) fn fields(&self) -> (WorkflowCallStatus, Option<&JsonValue>, Option<&str>) {
        match self {
            Self::Completed(result) => (WorkflowCallStatus::Completed, Some(result), None),
            Self::Failed(error) => (WorkflowCallStatus::Failed, None, Some(error)),
            Self::Cancelled(error) => (WorkflowCallStatus::Cancelled, None, Some(error)),
            Self::Uncertain(error) => (WorkflowCallStatus::Uncertain, None, Some(error)),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowRunCompletion {
    Completed {
        result: JsonValue,
        agents_started: u32,
    },
    Failed {
        error: String,
        agents_started: u32,
    },
    Cancelled {
        error: String,
        agents_started: u32,
    },
    Uncertain {
        error: String,
        agents_started: u32,
    },
}

impl WorkflowRunCompletion {
    pub(super) fn fields(&self) -> (WorkflowRunStatus, Option<&JsonValue>, Option<&str>, u32) {
        match self {
            Self::Completed {
                result,
                agents_started,
            } => (
                WorkflowRunStatus::Completed,
                Some(result),
                None,
                *agents_started,
            ),
            Self::Failed {
                error,
                agents_started,
            } => (
                WorkflowRunStatus::Failed,
                None,
                Some(error),
                *agents_started,
            ),
            Self::Cancelled {
                error,
                agents_started,
            } => (
                WorkflowRunStatus::Cancelled,
                None,
                Some(error),
                *agents_started,
            ),
            Self::Uncertain {
                error,
                agents_started,
            } => (
                WorkflowRunStatus::Uncertain,
                None,
                Some(error),
                *agents_started,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySummary {
    pub uncertain_runs: u64,
    pub uncertain_calls: u64,
}

fn digest_json(value: &JsonValue) -> Result<String, WorkflowLedgerError> {
    let canonical = canonical_json(value);
    Ok(sha256_hex(&serde_json::to_vec(&canonical)?))
}

fn canonical_json(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(values) => JsonValue::Array(values.iter().map(canonical_json).collect()),
        JsonValue::Object(values) => JsonValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        value => value.clone(),
    }
}
