use std::collections::HashSet;
use std::error::Error as _;
use std::mem::discriminant;

use serde_json::{Value, json};
use zuno_llm::event::{
    ConnectionPhase, ContentBlock, PromptAccounting, RequestContentBlock, Role, StreamEvent,
    ThoughtSignature, TranscriptMessage, tool_arguments_text,
};
use zuno_llm::stream::StreamAccumulator;

#[test]
fn event_history_only_trace_is_stored_but_excluded_from_the_outbound_request() {
    let transcript = TranscriptMessage::new(
        Role::Assistant,
        vec![
            ContentBlock::ReasoningTrace {
                text: "history-only chain".to_owned(),
            },
            ContentBlock::Text {
                text: "answer".to_owned(),
            },
        ],
    );

    let request = transcript.to_request();

    assert_eq!(transcript.content.len(), 2);
    assert_eq!(
        transcript.content[0],
        ContentBlock::ReasoningTrace {
            text: "history-only chain".to_owned(),
        }
    );
    assert_eq!(
        request.content,
        vec![RequestContentBlock::Text {
            text: "answer".to_owned(),
        }]
    );
}

#[test]
fn resource_link_round_trips_and_has_one_stable_provider_projection() {
    let link = RequestContentBlock::ResourceLink {
        name: "notes.md".to_owned(),
        uri: "file:///workspace/notes.md".to_owned(),
        title: Some("Design notes".to_owned()),
        description: Some("ACP design context".to_owned()),
        media_type: Some("text/markdown".to_owned()),
        size: Some(42),
    };

    let encoded = serde_json::to_string(&link).expect("resource link serializes");
    let decoded: RequestContentBlock =
        serde_json::from_str(&encoded).expect("resource link deserializes");

    assert_eq!(decoded, link);
    assert_eq!(
        link.provider_text().as_deref(),
        Some(
            "Referenced resource `Design notes` (name: `notes.md`): file:///workspace/notes.md\n\
             Description: ACP design context\n\
             Media type: text/markdown\n\
             Size: 42 bytes"
        )
    );
}

#[test]
fn event_signed_thinking_round_trips_through_storage_and_into_a_request() {
    let stored = TranscriptMessage::new(
        Role::Assistant,
        vec![ContentBlock::SignedThinking {
            thinking: "check both branches".to_owned(),
            signature: "anthropic-signature-7".to_owned(),
        }],
    );
    let encoded = serde_json::to_string(&stored).expect("transcript serializes");
    let decoded: TranscriptMessage =
        serde_json::from_str(&encoded).expect("transcript deserializes");

    let request = decoded.to_request();

    assert_eq!(
        request.content,
        vec![RequestContentBlock::SignedThinking {
            thinking: "check both branches".to_owned(),
            signature: "anthropic-signature-7".to_owned(),
        }]
    );
    println!("stored={encoded}");
    println!("outbound={:?}", request.content);
}

#[test]
fn event_unsigned_reasoning_cannot_enter_the_generic_outbound_request_type() {
    let transcript = TranscriptMessage::new(
        Role::Assistant,
        vec![ContentBlock::Reasoning {
            text: "unsigned thinking".to_owned(),
        }],
    );

    let request = transcript.to_request();

    assert!(request.content.is_empty());
    assert_eq!(transcript.content.len(), 1);
    println!("stored={:?}", transcript.content);
    println!("outbound={:?}", request.content);
}

#[test]
fn event_per_tool_thought_signature_survives_storage_and_request_conversion() {
    let stored = TranscriptMessage::new(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call-7".to_owned(),
            name: "read".to_owned(),
            input: json!({ "path": "README.md" }),
            raw_arguments: None,
            thought_signature: Some(ThoughtSignature::new("gemini-signature-7")),
        }],
    );
    let encoded = serde_json::to_string(&stored).expect("transcript serializes");
    let decoded: TranscriptMessage =
        serde_json::from_str(&encoded).expect("transcript deserializes");

    assert_eq!(decoded.to_request().content.len(), 1);
    assert!(matches!(
        &decoded.to_request().content[0],
        RequestContentBlock::ToolUse {
            thought_signature: Some(signature),
            ..
        } if signature.as_str() == "gemini-signature-7"
    ));
}

#[test]
fn event_vocabulary_has_twenty_four_distinct_variants() {
    let events = [
        StreamEvent::TextDelta(String::new()),
        StreamEvent::ToolUseStart {
            id: String::new(),
            name: String::new(),
        },
        StreamEvent::ToolInputDelta {
            id: String::new(),
            delta: String::new(),
        },
        StreamEvent::ToolUseEnd { id: String::new() },
        StreamEvent::ToolUseSignature {
            id: String::new(),
            signature: ThoughtSignature::new("signature"),
        },
        StreamEvent::ToolResult {
            tool_use_id: String::new(),
            content: String::new(),
            is_error: false,
        },
        StreamEvent::GeneratedImage {
            id: String::new(),
            path: String::new(),
            metadata_path: None,
            output_format: String::new(),
            revised_prompt: None,
        },
        StreamEvent::ReasoningStart,
        StreamEvent::ReasoningDelta(String::new()),
        StreamEvent::ReasoningSignatureDelta(String::new()),
        StreamEvent::ProviderReasoningItem {
            id: String::new(),
            summary: Vec::new(),
            encrypted_content: None,
            status: None,
        },
        StreamEvent::ReasoningEnd,
        StreamEvent::ReasoningDone { duration_secs: 0.0 },
        StreamEvent::MessageEnd { stop_reason: None },
        StreamEvent::RetryRollback { attempt: 1, max: 1 },
        StreamEvent::TokenUsage {
            input_tokens: None,
            output_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        },
        StreamEvent::ConnectionType {
            connection: String::new(),
        },
        StreamEvent::ConnectionPhase {
            phase: ConnectionPhase::Connecting,
        },
        StreamEvent::StatusDetail {
            detail: String::new(),
        },
        StreamEvent::Error {
            message: String::new(),
            retry_after: None,
        },
        StreamEvent::SessionId(String::new()),
        StreamEvent::Compaction {
            trigger: String::new(),
            pre_tokens: None,
            openai_encrypted_content: None,
        },
        StreamEvent::UpstreamProvider {
            provider: String::new(),
        },
        StreamEvent::NativeToolCall {
            request_id: String::new(),
            tool_name: String::new(),
            input: Value::Null,
        },
    ];
    let variants: HashSet<_> = events.iter().map(discriminant).collect();

    assert_eq!(events.len(), 24);
    assert_eq!(variants.len(), 24);
}

#[test]
fn retry_rollback_event_clears_text_and_tool_call_accumulators() {
    let mut accumulator = StreamAccumulator::new();
    accumulator
        .apply(&StreamEvent::TextDelta("partial answer".to_owned()))
        .expect("text remains within stream limits");
    accumulator
        .apply(&StreamEvent::ToolUseStart {
            id: "call-1".to_owned(),
            name: "shell".to_owned(),
        })
        .expect("tool start remains within stream limits");
    accumulator
        .apply(&StreamEvent::ToolInputDelta {
            id: "call-1".to_owned(),
            delta: r#"{"command":"cargo test"}"#.to_owned(),
        })
        .expect("tool input remains within stream limits");
    accumulator
        .apply(&StreamEvent::ReasoningDelta("partial reasoning".to_owned()))
        .expect("reasoning remains within stream limits");

    assert_eq!(accumulator.text(), "partial answer");
    assert_eq!(accumulator.tool_calls().len(), 1);
    assert_eq!(
        accumulator.tool_calls()[0].raw_input,
        r#"{"command":"cargo test"}"#
    );

    accumulator
        .apply(&StreamEvent::RetryRollback { attempt: 2, max: 3 })
        .expect("rollback remains within stream limits");

    assert!(accumulator.text().is_empty());
    assert!(accumulator.tool_calls().is_empty());
    assert!(accumulator.reasoning().is_empty());
    assert!(accumulator.reasoning_signature().is_empty());
    assert!(accumulator.is_empty());
    println!(
        "after RetryRollback(attempt=2,max=3): text={:?}, tool_calls={:?}, reasoning={:?}",
        accumulator.text(),
        accumulator.tool_calls(),
        accumulator.reasoning()
    );
}

#[test]
fn tool_input_accumulator_rejects_json_over_its_cap() {
    let mut accumulator = StreamAccumulator::with_limits("openai", "tool-stream", 8);
    accumulator
        .apply(&StreamEvent::ToolUseStart {
            id: "call-1".to_owned(),
            name: "write".to_owned(),
        })
        .expect("tool call starts");

    let error = accumulator
        .apply(&StreamEvent::ToolInputDelta {
            id: "call-1".to_owned(),
            delta: "123456789".to_owned(),
        })
        .expect_err("the ninth byte must exceed an eight-byte cap");
    let detail = error
        .source()
        .expect("the provider error retains the stream limit detail")
        .to_string();

    assert!(detail.contains("openai"), "{detail}");
    assert!(detail.contains("tool-stream"), "{detail}");
    assert!(detail.contains("9 bytes"), "{detail}");
    assert!(detail.contains("limit 8 bytes"), "{detail}");
    assert!(accumulator.tool_calls()[0].raw_input.is_empty());
}

#[test]
fn tool_arguments_replay_the_provider_bytes_when_they_decode_to_the_same_value() {
    let input = json!({ "command": "ls -a", "intent": "list" });
    let raw = r#"{"command": "ls -a", "intent": "list"}"#;

    let replayed = tool_arguments_text(&input, Some(raw));

    assert_eq!(replayed, raw);
    assert_ne!(
        replayed,
        input.to_string(),
        "a re-serialization would drop the separators the endpoint fingerprinted"
    );
}

#[test]
fn tool_arguments_replay_the_provider_key_order_not_the_sorted_order() {
    let raw = r#"{"intent":"list","command":"ls -a"}"#;
    let input: Value = serde_json::from_str(raw).expect("provider bytes decode");

    assert_eq!(tool_arguments_text(&input, Some(raw)), raw);
}

#[test]
fn tool_arguments_fall_back_to_the_decoded_call_when_the_bytes_disagree() {
    let input = json!({ "command": "ls -a" });
    // Bytes that decode to a different call than the one in history: a mispaired row,
    // never a tool hook, which rewrites the dispatcher's copy and leaves history alone.
    let unrelated_bytes = r#"{"command": "rm -rf /"}"#;

    let replayed = tool_arguments_text(&input, Some(unrelated_bytes));

    assert_eq!(replayed, input.to_string());
    assert!(
        !replayed.contains("rm -rf"),
        "the wire must carry the call durable history recorded: {replayed}"
    );
}

#[test]
fn tool_arguments_fall_back_to_the_executed_value_for_unusable_bytes() {
    let input = json!({ "command": "ls -a" });

    assert_eq!(
        tool_arguments_text(&input, Some(r#"{"command": "ls -a""#)),
        input.to_string(),
        "a truncated capture is not valid JSON"
    );
    assert_eq!(
        tool_arguments_text(&input, Some("")),
        input.to_string(),
        "an empty capture carries no arguments"
    );
    assert_eq!(
        tool_arguments_text(&input, None),
        input.to_string(),
        "a call with no captured bytes re-serializes the value"
    );
}

#[test]
fn event_tool_use_carries_the_provider_argument_bytes_into_the_request() {
    let raw = r#"{"command": "ls -a"}"#;
    let stored = TranscriptMessage::new(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call-9".to_owned(),
            name: "shell".to_owned(),
            input: json!({ "command": "ls -a" }),
            raw_arguments: Some(raw.to_owned()),
            thought_signature: None,
        }],
    );
    let encoded = serde_json::to_string(&stored).expect("transcript serializes");
    let decoded: TranscriptMessage =
        serde_json::from_str(&encoded).expect("transcript deserializes");

    let request = decoded.to_request();

    assert!(matches!(
        &request.content[0],
        RequestContentBlock::ToolUse {
            raw_arguments: Some(bytes),
            ..
        } if bytes == raw
    ));
}

#[test]
fn event_tool_use_without_provider_argument_bytes_stays_absent_on_the_wire_shape() {
    let stored = TranscriptMessage::new(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call-10".to_owned(),
            name: "shell".to_owned(),
            input: json!({ "command": "ls -a" }),
            raw_arguments: None,
            thought_signature: None,
        }],
    );

    let encoded = serde_json::to_string(&stored).expect("transcript serializes");

    assert!(
        !encoded.contains("raw_arguments"),
        "an absent capture must not grow every stored tool part: {encoded}"
    );
    let decoded: TranscriptMessage =
        serde_json::from_str(&encoded).expect("older stored rows decode without the field");
    assert_eq!(decoded, stored);
}
