//! Anthropic HTTP and in-stream error classification.

use std::fmt;
use std::time::Duration;

use oc_error::ProviderError;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Deserialize;

/// The structured `error` object returned by Anthropic.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AnthropicErrorBody {
    /// Anthropic's machine-readable error category.
    #[serde(rename = "type")]
    pub kind: String,
    /// Anthropic's human-readable detail. This is display payload only and is
    /// never inspected to decide recovery.
    #[serde(default)]
    pub message: Option<String>,
    /// Context window size when an Anthropic-compatible endpoint supplies it.
    #[serde(default)]
    pub limit_tokens: Option<u64>,
    /// Submitted token count when an Anthropic-compatible endpoint supplies it.
    #[serde(default)]
    pub used_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: AnthropicErrorBody,
    #[serde(default)]
    request_id: Option<String>,
}

/// Parse a numeric `retry-after` response header.
///
/// Both integer and fractional seconds are accepted. HTTP-date values are left
/// to the caller's normal backoff because converting them requires a wall clock
/// and the provider's test corpus only establishes the numeric form.
#[must_use]
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let seconds = value.parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds.is_sign_negative() {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}

/// Classify a non-success Anthropic response from status and structured body.
///
/// The rendered `message` field is retained as source/display data but never
/// participates in the match. If the body is malformed, status alone supplies
/// the recovery class through [`ProviderError::from_status`].
#[must_use]
pub fn map_http_error(
    provider: &str,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
) -> ProviderError {
    let envelope = serde_json::from_slice::<ErrorEnvelope>(body).ok();
    let retry_after = retry_after(headers);
    map_error(provider, Some(status), retry_after, envelope)
}

pub(crate) fn map_stream_error(provider: &str, error: AnthropicErrorBody) -> ProviderError {
    map_error(
        provider,
        None,
        None,
        Some(ErrorEnvelope {
            error,
            request_id: None,
        }),
    )
}

fn map_error(
    provider: &str,
    status: Option<u16>,
    retry_after: Option<Duration>,
    envelope: Option<ErrorEnvelope>,
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
            source: envelope.map(api_source),
        },
        Some("overloaded_error" | "api_error") => ProviderError::Transient {
            status,
            source: envelope.map(api_source),
        },
        Some("refusal_error") => ProviderError::Refused {
            provider: provider.to_owned(),
            provider_text: envelope.and_then(|value| value.error.message),
        },
        _ => match status {
            Some(429) => ProviderError::RateLimited { retry_after },
            Some(401 | 403) => ProviderError::Auth {
                provider: provider.to_owned(),
                source: envelope.map(api_source),
            },
            Some(code @ (408 | 425 | 500..=599)) => ProviderError::Transient {
                status: Some(code),
                source: envelope.map(api_source),
            },
            Some(code) => ProviderError::Fatal {
                status: Some(code),
                source: envelope.map(api_source),
            },
            None => ProviderError::Fatal {
                status: None,
                source: envelope.map(api_source),
            },
        },
    }
}

fn api_source(envelope: ErrorEnvelope) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(AnthropicApiError {
        kind: envelope.error.kind,
        message: envelope.error.message,
        request_id: envelope.request_id,
    })
}

#[derive(Debug)]
struct AnthropicApiError {
    kind: String,
    message: Option<String>,
    request_id: Option<String>,
}

impl fmt::Display for AnthropicApiError {
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

impl std::error::Error for AnthropicApiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

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
