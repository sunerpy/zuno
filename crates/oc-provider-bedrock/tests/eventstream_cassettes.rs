use oc_llm::event::{FinishReason, StreamEvent};
use oc_provider_bedrock::{BedrockDecodeError, BedrockEventDecoder, CrcKind, EventStreamError};
use oc_testkit::CassettePlayer;

fn recorded_body(name: &str, interaction_index: usize) -> Vec<u8> {
    let mut player = CassettePlayer::from_oracle(name).expect("recorded Bedrock cassette");
    let mut body = Vec::new();
    for index in 0..=interaction_index {
        body = player
            .next_unchecked()
            .expect("recorded interaction")
            .response
            .decoded_body(name, index + 1)
            .expect("base64 EventStream body");
    }
    body
}

fn decode(chunks: &[&[u8]]) -> Result<Vec<StreamEvent>, BedrockDecodeError> {
    let mut decoder = BedrockEventDecoder::new();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(decoder.push(chunk)?);
    }
    events.extend(decoder.finish()?);
    Ok(events)
}

#[test]
fn recorded_stream_decodes_identically_when_split_at_every_byte_offset() {
    let bytes = recorded_body("bedrock-converse/streams-text", 0);
    let expected = decode(&[&bytes]).expect("whole recording decodes");

    for split in 0..=bytes.len() {
        let actual = decode(&[&bytes[..split], &bytes[split..]])
            .unwrap_or_else(|error| panic!("split at byte {split} failed: {error}"));
        assert_eq!(actual, expected, "different output at byte split {split}");
    }

    assert_eq!(
        expected,
        vec![
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
        ]
    );
    println!("byte-split sweep offsets: {}", bytes.len() + 1);
}

#[test]
fn recorded_text_and_tool_conversations_replay_to_shared_events() {
    let text =
        decode(&[&recorded_body("bedrock-converse/streams-text", 0)]).expect("text conversation");
    assert!(matches!(text.first(), Some(StreamEvent::TextDelta(value)) if value == "Hello"));

    let tool = decode(&[&recorded_body("bedrock-converse/streams-a-tool-call", 0)])
        .expect("tool conversation");
    assert!(matches!(
        tool.as_slice(),
        [
            StreamEvent::ToolUseStart { id, name },
            StreamEvent::ToolInputDelta(input),
            StreamEvent::ToolUseEnd,
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
            StreamEvent::TokenUsage { .. },
        ] if id == "tooluse_6a1pPvnc99GLKO3KGkUA2N"
            && name == "get_weather"
            && input == "{\"city\":\"Paris\"}"
    ));

    let first_loop_turn = decode(&[&recorded_body("bedrock-converse/drives-a-tool-loop", 0)])
        .expect("first tool-loop turn");
    let second_loop_turn = decode(&[&recorded_body("bedrock-converse/drives-a-tool-loop", 1)])
        .expect("second tool-loop turn");
    assert!(
        first_loop_turn
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolUseStart { .. }))
    );
    assert!(
        second_loop_turn
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(_)))
    );
}

#[test]
fn corrupted_message_crc_is_typed_and_names_its_absolute_offset() {
    let mut bytes = recorded_body("bedrock-converse/streams-text", 0);
    let first_frame_len = u32::from_be_bytes(bytes[0..4].try_into().expect("four bytes")) as usize;
    let crc_offset = first_frame_len - 4;
    bytes[crc_offset] ^= 0x80;

    let error = decode(&[&bytes]).expect_err("corrupt CRC must fail");
    let BedrockDecodeError::Framing(EventStreamError::CrcMismatch {
        kind,
        offset,
        expected: _,
        actual: _,
    }) = &error
    else {
        panic!("expected a typed message CRC error, got {error:?}");
    };
    assert_eq!(*kind, CrcKind::Message);
    assert_eq!(*offset, crc_offset);
    assert!(
        error
            .to_string()
            .contains(&format!("byte offset {crc_offset}"))
    );
    println!("corrupted CRC transcript: {error}");
}
