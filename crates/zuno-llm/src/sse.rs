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

/// Longest user-visible wait budget for one provider recovery sequence.
///
/// Compatible transports also cap one silent response window at this value. The
/// retry executor derives its total deadline from the same constant, so changing
/// an attempt count or an idle default cannot accidentally restore a multi-window
/// hang.
pub const MAX_PROVIDER_WAIT: Duration = Duration::from_secs(180);

/// Default idle allowance for reasoning models that may pause before emitting.
pub const DEFAULT_STREAM_IDLE_TIMEOUT_SECS: u64 = 300;

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
#[derive(Debug, Default)]
pub struct SseParser {
    decoder: Utf8StreamDecoder,
    buffer: String,
}

impl SseParser {
    /// Creates an empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepts one raw network chunk and returns every complete SSE event in it.
    ///
    /// Both LF and CRLF blank-line separators are accepted, including separators
    /// and UTF-8 code points split across calls.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.push_str(&self.decoder.decode(chunk));
        self.take_complete_events()
    }

    /// Ends the stream and emits a final frame even when it has no blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        self.buffer.push_str(&self.decoder.finish());
        let mut events = self.take_complete_events();
        let trailing = std::mem::take(&mut self.buffer);
        if let Some(event) = parse_frame(&trailing) {
            events.push(event);
        }
        events
    }

    fn take_complete_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        while let Some((position, separator_len)) = next_separator(&self.buffer) {
            let frame = self.buffer[..position].to_owned();
            self.buffer.drain(..position + separator_len);
            if let Some(event) = parse_frame(&frame) {
                events.push(event);
            }
        }
        events
    }
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
}
