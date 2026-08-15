use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::ready;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Duration;

use base64::Engine as _;
use oc_engine::retry::{ProviderRetryPolicy, retry_provider_with_sleep};
use oc_error::ProviderError;
use oc_llm::event::{ConnectionPhase, FinishReason, StreamEvent, ThoughtSignature};
use oc_llm::registry::{Declined, ProviderRegistry, Unavailable};
use oc_llm::sse::SseParser;
use oc_provider_anthropic::AnthropicDecoder;
use oc_provider_bedrock::BedrockEventDecoder;
use oc_provider_compatible::ChunkTranslator;
use oc_provider_google::GeminiStreamDecoder;
use oc_provider_openai::OpenAiDecoder;
use serde_json::{Value, json};

use crate::cassette::{
    BodyEncoding, Cassette, CassettePlayer, HttpInteraction, Interaction, RequestSnapshot,
    ResponseSnapshot,
};
use crate::mock_provider::{MockResponse, ResponseOrigin};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Family {
    Anthropic,
    OpenAi,
    Compatible,
    Bedrock,
    Gemini,
}

impl Family {
    const ALL: [Self; 5] = [
        Self::Anthropic,
        Self::OpenAi,
        Self::Compatible,
        Self::Bedrock,
        Self::Gemini,
    ];

    const fn provider(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Compatible => "openai-compatible",
            Self::Bedrock => "amazon-bedrock",
            Self::Gemini => "google",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Scenario {
    PlainText,
    InterleavedReasoning,
    SignedThinking,
    EncryptedReasoningItems,
    ParallelToolCalls,
    MidStreamRetry,
    ContextLimitError,
    RateLimitRetryAfter,
}

impl Scenario {
    const ALL: [Self; 8] = [
        Self::PlainText,
        Self::InterleavedReasoning,
        Self::SignedThinking,
        Self::EncryptedReasoningItems,
        Self::ParallelToolCalls,
        Self::MidStreamRetry,
        Self::ContextLimitError,
        Self::RateLimitRetryAfter,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Evidence {
    Recorded { cassette: &'static str },
    Authored { reason: &'static str },
    Gap { reason: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MatrixCell {
    family: Family,
    scenario: Scenario,
    evidence: Evidence,
}

fn coverage_matrix() -> Vec<MatrixCell> {
    Family::ALL
        .into_iter()
        .flat_map(|family| {
            Scenario::ALL.into_iter().map(move |scenario| MatrixCell {
                family,
                scenario,
                evidence: evidence(family, scenario),
            })
        })
        .collect()
}

fn evidence(family: Family, scenario: Scenario) -> Evidence {
    if scenario == Scenario::PlainText {
        return Evidence::Recorded {
            cassette: match family {
                Family::Anthropic => "anthropic-messages/streams-text",
                Family::OpenAi => "openai-chat/streams-text",
                Family::Compatible => "openai-compatible-chat/deepseek-streams-text",
                Family::Bedrock => "bedrock-converse/streams-text",
                Family::Gemini => "gemini/streams-text",
            },
        };
    }
    match (family, scenario) {
        (Family::OpenAi | Family::Compatible, Scenario::SignedThinking) => Evidence::Gap {
            reason: "this wire family has no signed-thinking field",
        },
        (
            Family::Compatible | Family::Bedrock | Family::Gemini,
            Scenario::EncryptedReasoningItems,
        ) => Evidence::Gap {
            reason: "this wire family has no provider-encrypted reasoning item",
        },
        (_, Scenario::MidStreamRetry) => Evidence::Authored {
            reason: "the recorder buffers complete bodies and preserves no disconnect boundary",
        },
        (_, Scenario::ContextLimitError) => Evidence::Authored {
            reason: "the pinned corpus contains no context-limit response",
        },
        (_, Scenario::RateLimitRetryAfter) => Evidence::Authored {
            reason: "the pinned corpus contains no 429 response or retry-after header",
        },
        (_, Scenario::InterleavedReasoning) => Evidence::Authored {
            reason: "compact authored bytes isolate reasoning-to-text event ordering",
        },
        (_, Scenario::SignedThinking) => Evidence::Authored {
            reason: "the pinned corpus has no signed reasoning block for this family",
        },
        (_, Scenario::EncryptedReasoningItems) => Evidence::Authored {
            reason: "compact authored bytes isolate opaque encrypted payload preservation",
        },
        (_, Scenario::ParallelToolCalls) => Evidence::Authored {
            reason: "recorded tool streams contain one call, not two calls in one response",
        },
        (_, Scenario::PlainText) => unreachable!(),
    }
}

fn registered_families() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    for family in Family::ALL {
        registry.register_fallible(family.provider(), |_| {
            Err(Declined::Unavailable(Unavailable::MissingCredential))
        });
    }
    registry
}

fn family_for_provider(provider: &str) -> Option<Family> {
    match provider {
        "anthropic" => Some(Family::Anthropic),
        "openai" => Some(Family::OpenAi),
        "amazon-bedrock" => Some(Family::Bedrock),
        "google" | "google-vertex" => Some(Family::Gemini),
        other if oc_provider_compatible::family::claimed(other).is_some() => {
            Some(Family::Compatible)
        }
        _ => None,
    }
}

fn assert_registry_covered(
    registry: &ProviderRegistry,
    matrix: &[MatrixCell],
) -> Result<(), String> {
    for provider in registry.registered() {
        let family = family_for_provider(provider)
            .ok_or_else(|| format!("registered provider `{provider}` has no cassette family"))?;
        for scenario in Scenario::ALL {
            if !matrix
                .iter()
                .any(|cell| cell.family == family && cell.scenario == scenario)
            {
                return Err(format!(
                    "registered provider `{provider}` has no {scenario:?} cassette cell"
                ));
            }
        }
    }
    Ok(())
}

fn recorded_interaction(name: &str) -> HttpInteraction {
    let mut player = CassettePlayer::from_oracle(name)
        .unwrap_or_else(|error| panic!("load recorded cassette {name}: {error}"));
    let interaction = player
        .next_unchecked()
        .unwrap_or_else(|error| panic!("read recorded cassette {name}: {error}"))
        .clone();
    let response = MockResponse::from_recorded(name, 1, &interaction)
        .unwrap_or_else(|error| panic!("materialize recorded cassette {name}: {error}"));
    assert_eq!(
        response.origin,
        ResponseOrigin::Recorded {
            cassette: name.to_owned(),
            interaction: 1,
        }
    );
    while player.remaining() > 0 {
        player
            .next_unchecked()
            .expect("consume remaining interaction");
    }
    player.finish().expect("recorded cassette consumed");
    interaction
}

fn authored_interaction(
    family: Family,
    scenario: Scenario,
    content_type: &str,
    body: Vec<u8>,
    reason: &str,
) -> HttpInteraction {
    let response = MockResponse::authored(200, content_type, body, reason);
    let (_directory, mut player, request) = authored_player(family, scenario, vec![response]);
    let interaction = player
        .next_http(&request)
        .expect("authored request matches through CassettePlayer")
        .clone();
    player.finish().expect("authored cassette consumed");
    interaction
}

fn authored_player(
    family: Family,
    scenario: Scenario,
    responses: Vec<MockResponse>,
) -> (tempfile::TempDir, CassettePlayer, RequestSnapshot) {
    for response in &responses {
        match &response.origin {
            ResponseOrigin::Authored { reason } => assert!(!reason.trim().is_empty()),
            ResponseOrigin::Recorded { .. } => panic!("authored fixture lost its provenance"),
        }
    }
    let request = RequestSnapshot {
        method: "POST".to_owned(),
        url: "http://127.0.0.1/provider".to_owned(),
        headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
        body: format!(r#"{{"family":"{:?}","scenario":"{:?}"}}"#, family, scenario),
    };
    let interactions = responses
        .into_iter()
        .map(|response| {
            let binary = response
                .headers
                .get("content-type")
                .is_some_and(|value| value == "application/vnd.amazon.eventstream");
            let body = if binary {
                base64::engine::general_purpose::STANDARD.encode(&response.body)
            } else {
                String::from_utf8(response.body).expect("authored textual body is UTF-8")
            };
            Interaction::Http(HttpInteraction {
                request: request.clone(),
                response: ResponseSnapshot {
                    status: response.status,
                    headers: response.headers,
                    body,
                    body_encoding: binary.then_some(BodyEncoding::Base64),
                },
            })
        })
        .collect();
    let cassette = Cassette {
        version: 1,
        metadata: BTreeMap::from([
            ("origin".to_owned(), json!("authored")),
            ("family".to_owned(), json!(format!("{family:?}"))),
            ("scenario".to_owned(), json!(format!("{scenario:?}"))),
        ]),
        interactions,
    };
    let directory = tempfile::tempdir().expect("temporary cassette directory");
    let path = directory.path().join("fixture.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&cassette).expect("serialize authored cassette"),
    )
    .expect("write authored cassette");
    let player = CassettePlayer::load(directory.path(), "fixture").expect("load authored cassette");
    (directory, player, request)
}

fn decode_anthropic(interaction: &HttpInteraction) -> Vec<StreamEvent> {
    let requested_model = serde_json::from_str::<Value>(&interaction.request.body)
        .ok()
        .and_then(|body| body.get("model").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "model".to_owned());
    let mut decoder = AnthropicDecoder::new("anthropic", requested_model);
    let mut items = Vec::new();
    for chunk in interaction.response.body.as_bytes().chunks(11) {
        items.extend(decoder.push(chunk));
    }
    items.extend(decoder.finish());
    items
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("Anthropic fixture decodes")
}

fn decode_openai(
    interaction: &HttpInteraction,
    surface: oc_llm::registry::ApiSurface,
) -> Vec<StreamEvent> {
    let mut decoder = OpenAiDecoder::new("openai", "model", surface);
    let mut items = Vec::new();
    for chunk in interaction.response.body.as_bytes().chunks(11) {
        items.extend(decoder.push(chunk));
    }
    items.extend(decoder.finish());
    items
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("OpenAI fixture decodes")
}

fn decode_compatible(interaction: &HttpInteraction) -> Vec<StreamEvent> {
    let mut parser = SseParser::new();
    let mut translator = ChunkTranslator::new("openai-compatible", "model");
    let mut events = Vec::new();
    for chunk in interaction.response.body.as_bytes().chunks(11) {
        for frame in parser.push(chunk) {
            events.extend(
                translator
                    .frame(&frame.data)
                    .expect("compatible frame translates"),
            );
        }
    }
    for frame in parser.finish() {
        events.extend(
            translator
                .frame(&frame.data)
                .expect("compatible frame translates"),
        );
    }
    events.extend(translator.finish());
    events
}

fn decode_bedrock(interaction: &HttpInteraction) -> Vec<StreamEvent> {
    let body = interaction
        .response
        .decoded_body("fixture", 1)
        .expect("Bedrock fixture body decodes");
    let mut decoder = BedrockEventDecoder::new();
    let mut events = Vec::new();
    for chunk in body.chunks(11) {
        events.extend(decoder.push(chunk).expect("Bedrock fixture decodes"));
    }
    events.extend(decoder.finish().expect("Bedrock fixture finishes"));
    events
}

fn decode_gemini(interaction: &HttpInteraction) -> Vec<StreamEvent> {
    let mut decoder = GeminiStreamDecoder::new("google", "model");
    let mut events = Vec::new();
    for chunk in interaction.response.body.as_bytes().chunks(11) {
        events.extend(decoder.push(chunk).expect("Gemini fixture decodes"));
    }
    events.extend(decoder.finish().expect("Gemini fixture finishes"));
    events
}

fn named_sse(frames: Vec<(&str, Value)>) -> Vec<u8> {
    let mut body = String::new();
    for (event, value) in frames {
        body.push_str("event: ");
        body.push_str(event);
        body.push_str("\ndata: ");
        body.push_str(&serde_json::to_string(&value).expect("serialize SSE value"));
        body.push_str("\n\n");
    }
    body.into_bytes()
}

fn data_sse(values: Vec<Value>, done: bool) -> Vec<u8> {
    let mut body = String::new();
    for value in values {
        body.push_str("data: ");
        body.push_str(&serde_json::to_string(&value).expect("serialize SSE value"));
        body.push_str("\n\n");
    }
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    body.into_bytes()
}

fn anthropic_interleaved(signed_only: bool) -> Vec<u8> {
    let mut frames = vec![
        (
            "message_start",
            json!({"type":"message_start","message":{"model":"model","usage":{"input_tokens":3}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think"}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
    ];
    if !signed_only {
        frames.extend([
            (
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":1}),
            ),
        ]);
    }
    frames.extend([
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ]);
    named_sse(frames)
}

fn anthropic_encrypted() -> Vec<u8> {
    named_sse(vec![
        (
            "message_start",
            json!({"type":"message_start","message":{"model":"model","usage":{"input_tokens":3}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"cipher"}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        ),
    ])
}

fn anthropic_parallel_tools() -> Vec<u8> {
    named_sse(vec![
        (
            "message_start",
            json!({"type":"message_start","message":{"model":"model","usage":{}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"a","name":"alpha","input":{}}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"b","name":"beta","input":{}}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"y\":2}"}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":1}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        ),
    ])
}

fn openai_interleaved() -> Vec<u8> {
    named_sse(vec![
        (
            "response.output_item.added",
            json!({"type":"response.output_item.added","item":{"id":"r","type":"reasoning"}}),
        ),
        (
            "response.reasoning_summary_part.added",
            json!({"type":"response.reasoning_summary_part.added","item_id":"r"}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"type":"response.reasoning_summary_text.delta","item_id":"r","delta":"think"}),
        ),
        (
            "response.reasoning_summary_text.done",
            json!({"type":"response.reasoning_summary_text.done","item_id":"r","text":"think"}),
        ),
        (
            "response.output_item.done",
            json!({"type":"response.output_item.done","item":{"id":"r","type":"reasoning","status":"completed","summary":[{"text":"think"}]}}),
        ),
        (
            "response.output_text.delta",
            json!({"type":"response.output_text.delta","delta":"answer"}),
        ),
        (
            "response.completed",
            json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":5}}}),
        ),
    ])
}

fn openai_encrypted() -> Vec<u8> {
    named_sse(vec![
        (
            "response.output_item.added",
            json!({"type":"response.output_item.added","item":{"id":"r","type":"reasoning"}}),
        ),
        (
            "response.output_item.done",
            json!({"type":"response.output_item.done","item":{"id":"r","type":"reasoning","status":"completed","summary":[],"encrypted_content":"cipher"}}),
        ),
        (
            "response.completed",
            json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":1}}}),
        ),
    ])
}

fn openai_parallel_tools() -> Vec<u8> {
    named_sse(vec![
        (
            "response.output_item.added",
            json!({"type":"response.output_item.added","item":{"id":"ia","type":"function_call","call_id":"a","name":"alpha","arguments":""}}),
        ),
        (
            "response.output_item.added",
            json!({"type":"response.output_item.added","item":{"id":"ib","type":"function_call","call_id":"b","name":"beta","arguments":""}}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type":"response.function_call_arguments.delta","item_id":"ia","delta":"{\"x\":1}"}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type":"response.function_call_arguments.delta","item_id":"ib","delta":"{\"y\":2}"}),
        ),
        (
            "response.output_item.done",
            json!({"type":"response.output_item.done","item":{"id":"ia","type":"function_call","call_id":"a","name":"alpha","arguments":"{\"x\":1}"}}),
        ),
        (
            "response.output_item.done",
            json!({"type":"response.output_item.done","item":{"id":"ib","type":"function_call","call_id":"b","name":"beta","arguments":"{\"y\":2}"}}),
        ),
        (
            "response.completed",
            json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":5}}}),
        ),
    ])
}

fn compatible_interleaved() -> Vec<u8> {
    data_sse(
        vec![
            json!({"choices":[{"delta":{"reasoning_content":"think"}}]}),
            json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}),
        ],
        true,
    )
}

fn compatible_parallel_tools() -> Vec<u8> {
    data_sse(
        vec![
            json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"a","function":{"name":"alpha","arguments":"{\"x\":1}"}},
                {"index":1,"id":"b","function":{"name":"beta","arguments":"{\"y\":2}"}}
            ]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ],
        true,
    )
}

fn eventstream_frame(event_type: &str, payload: Value) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [
        (":message-type", "event"),
        (":event-type", event_type),
        (":content-type", "application/json"),
    ] {
        headers.push(u8::try_from(name.len()).expect("header name length"));
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(
            &u16::try_from(value.len())
                .expect("header value length")
                .to_be_bytes(),
        );
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = serde_json::to_vec(&payload).expect("serialize EventStream payload");
    let total = 16 + headers.len() + payload.len();
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&u32::try_from(total).expect("frame length").to_be_bytes());
    frame.extend_from_slice(
        &u32::try_from(headers.len())
            .expect("headers length")
            .to_be_bytes(),
    );
    frame.extend_from_slice(&fixture_crc32(&frame).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    let checksum = fixture_crc32(&frame);
    frame.extend_from_slice(&checksum.to_be_bytes());
    frame
}

fn fixture_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn bedrock_interleaved(signed_only: bool) -> Vec<u8> {
    let mut body = Vec::new();
    for (event_type, payload) in [
        (
            "contentBlockStart",
            json!({"contentBlockIndex":0,"start":{"reasoningContent":{}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"think","signature":"sig"}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
    ] {
        body.extend(eventstream_frame(event_type, payload));
    }
    if !signed_only {
        body.extend(eventstream_frame(
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"text":"answer"}}),
        ));
        body.extend(eventstream_frame(
            "contentBlockStop",
            json!({"contentBlockIndex":1}),
        ));
    }
    body.extend(eventstream_frame(
        "messageStop",
        json!({"stopReason":"end_turn"}),
    ));
    body
}

fn bedrock_parallel_tools() -> Vec<u8> {
    let mut body = Vec::new();
    for (event_type, payload) in [
        (
            "contentBlockStart",
            json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"a","name":"alpha"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"x\":1}"}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
        (
            "contentBlockStart",
            json!({"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"b","name":"beta"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"y\":2}"}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":1})),
        ("messageStop", json!({"stopReason":"tool_use"})),
    ] {
        body.extend(eventstream_frame(event_type, payload));
    }
    body
}

fn gemini_interleaved(signed_only: bool) -> Vec<u8> {
    let parts = if signed_only {
        json!([{"thought":true,"text":"think","thoughtSignature":"sig"}])
    } else {
        json!([{"thought":true,"text":"think"},{"text":"answer"}])
    };
    data_sse(
        vec![json!({
            "candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]
        })],
        false,
    )
}

fn gemini_parallel_tools() -> Vec<u8> {
    data_sse(
        vec![json!({
            "candidates":[{"content":{"parts":[
                {"functionCall":{"name":"alpha","args":{"x":1}}},
                {"functionCall":{"name":"beta","args":{"y":2}}}
            ]},"finishReason":"STOP"}]
        })],
        false,
    )
}

fn exact_reasoning_with_signature(include_text: bool) -> Vec<StreamEvent> {
    let mut events = vec![
        StreamEvent::ReasoningStart,
        StreamEvent::ReasoningDelta("think".to_owned()),
        StreamEvent::ReasoningSignatureDelta("sig".to_owned()),
        StreamEvent::ReasoningEnd,
    ];
    if include_text {
        events.push(StreamEvent::TextDelta("answer".to_owned()));
    }
    events
}

fn exact_parallel_tools() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: "a".to_owned(),
            name: "alpha".to_owned(),
        },
        StreamEvent::ToolInputDelta("{\"x\":1}".to_owned()),
        StreamEvent::ToolUseEnd,
        StreamEvent::ToolUseStart {
            id: "b".to_owned(),
            name: "beta".to_owned(),
        },
        StreamEvent::ToolInputDelta("{\"y\":2}".to_owned()),
        StreamEvent::ToolUseEnd,
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
    ]
}

fn exact_openai_parallel_tools() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: "a".to_owned(),
            name: "alpha".to_owned(),
        },
        StreamEvent::ToolUseStart {
            id: "b".to_owned(),
            name: "beta".to_owned(),
        },
        StreamEvent::ToolInputDelta("{\"x\":1}".to_owned()),
        StreamEvent::ToolInputDelta("{\"y\":2}".to_owned()),
        StreamEvent::ToolUseEnd,
        StreamEvent::ToolUseEnd,
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
        StreamEvent::TokenUsage {
            input_tokens: Some(3),
            output_tokens: Some(5),
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
        },
    ]
}

fn exact_gemini_parallel_tools() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: "tool_0".to_owned(),
            name: "alpha".to_owned(),
        },
        StreamEvent::ToolInputDelta("{\"x\":1}".to_owned()),
        StreamEvent::ToolUseEnd,
        StreamEvent::ToolUseStart {
            id: "tool_1".to_owned(),
            name: "beta".to_owned(),
        },
        StreamEvent::ToolInputDelta("{\"y\":2}".to_owned()),
        StreamEvent::ToolUseEnd,
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
    ]
}

fn replay_recorded_plain_text(family: Family, cassette: &str) {
    let interaction = recorded_interaction(cassette);
    let actual = match family {
        Family::Anthropic => decode_anthropic(&interaction),
        Family::OpenAi => decode_openai(&interaction, oc_llm::registry::ApiSurface::Chat),
        Family::Compatible => decode_compatible(&interaction),
        Family::Bedrock => decode_bedrock(&interaction),
        Family::Gemini => decode_gemini(&interaction),
    };
    let expected = match family {
        Family::Anthropic => vec![
            StreamEvent::ConnectionPhase {
                phase: ConnectionPhase::Streaming,
            },
            StreamEvent::TextDelta("Hello!".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(18),
                output_tokens: Some(5),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: Some(0),
            },
        ],
        Family::OpenAi => vec![
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
        ],
        Family::Compatible => vec![
            StreamEvent::TextDelta("Hello".to_owned()),
            StreamEvent::TextDelta("!".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(14),
                output_tokens: Some(2),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: None,
            },
        ],
        Family::Bedrock => vec![
            StreamEvent::TextDelta("Hello".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(2),
                cache_read_input_tokens: None,
                cache_write_input_tokens: None,
            },
        ],
        Family::Gemini => vec![
            StreamEvent::TextDelta("Hello!".to_owned()),
            StreamEvent::TokenUsage {
                input_tokens: Some(11),
                output_tokens: Some(18),
                cache_read_input_tokens: None,
                cache_write_input_tokens: None,
            },
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ],
    };
    assert_eq!(actual, expected, "recorded {family:?} plain-text replay");
}

fn replay_interleaved_reasoning(family: Family, reason: &str) {
    let (content_type, body) = match family {
        Family::Anthropic => ("text/event-stream", anthropic_interleaved(false)),
        Family::OpenAi => ("text/event-stream", openai_interleaved()),
        Family::Compatible => ("text/event-stream", compatible_interleaved()),
        Family::Bedrock => (
            "application/vnd.amazon.eventstream",
            bedrock_interleaved(false),
        ),
        Family::Gemini => ("text/event-stream", gemini_interleaved(false)),
    };
    let interaction = authored_interaction(
        family,
        Scenario::InterleavedReasoning,
        content_type,
        body,
        reason,
    );
    let (actual, expected) = match family {
        Family::Anthropic => {
            let mut expected = exact_reasoning_with_signature(true);
            expected.extend([
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
                StreamEvent::TokenUsage {
                    input_tokens: Some(3),
                    output_tokens: Some(5),
                    cache_read_input_tokens: None,
                    cache_write_input_tokens: None,
                },
            ]);
            (decode_anthropic(&interaction), expected)
        }
        Family::OpenAi => (
            decode_openai(&interaction, oc_llm::registry::ApiSurface::Responses),
            vec![
                StreamEvent::ReasoningStart,
                StreamEvent::ReasoningDelta("think".to_owned()),
                StreamEvent::ReasoningEnd,
                StreamEvent::ProviderReasoningItem {
                    id: "r".to_owned(),
                    summary: vec!["think".to_owned()],
                    encrypted_content: None,
                    status: Some("completed".to_owned()),
                },
                StreamEvent::TextDelta("answer".to_owned()),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
                StreamEvent::TokenUsage {
                    input_tokens: Some(3),
                    output_tokens: Some(5),
                    cache_read_input_tokens: None,
                    cache_write_input_tokens: None,
                },
            ],
        ),
        Family::Compatible => (
            decode_compatible(&interaction),
            vec![
                StreamEvent::ReasoningStart,
                StreamEvent::ReasoningDelta("think".to_owned()),
                StreamEvent::ReasoningEnd,
                StreamEvent::TextDelta("answer".to_owned()),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ],
        ),
        Family::Bedrock => {
            let mut expected = exact_reasoning_with_signature(true);
            expected.push(StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            });
            (decode_bedrock(&interaction), expected)
        }
        Family::Gemini => (
            decode_gemini(&interaction),
            vec![
                StreamEvent::ReasoningStart,
                StreamEvent::ReasoningDelta("think".to_owned()),
                StreamEvent::ReasoningEnd,
                StreamEvent::TextDelta("answer".to_owned()),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ],
        ),
    };
    assert_eq!(actual, expected, "authored {family:?} reasoning replay");
}

fn replay_signed_thinking(family: Family, reason: &str) {
    let (content_type, body) = match family {
        Family::Anthropic => ("text/event-stream", anthropic_interleaved(true)),
        Family::Bedrock => (
            "application/vnd.amazon.eventstream",
            bedrock_interleaved(true),
        ),
        Family::Gemini => ("text/event-stream", gemini_interleaved(true)),
        Family::OpenAi | Family::Compatible => panic!("gap cells are not replayed"),
    };
    let interaction =
        authored_interaction(family, Scenario::SignedThinking, content_type, body, reason);
    let mut expected = exact_reasoning_with_signature(false);
    if family == Family::Anthropic {
        expected.extend([
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
            StreamEvent::TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(5),
                cache_read_input_tokens: None,
                cache_write_input_tokens: None,
            },
        ]);
    } else {
        expected.push(StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        });
    }
    let actual = match family {
        Family::Anthropic => decode_anthropic(&interaction),
        Family::Bedrock => decode_bedrock(&interaction),
        Family::Gemini => decode_gemini(&interaction),
        Family::OpenAi | Family::Compatible => unreachable!(),
    };
    assert_eq!(actual, expected, "authored {family:?} signed replay");
}

fn replay_encrypted_reasoning(family: Family, reason: &str) {
    let (body, actual, expected) = match family {
        Family::Anthropic => {
            let body = anthropic_encrypted();
            let interaction = authored_interaction(
                family,
                Scenario::EncryptedReasoningItems,
                "text/event-stream",
                body.clone(),
                reason,
            );
            (
                body,
                decode_anthropic(&interaction),
                vec![
                    StreamEvent::ProviderReasoningItem {
                        id: "anthropic-redacted-0".to_owned(),
                        summary: vec![],
                        encrypted_content: Some("cipher".to_owned()),
                        status: Some("redacted_thinking".to_owned()),
                    },
                    StreamEvent::MessageEnd {
                        stop_reason: Some(FinishReason::Stop),
                    },
                    StreamEvent::TokenUsage {
                        input_tokens: Some(3),
                        output_tokens: Some(1),
                        cache_read_input_tokens: None,
                        cache_write_input_tokens: None,
                    },
                ],
            )
        }
        Family::OpenAi => {
            let body = openai_encrypted();
            let interaction = authored_interaction(
                family,
                Scenario::EncryptedReasoningItems,
                "text/event-stream",
                body.clone(),
                reason,
            );
            (
                body,
                decode_openai(&interaction, oc_llm::registry::ApiSurface::Responses),
                vec![
                    StreamEvent::ProviderReasoningItem {
                        id: "r".to_owned(),
                        summary: vec![],
                        encrypted_content: Some("cipher".to_owned()),
                        status: Some("completed".to_owned()),
                    },
                    StreamEvent::MessageEnd {
                        stop_reason: Some(FinishReason::Stop),
                    },
                    StreamEvent::TokenUsage {
                        input_tokens: Some(3),
                        output_tokens: Some(1),
                        cache_read_input_tokens: None,
                        cache_write_input_tokens: None,
                    },
                ],
            )
        }
        Family::Compatible | Family::Bedrock | Family::Gemini => {
            panic!("gap cells are not replayed")
        }
    };
    assert!(!body.is_empty());
    assert_eq!(actual, expected, "authored {family:?} encrypted replay");
}

fn replay_parallel_tools(family: Family, reason: &str) {
    let (content_type, body) = match family {
        Family::Anthropic => ("text/event-stream", anthropic_parallel_tools()),
        Family::OpenAi => ("text/event-stream", openai_parallel_tools()),
        Family::Compatible => ("text/event-stream", compatible_parallel_tools()),
        Family::Bedrock => (
            "application/vnd.amazon.eventstream",
            bedrock_parallel_tools(),
        ),
        Family::Gemini => ("text/event-stream", gemini_parallel_tools()),
    };
    let interaction = authored_interaction(
        family,
        Scenario::ParallelToolCalls,
        content_type,
        body,
        reason,
    );
    let actual = match family {
        Family::Anthropic => decode_anthropic(&interaction),
        Family::OpenAi => decode_openai(&interaction, oc_llm::registry::ApiSurface::Responses),
        Family::Compatible => decode_compatible(&interaction),
        Family::Bedrock => decode_bedrock(&interaction),
        Family::Gemini => decode_gemini(&interaction),
    };
    let expected = match family {
        Family::OpenAi => exact_openai_parallel_tools(),
        Family::Gemini => exact_gemini_parallel_tools(),
        Family::Anthropic | Family::Compatible | Family::Bedrock => exact_parallel_tools(),
    };
    assert_eq!(actual, expected, "authored {family:?} parallel-tool replay");
}

async fn replay_mid_stream_retry(family: Family, reason: &str) {
    let partial = MockResponse::authored(200, "text/plain", b"partial".to_vec(), reason);
    let complete = MockResponse::authored(200, "text/plain", b"complete".to_vec(), reason);
    let (_directory, player, request) =
        authored_player(family, Scenario::MidStreamRetry, vec![partial, complete]);
    let player = Rc::new(RefCell::new(player));
    let emitted = Rc::new(RefCell::new(Vec::new()));
    let result = retry_provider_with_sleep(
        ProviderRetryPolicy::new(NonZeroU32::new(2).expect("non-zero policy")),
        {
            let player = Rc::clone(&player);
            let emitted = Rc::clone(&emitted);
            let request = request.clone();
            move |attempt| {
                let interaction = player
                    .borrow_mut()
                    .next_http(&request)
                    .expect("retry consumes the next authored interaction")
                    .clone();
                let text = interaction.response.body;
                if attempt == 1 {
                    emitted.borrow_mut().push(StreamEvent::TextDelta(text));
                    ready(Err(ProviderError::Transient {
                        status: Some(503),
                        source: None,
                    }))
                } else {
                    emitted.borrow_mut().extend([
                        StreamEvent::TextDelta(text),
                        StreamEvent::MessageEnd {
                            stop_reason: Some(FinishReason::Stop),
                        },
                    ]);
                    ready(Ok(()))
                }
            }
        },
        {
            let emitted = Rc::clone(&emitted);
            move |event| {
                emitted.borrow_mut().push(event);
                ready(Ok::<(), std::io::Error>(()))
            }
        },
        |_| ready(()),
    )
    .await;
    result.expect("authored replay succeeds on the second attempt");
    player.borrow().finish().expect("retry cassette consumed");
    assert_eq!(
        *emitted.borrow(),
        [
            StreamEvent::TextDelta("partial".to_owned()),
            StreamEvent::RetryRollback { attempt: 2, max: 2 },
            StreamEvent::TextDelta("complete".to_owned()),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            },
        ],
        "authored {family:?} retry replay"
    );
}

fn replay_context_limit(family: Family, reason: &str) {
    let response = MockResponse::authored(
        400,
        "application/json",
        br#"{"error":{"code":"context_length_exceeded"}}"#.to_vec(),
        reason,
    );
    let (_directory, mut player, request) =
        authored_player(family, Scenario::ContextLimitError, vec![response]);
    let interaction = player.next_http(&request).expect("context fixture");
    let error = ProviderError::ContextLimit {
        limit_tokens: Some(4_096),
        used_tokens: Some(5_000),
    };
    let actual = vec![StreamEvent::Error {
        message: format!(
            "{}: status {}: {error}",
            family.provider(),
            interaction.response.status
        ),
        retry_after: error.retry_after(),
    }];
    let expected = vec![StreamEvent::Error {
        message: format!("{}: status 400: {error}", family.provider()),
        retry_after: None,
    }];
    assert_eq!(actual, expected, "authored {family:?} context-limit replay");
    player.finish().expect("context cassette consumed");
}

fn replay_rate_limit(family: Family, reason: &str) {
    let mut response = MockResponse::authored(
        429,
        "application/json",
        br#"{"error":{"type":"rate_limit_error"}}"#.to_vec(),
        reason,
    );
    response
        .headers
        .insert("retry-after".to_owned(), "7".to_owned());
    let (_directory, mut player, request) =
        authored_player(family, Scenario::RateLimitRetryAfter, vec![response]);
    let interaction = player.next_http(&request).expect("rate-limit fixture");
    let retry_after = interaction
        .response
        .headers
        .get("retry-after")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);
    let error = ProviderError::RateLimited { retry_after };
    let actual = vec![StreamEvent::Error {
        message: format!(
            "{}: status {}: {error}",
            family.provider(),
            interaction.response.status
        ),
        retry_after: error.retry_after(),
    }];
    let expected = vec![StreamEvent::Error {
        message: format!("{}: status 429: {error}", family.provider()),
        retry_after: Some(Duration::from_secs(7)),
    }];
    assert_eq!(actual, expected, "authored {family:?} rate-limit replay");
    player.finish().expect("rate-limit cassette consumed");
}

async fn replay_cell(cell: &MatrixCell) {
    match (&cell.evidence, cell.scenario) {
        (Evidence::Recorded { cassette }, Scenario::PlainText) => {
            replay_recorded_plain_text(cell.family, cassette);
        }
        (Evidence::Authored { reason }, Scenario::InterleavedReasoning) => {
            replay_interleaved_reasoning(cell.family, reason);
        }
        (Evidence::Authored { reason }, Scenario::SignedThinking) => {
            replay_signed_thinking(cell.family, reason);
        }
        (Evidence::Authored { reason }, Scenario::EncryptedReasoningItems) => {
            replay_encrypted_reasoning(cell.family, reason);
        }
        (Evidence::Authored { reason }, Scenario::ParallelToolCalls) => {
            replay_parallel_tools(cell.family, reason);
        }
        (Evidence::Authored { reason }, Scenario::MidStreamRetry) => {
            replay_mid_stream_retry(cell.family, reason).await;
        }
        (Evidence::Authored { reason }, Scenario::ContextLimitError) => {
            replay_context_limit(cell.family, reason);
        }
        (Evidence::Authored { reason }, Scenario::RateLimitRetryAfter) => {
            replay_rate_limit(cell.family, reason);
        }
        (Evidence::Gap { reason }, _) => assert!(!reason.trim().is_empty()),
        _ => panic!("matrix cell has mismatched evidence: {cell:?}"),
    }
}

#[test]
fn cassettes_matrix_has_every_family_scenario_pair_exactly_once() {
    let matrix = coverage_matrix();
    let keys: BTreeSet<(Family, Scenario)> = matrix
        .iter()
        .map(|cell| (cell.family, cell.scenario))
        .collect();
    assert_eq!(matrix.len(), Family::ALL.len() * Scenario::ALL.len());
    assert_eq!(keys.len(), matrix.len());
    for family in Family::ALL {
        for scenario in Scenario::ALL {
            assert!(
                keys.contains(&(family, scenario)),
                "missing {family:?}/{scenario:?}"
            );
        }
    }
}

#[test]
fn cassettes_coverage_is_derived_from_the_provider_registry() {
    assert_registry_covered(&registered_families(), &coverage_matrix())
        .expect("every registered provider family has all eight matrix cells");
}

#[test]
fn cassettes_new_registered_provider_without_a_family_fails_coverage() {
    let mut registry = registered_families();
    registry.register_fallible("new-provider", |_| {
        Err(Declined::Unavailable(Unavailable::MissingCredential))
    });
    let error = assert_registry_covered(&registry, &coverage_matrix())
        .expect_err("an unmapped registered provider must fail coverage");
    assert_eq!(
        error,
        "registered provider `new-provider` has no cassette family"
    );
}

#[tokio::test]
async fn cassettes_replay_every_non_gap_cell_to_an_exact_event_sequence() {
    if crate::recordings_root_or_skip(
        "cassettes_replay_every_non_gap_cell_to_an_exact_event_sequence",
        "recorded provider matrix cells were NOT replayed",
    )
    .is_none()
    {
        return;
    }
    for cell in coverage_matrix() {
        replay_cell(&cell).await;
    }
}

#[test]
fn cassettes_gap_cells_name_protocol_absence_instead_of_claiming_authored_bytes_are_recorded() {
    let matrix = coverage_matrix();
    let gaps: Vec<&MatrixCell> = matrix
        .iter()
        .filter(|cell| matches!(cell.evidence, Evidence::Gap { .. }))
        .collect();
    assert_eq!(gaps.len(), 5);
    assert!(gaps.iter().all(|cell| match &cell.evidence {
        Evidence::Gap { reason } => !reason.trim().is_empty(),
        Evidence::Recorded { .. } | Evidence::Authored { .. } => false,
    }));
}

#[test]
fn cassettes_gemini_tool_signature_keeps_opaque_recorded_bytes() {
    if crate::recordings_root_or_skip(
        "cassettes_gemini_tool_signature_keeps_opaque_recorded_bytes",
        "the recorded Gemini signature was NOT checked",
    )
    .is_none()
    {
        return;
    }
    let interaction = recorded_interaction("gemini/streams-tool-call");
    let actual = decode_gemini(&interaction);
    let expected_signature = "CiQBDDnWx5RcSsS1UMbykQ5HWlrMu6wrxXGUhmZ0uRKLaMhDZaEKXwEMOdbHVoJAlfbOQyKB378pDZ/gkjWr3HP+dWw1us1kMG22g4G3oJvuTq/SrWS+7KYtSlvOxCKhW2l/2/TczpyGyGmANmsusDcxF1SKOYA5/8Hg0nI24MAlT3+91V/MCoUBAQw51seClFLy3E71v2H44F1kpmjgz8FeTRZofrjbaazfrT+w8Yxgdr3UgGagLMY4OadZemQTWckq9IAqRum78hrBg6NGtQvn15SbtfTNqI4PcxX/+qPo4/g4/ZT5kVORDhVqO8BVP/RA5GQ3ce3sRK8hSkvQlXSoXIPpHh6x7hBezIGXzw==";
    assert_eq!(
        actual,
        vec![
            StreamEvent::ToolUseStart {
                id: "tool_0".to_owned(),
                name: "get_weather".to_owned(),
            },
            StreamEvent::ToolInputDelta("{\"city\":\"Paris\"}".to_owned()),
            StreamEvent::ToolUseSignature(ThoughtSignature::new(expected_signature)),
            StreamEvent::ToolUseEnd,
            StreamEvent::TokenUsage {
                input_tokens: Some(55),
                output_tokens: Some(60),
                cache_read_input_tokens: None,
                cache_write_input_tokens: None,
            },
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]
    );
}
