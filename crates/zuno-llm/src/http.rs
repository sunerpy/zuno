//! Shared HTTP request policy for every provider family.
//!
//! # Why this lives here
//!
//! Five provider crates speak HTTP to a model endpoint, and before this module
//! each one re-derived the same three policies independently: how long to wait
//! for response headers, how much of an error body to read, and how to read a
//! peer `Retry-After`. The results diverged — two providers had no idle bound at
//! all, none bounded the wait for headers, and the five `Retry-After` parsers
//! disagreed about the HTTP-date form. Divergence in a retry input is not a
//! cosmetic problem: it decides whether a turn waits the interval the peer asked
//! for or hammers a rate limiter.
//!
//! The policy belongs here rather than in `zuno-network` because
//! [`zuno_network::client_builder`] is shared with roughly twenty non-provider
//! callers — telemetry, updates, MCP, and loopback probes. Changing its defaults
//! would silently retime all of them. This module instead applies deadlines at
//! the provider's own call sites, so the blast radius is exactly the providers.
//!
//! # What it does not do
//!
//! It holds no idle-timeout *value*. Native providers keep the three-hundred
//! second [`crate::sse::StreamIdleTimeout`] ceiling that tolerates long reasoning pauses,
//! while the OpenAI-compatible transport keeps its own tighter liveness policy.
//! Only the mechanism is shared.

use std::fmt;
use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Deserialize;
use serde_json::Value;
use zuno_error::ProviderError;

use crate::sse::STREAM_IDLE_TIMEOUT_ENV;

/// Default maximum wait for a provider's response headers.
///
/// A streaming endpoint normally sends headers within a second, but a
/// non-streaming or heavily queued endpoint can withhold them until generation
/// finishes. The bound therefore has to exceed a long generation while still
/// terminating: without it, a peer that accepts the connection and then goes
/// silent holds the turn open until the process is killed.
pub const DEFAULT_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(330);

/// Largest response body read for classification or token exchange.
///
/// Real vendor error bodies are a few hundred bytes. The cap exists so a peer
/// that streams an unbounded body cannot exhaust memory while the caller is
/// merely trying to learn why a request failed.
pub const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;

/// Maximum time spent reading a bounded response body.
pub const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Absolute ceiling applied to a peer-supplied `Retry-After`.
///
/// This is a sanity bound, not the retry policy. The configured ceiling is
/// applied later by `zuno_goal::retry::Backoff::delay`, which clamps the value
/// this module returns; the bound here only keeps an absurd or hostile header
/// from overflowing [`Duration::from_secs_f64`], which panics rather than
/// saturating.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(86_400);

// ---------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------

/// Per-request HTTP timeouts resolved from provider configuration.
///
/// A whole-request timeout spans response headers and every streamed body chunk.
/// Header and chunk timeouts are phase-specific; the earliest applicable
/// deadline wins without changing the other phase's policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HttpTimeouts {
    total: Option<Duration>,
    header: Option<Duration>,
    chunk: Option<Duration>,
}

impl HttpTimeouts {
    /// Construct an explicit set of timeouts.
    #[must_use]
    pub const fn new(
        total: Option<Duration>,
        header: Option<Duration>,
        chunk: Option<Duration>,
    ) -> Self {
        Self {
            total,
            header,
            chunk,
        }
    }

    /// The policy used by the first-party native providers.
    ///
    /// Only the response-header phase is bounded here. The chunk phase stays with
    /// [`crate::sse::StreamIdleTimeout`] so the native three-hundred second allowance and its
    /// `ZUNO_STREAM_IDLE_TIMEOUT_SECS` override keep working unchanged, and no
    /// whole-request deadline is imposed because a legitimate long turn has no
    /// upper bound the provider can know.
    #[must_use]
    pub const fn native() -> Self {
        Self::new(None, Some(DEFAULT_RESPONSE_HEADER_TIMEOUT), None)
    }

    /// The whole-request deadline.
    #[must_use]
    pub const fn total(self) -> Option<Duration> {
        self.total
    }

    /// The response-header deadline.
    #[must_use]
    pub const fn header(self) -> Option<Duration> {
        self.header
    }

    /// The per-chunk idle allowance.
    #[must_use]
    pub const fn chunk(self) -> Option<Duration> {
        self.chunk
    }
}

/// The phase a timeout expired in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    /// The whole request, spanning headers and every body chunk.
    WholeRequest,
    /// Waiting for the response status line and headers.
    ResponseHeaders,
    /// Waiting for the next chunk of a response body.
    ResponseChunk,
    /// Reading a complete response body that is not being streamed to a decoder.
    ResponseBody,
}

impl TimeoutPhase {
    /// Wording used in the error chain a user sees.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::WholeRequest => "whole request timeout",
            Self::ResponseHeaders => "response headers timeout",
            Self::ResponseChunk => "response stream idle timeout",
            Self::ResponseBody => "response body read timeout",
        }
    }
}

/// One phase deadline.
#[derive(Debug, Clone, Copy)]
struct Deadline {
    at: tokio::time::Instant,
    duration: Duration,
    phase: TimeoutPhase,
}

impl Deadline {
    fn after(started: tokio::time::Instant, duration: Duration, phase: TimeoutPhase) -> Self {
        Self {
            at: started + duration,
            duration,
            phase,
        }
    }
}

fn earliest(left: Option<Deadline>, right: Option<Deadline>) -> Option<Deadline> {
    match (left, right) {
        (Some(left), Some(right)) if left.at <= right.at => Some(left),
        (Some(_), Some(right)) => Some(right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

async fn wait_until<F, T>(
    provider: &str,
    deadline: Option<Deadline>,
    future: F,
) -> Result<T, ProviderError>
where
    F: Future<Output = T>,
{
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.at, future)
            .await
            .map_err(|_| {
                ProviderError::transient(HttpTimeoutError {
                    provider: provider.to_owned(),
                    duration: deadline.duration,
                    phase: deadline.phase,
                })
            }),
        None => Ok(future.await),
    }
}

/// Deadline tracking for one outbound provider request.
///
/// Start this immediately before building the request, then route each awaited
/// phase through the matching method. A whole-request deadline stays anchored to
/// the start instant; the per-chunk allowance restarts on every chunk, so a slow
/// but progressing stream is never cut off.
#[derive(Debug, Clone, Copy)]
pub struct RequestDeadlines {
    total: Option<Deadline>,
    header: Option<Deadline>,
    chunk: Option<Duration>,
}

impl RequestDeadlines {
    /// Begin tracking `timeouts` from now.
    #[must_use]
    pub fn start(timeouts: HttpTimeouts) -> Self {
        let started = tokio::time::Instant::now();
        Self {
            total: timeouts
                .total()
                .map(|duration| Deadline::after(started, duration, TimeoutPhase::WholeRequest)),
            header: timeouts
                .header()
                .map(|duration| Deadline::after(started, duration, TimeoutPhase::ResponseHeaders)),
            chunk: timeouts.chunk(),
        }
    }

    /// Await the response status line and headers.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Transient`] when neither the header nor the whole-request
    /// deadline is met. The source names the provider, phase, and duration.
    pub async fn headers<F, T>(self, provider: &str, future: F) -> Result<T, ProviderError>
    where
        F: Future<Output = T>,
    {
        wait_until(provider, earliest(self.total, self.header), future).await
    }

    /// Await a whole-body read.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Transient`] when the whole-request deadline expires first.
    pub async fn body<F, T>(self, provider: &str, future: F) -> Result<T, ProviderError>
    where
        F: Future<Output = T>,
    {
        wait_until(provider, self.total, future).await
    }

    /// Await the next chunk of a response body.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Transient`] when the per-chunk allowance or the
    /// whole-request deadline expires first.
    pub async fn chunk<F, T>(self, provider: &str, future: F) -> Result<T, ProviderError>
    where
        F: Future<Output = T>,
    {
        let chunk = self.chunk.map(|duration| {
            Deadline::after(
                tokio::time::Instant::now(),
                duration,
                TimeoutPhase::ResponseChunk,
            )
        });
        wait_until(provider, earliest(self.total, chunk), future).await
    }
}

/// A provider request that exceeded one of its phase deadlines.
#[derive(Debug)]
pub struct HttpTimeoutError {
    provider: String,
    duration: Duration,
    phase: TimeoutPhase,
}

impl HttpTimeoutError {
    /// The phase that expired.
    #[must_use]
    pub const fn phase(&self) -> TimeoutPhase {
        self.phase
    }

    /// The allowance that expired.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
    }
}

impl fmt::Display for HttpTimeoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider `{}` {} after {:?}",
            self.provider,
            self.phase.description(),
            self.duration
        )?;
        if matches!(self.phase, TimeoutPhase::ResponseChunk) {
            write!(
                formatter,
                "; raise provider options.chunkTimeout or {STREAM_IDLE_TIMEOUT_ENV} for slower providers"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpTimeoutError {}

// ---------------------------------------------------------------------------
// Bounded body reads
// ---------------------------------------------------------------------------

/// A response body read under a byte cap and a wall-clock cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedBody {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedBody {
    /// The bytes that arrived.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether the byte cap stopped the read before the body ended.
    ///
    /// A truncated body may still classify correctly from its status, but it must
    /// never be parsed as if it were the whole document.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Take the bytes that arrived.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Read a response body under an explicit byte cap and wall-clock cap.
///
/// # Errors
///
/// [`ProviderError::Transient`] when the body stream fails or falls silent past
/// `timeout`. A read that stops at `limit` succeeds with
/// [`BoundedBody::truncated`] set, because the status of the response is often
/// still worth classifying.
pub async fn read_bounded_body(
    provider: &str,
    response: reqwest::Response,
    limit: usize,
    timeout: Duration,
) -> Result<BoundedBody, ProviderError> {
    let owner = provider.to_owned();
    let provider = provider.to_owned();
    let read = async move {
        let mut body = response.bytes_stream();
        let mut bytes = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|source| {
                ProviderError::transient(BodyReadError {
                    provider: provider.clone(),
                    source: Some(Box::new(source)),
                })
            })?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                let room = limit.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(BoundedBody { bytes, truncated })
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(result) => result,
        Err(_) => Err(ProviderError::transient(HttpTimeoutError {
            provider: owner,
            duration: timeout,
            phase: TimeoutPhase::ResponseBody,
        })),
    }
}

/// Read a non-2xx response body for classification, using the shared caps.
///
/// # Errors
///
/// [`ProviderError::Transient`] when the error body cannot be read. Retrying is
/// the right answer: a body that arrived incomplete cannot be trusted to say
/// whether the failure was permanent.
pub async fn read_error_body(
    provider: &str,
    response: reqwest::Response,
) -> Result<BoundedBody, ProviderError> {
    read_bounded_body(provider, response, MAX_ERROR_BODY_BYTES, BODY_READ_TIMEOUT).await
}

/// A response body that could not be read to its end.
#[derive(Debug)]
pub struct BodyReadError {
    provider: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl fmt::Display for BodyReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider `{}` response body read failed",
            self.provider
        )
    }
}

impl std::error::Error for BodyReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// The typed failure for a response body the byte cap cut short.
///
/// # Errors
///
/// Always. This exists so a caller that needs the whole document — a token
/// exchange, not a diagnostic — can convert truncation into a typed error
/// instead of parsing a partial JSON value.
pub fn truncated_body_error(provider: &str, limit: usize) -> ProviderError {
    ProviderError::transient(TruncatedBodyError {
        provider: provider.to_owned(),
        limit,
    })
}

/// A response body that exceeded the shared byte cap.
#[derive(Debug)]
struct TruncatedBodyError {
    provider: String,
    limit: usize,
}

impl fmt::Display for TruncatedBodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider `{}` response body exceeded {} bytes",
            self.provider, self.limit
        )
    }
}

impl std::error::Error for TruncatedBodyError {}

// ---------------------------------------------------------------------------
// Retry-After
// ---------------------------------------------------------------------------

/// Read a `Retry-After` response header.
///
/// Absent, unreadable, malformed, and already-elapsed values all yield `None`,
/// which means "the peer named no interval" and leaves the caller's own backoff
/// in charge.
#[must_use]
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    parse_retry_after(headers.get(RETRY_AFTER)?.to_str().ok()?)
}

/// Parse a `Retry-After` value against the current clock.
///
/// Both RFC 9110 forms are accepted: delta-seconds, and an IMF-fixdate. Vendors
/// also send fractional seconds, which the specification does not define but
/// which several OpenAI-compatible services emit, so those are accepted as well.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    parse_retry_after_at(value, SystemTime::now())
}

/// Parse a `Retry-After` value against an explicit clock.
///
/// The clock is a parameter so the HTTP-date path is testable without waiting
/// for a real date to pass.
#[must_use]
pub fn parse_retry_after_at(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(delta) = parse_delta_seconds(value) {
        return Some(delta);
    }
    let deadline = parse_http_date(value)?;
    let now = i64::try_from(now.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
    let remaining = deadline.checked_sub(now)?;
    if remaining <= 0 {
        // A date that has already passed asks for no delay at all. Returning
        // `None` hands the decision to local backoff rather than inventing a
        // zero-length wait that would retry instantly.
        return None;
    }
    Some(clamp_retry_after(Duration::from_secs(
        u64::try_from(remaining).ok()?,
    )))
}

fn parse_delta_seconds(value: &str) -> Option<Duration> {
    let seconds = value.parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds.is_sign_negative() {
        return None;
    }
    // Clamp before conversion: `Duration::from_secs_f64` panics on overflow, so a
    // header of `1e30` would otherwise abort the provider task.
    Some(Duration::from_secs_f64(
        seconds.min(MAX_RETRY_AFTER.as_secs_f64()),
    ))
}

fn clamp_retry_after(value: Duration) -> Duration {
    value.min(MAX_RETRY_AFTER)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Parse an IMF-fixdate into Unix seconds.
///
/// Only the preferred form (`Sun, 06 Nov 1994 08:49:37 GMT`) is accepted. The two
/// obsolete formats RFC 9110 still tolerates are rejected: no shipped provider
/// emits them, and guessing at an ambiguous two-digit year is worse than falling
/// back to local backoff.
fn parse_http_date(value: &str) -> Option<i64> {
    let (weekday, rest) = value.split_once(", ")?;
    if weekday.len() != 3 || !weekday.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    let mut fields = rest.split(' ');
    let day = fields.next()?;
    let month = fields.next()?;
    let year = fields.next()?;
    let time = fields.next()?;
    let zone = fields.next()?;
    if fields.next().is_some() || !zone.eq_ignore_ascii_case("GMT") {
        return None;
    }

    let day = number(day, 1, 2)?;
    let month = MONTHS.iter().position(|name| *name == month)? + 1;
    let year = number(year, 4, 4)?;
    let (hour, minute, second) = {
        let mut parts = time.split(':');
        let hour = number(parts.next()?, 2, 2)?;
        let minute = number(parts.next()?, 2, 2)?;
        let second = number(parts.next()?, 2, 2)?;
        if parts.next().is_some() {
            return None;
        }
        (hour, minute, second)
    };

    let month = u32::try_from(month).ok()?;
    if day < 1 || day > days_in_month(year, month) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let days = days_from_civil(i64::from(year), month, day);
    Some(
        days * 86_400
            + i64::from(hour) * 3_600
            + i64::from(minute) * 60
            + i64::from(second.min(59)),
    )
}

fn number(value: &str, min_digits: usize, max_digits: usize) -> Option<u32> {
    if value.len() < min_digits
        || value.len() > max_digits
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value.parse().ok()
}

const fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days between 1970-01-01 and `year-month-day`, proleptic Gregorian.
///
/// Howard Hinnant's `days_from_civil`, which is exact for the whole range a
/// four-digit year can express and needs no calendar tables.
const fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month as i64 + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ---------------------------------------------------------------------------
// Anthropic Messages error classification
// ---------------------------------------------------------------------------

/// The structured `error` object of the Anthropic Messages wire format.
///
/// Two provider crates speak this format: first-party Anthropic, and Anthropic
/// models published through Vertex AI. Both must classify it identically, so the
/// shape and the classifier live here rather than in either crate. This is a wire
/// format, not a provider dependency.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MessagesErrorBody {
    /// The machine-readable error category. This is the only field consulted.
    #[serde(rename = "type")]
    pub kind: String,
    /// The vendor's human-readable detail. Display payload only; never matched.
    #[serde(default)]
    pub message: Option<String>,
    /// Context window size, when the endpoint supplies it.
    #[serde(default)]
    pub limit_tokens: Option<u64>,
    /// Submitted token count, when the endpoint supplies it.
    #[serde(default)]
    pub used_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct MessagesErrorEnvelope {
    error: MessagesErrorBody,
    #[serde(default)]
    request_id: Option<String>,
}

/// Classify a non-success Anthropic Messages response.
///
/// The structured `type` decides first, then the HTTP status. The rendered
/// `message` is attached as a source for the human and never participates in the
/// match, so vendor prose cannot move a request between recovery classes.
#[must_use]
pub fn map_messages_http_error(
    provider: &str,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
) -> ProviderError {
    let envelope = serde_json::from_slice::<MessagesErrorEnvelope>(body).ok();
    map_messages_error(provider, Some(status), retry_after(headers), envelope)
}

/// Classify an in-stream Anthropic Messages `error` event from its typed body.
#[must_use]
pub fn map_messages_stream_error(provider: &str, error: MessagesErrorBody) -> ProviderError {
    map_messages_error(
        provider,
        None,
        None,
        Some(MessagesErrorEnvelope {
            error,
            request_id: None,
        }),
    )
}

/// Classify an in-stream Anthropic Messages `error` event from its raw JSON.
///
/// For decoders that hold the event as a [`Value`]. An event whose `error` object
/// is missing or misshaped classifies from status-free defaults exactly as a
/// malformed body would.
#[must_use]
pub fn map_messages_stream_error_value(provider: &str, event: &Value) -> ProviderError {
    let error = serde_json::from_value::<MessagesErrorBody>(event["error"].clone()).ok();
    map_messages_error(
        provider,
        None,
        None,
        error.map(|error| MessagesErrorEnvelope {
            error,
            request_id: None,
        }),
    )
}

fn map_messages_error(
    provider: &str,
    status: Option<u16>,
    retry_after: Option<Duration>,
    envelope: Option<MessagesErrorEnvelope>,
) -> ProviderError {
    let kind = envelope.as_ref().map(|value| value.error.kind.as_str());

    match kind {
        Some("context_length_exceeded" | "context_window_exceeded" | "prompt_too_long") => {
            let error = &envelope.as_ref().expect("matched envelope").error;
            ProviderError::ContextLimit {
                limit_tokens: error.limit_tokens,
                used_tokens: error.used_tokens,
            }
        }
        Some("rate_limit_error") => ProviderError::RateLimited { retry_after },
        Some("authentication_error" | "permission_error") => ProviderError::Auth {
            provider: provider.to_owned(),
            source: envelope.map(messages_source),
        },
        Some("overloaded_error" | "api_error") => ProviderError::Transient {
            // Anthropic returns HTTP 529 for `overloaded_error`. In an SSE event
            // there is no status to carry, so the stream path names it explicitly
            // and keeps the class retryable either way.
            status: status.or(match kind {
                Some("overloaded_error") => Some(529),
                _ => None,
            }),
            source: envelope.map(messages_source),
        },
        Some("refusal_error") => ProviderError::Refused {
            provider: provider.to_owned(),
            provider_text: envelope.and_then(|value| value.error.message),
        },
        _ => match status {
            Some(429) => ProviderError::RateLimited { retry_after },
            Some(401 | 403) => ProviderError::Auth {
                provider: provider.to_owned(),
                source: envelope.map(messages_source),
            },
            Some(code @ (408 | 425 | 500..=599)) => ProviderError::Transient {
                status: Some(code),
                source: envelope.map(messages_source),
            },
            Some(code) => ProviderError::Fatal {
                status: Some(code),
                source: envelope.map(messages_source),
            },
            None => ProviderError::Fatal {
                status: None,
                source: envelope.map(messages_source),
            },
        },
    }
}

fn messages_source(envelope: MessagesErrorEnvelope) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(MessagesApiError {
        kind: envelope.error.kind,
        message: envelope.error.message,
        request_id: envelope.request_id,
    })
}

#[derive(Debug)]
struct MessagesApiError {
    kind: String,
    message: Option<String>,
    request_id: Option<String>,
}

impl fmt::Display for MessagesApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Anthropic error type `{}`", self.kind)?;
        if let Some(request_id) = &self.request_id {
            write!(formatter, " request `{request_id}`")?;
        }
        if let Some(message) = &self.message {
            write!(formatter, ": {message}")?;
        }
        Ok(())
    }
}

impl std::error::Error for MessagesApiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    /// `Sun, 06 Nov 1994 08:49:37 GMT`, the RFC 9110 example, in Unix seconds.
    const RFC_EXAMPLE_EPOCH: i64 = 784_111_777;

    fn at(epoch: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(u64::try_from(epoch).expect("positive epoch"))
    }

    #[test]
    fn an_imf_fixdate_parses_to_the_documented_instant() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(RFC_EXAMPLE_EPOCH)
        );
    }

    #[test]
    fn retry_after_reads_delta_seconds() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("  7  "), Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_reads_the_fractional_seconds_vendors_actually_send() {
        assert_eq!(parse_retry_after("2.5"), Some(Duration::from_millis(2_500)));
    }

    #[test]
    fn retry_after_reads_the_http_date_form() {
        assert_eq!(
            parse_retry_after_at("Sun, 06 Nov 1994 08:49:37 GMT", at(RFC_EXAMPLE_EPOCH - 90)),
            Some(Duration::from_secs(90)),
            "the HTTP-date form is not optional; every provider parser must honour it"
        );
    }

    #[test]
    fn retry_after_ignores_a_date_that_has_already_passed() {
        assert_eq!(
            parse_retry_after_at("Sun, 06 Nov 1994 08:49:37 GMT", at(RFC_EXAMPLE_EPOCH + 1)),
            None
        );
        assert_eq!(
            parse_retry_after_at("Sun, 06 Nov 1994 08:49:37 GMT", at(RFC_EXAMPLE_EPOCH)),
            None
        );
    }

    #[test]
    fn retry_after_rejects_malformed_values() {
        for value in [
            "",
            "   ",
            "soon",
            "5 seconds",
            "-30",
            "NaN",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 30 Feb 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 PST",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(
                parse_retry_after_at(value, at(RFC_EXAMPLE_EPOCH)),
                None,
                "`{value}` must not become a retry delay"
            );
        }
    }

    #[test]
    fn retry_after_clamps_a_value_above_the_ceiling() {
        assert_eq!(parse_retry_after("999999999"), Some(MAX_RETRY_AFTER));
        assert_eq!(
            parse_retry_after_at("Fri, 01 Jan 2100 00:00:00 GMT", at(RFC_EXAMPLE_EPOCH)),
            Some(MAX_RETRY_AFTER)
        );
    }

    #[test]
    fn an_absurd_delta_cannot_panic_the_provider_task() {
        // `Duration::from_secs_f64` panics on overflow. Before the shared parser,
        // two providers fed this header straight into it.
        assert_eq!(parse_retry_after("1e30"), Some(MAX_RETRY_AFTER));
        assert_eq!(parse_retry_after("inf"), None);
    }

    #[test]
    fn retry_after_is_read_from_a_header_map() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, reqwest::header::HeaderValue::from_static("42"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(42)));
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn the_native_policy_bounds_headers_and_leaves_the_stream_to_the_idle_ceiling() {
        let native = HttpTimeouts::native();
        assert_eq!(native.header(), Some(DEFAULT_RESPONSE_HEADER_TIMEOUT));
        assert_eq!(
            native.chunk(),
            None,
            "the chunk phase stays with StreamIdleTimeout so the native 300s ceiling is preserved"
        );
        assert_eq!(native.total(), None);
    }

    #[tokio::test]
    async fn a_peer_that_accepts_and_never_answers_fails_with_a_typed_header_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind silent fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let _accepted = listener.accept().await.expect("accept request");
            // Hold the connection open without ever writing a status line. This is
            // the shape of a load balancer that accepted the TCP connection and
            // then lost its upstream.
            std::future::pending::<()>().await;
        });

        let header_timeout = Duration::from_millis(250);
        let deadlines =
            RequestDeadlines::start(HttpTimeouts::new(None, Some(header_timeout), None));
        let started = std::time::Instant::now();
        // The outer bound is deliberately far larger than the inner one: the
        // assertion below is a lower bound on elapsed time, so a loaded runner can
        // be arbitrarily late without failing the test.
        let outcome = tokio::time::timeout(
            Duration::from_secs(60),
            deadlines.headers(
                "silent-fixture",
                reqwest::Client::new()
                    .post(format!("http://{address}/v1/messages"))
                    .send(),
            ),
        )
        .await
        .expect("the header deadline must bound a silent peer");
        let elapsed = started.elapsed();

        let error = outcome.expect_err("a silent peer must not yield a response");
        assert!(
            matches!(error, ProviderError::Transient { .. }),
            "{error:?}"
        );
        assert!(error.is_retryable(), "{error:?}");
        let timeout = error
            .source()
            .expect("timeout cause")
            .downcast_ref::<HttpTimeoutError>()
            .expect("the failure must be the typed HTTP timeout, not rendered text");
        assert_eq!(timeout.phase(), TimeoutPhase::ResponseHeaders);
        assert_eq!(timeout.duration(), header_timeout);
        assert!(
            elapsed >= header_timeout,
            "the deadline fired early: {elapsed:?}"
        );

        server.abort();
        let _ = server.await;
    }

    #[test]
    fn a_vertex_prompt_too_long_asks_for_compaction_rather_than_failing() {
        // The deciding case for routing Vertex-hosted Anthropic through this
        // classifier: Gemini's classifier recognises only
        // `CONTEXT_LENGTH_EXCEEDED`, so an Anthropic `prompt_too_long` arrived as
        // HTTP 400 and became a permanent failure instead of a compaction.
        let error = map_messages_http_error(
            "google-vertex/anthropic",
            400,
            &HeaderMap::new(),
            br#"{"type":"error","error":{"type":"prompt_too_long","message":"opaque"}}"#,
        );
        assert!(
            matches!(error, ProviderError::ContextLimit { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_messages_error_event_classifies_from_its_structured_type() {
        let overloaded = map_messages_stream_error_value(
            "google-vertex/anthropic",
            &serde_json::json!({"type": "error", "error": {"type": "overloaded_error"}}),
        );
        assert!(overloaded.is_retryable(), "{overloaded:?}");
        let rate_limited = map_messages_stream_error_value(
            "anthropic",
            &serde_json::json!({"type": "error", "error": {"type": "rate_limit_error"}}),
        );
        assert!(
            matches!(rate_limited, ProviderError::RateLimited { .. }),
            "{rate_limited:?}"
        );
        let missing = map_messages_stream_error_value("anthropic", &serde_json::json!({}));
        assert!(
            matches!(missing, ProviderError::Fatal { .. }),
            "{missing:?}"
        );
    }

    #[test]
    fn vendor_prose_cannot_move_a_messages_error_between_classes() {
        let error = map_messages_http_error(
            "anthropic",
            400,
            &HeaderMap::new(),
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"429 overloaded context limit authentication"}}"#,
        );
        assert!(
            matches!(
                error,
                ProviderError::Fatal {
                    status: Some(400),
                    ..
                }
            ),
            "{error:?}"
        );
    }
}
