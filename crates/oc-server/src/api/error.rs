use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use oc_error::DbError;
use oc_pty::PtyError;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    NotImplemented(&'static str),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Pty(#[from] PtyError),
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
            Self::NotImplemented(message) => (
                StatusCode::NOT_IMPLEMENTED,
                "not_implemented",
                message.to_owned(),
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
