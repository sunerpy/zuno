//! Local turn persistence; SQL and transaction ownership stay in this adapter.

use super::*;
use std::sync::Mutex;
use zuno_db::message::{MessageStore, PartKind};
use zuno_db::{Connection, open, session};
use zuno_error::DbError;

/// The local adapter borrows the host's existing connection. No mutex guard
/// survives an await or a model/tool invocation.
pub struct SqliteTurnPersistence<'a> {
    connection: Mutex<&'a mut Connection>,
}

impl<'a> SqliteTurnPersistence<'a> {
    pub fn new(connection: &'a mut Connection) -> Self {
        Self {
            connection: Mutex::new(connection),
        }
    }

    fn with<T>(
        &self,
        scope: &TurnStateScope,
        operation: impl FnOnce(&mut Connection) -> Result<T, TurnError>,
    ) -> Result<T, TurnError> {
        let mut connection = self.connection.lock().map_err(|_| DbError::Query {
            source: Box::new(std::io::Error::other("turn persistence lock poisoned")),
        })?;
        session::get_owned(&connection, &scope.session_id, &scope.owner)?;
        operation(&mut connection)
    }
}

fn conflict(id: &str, detail: &str) -> TurnError {
    DbError::Conflict {
        table: "turn_state".to_owned(),
        id: id.to_owned(),
        detail: detail.to_owned(),
    }
    .into()
}

#[async_trait]
impl TurnPersistence for SqliteTurnPersistence<'_> {
    async fn session(&self, scope: &TurnStateScope) -> Result<TurnSession, TurnError> {
        self.with(scope, |connection| {
            let session = session::get_owned(connection, &scope.session_id, &scope.owner)?;
            Ok(TurnSession {
                id: session.id,
                parent_id: session.parent_id,
                directory: session.directory,
            })
        })
    }

    async fn clock(&self, scope: &TurnStateScope) -> Result<i64, TurnError> {
        self.with(scope, |connection| {
            connection
                .query_row(
                    "SELECT CAST(unixepoch('subsec') * 1000 AS INTEGER)",
                    [],
                    |row| row.get(0),
                )
                .map_err(open::map_error)
                .map_err(Into::into)
        })
    }

    async fn touch(&self, scope: &TurnStateScope) -> Result<(), TurnError> {
        self.with(scope, |connection| {
            let transaction = open::immediate_transaction(connection)?;
            session::touch(&transaction, &scope.session_id)?;
            transaction.commit().map_err(open::map_error)?;
            Ok(())
        })
    }

    async fn repair_history(&self, scope: &TurnStateScope) -> Result<usize, TurnError> {
        self.with(scope, |connection| {
            let transaction = open::immediate_transaction(connection)?;
            let count =
                crate::r#loop::repair_missing_tool_outputs(&transaction, &scope.session_id)?;
            transaction.commit().map_err(open::map_error)?;
            Ok(count)
        })
    }

    async fn has_uncertain_calls(&self, scope: &TurnStateScope) -> Result<bool, TurnError> {
        self.with(scope, |connection| {
            Ok(!MessageStore::new(connection)
                .pending_uncertain_tool_calls(&scope.session_id, i64::MIN)?
                .is_empty())
        })
    }

    async fn history(&self, scope: &TurnStateScope) -> Result<Vec<MessageWithParts>, TurnError> {
        self.with(scope, |connection| {
            crate::r#loop::hydrate_retained_history(connection, &scope.session_id)
                .map_err(Into::into)
        })
    }

    async fn legacy_tool_schemas(
        &self,
        scope: &TurnStateScope,
    ) -> Result<LegacyToolSchemas, TurnError> {
        self.with(scope, |connection| {
            crate::r#loop::load_legacy_tool_schema_snapshots(connection, &scope.session_id)
                .map_err(Into::into)
        })
    }

    async fn developer_contexts(
        &self,
        scope: &TurnStateScope,
        history: &[MessageWithParts],
        known: &DeveloperContexts,
    ) -> Result<DeveloperContexts, TurnError> {
        self.with(scope, |connection| {
            crate::r#loop::load_historical_developer_contexts(
                connection,
                &scope.session_id,
                history,
                known,
            )
            .map_err(Into::into)
        })
    }

    async fn context_epoch(&self, scope: &TurnStateScope) -> Result<i64, TurnError> {
        self.with(scope, |connection| {
            crate::r#loop::session_context_epoch(connection, &scope.session_id).map_err(Into::into)
        })
    }

    async fn commit_assistant(
        &self,
        scope: &TurnStateScope,
        commit: &AssistantCommit,
    ) -> Result<(), TurnError> {
        if commit.message.session_id != scope.session_id {
            return Err(conflict(&commit.message.id, "assistant scope differs"));
        }
        self.with(scope, |connection| {
            zuno_db::assistant_commit::commit_assistant(connection, &scope.owner, commit)
                .map_err(Into::into)
        })
    }

    async fn append_event(
        &self,
        scope: &TurnStateScope,
        event: NewSessionEvent,
        update: ProviderEventUpdate,
    ) -> Result<SessionEvent, TurnError> {
        self.with(scope, |connection| {
            let transaction = open::immediate_transaction(connection)?;
            let event = zuno_db::event_log::append_in(&transaction, &scope.session_id, event)?;
            match update {
                ProviderEventUpdate::None => {}
                ProviderEventUpdate::RequestStarted {
                    estimated_prompt_tokens,
                    context_limit,
                } => session::record_provider_request_started(
                    &transaction,
                    &scope.session_id,
                    estimated_prompt_tokens,
                    context_limit,
                )?,
                ProviderEventUpdate::AttemptStarted => {
                    zuno_db::provider_backoff::clear_session(&transaction, &scope.session_id)?;
                }
                ProviderEventUpdate::RequestFinished { request_id } => {
                    zuno_db::provider_backoff::clear_request(
                        &transaction,
                        &scope.session_id,
                        &request_id,
                    )?;
                }
            }
            transaction.commit().map_err(open::map_error)?;
            Ok(event)
        })
    }

    async fn commit_tool_parts(
        &self,
        scope: &TurnStateScope,
        parts: &[PartRecord],
        kind: ToolPartCommitKind,
        persisted_at_ms: i64,
    ) -> Result<(), TurnError> {
        self.with(scope, |connection| {
            let transaction = open::immediate_transaction(connection)?;
            let store = MessageStore::new(&transaction);
            let mut ids = std::collections::BTreeSet::new();
            for part in parts {
                if part.session_id != scope.session_id
                    || part.kind != PartKind::Tool
                    || !ids.insert(&part.id)
                {
                    return Err(conflict(&part.id, "invalid tool part identity"));
                }
                let previous = store.part(&part.id)?;
                if previous.session_id != part.session_id
                    || previous.message_id != part.message_id
                    || previous.kind != part.kind
                    || previous.time_created != part.time_created
                    || previous.data.get("callID") != part.data.get("callID")
                    || previous.data.get("tool") != part.data.get("tool")
                    || previous
                        .data
                        .get("state")
                        .and_then(|state| state.get("input"))
                        != part.data.get("state").and_then(|state| state.get("input"))
                {
                    return Err(conflict(&part.id, "tool invocation binding differs"));
                }
                let status = previous
                    .data
                    .get("state")
                    .and_then(|state| state.get("status"))
                    .and_then(serde_json::Value::as_str);
                if kind == ToolPartCommitKind::Dispatched
                    && previous
                        .data
                        .get("state")
                        .is_some_and(|state| state.get("dispatchedAtMs").is_some())
                {
                    return Err(conflict(
                        &part.id,
                        "an admitted invocation cannot be dispatched again",
                    ));
                }
                if matches!(status, Some("completed" | "error"))
                    && (kind != ToolPartCommitKind::Result || previous.data != part.data)
                {
                    return Err(conflict(
                        &part.id,
                        "a settled invocation cannot be overwritten",
                    ));
                }
                store.put_part_at(part, persisted_at_ms)?;
            }
            transaction.commit().map_err(open::map_error)?;
            Ok(())
        })
    }

    async fn consume_input(
        &self,
        scope: &TurnStateScope,
        mut input: InputMaterialization,
    ) -> Result<(), TurnError> {
        self.with(scope, |connection| {
            if input.message.session_id != scope.session_id
                || input.message.role != zuno_db::message::MessageRole::User
                || input.parts.iter().any(|part| {
                    part.session_id != scope.session_id || part.message_id != input.message.id
                })
            {
                return Err(conflict(&input.message.id, "input scope differs"));
            }
            let transaction = open::immediate_transaction(connection)?;
            let store = MessageStore::new(&transaction);
            let created = zuno_db::message::created_after(
                zuno_db::message::now_millis(),
                store.latest_time_created(&scope.session_id)?,
            );
            if store.find_message(&input.message.id)?.is_some() {
                return Err(conflict(&input.message.id, "input message already exists"));
            }
            input.message.time_created = created;
            input
                .message
                .data
                .insert("time".to_owned(), serde_json::json!({"created":created}));
            if let Some(input_id) = &input.input_id {
                if let Some(stored) =
                    zuno_db::inbox::read_in(&transaction, &scope.session_id, input_id)?
                    && stored
                        .prompt
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        == Some("subagentReport")
                    && let Some(metadata) = stored.prompt.get("metadata")
                {
                    input.message.data.insert(
                        zuno_db::message::TASK_REPORT_METADATA_KEY.to_owned(),
                        metadata.clone(),
                    );
                }
                if zuno_db::inbox::mark_consumed_in(&transaction, &scope.session_id, input_id)?
                    .is_none()
                {
                    return Err(conflict(
                        input_id,
                        "input is not available for consumed settlement",
                    ));
                }
            }
            store.put_message_at(&input.message, created)?;
            for (index, mut part) in input.parts.into_iter().enumerate() {
                match store.part(&part.id) {
                    Ok(_) => return Err(conflict(&part.id, "input part already exists")),
                    Err(DbError::NotFound { .. }) => {}
                    Err(error) => return Err(error.into()),
                }
                part.time_created =
                    created.saturating_add(i64::try_from(index).unwrap_or(i64::MAX));
                store.put_part_at(&part, part.time_created)?;
            }
            transaction.commit().map_err(open::map_error)?;
            Ok(())
        })
    }

    async fn schedule_backoff(
        &self,
        scope: &TurnStateScope,
        checkpoint: ProviderBackoffCheckpoint,
    ) -> Result<(), TurnError> {
        if checkpoint.session_id != scope.session_id {
            return Err(conflict(&checkpoint.request_id, "backoff scope differs"));
        }
        self.with(scope, |connection| {
            zuno_db::provider_backoff::schedule(connection, &checkpoint).map_err(Into::into)
        })
    }

    async fn begin_advance(
        &self,
        scope: &TurnStateScope,
        request: &AdvanceRequest,
    ) -> Result<BeginAdvance, AdvanceError> {
        if scope.session_id != request.run.session_id {
            return Err(AdvanceError::Conflict);
        }
        let mut connection = self.connection.lock().map_err(|_| AdvanceError::Conflict)?;
        crate::advance::begin(&mut connection, request, scope.owner.clone())
    }

    async fn commit_advance(
        &self,
        scope: &TurnStateScope,
        request: &AdvanceRequest,
        admission: &AdvanceAdmission,
        state: AdvanceState,
    ) -> Result<CheckpointRef, AdvanceError> {
        if scope.session_id != request.run.session_id || scope.owner != admission.owner {
            return Err(AdvanceError::Conflict);
        }
        let mut connection = self.connection.lock().map_err(|_| AdvanceError::Conflict)?;
        crate::advance::commit(&mut connection, request, admission, state)
    }
}
