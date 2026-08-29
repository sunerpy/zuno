use std::collections::{BTreeMap, HashMap};

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use base64::Engine as _;
use rusqlite::OptionalExtension;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SessionInput};
use zuno_db::session::{ListQuery, Session, SessionCreate, SortDirection};
use zuno_engine::interrupt::{
    HardInterruptReason, HardInterruptRequest, HardInterruptSource, SoftInterruptMessage,
    SoftInterruptSource,
};
use zuno_engine::r#loop::event_channel;
use zuno_engine::status::{SessionRunGuard, SessionStatus};
use zuno_error::DbError;
use zuno_paths::GLOBAL_PROJECT_ID;

use super::Data;
use super::error::ApiError;
use super::state::ApiState;
use crate::{
    ServerServices, SessionCompactExecution, SessionModelSelection, SessionPromptExecution,
};

#[derive(Debug, Deserialize)]
pub struct SessionListQuery {
    pub(crate) workspace: Option<String>,
    pub(crate) limit: Option<u32>,
    pub(crate) order: Option<SessionOrder>,
    pub(crate) search: Option<String>,
    pub(crate) directory: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) subpath: Option<String>,
    pub(crate) cursor: Option<i64>,
    pub(crate) sort: Option<SessionOrderBy>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SessionOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SessionOrderBy {
    Created,
    Updated,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionBody {
    id: Option<String>,
    agent: Option<String>,
    location: Option<LocationRef>,
}

pub(crate) struct SessionCreateInput {
    pub id: Option<String>,
    pub agent: Option<String>,
    pub directory: Option<String>,
    pub parent_id: Option<String>,
    pub workspace_id: Option<String>,
    pub title: Option<String>,
    pub model: Option<Value>,
    pub metadata: Option<Value>,
    pub permission: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LocationRef {
    directory: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub project_id: String,
    pub workspace_id: Option<String>,
    pub parent_id: Option<String>,
    pub slug: String,
    pub directory: String,
    pub path: Option<String>,
    pub title: String,
    pub version: String,
    pub time: SessionTime,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionTime {
    pub created: i64,
    pub updated: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionCursor {
    pub previous: Option<String>,
    pub next: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionListResponse {
    pub(crate) data: Vec<SessionInfo>,
    pub(crate) cursor: SessionCursor,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionActive {
    #[serde(rename = "type")]
    pub kind: SessionActiveKind,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionActiveKind {
    Running,
}

pub type SessionActiveResponse = Data<BTreeMap<String, SessionActive>>;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MessageOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum CursorDirection {
    Previous,
    Next,
}

#[derive(Debug, Deserialize)]
pub struct MessagesQuery {
    pub(crate) limit: Option<usize>,
    pub(crate) order: Option<MessageOrder>,
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct MessageCursorValue {
    id: String,
    order: MessageOrder,
    direction: CursorDirection,
}

#[derive(Debug, Serialize)]
pub struct MessageCursor {
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MessagesResponse {
    pub(crate) data: Vec<Value>,
    pub(crate) cursor: MessageCursor,
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryResponse {
    data: Vec<Value>,
    has_more: bool,
}

#[derive(Debug, Deserialize)]
pub struct AgentBody {
    agent: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelRefBody {
    pub(crate) id: String,
    #[serde(rename = "providerID")]
    pub(crate) provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) variant: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModelBody {
    model: ModelRefBody,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PromptInputBody {
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) files: Vec<Value>,
    #[serde(default)]
    pub(crate) agents: Vec<Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PromptDelivery {
    Queue,
    Steer,
}

impl PromptDelivery {
    fn into_inbox(self) -> InputDelivery {
        match self {
            Self::Queue => InputDelivery::Queue,
            Self::Steer => InputDelivery::Steer,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PromptBody {
    pub(crate) id: Option<String>,
    pub(crate) prompt: PromptInputBody,
    pub(crate) delivery: Option<PromptDelivery>,
    pub(crate) resume: Option<bool>,
    pub(crate) agent: Option<String>,
    pub(crate) model: Option<ModelRefBody>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptAdmitted {
    admitted_seq: u64,
    pub(crate) id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    prompt: PromptInputBody,
    delivery: PromptDelivery,
    time_created: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedModelSelection {
    provider_id: String,
    model_id: String,
}

impl From<SessionModelSelection> for PersistedModelSelection {
    fn from(model: SessionModelSelection) -> Self {
        Self {
            provider_id: model.provider_id,
            model_id: model.model_id,
        }
    }
}

impl From<PersistedModelSelection> for SessionModelSelection {
    fn from(model: PersistedModelSelection) -> Self {
        Self {
            provider_id: model.provider_id,
            model_id: model.model_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum PersistedDriverInput {
    User {
        prompt: PromptInputBody,
        agent: Option<String>,
        model: Option<PersistedModelSelection>,
    },
    SubagentReport {
        #[serde(rename = "jobID")]
        _job_id: String,
        #[serde(rename = "childSessionID")]
        _child_session_id: String,
        #[serde(rename = "status")]
        _status: String,
        text: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevertStageBody {
    #[serde(rename = "messageID")]
    message_id: String,
    files: Option<bool>,
}

struct MessageRow {
    id: String,
    kind: String,
    data: String,
}

struct MessageCandidate {
    id: String,
    canonical: bool,
}

impl From<Session> for SessionInfo {
    fn from(session: Session) -> Self {
        Self {
            id: session.id,
            project_id: session.project_id,
            workspace_id: session.workspace_id,
            parent_id: session.parent_id,
            slug: session.slug,
            directory: session.directory,
            path: session.path,
            title: session.title,
            version: session.version,
            time: SessionTime {
                created: session.time_created,
                updated: session.time_updated,
            },
        }
    }
}

pub async fn list(
    State(state): State<ApiState>,
    Query(input): Query<SessionListQuery>,
) -> Result<Json<SessionListResponse>, ApiError> {
    let scope = match (input.directory, input.project) {
        (Some(_), Some(_)) => {
            return Err(ApiError::InvalidRequest(
                "directory and project are mutually exclusive",
            ));
        }
        (Some(directory), None) => ListQuery::directory(directory),
        (None, Some(project)) => {
            let query = ListQuery::project(project);
            match input.subpath {
                Some(subpath) => query.with_subpath(subpath),
                None => query,
            }
        }
        (None, None) => {
            if input.subpath.is_some() {
                return Err(ApiError::InvalidRequest("subpath requires project"));
            }
            ListQuery::global()
        }
    };
    let mut query = ListQuery {
        workspace_id: input.workspace,
        search: input.search,
        cursor: input.cursor,
        limit: Some(input.limit.unwrap_or(50).min(200)),
        ..scope
    };
    query.direction = match input.order.unwrap_or(SessionOrder::Desc) {
        SessionOrder::Asc => SortDirection::Ascending,
        SessionOrder::Desc => SortDirection::Descending,
    };
    query = match input.sort {
        Some(SessionOrderBy::Created) => query.created_order(),
        Some(SessionOrderBy::Updated) | None => query,
    };
    let data = state
        .sessions()
        .list(&query)?
        .into_iter()
        .map(SessionInfo::from)
        .collect();
    Ok(Json(SessionListResponse {
        data,
        cursor: SessionCursor {
            previous: None,
            next: None,
        },
    }))
}

pub async fn create(
    State(state): State<ApiState>,
    Json(input): Json<CreateSessionBody>,
) -> Result<Json<Data<SessionInfo>>, ApiError> {
    create_session(
        &state,
        SessionCreateInput {
            id: input.id,
            agent: input.agent,
            directory: input.location.and_then(|location| location.directory),
            parent_id: None,
            workspace_id: None,
            title: None,
            model: None,
            metadata: None,
            permission: None,
        },
    )
    .await
}

pub(crate) async fn create_session(
    state: &ApiState,
    input: SessionCreateInput,
) -> Result<Json<Data<SessionInfo>>, ApiError> {
    let id = input
        .id
        .unwrap_or_else(|| format!("ses_{}", Uuid::new_v4().simple()));
    let directory = input
        .directory
        .unwrap_or_else(|| state.directory().to_owned());
    let mut create = SessionCreate::new(
        &id,
        &id,
        GLOBAL_PROJECT_ID,
        state.directory(),
        directory,
        input.title.unwrap_or_else(|| format!("New session - {id}")),
        env!("CARGO_PKG_VERSION"),
    );
    create.agent = input.agent;
    create.parent_id = input.parent_id;
    create.workspace_id = input.workspace_id;
    create.model = input
        .model
        .map(|value| serde_json::to_string(&value))
        .transpose()
        .map_err(|error| ApiError::MutationFailed(error.to_string()))?;
    create.metadata = input
        .metadata
        .map(|value| serde_json::to_string(&value))
        .transpose()
        .map_err(|error| ApiError::MutationFailed(error.to_string()))?;
    create.permission = input
        .permission
        .map(|value| serde_json::to_string(&value))
        .transpose()
        .map_err(|error| ApiError::MutationFailed(error.to_string()))?;
    let session = state.sessions().create(&create)?.into_session();
    let info = SessionInfo::from(session);
    if let Some(events) = state.events() {
        let properties = json!({"sessionID": id, "info": &info})
            .as_object()
            .expect("the session-created payload is an object")
            .clone();
        events
            .publish(
                &info.id,
                crate::NewEvent::new("session.created", properties)?,
            )
            .await?;
    }
    Ok(Json(Data::new(info)))
}

pub async fn active(Extension(services): Extension<ServerServices>) -> Json<SessionActiveResponse> {
    let data = services
        .runs
        .active_sessions()
        .into_iter()
        .map(|session_id| {
            (
                session_id,
                SessionActive {
                    kind: SessionActiveKind::Running,
                },
            )
        })
        .collect();
    Json(Data::new(data))
}

pub async fn get(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Result<Json<Data<SessionInfo>>, ApiError> {
    let session = state.sessions().get(&session_id)?;
    Ok(Json(Data::new(SessionInfo::from(session))))
}

pub async fn messages(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
    Query(input): Query<MessagesQuery>,
) -> Result<Json<MessagesResponse>, ApiError> {
    state.sessions().get(&session_id)?;
    let limit = input.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::InvalidRequest("limit must be between 1 and 200"));
    }
    if input.cursor.is_some() && input.order.is_some() {
        return Err(ApiError::InvalidRequest(
            "cursor cannot be combined with order",
        ));
    }
    let decoded = input
        .cursor
        .as_deref()
        .map(decode_message_cursor)
        .transpose()?;
    let order = decoded
        .as_ref()
        .map_or(input.order.unwrap_or(MessageOrder::Desc), |cursor| {
            cursor.order
        });
    let direction = decoded
        .as_ref()
        .map_or(CursorDirection::Next, |cursor| cursor.direction);
    let query_order = match (order, direction) {
        (MessageOrder::Asc, CursorDirection::Next)
        | (MessageOrder::Desc, CursorDirection::Previous) => "ASC",
        (MessageOrder::Desc, CursorDirection::Next)
        | (MessageOrder::Asc, CursorDirection::Previous) => "DESC",
    };

    let connection = state.pool().get()?;
    let candidates = message_candidates(
        &connection,
        &session_id,
        decoded.as_ref().map(|cursor| cursor.id.as_str()),
        limit,
        query_order,
    )?;
    let canonical_ids = candidates
        .iter()
        .filter(|candidate| candidate.canonical)
        .map(|candidate| candidate.id.clone())
        .collect::<Vec<_>>();
    let projected_ids = candidates
        .iter()
        .filter(|candidate| !candidate.canonical)
        .map(|candidate| candidate.id.clone())
        .collect::<Vec<_>>();
    let store = zuno_db::message::MessageStore::new(&connection);
    let canonical = store.hydrate(store.messages_by_id(&canonical_ids)?)?;
    let mut canonical = canonical
        .into_iter()
        .map(|message| {
            let id = message.info.id.clone();
            let value = json!({
                "info": message.info.to_json(),
                "parts": message
                    .parts
                    .into_iter()
                    .map(|part| part.to_json())
                    .collect::<Vec<_>>(),
            });
            (id, value)
        })
        .collect::<HashMap<_, _>>();
    let mut projected = projected_messages(&connection, &projected_ids)?;
    let mut data = candidates
        .into_iter()
        .filter_map(|candidate| {
            if candidate.canonical {
                canonical.remove(&candidate.id)
            } else {
                projected.remove(&candidate.id)
            }
        })
        .collect::<Vec<_>>();
    if matches!(direction, CursorDirection::Previous) {
        data.reverse();
    }
    let previous = data
        .first()
        .and_then(message_id)
        .map(|id| encode_message_cursor(id, order, CursorDirection::Previous));
    let next = data
        .last()
        .and_then(message_id)
        .map(|id| encode_message_cursor(id, order, CursorDirection::Next));
    Ok(Json(MessagesResponse {
        data,
        cursor: MessageCursor { previous, next },
    }))
}

pub async fn context(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Result<Json<Data<Vec<Value>>>, ApiError> {
    state.sessions().get(&session_id)?;
    let connection = state.pool().get()?;
    let compaction = connection
        .query_row(
            "SELECT seq FROM session_message \
             WHERE session_id = ?1 AND type = 'compaction' ORDER BY seq DESC LIMIT 1",
            [&session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    let baseline = connection
        .query_row(
            "SELECT baseline_seq FROM session_context_epoch WHERE session_id = ?1",
            [&session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(zuno_db::map_error)?;
    let mut statement = connection
        .prepare(
            "SELECT id, type, data FROM session_message \
             WHERE session_id = ?1 \
               AND (?2 IS NULL OR seq >= ?2 OR (type = 'system' AND ?3 IS NOT NULL AND seq > ?3)) \
               AND (?3 IS NULL OR type != 'system' OR seq > ?3) \
             ORDER BY seq ASC",
        )
        .map_err(zuno_db::map_error)?;
    let rows = statement
        .query_map(
            rusqlite::params![session_id, compaction, baseline],
            message_row,
        )
        .map_err(zuno_db::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(zuno_db::map_error)?;
    Ok(Json(Data::new(decode_messages(rows)?)))
}

pub async fn history(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
    Query(input): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, ApiError> {
    state.sessions().get(&session_id)?;
    if input.after.is_some_and(|after| after < 0) {
        return Err(ApiError::InvalidRequest("after must be non-negative"));
    }
    let limit = input.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::InvalidRequest("limit must be between 1 and 100"));
    }
    let Some(events) = state.events() else {
        return Ok(Json(HistoryResponse {
            data: Vec::new(),
            has_more: false,
        }));
    };
    let page = events.history_page(&session_id, input.after, limit).await?;
    let data = page
        .events
        .into_iter()
        .map(|event| {
            json!({
                "id": event.id(),
                "type": event.event_type(),
                "durable": {
                    "aggregateID": session_id,
                    "seq": event.sequence(),
                    "version": event.version(),
                },
                "data": event.properties(),
            })
        })
        .collect();
    Ok(Json(HistoryResponse {
        data,
        has_more: page.has_more,
    }))
}

pub async fn switch_agent(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
    Json(input): Json<AgentBody>,
) -> Result<StatusCode, ApiError> {
    require_idle(&state, &services, &session_id)?;
    let message_id = format!("msg_{}", Uuid::new_v4().simple());
    let switched = zuno_db::message::now_millis();
    if let Some(events) = state.events() {
        let properties = json!({
            "timestamp": switched,
            "sessionID": session_id,
            "messageID": message_id,
            "agent": input.agent,
        })
        .as_object()
        .expect("the agent switch event is an object")
        .clone();
        events
            .publish(
                &session_id,
                crate::NewEvent::new("session.next.agent.switched", properties)?,
            )
            .await?;
    }
    state
        .sessions()
        .switch_agent_at(&session_id, &message_id, &input.agent, switched)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn switch_model(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
    Json(input): Json<ModelBody>,
) -> Result<StatusCode, ApiError> {
    require_idle(&state, &services, &session_id)?;
    let model = serde_json::to_string(&input.model)
        .map_err(|error| ApiError::MutationFailed(error.to_string()))?;
    let message_id = format!("msg_{}", Uuid::new_v4().simple());
    let switched = zuno_db::message::now_millis();
    if let Some(events) = state.events() {
        let properties = json!({
            "timestamp": switched,
            "sessionID": session_id,
            "messageID": message_id,
            "model": input.model,
        })
        .as_object()
        .expect("the model switch event is an object")
        .clone();
        events
            .publish(
                &session_id,
                crate::NewEvent::new("session.next.model.switched", properties)?,
            )
            .await?;
    }
    state
        .sessions()
        .switch_model_at(&session_id, &message_id, &model, switched)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn prompt(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
    Json(input): Json<PromptBody>,
) -> Result<Json<Data<PromptAdmitted>>, ApiError> {
    let session = state.sessions().get(&session_id)?;
    let executor = Arc::clone(services.mutations.as_ref().ok_or_else(|| {
        ApiError::BackendUnavailable("POST /api/session/{sessionID}/prompt".to_owned())
    })?);
    let PromptBody {
        id,
        prompt,
        delivery,
        resume,
        agent,
        model,
    } = input;
    let message_id = id.unwrap_or_else(|| format!("msg_{}", Uuid::new_v4().simple()));
    let delivery = delivery.unwrap_or(PromptDelivery::Queue);
    let created = zuno_db::message::now_millis();
    let selected_model = match model {
        Some(model) => Some(SessionModelSelection {
            provider_id: model.provider_id,
            model_id: model.id,
        }),
        None => session_model(session.model.as_deref())?,
    };
    let persisted = PersistedDriverInput::User {
        prompt: prompt.clone(),
        agent: agent.or(session.agent),
        model: selected_model.map(PersistedModelSelection::from),
    };
    let admitted_input = SessionInbox::new(state.pool_arc()).admit(NewSessionInput::new(
        message_id.clone(),
        session_id.clone(),
        serde_json::to_value(persisted)
            .map_err(|error| ApiError::MutationFailed(error.to_string()))?,
        delivery.into_inbox(),
        created,
    ))?;
    let admitted_seq = u64::try_from(admitted_input.admitted_sequence)
        .map_err(|_| ApiError::MutationFailed("negative admission sequence".to_owned()))?;
    let admitted = PromptAdmitted {
        admitted_seq,
        id: message_id.clone(),
        session_id: session_id.clone(),
        prompt: prompt.clone(),
        delivery,
        time_created: created,
    };

    if resume != Some(false) {
        match services.runs.begin_turn(&session_id) {
            Ok(guard) => spawn_prompt_driver(state, services, executor, session_id, guard),
            Err(_) if delivery == PromptDelivery::Steer => {
                let _live_turn_won_race = services.runs.queue_soft_interrupt(
                    &session_id,
                    SoftInterruptMessage {
                        input_id: Some(message_id),
                        content: prompt.text,
                        images: Vec::new(),
                        urgent: false,
                        source: SoftInterruptSource::User,
                    },
                );
            }
            Err(_) => {}
        }
    }
    Ok(Json(Data::new(admitted)))
}

fn spawn_prompt_driver(
    state: ApiState,
    services: ServerServices,
    executor: Arc<dyn crate::SessionMutationExecutor>,
    session_id: String,
    guard: SessionRunGuard,
) {
    tokio::spawn(async move {
        let _session_count = zuno_observability::memory::SessionCount::enter();
        let inbox = SessionInbox::new(state.pool_arc());
        let mut guard = Some(guard);
        loop {
            let promoted = match inbox.promote_next(&session_id, None) {
                Ok(promoted) => promoted,
                Err(error) => {
                    eprintln!("session input promotion failed for `{session_id}`: {error}");
                    return;
                }
            };
            let Some(promoted) = promoted else {
                return;
            };
            let promoted_id = promoted.id.clone();
            let request = match prompt_execution(&state, promoted) {
                Ok(request) => request,
                Err(error) => {
                    let _settled = inbox.mark_failed(&session_id, &promoted_id, error.clone());
                    publish_prompt_error(&state, &session_id, &error).await;
                    if !continue_prompt_driver(&inbox, &services, &session_id, &mut guard) {
                        return;
                    }
                    continue;
                }
            };
            let current_guard = guard
                .take()
                .expect("prompt driver owns a guard before each execution");
            let input_id = request.message_id.clone();
            let outcome = run_prompt_execution(
                &state,
                &services,
                Arc::clone(&executor),
                request,
                current_guard,
            )
            .await;
            if let Err(error) = outcome {
                let _settled = inbox.mark_failed(&session_id, &input_id, error.clone());
                eprintln!("session prompt execution failed: {error}");
                publish_prompt_error(&state, &session_id, &error).await;
            }
            if !continue_prompt_driver(&inbox, &services, &session_id, &mut guard) {
                return;
            }
        }
    });
}

fn continue_prompt_driver(
    inbox: &SessionInbox,
    services: &ServerServices,
    session_id: &str,
    guard: &mut Option<SessionRunGuard>,
) -> bool {
    match inbox.pending(session_id) {
        Ok(pending) if pending.is_empty() => false,
        Ok(_) if guard.is_some() => true,
        Ok(_) => match services.runs.begin_turn(session_id) {
            Ok(next_guard) => {
                *guard = Some(next_guard);
                true
            }
            Err(_) => false,
        },
        Err(error) => {
            eprintln!("session input inspection failed for `{session_id}`: {error}");
            false
        }
    }
}

fn prompt_execution(
    state: &ApiState,
    input: SessionInput,
) -> Result<SessionPromptExecution, String> {
    let stored = serde_json::from_value::<PersistedDriverInput>(input.prompt)
        .map_err(|error| format!("invalid persisted session input `{}`: {error}", input.id))?;
    let session = state
        .sessions()
        .get(&input.session_id)
        .map_err(|error| error.to_string())?;
    match stored {
        PersistedDriverInput::User {
            prompt,
            agent,
            model,
        } => Ok(SessionPromptExecution {
            session_id: input.session_id,
            directory: session.directory.into(),
            message_id: input.id,
            prompt: prompt.text,
            agent,
            model: model.map(SessionModelSelection::from),
        }),
        PersistedDriverInput::SubagentReport { text, .. } => {
            let model =
                session_model(session.model.as_deref()).map_err(|error| error.to_string())?;
            Ok(SessionPromptExecution {
                session_id: input.session_id,
                directory: session.directory.into(),
                message_id: input.id,
                prompt: text,
                agent: session.agent,
                model,
            })
        }
    }
}

async fn run_prompt_execution(
    state: &ApiState,
    services: &ServerServices,
    executor: Arc<dyn crate::SessionMutationExecutor>,
    request: SessionPromptExecution,
    guard: SessionRunGuard,
) -> Result<(), String> {
    let fanout = services.events.clone();
    let durable_events = state.events().cloned();
    let event_session_id = request.session_id.clone();
    let (sender, receiver) = event_channel();
    if let Some(events) = durable_events.as_ref() {
        let (outcome, ()) = tokio::join!(
            executor.prompt(request, guard, sender),
            events.forward_engine_events(&event_session_id, &fanout, receiver)
        );
        outcome
    } else {
        let (outcome, ()) = tokio::join!(
            executor.prompt(request, guard, sender),
            fanout.forward_engine_events(receiver)
        );
        outcome
    }
}

async fn publish_prompt_error(state: &ApiState, session_id: &str, error: &str) {
    let Some(events) = state.events() else {
        return;
    };
    let properties = object(json!({
        "sessionID": session_id,
        "message": error,
    }));
    if let Err(publish_error) = events
        .publish(
            session_id,
            crate::NewEvent::new("session.error", properties)
                .expect("fixed session error type is valid"),
        )
        .await
    {
        eprintln!("failed to publish HTTP turn error for `{session_id}`: {publish_error}");
    }
}

pub async fn compact(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    compact_session(state, services, session_id, None, false).await
}

pub(crate) async fn compact_session(
    state: ApiState,
    services: ServerServices,
    session_id: String,
    requested_model: Option<SessionModelSelection>,
    automatic: bool,
) -> Result<StatusCode, ApiError> {
    let session = state.sessions().get(&session_id)?;
    let executor = services.mutations.as_ref().ok_or_else(|| {
        ApiError::BackendUnavailable("POST /api/session/{sessionID}/compact".to_owned())
    })?;
    let guard = services
        .runs
        .begin_turn(&session_id)
        .map_err(|error| ApiError::Conflict(error.to_string()))?;
    let request = SessionCompactExecution {
        session_id: session_id.clone(),
        directory: session.directory.into(),
        agent: session.agent,
        model: match requested_model {
            Some(model) => Some(model),
            None => session_model(session.model.as_deref())?,
        },
        automatic,
    };
    let fanout = services.events.clone();
    let durable_events = state.events().cloned();
    let (sender, receiver) = event_channel();
    // Counted for the same reason the prompt path is, and it is a *separate* entry point:
    // a compaction re-reads the whole transcript and re-prompts the model, so it is one of
    // the most memory-expensive things this server does. Leaving it uncounted made the
    // sampler under-report active sessions and attribute that growth to the heap instead.
    let _session = zuno_observability::memory::SessionCount::enter();
    let outcome = if let Some(events) = durable_events.as_ref() {
        let (outcome, ()) = tokio::join!(
            executor.compact(request, guard, sender),
            events.forward_engine_events(&session_id, &fanout, receiver)
        );
        outcome
    } else {
        let (outcome, ()) = tokio::join!(
            executor.compact(request, guard, sender),
            fanout.forward_engine_events(receiver)
        );
        outcome
    };
    outcome.map_err(ApiError::MutationFailed)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn wait(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.sessions().get(&session_id)?;
    services.runs.wait_until_idle(&session_id).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn interrupt(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.sessions().get(&session_id)?;
    services.runs.abort(
        &session_id,
        HardInterruptRequest::new(HardInterruptSource::Api, HardInterruptReason::UserCancel),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn revert_stage(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
    Json(input): Json<RevertStageBody>,
) -> Result<Json<Data<Value>>, ApiError> {
    require_idle(&state, &services, &session_id)?;
    let mut revert = json!({"messageID": input.message_id});
    if input.files == Some(false) {
        revert["files"] = Value::Array(Vec::new());
    }
    let raw = serde_json::to_string(&revert)
        .map_err(|error| ApiError::MutationFailed(error.to_string()))?;
    state.sessions().stage_revert_at(
        &session_id,
        revert["messageID"]
            .as_str()
            .expect("the marker contains a string"),
        &raw,
        zuno_db::message::now_millis(),
    )?;
    Ok(Json(Data::new(revert)))
}

pub async fn revert_clear(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_idle(&state, &services, &session_id)?;
    state
        .sessions()
        .clear_revert_at(&session_id, zuno_db::message::now_millis())?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn revert_commit(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_idle(&state, &services, &session_id)?;
    let committed = state
        .sessions()
        .commit_revert_at(&session_id, zuno_db::message::now_millis())?;
    let _ = committed;
    Ok(StatusCode::NO_CONTENT)
}

fn require_idle(
    state: &ApiState,
    services: &ServerServices,
    session_id: &str,
) -> Result<Session, ApiError> {
    let session = state.sessions().get(session_id)?;
    if services.runs.status(session_id) == SessionStatus::Busy {
        return Err(ApiError::Conflict(format!(
            "session `{session_id}` already has an active turn"
        )));
    }
    Ok(session)
}

fn session_model(raw: Option<&str>) -> Result<Option<SessionModelSelection>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let model: ModelRefBody =
        serde_json::from_str(raw).map_err(|error| ApiError::MutationFailed(error.to_string()))?;
    Ok(Some(SessionModelSelection {
        provider_id: model.provider_id,
        model_id: model.id,
    }))
}

fn message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: row.get(0)?,
        kind: row.get(1)?,
        data: row.get(2)?,
    })
}

fn decode_messages(rows: Vec<MessageRow>) -> Result<Vec<Value>, ApiError> {
    rows.into_iter().map(decode_message).collect()
}

fn decode_message(row: MessageRow) -> Result<Value, ApiError> {
    let mut data = serde_json::from_str::<Map<String, Value>>(&row.data).map_err(|source| {
        DbError::Decode {
            table: "session_message".to_owned(),
            source,
        }
    })?;
    data.insert("id".to_owned(), Value::String(row.id));
    data.insert("type".to_owned(), Value::String(row.kind));
    Ok(Value::Object(data))
}

fn message_candidates(
    connection: &rusqlite::Connection,
    session_id: &str,
    anchor_id: Option<&str>,
    limit: usize,
    query_order: &str,
) -> Result<Vec<MessageCandidate>, ApiError> {
    let comparison = if query_order == "ASC" { ">" } else { "<" };
    let combined = "SELECT id, time_created, 1 AS canonical FROM message WHERE session_id = ?1 \
                    UNION ALL \
                    SELECT projected.id, projected.time_created, 0 AS canonical \
                    FROM session_message AS projected WHERE projected.session_id = ?1 \
                      AND NOT EXISTS (SELECT 1 FROM message AS canonical \
                                      WHERE canonical.session_id = ?1 \
                                        AND canonical.id = projected.id)";
    let sql = if anchor_id.is_some() {
        format!(
            "WITH combined AS ({combined}), \
             anchor AS (SELECT time_created, id FROM combined WHERE id = ?2 LIMIT 1) \
             SELECT combined.id, combined.canonical FROM combined, anchor \
             WHERE combined.time_created {comparison} anchor.time_created \
                OR (combined.time_created = anchor.time_created \
                    AND combined.id {comparison} anchor.id) \
             ORDER BY combined.time_created {query_order}, combined.id {query_order} LIMIT ?3"
        )
    } else {
        format!(
            "WITH combined AS ({combined}) \
             SELECT id, canonical FROM combined \
             ORDER BY time_created {query_order}, id {query_order} LIMIT ?2"
        )
    };
    let mut statement = connection.prepare(&sql).map_err(zuno_db::map_error)?;
    let rows = if let Some(anchor_id) = anchor_id {
        statement
            .query_map(
                rusqlite::params![session_id, anchor_id, limit as i64],
                |row| {
                    Ok(MessageCandidate {
                        id: row.get(0)?,
                        canonical: row.get(1)?,
                    })
                },
            )
            .map_err(zuno_db::map_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(zuno_db::map_error)?
    } else {
        statement
            .query_map(rusqlite::params![session_id, limit as i64], |row| {
                Ok(MessageCandidate {
                    id: row.get(0)?,
                    canonical: row.get(1)?,
                })
            })
            .map_err(zuno_db::map_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(zuno_db::map_error)?
    };
    Ok(rows)
}

fn projected_messages(
    connection: &rusqlite::Connection,
    message_ids: &[String],
) -> Result<HashMap<String, Value>, ApiError> {
    if message_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = (1..=message_ids.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT id, type, data FROM session_message WHERE id IN ({placeholders})");
    let mut statement = connection.prepare(&sql).map_err(zuno_db::map_error)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(message_ids.iter()), message_row)
        .map_err(zuno_db::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(zuno_db::map_error)?;
    rows.into_iter()
        .map(|row| {
            let id = row.id.clone();
            decode_message(row).map(|value| (id, value))
        })
        .collect()
}

fn message_id(message: &Value) -> Option<&str> {
    message
        .get("info")
        .and_then(|info| info.get("id"))
        .or_else(|| message.get("id"))
        .and_then(Value::as_str)
}

fn object(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("fixed session event payloads are objects")
        .clone()
}

fn decode_message_cursor(input: &str) -> Result<MessageCursorValue, ApiError> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|_| ApiError::InvalidRequest("invalid cursor"))?;
    let cursor = serde_json::from_slice::<MessageCursorValue>(&bytes)
        .map_err(|_| ApiError::InvalidRequest("invalid cursor"))?;
    if !cursor.id.starts_with("msg_") {
        return Err(ApiError::InvalidRequest("invalid cursor"));
    }
    Ok(cursor)
}

fn encode_message_cursor(id: &str, order: MessageOrder, direction: CursorDirection) -> String {
    let value = MessageCursorValue {
        id: id.to_owned(),
        order,
        direction,
    };
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&value).expect("message cursor has an infallible JSON representation"),
    )
}
