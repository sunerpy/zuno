use std::collections::BTreeMap;

use serde_json::json;
use zuno_error::ProviderError;
use zuno_llm::event::{
    ConnectionPhase, FinishReason, Message, PromptAccounting, Role, StreamEvent,
};
use zuno_llm::registry::CompletionRequest;
use zuno_provider_anthropic::{
    AnthropicConfig, AnthropicDecoder, build_request_body, map_http_error,
};
use zuno_testkit::{CassettePlayer, HttpInteraction, RequestSnapshot};

const HAIKU: &str = "claude-haiku-4-5-20251001";
const OPUS: &str = "claude-opus-4-7";

fn recordings_available(test: &str) -> bool {
    zuno_testkit::recordings_root_or_skip(test, "Anthropic cassette replay was NOT tested")
        .is_some()
}

fn decode(interaction: &HttpInteraction, model: &str) -> Vec<StreamEvent> {
    assert_eq!(interaction.response.status, 200);
    assert!(interaction.response.is_sse());
    let mut decoder = AnthropicDecoder::new("anthropic", model);
    let mut items = Vec::new();
    for chunk in interaction.response.body.as_bytes().chunks(17) {
        items.extend(decoder.push(chunk));
    }
    items.extend(decoder.finish());
    items
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("recorded response decodes")
}

fn text_transcript(
    fragments: &[&str],
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write: u64,
) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::ConnectionPhase {
        phase: ConnectionPhase::Streaming,
    }];
    events.extend(
        fragments
            .iter()
            .map(|text| StreamEvent::TextDelta((*text).to_owned())),
    );
    events.push(StreamEvent::MessageEnd {
        stop_reason: Some(FinishReason::Stop),
    });
    events.push(StreamEvent::TokenUsage {
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
        cache_read_input_tokens: Some(cache_read),
        cache_write_input_tokens: Some(cache_write),
        accounting: PromptAccounting::CacheBesideInput,
    });
    events
}

fn request_snapshot(body: serde_json::Value) -> RequestSnapshot {
    RequestSnapshot {
        method: "POST".to_owned(),
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: BTreeMap::from([
            ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ]),
        body: serde_json::to_string(&body).expect("request json"),
    }
}

#[test]
fn recorded_streams_text_matches_request_and_exact_events() {
    if !recordings_available("recorded_streams_text_matches_request_and_exact_events") {
        return;
    }
    let mut player = CassettePlayer::from_oracle("anthropic-messages/streams-text")
        .expect("recorded conversation");
    let request = CompletionRequest::new(
        HAIKU,
        vec![
            Message::new(Role::System, "You are concise."),
            Message::new(Role::User, "Reply with exactly: Hello!"),
        ],
    );
    let config = AnthropicConfig::default()
        .with_max_tokens(20)
        .with_temperature(0.0)
        .with_prompt_cache(false);
    let body = build_request_body(&request, &config).expect("build request");
    let interaction = player
        .next_http(&request_snapshot(body))
        .expect("request parity");
    assert_eq!(
        decode(interaction, HAIKU),
        text_transcript(&["Hello!"], 18, 5, 0, 0)
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_streams_tool_call_matches_request_and_exact_events() {
    if !recordings_available("recorded_streams_tool_call_matches_request_and_exact_events") {
        return;
    }
    let mut player = CassettePlayer::from_oracle("anthropic-messages/streams-tool-call")
        .expect("recorded conversation");
    let request = CompletionRequest::new(
        HAIKU,
        vec![
            Message::new(Role::System, "Call tools exactly as requested."),
            Message::new(Role::User, "Call get_weather with city exactly Paris."),
        ],
    );
    let config = AnthropicConfig::default()
        .with_max_tokens(80)
        .with_temperature(0.0)
        .with_prompt_cache(false)
        .with_tools(vec![json!({
            "name": "get_weather",
            "description": "Get current weather for a city.",
            "input_schema": {
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
                "additionalProperties": false
            }
        })])
        .with_tool_choice(json!({ "type": "tool", "name": "get_weather" }));
    let body = build_request_body(&request, &config).expect("build request");
    let interaction = player
        .next_http(&request_snapshot(body))
        .expect("request parity");
    assert_eq!(
        decode(interaction, HAIKU),
        vec![
            StreamEvent::ToolUseStart {
                id: "toolu_012rmAruviySvUXSjgCPWVRu".to_owned(),
                name: "get_weather".to_owned(),
            },
            StreamEvent::ConnectionPhase {
                phase: ConnectionPhase::Streaming,
            },
            StreamEvent::ToolInputDelta(String::new()),
            StreamEvent::ToolInputDelta("{\"city\":".to_owned()),
            StreamEvent::ToolInputDelta(" \"Paris\"}".to_owned()),
            StreamEvent::ToolUseEnd,
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(677),
                output_tokens: Some(33),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: Some(0),
                accounting: PromptAccounting::CacheBesideInput,
            },
        ]
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_two_turn_tool_loop_matches_both_exact_event_sequences() {
    if !recordings_available("recorded_two_turn_tool_loop_matches_both_exact_event_sequences") {
        return;
    }
    let mut player =
        CassettePlayer::from_oracle("anthropic-messages/claude-opus-4-7-drives-a-tool-loop")
            .expect("recorded conversation");

    let first = player.next_unchecked().expect("first interaction");
    assert_eq!(
        decode(first, OPUS),
        vec![
            StreamEvent::ToolUseStart {
                id: "toolu_01M8nJQQMxqpv1VaPYuJKT4j".to_owned(),
                name: "get_weather".to_owned(),
            },
            StreamEvent::ConnectionPhase {
                phase: ConnectionPhase::Streaming,
            },
            StreamEvent::ToolInputDelta(String::new()),
            StreamEvent::ToolInputDelta("{\"city\": ".to_owned()),
            StreamEvent::ToolInputDelta("\"Pa".to_owned()),
            StreamEvent::ToolInputDelta("ris\"}".to_owned()),
            StreamEvent::ToolUseEnd,
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(798),
                output_tokens: Some(66),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: Some(0),
                accounting: PromptAccounting::CacheBesideInput,
            },
        ]
    );

    let second = player.next_unchecked().expect("second interaction");
    assert_eq!(
        decode(second, OPUS),
        text_transcript(&["Paris is curr", "ently sunny at 22°C."], 895, 19, 0, 0,)
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_malformed_order_patch_matches_exact_events() {
    if !recordings_available("recorded_malformed_order_patch_matches_exact_events") {
        return;
    }
    let mut player = CassettePlayer::from_oracle(
        "anthropic-messages/accepts-malformed-assistant-tool-order-with-default-patch",
    )
    .expect("recorded conversation");
    let interaction = player.next_unchecked().expect("interaction");
    assert_eq!(
        decode(interaction, HAIKU),
        text_transcript(
            &["The", " weather in Paris is currently 72°F."],
            638,
            14,
            0,
            0,
        )
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_image_tool_result_matches_exact_events() {
    if !recordings_available("recorded_image_tool_result_matches_exact_events") {
        return;
    }
    let mut player =
        CassettePlayer::from_oracle("anthropic-messages/anthropic-opus-4-7-image-tool-result")
            .expect("recorded conversation");
    let interaction = player.next_unchecked().expect("interaction");
    assert_eq!(
        decode(interaction, OPUS),
        text_transcript(&["j", "iggling restroom prison"], 1_005, 13, 0, 0)
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_cache_write_then_read_matches_exact_usage() {
    if !recordings_available("recorded_cache_write_then_read_matches_exact_usage") {
        return;
    }
    let mut player = CassettePlayer::from_oracle(
        "anthropic-messages-cache/writes-then-reads-cache-control-on-identical-second-call",
    )
    .expect("recorded conversation");

    let first = decode(player.next_unchecked().expect("cache write"), HAIKU);
    let second = decode(player.next_unchecked().expect("cache read"), HAIKU);
    assert_eq!(first, text_transcript(&["Hi."], 9, 5, 0, 5_752));
    assert_eq!(second, text_transcript(&["Hi."], 9, 5, 5_752, 0));
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_structured_invalid_request_is_fatal_without_message_matching() {
    if !recordings_available(
        "recorded_structured_invalid_request_is_fatal_without_message_matching",
    ) {
        return;
    }
    let mut player = CassettePlayer::from_oracle(
        "anthropic-messages/rejects-malformed-assistant-tool-order-without-patch",
    )
    .expect("recorded conversation");
    let interaction = player.next_unchecked().expect("interaction");
    let error = map_http_error(
        "anthropic",
        interaction.response.status,
        &interaction
            .response
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                    reqwest::header::HeaderValue::from_str(value).expect("header value"),
                )
            })
            .collect(),
        interaction.response.body.as_bytes(),
    );
    assert!(matches!(
        error,
        ProviderError::Fatal {
            status: Some(400),
            ..
        }
    ));
    player.finish().expect("cassette consumed");
}
