//! Public learning management. This contract contains no model prompts,
//! configuration snapshots, Worker identity, grant, or lease token.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_types::{
    activity::Counter,
    identity::{JobId, RequestId, SessionId, WorkspaceId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LearningStage {
    Extraction,
    Maintenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LearningState {
    Queued,
    Running,
    Completed,
    Skipped,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningFailureView {
    pub code: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningBudgetView {
    pub limit: Counter,
    pub charged: Counter,
    pub reserved: Counter,
    pub model_requests: Counter,
    pub unconfirmed_requests: Counter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningJobView {
    pub id: JobId,
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub source_job_id: JobId,
    pub stage: LearningStage,
    pub state: LearningState,
    pub attempts: Counter,
    pub budget: LearningBudgetView,
    pub created_at_ms: Counter,
    pub updated_at_ms: Counter,
    pub ready_at_ms: Option<Counter>,
    pub deadline_at_ms: Option<Counter>,
    pub failure: Option<LearningFailureView>,
    pub can_cancel: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningCursor {
    pub created_at_ms: Counter,
    pub job_id: JobId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningPageRequest {
    pub before: Option<LearningCursor>,
    pub stage: Option<LearningStage>,
    pub state: Option<LearningState>,
    #[serde(default)]
    pub limit: crate::PageSize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningPage {
    pub items: Vec<LearningJobView>,
    pub before: Option<LearningCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelLearning {
    pub request_id: RequestId,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningCancellation {
    pub request_id: RequestId,
    pub job: LearningJobView,
}
