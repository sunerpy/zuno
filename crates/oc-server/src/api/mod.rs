mod error;
mod openapi;
mod pty;
mod session;
mod state;

use axum::Json;
use axum::Router;
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
        .route("/api/session/{sessionID}", get(session::get))
        .route("/api/pty", get(pty::list).post(pty::create))
        .route(
            "/api/pty/{ptyID}",
            get(pty::get).put(pty::update).delete(pty::remove),
        )
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

async fn unsupported() -> error::ApiError {
    error::ApiError::NotImplemented("operation is registered but its backend is not available")
}

fn unsupported_routes() -> Router<ApiState> {
    Router::new()
        .route("/api/agent", get(unsupported))
        .route("/api/model", get(unsupported))
        .route("/api/command", get(unsupported))
        .route("/api/skill", get(unsupported))
        .route("/api/reference", get(unsupported))
        .route("/api/provider", get(unsupported))
        .route("/api/provider/{providerID}", get(unsupported))
        .route("/api/integration", get(unsupported))
        .route("/api/integration/{integrationID}", get(unsupported))
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
        .route("/api/fs/read/{*path}", get(unsupported))
        .route("/api/fs/list", get(unsupported))
        .route("/api/fs/find", get(unsupported))
        .route("/api/pty/{ptyID}/connect-token", post(unsupported))
        .route("/api/pty/{ptyID}/connect", get(unsupported))
        .route("/api/permission/request", get(unsupported))
        .route("/api/permission/saved", get(unsupported))
        .route("/api/permission/saved/{id}", delete(unsupported))
        .route(
            "/api/session/{sessionID}/permission",
            get(unsupported).post(unsupported),
        )
        .route(
            "/api/session/{sessionID}/permission/{requestID}",
            get(unsupported),
        )
        .route(
            "/api/session/{sessionID}/permission/{requestID}/reply",
            post(unsupported),
        )
        .route("/api/question/request", get(unsupported))
        .route("/api/session/{sessionID}/question", get(unsupported))
        .route(
            "/api/session/{sessionID}/question/{requestID}/reply",
            post(unsupported),
        )
        .route(
            "/api/session/{sessionID}/question/{requestID}/reject",
            post(unsupported),
        )
        .route("/api/session/active", get(unsupported))
        .route("/api/session/{sessionID}/agent", post(unsupported))
        .route("/api/session/{sessionID}/model", post(unsupported))
        .route("/api/session/{sessionID}/prompt", post(unsupported))
        .route("/api/session/{sessionID}/compact", post(unsupported))
        .route("/api/session/{sessionID}/wait", post(unsupported))
        .route("/api/session/{sessionID}/revert/stage", post(unsupported))
        .route("/api/session/{sessionID}/revert/clear", post(unsupported))
        .route("/api/session/{sessionID}/revert/commit", post(unsupported))
        .route("/api/session/{sessionID}/context", get(unsupported))
        .route("/api/session/{sessionID}/history", get(unsupported))
        .route("/api/session/{sessionID}/interrupt", post(unsupported))
        .route(
            "/api/session/{sessionID}/message/{messageID}",
            get(unsupported),
        )
        .route("/api/session/{sessionID}/message", get(unsupported))
}
