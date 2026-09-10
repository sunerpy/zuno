use zuno_llm::event::{PromptAccounting, StreamEvent};
use zuno_provider_bedrock::BedrockEventDecoder;

fn decode(bytes: &[u8], chunk_size: usize) -> Vec<StreamEvent> {
    let mut decoder = BedrockEventDecoder::new();
    let mut events = Vec::new();
    for chunk in bytes.chunks(chunk_size) {
        events.extend(decoder.push(chunk).expect("captured frame"));
    }
    events.extend(decoder.finish().expect("complete response"));
    events
}

#[test]
fn real_converse_cache_receipts_keep_uncached_input_and_full_context_distinct() {
    for (bytes, read, write) in [
        (
            include_bytes!("fixtures/usage/converse-cold.eventstream").as_slice(),
            0,
            4209,
        ),
        (
            include_bytes!("fixtures/usage/converse-warm.eventstream").as_slice(),
            4209,
            0,
        ),
    ] {
        for chunk_size in [1, 17, bytes.len()] {
            let events = decode(bytes, chunk_size);
            let usage = events
                .iter()
                .find(|event| matches!(event, StreamEvent::TokenUsage { .. }))
                .expect("usage survives even a content-filtered response");
            assert_eq!(
                usage,
                &StreamEvent::TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(1),
                    reasoning_tokens: None,
                    cache_read_input_tokens: Some(read),
                    cache_write_input_tokens: Some(write),
                    accounting: PromptAccounting::CacheBesideInput,
                }
            );
            assert_eq!(
                PromptAccounting::CacheBesideInput.prompt_total(10, read, write) + 1,
                4220
            );
            assert_eq!(
                PromptAccounting::CacheBesideInput.uncached_input(10, read, write),
                10
            );
        }
    }
}

#[test]
fn real_invoke_receipts_include_start_usage_and_explicit_thinking_breakdown() {
    for (bytes, read, write) in [
        (
            include_bytes!("fixtures/usage/invoke-cold.eventstream").as_slice(),
            0,
            4210,
        ),
        (
            include_bytes!("fixtures/usage/invoke-warm.eventstream").as_slice(),
            4210,
            0,
        ),
    ] {
        let usage = decode(bytes, 13)
            .into_iter()
            .filter(|event| matches!(event, StreamEvent::TokenUsage { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            usage,
            vec![
                StreamEvent::TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(1),
                    reasoning_tokens: None,
                    cache_read_input_tokens: Some(read),
                    cache_write_input_tokens: Some(write),
                    accounting: PromptAccounting::CacheBesideInput,
                },
                StreamEvent::TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(1),
                    reasoning_tokens: Some(0),
                    cache_read_input_tokens: Some(read),
                    cache_write_input_tokens: Some(write),
                    accounting: PromptAccounting::CacheBesideInput,
                }
            ]
        );
    }
}

#[test]
fn successful_responses_preserve_the_final_provider_snapshot_without_adding_start_usage() {
    for (bytes, input, read, write, output, reports) in [
        (
            include_bytes!("fixtures/usage/fable-converse-cold.eventstream").as_slice(),
            22,
            0,
            4987,
            38,
            1,
        ),
        (
            include_bytes!("fixtures/usage/fable-converse-warm.eventstream").as_slice(),
            22,
            4987,
            0,
            48,
            1,
        ),
        (
            include_bytes!("fixtures/usage/fable-invoke-cold.eventstream").as_slice(),
            22,
            0,
            4988,
            39,
            2,
        ),
        (
            include_bytes!("fixtures/usage/fable-invoke-warm.eventstream").as_slice(),
            22,
            4988,
            0,
            36,
            2,
        ),
    ] {
        let events = decode(bytes, 7);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::TextDelta(text) if !text.is_empty()))
        );
        let usages = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::TokenUsage { .. }))
            .collect::<Vec<_>>();
        assert_eq!(usages.len(), reports);
        assert!(matches!(
            usages.last(),
            Some(StreamEvent::TokenUsage {
                input_tokens: Some(actual_input),
                output_tokens: Some(actual_output),
                cache_read_input_tokens: Some(actual_read),
                cache_write_input_tokens: Some(actual_write),
                accounting: PromptAccounting::CacheBesideInput,
                ..
            }) if *actual_input == input && *actual_output == output
                && *actual_read == read && *actual_write == write
        ));
    }
}
