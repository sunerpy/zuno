use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Extension, Path, Request, State};
use axum::http::StatusCode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_permission::ReplyKind;

use super::Data;
use super::error::ApiError;
use super::state::ApiState;
use crate::{QuestionDecision, ServerServices};
use crate::{SettleError, Settled};

const MAX_REPLY_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocationResponse<T> {
    location: Location,
    data: Vec<T>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Location {
    directory: String,
    project: Project,
}

#[derive(Debug, Serialize, JsonSchema)]
struct Project {
    id: &'static str,
    directory: String,
}

fn location_response<T>(state: &ApiState, data: Vec<T>) -> LocationResponse<T> {
    LocationResponse {
        location: Location {
            directory: state.directory().to_owned(),
            project: Project {
                id: zuno_paths::GLOBAL_PROJECT_ID,
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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PermissionReplyBody {
    #[schemars(with = "String")]
    reply: ReplyKind,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct QuestionReplyBody {
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
    // `settle` owns the `permission.v2.replied` event: it commits inside the same
    // transaction as the row it describes, so a reply that does not land never
    // announces itself. Publishing here first meant two concurrent replies to one
    // recovered request both published while only one wrote.
    settled(
        "permission",
        &session_id,
        &request_id,
        resolution.settle(body.reply).await,
    )
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
    // See `permission_reply`: the event is committed with the row it describes.
    settled(
        "question",
        &session_id,
        &request_id,
        resolution.settle(decision).await,
    )
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
    let decision = QuestionDecision::Cancelled;
    // See `permission_reply`: the event is committed with the row it describes.
    settled(
        "question",
        &session_id,
        &request_id,
        resolution.settle(decision).await,
    )
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

/// Turns the outcome of one settle into this route's status.
///
/// A request somebody else already answered is `404`, exactly as an unknown id is: the
/// client's reply had no effect either way, because [`SettleError::Gone`] is only
/// returned before anything is written or published. A durable failure is `500`, because
/// the request is still pending and the reply is worth retrying — reporting it as `404`
/// would tell the client to stop.
///
/// `204` therefore means the audit row, the event, and — for a request recovered after a
/// restart — the inbox input all committed. It does not promise that the tool call which
/// asked was still there to receive it: an asker that timed out or was interrupted
/// leaves [`Settled::delivered`] false, and the reply then authorizes nothing, including
/// no standing `always`. That is a fact about the call, not a failed write, so it is
/// logged rather than turned into a status the client would retry into a `404`.
fn settled(
    kind: &'static str,
    session_id: &str,
    request_id: &str,
    outcome: Result<Settled, SettleError>,
) -> Result<StatusCode, ApiError> {
    match outcome {
        Ok(settled) => {
            if !settled.delivered {
                eprintln!(
                    "the reply to {kind} request `{request_id}` is recorded, but the call that \
                     asked had already ended, so it was not authorized"
                );
            }
            if settled.goal_stuck {
                eprintln!(
                    "the reply to {kind} request `{request_id}` is recorded, but its goal did \
                     not resume"
                );
            }
            Ok(StatusCode::NO_CONTENT)
        }
        Err(SettleError::Gone) => Err(request_not_found(kind, session_id, request_id)),
        Err(SettleError::Durable(detail)) => {
            eprintln!("failed to settle {kind} request `{request_id}`: {detail}");
            Err(ApiError::MutationFailed(format!(
                "the reply to {kind} request `{request_id}` could not be recorded"
            )))
        }
    }
}

fn request_not_found(kind: &'static str, session_id: &str, request_id: &str) -> ApiError {
    ApiError::RequestNotFound {
        kind,
        id: request_id.to_owned(),
        session_id: session_id.to_owned(),
    }
}
