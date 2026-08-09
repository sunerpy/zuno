use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use oc_db::prune::{RemoteUnshare, SharedSession, UnshareError};
use oc_db::retention::{Liveness, LivenessProbe, RetentionKey};
use oc_db::session_list::resolve_project;
use oc_db::session_prune::{
    SessionPruneAction, SessionPruneProgress, SessionPruneRequest, SessionPruneScope,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::error::ApiError;
use super::state::ApiState;
use crate::ServerServices;

struct ReachableServer(std::collections::BTreeSet<String>);

impl LivenessProbe for ReachableServer {
    fn probe(&self) -> Liveness {
        Liveness::Reachable {
            active_session_ids: self.0.clone(),
        }
    }
}

struct UnavailableRemote;

impl RemoteUnshare for UnavailableRemote {
    fn unshare(&self, _session: &SharedSession) -> Result<(), UnshareError> {
        Err(UnshareError::new(
            "remote unshare is unavailable from the maintenance API",
        ))
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum RetentionBy {
    #[default]
    Updated,
    Created,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewQuery {
    older_than: u64,
    #[serde(default)]
    all_projects: bool,
    project: Option<String>,
    #[serde(default)]
    by: RetentionBy,
    #[serde(default)]
    include_shared: bool,
    #[serde(default)]
    include_recent: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MutationAction {
    Archive,
    Delete,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MutationBody {
    older_than: u64,
    #[serde(default)]
    all_projects: bool,
    project: Option<String>,
    #[serde(default)]
    by: RetentionBy,
    action: MutationAction,
    apply: Option<bool>,
    #[serde(default)]
    include_shared: bool,
    #[serde(default)]
    include_recent: bool,
    #[serde(default)]
    force: bool,
}

pub async fn preview(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Query(input): Query<PreviewQuery>,
) -> Result<Response, ApiError> {
    let now_ms = unix_millis()?;
    run(
        &state,
        input.older_than,
        input.all_projects,
        input.project.as_deref(),
        input.by,
        SessionPruneAction::Preview,
        input.include_shared,
        input.include_recent,
        false,
        false,
        now_ms,
        &services,
    )
}

pub async fn mutate(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Json(input): Json<MutationBody>,
) -> Result<Response, ApiError> {
    if input.apply != Some(true) {
        return Err(ApiError::InvalidRequest(
            "session prune mutation requires `apply: true`; nothing was changed",
        ));
    }
    let now_ms = unix_millis()?;
    let action = match input.action {
        MutationAction::Archive => SessionPruneAction::Archive { at_ms: now_ms },
        MutationAction::Delete => SessionPruneAction::Delete,
    };
    run(
        &state,
        input.older_than,
        input.all_projects,
        input.project.as_deref(),
        input.by,
        action,
        input.include_shared,
        input.include_recent,
        input.force,
        true,
        now_ms,
        &services,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "preview and mutate share this policy path, whose inputs mirror the request plus execution context"
)]
fn run(
    state: &ApiState,
    older_than_days: u64,
    all_projects: bool,
    project: Option<&str>,
    by: RetentionBy,
    action: SessionPruneAction,
    include_shared: bool,
    include_recent: bool,
    force: bool,
    confirm_delete: bool,
    now_ms: i64,
    services: &ServerServices,
) -> Result<Response, ApiError> {
    let mut connection = state.pool().get()?;
    let scope = scope(&connection, state.directory(), all_projects, project)?;
    let request = SessionPruneRequest {
        older_than_days,
        scope,
        key: match by {
            RetentionBy::Updated => RetentionKey::Updated,
            RetentionBy::Created => RetentionKey::Created,
        },
        action,
        include_shared,
        include_recent,
        force,
        confirm_delete,
        now_ms,
    };
    let events = services.maintenance_events.clone();
    let liveness = ReachableServer(services.runs.active_sessions());
    let report = oc_db::session_prune::execute(
        &mut connection,
        state.artifact_paths(),
        &request,
        &liveness,
        &UnavailableRemote,
        &mut move |progress: SessionPruneProgress| events.publish(progress),
    )?;
    let bytes = oc_db::session_prune::to_json_bytes(&report)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .map_err(|_| ApiError::InvalidRequest("failed to build maintenance response"))
}

fn scope(
    connection: &oc_db::Connection,
    directory: &str,
    all_projects: bool,
    project: Option<&str>,
) -> Result<SessionPruneScope, ApiError> {
    if all_projects && project.is_some() {
        return Err(ApiError::InvalidRequest(
            "allProjects and project are mutually exclusive",
        ));
    }
    if all_projects {
        return Ok(SessionPruneScope::AllProjects);
    }
    if let Some(project) = project {
        let Some(project) = resolve_project(connection, project)? else {
            return Err(ApiError::InvalidRequest(
                "project must name an existing project id or worktree",
            ));
        };
        return Ok(SessionPruneScope::Project(project.id));
    }
    Ok(SessionPruneScope::CurrentProject(
        oc_paths::project::resolve_project(std::path::Path::new(directory)).id,
    ))
}

fn unix_millis() -> Result<i64, ApiError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApiError::InvalidRequest("system clock is before the Unix epoch"))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| ApiError::InvalidRequest("system clock is outside the supported range"))
}
