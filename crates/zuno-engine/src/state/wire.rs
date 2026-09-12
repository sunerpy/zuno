//! Internal Worker protocol. These records must never enter a public Web DTO.

use super::{
    DeveloperContexts, InputMaterialization, LegacyToolSchemas, ProviderEventUpdate,
    ToolPartCommitKind, TurnStateError,
};
use crate::advance::{AdvanceAdmission, AdvanceRequest, AdvanceState, BeginAdvance, CheckpointRef};
use crate::r#loop::RunTurnRequest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::num::NonZeroU32;
use zuno_db::assistant_commit::AssistantCommit;
use zuno_db::event_log::{NewSessionEvent, SessionEvent};
use zuno_db::message::{MessageRecord, MessageWithParts, PartRecord};
use zuno_db::provider_backoff::ProviderBackoffCheckpoint;
use zuno_types::identity::{InputId, SessionId, TurnId};

pub const WORKER_PROTOCOL_VERSION: u32 = 6;
pub const MAX_WORKER_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredPart {
    pub body: Value,
    pub created_at_ms: i64,
}

impl From<PartRecord> for StoredPart {
    fn from(part: PartRecord) -> Self {
        Self {
            body: part.to_json(),
            created_at_ms: part.time_created,
        }
    }
}
impl TryFrom<StoredPart> for PartRecord {
    type Error = TurnStateError;
    fn try_from(part: StoredPart) -> Result<Self, Self::Error> {
        PartRecord::from_json(part.body, part.created_at_ms)
            .map_err(|_| TurnStateError::InvalidData)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredMessage {
    pub body: Value,
    pub parts: Vec<StoredPart>,
}
impl From<MessageWithParts> for StoredMessage {
    fn from(message: MessageWithParts) -> Self {
        Self {
            body: message.info.to_json(),
            parts: message.parts.into_iter().map(Into::into).collect(),
        }
    }
}
impl TryFrom<StoredMessage> for MessageWithParts {
    type Error = TurnStateError;
    fn try_from(message: StoredMessage) -> Result<Self, Self::Error> {
        let info =
            MessageRecord::from_json(message.body).map_err(|_| TurnStateError::InvalidData)?;
        let parts = message
            .parts
            .into_iter()
            .map(PartRecord::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        if parts
            .iter()
            .any(|part| part.message_id != info.id || part.session_id != info.session_id)
        {
            return Err(TurnStateError::InvalidData);
        }
        Ok(Self { info, parts })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssistantWrite {
    pub message: StoredMessage,
    pub persisted_at_ms: i64,
    pub context_limit: Option<i64>,
    pub context_usage: Option<zuno_types::context_usage::ContextUsageWrite>,
}
impl From<&AssistantCommit> for AssistantWrite {
    fn from(commit: &AssistantCommit) -> Self {
        Self {
            message: MessageWithParts {
                info: commit.message.clone(),
                parts: commit.parts.clone(),
            }
            .into(),
            persisted_at_ms: commit.persisted_at_ms,
            context_limit: commit.context_limit,
            context_usage: commit.context_usage.clone(),
        }
    }
}
impl TryFrom<AssistantWrite> for AssistantCommit {
    type Error = TurnStateError;
    fn try_from(value: AssistantWrite) -> Result<Self, Self::Error> {
        let message = MessageWithParts::try_from(value.message)?;
        let commit = Self {
            message: message.info,
            parts: message.parts,
            persisted_at_ms: value.persisted_at_ms,
            context_limit: value.context_limit,
            context_usage: value.context_usage,
        };
        zuno_db::assistant_commit::validate_commit(&commit, None, &[])
            .map_err(|_| TurnStateError::InvalidData)?;
        Ok(commit)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderRequestWrite {
    pub assistant: StoredMessage,
    pub event: EventDraft,
    pub estimated_prompt_tokens: u64,
    pub context_limit: Option<u64>,
    pub context: crate::context_usage::ContextRequestPreparation,
}

impl From<super::ProviderRequestCommit> for ProviderRequestWrite {
    fn from(commit: super::ProviderRequestCommit) -> Self {
        Self {
            assistant: MessageWithParts {
                info: commit.assistant,
                parts: Vec::new(),
            }
            .into(),
            event: commit.event.into(),
            estimated_prompt_tokens: commit.estimated_prompt_tokens,
            context_limit: commit.context_limit,
            context: commit.context,
        }
    }
}

impl TryFrom<ProviderRequestWrite> for super::ProviderRequestCommit {
    type Error = TurnStateError;
    fn try_from(commit: ProviderRequestWrite) -> Result<Self, Self::Error> {
        let assistant = MessageWithParts::try_from(commit.assistant)?;
        if !assistant.parts.is_empty()
            || assistant.info.role != zuno_db::message::MessageRole::Assistant
        {
            return Err(TurnStateError::InvalidData);
        }
        Ok(Self {
            assistant: assistant.info,
            event: commit.event.try_into()?,
            estimated_prompt_tokens: commit.estimated_prompt_tokens,
            context_limit: commit.context_limit,
            context: commit.context,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputWrite {
    pub input_id: InputId,
    pub turn_id: Option<TurnId>,
    pub message: StoredMessage,
}
impl TryFrom<InputMaterialization> for InputWrite {
    type Error = TurnStateError;
    fn try_from(value: InputMaterialization) -> Result<Self, Self::Error> {
        let input_id = InputId::new(value.input_id.ok_or(TurnStateError::InvalidData)?)
            .map_err(|_| TurnStateError::InvalidData)?;
        Ok(Self {
            input_id,
            turn_id: value
                .turn_id
                .map(TurnId::new)
                .transpose()
                .map_err(|_| TurnStateError::InvalidData)?,
            message: MessageWithParts {
                info: value.message,
                parts: value.parts,
            }
            .into(),
        })
    }
}
impl TryFrom<InputWrite> for InputMaterialization {
    type Error = TurnStateError;
    fn try_from(value: InputWrite) -> Result<Self, Self::Error> {
        let message = MessageWithParts::try_from(value.message)?;
        if message.info.id != value.input_id.as_str() {
            return Err(TurnStateError::InvalidData);
        }
        Ok(Self {
            input_id: Some(value.input_id.to_string()),
            turn_id: value.turn_id.map(String::from),
            message: message.info,
            parts: message.parts,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdvanceCall {
    pub run: RunTurnRequest,
    pub configuration_digest: String,
    pub checkpoint: Option<CheckpointRef>,
    pub max_steps: u32,
}
impl From<&AdvanceRequest> for AdvanceCall {
    fn from(value: &AdvanceRequest) -> Self {
        Self {
            run: value.run.clone(),
            configuration_digest: value.configuration_digest().to_owned(),
            checkpoint: value.checkpoint.clone(),
            max_steps: value.max_steps.get(),
        }
    }
}
impl TryFrom<AdvanceCall> for AdvanceRequest {
    type Error = TurnStateError;
    fn try_from(value: AdvanceCall) -> Result<Self, Self::Error> {
        SessionId::new(&value.run.session_id).map_err(|_| TurnStateError::InvalidData)?;
        TurnId::new(&value.run.turn_id).map_err(|_| TurnStateError::InvalidData)?;
        let maximum = NonZeroU32::new(value.max_steps)
            .filter(|steps| steps.get() <= 64)
            .ok_or(TurnStateError::InvalidData)?;
        let mut request = Self::new(value.run, value.configuration_digest, maximum)
            .map_err(|_| TurnStateError::InvalidData)?;
        request.checkpoint = value.checkpoint;
        Ok(request)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventDraft {
    pub event_type: String,
    pub properties: serde_json::Map<String, Value>,
}
impl From<NewSessionEvent> for EventDraft {
    fn from(event: NewSessionEvent) -> Self {
        Self {
            event_type: event.event_type,
            properties: event.properties,
        }
    }
}
impl TryFrom<EventDraft> for NewSessionEvent {
    type Error = TurnStateError;
    fn try_from(event: EventDraft) -> Result<Self, Self::Error> {
        Self::new(event.event_type, event.properties).map_err(|_| TurnStateError::InvalidData)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventReceipt {
    pub id: String,
    pub session_id: SessionId,
    pub sequence: i64,
    pub event_type: String,
    pub version: u32,
    pub properties: serde_json::Map<String, Value>,
}
impl TryFrom<SessionEvent> for EventReceipt {
    type Error = TurnStateError;
    fn try_from(value: SessionEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id,
            session_id: SessionId::new(value.session_id)
                .map_err(|_| TurnStateError::InvalidData)?,
            sequence: value.sequence,
            event_type: value.event_type,
            version: value.version,
            properties: value.properties,
        })
    }
}
impl From<EventReceipt> for SessionEvent {
    fn from(value: EventReceipt) -> Self {
        Self {
            id: value.id,
            session_id: value.session_id.to_string(),
            sequence: value.sequence,
            event_type: value.event_type,
            version: value.version,
            properties: value.properties,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StateCommand {
    Session,
    ContextUsage,
    CommitContextUsage(Box<zuno_types::context_usage::ContextUsageWrite>),
    StartProviderRequest(Box<ProviderRequestWrite>),
    ApplicableInputs {
        candidates: Vec<String>,
    },
    MarkInputsApplied {
        turn_id: TurnId,
        input_ids: Vec<InputId>,
        at_ms: i64,
    },
    Clock,
    Touch,
    RepairHistory,
    HasUncertainCalls,
    History,
    LegacyToolSchemas,
    DeveloperContexts {
        history: Vec<StoredMessage>,
        known: DeveloperContexts,
    },
    ContextEpoch,
    CommitAssistant(Box<AssistantWrite>),
    AppendEvent {
        event: EventDraft,
        update: ProviderEventUpdate,
    },
    CommitToolParts {
        parts: Vec<StoredPart>,
        kind: ToolPartCommitKind,
        persisted_at_ms: i64,
    },
    ConsumeInput(InputWrite),
    ScheduleBackoff(ProviderBackoffCheckpoint),
    BeginAdvance(Box<AdvanceCall>),
    CommitAdvance {
        request: Box<AdvanceCall>,
        admission: Box<AdvanceAdmission>,
        state: Box<AdvanceState>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StateReply {
    ContextUsage(Box<crate::context_usage::ContextUsageSeed>),
    ProviderRequest {
        event: EventReceipt,
        context: Box<zuno_types::context_usage::ContextUsageTracker>,
    },
    InputIds(Vec<String>),
    Done,
    Session {
        id: SessionId,
        parent_id: Option<SessionId>,
    },
    Integer(i64),
    Count(usize),
    Boolean(bool),
    History(Vec<StoredMessage>),
    LegacyToolSchemas(LegacyToolSchemas),
    DeveloperContexts(DeveloperContexts),
    Event(EventReceipt),
    BeginAdvance(BeginAdvance),
    Checkpoint(CheckpointRef),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StateFailure {
    State(TurnStateError),
    CheckpointConflict,
    NeedsInspection,
    InvalidCheckpoint,
}

impl From<crate::r#loop::TurnError> for StateFailure {
    fn from(error: crate::r#loop::TurnError) -> Self {
        use crate::r#loop::TurnError;
        use zuno_error::DbError;
        Self::State(match error {
            TurnError::State(error) => error,
            TurnError::Database(DbError::Busy { .. }) => TurnStateError::Unavailable,
            TurnError::Database(DbError::NotFound { .. }) => TurnStateError::NotFound,
            TurnError::Database(DbError::Conflict { .. }) => TurnStateError::Conflict,
            _ => TurnStateError::InvalidData,
        })
    }
}
impl From<crate::advance::AdvanceError> for StateFailure {
    fn from(error: crate::advance::AdvanceError) -> Self {
        use crate::advance::AdvanceError;
        match error {
            AdvanceError::Conflict => Self::CheckpointConflict,
            AdvanceError::NeedsInspection => Self::NeedsInspection,
            AdvanceError::Turn(error) => error.into(),
            AdvanceError::Database(error) => crate::r#loop::TurnError::Database(error).into(),
            _ => Self::InvalidCheckpoint,
        }
    }
}
impl From<TurnStateError> for StateFailure {
    fn from(error: TurnStateError) -> Self {
        Self::State(error)
    }
}

/// The authenticated data owner supplies the scope and provider. Neither is
/// read from the request body. The provider still owns policy and lease fencing.
pub async fn execute(
    provider: &dyn super::TurnPersistence,
    scope: &super::TurnStateScope,
    command: StateCommand,
) -> Result<StateReply, StateFailure> {
    Ok(match command {
        StateCommand::ContextUsage => {
            StateReply::ContextUsage(Box::new(provider.context_usage(scope).await?))
        }
        StateCommand::CommitContextUsage(update) => {
            provider.commit_context_usage(scope, &update).await?;
            StateReply::Done
        }
        StateCommand::StartProviderRequest(commit) => {
            let receipt = provider
                .start_provider_request(scope, super::ProviderRequestCommit::try_from(*commit)?)
                .await?;
            StateReply::ProviderRequest {
                event: receipt.event.try_into()?,
                context: Box::new(receipt.context),
            }
        }
        StateCommand::ApplicableInputs { candidates } => {
            if candidates.len() > 4096
                || candidates
                    .iter()
                    .any(|id| id.is_empty() || id.len() > 1024 || id.contains('\0'))
            {
                return Err(TurnStateError::InvalidData.into());
            }
            StateReply::InputIds(provider.applicable_inputs(scope, &candidates).await?)
        }
        StateCommand::MarkInputsApplied {
            turn_id,
            input_ids,
            at_ms,
        } => {
            provider
                .mark_inputs_applied(
                    scope,
                    turn_id.as_str(),
                    &input_ids.into_iter().map(String::from).collect::<Vec<_>>(),
                    at_ms,
                )
                .await?;
            StateReply::Done
        }
        StateCommand::Session => {
            let session = provider.session(scope).await?;
            StateReply::Session {
                id: SessionId::new(session.id).map_err(|_| TurnStateError::InvalidData)?,
                parent_id: session
                    .parent_id
                    .map(SessionId::new)
                    .transpose()
                    .map_err(|_| TurnStateError::InvalidData)?,
            }
        }
        StateCommand::Clock => StateReply::Integer(provider.clock(scope).await?),
        StateCommand::Touch => {
            provider.touch(scope).await?;
            StateReply::Done
        }
        StateCommand::RepairHistory => StateReply::Count(provider.repair_history(scope).await?),
        StateCommand::HasUncertainCalls => {
            StateReply::Boolean(provider.has_uncertain_calls(scope).await?)
        }
        StateCommand::History => StateReply::History(
            provider
                .history(scope)
                .await?
                .into_iter()
                .map(Into::into)
                .collect(),
        ),
        StateCommand::LegacyToolSchemas => {
            StateReply::LegacyToolSchemas(provider.legacy_tool_schemas(scope).await?)
        }
        StateCommand::DeveloperContexts { history, known } => {
            let history = history
                .into_iter()
                .map(MessageWithParts::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            StateReply::DeveloperContexts(
                provider.developer_contexts(scope, &history, &known).await?,
            )
        }
        StateCommand::ContextEpoch => StateReply::Integer(provider.context_epoch(scope).await?),
        StateCommand::CommitAssistant(commit) => {
            provider
                .commit_assistant(scope, &AssistantCommit::try_from(*commit)?)
                .await?;
            StateReply::Done
        }
        StateCommand::AppendEvent { event, update } => StateReply::Event(
            provider
                .append_event(scope, NewSessionEvent::try_from(event)?, update)
                .await?
                .try_into()?,
        ),
        StateCommand::CommitToolParts {
            parts,
            kind,
            persisted_at_ms,
        } => {
            let parts = parts
                .into_iter()
                .map(PartRecord::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            provider
                .commit_tool_parts(scope, &parts, kind, persisted_at_ms)
                .await?;
            StateReply::Done
        }
        StateCommand::ConsumeInput(input) => {
            provider
                .consume_input(scope, InputMaterialization::try_from(input)?)
                .await?;
            StateReply::Done
        }
        StateCommand::ScheduleBackoff(checkpoint) => {
            provider.schedule_backoff(scope, checkpoint).await?;
            StateReply::Done
        }
        StateCommand::BeginAdvance(request) => StateReply::BeginAdvance(
            provider
                .begin_advance(scope, &AdvanceRequest::try_from(*request)?)
                .await?,
        ),
        StateCommand::CommitAdvance {
            request,
            admission,
            state,
        } => StateReply::Checkpoint(
            provider
                .commit_advance(
                    scope,
                    &AdvanceRequest::try_from(*request)?,
                    &admission,
                    *state,
                )
                .await?,
        ),
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateRequest {
    pub version: u32,
    pub command: StateCommand,
}
impl StateRequest {
    pub fn new(command: StateCommand) -> Self {
        Self {
            version: WORKER_PROTOCOL_VERSION,
            command,
        }
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, TurnStateError> {
        if bytes.len() > MAX_WORKER_FRAME_BYTES {
            return Err(TurnStateError::InvalidData);
        }
        let request: Self =
            serde_json::from_slice(bytes).map_err(|_| TurnStateError::InvalidData)?;
        if request.version != WORKER_PROTOCOL_VERSION {
            return Err(TurnStateError::InvalidData);
        }
        Ok(request)
    }
    pub fn encode(&self) -> Result<Vec<u8>, TurnStateError> {
        let bytes = serde_json::to_vec(self).map_err(|_| TurnStateError::InvalidData)?;
        if bytes.len() > MAX_WORKER_FRAME_BYTES || self.version != WORKER_PROTOCOL_VERSION {
            return Err(TurnStateError::InvalidData);
        }
        Ok(bytes)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateResponse {
    pub version: u32,
    pub result: Result<StateReply, StateFailure>,
}
impl StateResponse {
    pub fn new(result: Result<StateReply, StateFailure>) -> Self {
        Self {
            version: WORKER_PROTOCOL_VERSION,
            result,
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>, TurnStateError> {
        let bytes = serde_json::to_vec(self).map_err(|_| TurnStateError::InvalidData)?;
        if bytes.len() > MAX_WORKER_FRAME_BYTES {
            return Err(TurnStateError::InvalidData);
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, TurnStateError> {
        if bytes.len() > MAX_WORKER_FRAME_BYTES {
            return Err(TurnStateError::InvalidData);
        }
        let response: Self =
            serde_json::from_slice(bytes).map_err(|_| TurnStateError::InvalidData)?;
        if response.version != WORKER_PROTOCOL_VERSION {
            return Err(TurnStateError::InvalidData);
        }
        Ok(response)
    }
}
