use std::collections::BTreeMap;

use oc_llm::event::{FinishReason, Message, RequestContentBlock, Role, StreamEvent};
use oc_llm::registry::{ApiSurface, CompletionRequest};
use oc_provider_openai::{OpenAiConfig, OpenAiDecoder, Sampling, build_request_body};
use oc_testkit::{CassettePlayer, HttpInteraction, RequestSnapshot};
use serde_json::{Value, json};

const CHAT_MODEL: &str = "gpt-4o-mini";
const RESPONSES_MODEL: &str = "gpt-5.5";

fn recordings_available(test: &str) -> bool {
    oc_testkit::recordings_root_or_skip(test, "OpenAI cassette replay was NOT tested").is_some()
}

fn decode(
    interaction: &HttpInteraction,
    model: &str,
    surface: ApiSurface,
) -> (Vec<StreamEvent>, Vec<RequestContentBlock>) {
    assert_eq!(interaction.response.status, 200);
    assert!(interaction.response.is_sse());
    let mut decoder = OpenAiDecoder::new("openai", model, surface);
    let mut items = Vec::new();
    for chunk in interaction.response.body.as_bytes().chunks(17) {
        items.extend(decoder.push(chunk));
    }
    items.extend(decoder.finish());
    let events = items
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("recorded response decodes");
    (events, decoder.into_completed_blocks())
}

fn snapshot(url: &str, body: Value) -> RequestSnapshot {
    RequestSnapshot {
        method: "POST".to_owned(),
        url: url.to_owned(),
        headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
        body: serde_json::to_string(&body).expect("request JSON"),
    }
}

fn chat_config(max_tokens: u64) -> OpenAiConfig {
    OpenAiConfig::default()
        .with_max_tokens(max_tokens)
        .with_sampling(Sampling {
            temperature: Some(0.0),
            ..Sampling::default()
        })
}

fn weather_tool_chat() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get current weather for a city.",
            "parameters": {
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
                "additionalProperties": false,
            },
        },
    })
}

fn weather_tool_responses() -> Value {
    json!({
        "type": "function",
        "name": "get_weather",
        "description": "Get current weather for a city.",
        "parameters": {
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
            "additionalProperties": false,
        },
        "strict": false,
    })
}

#[test]
fn recorded_chat_text_matches_request_and_exact_events() {
    if !recordings_available("recorded_chat_text_matches_request_and_exact_events") {
        return;
    }
    let mut player =
        CassettePlayer::from_oracle("openai-chat/streams-text").expect("recorded conversation");
    let request = CompletionRequest::new(
        CHAT_MODEL,
        vec![
            Message::new(Role::System, "You are concise."),
            Message::new(Role::User, "Say hello in one short sentence."),
        ],
    )
    .on_surface(ApiSurface::Chat);
    let body = build_request_body(&request, &chat_config(20)).expect("request");
    let interaction = player
        .next_http(&snapshot(
            "https://api.openai.com/v1/chat/completions",
            body,
        ))
        .expect("request parity");
    let (events, blocks) = decode(interaction, CHAT_MODEL, ApiSurface::Chat);
    assert_eq!(
        events,
        vec![
            StreamEvent::TextDelta("Hello".to_owned()),
            StreamEvent::TextDelta("!".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(22),
                output_tokens: Some(2),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: None,
            },
        ]
    );
    assert_eq!(
        blocks,
        vec![RequestContentBlock::Text {
            text: "Hello!".to_owned(),
        }]
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_chat_tool_call_matches_request_and_exact_events() {
    if !recordings_available("recorded_chat_tool_call_matches_request_and_exact_events") {
        return;
    }
    let mut player = CassettePlayer::from_oracle("openai-chat/streams-tool-call")
        .expect("recorded conversation");
    let request = CompletionRequest::new(
        CHAT_MODEL,
        vec![
            Message::new(Role::System, "Call tools exactly as requested."),
            Message::new(Role::User, "Call get_weather with city exactly Paris."),
        ],
    )
    .on_surface(ApiSurface::Chat);
    let config = chat_config(80)
        .with_tools(vec![weather_tool_chat()])
        .with_tool_choice(json!({
            "type": "function",
            "function": { "name": "get_weather" },
        }));
    let body = build_request_body(&request, &config).expect("request");
    let interaction = player
        .next_http(&snapshot(
            "https://api.openai.com/v1/chat/completions",
            body,
        ))
        .expect("request parity");
    let (events, blocks) = decode(interaction, CHAT_MODEL, ApiSurface::Chat);
    assert_eq!(
        events,
        vec![
            StreamEvent::ToolUseStart {
                id: "call_5wBV98AvGPwOyC6a2HtKh85w".to_owned(),
                name: "get_weather".to_owned(),
            },
            StreamEvent::ToolInputDelta("{\"".to_owned()),
            StreamEvent::ToolInputDelta("city".to_owned()),
            StreamEvent::ToolInputDelta("\":\"".to_owned()),
            StreamEvent::ToolInputDelta("Paris".to_owned()),
            StreamEvent::ToolInputDelta("\"}".to_owned()),
            StreamEvent::ToolUseEnd,
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(67),
                output_tokens: Some(5),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: None,
            },
        ]
    );
    assert_eq!(
        blocks,
        vec![RequestContentBlock::ToolUse {
            id: "call_5wBV98AvGPwOyC6a2HtKh85w".to_owned(),
            name: "get_weather".to_owned(),
            input: json!({ "city": "Paris" }),
            thought_signature: None,
        }]
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_chat_two_turn_tool_loop_matches_both_requests() {
    if !recordings_available("recorded_chat_two_turn_tool_loop_matches_both_requests") {
        return;
    }
    let mut player = CassettePlayer::from_oracle("openai-chat/drives-a-tool-loop-end-to-end")
        .expect("recorded conversation");
    let system = Message::new(
        Role::System,
        "Use the get_weather tool, then answer in one short sentence.",
    );
    let user = Message::new(Role::User, "What is the weather in Paris?");
    let config = chat_config(80).with_tools(vec![weather_tool_chat()]);
    let first_request = CompletionRequest::new(CHAT_MODEL, vec![system.clone(), user.clone()])
        .on_surface(ApiSurface::Chat);
    let first_body = build_request_body(&first_request, &config).expect("first request");
    let first = player
        .next_http(&snapshot(
            "https://api.openai.com/v1/chat/completions",
            first_body,
        ))
        .expect("first request parity");
    let (first_events, first_blocks) = decode(first, CHAT_MODEL, ApiSurface::Chat);
    assert!(matches!(
        first_events.as_slice(),
        [
            StreamEvent::ToolUseStart { .. },
            ..,
            StreamEvent::TokenUsage { .. }
        ]
    ));
    assert!(first_events.contains(&StreamEvent::MessageEnd {
        stop_reason: Some(FinishReason::ToolCalls),
    }));

    let assistant = Message::from_content(Role::Assistant, first_blocks);
    let tool = Message::from_content(
        Role::Tool,
        vec![RequestContentBlock::ToolResult {
            tool_use_id: "call_tyZNHs2AudCbG4XJUEmX5Waw".to_owned(),
            content: "{\"temperature\":22,\"condition\":\"sunny\"}".to_owned(),
            is_error: None,
        }],
    );
    let second_request = CompletionRequest::new(CHAT_MODEL, vec![system, user, assistant, tool])
        .on_surface(ApiSurface::Chat);
    let second_body = build_request_body(&second_request, &config).expect("second request");
    let second = player
        .next_http(&snapshot(
            "https://api.openai.com/v1/chat/completions",
            second_body,
        ))
        .expect("second request parity");
    let (second_events, _) = decode(second, CHAT_MODEL, ApiSurface::Chat);
    assert_eq!(
        second_events.last(),
        Some(&StreamEvent::TokenUsage {
            input_tokens: Some(96),
            output_tokens: Some(15),
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: None,
        })
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_responses_text_uses_default_surface_and_exact_events() {
    if !recordings_available("recorded_responses_text_uses_default_surface_and_exact_events") {
        return;
    }
    let mut player = CassettePlayer::from_oracle("openai-responses/gpt-5-5-streams-text")
        .expect("recorded conversation");
    let request = CompletionRequest::new(
        RESPONSES_MODEL,
        vec![
            Message::new(Role::System, "You are concise."),
            Message::new(Role::User, "Reply with exactly: Hello!"),
        ],
    );
    let config = OpenAiConfig::default().with_max_tokens(80);
    let body = build_request_body(&request, &config).expect("request");
    let interaction = player
        .next_http(&snapshot("https://api.openai.com/v1/responses", body))
        .expect("request parity");
    let (events, _) = decode(interaction, RESPONSES_MODEL, ApiSurface::Default);
    assert!(matches!(
        &events[0],
        StreamEvent::ProviderReasoningItem {
            id,
            summary,
            encrypted_content: None,
            status: None,
        } if id == "rs_0ea948e2f42449980069fa8aa1d588819cbbcb9b056624d27c" && summary.is_empty()
    ));
    assert_eq!(
        &events[1..],
        [
            StreamEvent::TextDelta("Hello".to_owned()),
            StreamEvent::TextDelta("!".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(20),
                output_tokens: Some(18),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: None,
            },
        ]
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_responses_tool_call_matches_request_and_exact_events() {
    if !recordings_available("recorded_responses_tool_call_matches_request_and_exact_events") {
        return;
    }
    let mut player = CassettePlayer::from_oracle("openai-responses/gpt-5-5-streams-tool-call")
        .expect("recorded conversation");
    let request = CompletionRequest::new(
        RESPONSES_MODEL,
        vec![
            Message::new(Role::System, "Call tools exactly as requested."),
            Message::new(Role::User, "Call get_weather with city exactly Paris."),
        ],
    );
    let config = OpenAiConfig::default()
        .with_max_tokens(80)
        .with_tools(vec![weather_tool_responses()])
        .with_tool_choice(json!({ "type": "function", "name": "get_weather" }));
    let body = build_request_body(&request, &config).expect("request");
    let interaction = player
        .next_http(&snapshot("https://api.openai.com/v1/responses", body))
        .expect("request parity");
    let (events, blocks) = decode(interaction, RESPONSES_MODEL, ApiSurface::Responses);
    assert_eq!(
        events,
        vec![
            StreamEvent::ToolUseStart {
                id: "call_ZAbAwsIFeJSyPqz3HaHRXBSn".to_owned(),
                name: "get_weather".to_owned(),
            },
            StreamEvent::ToolInputDelta("{\"".to_owned()),
            StreamEvent::ToolInputDelta("city".to_owned()),
            StreamEvent::ToolInputDelta("\":\"".to_owned()),
            StreamEvent::ToolInputDelta("Paris".to_owned()),
            StreamEvent::ToolInputDelta("\"}".to_owned()),
            StreamEvent::ToolUseEnd,
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(61),
                output_tokens: Some(18),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: None,
            },
        ]
    );
    assert_eq!(
        blocks,
        vec![RequestContentBlock::ToolUse {
            id: "call_ZAbAwsIFeJSyPqz3HaHRXBSn".to_owned(),
            name: "get_weather".to_owned(),
            input: json!({ "city": "Paris" }),
            thought_signature: None,
        }]
    );
    player.finish().expect("cassette consumed");
}

#[test]
fn recorded_encrypted_reasoning_survives_store_false_continuation() {
    if !recordings_available("recorded_encrypted_reasoning_survives_store_false_continuation") {
        return;
    }
    let mut player = CassettePlayer::from_oracle(
        "openai-responses/openai-responses-gpt-5-5-reasoning-continuation",
    )
    .expect("recorded conversation");
    let reasoning_config = OpenAiConfig::default()
        .with_store(false)
        .with_include(vec![json!("reasoning.encrypted_content")])
        .with_reasoning(json!({ "effort": "low", "summary": "auto" }))
        .with_text(json!({ "verbosity": "low" }))
        .with_max_tokens(120);
    let first_request = CompletionRequest::new(
        RESPONSES_MODEL,
        vec![
            Message::new(
                Role::System,
                "Show concise reasoning when the provider supports visible reasoning summaries.",
            ),
            Message::new(Role::User, "Think briefly, then reply exactly with: Hello!"),
        ],
    );
    let first_body = build_request_body(&first_request, &reasoning_config).expect("first request");
    let first = player
        .next_http(&snapshot("https://api.openai.com/v1/responses", first_body))
        .expect("first request parity");
    let (first_events, first_blocks) = decode(first, RESPONSES_MODEL, ApiSurface::Responses);
    let encrypted = match &first_blocks[0] {
        RequestContentBlock::ProviderEncryptedReasoning {
            encrypted_content: Some(encrypted),
            ..
        } => encrypted.clone(),
        block => panic!("expected encrypted reasoning, got {block:?}"),
    };
    assert!(first_events.iter().any(|event| matches!(
        event,
        StreamEvent::ProviderReasoningItem {
            encrypted_content: Some(value),
            ..
        } if value == &encrypted
    )));

    let mut second_config = reasoning_config.clone().with_max_tokens(40);
    second_config = second_config.with_text(json!({ "verbosity": "low" }));
    let second_request = CompletionRequest::new(
        RESPONSES_MODEL,
        vec![
            Message::new(Role::User, "Think briefly, then reply exactly with: Hello!"),
            Message::from_content(Role::Assistant, first_blocks),
            Message::new(Role::User, "Now reply exactly with: Done."),
        ],
    );
    let second_body = build_request_body(&second_request, &second_config).expect("second request");
    assert_eq!(second_body["input"][1]["encrypted_content"], encrypted);
    assert!(second_body["input"][1].get("id").is_none());
    let second = player
        .next_http(&snapshot(
            "https://api.openai.com/v1/responses",
            second_body,
        ))
        .expect("encrypted replay request parity");
    let (second_events, _) = decode(second, RESPONSES_MODEL, ApiSurface::Responses);
    assert!(second_events.contains(&StreamEvent::TextDelta("Done".to_owned())));
    assert_eq!(
        second_events.last(),
        Some(&StreamEvent::TokenUsage {
            input_tokens: Some(35),
            output_tokens: Some(20),
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: None,
        })
    );
    player.finish().expect("cassette consumed");
}
