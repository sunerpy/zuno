//! OpenAI HTTP and in-stream error classification.

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Deserialize;
use serde_json::Value;
use zuno_error::ProviderError;

/// The structured OpenAI error object.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenAiErrorBody {
    /// Human-readable provider detail, retained only for display.
    #[serde(default)]
    pub message: Option<String>,
    /// Machine-readable error class.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Machine-readable error code.
    #[serde(default)]
    pub code: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct ErrorEnvelope {
    #[serde(default)]
    error: Option<OpenAiErrorBody>,
}

/// Parse an OpenAI `retry-after` header expressed as seconds.
#[must_use]
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    if seconds.is_finite() && !seconds.is_sign_negative() {
        Some(Duration::from_secs_f64(seconds))
    } else {
        None
    }
}

/// Classify a non-success OpenAI HTTP response.
#[must_use]
pub fn map_http_error(
    provider: &str,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
) -> ProviderError {
    let error = serde_json::from_slice::<ErrorEnvelope>(body)
        .ok()
        .and_then(|envelope| envelope.error);
    classify(provider, Some(status), retry_after(headers), error)
}

pub(crate) fn map_stream_error(provider: &str, error: OpenAiErrorBody) -> ProviderError {
    classify(provider, None, None, Some(error))
}

fn classify(
    provider: &str,
    status: Option<u16>,
    retry_after: Option<Duration>,
    error: Option<OpenAiErrorBody>,
) -> ProviderError {
    let code = error.as_ref().and_then(code_str);
    let kind = error.as_ref().and_then(|value| value.kind.as_deref());
    match (code, kind, status) {
        (Some("context_length_exceeded"), _, _)
        | (Some("max_tokens_exceeded"), _, _)
        | (_, Some("context_length_exceeded"), _) => ProviderError::ContextLimit {
            limit_tokens: None,
            used_tokens: None,
        },
        (Some("content_filter"), _, _) | (_, Some("content_filter"), _) => ProviderError::Refused {
            provider: provider.to_owned(),
            provider_text: error.and_then(|value| value.message),
        },
        (_, Some("authentication_error" | "permission_error"), _) | (_, _, Some(401 | 403)) => {
            ProviderError::Auth {
                provider: provider.to_owned(),
                source: error.map(api_source),
            }
        }
        (_, Some("rate_limit_error"), _) | (_, _, Some(429)) => {
            ProviderError::RateLimited { retry_after }
        }
        (_, Some("server_error"), _) | (_, _, Some(408 | 425 | 500..=599)) => {
            ProviderError::Transient {
                status,
                source: error.map(api_source),
            }
        }
        (_, _, Some(code)) => ProviderError::Fatal {
            status: Some(code),
            source: error.map(api_source),
        },
        _ => ProviderError::Fatal {
            status: None,
            source: error.map(api_source),
        },
    }
}

fn code_str(error: &OpenAiErrorBody) -> Option<&str> {
    error.code.as_ref()?.as_str()
}

fn api_source(error: OpenAiErrorBody) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(OpenAiApiError(error))
}

#[derive(Debug)]
struct OpenAiApiError(OpenAiErrorBody);

impl fmt::Display for OpenAiApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpenAI error")?;
        if let Some(kind) = &self.0.kind {
            write!(formatter, " type `{kind}`")?;
        }
        if let Some(code) = &self.0.code {
            write!(formatter, " code `{code}`")?;
        }
        if let Some(message) = &self.0.message {
            write!(formatter, ": {message}")?;
        }
        Ok(())
    }
}

impl std::error::Error for OpenAiApiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn rate_limit_preserves_fractional_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("1.5"));
        let error = map_http_error(
            "openai",
            429,
            &headers,
            br#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#,
        );
        assert!(matches!(
            error,
            ProviderError::RateLimited { retry_after: Some(value) }
                if value == Duration::from_millis(1_500)
        ));
    }
}
