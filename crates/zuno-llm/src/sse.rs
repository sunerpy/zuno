//! Shared incremental decoding and framing for provider SSE streams.
//!
//! Provider transports pass every received byte chunk to [`SseParser::push`].
//! The parser owns the UTF-8 boundary state, so a code point split across network
//! chunks is completed before any frame or JSON processing sees it. Provider
//! implementations must not decode chunks or split frames independently.

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use serde::de::DeserializeOwned;
use zuno_error::ProviderError;

/// Environment override for the maximum gap between streamed chunks.
pub const STREAM_IDLE_TIMEOUT_ENV: &str = "ZUNO_STREAM_IDLE_TIMEOUT_SECS";

/// Environment override for the maximum wire bytes in one SSE event.
pub const STREAM_MAX_EVENT_BYTES_ENV: &str = "ZUNO_STREAM_MAX_EVENT_BYTES";

/// Environment override for the maximum accumulated JSON bytes in one tool call.
pub const STREAM_MAX_TOOL_INPUT_BYTES_ENV: &str = "ZUNO_STREAM_MAX_TOOL_INPUT_BYTES";

/// Longest user-visible wait budget for one provider recovery sequence.
///
/// Compatible transports also cap one silent response window at this value. The
/// retry executor derives its total deadline from the same constant, so changing
/// an attempt count or an idle default cannot accidentally restore a multi-window
/// hang.
pub const MAX_PROVIDER_WAIT: Duration = Duration::from_secs(180);

/// Default idle allowance for reasoning models that may pause before emitting.
pub const DEFAULT_STREAM_IDLE_TIMEOUT_SECS: u64 = 300;

/// Default maximum wire bytes in one SSE event.
///
/// The local tool-output gate is 51,200 bytes
/// (`crates/zuno-tool/src/output.rs:29-32`), while the reference implementation's
/// limit is 524,288 characters (the perf plan §3.2).
/// A real event may contain a long code block or an escaped large tool argument,
/// so 8 MiB leaves more than 16x the larger reference allowance without making a
/// missing SSE delimiter an unbounded allocation.
pub const DEFAULT_MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Default maximum accumulated JSON bytes in one tool call.
///
/// Four MiB is over 80x the local 51,200-byte tool-output gate and 8x the
/// reference implementation's 524,288-character gate cited above. That is ample
/// for a legitimate large patch or source payload while bounding JSON fragments
/// spread across many individually valid SSE events.
pub const DEFAULT_MAX_TOOL_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// Bytes handed to the UTF-8 decoder at a time.
///
/// [`crate::buffer::STEADY_STATE_CAPACITY_BYTES`] is calibrated to stay above
/// this, so an ordinary stream never reaches a capacity release.
pub const SSE_DECODE_CHUNK_BYTES: usize = 8 * 1024;

/// Size limits resolved once for a provider stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamLimits {
    max_event_bytes: usize,
    max_tool_input_bytes: usize,
}

impl StreamLimits {
    /// Creates explicit limits without consulting the environment.
    #[must_use]
    pub const fn new(max_event_bytes: usize, max_tool_input_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            max_tool_input_bytes,
        }
    }

    /// Resolves both limits from the process environment.
    ///
    /// A positive integer wins. Missing, zero, malformed, or out-of-range values
    /// retain the documented default for that limit independently.
    #[must_use]
    pub fn from_environment() -> Self {
        Self {
            max_event_bytes: resolve_stream_limit(
                DEFAULT_MAX_EVENT_BYTES,
                std::env::var(STREAM_MAX_EVENT_BYTES_ENV).ok().as_deref(),
            ),
            max_tool_input_bytes: resolve_stream_limit(
                DEFAULT_MAX_TOOL_INPUT_BYTES,
                std::env::var(STREAM_MAX_TOOL_INPUT_BYTES_ENV)
                    .ok()
                    .as_deref(),
            ),
        }
    }

    /// Maximum wire bytes accepted for one SSE event, inclusive.
    #[must_use]
    pub const fn max_event_bytes(self) -> usize {
        self.max_event_bytes
    }

    /// Maximum UTF-8 bytes accepted for one tool input, inclusive.
    #[must_use]
    pub const fn max_tool_input_bytes(self) -> usize {
        self.max_tool_input_bytes
    }
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_EVENT_BYTES, DEFAULT_MAX_TOOL_INPUT_BYTES)
    }
}

/// Incrementally decodes UTF-8 without treating a network chunk as a text unit.
#[derive(Debug, Default)]
pub struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    /// Creates a decoder with no pending bytes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one byte chunk and retains an incomplete trailing code point.
    ///
    /// Genuinely invalid byte sequences produce the Unicode replacement
    /// character, but a potentially valid truncated sequence is never replaced
    /// until the stream ends.
    pub fn decode(&mut self, chunk: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);

        let mut decoded = String::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            let remaining = &bytes[offset..];
            match std::str::from_utf8(remaining) {
                Ok(text) => {
                    decoded.push_str(text);
                    offset = bytes.len();
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        let valid = std::str::from_utf8(&remaining[..valid_up_to])
                            .expect("Utf8Error::valid_up_to always identifies valid UTF-8");
                        decoded.push_str(valid);
                    }

                    match error.error_len() {
                        Some(invalid_len) => {
                            decoded.push('\u{FFFD}');
                            offset += valid_up_to + invalid_len;
                        }
                        None => {
                            self.pending.extend_from_slice(&remaining[valid_up_to..]);
                            offset = bytes.len();
                        }
                    }
                }
            }
        }
        decoded
    }

    /// Ends the stream, replacing an incomplete trailing code point if present.
    pub fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        self.pending.clear();
        "\u{FFFD}".to_owned()
    }

    /// Whether a potentially valid but incomplete code point is buffered.
    #[must_use]
    pub fn has_pending_bytes(&self) -> bool {
        !self.pending.is_empty()
    }

    fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// One decoded server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The optional `event:` field.
    pub event: Option<String>,
    /// All `data:` fields joined with a newline, as required by the SSE format.
    pub data: String,
}

impl SseEvent {
    /// Deserializes this event's data with provider and model context preserved.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Fatal`] when the provider emitted malformed JSON.
    /// Its source names both the provider and model and retains the concrete
    /// [`serde_json::Error`] with its line and column.
    pub fn deserialize<T>(&self, provider: &str, model: &str) -> Result<T, ProviderError>
    where
        T: DeserializeOwned,
    {
        serde_json::from_str(&self.data).map_err(|source| {
            ProviderError::fatal(SseJsonError {
                provider: provider.to_owned(),
                model: model.to_owned(),
                source,
            })
        })
    }
}

/// Incremental parser shared by all text/event-stream provider transports.
#[derive(Debug)]
pub struct SseParser {
    decoder: Utf8StreamDecoder,
    buffer: String,
    provider: String,
    stream: String,
    limits: StreamLimits,
    failed: bool,
}

impl SseParser {
    /// Creates an empty parser with default limits and generic test context.
    #[must_use]
    pub fn new() -> Self {
        Self::for_stream("shared-sse", "unattributed")
    }

    /// Creates a parser for one provider/model stream using environment limits.
    #[must_use]
    pub fn for_stream(provider: impl Into<String>, stream: impl Into<String>) -> Self {
        Self::with_limits(provider, stream, StreamLimits::from_environment())
    }

    /// Creates a parser with explicit context and limits.
    #[must_use]
    pub fn with_limits(
        provider: impl Into<String>,
        stream: impl Into<String>,
        limits: StreamLimits,
    ) -> Self {
        Self {
            decoder: Utf8StreamDecoder::new(),
            buffer: String::new(),
            provider: provider.into(),
            stream: stream.into(),
            limits,
            failed: false,
        }
    }

    /// The limits frozen when this stream was created.
    #[must_use]
    pub const fn limits(&self) -> StreamLimits {
        self.limits
    }

    /// Accepts one raw network chunk and returns every complete SSE event in it.
    ///
    /// Both LF and CRLF blank-line separators are accepted, including separators
    /// and UTF-8 code points split across calls.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Result<SseEvent, ProviderError>> {
        if self.failed {
            return Vec::new();
        }

        let mut events = Vec::new();
        for bytes in chunk.chunks(SSE_DECODE_CHUNK_BYTES) {
            self.buffer.push_str(&self.decoder.decode(bytes));
            self.take_complete_events(&mut events);
            if self.failed {
                break;
            }

            let actual = incomplete_event_bytes(&self.buffer, self.decoder.pending_len());
            if actual > self.limits.max_event_bytes {
                let error = self.fail(StreamPayload::SseEvent, actual);
                events.push(Err(error));
                break;
            }
        }
        events
    }

    /// Ends the stream and emits a final frame even when it has no blank line.
    pub fn finish(&mut self) -> Vec<Result<SseEvent, ProviderError>> {
        if self.failed {
            return Vec::new();
        }

        let wire_bytes = self.buffer.len().saturating_add(self.decoder.pending_len());
        if wire_bytes > self.limits.max_event_bytes {
            return vec![Err(self.fail(StreamPayload::SseEvent, wire_bytes))];
        }

        self.buffer.push_str(&self.decoder.finish());
        let mut events = Vec::new();
        self.take_complete_events(&mut events);
        if self.failed {
            return events;
        }
        let trailing = std::mem::take(&mut self.buffer);
        if let Some(event) = parse_frame(&trailing) {
            events.push(Ok(event));
        }
        events
    }

    fn take_complete_events(&mut self, events: &mut Vec<Result<SseEvent, ProviderError>>) {
        while let Some((position, separator_len)) = next_separator(&self.buffer) {
            if position > self.limits.max_event_bytes {
                let error = self.fail(StreamPayload::SseEvent, position);
                events.push(Err(error));
                return;
            }
            let frame = self.buffer[..position].to_owned();
            self.buffer.drain(..position + separator_len);
            if let Some(event) = parse_frame(&frame) {
                events.push(Ok(event));
            }
        }
        // `drain` keeps the allocation, so without this one large event pins up to
        // `max_event_bytes` resident for the rest of the stream. See `crate::buffer`.
        crate::buffer::release_text_capacity(&mut self.buffer);
    }

    fn fail(&mut self, payload: StreamPayload, actual_bytes: usize) -> ProviderError {
        self.failed = true;
        self.buffer.clear();
        crate::buffer::release_text_capacity(&mut self.buffer);
        self.decoder = Utf8StreamDecoder::new();
        ProviderError::fatal(StreamLimitError {
            provider: self.provider.clone(),
            stream: self.stream.clone(),
            payload,
            actual_bytes,
            limit_bytes: self.limits.max_event_bytes,
        })
    }
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Appends one tool-input fragment only when the complete JSON remains bounded.
///
/// The target is unchanged on error. This is intentionally refusal rather than
/// truncation: a truncated JSON argument would fail later with unrelated syntax.
///
/// # Errors
///
/// Returns [`ProviderError::Fatal`] with the provider, stream, actual byte count,
/// configured limit, and environment override when the append would exceed `limit`.
pub fn append_tool_input(
    target: &mut String,
    fragment: &str,
    provider: &str,
    stream: &str,
    limit: usize,
) -> Result<(), ProviderError> {
    let actual_bytes = target.len().saturating_add(fragment.len());
    ensure_tool_input_size(actual_bytes, provider, stream, limit)?;
    target.push_str(fragment);
    Ok(())
}

/// Validates a complete tool-input string before it is retained.
///
/// # Errors
///
/// Returns [`ProviderError::Fatal`] when `actual_bytes` exceeds `limit`.
pub fn ensure_tool_input_size(
    actual_bytes: usize,
    provider: &str,
    stream: &str,
    limit: usize,
) -> Result<(), ProviderError> {
    if actual_bytes <= limit {
        return Ok(());
    }
    Err(ProviderError::fatal(StreamLimitError {
        provider: provider.to_owned(),
        stream: stream.to_owned(),
        payload: StreamPayload::ToolInput,
        actual_bytes,
        limit_bytes: limit,
    }))
}

/// Configured maximum time between two chunks from an SSE response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamIdleTimeout {
    duration: Duration,
}

impl StreamIdleTimeout {
    /// Creates an explicit timeout without consulting the environment.
    #[must_use]
    pub const fn new(duration: Duration) -> Self {
        Self { duration }
    }

    /// Resolves a provider-configured timeout with the process environment.
    ///
    /// A positive integer in [`STREAM_IDLE_TIMEOUT_ENV`] wins. Missing, zero, or
    /// malformed values retain the provider's configured duration.
    #[must_use]
    pub fn from_config(configured: Duration) -> Self {
        let environment = std::env::var(STREAM_IDLE_TIMEOUT_ENV).ok();
        Self::new(resolve_idle_timeout(configured, environment.as_deref()))
    }

    /// The resolved idle duration.
    #[must_use]
    pub const fn duration(self) -> Duration {
        self.duration
    }

    /// Waits for the next stream operation and classifies an idle gap as transient.
    ///
    /// Providers pass their `stream.next()` future here, then pass successful byte
    /// chunks to [`SseParser::push`]. This keeps timeout wording and classification
    /// consistent while leaving each transport's item error type intact.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Transient`] when no result arrives before the
    /// configured duration. The source error names the provider, model, duration,
    /// and the environment setting users can raise.
    pub async fn wait<F, T>(self, provider: &str, model: &str, next: F) -> Result<T, ProviderError>
    where
        F: Future<Output = T>,
    {
        tokio::time::timeout(self.duration, next)
            .await
            .map_err(|_| {
                ProviderError::transient(SseIdleTimeoutError {
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                    duration: self.duration,
                })
            })
    }
}

impl Default for StreamIdleTimeout {
    fn default() -> Self {
        Self::from_config(Duration::from_secs(DEFAULT_STREAM_IDLE_TIMEOUT_SECS))
    }
}

fn resolve_idle_timeout(configured: Duration, environment: Option<&str>) -> Duration {
    environment
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(configured)
}

fn resolve_stream_limit(default: usize, environment: Option<&str>) -> usize {
    environment
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(default)
}

fn incomplete_event_bytes(buffer: &str, pending_utf8_bytes: usize) -> usize {
    buffer
        .len()
        .saturating_sub(possible_separator_prefix_len(buffer.as_bytes()))
        .saturating_add(pending_utf8_bytes)
}

fn possible_separator_prefix_len(bytes: &[u8]) -> usize {
    [b"\r\n\r".as_slice(), b"\r\n", b"\r", b"\n"]
        .into_iter()
        .find(|prefix| bytes.ends_with(prefix))
        .map_or(0, <[u8]>::len)
}

fn next_separator(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|position| (position, 2));
    let crlf = buffer.find("\r\n\r\n").map(|position| (position, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 < right.0 { left } else { right }),
        (Some(separator), None) | (None, Some(separator)) => Some(separator),
        (None, None) => None,
    }
}

fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data = Vec::new();

    for raw_line in frame.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value.to_owned()),
            _ => {}
        }
    }

    if event.is_none() && data.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[derive(Debug, Clone, Copy)]
enum StreamPayload {
    SseEvent,
    ToolInput,
}

impl StreamPayload {
    const fn label(self) -> &'static str {
        match self {
            Self::SseEvent => "SSE event",
            Self::ToolInput => "tool input_json",
        }
    }

    const fn environment(self) -> &'static str {
        match self {
            Self::SseEvent => STREAM_MAX_EVENT_BYTES_ENV,
            Self::ToolInput => STREAM_MAX_TOOL_INPUT_BYTES_ENV,
        }
    }
}

#[derive(Debug)]
struct StreamLimitError {
    provider: String,
    stream: String,
    payload: StreamPayload,
    actual_bytes: usize,
    limit_bytes: usize,
}

impl fmt::Display for StreamLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} from provider `{}` stream `{}` reached {} bytes, exceeding limit {} bytes; raise {} only for a provider that legitimately emits larger payloads",
            self.payload.label(),
            self.provider,
            self.stream,
            self.actual_bytes,
            self.limit_bytes,
            self.payload.environment()
        )
    }
}

impl StdError for StreamLimitError {}

#[derive(Debug)]
struct SseJsonError {
    provider: String,
    model: String,
    source: serde_json::Error,
}

impl fmt::Display for SseJsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to deserialize SSE data from provider `{}` model `{}`: {}",
            self.provider, self.model, self.source
        )
    }
}

impl StdError for SseJsonError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
struct SseIdleTimeoutError {
    provider: String,
    model: String,
    duration: Duration,
}

impl fmt::Display for SseIdleTimeoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SSE stream for provider `{}` model `{}` received no data for {} seconds; raise {} for slower reasoning models",
            self.provider,
            self.model,
            self.duration.as_secs(),
            STREAM_IDLE_TIMEOUT_ENV
        )
    }
}

impl StdError for SseIdleTimeoutError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_idle_timeout_environment_value_overrides_config() {
        assert_eq!(
            resolve_idle_timeout(Duration::from_secs(300), Some("900")),
            Duration::from_secs(900)
        );
    }

    #[test]
    fn sse_invalid_idle_timeout_environment_value_keeps_config() {
        for value in [None, Some(""), Some("zero"), Some("0")] {
            assert_eq!(
                resolve_idle_timeout(Duration::from_secs(300), value),
                Duration::from_secs(300)
            );
        }
    }

    #[test]
    fn stream_size_environment_values_must_be_positive_integers() {
        assert_eq!(resolve_stream_limit(17, Some("23")), 23);
        for value in [None, Some(""), Some("unlimited"), Some("0")] {
            assert_eq!(resolve_stream_limit(17, value), 17);
        }
    }

    fn one_event(bytes: usize) -> String {
        format!("data: {}\n\n", "x".repeat(bytes))
    }

    /// Measured before the release existed: 8,388,608 resident bytes at `len() == 0`.
    #[test]
    fn a_large_event_does_not_strand_its_capacity_for_the_rest_of_the_stream() {
        let mut parser = SseParser::new();
        let events = parser.push(one_event(4 * 1024 * 1024).as_bytes());
        assert_eq!(
            events.len(),
            1,
            "the large event itself must still be parsed"
        );

        assert_eq!(parser.buffer.len(), 0, "the event should have drained");
        assert!(
            parser.buffer.capacity() <= crate::buffer::STEADY_STATE_CAPACITY_BYTES,
            "a drained stream is still holding {} bytes of capacity for zero live bytes, \
             against a {}-byte floor",
            parser.buffer.capacity(),
            crate::buffer::STEADY_STATE_CAPACITY_BYTES
        );
    }

    #[test]
    fn a_thousand_small_events_after_a_large_one_run_on_the_floor() {
        let mut parser = SseParser::new();
        let _ = parser.push(one_event(4 * 1024 * 1024).as_bytes());
        let before = parser.buffer.capacity();
        for _ in 0..1_000 {
            let events = parser.push(b"data: {\"delta\":\"tok\"}\n\n");
            assert_eq!(events.len(), 1);
        }
        assert!(
            parser.buffer.capacity() <= crate::buffer::STEADY_STATE_CAPACITY_BYTES,
            "1,000 small events after one large event are still running on {} bytes of \
             capacity, against a {}-byte floor",
            parser.buffer.capacity(),
            crate::buffer::STEADY_STATE_CAPACITY_BYTES
        );
        assert_eq!(
            parser.buffer.capacity(),
            before,
            "steady-state framing reallocated after the large event"
        );
    }

    #[test]
    fn a_rejected_oversized_event_leaves_no_capacity_behind() {
        let limits = StreamLimits::new(64 * 1024, DEFAULT_MAX_TOOL_INPUT_BYTES);
        let mut parser = SseParser::with_limits("probe", "stream", limits);
        let events = parser.push("z".repeat(4 * 1024 * 1024).as_bytes());
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err(), "the cap must still reject the event");

        assert!(
            parser.buffer.capacity() <= crate::buffer::STEADY_STATE_CAPACITY_BYTES,
            "the rejection path stranded {} bytes",
            parser.buffer.capacity()
        );
    }

    /// A partial frame must keep its room, or framing would corrupt mid-event.
    #[test]
    fn an_event_still_arriving_keeps_the_room_it_needs() {
        let mut parser = SseParser::new();
        let partial = "data: ".to_owned() + &"q".repeat(1024 * 1024);
        let events = parser.push(partial.as_bytes());
        assert!(events.is_empty(), "there is no separator yet");
        assert_eq!(parser.buffer.len(), partial.len());

        let completed = parser.push(b"\n\n");
        let event = completed
            .into_iter()
            .next()
            .expect("the completed event must parse")
            .expect("framing must succeed");
        assert_eq!(event.data.len(), 1024 * 1024);
    }
}
