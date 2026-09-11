//! The Worker owns a transport and execution directory, never a database pool.

use super::wire::*;
use super::*;

#[async_trait]
pub trait StateTransport: Send + Sync {
    /// An unsuccessful mutation is not retried without authoritative reconciliation.
    async fn exchange(&self, request: StateRequest) -> Result<StateResponse, TurnStateError>;
}

pub struct RemoteTurnPersistence {
    transport: Arc<dyn StateTransport>,
    scope: TurnStateScope,
    executor_directory: String,
}

impl RemoteTurnPersistence {
    pub fn new(
        transport: Arc<dyn StateTransport>,
        scope: TurnStateScope,
        executor_directory: String,
    ) -> Result<Self, TurnStateError> {
        if executor_directory.is_empty()
            || executor_directory.len() > 4096
            || executor_directory.contains(['\0', '\r', '\n'])
        {
            return Err(TurnStateError::InvalidData);
        }
        Ok(Self {
            transport,
            scope,
            executor_directory,
        })
    }

    async fn exchange(
        &self,
        scope: &TurnStateScope,
        command: StateCommand,
    ) -> Result<StateReply, StateFailure> {
        if scope != &self.scope {
            return Err(TurnStateError::NotFound.into());
        }
        let response = self.transport.exchange(StateRequest::new(command)).await?;
        if response.version != WORKER_PROTOCOL_VERSION {
            return Err(TurnStateError::InvalidData.into());
        }
        response.result
    }

    async fn call(
        &self,
        scope: &TurnStateScope,
        command: StateCommand,
    ) -> Result<StateReply, TurnError> {
        self.exchange(scope, command)
            .await
            .map_err(|failure| match failure {
                StateFailure::State(error) => error.into(),
                _ => TurnStateError::InvalidData.into(),
            })
    }

    async fn done(&self, scope: &TurnStateScope, command: StateCommand) -> Result<(), TurnError> {
        match self.call(scope, command).await? {
            StateReply::Done => Ok(()),
            _ => Err(TurnStateError::InvalidData.into()),
        }
    }
}

fn advance_error(failure: StateFailure) -> AdvanceError {
    match failure {
        StateFailure::State(error) => AdvanceError::Turn(error.into()),
        StateFailure::CheckpointConflict => AdvanceError::Conflict,
        StateFailure::NeedsInspection => AdvanceError::NeedsInspection,
        StateFailure::InvalidCheckpoint => {
            AdvanceError::InvalidCheckpoint("invalid remote checkpoint".to_owned())
        }
    }
}

#[async_trait]
impl TurnPersistence for RemoteTurnPersistence {
    async fn session(&self, scope: &TurnStateScope) -> Result<TurnSession, TurnError> {
        match self.call(scope, StateCommand::Session).await? {
            StateReply::Session { id, parent_id } if id.as_str() == scope.session_id => {
                Ok(TurnSession {
                    id: id.to_string(),
                    parent_id: parent_id.map(|id| id.to_string()),
                    directory: Some(self.executor_directory.clone()),
                })
            }
            _ => Err(TurnStateError::InvalidData.into()),
        }
    }
    async fn clock(&self, scope: &TurnStateScope) -> Result<i64, TurnError> {
        match self.call(scope, StateCommand::Clock).await? {
            StateReply::Integer(value) => Ok(value),
            _ => Err(TurnStateError::InvalidData.into()),
        }
    }
    async fn touch(&self, scope: &TurnStateScope) -> Result<(), TurnError> {
        self.done(scope, StateCommand::Touch).await
    }
    async fn repair_history(&self, scope: &TurnStateScope) -> Result<usize, TurnError> {
        match self.call(scope, StateCommand::RepairHistory).await? {
            StateReply::Count(value) => Ok(value),
            _ => Err(TurnStateError::InvalidData.into()),
        }
    }
    async fn has_uncertain_calls(&self, scope: &TurnStateScope) -> Result<bool, TurnError> {
        match self.call(scope, StateCommand::HasUncertainCalls).await? {
            StateReply::Boolean(value) => Ok(value),
            _ => Err(TurnStateError::InvalidData.into()),
        }
    }
    async fn history(&self, scope: &TurnStateScope) -> Result<Vec<MessageWithParts>, TurnError> {
        let StateReply::History(value) = self.call(scope, StateCommand::History).await? else {
            return Err(TurnStateError::InvalidData.into());
        };
        let messages = value
            .into_iter()
            .map(MessageWithParts::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        if messages
            .iter()
            .any(|message| message.info.session_id != scope.session_id)
        {
            return Err(TurnStateError::InvalidData.into());
        }
        Ok(messages)
    }
    async fn legacy_tool_schemas(
        &self,
        scope: &TurnStateScope,
    ) -> Result<LegacyToolSchemas, TurnError> {
        match self.call(scope, StateCommand::LegacyToolSchemas).await? {
            StateReply::LegacyToolSchemas(value) => Ok(value),
            _ => Err(TurnStateError::InvalidData.into()),
        }
    }
    async fn developer_contexts(
        &self,
        scope: &TurnStateScope,
        history: &[MessageWithParts],
        known: &DeveloperContexts,
    ) -> Result<DeveloperContexts, TurnError> {
        match self
            .call(
                scope,
                StateCommand::DeveloperContexts {
                    history: history.iter().cloned().map(Into::into).collect(),
                    known: known.clone(),
                },
            )
            .await?
        {
            StateReply::DeveloperContexts(value) => Ok(value),
            _ => Err(TurnStateError::InvalidData.into()),
        }
    }
    async fn context_epoch(&self, scope: &TurnStateScope) -> Result<i64, TurnError> {
        match self.call(scope, StateCommand::ContextEpoch).await? {
            StateReply::Integer(value) => Ok(value),
            _ => Err(TurnStateError::InvalidData.into()),
        }
    }
    async fn commit_assistant(
        &self,
        scope: &TurnStateScope,
        commit: &AssistantCommit,
    ) -> Result<(), TurnError> {
        self.done(scope, StateCommand::CommitAssistant(commit.into()))
            .await
    }
    async fn append_event(
        &self,
        scope: &TurnStateScope,
        event: NewSessionEvent,
        update: ProviderEventUpdate,
    ) -> Result<SessionEvent, TurnError> {
        match self
            .call(
                scope,
                StateCommand::AppendEvent {
                    event: event.into(),
                    update,
                },
            )
            .await?
        {
            StateReply::Event(value) if value.session_id.as_str() == scope.session_id => {
                Ok(value.into())
            }
            _ => Err(TurnStateError::InvalidData.into()),
        }
    }
    async fn commit_tool_parts(
        &self,
        scope: &TurnStateScope,
        parts: &[PartRecord],
        kind: ToolPartCommitKind,
        persisted_at_ms: i64,
    ) -> Result<(), TurnError> {
        self.done(
            scope,
            StateCommand::CommitToolParts {
                parts: parts.iter().cloned().map(Into::into).collect(),
                kind,
                persisted_at_ms,
            },
        )
        .await
    }
    async fn consume_input(
        &self,
        scope: &TurnStateScope,
        input: InputMaterialization,
    ) -> Result<(), TurnError> {
        self.done(scope, StateCommand::ConsumeInput(input.try_into()?))
            .await
    }
    async fn schedule_backoff(
        &self,
        scope: &TurnStateScope,
        checkpoint: ProviderBackoffCheckpoint,
    ) -> Result<(), TurnError> {
        self.done(scope, StateCommand::ScheduleBackoff(checkpoint))
            .await
    }
    async fn begin_advance(
        &self,
        scope: &TurnStateScope,
        request: &AdvanceRequest,
    ) -> Result<BeginAdvance, AdvanceError> {
        match self
            .exchange(scope, StateCommand::BeginAdvance(request.into()))
            .await
            .map_err(advance_error)?
        {
            StateReply::BeginAdvance(value) => Ok(value),
            _ => Err(AdvanceError::InvalidCheckpoint(
                "invalid remote admission".to_owned(),
            )),
        }
    }
    async fn commit_advance(
        &self,
        scope: &TurnStateScope,
        request: &AdvanceRequest,
        admission: &AdvanceAdmission,
        state: AdvanceState,
    ) -> Result<CheckpointRef, AdvanceError> {
        match self
            .exchange(
                scope,
                StateCommand::CommitAdvance {
                    request: request.into(),
                    admission: Box::new(admission.clone()),
                    state,
                },
            )
            .await
            .map_err(advance_error)?
        {
            StateReply::Checkpoint(value)
                if value.session_id() == scope.session_id
                    && value.turn_id() == request.run.turn_id =>
            {
                Ok(value)
            }
            _ => Err(AdvanceError::InvalidCheckpoint(
                "invalid remote checkpoint".to_owned(),
            )),
        }
    }
}
