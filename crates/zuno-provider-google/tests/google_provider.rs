use std::collections::BTreeMap;

use serde_json::{Value, json};
use zuno_llm::effort::{DeclaredVariants, EffortCapabilities, ReasoningEffort};
use zuno_llm::event::{Message, RequestContentBlock, Role, StreamEvent, ThoughtSignature};
use zuno_llm::registry::{CompletionRequest, Provider};
use zuno_provider_google::{
    GeminiGenerationConfig, GeminiOptions, GeminiStreamDecoder, GeminiToolChoice,
    GeminiToolDefinition, GoogleGenerativeAi, VertexAnthropic, VertexAnthropicOptions,
    VertexCredentials, VertexGemini, google_thinking_config, vertex_anthropic_endpoint,
    vertex_gemini_endpoint,
};
use zuno_testkit::{CassettePlayer, RequestSnapshot};

fn recordings_available(test: &str) -> bool {
    zuno_testkit::recordings_root_or_skip(test, "Google provider cassette replay was NOT tested")
        .is_some()
}

fn text(role: Role, value: &str) -> Message {
    Message::new(role, value)
}

fn cassette_snapshot(url: String, body: Value) -> RequestSnapshot {
    RequestSnapshot {
        method: "POST".to_owned(),
        url,
        headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
        body: serde_json::to_string(&body).expect("request body serializes"),
    }
}

fn gemini_tool_options() -> GeminiOptions {
    GeminiOptions {
        generation: GeminiGenerationConfig {
            max_output_tokens: Some(80),
            temperature: Some(0.0),
            ..GeminiGenerationConfig::default()
        },
        tools: vec![GeminiToolDefinition {
            name: "get_weather".to_owned(),
            description: "Get current weather for a city.".to_owned(),
            parameters: json!({
                "required": ["city"],
                "type": "object",
                "properties": {"city": {"type": "string"}}
            }),
        }],
        tool_choice: Some(GeminiToolChoice::Tool("get_weather".to_owned())),
        ..GeminiOptions::default()
    }
}

#[test]
fn every_canonical_effort_maps_to_the_documented_thinking_config() {
    let expected = [
        json!({"includeThoughts": false, "thinkingBudget": 0}),
        json!({"includeThoughts": true, "thinkingLevel": "low"}),
        json!({"includeThoughts": true, "thinkingLevel": "medium"}),
        json!({"includeThoughts": true, "thinkingLevel": "high"}),
        json!({"includeThoughts": true, "thinkingLevel": "high"}),
        json!({"includeThoughts": true, "thinkingLevel": "high"}),
    ];
    let variants = DeclaredVariants::new();

    for (index, effort) in ReasoningEffort::ALL.into_iter().enumerate() {
        assert_eq!(
            google_thinking_config(effort, EffortCapabilities::default(), &variants),
            expected[index],
            "canonical effort {effort}"
        );
    }
}

#[test]
fn token_budget_capability_uses_the_resolver_budget_shape() {
    let variants = DeclaredVariants::new();
    let capabilities = EffortCapabilities {
        token_budget: true,
        max_budget_tokens: Some(20_000),
        ..EffortCapabilities::default()
    };

    assert_eq!(
        google_thinking_config(ReasoningEffort::Xhigh, capabilities, &variants),
        json!({"includeThoughts": true, "thinkingBudget": 20000})
    );
}

#[test]
fn gemini_keeps_static_and_volatile_context_as_separate_system_parts() {
    let provider = GoogleGenerativeAi::new("test-api-key", GeminiOptions::default())
        .expect("provider configuration");
    let request = CompletionRequest::new(
        "gemini-2.5-flash",
        vec![
            text(Role::System, "stable kernel"),
            text(Role::User, "exact user text"),
        ],
    )
    .with_developer_context(vec!["active goal".to_owned(), "memory".to_owned()]);

    let prepared = provider.prepare(&request).expect("Gemini request");
    assert_eq!(
        prepared.body["systemInstruction"]["parts"],
        json!([
            {"text": "stable kernel"},
            {"text": "active goal"},
            {"text": "memory"},
        ])
    );
    assert_eq!(prepared.body["contents"][0]["role"], "user");
    assert_eq!(
        prepared.body["contents"][0]["parts"][0]["text"],
        "exact user text"
    );
}

#[test]
fn gemini_request_and_stream_replay_the_real_tool_call_cassette() {
    if !recordings_available("gemini_request_and_stream_replay_the_real_tool_call_cassette") {
        return;
    }
    let provider = GoogleGenerativeAi::new("test-api-key", gemini_tool_options())
        .expect("provider configuration");
    let request = CompletionRequest::new(
        "gemini-2.5-flash",
        vec![
            text(Role::System, "Call tools exactly as requested."),
            text(Role::User, "Call get_weather with city exactly Paris."),
        ],
    );
    let prepared = provider.prepare(&request).expect("Gemini request");
    let incoming = cassette_snapshot(prepared.url.clone(), prepared.body.clone());

    let mut cassette = CassettePlayer::from_oracle("gemini/streams-tool-call")
        .expect("real Gemini cassette exists");
    let response = cassette
        .next_http(&incoming)
        .expect("request is byte-semantically identical to the recording")
        .response
        .decoded_body("gemini/streams-tool-call", 1)
        .expect("text body");
    cassette.finish().expect("cassette fully consumed");

    let mut decoder = GeminiStreamDecoder::new("google", "gemini-2.5-flash");
    let mut events = decoder.push(&response).expect("recorded SSE parses");
    events.extend(decoder.finish().expect("SSE finishes"));

    assert!(matches!(
        events.as_slice(),
        [
            StreamEvent::ToolUseStart { name, .. },
            StreamEvent::ToolInputDelta {
                id: input_id,
                delta: input,
            },
            StreamEvent::ToolUseSignature {
                id: signature_id,
                ..
            },
            StreamEvent::ToolUseEnd { id: end_id },
            StreamEvent::TokenUsage { .. },
            StreamEvent::MessageEnd { .. }
        ] if name == "get_weather"
            && input_id == "tool_0"
            && signature_id == input_id
            && end_id == input_id
            && input == r#"{"city":"Paris"}"#
    ));
}

#[test]
fn captured_tool_thought_signature_is_replayed_byte_identically_next_turn() {
    if !recordings_available(
        "captured_tool_thought_signature_is_replayed_byte_identically_next_turn",
    ) {
        return;
    }
    let mut cassette = CassettePlayer::from_oracle("gemini/streams-tool-call")
        .expect("real Gemini cassette exists");
    let response = cassette
        .next_unchecked()
        .expect("one interaction")
        .response
        .decoded_body("gemini/streams-tool-call", 1)
        .expect("text body");

    let mut decoder = GeminiStreamDecoder::new("google", "gemini-2.5-flash");
    let mut events = decoder.push(&response).expect("recorded SSE parses");
    events.extend(decoder.finish().expect("SSE finishes"));

    let (id, name) = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::ToolUseStart { id, name } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .expect("tool call start");
    let input: Value = serde_json::from_str(
        events
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolInputDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .expect("tool input"),
    )
    .expect("tool input JSON");
    let captured = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::ToolUseSignature { signature, .. } => Some(signature.clone()),
            _ => None,
        })
        .expect("the real response carries a thought signature");

    let second_turn = CompletionRequest::new(
        "gemini-2.5-flash",
        vec![
            Message::from_content(
                Role::Assistant,
                vec![RequestContentBlock::ToolUse {
                    id: id.clone(),
                    name,
                    input,
                    thought_signature: Some(captured.clone()),
                }],
            ),
            Message::from_content(
                Role::Tool,
                vec![RequestContentBlock::ToolResult {
                    tool_use_id: id,
                    content: r#"{"temperature":22,"condition":"sunny"}"#.to_owned(),
                    is_error: None,
                }],
            ),
        ],
    );
    let provider = GoogleGenerativeAi::new("test-api-key", GeminiOptions::default())
        .expect("provider configuration");
    let prepared = provider.prepare(&second_turn).expect("next-turn request");

    assert_eq!(
        prepared.body["contents"][0]["parts"][0]["thoughtSignature"],
        captured.as_str(),
        "turn N+1 must carry exactly the opaque bytes received on turn N"
    );
    assert_eq!(
        prepared.body["contents"][1]["parts"][0]["functionResponse"]["name"],
        "get_weather"
    );
}

#[test]
fn missing_tool_signature_stays_absent_rather_than_being_fabricated() {
    let request = CompletionRequest::new(
        "model-under-test",
        vec![Message::from_content(
            Role::Assistant,
            vec![RequestContentBlock::ToolUse {
                id: "tool_0".to_owned(),
                name: "lookup".to_owned(),
                input: json!({"query": "weather"}),
                thought_signature: None,
            }],
        )],
    );
    let provider = GoogleGenerativeAi::new("test-api-key", GeminiOptions::default())
        .expect("provider configuration");
    let prepared = provider.prepare(&request).expect("request");
    assert!(
        prepared.body["contents"][0]["parts"][0]
            .get("thoughtSignature")
            .is_none()
    );
}

#[test]
fn vertex_endpoint_rules_distinguish_global_regional_and_continental_hosts() {
    assert_eq!(
        vertex_gemini_endpoint("project-a", "global", "model-a").expect("global"),
        "https://aiplatform.googleapis.com/v1/projects/project-a/locations/global/publishers/google/models/model-a:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        vertex_gemini_endpoint("project-a", "us-central1", "model-a").expect("regional"),
        "https://us-central1-aiplatform.googleapis.com/v1/projects/project-a/locations/us-central1/publishers/google/models/model-a:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        vertex_anthropic_endpoint("project-a", "us", "model-a").expect("continental"),
        "https://aiplatform.us.rep.googleapis.com/v1/projects/project-a/locations/us/publishers/anthropic/models/model-a:streamRawPredict"
    );
    assert_eq!(
        vertex_anthropic_endpoint("project-a", "eu", "model-a").expect("continental"),
        "https://aiplatform.eu.rep.googleapis.com/v1/projects/project-a/locations/eu/publishers/anthropic/models/model-a:streamRawPredict"
    );
    assert_eq!(
        vertex_anthropic_endpoint("project-a", "global", "model-a").expect("global"),
        "https://aiplatform.googleapis.com/v1/projects/project-a/locations/global/publishers/anthropic/models/model-a:streamRawPredict"
    );
}

#[test]
fn three_provider_factories_remain_explicitly_distinct() {
    let credentials = VertexCredentials::access_token("test-token");
    let google = GoogleGenerativeAi::new("test-key", GeminiOptions::default()).expect("google");
    let vertex = VertexGemini::new(
        "project-a",
        "us-central1",
        credentials.clone(),
        GeminiOptions::default(),
    )
    .expect("vertex Gemini");
    let anthropic = VertexAnthropic::new(
        "project-a",
        "us",
        credentials,
        VertexAnthropicOptions::default(),
    )
    .expect("Vertex Anthropic");

    assert_eq!(google.id(), "google");
    assert_eq!(vertex.id(), "google-vertex");
    assert_eq!(anthropic.id(), "google-vertex/anthropic");
}

#[test]
fn service_account_json_is_supported_without_leaking_private_material() {
    let private_key =
        "-----BEGIN PRIVATE KEY-----\nSECRET-PRIVATE-MATERIAL\n-----END PRIVATE KEY-----\n";
    let credentials = VertexCredentials::service_account_json(&format!(
        r#"{{
          "type":"service_account",
          "project_id":"project-a",
          "private_key_id":"key-id",
          "private_key":{private_key:?},
          "client_email":"agent@project-a.iam.gserviceaccount.com",
          "token_uri":"https://oauth2.googleapis.com/token"
        }}"#
    ))
    .expect("standard service-account JSON parses");

    let rendered = format!("{credentials:?}");
    assert!(rendered.contains("service_account"));
    assert!(!rendered.contains("SECRET-PRIVATE-MATERIAL"));
}

#[test]
fn vertex_anthropic_uses_anthropic_shape_and_replays_real_anthropic_wire_events() {
    if !recordings_available(
        "vertex_anthropic_uses_anthropic_shape_and_replays_real_anthropic_wire_events",
    ) {
        return;
    }
    let provider = VertexAnthropic::new(
        "project-a",
        "us",
        VertexCredentials::access_token("test-token"),
        VertexAnthropicOptions {
            max_tokens: 80,
            system: vec!["Use the weather tool.".to_owned()],
            tools: vec![json!({
                "name": "get_weather",
                "description": "Get current weather for a city.",
                "input_schema": {"type":"object","properties":{"city":{"type":"string"}}}
            })],
            ..VertexAnthropicOptions::default()
        },
    )
    .expect("Vertex Anthropic");
    let request = CompletionRequest::new(
        "claude-model-under-test",
        vec![text(Role::User, "What is the weather in Paris?")],
    )
    .with_developer_context(vec!["active goal".to_owned(), "memory".to_owned()]);
    let prepared = provider.prepare(&request).expect("Anthropic request");

    assert_eq!(prepared.body["anthropic_version"], "vertex-2023-10-16");
    assert_eq!(prepared.body["stream"], true);
    assert!(prepared.body.get("contents").is_none());
    assert!(prepared.body.get("model").is_none());
    assert_eq!(prepared.body["messages"][0]["role"], "user");
    assert_eq!(
        prepared.body["system"],
        json!([
            {"type": "text", "text": "Use the weather tool."},
            {"type": "text", "text": "active goal"},
            {"type": "text", "text": "memory"},
        ])
    );
    assert!(prepared.url.contains(":streamRawPredict"));

    let mut cassette = CassettePlayer::from_oracle("anthropic-messages/streams-tool-call")
        .expect("real Anthropic protocol cassette exists");
    let response = cassette
        .next_unchecked()
        .expect("one interaction")
        .response
        .decoded_body("anthropic-messages/streams-tool-call", 1)
        .expect("text body");
    let mut decoder = provider.stream_decoder("claude-model-under-test");
    let mut events = decoder.push(&response).expect("Anthropic SSE parses");
    events.extend(decoder.finish().expect("SSE finishes"));

    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolUseStart { name, .. } if name == "get_weather"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::MessageEnd {
            stop_reason: Some(zuno_llm::event::FinishReason::ToolCalls)
        }
    )));
}

#[test]
fn thought_signature_wrapper_keeps_opaque_bytes() {
    let signature = ThoughtSignature::new("AA+/=_opaque_signature");
    assert_eq!(signature.as_str(), "AA+/=_opaque_signature");
}
