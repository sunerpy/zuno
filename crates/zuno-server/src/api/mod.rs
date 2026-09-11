mod blocking;
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

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::Router;
use axum::routing::{get, post};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use zuno_tool::question::QuestionPort;

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
    router_with_question_port(state, None)
}

/// Register durable question operations only with a real provider.
///
/// Production injects the same `QuestionPort` used by tools and other clients.
/// The provider owns persistence, consent, continuation, and committed events.
pub fn router_with_questions(state: ApiState, questions: Arc<dyn QuestionPort>) -> Router {
    router_with_question_port(state, Some(questions))
}

fn router_with_question_port(state: ApiState, questions: Option<Arc<dyn QuestionPort>>) -> Router {
    let has_questions = questions.is_some();
    let has_controls = state.session_controls().is_some();
    let document =
        get(move || async move { Json(openapi::document_for(has_questions, has_controls)) });
    let mut router = Router::new()
        .route("/doc", document.clone())
        .route("/openapi.json", document.clone())
        .route("/api/doc", document)
        .route("/api/health", get(health))
        .route("/api/location", get(location))
        .route("/api/session", get(session::list).post(session::create))
        .route("/api/session/active", get(session::active))
        .route(
            "/api/session/prune",
            get(maintenance::preview).post(maintenance::mutate),
        )
        .route("/api/session/{sessionID}", get(session::get))
        .route("/api/session/{sessionID}/learning", get(session::learning))
        .route(
            "/api/session/{sessionID}/memory-policy",
            get(session::memory_policy).put(session::update_memory_policy),
        )
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
            "/api/session/{sessionID}/permission",
            get(request::session_permission_requests),
        )
        .route(
            "/api/session/{sessionID}/permission/{requestID}/reply",
            post(request::permission_reply),
        )
        .route("/api/permission/request", get(request::permission_requests))
        .route("/api/pty", get(pty::list).post(pty::create))
        .route(
            "/api/pty/{ptyID}",
            get(pty::get).put(pty::update).delete(pty::remove),
        )
        .route("/api/pty/{ptyID}/connect-token", post(pty::connect_token))
        .route("/api/pty/{ptyID}/connect", get(pty::connect));
    if has_controls {
        router = router.route("/api/session/{sessionID}/resume", post(session::resume));
    }
    if let Some(questions) = questions {
        router = router.merge(
            Router::new()
                .route("/api/question/request", get(request::question_requests))
                .route(
                    "/api/session/{sessionID}/question",
                    get(request::session_questions),
                )
                .route(
                    "/api/session/{sessionID}/question/{requestID}/reply",
                    post(request::question_reply),
                )
                .route(
                    "/api/session/{sessionID}/question/{requestID}/reject",
                    post(request::question_reject),
                )
                .route(
                    "/api/session/{sessionID}/question/{requestID}/defer",
                    post(request::question_defer),
                )
                .layer(Extension(questions)),
        );
    }
    router.with_state(state)
}

#[must_use]
pub fn openapi() -> Value {
    openapi::document()
}

/// OpenAPI for a router assembled with [`router_with_questions`].
#[must_use]
pub fn openapi_with_questions() -> Value {
    openapi::document_with_questions()
}

#[must_use]
pub const fn openapi_body_schema_gaps() -> &'static [(&'static str, &'static str, &'static str)] {
    openapi::body_schema_gaps()
}

async fn health() -> Json<Value> {
    Json(json!({"healthy": true}))
}

async fn location(
    axum::extract::State(state): axum::extract::State<ApiState>,
) -> Json<LocationInfo> {
    Json(LocationInfo {
        directory: state.directory().to_owned(),
        project_id: zuno_paths::GLOBAL_PROJECT_ID,
    })
}
