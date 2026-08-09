use std::collections::BTreeMap;

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use base64::Engine as _;
use oc_db::session::{ListQuery, Session, SessionCreate, SortDirection};
use oc_engine::r#loop::event_channel;
use oc_engine::status::SessionStatus;
use oc_error::DbError;
use oc_paths::GLOBAL_PROJECT_ID;
use rusqlite::OptionalExtension;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::Data;
use super::error::ApiError;
use super::state::ApiState;
use crate::{
    ServerServices, SessionCompactExecution, SessionModelSelection, SessionPromptExecution,
};

#[derive(Debug, Deserialize)]
pub struct SessionListQuery {
    workspace: Option<String>,
    limit: Option<u32>,
    order: Option<SessionOrder>,
    search: Option<String>,
    directory: Option<String>,
    project: Option<String>,
    subpath: Option<String>,
    cursor: Option<i64>,
    sort: Option<SessionOrderBy>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SessionOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SessionOrderBy {
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
    pub data: Vec<SessionInfo>,
    pub cursor: SessionCursor,
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
enum MessageOrder {
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
    limit: Option<usize>,
    order: Option<MessageOrder>,
    cursor: Option<String>,
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
    data: Vec<Value>,
    cursor: MessageCursor,
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
    id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModelBody {
    model: ModelRefBody,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PromptInputBody {
    text: String,
    #[serde(default)]
    files: Vec<Value>,
    #[serde(default)]
    agents: Vec<Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptDelivery {
    Steer,
    Queue,
}

#[derive(Debug, Deserialize)]
pub struct PromptBody {
    id: Option<String>,
    prompt: PromptInputBody,
    delivery: Option<PromptDelivery>,
    resume: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptAdmitted {
    admitted_seq: u64,
    id: String,
    #[serde(rename = "sessionID")]
    session_id: String,
    prompt: PromptInputBody,
    delivery: PromptDelivery,
    time_created: i64,
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
    let id = input
        .id
        .unwrap_or_else(|| format!("ses_{}", Uuid::new_v4().simple()));
    let directory = input
        .location
        .and_then(|location| location.directory)
        .unwrap_or_else(|| state.directory().to_owned());
    let mut create = SessionCreate::new(
        &id,
        &id,
        GLOBAL_PROJECT_ID,
        state.directory(),
        directory,
        format!("New session - {id}"),
        env!("CARGO_PKG_VERSION"),
    );
    create.agent = input.agent;
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
    let anchor = decoded
        .as_ref()
        .map(|cursor| {
            connection
                .query_row(
                    "SELECT seq FROM session_message WHERE session_id = ?1 AND id = ?2",
                    rusqlite::params![session_id, cursor.id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(oc_db::map_error)
        })
        .transpose()?
        .flatten();
    if decoded.is_some() && anchor.is_none() {
        return Ok(Json(MessagesResponse {
            data: Vec::new(),
            cursor: MessageCursor {
                previous: None,
                next: None,
            },
        }));
    }
    let comparison = if query_order == "ASC" { ">" } else { "<" };
    let sql = if anchor.is_some() {
        format!(
            "SELECT id, type, data FROM session_message \
             WHERE session_id = ?1 AND seq {comparison} ?2 ORDER BY seq {query_order} LIMIT ?3"
        )
    } else {
        format!(
            "SELECT id, type, data FROM session_message \
             WHERE session_id = ?1 ORDER BY seq {query_order} LIMIT ?2"
        )
    };
    let mut statement = connection.prepare(&sql).map_err(oc_db::map_error)?;
    let rows = if let Some(anchor) = anchor {
        statement
            .query_map(
                rusqlite::params![session_id, anchor, limit as i64],
                message_row,
            )
            .map_err(oc_db::map_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(oc_db::map_error)?
    } else {
        statement
            .query_map(rusqlite::params![session_id, limit as i64], message_row)
            .map_err(oc_db::map_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(oc_db::map_error)?
    };
    let ordered = if matches!(direction, CursorDirection::Previous) {
        rows.into_iter().rev().collect()
    } else {
        rows
    };
    let data = decode_messages(ordered)?;
    let previous = data
        .first()
        .and_then(|message| message.get("id"))
        .and_then(Value::as_str)
        .map(|id| encode_message_cursor(id, order, CursorDirection::Previous));
    let next = data
        .last()
        .and_then(|message| message.get("id"))
        .and_then(Value::as_str)
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
        .map_err(oc_db::map_error)?;
    let baseline = connection
        .query_row(
            "SELECT baseline_seq FROM session_context_epoch WHERE session_id = ?1",
            [&session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(oc_db::map_error)?;
    let mut statement = connection
        .prepare(
            "SELECT id, type, data FROM session_message \
             WHERE session_id = ?1 \
               AND (?2 IS NULL OR seq >= ?2 OR (type = 'system' AND ?3 IS NOT NULL AND seq > ?3)) \
               AND (?3 IS NULL OR type != 'system' OR seq > ?3) \
             ORDER BY seq ASC",
        )
        .map_err(oc_db::map_error)?;
    let rows = statement
        .query_map(
            rusqlite::params![session_id, compaction, baseline],
            message_row,
        )
        .map_err(oc_db::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(oc_db::map_error)?;
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
    let switched = oc_db::message::now_millis();
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
    let switched = oc_db::message::now_millis();
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
    let executor = services.mutations.as_ref().ok_or_else(|| {
        ApiError::BackendUnavailable("POST /api/session/{sessionID}/prompt".to_owned())
    })?;
    let guard = services
        .runs
        .begin_turn(&session_id)
        .map_err(|error| ApiError::Conflict(error.to_string()))?;
    let message_id = input
        .id
        .unwrap_or_else(|| format!("msg_{}", Uuid::new_v4().simple()));
    let delivery = input.delivery.unwrap_or(PromptDelivery::Steer);
    let created = oc_db::message::now_millis();
    let admitted = PromptAdmitted {
        admitted_seq: 0,
        id: message_id.clone(),
        session_id: session_id.clone(),
        prompt: input.prompt.clone(),
        delivery,
        time_created: created,
    };

    if input.resume != Some(false) {
        let request = SessionPromptExecution {
            session_id,
            directory: session.directory.into(),
            message_id,
            prompt: input.prompt.text,
            agent: session.agent,
            model: session_model(session.model.as_deref())?,
        };
        let signal = guard.interrupt_signal().clone();
        let executor = Arc::clone(executor);
        let fanout = services.events.clone();
        let (sender, receiver) = event_channel();
        tokio::spawn(async move {
            let (outcome, ()) = tokio::join!(
                executor.prompt(request, signal, sender),
                fanout.forward_engine_events(receiver)
            );
            if let Err(error) = outcome {
                eprintln!("session prompt execution failed: {error}");
            }
            drop(guard);
        });
    } else {
        drop(guard);
    }
    Ok(Json(Data::new(admitted)))
}

pub async fn compact(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
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
        session_id,
        directory: session.directory.into(),
        agent: session.agent,
        model: session_model(session.model.as_deref())?,
    };
    executor
        .compact(request, guard.interrupt_signal().clone())
        .await
        .map_err(ApiError::MutationFailed)?;
    drop(guard);
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
    services.runs.abort(&session_id);
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
        oc_db::message::now_millis(),
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
        .clear_revert_at(&session_id, oc_db::message::now_millis())?;
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
        .commit_revert_at(&session_id, oc_db::message::now_millis())?;
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
