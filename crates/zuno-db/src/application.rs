//! SQLite provider for the scoped application port.
//!
//! Only the trusted host registers local workspace locations. Blocking database
//! work owns a bounded permit until it actually finishes, including after a
//! request future is dropped.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::params;
use serde_json::json;
use tokio::sync::Semaphore;
use zuno_application::{
    ApplicationError, CreateSession, InputReceipt, InputState, QueueText, SessionCursor,
    SessionPage, SessionPageRequest, SessionPersistence, SessionSummary,
};
use zuno_error::DbError;
use zuno_types::execution::InputTriggerKind;
use zuno_types::identity::{
    InputId, PrincipalKey, PrincipalScope, ProjectId, SessionId, WorkspaceId,
};

use crate::event_log::{NewSessionEvent, append_in, latest_of_type_in};
use crate::inbox::{
    InputDelivery, NewSessionInput, SessionInput, SubmissionState, admit_in, read_in,
};
use crate::{Pool, session};

const CREATED_EVENT: &str = "application.session.created";

/// A host-owned local binding. It is intentionally not a wire DTO.
#[derive(Debug, Clone)]
pub struct LocalWorkspace {
    pub id: WorkspaceId,
    pub owner: PrincipalKey,
    pub project_id: ProjectId,
    pub root: PathBuf,
}

#[derive(Clone)]
pub struct SqliteSessionPersistence {
    pool: Arc<Pool>,
    principal: PrincipalScope,
    workspaces: Arc<BTreeMap<WorkspaceId, LocalWorkspace>>,
    slots: Arc<Semaphore>,
}

impl SqliteSessionPersistence {
    pub fn new(
        pool: Arc<Pool>,
        principal: PrincipalScope,
        workspaces: Vec<LocalWorkspace>,
        concurrency: NonZeroUsize,
    ) -> Result<Self, ApplicationError> {
        if concurrency.get() > 64 {
            return Err(ApplicationError::Invalid(
                "SQLite application concurrency must be between 1 and 64".to_owned(),
            ));
        }
        let mut registered = BTreeMap::new();
        for workspace in workspaces {
            if !workspace.root.is_absolute() || workspace.root.to_str().is_none() {
                return Err(ApplicationError::Invalid(
                    "a local workspace requires an absolute UTF-8 path".to_owned(),
                ));
            }
            if registered.insert(workspace.id.clone(), workspace).is_some() {
                return Err(ApplicationError::Invalid(
                    "duplicate workspace identity".to_owned(),
                ));
            }
        }
        Ok(Self {
            pool,
            principal,
            workspaces: Arc::new(registered),
            slots: Arc::new(Semaphore::new(concurrency.get())),
        })
    }

    /// Build another authenticated view while sharing the pool and capacity.
    /// This host API is not authentication and is never exposed as a client DTO.
    #[must_use]
    pub fn for_principal(&self, principal: PrincipalScope) -> Self {
        Self {
            principal,
            ..self.clone()
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(Self) -> Result<T, ApplicationError> + Send + 'static,
    ) -> Result<T, ApplicationError> {
        let permit = Arc::clone(&self.slots)
            .acquire_owned()
            .await
            .map_err(|_| ApplicationError::Unavailable)?;
        let provider = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work(provider)
        })
        .await
        .map_err(ApplicationError::storage)?
    }
}

#[async_trait]
impl SessionPersistence for SqliteSessionPersistence {
    fn principal(&self) -> &PrincipalScope {
        &self.principal
    }

    async fn create(&self, request: CreateSession) -> Result<SessionSummary, ApplicationError> {
        let workspace = self
            .workspaces
            .get(&request.workspace_id)
            .filter(|workspace| workspace.owner == self.principal.owner())
            .cloned()
            .ok_or(ApplicationError::NotFound)?;
        self.blocking(move |provider| {
            let id = stable_id(
                &provider.principal,
                "create-session",
                request.request_id.as_str(),
                None,
            );
            let id = format!("ses_{id}");
            let request_digest = zuno_orchestration::sha256_json(&json!(request));
            let owner = provider.principal.owner();
            let stored = provider
                .pool
                .transaction(|tx| {
                    if session::find(tx, &id)?.is_some() {
                        let stored = session::get_owned(tx, &id, &owner)?;
                        let creation = latest_of_type_in(tx, &id, CREATED_EVENT)?;
                        if !creation.is_some_and(|event| {
                            event
                                .properties
                                .get("requestDigest")
                                .and_then(serde_json::Value::as_str)
                                == Some(&request_digest)
                        }) {
                            return Err(conflict(&id));
                        }
                        return Ok(stored);
                    }
                    let root = workspace.root.to_str().expect("validated local binding");
                    let input = session::SessionCreate::new(
                        &id,
                        &id,
                        workspace.project_id.as_str(),
                        root,
                        root,
                        &request.title,
                        env!("CARGO_PKG_VERSION"),
                    )
                    .with_workspace(workspace.id.as_str())
                    .with_owner(owner.clone());
                    session::create(tx, &input)?;
                    append_in(
                        tx,
                        &id,
                        NewSessionEvent::new(
                            CREATED_EVENT,
                            json!({
                                "requestID": request.request_id,
                                "requestDigest": request_digest,
                                "workspaceID": workspace.id,
                                "principal": provider.principal,
                            })
                            .as_object()
                            .expect("object")
                            .clone(),
                        )?,
                    )?;
                    session::get_owned(tx, &id, &owner)
                })
                .map_err(map_database)?;
            summary(stored)
        })
        .await
    }

    async fn get(&self, id: &SessionId) -> Result<SessionSummary, ApplicationError> {
        let id = id.clone();
        self.blocking(move |provider| {
            let connection = provider.pool.get().map_err(map_database)?;
            summary(
                session::get_owned(&connection, id.as_str(), &provider.principal.owner())
                    .map_err(map_database)?,
            )
        })
        .await
    }

    async fn list(&self, request: SessionPageRequest) -> Result<SessionPage, ApplicationError> {
        self.blocking(move |provider| {
            let connection = provider.pool.get().map_err(map_database)?;
            let owner = provider.principal.owner();
            let mut statement = connection
                .prepare(
                    "SELECT s.id,s.workspace_id,s.title,s.time_created,s.time_updated
                 FROM session s JOIN session_ownership o ON o.session_id=s.id
                 WHERE o.tenant_id=?1 AND o.principal_id=?2 AND s.time_archived IS NULL
                   AND (?3 IS NULL OR s.time_updated<?3 OR (s.time_updated=?3 AND s.id<?4))
                 ORDER BY s.time_updated DESC,s.id DESC LIMIT ?5",
                )
                .map_err(|error| map_database(crate::open::map_error(error)))?;
            let rows = statement
                .query_map(
                    params![
                        owner.tenant_id.as_str(),
                        owner.principal_id.as_str(),
                        request.after.as_ref().map(|cursor| cursor.updated_at),
                        request
                            .after
                            .as_ref()
                            .map(|cursor| cursor.session_id.as_str()),
                        u32::from(request.limit.get()) + 1,
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    },
                )
                .map_err(|error| map_database(crate::open::map_error(error)))?;
            let mut items = Vec::new();
            for row in rows {
                let (id, workspace, title, created_at, updated_at) =
                    row.map_err(|error| map_database(crate::open::map_error(error)))?;
                items.push(SessionSummary {
                    id: SessionId::new(id).map_err(ApplicationError::storage)?,
                    workspace_id: workspace
                        .map(WorkspaceId::new)
                        .transpose()
                        .map_err(ApplicationError::storage)?,
                    title,
                    created_at,
                    updated_at,
                });
            }
            let more = items.len() > usize::from(request.limit.get());
            items.truncate(usize::from(request.limit.get()));
            let next = more
                .then(|| {
                    items.last().map(|last| SessionCursor {
                        updated_at: last.updated_at,
                        session_id: last.id.clone(),
                    })
                })
                .flatten();
            Ok(SessionPage { items, next })
        })
        .await
    }

    async fn queue_text(&self, request: QueueText) -> Result<InputReceipt, ApplicationError> {
        self.blocking(move |provider| {
            let id = format!(
                "msg_{}",
                stable_id(
                    &provider.principal,
                    "queue-text",
                    request.request_id.as_str(),
                    Some(request.session_id.as_str()),
                )
            );
            let source_key = format!("application:{id}");
            let input = provider
                .pool
                .transaction(|tx| {
                    let session = session::get_owned(
                        tx,
                        request.session_id.as_str(),
                        &provider.principal.owner(),
                    )?;
                    if let Some(existing) = read_in(tx, request.session_id.as_str(), &id)? {
                        if existing.source_key.as_deref() != Some(&source_key)
                            || existing.prompt["kind"] != "user"
                            || existing.prompt["prompt"]["text"] != request.text
                        {
                            return Err(conflict(&id));
                        }
                        return Ok(existing);
                    }
                    let model = session
                        .model
                        .as_deref()
                        .map(|raw| {
                            session::decode_model_reference(raw).ok_or_else(|| {
                                crate::event_log::query_error(std::io::Error::other(
                                    "invalid stored model selection",
                                ))
                            })
                        })
                        .transpose()?;
                    let prompt = json!({
                        "kind":"user",
                        "prompt":{"text":request.text,"files":[],"agents":[]},
                        "agent": session.agent,
                        "model": model.map(|model| json!({
                            "providerId":model.provider_id,"modelId":model.model_id,
                        })),
                    });
                    let input = admit_in(
                        tx,
                        NewSessionInput::new(
                            &id,
                            request.session_id.as_str(),
                            prompt,
                            InputDelivery::Queue,
                            crate::message::now_millis(),
                        )
                        .with_source_key(source_key)
                        .with_trigger_kind(InputTriggerKind::User),
                    )?;
                    append_in(
                        tx,
                        request.session_id.as_str(),
                        NewSessionEvent::new(
                            "application.input.queued",
                            json!({
                                "inputID":input.id,
                                "requestID":request.request_id,
                                "principal":provider.principal,
                            })
                            .as_object()
                            .expect("object")
                            .clone(),
                        )?,
                    )?;
                    Ok(input)
                })
                .map_err(map_database)?;
            receipt(input)
        })
        .await
    }
}

fn stable_id(
    scope: &PrincipalScope,
    kind: &str,
    request_id: &str,
    session_id: Option<&str>,
) -> String {
    zuno_orchestration::sha256_json(&json!([
        kind,
        scope.owner(),
        scope.client_id(),
        session_id,
        request_id,
    ]))
}

fn summary(session: session::Session) -> Result<SessionSummary, ApplicationError> {
    Ok(SessionSummary {
        id: SessionId::new(session.id).map_err(ApplicationError::storage)?,
        workspace_id: session
            .workspace_id
            .map(WorkspaceId::new)
            .transpose()
            .map_err(ApplicationError::storage)?,
        title: session.title,
        created_at: session.time_created,
        updated_at: session.time_updated,
    })
}

fn receipt(input: SessionInput) -> Result<InputReceipt, ApplicationError> {
    let state = match input.state {
        SubmissionState::Queued => InputState::Queued,
        SubmissionState::Steering => InputState::Steering,
        SubmissionState::Promoted => InputState::Promoted,
        SubmissionState::Consumed => InputState::Consumed,
        SubmissionState::Cancelled => InputState::Cancelled,
        SubmissionState::Failed => InputState::Failed,
        SubmissionState::Admitting => {
            return Err(ApplicationError::storage(std::io::Error::other(
                "a durable input cannot remain admitting",
            )));
        }
    };
    Ok(InputReceipt {
        id: InputId::new(input.id).map_err(ApplicationError::storage)?,
        session_id: SessionId::new(input.session_id).map_err(ApplicationError::storage)?,
        state,
        admitted_cursor: u64::try_from(input.admitted_sequence)
            .map_err(ApplicationError::storage)?
            .to_string(),
    })
}

fn conflict(id: &str) -> DbError {
    DbError::Conflict {
        table: "application_request".to_owned(),
        id: id.to_owned(),
        detail: "request identity was already used for different content".to_owned(),
    }
}

fn map_database(error: DbError) -> ApplicationError {
    match error {
        DbError::NotFound { .. } => ApplicationError::NotFound,
        DbError::Conflict { .. } => ApplicationError::Conflict,
        DbError::Busy { .. } => ApplicationError::Unavailable,
        other => ApplicationError::storage(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_paths::DbLocation;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_caller_does_not_release_capacity_while_its_blocking_work_runs() {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).unwrap());
        let provider = SqliteSessionPersistence::new(
            pool,
            PrincipalScope::local(),
            Vec::new(),
            NonZeroUsize::MIN,
        )
        .unwrap();
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = provider.clone();
        let caller = tokio::spawn(async move {
            worker
                .blocking(move |_| {
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    Ok(())
                })
                .await
        });
        waiting.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(provider.slots.available_permits(), 0);
        release.send(()).unwrap();
        let permit =
            tokio::time::timeout(std::time::Duration::from_secs(10), provider.slots.acquire())
                .await
                .unwrap()
                .unwrap();
        drop(permit);
        assert_eq!(provider.slots.available_permits(), 1);
    }
}
