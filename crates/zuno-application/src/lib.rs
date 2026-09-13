//! Client-independent application services.
//!
//! A host authenticates and authorizes before constructing a principal-bound
//! persistence provider. Client DTOs contain neither owner overrides nor host
//! paths. Drivers and clients do not acquire database connections through this API.

pub mod activity;
pub mod api;
pub mod authorization;
pub mod child;
pub mod control;
pub mod council;
pub mod environment;
pub mod live;
pub mod runtime;
pub mod workflow;
pub mod workspace_import;
pub mod workspace_merge;
pub mod workspace_transfer;

use std::sync::Arc;

use async_trait::async_trait;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use zuno_runtime::{Component, PrepareContext, RuntimeError};
use zuno_types::identity::{InputId, PrincipalScope, RequestId, SessionId, WorkspaceId};

pub const MAX_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    #[error("invalid application input: {0}")]
    Invalid(String),
    #[error("the requested resource is unavailable to this principal")]
    NotFound,
    #[error("organization policy does not authorize this action")]
    Forbidden,
    #[error("the request conflicts with already committed state")]
    Conflict,
    #[error("the execution lease is no longer authoritative")]
    LeaseLost,
    #[error("the state service is temporarily unavailable")]
    Unavailable,
    #[error("the state service failed")]
    Storage {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl ApplicationError {
    #[must_use]
    pub fn storage(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Storage {
            source: Box::new(source),
        }
    }
}

/// A bounded page size validated at deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct PageSize(u16);

impl PageSize {
    pub fn new(value: u16) -> Result<Self, ApplicationError> {
        if (1..=100).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ApplicationError::Invalid(
                "page size must be between 1 and 100".to_owned(),
            ))
        }
    }
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl Default for PageSize {
    fn default() -> Self {
        Self(50)
    }
}
impl TryFrom<u16> for PageSize {
    type Error = ApplicationError;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<PageSize> for u16 {
    fn from(value: PageSize) -> Self {
        value.0
    }
}
impl JsonSchema for PageSize {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "PageSize".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"integer","minimum":1,"maximum":100})
    }
}

/// A session's public summary. Filesystem locations remain backend-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSummary {
    pub id: SessionId,
    pub workspace_id: Option<WorkspaceId>,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Both ordering keys are needed when sessions have equal timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCursor {
    pub updated_at: i64,
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionPageRequest {
    pub after: Option<SessionCursor>,
    #[serde(default)]
    pub limit: PageSize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionPage {
    pub items: Vec<SessionSummary>,
    pub next: Option<SessionCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateSession {
    pub request_id: RequestId,
    pub workspace_id: WorkspaceId,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueText {
    pub request_id: RequestId,
    pub session_id: SessionId,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputState {
    Queued,
    Steering,
    Promoted,
    Consumed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputReceipt {
    pub id: InputId,
    pub session_id: SessionId,
    pub state: InputState,
    /// Logical per-session event position, encoded without JavaScript precision loss.
    pub admitted_cursor: String,
}

/// A provider is bound to a host-verified principal for its whole lifetime.
///
/// Create + attribution + event, and input + admission event, each commit
/// atomically. A repeated request returns the original resource; different
/// content under that request ID conflicts. Paging filters ownership in storage
/// before applying the two-key cursor and limit.
#[async_trait]
pub trait SessionPersistence: Send + Sync {
    fn principal(&self) -> &PrincipalScope;
    async fn create(&self, request: CreateSession) -> Result<SessionSummary, ApplicationError>;
    async fn get(&self, id: &SessionId) -> Result<SessionSummary, ApplicationError>;
    async fn list(&self, request: SessionPageRequest) -> Result<SessionPage, ApplicationError>;
    async fn queue_text(&self, request: QueueText) -> Result<InputReceipt, ApplicationError>;
}

/// The application service consumed by client adapters.
#[derive(Clone)]
pub struct AgentApplication {
    sessions: Arc<dyn SessionPersistence>,
}

impl AgentApplication {
    #[must_use]
    pub fn new(sessions: Arc<dyn SessionPersistence>) -> Self {
        Self { sessions }
    }

    #[must_use]
    pub fn principal(&self) -> &PrincipalScope {
        self.sessions.principal()
    }

    pub async fn create_session(
        &self,
        mut request: CreateSession,
    ) -> Result<SessionSummary, ApplicationError> {
        request.title = request.title.trim().to_owned();
        if request.title.is_empty()
            || request.title.chars().count() > 256
            || request.title.chars().any(char::is_control)
        {
            return Err(ApplicationError::Invalid(
                "a session title must contain 1–256 visible characters".to_owned(),
            ));
        }
        self.sessions.create(request).await
    }

    pub async fn session(&self, id: &SessionId) -> Result<SessionSummary, ApplicationError> {
        self.sessions.get(id).await
    }

    pub async fn sessions(
        &self,
        page: SessionPageRequest,
    ) -> Result<SessionPage, ApplicationError> {
        self.sessions.list(page).await
    }

    /// Queue a genuine text input for the native inbox. Execution/steering is a
    /// separate control operation, not implied by an admission receipt.
    pub async fn queue_text(&self, request: QueueText) -> Result<InputReceipt, ApplicationError> {
        if request.text.trim().is_empty()
            || request.text.len() > MAX_INPUT_BYTES
            || request.text.contains('\0')
        {
            return Err(ApplicationError::Invalid(
                "text must be nonempty, contain no NUL, and fit within 1 MiB".to_owned(),
            ));
        }
        self.sessions.queue_text(request).await
    }
}

#[async_trait]
impl Component for AgentApplication {
    fn id(&self) -> &str {
        "agent-application"
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::new(self.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_and_identity_boundaries_are_validated_before_a_store_is_called() {
        assert!(
            serde_json::from_value::<SessionPageRequest>(serde_json::json!({"limit":0})).is_err()
        );
        assert!(
            serde_json::from_value::<SessionPageRequest>(serde_json::json!({"limit":101})).is_err()
        );
        assert!(
            serde_json::from_value::<SessionPageRequest>(
                serde_json::json!({"limit":10,"owner":"someone-else"})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<CreateSession>(
            serde_json::json!({"requestId":"../escape","workspaceId":"workspace","title":"Task"})
        ).is_err());
        assert!(serde_json::from_value::<CreateSession>(
            serde_json::json!({"requestId":"request","workspaceId":"workspace","title":"Task","directory":"/host"})
        ).is_err());
    }
}
