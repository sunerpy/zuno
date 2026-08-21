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

/// The Converse body a provider built from `options` sends.
///
/// Built from a [`Spec`] option bag rather than by setting fields on
/// [`BedrockGeneration`], because the bag is what the composition root writes: a
/// test that assigned the struct directly would keep passing if `from_spec` stopped
/// reading the keys, which is precisely how an accepted-and-ignored option survives.
fn bedrock_body(options: serde_json::Value, operation: Option<&str>) -> serde_json::Value {
    let mut spec = zuno_llm::registry::Spec::new("amazon-bedrock").with_region("us-east-1");
    for (name, value) in options.as_object().expect("options are an object") {
        spec = spec.with_option(name.clone(), value.clone());
    }
    if let Some(operation) = operation {
        spec = spec.with_option("operation", serde_json::json!(operation));
    }
    let provider = zuno_provider_bedrock::BedrockProvider::from_spec(&spec)
        .expect("the Bedrock spec resolves");
    let request = zuno_llm::registry::CompletionRequest::new(
        "anthropic.claude-sonnet-4-5",
        vec![zuno_llm::registry::Message {
            role: zuno_llm::registry::Role::User,
            content: vec![zuno_llm::event::RequestContentBlock::Text {
                text: "Say hello.".to_owned(),
            }],
        }],
    );
    provider
        .body_for(&request)
        .expect("the Bedrock body builds")
}

#[test]
fn converse_carries_the_generation_controls_under_inference_config() {
    let body = bedrock_body(
        serde_json::json!({"maxTokens": 8_192, "temperature": 0.3, "topP": 0.9}),
        None,
    );

    assert_eq!(
        body.get("inferenceConfig"),
        Some(&serde_json::json!({
            "maxTokens": 8_192,
            "temperature": 0.3,
            "topP": 0.9
        })),
        "Converse takes its base inference parameters in `inferenceConfig` \
         (Bedrock Runtime `InferenceConfiguration`), and this build sent none of them, \
         so every Converse request ran on the model's defaults"
    );
}

#[test]
fn converse_omits_inference_config_entirely_when_nothing_is_configured() {
    let body = bedrock_body(serde_json::json!({}), None);

    assert_eq!(
        body.get("inferenceConfig"),
        None,
        "an empty `inferenceConfig` is not the same as an absent one: absent means \
         `use the model's defaults`, which is what a caller who configured nothing asked for"
    );
}

#[test]
fn the_anthropic_native_cap_comes_from_configuration_rather_than_a_constant() {
    let body = bedrock_body(
        serde_json::json!({"maxTokens": 8_192, "temperature": 0.3, "topP": 0.9}),
        Some("invoke-with-response-stream"),
    );

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(8_192)),
        "the Anthropic-native path hardcoded 4096, so a user raising the cap silently \
         kept the old ceiling and long answers were truncated"
    );
    assert_eq!(body.get("temperature"), Some(&serde_json::json!(0.3)));
    assert_eq!(body.get("top_p"), Some(&serde_json::json!(0.9)));
}

#[test]
fn the_anthropic_native_cap_falls_back_because_the_field_is_required() {
    let body = bedrock_body(serde_json::json!({}), Some("invoke-with-response-stream"));

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(4096)),
        "Anthropic's Messages schema declares `max_tokens` required, so this is the one \
         generation control that must not be omitted when unset"
    );
}
