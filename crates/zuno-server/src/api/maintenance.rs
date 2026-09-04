use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use schemars::JsonSchema;
use serde::Deserialize;
use zuno_db::prune::{RemoteUnshare, SharedSession, UnshareError};
use zuno_db::retention::{Liveness, LivenessProbe, RetentionKey};
use zuno_db::session_list::resolve_project;
use zuno_db::session_prune::{
    SessionPruneAction, SessionPruneProgress, SessionPruneRequest, SessionPruneScope,
};

use super::blocking::Budget;
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
        state,
        input.older_than,
        input.all_projects,
        input.project,
        input.by,
        SessionPruneAction::Preview,
        input.include_shared,
        input.include_recent,
        false,
        false,
        now_ms,
        services,
    )
    .await
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
        state,
        input.older_than,
        input.all_projects,
        input.project,
        input.by,
        action,
        input.include_shared,
        input.include_recent,
        input.force,
        true,
        now_ms,
        services,
    )
    .await
}

/// Runs the prune off the reactor, inside the maintenance budget, and renders its
/// report.
///
/// The scan, the artifact unlinks, and the database writes are all synchronous and
/// unbounded in the size of the database, and `zuno serve` polls this router on a
/// single-threaded runtime. Left in the handler future they freeze every SSE stream
/// in the process — including the progress stream this endpoint exists to feed,
/// whose subscribers cannot be polled while the same thread is inside the scan.
///
/// A bare `spawn_blocking` is not the answer either:
/// `GET /api/session/prune?olderThan=0&allProjects=true` is a full retention scan over
/// every project, the route is unauthenticated unless the operator sets
/// `ZUNO_SERVER_PASSWORD`, and N concurrent requests would occupy up to the blocking
/// pool's 512 threads — the same pool the durable event commits, permission settles,
/// and goal resumes the agent loop depends on run in. It is charged to
/// [`Budget::Maintenance`] for that reason, which queues the surplus rather than
/// refusing it: an operator dashboard issuing a handful of concurrent previews is
/// ordinary input, while a queued request that its client abandons never starts the
/// scan at all.
#[allow(
    clippy::too_many_arguments,
    reason = "preview and mutate share this policy path, whose inputs mirror the request plus execution context"
)]
async fn run(
    state: ApiState,
    older_than_days: u64,
    all_projects: bool,
    project: Option<String>,
    by: RetentionBy,
    action: SessionPruneAction,
    include_shared: bool,
    include_recent: bool,
    force: bool,
    confirm_delete: bool,
    now_ms: i64,
    services: ServerServices,
) -> Result<Response, ApiError> {
    super::blocking::run(Budget::Maintenance, move || {
        execute(
            &state,
            older_than_days,
            all_projects,
            project.as_deref(),
            by,
            action,
            include_shared,
            include_recent,
            force,
            confirm_delete,
            now_ms,
            &services,
        )
    })
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "preview and mutate share this policy path, whose inputs mirror the request plus execution context"
)]
fn execute(
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
    let report = zuno_db::session_prune::execute(
        &mut connection,
        state.artifact_paths(),
        &request,
        &liveness,
        &UnavailableRemote,
        &mut move |progress: SessionPruneProgress| events.publish(progress),
    )?;
    // A session's standing permission grants end with the session. This is the one
    // place the server observes a session ending, so archiving or deleting a session
    // withdraws every `always` it granted; a preview changes nothing and withdraws
    // nothing.
    if matches!(
        report.action,
        SessionPruneAction::Archive { .. } | SessionPruneAction::Delete
    ) {
        services
            .requests
            .forget_session_grants(&report.selected_session_ids);
    }
    let bytes = zuno_db::session_prune::to_json_bytes(&report)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .map_err(|_| ApiError::InvalidRequest("failed to build maintenance response"))
}

fn scope(
    connection: &zuno_db::Connection,
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
        zuno_paths::project::resolve_project(std::path::Path::new(directory)).id,
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The reviewed input, spelled exactly as the reviewer sent it.
    ///
    /// `GET /api/session/prune?olderThan=0&allProjects=true` is a full retention scan
    /// over every project on a route that is unauthenticated unless the operator sets
    /// `ZUNO_SERVER_PASSWORD`. Wrapped in a bare `spawn_blocking`, N concurrent copies
    /// occupy up to the blocking pool's 512 threads, which is where the durable event
    /// commits and the permission settles the agent loop depends on also run.
    ///
    /// The oracle is the handler waiting: with the maintenance budget fully held, this
    /// preview cannot start. It completes as soon as a permit frees, so the bound
    /// queues the scan rather than refusing it.
    ///
    /// The wait is a real-clock window rather than a poll count. A poll count passes
    /// vacuously — a scan that *did* start off the budget also needs more than 64
    /// yields to finish — while a handler that is waiting for a permit that nothing
    /// releases stays pending for any window at all. Measured against the bare
    /// `spawn_blocking` this replaces, the same scan completed well inside this window.
    #[tokio::test]
    async fn a_full_retention_scan_waits_for_the_maintenance_budget() {
        let state = ApiState::memory("/repo").expect("in-memory API state initializes");
        let services = ServerServices::new(64);
        let uri: axum::http::Uri =
            "http://127.0.0.1/api/session/prune?olderThan=0&allProjects=true"
                .parse()
                .expect("the reviewed request URI parses");
        let Query(input) =
            Query::<PreviewQuery>::try_from_uri(&uri).expect("the reviewed query parses");

        let held = Budget::Maintenance.hold_all().await;
        let mut previewing =
            std::pin::pin!(preview(State(state), Extension(services), Query(input)));
        let window = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < window {
            assert!(
                futures::poll!(&mut previewing).is_pending(),
                "a full retention scan started outside the maintenance budget, so \
                 concurrent previews can occupy the blocking pool that the durable \
                 event commits and permission settles share"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        drop(held);
        let response = previewing
            .await
            .expect("the preview runs once the budget frees");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the budget must queue the scan, not refuse it"
        );
    }
}
