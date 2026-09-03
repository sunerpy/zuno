use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;
use zuno_error::{BoxSource, ProviderError};

pub const PROVIDER_ID: &str = "amazon-bedrock";

pub fn classify_bedrock_error(
    status: u16,
    headers: &BTreeMap<String, String>,
    body: &[u8],
) -> ProviderError {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let code = parsed.as_ref().and_then(error_code);
    let retry_after = retry_after(headers, parsed.as_ref());
    let source = || service_error_source(status, code, parsed.as_ref());

    if status == 429 || code.is_some_and(is_rate_limit_code) {
        return ProviderError::RateLimited { retry_after };
    }
    if code.is_some_and(is_context_limit_code) {
        return ProviderError::ContextLimit {
            limit_tokens: parsed
                .as_ref()
                .and_then(|value| value.get("maxInputTokens"))
                .and_then(Value::as_u64),
            used_tokens: parsed
                .as_ref()
                .and_then(|value| value.get("inputTokenCount"))
                .and_then(Value::as_u64),
        };
    }
    if matches!(status, 401 | 403) || code.is_some_and(is_auth_code) {
        return ProviderError::Auth {
            provider: PROVIDER_ID.to_owned(),
            source: Some(source()),
        };
    }
    if code.is_some_and(is_refusal_code) {
        return ProviderError::Refused {
            provider: PROVIDER_ID.to_owned(),
            provider_text: parsed
                .as_ref()
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
    }

    let original_status = parsed
        .as_ref()
        .and_then(|value| value.get("originalStatusCode"))
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok());
    if matches!(status, 408 | 425 | 500..=599)
        || code.is_some_and(is_transient_code)
        || original_status.is_some_and(|value| value >= 500)
    {
        return ProviderError::Transient {
            status: Some(original_status.unwrap_or(status)),
            source: Some(source()),
        };
    }
    ProviderError::Fatal {
        status: Some(status),
        source: Some(source()),
    }
}

fn error_code(value: &Value) -> Option<&str> {
    ["__type", "code", "Code", "type"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(|code| code.rsplit('#').next().unwrap_or(code))
}

fn retry_after(headers: &BTreeMap<String, String>, body: Option<&Value>) -> Option<Duration> {
    let from_body = body
        .and_then(|value| value.get("retryAfterSeconds"))
        .and_then(Value::as_u64)
        .map(Duration::from_secs);
    if from_body.is_some() {
        return from_body;
    }
    // Bedrock's own `retryAfterSeconds` is the service's instruction and stays ahead
    // of the transport header; the header itself is RFC 9110 §10.2.3 and is parsed by
    // the one shared implementation so every provider reads both forms identically.
    let value = headers
        .iter()
        .find_map(|(name, value)| name.eq_ignore_ascii_case("retry-after").then_some(value))?;
    zuno_llm::http::parse_retry_after(value)
}

fn is_rate_limit_code(code: &str) -> bool {
    matches!(
        code,
        "ThrottlingException" | "TooManyRequestsException" | "ServiceQuotaExceededException"
    )
}

fn is_context_limit_code(code: &str) -> bool {
    matches!(
        code,
        "ContextLengthExceededException"
            | "InputTooLongException"
            | "context_length_exceeded"
            | "input_too_long"
    )
}

fn is_auth_code(code: &str) -> bool {
    matches!(
        code,
        "AccessDeniedException"
            | "UnrecognizedClientException"
            | "InvalidSignatureException"
            | "ExpiredTokenException"
            | "IncompleteSignature"
    )
}

fn is_refusal_code(code: &str) -> bool {
    matches!(
        code,
        "GuardrailIntervened"
            | "GuardrailIntervenedException"
            | "ContentFilterException"
            | "SafetyViolationException"
    )
}

fn is_transient_code(code: &str) -> bool {
    matches!(
        code,
        "InternalServerException"
            | "ServiceUnavailableException"
            | "ModelNotReadyException"
            | "ModelTimeoutException"
            | "ModelStreamErrorException"
    )
}

fn service_error_source(status: u16, code: Option<&str>, body: Option<&Value>) -> BoxSource {
    Box::new(BedrockServiceError {
        status,
        code: code.map(str::to_owned),
        provider_text: body
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

#[derive(Debug, thiserror::Error)]
#[error("Bedrock service error status={status} code={code:?}: {provider_text:?}")]
struct BedrockServiceError {
    status: u16,
    code: Option<String>,
    provider_text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_aws_error_type_is_normalized_structurally() {
        let value = serde_json::json!({"__type": "com.amazonaws#ThrottlingException"});
        assert_eq!(error_code(&value), Some("ThrottlingException"));
    }

    #[test]
    fn the_services_own_retry_after_seconds_outranks_the_transport_header() {
        let headers = BTreeMap::from([("Retry-After".to_owned(), "5".to_owned())]);
        let body = serde_json::json!({"__type": "ThrottlingException", "retryAfterSeconds": 30});
        assert_eq!(
            retry_after(&headers, Some(&body)),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn a_throttle_carries_the_http_date_form_of_retry_after() {
        let headers = BTreeMap::from([(
            "retry-after".to_owned(),
            // One RFC 9110 IMF-fixdate far enough out that the delta stays positive
            // for the lifetime of this repository.
            "Sun, 06 Nov 2044 08:49:37 GMT".to_owned(),
        )]);
        let error = classify_bedrock_error(
            429,
            &headers,
            br#"{"__type":"ThrottlingException","message":"slow down"}"#,
        );
        let ProviderError::RateLimited {
            retry_after: Some(delay),
        } = error
        else {
            panic!("expected a rate limit carrying the peer's deadline, got {error:?}");
        };
        assert!(!delay.is_zero(), "an HTTP-date deadline must survive");
    }

    #[test]
    fn a_malformed_retry_after_is_dropped_rather_than_guessed() {
        let headers = BTreeMap::from([("retry-after".to_owned(), "soon".to_owned())]);
        assert_eq!(retry_after(&headers, None), None);
    }
}
