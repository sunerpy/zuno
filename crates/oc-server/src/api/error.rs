use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use oc_db::prune::PruneError;
use oc_db::session_prune::SessionPruneError;
use oc_error::DbError;
use oc_pty::PtyError;
use serde::Serialize;

use crate::EventStreamError;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("backend unavailable for {0}")]
    BackendUnavailable(String),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Pty(#[from] PtyError),
    #[error(transparent)]
    Maintenance(#[from] SessionPruneError),
    #[error(transparent)]
    Event(#[from] EventStreamError),
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::InvalidRequest(message) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                message.to_owned(),
            ),
            Self::BackendUnavailable(operation) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "backend_unavailable",
                format!("backend unavailable for {operation}"),
            ),
            Self::Database(DbError::NotFound { table, id }) => (
                StatusCode::NOT_FOUND,
                "not_found",
                format!("{table} `{id}` was not found"),
            ),
            Self::Pty(PtyError::NotFound { id }) => (
                StatusCode::NOT_FOUND,
                "not_found",
                format!("pty session `{id}` was not found"),
            ),
            Self::Database(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "database_error",
                "internal database error".to_owned(),
            ),
            Self::Pty(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "pty_error",
                "internal pseudo-terminal error".to_owned(),
            ),
            Self::Maintenance(SessionPruneError::Prune(PruneError::ConfirmationRequired)) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "session delete requires explicit confirmation".to_owned(),
            ),
            Self::Maintenance(SessionPruneError::Prune(PruneError::RemoteUnshareFailed {
                session_id,
                detail,
            })) => (
                StatusCode::CONFLICT,
                "maintenance_refused",
                format!(
                    "remote unshare failed for shared session {session_id}: {detail}; local rows were not deleted"
                ),
            ),
            Self::Maintenance(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "maintenance_failed",
                "session maintenance failed".to_owned(),
            ),
            Self::Event(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "event_stream_failed",
                "session was created but its event could not be published".to_owned(),
            ),
        };
        (
            status,
            Json(ErrorEnvelope {
                error: ErrorBody { code, message },
            }),
        )
            .into_response()
    }
}
