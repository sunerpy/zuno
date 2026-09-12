//! Failures reported by a model provider.

use crate::recovery::{Recoverable, Recovery};
use crate::source::BoxSource;
use std::fmt;
use std::time::Duration;

/// A structured provider stream failure that may be replayed as a replacement attempt.
///
/// These codes are emitted only after the HTTP response has started, so no status
/// code remains available. The code is retained as data because the engine must
/// distinguish a replay-safe truncated attempt from an opaque transport error
/// after partial output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStreamFailure {
    UpstreamStreamError,
    UpstreamStreamIncomplete,
    UpstreamStreamIdleTimeout,
    MalformedUpstreamToolArguments,
    RequestDeadlineExceeded,
}

impl ProviderStreamFailure {
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "upstream_stream_error" => Some(Self::UpstreamStreamError),
            "upstream_stream_incomplete" => Some(Self::UpstreamStreamIncomplete),
            "upstream_stream_idle_timeout" => Some(Self::UpstreamStreamIdleTimeout),
            "malformed_upstream_tool_arguments" => Some(Self::MalformedUpstreamToolArguments),
            "request_deadline_exceeded" => Some(Self::RequestDeadlineExceeded),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpstreamStreamError => "upstream_stream_error",
            Self::UpstreamStreamIncomplete => "upstream_stream_incomplete",
            Self::UpstreamStreamIdleTimeout => "upstream_stream_idle_timeout",
            Self::MalformedUpstreamToolArguments => "malformed_upstream_tool_arguments",
            Self::RequestDeadlineExceeded => "request_deadline_exceeded",
        }
    }
}

impl fmt::Display for ProviderStreamFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A structured provider protocol failure that requires implementation correction.
///
/// Repeating the same request cannot repair these failures. Keeping their wire code
/// typed lets durable attempt records and diagnostics identify the violated contract
/// without parsing the chained source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProtocolFailure {
    UpstreamProtocolError,
    UpstreamInvalidState,
    UnsupportedUpstreamEvent,
    InvalidUpstreamReasoning,
    InvalidUpstreamToolCall,
    IncompleteUpstreamToolCall,
    MissingUpstreamStream,
}

impl ProviderProtocolFailure {
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "upstream_protocol_error" => Some(Self::UpstreamProtocolError),
            "upstream_invalid_state" => Some(Self::UpstreamInvalidState),
            "unsupported_upstream_event" => Some(Self::UnsupportedUpstreamEvent),
            "invalid_upstream_reasoning" => Some(Self::InvalidUpstreamReasoning),
            "invalid_upstream_tool_call" => Some(Self::InvalidUpstreamToolCall),
            "incomplete_upstream_tool_call" => Some(Self::IncompleteUpstreamToolCall),
            "missing_upstream_stream" => Some(Self::MissingUpstreamStream),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpstreamProtocolError => "upstream_protocol_error",
            Self::UpstreamInvalidState => "upstream_invalid_state",
            Self::UnsupportedUpstreamEvent => "unsupported_upstream_event",
            Self::InvalidUpstreamReasoning => "invalid_upstream_reasoning",
            Self::InvalidUpstreamToolCall => "invalid_upstream_tool_call",
            Self::IncompleteUpstreamToolCall => "incomplete_upstream_tool_call",
            Self::MissingUpstreamStream => "missing_upstream_stream",
        }
    }
}

impl fmt::Display for ProviderProtocolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Owned, bounded and redacted provider facts. This is diagnostic data, never a
/// retry classification. It survives cancellation of a replacement request after
/// the original ProviderError (and its transport source) has been dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDiagnostic {
    status: Option<u16>,
    code: Option<String>,
    request_id: Option<String>,
    reason: Option<String>,
}

impl ProviderDiagnostic {
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    #[must_use]
    pub fn fields(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status,
            "code": self.code,
            "requestID": self.request_id,
            "reason": self.reason,
        })
    }
}

impl fmt::Display for ProviderDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(status) = self.status {
            write!(f, "HTTP {status}")?;
        } else {
            f.write_str("provider failure")?;
        }
        if let Some(code) = &self.code {
            write!(f, " code={code}")?;
        }
        if let Some(request_id) = &self.request_id {
            write!(f, " requestID={request_id}")?;
        }
        if let Some(reason) = &self.reason {
            write!(f, ": {reason}")?;
        }
        Ok(())
    }
}

/// A failure from a model provider, classified by what recovery it permits.
///
/// Every variant is a recovery class, not a description. A caller decides what to
/// do by matching the variant and reading its fields — never by inspecting
/// [`std::fmt::Display`] output. See the crate documentation for why that rule
/// exists and what it costs when it is broken.
///
/// Rendered text is for humans and logs. If a recovery decision needs a piece of
/// information, that information is a field.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The prompt exceeded the model's context window.
    ///
    /// Retrying unchanged fails identically; the conversation must be compacted
    /// first. `limit_tokens` and `used_tokens` carry whatever the provider
    /// reported so a compactor can size its work instead of guessing — exactly
    /// the data that is unrecoverable once a failure has been flattened into a
    /// string.
    #[error("context limit exceeded (used={used_tokens:?} limit={limit_tokens:?})")]
    ContextLimit {
        limit_tokens: Option<u64>,
        used_tokens: Option<u64>,
    },

    /// The provider asked the caller to slow down.
    ///
    /// `retry_after` is the delay the provider itself named, propagated from the
    /// wire. A provider that names no delay yields `None` and the caller applies
    /// its own backoff. A message-parsing classifier cannot recover this value at
    /// all, which is why it is the one field this taxonomy mandates.
    #[error("rate limited by provider (retry_after={retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// A transport fault or a server-side error expected to clear on its own: a
    /// dropped connection, a 5xx, an overloaded upstream.
    #[error("transient provider failure (status={status:?})")]
    Transient {
        status: Option<u16>,
        #[source]
        source: Option<BoxSource>,
    },

    /// A structured in-stream failure whose partial output may be discarded and
    /// replaced by a bounded replay of the identical request.
    #[error("retryable provider stream failure `{code}`")]
    Stream {
        code: ProviderStreamFailure,
        #[source]
        source: Option<BoxSource>,
    },

    /// Credentials were missing, expired, or rejected.
    ///
    /// `provider` names whose credentials to refresh, so the recovery path does
    /// not have to guess which of several configured providers failed.
    #[error("authentication rejected by provider {provider}")]
    Auth {
        provider: String,
        #[source]
        source: Option<BoxSource>,
    },

    /// The model declined to answer: a content filter, a safety policy, or a
    /// refusal stop reason.
    ///
    /// `provider_text` is the provider's own wording, carried verbatim for
    /// display. It is payload, never a classification channel — the variant has
    /// already established that this request cannot succeed.
    #[error("provider {provider} refused the request")]
    Refused {
        provider: String,
        provider_text: Option<String>,
    },

    /// The selected model cannot accept a typed input capability present in the request.
    ///
    /// This is a local permanent failure: retrying the same model cannot make an
    /// image-capable request valid, and silently dropping the typed input would change
    /// what the user asked the model to inspect.
    #[error("model `{provider}/{model}` does not support `{capability}` input")]
    UnsupportedCapability {
        provider: String,
        model: String,
        capability: &'static str,
    },

    /// A structured upstream protocol violation that mechanical retry cannot fix.
    #[error("provider protocol failure `{code}`")]
    Protocol {
        code: ProviderProtocolFailure,
        #[source]
        source: Option<BoxSource>,
    },

    /// A failure no retry can fix: a malformed request, an unknown model, a
    /// protocol violation, a revoked account.
    #[error("unrecoverable provider failure (status={status:?})")]
    Fatal {
        status: Option<u16>,
        #[source]
        source: Option<BoxSource>,
    },
}

impl ProviderError {
    /// Bounded diagnostic text, never a recovery classifier.
    /// Causes retain the provider's actual reason instead of only the taxonomy label.
    #[must_use]
    pub fn diagnostic(&self) -> String {
        use std::error::Error as _;
        let mut text = bounded_display(self);
        let mut source = self.source();
        for _ in 0..8 {
            let Some(cause) = source else { break };
            text.push_str(": ");
            text.push_str(&bounded_display(cause));
            source = cause.source();
        }
        Self::sanitize_diagnostic(&text, &[])
    }

    /// Structured wire metadata when the adapter captured it. Missing metadata
    /// stays missing; rendered prose is never parsed to infer a code or request id.
    #[must_use]
    pub fn diagnostic_fields(&self) -> serde_json::Value {
        self.diagnostic_snapshot().fields()
    }

    /// Copy the sanitized metadata captured at the provider boundary without
    /// retaining the original response body or transport source object.
    #[must_use]
    pub fn diagnostic_snapshot(&self) -> ProviderDiagnostic {
        if let Some(wire) = self.http_diagnostic() {
            return ProviderDiagnostic {
                status: Some(wire.status),
                code: wire.code.clone(),
                request_id: wire.request_id.clone(),
                reason: wire.reason.clone(),
            };
        }
        ProviderDiagnostic {
            status: match self {
                Self::Transient { status, .. } | Self::Fatal { status, .. } => *status,
                _ => None,
            },
            code: self.structured_code().map(str::to_owned),
            request_id: None,
            reason: Some(self.diagnostic()),
        }
    }

    fn http_diagnostic(&self) -> Option<&HttpDiagnostic> {
        use std::error::Error as _;
        let mut source = self.source();
        for _ in 0..8 {
            let Some(cause) = source else { break };
            if let Some(wire) = cause.downcast_ref::<HttpDiagnostic>() {
                return Some(wire);
            }
            source = cause.source();
        }
        None
    }

    /// Attach facts read from a bounded HTTP response without changing recovery.
    #[must_use]
    pub fn with_http_diagnostic(
        mut self,
        status: u16,
        code: Option<&str>,
        request_id: Option<&str>,
        reason: Option<&str>,
        credentials: &[&str],
    ) -> Self {
        let clean = |text: &str, max: usize| {
            let text = Self::sanitize_diagnostic(text, credentials);
            text[..text.floor_char_boundary(text.len().min(max))].to_owned()
        };
        let detail = HttpDiagnostic {
            status,
            code: code.map(|text| clean(text, 192)),
            request_id: request_id.map(|text| clean(text, 256)),
            reason: reason.map(|text| clean(text, 3_072)),
        };
        match &mut self {
            Self::Transient { source, .. }
            | Self::Fatal { source, .. }
            | Self::Auth { source, .. } => {
                *source = Some(Box::new(detail));
            }
            Self::Refused { provider_text, .. } => {
                *provider_text = detail.reason;
            }
            _ => {}
        }
        self
    }

    /// Remove adapter-owned secrets before a transport failure crosses its boundary.
    #[must_use]
    pub fn redacted(mut self, credentials: &[&str]) -> Self {
        match &mut self {
            Self::Transient { source, .. }
            | Self::Fatal { source, .. }
            | Self::Auth { source, .. }
            | Self::Stream { source, .. }
            | Self::Protocol { source, .. } => {
                if let Some(cause) = source.take() {
                    if let Some(wire) = cause.downcast_ref::<HttpDiagnostic>() {
                        *source = Some(Box::new(HttpDiagnostic {
                            status: wire.status,
                            code: wire
                                .code
                                .as_deref()
                                .map(|s| Self::sanitize_diagnostic(s, credentials)),
                            request_id: wire
                                .request_id
                                .as_deref()
                                .map(|s| Self::sanitize_diagnostic(s, credentials)),
                            reason: wire
                                .reason
                                .as_deref()
                                .map(|s| Self::sanitize_diagnostic(s, credentials)),
                        }));
                    } else {
                        *source = Some(Box::new(DiagnosticText(Self::sanitize_diagnostic(
                            &bounded_display(cause.as_ref()),
                            credentials,
                        ))));
                    }
                }
            }
            Self::Refused { provider_text, .. } => {
                *provider_text = provider_text
                    .as_deref()
                    .map(|text| Self::sanitize_diagnostic(text, credentials));
            }
            _ => {}
        }
        self
    }

    /// Scrub complete text before clipping, including exact reflected credentials,
    /// JSON credential fields, and common inline authorization assignments.
    #[must_use]
    pub fn sanitize_diagnostic(text: &str, credentials: &[&str]) -> String {
        let mut text = text.to_owned();
        for secret in credentials
            .iter()
            .copied()
            .filter(|secret| !secret.is_empty())
        {
            text = text.replace(secret, "<redacted>");
            let escaped = serde_json::to_string(secret).expect("strings serialize");
            text = text.replace(&escaped[1..escaped.len() - 1], "<redacted>");
            if let Some((scheme, token)) = secret.split_once(' ')
                && matches!(scheme.to_ascii_lowercase().as_str(), "bearer" | "basic")
                && !token.is_empty()
            {
                text = text.replace(token, "<redacted>");
            }
        }
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&text) {
            scrub_json(&mut value);
            text = value.to_string();
        }
        text = scrub_inline(text);
        // Control characters must not inject terminal escapes or forged log lines.
        text = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        text.truncate(text.floor_char_boundary(text.len().min(4_096)));
        text
    }

    /// Classify a wire status code into the taxonomy.
    ///
    /// This is the single place a status code becomes a recovery class, so the
    /// five provider crates cannot drift apart the way five copies of a
    /// `message.contains("503 service unavailable")` check inevitably do.
    ///
    /// It is a floor, not the final word. A provider crate that can read a richer
    /// signal out of the *response body* — a token count, a
    /// `context_length_exceeded` code, a `Retry-After` header — should build the
    /// more specific variant directly. Parsing a response body is reading the
    /// wire; parsing a rendered error message is not, and only the latter is
    /// forbidden.
    #[must_use]
    pub fn from_status(provider: &str, status: u16) -> Self {
        match status {
            401 | 403 => Self::Auth {
                provider: provider.to_owned(),
                source: None,
            },
            429 => Self::RateLimited { retry_after: None },
            408 | 425 | 500..=599 => Self::Transient {
                status: Some(status),
                source: None,
            },
            _ => Self::Fatal {
                status: Some(status),
                source: None,
            },
        }
    }

    /// A transport-level fault expected to clear on its own.
    pub fn transient(source: impl Into<BoxSource>) -> Self {
        Self::Transient {
            status: None,
            source: Some(source.into()),
        }
    }

    /// A fault no retry can fix.
    pub fn fatal(source: impl Into<BoxSource>) -> Self {
        Self::Fatal {
            status: None,
            source: Some(source.into()),
        }
    }

    /// The action this failure calls for.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// True when sending the identical request again may succeed.
    ///
    /// [`ProviderError::ContextLimit`] is **not** retryable: the same request
    /// overflows the same window every time. It is *recoverable*, via
    /// [`Recovery::Compact`], and conflating the two is what makes a retry loop
    /// spin until it exhausts its attempt budget.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        Recoverable::recovery(self).is_retry()
    }

    /// The delay the provider itself asked for, if it named one.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            Self::ContextLimit { .. }
            | Self::Transient { .. }
            | Self::Stream { .. }
            | Self::Auth { .. }
            | Self::Refused { .. }
            | Self::UnsupportedCapability { .. }
            | Self::Protocol { .. }
            | Self::Fatal { .. } => None,
        }
    }

    /// The exact structured provider code, when the wire contract supplied one.
    #[must_use]
    pub fn structured_code(&self) -> Option<&str> {
        if let Some(code) = self.http_diagnostic().and_then(|wire| wire.code.as_deref()) {
            return Some(code);
        }
        match self {
            Self::Stream { code, .. } => Some(code.as_str()),
            Self::Protocol { code, .. } => Some(code.as_str()),
            Self::ContextLimit { .. }
            | Self::RateLimited { .. }
            | Self::Transient { .. }
            | Self::Auth { .. }
            | Self::Refused { .. }
            | Self::UnsupportedCapability { .. }
            | Self::Fatal { .. } => None,
        }
    }

    /// Whether partial output from this failed request may be discarded before
    /// replaying the identical request as a replacement attempt.
    #[must_use]
    pub const fn permits_partial_output_retry(&self) -> bool {
        matches!(self, Self::Stream { .. })
    }
}

#[derive(Debug)]
struct HttpDiagnostic {
    status: u16,
    code: Option<String>,
    request_id: Option<String>,
    reason: Option<String>,
}

impl fmt::Display for HttpDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {}", self.status)?;
        if let Some(code) = &self.code {
            write!(f, " code={code}")?;
        }
        if let Some(id) = &self.request_id {
            write!(f, " requestID={id}")?;
        }
        if let Some(reason) = &self.reason {
            write!(f, " reason={reason}")?;
        }
        Ok(())
    }
}
impl std::error::Error for HttpDiagnostic {}

#[derive(Debug)]
struct DiagnosticText(String);
impl fmt::Display for DiagnosticText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for DiagnosticText {}

fn bounded_display(value: &(impl fmt::Display + ?Sized)) -> String {
    struct Bounded(String);
    impl fmt::Write for Bounded {
        fn write_str(&mut self, value: &str) -> fmt::Result {
            if self.0.len().saturating_add(value.len()) > 16_384 {
                return Err(fmt::Error);
            }
            self.0.push_str(value);
            Ok(())
        }
    }
    let mut output = Bounded(String::new());
    if fmt::write(&mut output, format_args!("{value}")).is_err() {
        return "[provider diagnostic exceeded its source limit]".to_owned();
    }
    output.0
}

fn sensitive_diagnostic_key(key: &str) -> bool {
    let key = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(
        key.as_str(),
        "authorization"
            | "proxyauthorization"
            | "apikey"
            | "xapikey"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "password"
            | "secret"
            | "clientsecret"
            | "credential"
            | "cookie"
            | "setcookie"
    )
}

fn scrub_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if sensitive_diagnostic_key(key) {
                    *value = serde_json::json!("<redacted>");
                } else {
                    scrub_json(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                scrub_json(value);
            }
        }
        serde_json::Value::String(text) => *text = scrub_inline(std::mem::take(text)),
        _ => {}
    }
}

fn scrub_inline(mut text: String) -> String {
    for marker in [
        "bearer ",
        "basic ",
        "authorization",
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "refresh_token",
        "password",
        "client_secret",
        "credential",
        "token",
    ] {
        let mut cursor = 0;
        loop {
            let lower = text.to_ascii_lowercase();
            let Some(relative) = lower[cursor..].find(marker) else {
                break;
            };
            let offset = cursor + relative;
            let after = offset + marker.len();
            cursor = after;
            if offset > 0 && text.as_bytes()[offset - 1].is_ascii_alphanumeric() {
                continue;
            }
            let mut start = after;
            if !marker.ends_with(' ') {
                while matches!(
                    text.as_bytes().get(start),
                    Some(b' ' | b'\t' | b'"' | b'\'')
                ) {
                    start += 1;
                }
                if !matches!(text.as_bytes().get(start), Some(b'=' | b':')) {
                    continue;
                }
                start += 1;
                while matches!(text.as_bytes().get(start), Some(b' ' | b'\t')) {
                    start += 1;
                }
            }
            let quote = match text.as_bytes().get(start) {
                Some(b'"' | b'\'') => {
                    let quote = text.as_bytes()[start];
                    start += 1;
                    Some(quote)
                }
                _ => None,
            };
            let end = text[start..]
                .char_indices()
                .find_map(|(i, c)| {
                    let end = quote.map_or_else(
                        || c.is_whitespace() || matches!(c, ',' | ';' | '}' | ']' | '"' | '\''),
                        |quote| c == char::from(quote),
                    );
                    end.then_some(start + i)
                })
                .unwrap_or(text.len());
            if end > start {
                text.replace_range(start..end, "<redacted>");
                cursor = start + "<redacted>".len();
            }
        }
    }
    text
}

impl Recoverable for ProviderError {
    /// Each variant is listed explicitly rather than collapsed into a `_ =>` arm.
    /// That is deliberate: adding a variant must break this function so its
    /// author has to decide what the new failure means, instead of silently
    /// inheriting a wildcard's answer.
    fn recovery(&self) -> Recovery {
        match self {
            Self::ContextLimit { .. } => Recovery::Compact,
            Self::RateLimited { retry_after } => Recovery::Retry {
                after: *retry_after,
            },
            Self::Transient { .. } | Self::Stream { .. } => Recovery::Retry { after: None },
            Self::Auth { .. } => Recovery::Reauthenticate,
            Self::Refused { .. }
            | Self::UnsupportedCapability { .. }
            | Self::Protocol { .. }
            | Self::Fatal { .. } => Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_http_diagnostic_retains_dynamic_metadata_and_recovery_class() {
        let snapshot = {
            let code = String::from("reasoning_replay_account_unavailable");
            let error = ProviderError::from_status("test", 503).with_http_diagnostic(
                503,
                Some(&code),
                Some("upstream-request-7"),
                Some("Temporarily unavailable"),
                &[],
            );
            assert!(error.is_retryable());
            assert_eq!(error.structured_code(), Some(code.as_str()));
            error.diagnostic_snapshot()
        };
        assert_eq!(snapshot.status(), Some(503));
        assert_eq!(
            snapshot.code(),
            Some("reasoning_replay_account_unavailable")
        );
        assert_eq!(snapshot.fields()["requestID"], "upstream-request-7");
        assert!(snapshot.to_string().contains("HTTP 503"));
        assert!(snapshot.to_string().contains("Temporarily unavailable"));
    }

    #[test]
    fn owned_diagnostic_stays_bounded_and_redacted_after_source_drop() {
        let secret = "private-fixture-key";
        let large = format!("{secret}\n\u{1b}[31m{}", "中".repeat(3000));
        let snapshot = ProviderError::from_status("test", 503)
            .with_http_diagnostic(503, Some(&large), Some(&large), Some(&large), &[secret])
            .diagnostic_snapshot();
        let fields = snapshot.fields();
        assert!(fields["code"].as_str().unwrap().len() <= 192);
        assert!(fields["requestID"].as_str().unwrap().len() <= 256);
        assert!(fields["reason"].as_str().unwrap().len() <= 3072);
        assert!(!fields.to_string().contains(secret));
        assert!(!snapshot.to_string().chars().any(char::is_control));
    }

    #[test]
    fn rate_limited_returns_the_duration_the_provider_sent() {
        let e = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(e.retry_after(), Some(Duration::from_secs(30)));
        assert!(e.is_retryable());
        assert_eq!(
            e.recovery(),
            Recovery::Retry {
                after: Some(Duration::from_secs(30))
            }
        );
    }

    #[test]
    fn rate_limited_without_a_named_delay_is_still_retryable() {
        let e = ProviderError::RateLimited { retry_after: None };
        assert_eq!(e.retry_after(), None);
        assert!(e.is_retryable());
    }

    #[test]
    fn context_limit_asks_for_compaction_not_retry() {
        let e = ProviderError::ContextLimit {
            limit_tokens: Some(200_000),
            used_tokens: Some(214_311),
        };
        assert_eq!(e.recovery(), Recovery::Compact);
        assert!(!e.is_retryable());
        assert_eq!(e.retry_after(), None);
    }

    #[test]
    fn context_limit_carries_the_numbers_a_compactor_needs() {
        let e = ProviderError::ContextLimit {
            limit_tokens: Some(200_000),
            used_tokens: Some(214_311),
        };
        let ProviderError::ContextLimit {
            limit_tokens,
            used_tokens,
        } = e
        else {
            panic!("constructed a ContextLimit, matched something else");
        };
        assert_eq!(limit_tokens, Some(200_000));
        assert_eq!(used_tokens, Some(214_311));
    }

    #[test]
    fn transient_is_retryable_with_caller_chosen_backoff() {
        let e = ProviderError::Transient {
            status: Some(503),
            source: None,
        };
        assert!(e.is_retryable());
        assert_eq!(e.retry_after(), None);
    }

    #[test]
    fn structured_stream_failures_round_trip_and_allow_replacement_retry() {
        let cases = [
            (
                "upstream_stream_error",
                ProviderStreamFailure::UpstreamStreamError,
            ),
            (
                "upstream_stream_incomplete",
                ProviderStreamFailure::UpstreamStreamIncomplete,
            ),
            (
                "upstream_stream_idle_timeout",
                ProviderStreamFailure::UpstreamStreamIdleTimeout,
            ),
            (
                "malformed_upstream_tool_arguments",
                ProviderStreamFailure::MalformedUpstreamToolArguments,
            ),
            (
                "request_deadline_exceeded",
                ProviderStreamFailure::RequestDeadlineExceeded,
            ),
        ];

        for (wire_code, code) in cases {
            assert_eq!(ProviderStreamFailure::from_code(wire_code), Some(code));
            assert_eq!(code.as_str(), wire_code);

            let error = ProviderError::Stream { code, source: None };
            assert_eq!(error.recovery(), Recovery::Retry { after: None });
            assert_eq!(error.structured_code(), Some(wire_code));
            assert!(error.permits_partial_output_retry());
        }

        assert_eq!(ProviderStreamFailure::from_code("upstream_error"), None);
    }

    #[test]
    fn structured_protocol_failures_are_terminal() {
        let error = ProviderError::Protocol {
            code: ProviderProtocolFailure::InvalidUpstreamToolCall,
            source: None,
        };
        assert_eq!(error.recovery(), Recovery::Fail);
        assert_eq!(error.structured_code(), Some("invalid_upstream_tool_call"));
        assert!(!error.permits_partial_output_retry());
    }

    #[test]
    fn auth_asks_for_reauthentication_and_names_the_provider() {
        let e = ProviderError::Auth {
            provider: "anthropic".to_owned(),
            source: None,
        };
        assert_eq!(e.recovery(), Recovery::Reauthenticate);
        assert!(!e.is_retryable());
        assert_eq!(
            e.to_string(),
            "authentication rejected by provider anthropic"
        );
    }

    #[test]
    fn refused_and_fatal_are_terminal() {
        let refused = ProviderError::Refused {
            provider: "openai".to_owned(),
            provider_text: Some("I can't help with that".to_owned()),
        };
        let fatal = ProviderError::Fatal {
            status: Some(400),
            source: None,
        };
        assert_eq!(refused.recovery(), Recovery::Fail);
        assert_eq!(fatal.recovery(), Recovery::Fail);
        assert!(!refused.is_retryable());
        assert!(!fatal.is_retryable());
    }

    #[test]
    fn unsupported_capability_is_explicit_and_terminal() {
        let error = ProviderError::UnsupportedCapability {
            provider: "custom".to_owned(),
            model: "text-only".to_owned(),
            capability: "attachments",
        };
        assert_eq!(
            error.to_string(),
            "model `custom/text-only` does not support `attachments` input"
        );
        assert_eq!(error.recovery(), Recovery::Fail);
        assert!(!error.is_retryable());
    }

    #[test]
    fn status_classification_covers_the_codes_message_matching_used_to_chase() {
        let cases: &[(u16, Recovery)] = &[
            (401, Recovery::Reauthenticate),
            (403, Recovery::Reauthenticate),
            (408, Recovery::Retry { after: None }),
            (425, Recovery::Retry { after: None }),
            (429, Recovery::Retry { after: None }),
            (500, Recovery::Retry { after: None }),
            (502, Recovery::Retry { after: None }),
            (503, Recovery::Retry { after: None }),
            (504, Recovery::Retry { after: None }),
            (529, Recovery::Retry { after: None }),
            (400, Recovery::Fail),
            (404, Recovery::Fail),
            (422, Recovery::Fail),
        ];
        for &(status, expected) in cases {
            let actual = ProviderError::from_status("anthropic", status).recovery();
            assert_eq!(actual, expected, "status {status} classified wrongly");
        }
    }

    #[test]
    fn status_429_is_rate_limited_rather_than_merely_transient() {
        assert!(matches!(
            ProviderError::from_status("anthropic", 429),
            ProviderError::RateLimited { retry_after: None }
        ));
    }

    #[test]
    fn constructors_chain_the_underlying_cause() {
        use std::error::Error as _;

        let transient = ProviderError::transient(std::io::Error::other("connection reset"));
        assert_eq!(
            transient.source().map(ToString::to_string).as_deref(),
            Some("connection reset")
        );
        assert!(transient.is_retryable());

        let fatal = ProviderError::fatal(std::io::Error::other("unknown model"));
        assert_eq!(
            fatal.source().map(ToString::to_string).as_deref(),
            Some("unknown model")
        );
        assert!(!fatal.is_retryable());
    }

    #[test]
    fn inherent_and_trait_recovery_agree_on_every_variant() {
        let errors = [
            ProviderError::ContextLimit {
                limit_tokens: None,
                used_tokens: None,
            },
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            },
            ProviderError::Transient {
                status: None,
                source: None,
            },
            ProviderError::Stream {
                code: ProviderStreamFailure::UpstreamStreamError,
                source: None,
            },
            ProviderError::Auth {
                provider: "google".to_owned(),
                source: None,
            },
            ProviderError::Refused {
                provider: "google".to_owned(),
                provider_text: None,
            },
            ProviderError::UnsupportedCapability {
                provider: "google".to_owned(),
                model: "text-only".to_owned(),
                capability: "attachments",
            },
            ProviderError::Protocol {
                code: ProviderProtocolFailure::UpstreamProtocolError,
                source: None,
            },
            ProviderError::Fatal {
                status: None,
                source: None,
            },
        ];
        for e in &errors {
            assert_eq!(e.recovery(), Recoverable::recovery(e), "{e}");
            assert_eq!(e.is_retryable(), Recoverable::is_retryable(e), "{e}");
            assert_eq!(e.retry_after(), Recoverable::retry_after(e), "{e}");
        }
    }
}
