//! Anthropic HTTP and in-stream error classification.
//!
//! The classification itself lives in [`zuno_llm::http`]. Anthropic Messages is a
//! wire format with two first-party speakers in this workspace — this crate and
//! the Vertex-hosted Anthropic path in `zuno-provider-google` — and they were
//! classifying it differently: Vertex reused Gemini's classifier, which does not
//! recognise `prompt_too_long`, so a context overflow there became a permanent
//! failure instead of a compaction. Keeping the match table in one place is the
//! only way both speakers can stay in agreement.

use std::time::Duration;

use reqwest::header::HeaderMap;
use zuno_error::ProviderError;

pub use zuno_llm::http::MessagesErrorBody as AnthropicErrorBody;

/// Parse a `retry-after` response header.
///
/// Delegates to the single shared parser, which accepts delta-seconds — integer
/// or the fractional form several vendors send — and the RFC 9110 HTTP-date form.
#[must_use]
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    zuno_llm::http::retry_after(headers)
}

/// Classify a non-success Anthropic response from status and structured body.
///
/// The rendered `message` field is retained as source/display data but never
/// participates in the match. If the body is malformed, status alone supplies
/// the recovery class.
#[must_use]
pub fn map_http_error(
    provider: &str,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
) -> ProviderError {
    zuno_llm::http::map_messages_http_error(provider, status, headers, body)
}

pub(crate) fn map_stream_error(provider: &str, error: AnthropicErrorBody) -> ProviderError {
    zuno_llm::http::map_messages_stream_error(provider, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderValue, RETRY_AFTER};

    #[test]
    fn status_429_preserves_numeric_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("2.5"));
        let error = map_http_error(
            "anthropic",
            429,
            &headers,
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        );
        assert!(matches!(
            error,
            ProviderError::RateLimited {
                retry_after: Some(duration)
            } if duration == Duration::from_millis(2_500)
        ));
    }

    #[test]
    fn status_429_now_also_preserves_an_http_date_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Fri, 01 Jan 2100 00:00:00 GMT"),
        );
        let error = map_http_error(
            "anthropic",
            429,
            &headers,
            br#"{"type":"error","error":{"type":"rate_limit_error"}}"#,
        );
        assert!(
            matches!(
                error,
                ProviderError::RateLimited {
                    retry_after: Some(_)
                }
            ),
            "the date form used to be dropped, silently replacing the peer's interval \
             with local backoff: {error:?}"
        );
    }

    #[test]
    fn error_message_cannot_change_status_classification() {
        let headers = HeaderMap::new();
        let error = map_http_error(
            "anthropic",
            400,
            &headers,
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"429 overloaded context limit authentication"}}"#,
        );
        assert!(matches!(
            error,
            ProviderError::Fatal {
                status: Some(400),
                ..
            }
        ));
    }

    #[test]
    fn structured_context_type_wins_without_message_inspection() {
        let error = map_http_error(
            "anthropic",
            400,
            &HeaderMap::new(),
            br#"{"type":"error","error":{"type":"context_length_exceeded","message":"opaque","limit_tokens":200000,"used_tokens":210123}}"#,
        );
        assert!(matches!(
            error,
            ProviderError::ContextLimit {
                limit_tokens: Some(200_000),
                used_tokens: Some(210_123)
            }
        ));
    }
}
