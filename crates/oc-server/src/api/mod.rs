pub(crate) mod catalog;
pub(crate) mod error;
mod fs;
mod maintenance;
mod openapi;
pub(crate) mod provider;
mod pty;
mod request;
pub(crate) mod session;
mod state;

use axum::Json;
use axum::Router;
use axum::extract::MatchedPath;
use axum::http::Method;
use axum::routing::{delete, get, patch, post};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

pub use state::ApiState;

#[derive(Debug, Serialize, JsonSchema)]
pub struct Data<T> {
    pub data: T,
}

impl<T> Data<T> {
    const fn new(data: T) -> Self {
        Self { data }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocationInfo {
    directory: String,
    project_id: &'static str,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/doc", get(document))
        .route("/openapi.json", get(document))
        .route("/api/doc", get(document))
        .route("/api/health", get(health))
        .route("/api/location", get(location))
        .route("/api/session", get(session::list).post(session::create))
        .route("/api/session/active", get(session::active))
        .route(
            "/api/session/prune",
            get(maintenance::preview).post(maintenance::mutate),
        )
        .route("/api/session/{sessionID}", get(session::get))
        .route("/api/agent", get(catalog::agents))
        .route("/api/command", get(catalog::commands))
        .route("/api/skill", get(catalog::skills))
        .route("/api/reference", get(catalog::references))
        .route("/api/model", get(provider::models))
        .route("/api/provider", get(provider::providers))
        .route("/api/provider/{providerID}", get(provider::provider))
        .route("/api/integration", get(provider::integrations))
        .route(
            "/api/integration/{integrationID}",
            get(provider::integration),
        )
        .route("/api/fs/read/{*path}", get(fs::read))
        .route("/api/fs/list", get(fs::list))
        .route("/api/fs/find", get(fs::find))
        .route("/api/session/{sessionID}/context", get(session::context))
        .route("/api/session/{sessionID}/history", get(session::history))
        .route("/api/session/{sessionID}/message", get(session::messages))
        .route(
            "/api/session/{sessionID}/agent",
            post(session::switch_agent),
        )
        .route(
            "/api/session/{sessionID}/model",
            post(session::switch_model),
        )
        .route("/api/session/{sessionID}/prompt", post(session::prompt))
        .route("/api/session/{sessionID}/compact", post(session::compact))
        .route("/api/session/{sessionID}/wait", post(session::wait))
        .route(
            "/api/session/{sessionID}/revert/stage",
            post(session::revert_stage),
        )
        .route(
            "/api/session/{sessionID}/revert/clear",
            post(session::revert_clear),
        )
        .route(
            "/api/session/{sessionID}/revert/commit",
            post(session::revert_commit),
        )
        .route(
            "/api/session/{sessionID}/interrupt",
            post(session::interrupt),
        )
        .route(
            "/api/session/{sessionID}/question",
            get(request::session_questions),
        )
        .route(
            "/api/session/{sessionID}/permission",
            get(request::session_permission_requests),
        )
        .route(
            "/api/session/{sessionID}/permission/{requestID}/reply",
            post(request::permission_reply),
        )
        .route(
            "/api/session/{sessionID}/question/{requestID}/reply",
            post(request::question_reply),
        )
        .route(
            "/api/session/{sessionID}/question/{requestID}/reject",
            post(request::question_reject),
        )
        .route("/api/permission/request", get(request::permission_requests))
        .route("/api/permission/saved", get(request::saved_permissions))
        .route(
            "/api/permission/saved/{id}",
            delete(request::remove_saved_permission),
        )
        .route("/api/question/request", get(request::question_requests))
        .route("/api/pty", get(pty::list).post(pty::create))
        .route(
            "/api/pty/{ptyID}",
            get(pty::get).put(pty::update).delete(pty::remove),
        )
        .route("/api/pty/{ptyID}/connect-token", post(pty::connect_token))
        .route("/api/pty/{ptyID}/connect", get(pty::connect))
        .merge(unsupported_routes())
        .with_state(state)
}

#[must_use]
pub fn openapi() -> Value {
    openapi::document()
}

async fn document() -> Json<Value> {
    Json(openapi())
}

async fn health() -> Json<Value> {
    Json(json!({"healthy": true}))
}

async fn location(
    axum::extract::State(state): axum::extract::State<ApiState>,
) -> Json<LocationInfo> {
    Json(LocationInfo {
        directory: state.directory().to_owned(),
        project_id: oc_paths::GLOBAL_PROJECT_ID,
    })
}

async fn unsupported(method: Method, path: MatchedPath) -> error::ApiError {
    error::ApiError::BackendUnavailable(format!("{} {}", method.as_str(), path.as_str()))
}

fn unsupported_routes() -> Router<ApiState> {
    Router::new()
        .route(
            "/api/integration/{integrationID}/connect/key",
            post(unsupported),
        )
        .route(
            "/api/integration/{integrationID}/connect/oauth",
            post(unsupported),
        )
        .route(
            "/api/integration/attempt/{attemptID}",
            get(unsupported).delete(unsupported),
        )
        .route(
            "/api/integration/attempt/{attemptID}/complete",
            post(unsupported),
        )
        .route(
            "/api/credential/{credentialID}",
            patch(unsupported).delete(unsupported),
        )
        .route("/api/session/{sessionID}/permission", post(unsupported))
        .route(
            "/api/session/{sessionID}/permission/{requestID}",
            get(unsupported),
        )
        .route(
            "/api/session/{sessionID}/message/{messageID}",
            get(unsupported),
        )
}
