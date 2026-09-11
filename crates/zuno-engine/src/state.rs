//! Persistence operations used by the shared turn driver.
//!
//! Each operation has a domain boundary. Providers must authorize the session
//! and, for distributed execution, fence writes against the current execution
//! lease. A caller never supplies a database connection or a SQL statement.

pub mod remote;
mod sqlite;
pub mod wire;
pub use sqlite::SqliteTurnPersistence;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use zuno_db::assistant_commit::AssistantCommit;
use zuno_db::event_log::{NewSessionEvent, SessionEvent};
use zuno_db::message::{MessageRecord, MessageWithParts, PartRecord};
use zuno_db::provider_backoff::ProviderBackoffCheckpoint;
use zuno_orchestration::ToolSchemaIdentity;
use zuno_types::identity::PrincipalKey;

use crate::advance::{
    AdvanceAdmission, AdvanceError, AdvanceRequest, AdvanceState, BeginAdvance, CheckpointRef,
};
use crate::r#loop::TurnError;

pub type LegacyToolSchemas = BTreeMap<String, BTreeMap<String, ToolSchemaIdentity>>;
pub type DeveloperContexts = BTreeMap<String, Option<Vec<String>>>;

/// Backend-independent failures. Transport failure is not permission to replay
/// an operation whose commit acknowledgement may have been lost.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnStateError {
    #[error("turn state is temporarily unavailable; reconcile the commit before resuming")]
    Unavailable,
    #[error("the execution lease is no longer current")]
    LeaseLost,
    #[error("current organization authorization denies this turn")]
    Forbidden,
    #[error("turn state was not found in this scope")]
    NotFound,
    #[error("turn state changed concurrently")]
    Conflict,
    #[error("stored turn state is invalid")]
    InvalidData,
}

/// Only the session facts consumed by the kernel. The directory is the
/// executor-visible working directory; transport adapters must not substitute
/// a control-plane host path for an environment's own path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSession {
    pub id: String,
    pub parent_id: Option<String>,
    pub directory: Option<String>,
}

impl TurnSession {
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
}

/// Coordinates identify data; they are not proof of authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStateScope {
    pub owner: PrincipalKey,
    pub session_id: String,
}

/// Bookkeeping committed in the same transaction as a provider event.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderEventUpdate {
    #[default]
    None,
    RequestStarted {
        estimated_prompt_tokens: u64,
        context_limit: Option<u64>,
    },
    AttemptStarted,
    RequestFinished {
        request_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPartCommitKind {
    Dispatched,
    Result,
}

/// Attachment admission happens before this operation. All content is already
/// validated; the store chooses the message time and consumes the inbox atomically.
#[derive(Debug, Clone)]
pub struct InputMaterialization {
    pub input_id: Option<String>,
    pub turn_id: Option<String>,
    pub message: MessageRecord,
    pub parts: Vec<PartRecord>,
}

#[derive(Debug, Clone)]
pub struct ProviderRequestCommit {
    pub assistant: MessageRecord,
    pub event: NewSessionEvent,
    pub estimated_prompt_tokens: u64,
    pub context_limit: Option<u64>,
    pub context: crate::context_usage::ContextRequestPreparation,
}

#[derive(Debug, Clone)]
pub struct ProviderRequestReceipt {
    pub event: SessionEvent,
    pub context: zuno_types::context_usage::ContextUsageTracker,
}

#[async_trait]
pub trait TurnPersistence: Send + Sync {
    async fn context_usage(
        &self,
        scope: &TurnStateScope,
    ) -> Result<crate::context_usage::ContextUsageSeed, TurnError>;
    async fn commit_context_usage(
        &self,
        scope: &TurnStateScope,
        update: &zuno_types::context_usage::ContextUsageWrite,
    ) -> Result<(), TurnError>;
    /// Request admission, the initial assistant, bookkeeping and canonical
    /// context state share the event's database-assigned logical sequence.
    async fn start_provider_request(
        &self,
        scope: &TurnStateScope,
        commit: ProviderRequestCommit,
    ) -> Result<ProviderRequestReceipt, TurnError>;
    /// Only consumed inputs with nonterminal execution receipts are eligible.
    async fn applicable_inputs(
        &self,
        scope: &TurnStateScope,
        candidates: &[String],
    ) -> Result<Vec<String>, TurnError>;
    async fn mark_inputs_applied(
        &self,
        scope: &TurnStateScope,
        turn_id: &str,
        input_ids: &[String],
        at_ms: i64,
    ) -> Result<(), TurnError>;
    async fn session(&self, scope: &TurnStateScope) -> Result<TurnSession, TurnError>;
    async fn clock(&self, scope: &TurnStateScope) -> Result<i64, TurnError>;
    async fn touch(&self, scope: &TurnStateScope) -> Result<(), TurnError>;
    async fn repair_history(&self, scope: &TurnStateScope) -> Result<usize, TurnError>;
    async fn has_uncertain_calls(&self, scope: &TurnStateScope) -> Result<bool, TurnError>;
    async fn history(&self, scope: &TurnStateScope) -> Result<Vec<MessageWithParts>, TurnError>;
    async fn legacy_tool_schemas(
        &self,
        scope: &TurnStateScope,
    ) -> Result<LegacyToolSchemas, TurnError>;
    async fn developer_contexts(
        &self,
        scope: &TurnStateScope,
        history: &[MessageWithParts],
        known: &DeveloperContexts,
    ) -> Result<DeveloperContexts, TurnError>;
    async fn context_epoch(&self, scope: &TurnStateScope) -> Result<i64, TurnError>;
    async fn commit_assistant(
        &self,
        scope: &TurnStateScope,
        commit: &AssistantCommit,
    ) -> Result<(), TurnError>;
    async fn append_event(
        &self,
        scope: &TurnStateScope,
        event: NewSessionEvent,
        update: ProviderEventUpdate,
    ) -> Result<SessionEvent, TurnError>;
    async fn commit_tool_parts(
        &self,
        scope: &TurnStateScope,
        parts: &[PartRecord],
        kind: ToolPartCommitKind,
        persisted_at_ms: i64,
    ) -> Result<(), TurnError>;
    async fn consume_input(
        &self,
        scope: &TurnStateScope,
        input: InputMaterialization,
    ) -> Result<(), TurnError>;
    async fn schedule_backoff(
        &self,
        scope: &TurnStateScope,
        checkpoint: ProviderBackoffCheckpoint,
    ) -> Result<(), TurnError>;
    async fn begin_advance(
        &self,
        scope: &TurnStateScope,
        request: &AdvanceRequest,
    ) -> Result<BeginAdvance, AdvanceError>;
    async fn commit_advance(
        &self,
        scope: &TurnStateScope,
        request: &AdvanceRequest,
        admission: &AdvanceAdmission,
        state: AdvanceState,
    ) -> Result<CheckpointRef, AdvanceError>;
}

/// A turn keeps one provider and scope for its whole lifetime.
#[derive(Clone)]
pub struct TurnState<'a> {
    pub(crate) persistence: Arc<dyn TurnPersistence + 'a>,
    pub(crate) scope: TurnStateScope,
}

impl<'a> TurnState<'a> {
    pub fn new(
        persistence: Arc<dyn TurnPersistence + 'a>,
        owner: PrincipalKey,
        session_id: String,
    ) -> Self {
        Self {
            persistence,
            scope: TurnStateScope { owner, session_id },
        }
    }

    pub async fn append(
        &self,
        event: NewSessionEvent,
        update: ProviderEventUpdate,
    ) -> Result<SessionEvent, TurnError> {
        self.persistence
            .append_event(&self.scope, event, update)
            .await
    }

    pub async fn commit_assistant(&self, commit: &AssistantCommit) -> Result<(), TurnError> {
        self.persistence.commit_assistant(&self.scope, commit).await
    }
}
