use std::collections::BTreeMap;
use std::time::Duration;

use zuno_error::ProviderError;
use zuno_llm::registry::ApiSurface;
use zuno_provider_bedrock::{classify_bedrock_error, mantle_surface};

#[test]
fn mantle_routes_only_the_two_oracle_safeguard_ids_to_chat() {
    for model in [
        "openai.gpt-oss-safeguard-20b",
        "openai.gpt-oss-safeguard-120b",
    ] {
        assert_eq!(mantle_surface(model), ApiSurface::Chat, "{model}");
    }
    for model in [
        "openai.gpt-oss-20b",
        "openai.gpt-oss-120b",
        "anthropic.claude-sonnet-4-5",
    ] {
        assert_eq!(mantle_surface(model), ApiSurface::Responses, "{model}");
    }
}

#[test]
fn structured_error_codes_and_statuses_drive_classification() {
    let retry_headers = BTreeMap::from([("retry-after".to_owned(), "7".to_owned())]);
    assert!(matches!(
        classify_bedrock_error(
            429,
            &retry_headers,
            br#"{"__type":"ThrottlingException","message":"slow down"}"#,
        ),
        ProviderError::RateLimited {
            retry_after: Some(delay),
        } if delay == Duration::from_secs(7)
    ));

    assert!(matches!(
        classify_bedrock_error(
            400,
            &BTreeMap::new(),
            br#"{"__type":"ContextLengthExceededException","maxInputTokens":4096,"inputTokenCount":5000}"#,
        ),
        ProviderError::ContextLimit {
            limit_tokens: Some(4096),
            used_tokens: Some(5000),
        }
    ));

    assert!(matches!(
        classify_bedrock_error(
            403,
            &BTreeMap::new(),
            br#"{"__type":"AccessDeniedException"}"#,
        ),
        ProviderError::Auth { provider, .. } if provider == "amazon-bedrock"
    ));

    assert!(matches!(
        classify_bedrock_error(
            400,
            &BTreeMap::new(),
            br#"{"__type":"GuardrailIntervened","message":"blocked by policy"}"#,
        ),
        ProviderError::Refused {
            provider,
            provider_text: Some(text),
        } if provider == "amazon-bedrock" && text == "blocked by policy"
    ));
}

#[test]
fn validation_message_text_never_changes_retry_or_context_classification() {
    for message in [
        "rate limit, retry later",
        "maximum context length exceeded",
        "authentication failed",
    ] {
        let body = serde_json::to_vec(&serde_json::json!({
            "__type": "ValidationException",
            "message": message,
        }))
        .expect("serialize body");
        assert!(matches!(
            classify_bedrock_error(400, &BTreeMap::new(), &body),
            ProviderError::Fatal {
                status: Some(400),
                ..
            }
        ));
    }
}
