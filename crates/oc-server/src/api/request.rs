use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Extension, Path, Request, State};
use axum::http::StatusCode;
use oc_permission::ReplyKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Data;
use super::error::ApiError;
use super::state::ApiState;
use crate::{QuestionDecision, ServerServices};

const MAX_REPLY_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocationResponse<T> {
    location: Location,
    data: Vec<T>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    directory: String,
    project: Project,
}

#[derive(Debug, Serialize)]
struct Project {
    id: &'static str,
    directory: String,
}

fn location_response<T>(state: &ApiState, data: Vec<T>) -> LocationResponse<T> {
    LocationResponse {
        location: Location {
            directory: state.directory().to_owned(),
            project: Project {
                id: oc_paths::GLOBAL_PROJECT_ID,
                directory: state.directory().to_owned(),
            },
        },
        data,
    }
}

pub async fn permission_requests(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
) -> Json<impl Serialize> {
    Json(location_response(
        &state,
        services.requests.permissions(None),
    ))
}

pub async fn question_requests(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
) -> Json<impl Serialize> {
    Json(location_response(&state, services.requests.questions(None)))
}

pub async fn saved_permissions() -> Json<Data<Vec<Value>>> {
    Json(Data::new(Vec::new()))
}

pub async fn remove_saved_permission(Path(_id): Path<String>) -> StatusCode {
    StatusCode::NO_CONTENT
}

pub async fn session_questions(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
) -> Result<Json<Data<Vec<crate::QuestionRequest>>>, ApiError> {
    state.sessions().get(&session_id)?;
    Ok(Json(Data::new(
        services.requests.questions(Some(&session_id)),
    )))
}

pub async fn session_permission_requests(
    State(state): State<ApiState>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
) -> Result<Json<Data<Vec<crate::PermissionRequest>>>, ApiError> {
    state.sessions().get(&session_id)?;
    Ok(Json(Data::new(
        services.requests.permissions(Some(&session_id)),
    )))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionReplyBody {
    reply: ReplyKind,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionReplyBody {
    answers: Vec<Vec<String>>,
}

pub async fn permission_reply(
    Path((session_id, request_id)): Path<(String, String)>,
    Extension(services): Extension<ServerServices>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    validate_request_id(&request_id, "per")?;
    let body: PermissionReplyBody = match parse_reply(request).await {
        Ok(body) => body,
        Err(error) => {
            drop(services.requests.claim_permission(&session_id, &request_id));
            return Err(error);
        }
    };
    let resolution = services
        .requests
        .claim_permission(&session_id, &request_id)
        .ok_or_else(|| request_not_found("permission", &session_id, &request_id))?;
    let _message = body.message;
    services
        .requests
        .publish_permission_reply(&session_id, &request_id, body.reply)
        .await?;
    if !resolution.resolve(body.reply) {
        return Err(request_not_found("permission", &session_id, &request_id));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn question_reply(
    Path((session_id, request_id)): Path<(String, String)>,
    Extension(services): Extension<ServerServices>,
    request: Request,
) -> Result<StatusCode, ApiError> {
    validate_request_id(&request_id, "que")?;
    let body: QuestionReplyBody = match parse_reply(request).await {
        Ok(body) => body,
        Err(error) => {
            drop(services.requests.claim_question(&session_id, &request_id));
            return Err(error);
        }
    };
    let resolution = services
        .requests
        .claim_question(&session_id, &request_id)
        .ok_or_else(|| request_not_found("question", &session_id, &request_id))?;
    let decision = QuestionDecision::Answered(body.answers);
    services
        .requests
        .publish_question_reply(&session_id, &request_id, &decision)
        .await?;
    if !resolution.resolve(decision) {
        return Err(request_not_found("question", &session_id, &request_id));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn question_reject(
    Path((session_id, request_id)): Path<(String, String)>,
    Extension(services): Extension<ServerServices>,
) -> Result<StatusCode, ApiError> {
    validate_request_id(&request_id, "que")?;
    let resolution = services
        .requests
        .claim_question(&session_id, &request_id)
        .ok_or_else(|| request_not_found("question", &session_id, &request_id))?;
    let decision = QuestionDecision::Rejected;
    services
        .requests
        .publish_question_reply(&session_id, &request_id, &decision)
        .await?;
    if !resolution.resolve(decision) {
        return Err(request_not_found("question", &session_id, &request_id));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn parse_reply<T: for<'de> Deserialize<'de>>(request: Request) -> Result<T, ApiError> {
    let bytes = to_bytes(request.into_body(), MAX_REPLY_BODY_BYTES)
        .await
        .map_err(|_| ApiError::InvalidRequest("reply body is incomplete or too large"))?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::InvalidRequest("reply body is invalid"))
}

fn validate_request_id(request_id: &str, prefix: &str) -> Result<(), ApiError> {
    if request_id.starts_with(prefix) {
        Ok(())
    } else {
        Err(ApiError::InvalidRequest("request ID is invalid"))
    }
}

fn request_not_found(kind: &'static str, session_id: &str, request_id: &str) -> ApiError {
    ApiError::RequestNotFound {
        kind,
        id: request_id.to_owned(),
        session_id: session_id.to_owned(),
    }
}
