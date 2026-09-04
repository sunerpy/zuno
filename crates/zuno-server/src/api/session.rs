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
use zuno_db::inbox::{
    DurableInputKind, InputDelivery, NewSessionInput, SessionInbox, SessionInput,
};
use zuno_db::session::{ListQuery, Session, SessionCreate, SortDirection};
use zuno_engine::admission::{InputAdmission, SessionInputAdmission, SteeringContent, TurnLease};
use zuno_engine::interrupt::{HardInterruptReason, HardInterruptRequest, HardInterruptSource};
use zuno_engine::r#loop::event_channel;
use zuno_engine::report::ReportBatch;
use zuno_engine::status::{SessionRunGuard, SessionStatus};
use zuno_error::DbError;
use zuno_llm::event::RequestContentBlock;
use zuno_paths::GLOBAL_PROJECT_ID;

use super::Data;
use super::error::ApiError;
use super::state::ApiState;
use crate::{
    ServerServices, SessionCompactExecution, SessionModelSelection, SessionPromptExecution,
    SessionReportExecution,
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

/// The one durable inbox shape this surface writes: an HTTP prompt body together
/// with the agent and model overrides the request carried.
///
/// Every other shape this driver runs was written elsewhere and is read back
/// through [`DurableInputKind`], so there is a single decoder per published kind.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename = "user", rename_all = "camelCase")]
struct PersistedUserPrompt {
    prompt: PromptInputBody,
    agent: Option<String>,
    model: Option<PersistedModelSelection>,
}

/// One pending inbox row the HTTP prompt driver is the consumer for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrivenInput {
    /// An HTTP prompt body with its agent and model overrides.
    UserPrompt,
    /// A settled asynchronous report, driven together with the session's whole batch.
    Report,
    /// An answered human request, delivered as plain text on its own.
    HumanAnswer,
}

impl DrivenInput {
    /// Classify one pending row, or `None` when this driver must leave it alone.
    ///
    /// A TUI submission and an ACP prompt carry structured payloads only their own
    /// surface can render, and a turn host message is never observed pending.
    /// Promoting one of those here and then failing to decode it would settle
    /// another surface's durable input as `failed` instead of leaving it for the
    /// driver that can run it. A payload no writer publishes is left pending for the
    /// same reason: an unrecognized shape is preserved rather than destroyed.
    ///
    /// Settled reports are classified apart from answered human requests even though
    /// both carry plain text: a report is one member of a batch this surface claims
    /// and delivers in a single turn, while a human answer is the reply to one
    /// request and has no batch to join.
    fn of(prompt: &Value) -> Option<Self> {
        match DurableInputKind::classify(prompt)? {
            DurableInputKind::User => Some(Self::UserPrompt),
            DurableInputKind::SubagentReport
            | DurableInputKind::ProductAgentReport
            | DurableInputKind::WorkflowReport
            | DurableInputKind::CouncilReport
            | DurableInputKind::BackgroundExecutionReport => Some(Self::Report),
            DurableInputKind::HumanRequestAnswer => Some(Self::HumanAnswer),
            DurableInputKind::TuiPrompt
            | DurableInputKind::AcpPrompt
            | DurableInputKind::HostMessage => None,
        }
    }
}

/// One promoted unit of work the HTTP driver owns until it is consumed or failed.
enum DrivenPromotion {
    /// An HTTP prompt body with its own agent and model overrides.
    Prompt(SessionInput),
    /// One answered human request.
    HumanAnswer(SessionInput),
    /// Every settled report the session had pending, as one provider request.
    Reports(ReportBatch),
}

impl DrivenPromotion {
    /// The durable rows this promotion is responsible for settling.
    fn input_ids(&self) -> Vec<String> {
        match self {
            Self::Prompt(input) | Self::HumanAnswer(input) => vec![input.id.clone()],
            Self::Reports(batch) => batch
                .reports()
                .iter()
                .map(|report| report.input_id.clone())
                .collect(),
        }
    }
}

/// One promoted unit of work as its executor receives it.
enum DrivenRequest {
    Prompt(SessionPromptExecution),
    Reports(SessionReportExecution),
}

impl DrivenRequest {
    fn session_id(&self) -> &str {
        match self {
            Self::Prompt(request) => &request.session_id,
            Self::Reports(request) => &request.session_id,
        }
    }
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

pub async fn learning(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Result<Json<Data<zuno_types::LearningStateProjection>>, ApiError> {
    let session = state.sessions().get(&session_id)?;
    let projection = zuno_learning::LearningProjectionService::new(state.pool_arc())
        .snapshot(&session_id, &session.project_id)?;
    Ok(Json(Data::new(projection)))
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
        mut prompt,
        delivery,
        resume,
        agent,
        model,
    } = input;
    let admitted_attachments = admit_prompt_files(&state, &mut prompt)?;
    let message_id = id.unwrap_or_else(|| format!("msg_{}", Uuid::new_v4().simple()));
    let delivery = delivery.unwrap_or(PromptDelivery::Queue);
    let created = zuno_db::message::now_millis();
    let selected_model = match model {
        Some(model) => Some(SessionModelSelection {
            provider_id: model.provider_id,
            model_id: model.id,
        }),
        None => session_model(session.model.as_deref()),
    };
    let persisted = PersistedUserPrompt {
        prompt: prompt.clone(),
        agent: agent.or(session.agent),
        model: selected_model.map(PersistedModelSelection::from),
    };
    // Admission writes the durable row before contending for the live-turn lease,
    // so a prompt that arrives mid-turn is recorded and steered rather than lost.
    let admission =
        SessionInputAdmission::new(SessionInbox::new(state.pool_arc()), services.runs.clone());
    let steering = (delivery == PromptDelivery::Steer)
        .then(|| SteeringContent::user(prompt.text.clone()).with_attachments(admitted_attachments));
    let lease = if resume == Some(false) {
        TurnLease::Deferred
    } else {
        TurnLease::Acquire
    };
    let admitted_input = admission.admit(
        NewSessionInput::new(
            message_id.clone(),
            session_id.clone(),
            serde_json::to_value(persisted)
                .map_err(|error| ApiError::MutationFailed(error.to_string()))?,
            delivery.into_inbox(),
            created,
        ),
        lease,
        steering,
    )?;
    let admitted_seq = u64::try_from(admitted_input.input().admitted_sequence)
        .map_err(|_| ApiError::MutationFailed("negative admission sequence".to_owned()))?;
    let admitted = PromptAdmitted {
        admitted_seq,
        id: message_id,
        session_id: session_id.clone(),
        prompt,
        delivery,
        time_created: created,
    };
    if let InputAdmission::Drive { guard, .. } = admitted_input {
        spawn_prompt_driver(state, services, executor, session_id, guard);
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
            let promoted = match promote_next_driven(&inbox, &session_id) {
                Ok(promoted) => promoted,
                Err(error) => {
                    eprintln!("session input promotion failed for `{session_id}`: {error}");
                    return;
                }
            };
            let Some(promoted) = promoted else {
                return;
            };
            let input_ids = promoted.input_ids();
            let request = match driven_request(&state, &session_id, promoted) {
                Ok(request) => request,
                Err(error) => {
                    settle_failed(&inbox, &session_id, &input_ids, &error);
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
            let outcome = run_driven_execution(
                &state,
                &services,
                Arc::clone(&executor),
                request,
                current_guard,
            )
            .await;
            if let Err(error) = outcome {
                settle_failed(&inbox, &session_id, &input_ids, &error);
                eprintln!("session prompt execution failed: {error}");
                publish_prompt_error(&state, &session_id, &error).await;
            }
            if !continue_prompt_driver(&inbox, &services, &session_id, &mut guard) {
                return;
            }
        }
    });
}

/// Settle every row one failed execution had already promoted.
///
/// A batch shares its provider request, so a failure belongs to all of its members:
/// leaving the rest `promoted` would strand them out of both the pending queue and
/// the transcript.
fn settle_failed(inbox: &SessionInbox, session_id: &str, input_ids: &[String], error: &str) {
    for input_id in input_ids {
        let _settled = inbox.mark_failed(session_id, input_id, error.to_owned());
    }
}

/// Promote the oldest unit of work this surface drives, stepping over the rows
/// another client owns so they stay pending for their own driver.
///
/// The read is followed by a promotion keyed on that exact row, so a concurrent
/// driver that claimed it first simply moves this loop to the next candidate.
///
/// A settled report is promoted with the session's whole pending report batch in one
/// transaction, because the batch becomes one provider request. Driving reports one
/// row at a time made a fan-out that settled together cost one model turn per report,
/// each announcing a state a later report in the same batch had already replaced. A
/// promoted report carrying no model-visible text cannot become a user message, so it
/// is settled `failed` with that reason instead of stalling the batch behind it.
fn promote_next_driven(
    inbox: &SessionInbox,
    session_id: &str,
) -> Result<Option<DrivenPromotion>, DbError> {
    for pending in inbox.pending(session_id)? {
        let Some(driven) = DrivenInput::of(&pending.prompt) else {
            continue;
        };
        match driven {
            DrivenInput::Report => {
                let batch = ReportBatch::project(&inbox.promote_pending_async(session_id)?);
                for input_id in batch.undecodable() {
                    let _settled = inbox.mark_failed(
                        session_id,
                        input_id,
                        format!(
                            "persisted session input `{input_id}` carries no model-visible text"
                        ),
                    );
                }
                if batch.is_empty() {
                    continue;
                }
                return Ok(Some(DrivenPromotion::Reports(batch)));
            }
            DrivenInput::UserPrompt => {
                if let Some(promoted) = inbox.promote_id(session_id, &pending.id)? {
                    return Ok(Some(DrivenPromotion::Prompt(promoted)));
                }
            }
            DrivenInput::HumanAnswer => {
                if let Some(promoted) = inbox.promote_id(session_id, &pending.id)? {
                    return Ok(Some(DrivenPromotion::HumanAnswer(promoted)));
                }
            }
        }
    }
    Ok(None)
}

/// Whether any pending row is one this surface drives.
fn has_driven_pending(inbox: &SessionInbox, session_id: &str) -> Result<bool, DbError> {
    Ok(inbox
        .pending(session_id)?
        .iter()
        .any(|pending| DrivenInput::of(&pending.prompt).is_some()))
}

fn continue_prompt_driver(
    inbox: &SessionInbox,
    services: &ServerServices,
    session_id: &str,
    guard: &mut Option<SessionRunGuard>,
) -> bool {
    match has_driven_pending(inbox, session_id) {
        Ok(false) => false,
        Ok(true) if guard.is_some() => true,
        Ok(true) => match services.runs.begin_turn(session_id) {
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

/// Build the request one promoted unit of work runs as.
fn driven_request(
    state: &ApiState,
    session_id: &str,
    promoted: DrivenPromotion,
) -> Result<DrivenRequest, String> {
    match promoted {
        DrivenPromotion::Prompt(input) => prompt_execution(state, input).map(DrivenRequest::Prompt),
        DrivenPromotion::HumanAnswer(input) => {
            human_answer_execution(state, input).map(DrivenRequest::Prompt)
        }
        DrivenPromotion::Reports(batch) => {
            report_execution(state, session_id, batch).map(DrivenRequest::Reports)
        }
    }
}

fn prompt_execution(
    state: &ApiState,
    input: SessionInput,
) -> Result<SessionPromptExecution, String> {
    let session = state
        .sessions()
        .get(&input.session_id)
        .map_err(|error| error.to_string())?;
    let stored = serde_json::from_value::<PersistedUserPrompt>(input.prompt)
        .map_err(|error| format!("invalid persisted session input `{}`: {error}", input.id))?;
    let content = prompt_request_content(&stored.prompt)?;
    Ok(SessionPromptExecution {
        session_id: input.session_id,
        directory: session.directory.into(),
        message_id: input.id,
        prompt: stored.prompt.text,
        content,
        agent: stored.agent,
        model: stored.model.map(SessionModelSelection::from),
    })
}

fn human_answer_execution(
    state: &ApiState,
    input: SessionInput,
) -> Result<SessionPromptExecution, String> {
    let session = state
        .sessions()
        .get(&input.session_id)
        .map_err(|error| error.to_string())?;
    let text = DurableInputKind::classify(&input.prompt)
        .and_then(|kind| kind.plain_text(&input.prompt))
        .ok_or_else(|| {
            format!(
                "persisted session input `{}` carries no model-visible text",
                input.id
            )
        })?
        .to_owned();
    let model = session_model(session.model.as_deref());
    Ok(SessionPromptExecution {
        session_id: input.session_id,
        directory: session.directory.into(),
        message_id: input.id,
        prompt: text,
        content: Vec::new(),
        agent: session.agent,
        model,
    })
}

/// Build the one request a whole batch of settled reports runs as.
///
/// The session's own agent and model own a report batch: no report carries an agent or
/// model override, so a batch cannot be split by conflicting selections.
fn report_execution(
    state: &ApiState,
    session_id: &str,
    batch: ReportBatch,
) -> Result<SessionReportExecution, String> {
    let session = state
        .sessions()
        .get(session_id)
        .map_err(|error| error.to_string())?;
    let model = session_model(session.model.as_deref());
    Ok(SessionReportExecution {
        session_id: session_id.to_owned(),
        directory: session.directory.into(),
        agent: session.agent,
        model,
        reports: batch.reports().to_vec(),
    })
}

fn admit_prompt_files(
    state: &ApiState,
    prompt: &mut PromptInputBody,
) -> Result<Vec<zuno_attachment::ImageAttachmentRef>, ApiError> {
    let mut admitted = Vec::with_capacity(prompt.files.len());
    let mut durable = Vec::with_capacity(prompt.files.len());
    for (index, file) in prompt.files.iter().enumerate() {
        let object = file.as_object().ok_or_else(|| {
            ApiError::InvalidPrompt(format!("prompt.files[{index}] must be an object"))
        })?;
        let reference = if let Some(value) = object.get("attachment") {
            let reference =
                serde_json::from_value::<zuno_attachment::ImageAttachmentRef>(value.clone())
                    .map_err(|_| {
                        ApiError::InvalidPrompt(format!(
                            "prompt.files[{index}].attachment is invalid"
                        ))
                    })?;
            state.attachments().read(&reference).map_err(|error| {
                ApiError::InvalidPrompt(format!(
                    "prompt.files[{index}] references an invalid image object: {error}"
                ))
            })?;
            reference
        } else {
            let media_type = object
                .get("mimeType")
                .or_else(|| object.get("mime"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ApiError::InvalidPrompt(format!(
                        "prompt.files[{index}] must contain a non-empty image MIME type"
                    ))
                })?;
            if !media_type.starts_with("image/") {
                return Err(ApiError::InvalidPrompt(format!(
                    "prompt.files[{index}] uses unsupported MIME type {media_type}; only images are accepted"
                )));
            }
            let data = object
                .get("data")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ApiError::InvalidPrompt(format!(
                        "prompt.files[{index}] must contain non-empty base64 data"
                    ))
                })?;
            let filename = object
                .get("filename")
                .and_then(Value::as_str)
                .map(str::to_owned);
            state
                .attachments()
                .admit_base64_typed(data, Some(media_type), filename)
                .map_err(|error| {
                    ApiError::InvalidPrompt(format!(
                        "prompt.files[{index}] image admission failed: {error}"
                    ))
                })?
        };
        durable.push(json!({
            "type": "image",
            "attachment": reference,
        }));
        admitted.push(reference);
    }
    prompt.files = durable;
    Ok(admitted)
}

fn prompt_request_content(prompt: &PromptInputBody) -> Result<Vec<RequestContentBlock>, String> {
    if prompt.files.is_empty() {
        return Ok(Vec::new());
    }
    let mut content = Vec::with_capacity(1 + prompt.files.len());
    if !prompt.text.is_empty() {
        content.push(RequestContentBlock::Text {
            text: prompt.text.clone(),
        });
    }
    for (index, file) in prompt.files.iter().enumerate() {
        let reference = file
            .get("attachment")
            .cloned()
            .ok_or_else(|| format!("persisted prompt file {index} has no attachment reference"))
            .and_then(|value| {
                serde_json::from_value::<zuno_attachment::ImageAttachmentRef>(value).map_err(|_| {
                    format!("persisted prompt file {index} has an invalid attachment reference")
                })
            })?;
        content.push(RequestContentBlock::ImageAttachment { reference });
    }
    Ok(content)
}

async fn run_driven_execution(
    state: &ApiState,
    services: &ServerServices,
    executor: Arc<dyn crate::SessionMutationExecutor>,
    request: DrivenRequest,
    guard: SessionRunGuard,
) -> Result<(), String> {
    let fanout = services.events.clone();
    let durable_events = state.events().cloned();
    let event_session_id = request.session_id().to_owned();
    let (sender, receiver) = event_channel();
    let execution = match request {
        DrivenRequest::Prompt(request) => executor.prompt(request, guard, sender),
        DrivenRequest::Reports(request) => executor.reports(request, guard, sender),
    };
    if let Some(events) = durable_events.as_ref() {
        let (outcome, ()) = tokio::join!(
            execution,
            events.forward_engine_events(&event_session_id, &fanout, receiver)
        );
        outcome
    } else {
        let (outcome, ()) = tokio::join!(execution, fanout.forward_engine_events(receiver));
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
            None => session_model(session.model.as_deref()),
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
    // The lease is released either way. Input admitted while the compaction held it
    // has been waiting in the durable inbox; drive it now instead of leaving it for
    // whichever external event happens to wake the session next.
    resume_pending_inputs(&state, &services, Arc::clone(executor), &session_id).await;
    outcome.map_err(ApiError::MutationFailed)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Start a prompt driver for input admitted while another lease holder (a
/// compaction) owned the session.
///
/// Losing `begin_turn` to a concurrent prompt is fine: that prompt's driver drains
/// the same FIFO. The idle wait covers an executor that releases its guard a moment
/// after returning.
async fn resume_pending_inputs(
    state: &ApiState,
    services: &ServerServices,
    executor: Arc<dyn crate::SessionMutationExecutor>,
    session_id: &str,
) {
    match has_driven_pending(&SessionInbox::new(state.pool_arc()), session_id) {
        Ok(false) => return,
        Ok(true) => {}
        Err(error) => {
            eprintln!("session input inspection failed for `{session_id}`: {error}");
            return;
        }
    }
    services.runs.wait_until_idle(session_id).await;
    if let Ok(guard) = services.runs.begin_turn(session_id) {
        spawn_prompt_driver(
            state.clone(),
            services.clone(),
            executor,
            session_id.to_owned(),
            guard,
        );
    }
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
    // Only a live turn can be interrupted. With nothing running there is nothing to
    // cancel, and arming the registry instead would leave a marker with no expiry
    // that cancels whichever ordinary turn starts next — including an explicit
    // `resume` of input that was deliberately queued.
    let _interrupted_live_turn = services.runs.abort_active(
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

/// The model a request without one runs on: the session's saved model, decoded by the
/// same tolerant reader the CLI surfaces use.
///
/// A row an older writer stored as a plain `provider/model` string, or one this server
/// cannot read at all, yields `None` and lets turn resolution route from configuration
/// rather than failing a request over a column the caller did not send.
fn session_model(raw: Option<&str>) -> Option<SessionModelSelection> {
    let model = zuno_db::session::decode_model_reference(raw?)?;
    Some(SessionModelSelection {
        provider_id: model.provider_id,
        model_id: model.model_id,
    })
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
