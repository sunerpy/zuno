mod client;
mod exchange;
mod legacy;
mod sse;
mod transport;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use oc_auth::{McpAuthStore, Secret};
use oc_config::schema::mcp::McpRemote;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Response, StatusCode, Url};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::protocol::{Pending, ReaderFailure, fail_pending, lock};
use crate::stdio::{InitializeResult, Notification};
use sse::SseDecoder;

const MAX_LIST_PAGES: usize = 1_000;
const NOTIFICATION_CAPACITY: usize = 64;
const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";

/// The remote transport selected after connection negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTransport {
    /// MCP Streamable HTTP.
    StreamableHttp,
    /// The legacy HTTP GET event stream plus POST endpoint.
    Sse,
}

impl fmt::Display for RemoteTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StreamableHttp => formatter.write_str("streamable-http"),
            Self::Sse => formatter.write_str("sse"),
        }
    }
}

/// Result of attempting to connect a remote MCP server.
#[derive(Debug)]
pub enum RemoteConnect {
    /// The MCP handshake completed.
    Connected(RemoteClient),
    /// OAuth must be completed before the handshake can resume.
    AuthorizationRequired(Box<AuthorizationRequest>),
}

/// Browser authorization information created after an authentication challenge.
pub struct AuthorizationRequest {
    authorization_url: String,
    server: String,
    config: McpRemote,
    store: McpAuthStore,
    pending: crate::oauth::PendingAuthorization,
}

impl AuthorizationRequest {
    /// URL the user agent must open.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// Validates callback state, exchanges the code, persists tokens, and reconnects.
    pub async fn finish(
        self,
        authorization_code: &str,
        returned_state: &str,
    ) -> Result<RemoteConnect, RemoteError> {
        crate::oauth::finish_authorization(
            self.server,
            self.config,
            self.store,
            self.pending,
            authorization_code,
            returned_state,
        )
        .await
    }

    pub(crate) fn new(
        authorization_url: String,
        server: String,
        config: McpRemote,
        store: McpAuthStore,
        pending: crate::oauth::PendingAuthorization,
    ) -> Self {
        Self {
            authorization_url,
            server,
            config,
            store,
            pending,
        }
    }
}

impl fmt::Debug for AuthorizationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizationRequest")
            .field("server", &self.server)
            .field("authorization_url", &"<redacted>")
            .finish()
    }
}

/// Remote MCP connection and OAuth errors.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The configured URL or header is invalid.
    #[error("remote MCP {server} has invalid configuration: {message}")]
    Config { server: String, message: String },
    /// An HTTP operation failed before a response was received.
    #[error("remote MCP {server} {transport} request failed")]
    Http {
        server: String,
        transport: RemoteTransport,
        #[source]
        source: reqwest::Error,
    },
    /// The server returned an unsuccessful HTTP status.
    #[error("remote MCP {server} {transport} returned HTTP {status}")]
    Status {
        server: String,
        transport: RemoteTransport,
        status: StatusCode,
        challenge: Option<String>,
    },
    /// A wire response violated the MCP or SSE protocol.
    #[error("remote MCP {server} {transport} protocol error: {message}")]
    Protocol {
        server: String,
        transport: RemoteTransport,
        message: String,
    },
    /// A request exceeded the configured deadline.
    #[error("remote MCP {server} request timed out after {elapsed:?}")]
    Timeout { server: String, elapsed: Duration },
    /// OAuth was explicitly disabled for a server requiring authorization.
    #[error("remote MCP {server} requires authorization but oauth is disabled")]
    OAuthDisabled { server: String },
    /// OAuth discovery or token handling failed.
    #[error("remote MCP {server} OAuth failed: {message}")]
    OAuth { server: String, message: String },
    /// Neither remote transport could connect.
    #[error("remote MCP failed with both streamable HTTP and legacy SSE")]
    Fallback {
        streamable: Box<RemoteError>,
        sse: Box<RemoteError>,
    },
}

impl RemoteError {
    /// Whether a direct or fallback transport failure was caused by a deadline.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Timeout { .. } => true,
            Self::Http { source, .. } => source.is_timeout(),
            Self::Fallback { streamable, sse } => streamable.is_timeout() && sse.is_timeout(),
            _ => false,
        }
    }

    fn is_authorization_required(&self) -> bool {
        matches!(
            self,
            Self::Status {
                status: StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN,
                ..
            }
        )
    }

    pub(crate) fn challenge(&self) -> Option<&str> {
        match self {
            Self::Status { challenge, .. } => challenge.as_deref(),
            _ => None,
        }
    }
}

/// A connected remote MCP client.
#[derive(Clone)]
pub struct RemoteClient {
    inner: Arc<RemoteInner>,
}

impl fmt::Debug for RemoteClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteClient")
            .field("server", &self.inner.server)
            .field("transport", &self.inner.transport)
            .field("timeout", &self.inner.timeout)
            .finish_non_exhaustive()
    }
}

struct RemoteInner {
    server: String,
    base_url: Url,
    timeout: Duration,
    transport: RemoteTransport,
    http: reqwest::Client,
    headers: HeaderMap,
    bearer: Option<Secret>,
    next_id: AtomicU64,
    pending: Pending,
    notifications: broadcast::Sender<Notification>,
    refresh: mpsc::Sender<()>,
    initialization: OnceLock<InitializeResult>,
    session_id: tokio::sync::Mutex<Option<HeaderValue>>,
    legacy: Option<LegacyState>,
    operation: tokio::sync::Mutex<()>,
    closed: AtomicBool,
}

struct LegacyState {
    endpoint: Url,
    source: tokio::sync::Mutex<Option<(Response, SseDecoder)>>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for RemoteInner {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        fail_pending(&self.pending, ReaderFailure::Closed);
        if let Some(legacy) = &self.legacy
            && let Some(reader) = lock(&legacy.reader).take()
        {
            reader.abort();
        }
    }
}
