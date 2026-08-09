use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use oc_db::session::{ListQuery, Session, SessionCreate, SortDirection};
use oc_paths::GLOBAL_PROJECT_ID;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::Data;
use super::error::ApiError;
use super::state::ApiState;
use crate::ServerServices;

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
