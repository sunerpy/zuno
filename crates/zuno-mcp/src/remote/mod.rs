mod client;
mod exchange;
mod legacy;
mod sse;
mod transport;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Response, StatusCode, Url};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use zuno_auth::{McpAuthStore, Secret};
use zuno_config::schema::mcp::McpRemote;

use crate::protocol::{Pending, ReaderFailure, ReaderState, fail_pending, lock};
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

/// How a remote MCP failure must be recovered.
///
/// Three classes rather than a boolean, because "retry" and "may have already
/// happened" are different facts. A deadline leaves the request's outcome unknown,
/// so the tool layer has to say so before anything replays the call; a transport
/// hiccup did not reach a decision at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteFailureKind {
    /// A deadline expired. The request may already have taken effect on the server.
    Timeout,
    /// The identical request may succeed on a later attempt.
    Transient,
    /// The identical request fails the same way until configuration changes.
    Permanent,
}

/// Splits a non-success HTTP status into a recovery class.
///
/// 5xx is the server's own admission that a later attempt may work. 408 and 429 are
/// the only 4xx codes that mean "ask again": 408 says the server gave up before it
/// held the whole request, so nothing ran, and 429 says the rate window reopens.
/// Every other 4xx — 400, 401, 403, 404, 405 — is a property of the URL, the
/// credentials, or the payload, and retrying it hammers a server that will keep
/// refusing. A 1xx or 3xx that survived this far is a response the client will not
/// follow, which is a configuration fault rather than a delay.
///
/// 408 is classed `Transient` rather than `Timeout` on purpose: the server is
/// reporting that it never received a complete request, so unlike a client-side
/// deadline there is no side effect whose fate is unknown.
fn status_failure_kind(status: StatusCode) -> RemoteFailureKind {
    if status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
    {
        RemoteFailureKind::Transient
    } else {
        RemoteFailureKind::Permanent
    }
}

impl RemoteError {
    /// Whether OAuth could not proceed because dynamic client registration is unavailable.
    #[must_use]
    pub fn needs_client_registration(&self) -> bool {
        matches!(
            self,
            Self::OAuth { message, .. }
                if message.contains("does not support dynamic client registration")
        )
    }

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

    /// The recovery class of this failure, decided from typed variants only.
    ///
    /// This is the single authority the catalog turns into an
    /// [`McpError`](zuno_error::McpError). It exists because `RemoteError` mixes
    /// three recovery classes that used to be flattened into one retryable
    /// variant: a deadline whose side effect may already have landed on the
    /// server, a transport hiccup worth another attempt, and a configuration or
    /// authorization failure that will fail identically until a human changes
    /// something.
    pub(crate) fn failure_kind(&self) -> RemoteFailureKind {
        match self {
            // A fallback pair means *every* transport failed. The pair is only
            // worth retrying when NEITHER attempt failed permanently: a permanent
            // failure here is almost always a property the two transports share —
            // the URL, the credentials, the `oauth: false` setting — so a retry
            // that "still has the other transport left" is a request storm against
            // a server that will keep refusing. When neither is permanent and
            // either hit a deadline, the pair inherits the deadline's uncertain
            // outcome, because one of the two requests may already have taken
            // effect. This is deliberately wider than [`Self::is_timeout`], which
            // answers the narrower question "was the failure as a whole nothing
            // but a deadline" and therefore requires both halves to be timeouts.
            Self::Fallback { streamable, sse } => {
                let pair = [streamable.failure_kind(), sse.failure_kind()];
                if pair.contains(&RemoteFailureKind::Permanent) {
                    RemoteFailureKind::Permanent
                } else if pair.contains(&RemoteFailureKind::Timeout) {
                    RemoteFailureKind::Timeout
                } else {
                    RemoteFailureKind::Transient
                }
            }
            // The two shapes a deadline arrives in, matching [`Self::is_timeout`].
            Self::Timeout { .. } => RemoteFailureKind::Timeout,
            Self::Http { source, .. } if source.is_timeout() => RemoteFailureKind::Timeout,
            // A non-deadline `reqwest` failure. Only a locally malformed request
            // and a redirect loop are hopeless; connect, request, body, and decode
            // failures are the network faults the harness is expected to retry.
            Self::Http { source, .. } => {
                if source.is_builder() || source.is_redirect() {
                    RemoteFailureKind::Permanent
                } else {
                    RemoteFailureKind::Transient
                }
            }
            Self::Status { status, .. } => status_failure_kind(*status),
            // A bad URL or header, a server that needs authorization while OAuth is
            // switched off, a failed token exchange, and a wire response that
            // violated MCP or SSE all fail the same way on every attempt.
            Self::Config { .. }
            | Self::Protocol { .. }
            | Self::OAuthDisabled { .. }
            | Self::OAuth { .. } => RemoteFailureKind::Permanent,
        }
    }

    /// How long the failed request had been waiting, when a deadline measured it.
    ///
    /// Only [`Self::Timeout`] carries a measurement; a `reqwest` deadline reports
    /// no elapsed time, and a fallback pair reports the first measurement it has.
    /// A caller that needs a duration regardless substitutes the deadline it
    /// configured, exactly as the stdio transport does.
    pub(crate) fn timeout_elapsed(&self) -> Option<Duration> {
        match self {
            Self::Timeout { elapsed, .. } => Some(*elapsed),
            Self::Fallback { streamable, sse } => streamable
                .timeout_elapsed()
                .or_else(|| sse.timeout_elapsed()),
            Self::Config { .. }
            | Self::Http { .. }
            | Self::Status { .. }
            | Self::Protocol { .. }
            | Self::OAuthDisabled { .. }
            | Self::OAuth { .. } => None,
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
    /// What the legacy SSE reader has learned about its stream, including why it
    /// stopped. Empty on a streamable-HTTP connection, which reads every response
    /// inline and has no reader to outlive a request.
    reader_state: Arc<ReaderState>,
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
