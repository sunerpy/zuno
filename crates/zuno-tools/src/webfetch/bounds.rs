//! The three bounds every outbound fetch is subject to, and the failures they raise.
//!
//! # Why the bounds live in one module
//!
//! A network call is unbounded in three independent dimensions, and a missing bound
//! on any one of them is a distinct production failure:
//!
//! | dimension | missing bound | failure |
//! |-----------|---------------|---------|
//! | time      | no timeout    | the turn hangs forever on a slow endpoint |
//! | size      | no byte cap   | a large body exhausts memory |
//! | redirects | no hop cap    | a redirect cycle loops until something breaks |
//!
//! Scattering them across call sites is how one of them goes missing, so they are
//! declared together, each with the oracle line it came from, and each is covered by
//! a `wiremock` test in `tests/webfetch.rs`.
//!
//! # Why a local error enum
//!
//! [`zuno_error::ToolError`] deliberately has no `Other(String)` variant, so a web
//! failure has to be *classified* before it can be reported. This enum is that
//! classification; it appears only in [`zuno_error::ToolError`]'s `#[source]`
//! position, where the variant holding it has already answered "what kind of
//! failure is this".

use std::time::Duration;
use zuno_network::DiagnosticEndpoint;

/// The maximum number of body bytes a fetch will accept.
///
/// Oracle: `packages/core/src/tool/webfetch.ts:17`
/// (`MAX_RESPONSE_BYTES = 5 * 1024 * 1024`), matching
/// `packages/opencode/src/tool/webfetch.ts:9` (`MAX_RESPONSE_SIZE`).
pub const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;

/// The default overall time budget for one fetch.
///
/// Oracle: `packages/core/src/tool/webfetch.ts:18`
/// (`DEFAULT_TIMEOUT_SECONDS = 30`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest time budget a caller may ask for.
///
/// Oracle: `packages/core/src/tool/webfetch.ts:19`
/// (`MAX_TIMEOUT_SECONDS = 120`). A caller-supplied value above this is clamped
/// rather than refused, matching `packages/opencode/src/tool/webfetch.ts:50`
/// (`Math.min(..., MAX_TIMEOUT)`).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(120);

/// The maximum number of redirect hops followed before the chain is abandoned.
///
/// **Not from the oracle.** Upstream never states a hop cap: `HttpClient` inherits
/// whatever the platform fetch does, which on Node is undici's `maxRedirections`.
/// Leaving it implicit is how a redirect cycle becomes an unbounded loop, so the
/// bound is named here and pinned by a test. Five is enough for legitimate scheme
/// and canonical-host transitions without letting an attacker turn validation into
/// an unbounded resolver loop.
pub const MAX_REDIRECTS: usize = 5;

/// The number of body bytes buffered before the response size is known.
///
/// Oracle: `packages/core/src/tool/http-body.ts:17` buffers
/// `Math.min(maximumBytes, declaredSize || 64 * 1024)`, so an undeclared body
/// starts at 64 KiB and grows.
pub const INITIAL_BODY_CAPACITY: usize = 64 * 1024;

/// A classified web failure, carried in [`zuno_error::ToolError`]'s `#[source]`.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// The argument was not a URL, or not an `http`/`https` one.
    ///
    /// Oracle: `packages/core/src/tool/webfetch.ts:82-84` (`assertHttpUrl`).
    #[error("URL must use http:// or https://, got {url}")]
    UnsupportedScheme {
        /// The rejected URL, echoed so the message names the input.
        url: String,
    },

    /// The URL did not parse.
    #[error("could not parse {url} as a URL")]
    MalformedUrl {
        /// The rejected input.
        url: String,
        /// The parse failure.
        #[source]
        source: url::ParseError,
    },

    /// The target failed public-network validation.
    #[error(transparent)]
    PublicTarget {
        /// Typed resolver, address, redirect, or transport failure.
        #[from]
        source: zuno_network::PublicHttpError,
    },

    /// The public transport failed while streaming a validated response body.
    #[error("response body from {url} failed")]
    PublicBody {
        /// Credential-free endpoint.
        url: String,
        /// Hyper/socket body failure.
        #[source]
        source: std::io::Error,
    },

    /// The response exceeded [`MAX_RESPONSE_BYTES`].
    ///
    /// Carries how far the read got, so a caller can tell a declared-size rejection
    /// (`read == 0`) from a mid-stream abort.
    #[error("response too large (exceeds {limit} byte limit, saw {read} bytes)")]
    TooLarge {
        /// The cap that was exceeded.
        limit: usize,
        /// Bytes read before the abort; `0` when `content-length` alone was decisive.
        read: usize,
    },

    /// The redirect chain exceeded [`MAX_REDIRECTS`] hops.
    #[error("redirect chain exceeded {limit} hops and was abandoned")]
    TooManyRedirects {
        /// The hop cap that was exceeded.
        limit: usize,
    },

    /// The transport failed: connection refused, TLS failure, mid-stream reset.
    #[error("request to {url} failed")]
    Transport {
        /// The URL that failed.
        url: String,
        /// The transport failure.
        #[source]
        source: reqwest::Error,
    },

    /// A search-provider transport failure with a credential-free endpoint.
    #[error("web search provider `{provider}` request to {endpoint} failed")]
    SearchTransport {
        /// Stable provider id.
        provider: &'static str,
        /// Scheme, host, port, and path only.
        endpoint: DiagnosticEndpoint,
        /// Reqwest failure stripped of its URL.
        #[source]
        source: reqwest::Error,
    },

    /// The server answered, but not with success.
    ///
    /// Oracle: upstream runs every fetch through `filterStatusOk`
    /// (`packages/core/src/tool/webfetch.ts:88`), so a non-2xx is a failure rather
    /// than a body to hand the model.
    #[error("{url} returned HTTP {status}")]
    Status {
        /// The URL that answered.
        url: String,
        /// The status code returned.
        status: u16,
        /// Delay requested by the server, when it supplied numeric seconds.
        retry_after: Option<Duration>,
    },

    /// A search provider answered with a non-success status.
    #[error("web search provider `{provider}` endpoint {endpoint} returned HTTP {status}")]
    SearchStatus {
        /// Stable provider id.
        provider: &'static str,
        /// Scheme, host, port, and path only.
        endpoint: DiagnosticEndpoint,
        /// HTTP status code.
        status: u16,
        /// Delay requested by the provider, when supplied.
        retry_after: Option<Duration>,
    },

    /// The content type is not something this tool can turn into text.
    ///
    /// Oracle: `packages/core/src/tool/webfetch.ts:150-153`.
    #[error("unsupported fetched content type: {mime}")]
    UnsupportedContentType {
        /// The rejected media type.
        mime: String,
    },

    /// The caller cancelled the turn while the body was streaming.
    #[error("interrupted while reading the response body after {read} bytes")]
    Interrupted {
        /// Bytes read before the interrupt was observed.
        read: usize,
    },

    /// No search provider is configured, so the search tool cannot run.
    ///
    /// Reaching this is a registry bug: an unconfigured `websearch` is meant to be
    /// absent from the tool list, not present and failing. It exists so that bug
    /// surfaces as a named failure instead of a confusing transport error.
    #[error("no web search provider is configured")]
    NoSearchProvider,

    /// A selected search backend requires a credential that is not present.
    #[error("web search provider `{provider}` requires PARALLEL_API_KEY")]
    MissingSearchCredential {
        /// Stable provider id used in configuration and diagnostics.
        provider: &'static str,
    },

    /// The search backend's response was not the JSON-RPC envelope expected.
    #[error("could not parse the {provider} search response")]
    MalformedSearchResponse {
        /// The provider whose response failed to parse.
        provider: &'static str,
    },

    /// A configured or test-overridden search endpoint was not an absolute URL.
    #[error("web search provider `{provider}` has an invalid endpoint")]
    InvalidSearchEndpoint {
        /// Stable provider id.
        provider: &'static str,
    },
}

impl WebError {
    /// Whether repeating the same HTTP request may succeed after backoff.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        match self {
            Self::Transport { .. } | Self::SearchTransport { .. } | Self::PublicBody { .. } => true,
            Self::PublicTarget { source } => source.is_transient(),
            Self::Status { status, .. } | Self::SearchStatus { status, .. } => {
                matches!(*status, 408 | 425 | 429) || (*status >= 500 && *status <= 599)
            }
            Self::UnsupportedScheme { .. }
            | Self::MalformedUrl { .. }
            | Self::TooLarge { .. }
            | Self::TooManyRedirects { .. }
            | Self::UnsupportedContentType { .. }
            | Self::Interrupted { .. }
            | Self::NoSearchProvider
            | Self::MissingSearchCredential { .. }
            | Self::MalformedSearchResponse { .. }
            | Self::InvalidSearchEndpoint { .. } => false,
        }
    }

    /// Delay requested by the failed server, when one was supplied.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Status { retry_after, .. } | Self::SearchStatus { retry_after, .. } => {
                *retry_after
            }
            _ => None,
        }
    }
}

/// Parse a numeric `Retry-After` response header as seconds.
#[must_use]
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    if !seconds.is_finite() || seconds.is_sign_negative() {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}

/// Clamps a caller-supplied timeout into `(0, MAX_TIMEOUT]`.
///
/// Oracle: `Math.min((params.timeout ?? DEFAULT) * 1000, MAX_TIMEOUT)`
/// (`packages/opencode/src/tool/webfetch.ts:50`). A zero or absent value falls back
/// to [`DEFAULT_TIMEOUT`] rather than becoming an instantly-expiring budget.
#[must_use]
pub fn resolve_timeout(requested_seconds: Option<u64>) -> Duration {
    match requested_seconds {
        None | Some(0) => DEFAULT_TIMEOUT,
        Some(seconds) => Duration::from_secs(seconds).min(MAX_TIMEOUT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_timeout_is_the_default() {
        assert_eq!(resolve_timeout(None), DEFAULT_TIMEOUT);
    }

    #[test]
    fn a_zero_timeout_is_the_default_not_an_instant_expiry() {
        assert_eq!(resolve_timeout(Some(0)), DEFAULT_TIMEOUT);
    }

    #[test]
    fn an_oversized_timeout_is_clamped_not_refused() {
        assert_eq!(resolve_timeout(Some(9_999)), MAX_TIMEOUT);
    }

    #[test]
    fn a_timeout_inside_the_budget_is_honoured_exactly() {
        assert_eq!(resolve_timeout(Some(5)), Duration::from_secs(5));
    }
}
