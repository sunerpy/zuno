use std::sync::Arc;

use axum::Json;
use axum::body::to_bytes;
use axum::extract::rejection::PathRejection;
use axum::extract::{Extension, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_error::DbError;
use zuno_permission::ReplyKind;
use zuno_tool::question::{QuestionError, QuestionPort};
use zuno_types::question::{
    QuestionAction, QuestionCommand, QuestionReceipt, QuestionState, QuestionView,
};

use super::Data;
use super::error::ApiError;
use super::state::ApiState;
use crate::ServerServices;
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
    Extension(questions): Extension<Arc<dyn QuestionPort>>,
) -> Result<Json<LocationResponse<QuestionView>>, QuestionHttpError> {
    let lookup = state.clone();
    let sessions = tokio::task::spawn_blocking(move || {
        lookup
            .sessions()
            .list(&zuno_db::session::ListQuery::global())
    })
    .await
    .map_err(worker_error)??;
    let mut pending = Vec::new();
    for session in sessions {
        pending.extend(questions.pending(&session.id).await?);
    }
    pending.sort_by(|left, right| {
        left.time_created
            .cmp(&right.time_created)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(Json(location_response(&state, pending)))
}

pub async fn session_questions(
    State(state): State<ApiState>,
    Extension(questions): Extension<Arc<dyn QuestionPort>>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Data<Vec<QuestionView>>>, QuestionHttpError> {
    let Path(session_id) = path.map_err(|_| invalid_question("question path is invalid"))?;
    validate_question_id(&session_id, "ses")?;
    require_question_session(state, &session_id).await?;
    Ok(Json(Data::new(questions.pending(&session_id).await?)))
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
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    Extension(questions): Extension<Arc<dyn QuestionPort>>,
    request: Request,
) -> Result<Json<Data<QuestionReceipt>>, QuestionHttpError> {
    apply_question(state, questions, path, request, QuestionRoute::Reply).await
}

pub async fn question_reject(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    Extension(questions): Extension<Arc<dyn QuestionPort>>,
    request: Request,
) -> Result<Json<Data<QuestionReceipt>>, QuestionHttpError> {
    apply_question(state, questions, path, request, QuestionRoute::Cancel).await
}

pub async fn question_defer(
    State(state): State<ApiState>,
    path: Result<Path<(String, String)>, PathRejection>,
    Extension(questions): Extension<Arc<dyn QuestionPort>>,
    request: Request,
) -> Result<Json<Data<QuestionReceipt>>, QuestionHttpError> {
    apply_question(state, questions, path, request, QuestionRoute::Defer).await
}

enum QuestionRoute {
    Reply,
    Cancel,
    Defer,
}

async fn apply_question(
    state: ApiState,
    questions: Arc<dyn QuestionPort>,
    path: Result<Path<(String, String)>, PathRejection>,
    request: Request,
    route: QuestionRoute,
) -> Result<Json<Data<QuestionReceipt>>, QuestionHttpError> {
    let Path((session_id, request_id)) =
        path.map_err(|_| invalid_question("question path is invalid"))?;
    validate_question_id(&session_id, "ses")?;
    validate_question_id(&request_id, "que")?;
    let command: QuestionCommand = parse_reply(request)
        .await
        .map_err(|error| QuestionError::Invalid(error.to_string()))?;
    command.validate().map_err(QuestionError::from)?;
    match (&route, &command.action) {
        (QuestionRoute::Reply, _)
        | (QuestionRoute::Cancel, QuestionAction::Cancel)
        | (QuestionRoute::Defer, QuestionAction::Defer { .. }) => {}
        _ => {
            return Err(invalid_question(
                "question action does not match this route",
            ));
        }
    }
    require_question_session(state, &session_id).await?;
    // The port owns validation against the stored items, CAS, idempotency, Plan
    // authorization, inbox admission, and post-commit notification. A failed body
    // or request lookup never claims or closes a valid question.
    let receipt = questions.apply(&session_id, &request_id, command).await?;
    Ok(Json(Data::new(receipt)))
}

async fn require_question_session(
    state: ApiState,
    session_id: &str,
) -> Result<(), QuestionHttpError> {
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || state.sessions().get(&session_id))
        .await
        .map_err(worker_error)??;
    Ok(())
}

fn validate_question_id(id: &str, prefix: &str) -> Result<(), QuestionHttpError> {
    if id.starts_with(prefix)
        && id.len() > prefix.len()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(invalid_question("question or session ID is invalid"))
    }
}

fn invalid_question(message: &str) -> QuestionHttpError {
    QuestionError::Invalid(message.to_owned()).into()
}

fn worker_error(error: tokio::task::JoinError) -> QuestionHttpError {
    QuestionError::Database(DbError::Query {
        source: Box::new(error),
    })
    .into()
}

#[derive(Debug)]
pub(super) struct QuestionHttpError(QuestionError);

impl From<QuestionError> for QuestionHttpError {
    fn from(error: QuestionError) -> Self {
        Self(error)
    }
}

impl From<DbError> for QuestionHttpError {
    fn from(error: DbError) -> Self {
        Self(QuestionError::Database(error))
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct QuestionErrorResponse {
    error: QuestionErrorBody,
}

#[derive(Debug, Serialize, JsonSchema)]
struct QuestionErrorBody {
    message: String,
    #[serde(flatten)]
    details: QuestionErrorDetails,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(
    tag = "code",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum QuestionErrorDetails {
    InvalidRequest,
    NotFound {
        session_id: String,
        request_id: String,
    },
    SessionNotFound {
        session_id: String,
    },
    QuestionRevisionConflict {
        request_id: String,
        expected: i64,
        actual: i64,
    },
    QuestionCommandConflict {
        command_id: String,
    },
    QuestionClosed {
        request_id: String,
        state: QuestionState,
    },
    QuestionRejected {
        reason: String,
    },
    BackendUnavailable,
    QuestionInterrupted,
    DatabaseError,
}

impl IntoResponse for QuestionHttpError {
    fn into_response(self) -> Response {
        let message = match &self.0 {
            QuestionError::Database(DbError::NotFound { .. }) => self.0.to_string(),
            QuestionError::Database(_) => "internal database error".to_owned(),
            _ => self.0.to_string(),
        };
        let (status, details) = match self.0 {
            QuestionError::Invalid(_) => (
                StatusCode::BAD_REQUEST,
                QuestionErrorDetails::InvalidRequest,
            ),
            QuestionError::NotFound {
                session_id,
                request_id,
            } => (
                StatusCode::NOT_FOUND,
                QuestionErrorDetails::NotFound {
                    session_id,
                    request_id,
                },
            ),
            QuestionError::Conflict {
                request_id,
                expected,
                actual,
            } => (
                StatusCode::CONFLICT,
                QuestionErrorDetails::QuestionRevisionConflict {
                    request_id,
                    expected,
                    actual,
                },
            ),
            QuestionError::CommandConflict { command_id } => (
                StatusCode::CONFLICT,
                QuestionErrorDetails::QuestionCommandConflict { command_id },
            ),
            QuestionError::Closed { request_id, state } => (
                StatusCode::CONFLICT,
                QuestionErrorDetails::QuestionClosed { request_id, state },
            ),
            QuestionError::Rejected { code, .. } => (
                StatusCode::CONFLICT,
                QuestionErrorDetails::QuestionRejected {
                    reason: code.to_owned(),
                },
            ),
            QuestionError::Unavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                QuestionErrorDetails::BackendUnavailable,
            ),
            QuestionError::Interrupted => (
                StatusCode::CONFLICT,
                QuestionErrorDetails::QuestionInterrupted,
            ),
            QuestionError::Database(DbError::NotFound { id, .. }) => (
                StatusCode::NOT_FOUND,
                QuestionErrorDetails::SessionNotFound { session_id: id },
            ),
            QuestionError::Database(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                QuestionErrorDetails::DatabaseError,
            ),
        };
        let error = QuestionErrorBody { message, details };
        (status, Json(QuestionErrorResponse { error })).into_response()
    }
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
