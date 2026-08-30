use std::error::Error as _;
use std::future;
use std::time::Duration;

use zuno_error::ProviderError;
use zuno_llm::sse::{
    DEFAULT_STREAM_IDLE_TIMEOUT_SECS, SseEvent, SseParser, StreamIdleTimeout, StreamLimits,
    Utf8StreamDecoder, append_tool_input,
};
use zuno_testkit::{CassettePlayer, list_cassettes};

fn successful_events(
    items: impl IntoIterator<Item = Result<SseEvent, ProviderError>>,
) -> Vec<SseEvent> {
    items
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("SSE framing succeeds")
}

fn mixed_script_frame() -> (String, String) {
    let unit = "中文流式响应 preserves English, Ελληνικά, العربية, and emoji 🧠🚀。";
    let mut text = String::new();
    while text.len() < 4_096 {
        text.push_str(unit);
    }
    let frame = format!("event: content\ndata: {{\"text\":\"{text}\"}}\n\n");
    (text, frame)
}

#[test]
fn sse_every_byte_split_round_trips_a_four_kib_mixed_script_payload() {
    let (expected_text, frame) = mixed_script_frame();

    for split in 0..=frame.len() {
        let mut parser = SseParser::new();
        let mut items = parser.push(&frame.as_bytes()[..split]);
        items.extend(parser.push(&frame.as_bytes()[split..]));
        items.extend(parser.finish());
        let events = successful_events(items);

        assert_eq!(events.len(), 1, "unexpected event count at offset {split}");
        let value: serde_json::Value = events[0]
            .deserialize("sweep", "mixed-script")
            .unwrap_or_else(|error| panic!("JSON failed at offset {split}: {error:?}"));
        assert_eq!(
            value["text"].as_str(),
            Some(expected_text.as_str()),
            "decoded text changed at offset {split}"
        );
    }

    println!(
        "sweep passed: {} byte offsets over a {}-byte mixed-script SSE payload",
        frame.len() + 1,
        frame.len()
    );
}

#[test]
fn sse_decoder_holds_incomplete_code_points_between_chunks() {
    let text = "读取文件 🧩 then continue";
    let bytes = text.as_bytes();
    let mut decoder = Utf8StreamDecoder::new();
    let mut decoded = String::new();

    for byte in bytes {
        decoded.push_str(&decoder.decode(&[*byte]));
    }
    decoded.push_str(&decoder.finish());

    assert_eq!(decoded, text);
    assert!(!decoder.has_pending_bytes());
}

#[test]
fn sse_crlf_frames_parse_without_leaking_carriage_returns() {
    let mut parser = SseParser::new();
    let events = successful_events(
        parser.push(b"event: message\r\ndata: {\"text\":\"hello\"}\r\n\r\ndata: [DONE]\r\n\r\n"),
    );

    assert_eq!(
        events,
        vec![
            SseEvent {
                event: Some("message".to_owned()),
                data: "{\"text\":\"hello\"}".to_owned(),
            },
            SseEvent {
                event: None,
                data: "[DONE]".to_owned(),
            },
        ]
    );
}

#[test]
fn sse_finish_emits_a_trailing_unterminated_frame() {
    let mut parser = SseParser::new();
    assert!(parser.push(b"event: delta\ndata: final").is_empty());
    assert_eq!(
        successful_events(parser.finish()),
        vec![SseEvent {
            event: Some("delta".to_owned()),
            data: "final".to_owned(),
        }]
    );
}

#[test]
fn sse_malformed_json_error_names_provider_and_model() {
    let mut parser = SseParser::new();
    let event = parser
        .push(b"data: {\"broken\":]\n\n")
        .pop()
        .expect("one malformed frame")
        .expect("SSE framing succeeds");
    let error = event
        .deserialize::<serde_json::Value>("anthropic", "claude-opus-4-7")
        .expect_err("malformed JSON must fail");
    let detail = error
        .source()
        .expect("provider error preserves the contextual source")
        .to_string();

    println!("malformed-frame error: {detail}");
    assert!(detail.contains("anthropic"), "{detail}");
    assert!(detail.contains("claude-opus-4-7"), "{detail}");
    assert!(matches!(error, ProviderError::Fatal { .. }));
}

#[tokio::test(start_paused = true)]
async fn sse_idle_timeout_names_the_configured_seconds() {
    let timeout = StreamIdleTimeout::new(Duration::from_secs(137));
    let error = timeout
        .wait(
            "openai",
            "o3-reasoning",
            future::pending::<Option<Result<Vec<u8>, std::io::Error>>>(),
        )
        .await
        .expect_err("a stream with no data must time out");
    let detail = error
        .source()
        .expect("timeout preserves the contextual source")
        .to_string();

    println!("idle-timeout error: {detail}");
    assert!(detail.contains("137 seconds"), "{detail}");
    assert!(detail.contains("openai"), "{detail}");
    assert!(detail.contains("o3-reasoning"), "{detail}");
    assert!(matches!(error, ProviderError::Transient { .. }));
}

#[test]
fn sse_default_idle_timeout_allows_ninety_second_reasoning_gaps() {
    assert!(
        StreamIdleTimeout::new(Duration::from_secs(DEFAULT_STREAM_IDLE_TIMEOUT_SECS)).duration()
            > Duration::from_secs(90)
    );
}

#[test]
fn sse_real_provider_recordings_parse_with_the_shared_parser() {
    let Some(root) = zuno_testkit::recordings_root_or_skip(
        "sse_real_provider_recordings_parse_with_the_shared_parser",
        "the real provider SSE corpus was NOT parsed",
    ) else {
        return;
    };
    let cassette_names = list_cassettes(&root).expect("list real provider recordings");
    let mut sse_responses = 0usize;
    let mut lf_separators = 0usize;
    let mut crlf_separators = 0usize;
    let mut tested_cassettes = Vec::new();

    for name in cassette_names {
        let player = CassettePlayer::load(&root, &name)
            .unwrap_or_else(|error| panic!("load cassette {name}: {error}"));
        for (index, interaction) in player.cassette().http_interactions().enumerate() {
            let response = &interaction.response;
            if !response.is_sse() {
                continue;
            }

            let body = response
                .decoded_body(&name, index + 1)
                .unwrap_or_else(|error| panic!("decode cassette {name}: {error}"));
            lf_separators += body.windows(2).filter(|window| *window == b"\n\n").count();
            crlf_separators += body
                .windows(4)
                .filter(|window| *window == b"\r\n\r\n")
                .count();

            let expected = response.sse_frames();
            let mut parser = SseParser::new();
            let mut items = Vec::new();
            for chunk in body.chunks(17) {
                items.extend(parser.push(chunk));
            }
            items.extend(parser.finish());
            let actual = successful_events(items);

            assert_eq!(actual.len(), expected.len(), "frame count in {name}");
            for (frame_index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
                assert_eq!(
                    actual.event, expected.event,
                    "event field in {name} frame {frame_index}"
                );
                assert_eq!(
                    actual.data, expected.data,
                    "data field in {name} frame {frame_index}"
                );
            }

            sse_responses += 1;
            tested_cassettes.push(name.clone());
        }
    }

    tested_cassettes.sort();
    tested_cassettes.dedup();
    println!(
        "real SSE corpus: {sse_responses} responses from {} cassettes; separators LF={lf_separators}, CRLF={crlf_separators}; cassettes={}",
        tested_cassettes.len(),
        tested_cassettes.join(",")
    );
    assert!(sse_responses >= 45, "only {sse_responses} SSE responses");
    assert!(lf_separators > 0, "the corpus should exercise LF frames");
    assert!(
        crlf_separators > 0,
        "the corpus should exercise CRLF frames"
    );
}

#[test]
fn sse_stream_without_a_delimiter_errors_at_the_event_cap() {
    let limit = 64;
    let limits = StreamLimits::new(limit, 32);
    let mut parser = SseParser::with_limits("hostile-provider", "no-delimiter", limits);

    assert!(parser.push(&vec![b'x'; limit]).is_empty());
    let error = parser
        .push(b"x")
        .pop()
        .expect("the first byte over the cap must produce an item")
        .expect_err("an unterminated event must be refused before further growth");
    let detail = error
        .source()
        .expect("the provider error retains the size failure")
        .to_string();

    assert!(detail.contains("hostile-provider"), "{detail}");
    assert!(detail.contains("no-delimiter"), "{detail}");
    assert!(detail.contains("65 bytes"), "{detail}");
    assert!(detail.contains("limit 64 bytes"), "{detail}");
}

#[test]
fn sse_large_event_below_the_cap_parses_intact() {
    let text = "x".repeat(96 * 1024);
    let frame = format!("event: content\ndata: {text}\n\n");
    let limits = StreamLimits::new(frame.len(), 32);
    let mut parser = SseParser::with_limits("large-provider", "long-code", limits);

    let events = successful_events(parser.push(frame.as_bytes()));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.as_deref(), Some("content"));
    assert_eq!(events[0].data, text);
}

#[test]
fn sse_cap_boundary_retains_a_split_multibyte_code_point() {
    let content = "x".repeat(29);
    let frame_without_separator = format!("data: {content}界");
    let boundary = frame_without_separator.len();
    let bytes = frame_without_separator.as_bytes();
    let multibyte_start = boundary - "界".len();
    let limits = StreamLimits::new(boundary, 32);
    let mut parser = SseParser::with_limits("utf8-provider", "boundary", limits);

    assert!(parser.push(&bytes[..multibyte_start + 1]).is_empty());
    assert!(parser.push(&bytes[multibyte_start + 1..]).is_empty());
    assert!(parser.push(b"\n").is_empty());
    let events = successful_events(parser.push(b"\n"));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, format!("{content}界"));
}

#[test]
fn input_json_over_the_cap_errors_without_truncating_or_appending() {
    let mut input = String::from("{\"path\":\"");
    let before = input.clone();
    let limit = input.len() + 3;

    let error = append_tool_input(
        &mut input,
        "toolong\"}",
        "anthropic",
        "claude-tool-stream",
        limit,
    )
    .expect_err("tool JSON beyond the cap must be refused");
    let detail = error
        .source()
        .expect("the provider error retains the size failure")
        .to_string();

    assert_eq!(input, before, "a rejected fragment must not be appended");
    assert!(detail.contains("anthropic"), "{detail}");
    assert!(detail.contains("claude-tool-stream"), "{detail}");
    assert!(detail.contains(&format!("limit {limit} bytes")), "{detail}");
}

#[test]
fn sse_source_has_no_chunk_lossy_conversion() {
    let source = include_str!("../src/sse.rs");
    let forbidden = concat!("from_utf8_", "lossy");
    assert!(
        !source.contains(forbidden),
        "forbidden UTF-8 conversion found"
    );
}
