use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use zuno_db::prune::PruneError;
use zuno_db::session_prune::SessionPruneError;
use zuno_error::DbError;
use zuno_pty::PtyError;

use crate::EventStreamError;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("forbidden")]
    Forbidden,
    #[error("backend unavailable for {0}")]
    BackendUnavailable(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{kind} request `{id}` is not pending for session `{session_id}`")]
    RequestNotFound {
        kind: &'static str,
        id: String,
        session_id: String,
    },
    #[error("{0}")]
    MutationFailed(String),
    /// A filesystem path left the session directory.
    ///
    /// Upstream turns this into an opaque `500 UnknownError` with a random ref
    /// (`filesystem.ts:68-71` dies, and the HTTP layer renders a defect). This
    /// port answers `403` and names the violation instead, which is the stricter
    /// of the two behaviours and the only intentional divergence in the
    /// filesystem group.
    #[error("path escapes the session directory")]
    PathEscapedRoot,
    /// A filesystem path did not resolve to an existing file or directory.
    #[error("`{0}` was not found in the session directory")]
    PathNotFound(String),
    /// The session directory itself could not be read.
    #[error("the session directory could not be read")]
    FilesystemUnavailable,
    /// A required query key was absent.
    #[error("missing query key `{0}`")]
    MissingQueryKey(&'static str),
    /// A query value was present but not usable.
    #[error("invalid query value for `{0}`")]
    InvalidQueryValue(&'static str),
    /// The user's config tree does not parse.
    #[error("{0}")]
    ConfigInvalid(String),
    /// The catalogue could not be resolved.
    #[error("{0}")]
    CatalogUnavailable(String),
    /// No provider is registered under the requested id.
    #[error("Provider not found: {0}")]
    ProviderNotFound(String),
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

/// Upstream's tagged error body, which is a different envelope from this port's
/// `{error: {code, message}}`.
///
/// It exists for exactly the operations whose failure body the differential
/// compares: a client that switches on `_tag` (which the generated SDK does) must
/// see `_tag`, so for those two cases parity wins over local consistency.
#[derive(Serialize)]
struct TaggedError {
    #[serde(rename = "_tag")]
    tag: &'static str,
    #[serde(rename = "providerID", skip_serializing_if = "Option::is_none")]
    provider_id: Option<String>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // The upstream-shaped failures are handled before the local envelope so
        // that a caller of `/api/provider/{id}` or `/api/fs/find` sees the body the
        // released binary sends.
        match self {
            Self::ProviderNotFound(provider_id) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(TaggedError {
                        tag: "ProviderNotFoundError",
                        message: format!("Provider not found: {provider_id}"),
                        provider_id: Some(provider_id),
                        kind: None,
                    }),
                )
                    .into_response();
            }
            Self::MissingQueryKey(key) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(TaggedError {
                        tag: "InvalidRequestError",
                        provider_id: None,
                        message: format!("Missing key\n  at [\"{key}\"]"),
                        kind: Some("Query"),
                    }),
                )
                    .into_response();
            }
            _ => {}
        }
        let (status, code, message) = match self {
            Self::InvalidRequest(message) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                message.to_owned(),
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "request is not authorized".to_owned(),
            ),
            Self::BackendUnavailable(operation) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "backend_unavailable",
                format!("backend unavailable for {operation}"),
            ),
            Self::Conflict(message) => (StatusCode::CONFLICT, "conflict", message),
            Self::RequestNotFound {
                kind,
                id,
                session_id,
            } => (
                StatusCode::NOT_FOUND,
                "not_found",
                format!("{kind} request `{id}` is not pending for session `{session_id}`"),
            ),
            Self::MutationFailed(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "mutation_failed",
                message,
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
            Self::PathEscapedRoot => (
                StatusCode::FORBIDDEN,
                "path_escaped_root",
                "the requested path leaves the session directory".to_owned(),
            ),
            Self::PathNotFound(path) => (
                StatusCode::NOT_FOUND,
                "not_found",
                format!("`{path}` was not found in the session directory"),
            ),
            Self::FilesystemUnavailable => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "filesystem_error",
                "the session directory could not be read".to_owned(),
            ),
            Self::InvalidQueryValue(key) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("`{key}` must be a positive integer"),
            ),
            Self::ConfigInvalid(report) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "config_invalid", report)
            }
            Self::CatalogUnavailable(detail) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "catalog_unavailable",
                detail,
            ),
            Self::ProviderNotFound(_) | Self::MissingQueryKey(_) => {
                unreachable!("the upstream-shaped failures return before the local envelope")
            }
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
