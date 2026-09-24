use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::sync::oneshot;

mod client;
mod frame;
mod server;

pub use client::ClientConnection;
use frame::id_key;
pub use server::serve_stdio;

/// JSON-RPC code for a prompt admitted durably without owning its own turn.
///
/// The value avoids every code with a defined meaning on either side of the
/// bridge: ACP v1 reserves `-32000` (authentication required) and `-32002`
/// (resource not found), and Codex App Server reports `-32001` when its request
/// queue is full. Those downstream codes pass through unchanged, so the bridge's
/// own outcomes must not alias them.
pub const SESSION_BUSY_CODE: i64 = -32010;
/// JSON-RPC code for a rejected `session/steer` extension request.
pub const STEER_REJECTED_CODE: i64 = -32011;

/// Transport-owned identity of one accepted client request.
///
/// JSON-RPC ids may be reused after a response. The transport pairs their
/// canonical wire key with a fresh invocation identity, so late withdrawal state
/// and cleanup cannot alias a later request using the same wire id. Durable
/// message idempotence remains the Agent's separate responsibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId {
    wire_key: String,
    invocation: u64,
}

impl RequestId {
    /// Allocate a new invocation identity for a valid JSON-RPC wire id.
    /// Clone this value to retain ownership; parsing the same id creates a new one.
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        static NEXT_INVOCATION: AtomicU64 = AtomicU64::new(1);
        id_key(value).map(|wire_key| Self {
            wire_key,
            invocation: NEXT_INVOCATION.fetch_add(1, Ordering::Relaxed),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ACP request failed ({code}): {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(-32600, message)
    }

    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("Method not found: {method}"))
    }

    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    /// Preserve a downstream JSON-RPC error code and structured detail.
    #[must_use]
    pub fn downstream(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(-32800, message)
    }

    /// The session already owns a live turn, so this request is not its own turn.
    ///
    /// ACP v1 has no success shape for "accepted into a different request's turn":
    /// `stopReason` is a closed enum, and every member would be a false claim about
    /// a turn this request never ran. JSON-RPC reserves -32000 through -32099 for
    /// implementation-defined server errors, so the outcome is reported there with
    /// machine-readable [`Self::data`] describing the durable admission.
    #[must_use]
    pub fn session_busy(message: impl Into<String>) -> Self {
        Self::new(SESSION_BUSY_CODE, message)
    }

    /// The explicit steering extension could not target the requested live turn.
    #[must_use]
    pub fn steer_rejected(message: impl Into<String>) -> Self {
        Self::new(STEER_REJECTED_CODE, message)
    }

    /// Attach machine-readable detail to this error response.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn value(&self) -> Value {
        let mut error = json!({
            "code": self.code,
            "message": self.message,
        });
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        error
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("ACP transport I/O failed")]
    Io(#[from] std::io::Error),
    #[error("ACP writer task failed")]
    WriterTask(#[from] tokio::task::JoinError),
    #[error("ACP writer stopped before a frame was sent")]
    WriterClosed,
}

pub trait Agent: Send + Sync + 'static {
    fn request(
        &self,
        method: &str,
        request: &RequestId,
        params: Value,
        client: ClientConnection,
    ) -> impl std::future::Future<Output = Result<Value, RpcError>> + Send;

    fn notification(
        &self,
        method: &str,
        params: Value,
        client: ClientConnection,
    ) -> impl std::future::Future<Output = Result<(), RpcError>> + Send;

    /// Notify the Agent when a client explicitly sends `$/cancel_request`.
    ///
    /// The transport owns JSON-RPC request ids, while the Agent owns session and
    /// process-tree cancellation. `request` is the withdrawn request's identity, so
    /// the Agent can retire exactly what that request started; the method and
    /// params say what it was, without teaching the transport about
    /// product-specific session fields.
    fn request_cancelled(
        &self,
        _method: &str,
        _request: &RequestId,
        _params: &Value,
    ) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    /// Notify the Agent before disconnect drops an in-flight request observer.
    ///
    /// Loss of a connection does not withdraw accepted durable input. Session
    /// owners decide how native execution shuts down or recovers separately from
    /// the lifetime of this transport's request future.
    fn request_disconnected(
        &self,
        _method: &str,
        _request: &RequestId,
        _params: &Value,
    ) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }
}

/// Maximum number of encoded ACP frames waiting for the stdout writer.
///
/// The queue is lossless: once full, request tasks wait while the reader keeps
/// accepting client responses and the writer keeps draining frames. This bounds
/// memory without allowing a slow editor to make the adapter allocate forever.
pub const OUTBOUND_FRAME_CHANNEL_CAPACITY: usize = 64;

/// Maximum encoded JSON bytes accepted for one newline-delimited ACP frame.
pub const MAX_INBOUND_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Grace for already accepted requests to publish a ready response at clean EOF.
///
/// Editors commonly close stdin immediately after writing their final request.
/// A short drain keeps deterministic validation and lifecycle responses from
/// being replaced by cancellation, while truly blocked requests are still
/// cancelled promptly afterwards.
const EOF_REQUEST_DRAIN_GRACE: Duration = Duration::from_millis(25);

const UNINITIALIZED: u8 = 0;
const INITIALIZING: u8 = 1;
const INITIALIZED: u8 = 2;

struct InFlightRequest {
    identity: RequestId,
    // Keep the request id reserved after withdrawal until its worker has dropped
    // the Agent future. Its cleanup must never remove a newly reused request id.
    cancel: Option<oneshot::Sender<RequestTermination>>,
    method: String,
    params: Value,
    response_ready: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum RequestTermination {
    Withdrawn,
    Disconnected,
}

impl RequestTermination {
    fn error(self) -> RpcError {
        let (reason, message) = match self {
            Self::Withdrawn => ("requestWithdrawn", "request withdrawn by the client"),
            Self::Disconnected => (
                "connectionClosed",
                "connection closed before the request observer completed",
            ),
        };
        RpcError::cancelled(message).with_data(json!({"reason": reason}))
    }
}

type InFlight = HashMap<String, InFlightRequest>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
