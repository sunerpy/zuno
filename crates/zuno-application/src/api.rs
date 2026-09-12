//! Public application DTOs, independent of HTTP handlers and Worker state.
use crate::{
    authorization::{ApprovalAnswer, ApprovalBinding, ApprovalRecord, ApprovalState},
    runtime::{JobPhase, RuntimeJob},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zuno_permission::enterprise::ApprovalAudience;
use zuno_types::identity::{
    ApprovalId, InputId, JobId, PrincipalKey, RequestId, SessionId, TurnId, WorkspaceId,
};
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceView {
    pub id: WorkspaceId,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobView {
    pub id: JobId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub input_id: InputId,
    pub phase: JobPhase,
    /// Decimal strings preserve exact counters in JavaScript clients.
    pub input_version: String,
    /// Public waiting coordinates, without arguments, checkpoints or grants.
    pub waits: Vec<JobWaitView>,
    pub stop_requested: bool,
    pub pending_operations: Vec<zuno_types::identity::OperationId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobWaitView {
    pub invocation_id: zuno_types::identity::InvocationId,
    pub target: zuno_types::wait::WaitTarget,
}
impl From<RuntimeJob> for JobView {
    fn from(job: RuntimeJob) -> Self {
        Self {
            id: job.id,
            session_id: job.session_id,
            turn_id: job.turn_id,
            input_id: job.input_id,
            phase: job.phase,
            input_version: job.input_version.to_string(),
            waits: Vec::new(),
            stop_requested: false,
            pending_operations: Vec::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputVersionView {
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubmitTurn {
    pub request_id: RequestId,
    pub expected_input_version: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalView {
    pub id: ApprovalId,
    pub binding: ApprovalBinding,
    pub requester: PrincipalKey,
    pub audience: ApprovalAudience,
    pub state: ApprovalState,
    pub presentation: Value,
    pub expires_at_ms: i64,
}
impl From<ApprovalRecord> for ApprovalView {
    fn from(value: ApprovalRecord) -> Self {
        Self {
            id: value.id,
            binding: value.binding,
            requester: value.requester.owner(),
            audience: value.audience,
            state: value.state,
            presentation: value.presentation,
            expires_at_ms: value.expires_at_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalDecision {
    pub request_id: RequestId,
    pub answer: ApprovalAnswer,
}
