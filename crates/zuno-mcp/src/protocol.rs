use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::stdio::Notification;

pub(crate) type PendingResult = Result<Value, ReaderFailure>;
pub(crate) type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<PendingResult>>>>;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RpcResponseError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

impl std::fmt::Display for RpcResponseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "JSON-RPC error {}: {}", self.code, self.message)?;
        if let Some(data) = &self.data {
            write!(formatter, " ({data})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RpcResponseError {}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExchangeError {
    #[error("MCP connection closed")]
    Closed,
    #[error("MCP request timed out")]
    Timeout,
    #[error("MCP request could not be encoded")]
    Encode(#[source] serde_json::Error),
    #[error("MCP response result could not be decoded")]
    DecodeResult(#[source] serde_json::Error),
    #[error("MCP stdout line was not JSON")]
    FrameDecode { line: Arc<str> },
    #[error("MCP stdin write failed")]
    Write(#[source] io::Error),
    #[error("MCP stdout read failed")]
    Read(#[source] io::Error),
    #[error(transparent)]
    Rpc(#[from] RpcResponseError),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
pub(crate) enum ReaderFailure {
    Closed,
    Io {
        kind: io::ErrorKind,
        message: Arc<str>,
    },
    Decode {
        line: Arc<str>,
    },
}

impl From<ReaderFailure> for ExchangeError {
    fn from(failure: ReaderFailure) -> Self {
        match failure {
            ReaderFailure::Closed => Self::Closed,
            ReaderFailure::Io { kind, message } => {
                Self::Read(io::Error::new(kind, message.to_string()))
            }
            ReaderFailure::Decode { line } => Self::FrameDecode { line },
        }
    }
}

pub(crate) fn decode_response(method: &str, message: Value) -> Result<Value, ExchangeError> {
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(ExchangeError::Invalid(format!(
            "MCP response for {method} did not use jsonrpc 2.0"
        )));
    }
    if let Some(error) = message.get("error") {
        let error = serde_json::from_value(error.clone()).map_err(ExchangeError::DecodeResult)?;
        return Err(ExchangeError::Rpc(error));
    }
    message.get("result").cloned().ok_or_else(|| {
        ExchangeError::Invalid(format!(
            "MCP response for {method} contained neither result nor error"
        ))
    })
}

pub(crate) fn route_message(
    server: &str,
    pending: &Pending,
    notifications: &broadcast::Sender<Notification>,
    refresh: &mpsc::Sender<()>,
    message: Value,
) {
    let Some(object) = message.as_object() else {
        tracing::warn!(%server, "MCP server emitted a non-object JSON-RPC message");
        return;
    };

    if let Some(method) = object.get("method").and_then(Value::as_str) {
        if let Some(id) = object.get("id") {
            tracing::warn!(%server, id = %id, %method, "unsupported MCP server request");
            return;
        }
        let notification = Notification {
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        };
        if method == "notifications/tools/list_changed" {
            let _result = refresh.try_send(());
        }
        let _receivers = notifications.send(notification);
        return;
    }

    let Some(id_value) = object.get("id") else {
        tracing::warn!(%server, "MCP server emitted a message with neither method nor id");
        return;
    };
    let Some(id) = id_value.as_u64() else {
        tracing::warn!(%server, id = %id_value, "MCP response id was not an unsigned integer");
        return;
    };
    let sender = lock(pending).remove(&id);
    match sender {
        Some(sender) => {
            let _receiver = sender.send(Ok(message));
        }
        None => tracing::warn!(%server, id, "MCP response id has no pending request"),
    }
}

pub(crate) fn fail_pending(pending: &Pending, failure: ReaderFailure) {
    let waiters: Vec<_> = lock(pending).drain().map(|(_, waiter)| waiter).collect();
    for waiter in waiters {
        let _receiver = waiter.send(Err(failure.clone()));
    }
}

pub(crate) fn decode_error(line: &str) -> serde_json::Error {
    match serde_json::from_str::<Value>(line) {
        Err(error) => error,
        Ok(_) => serde_json::Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            "reader reported a JSON decode failure for a valid value",
        )),
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
