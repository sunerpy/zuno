//! Translating recorded chat-completions chunks into the shared event vocabulary.
//!
//! # What this module is not
//!
//! It is not an SSE parser. [`oc_llm::sse::SseParser`] owns framing and UTF-8
//! boundary state, proven against a byte-split sweep, and this module receives
//! already-complete frames. There is no `from_utf8_lossy` here, and no `\n\n`
//! search — writing either would fork the one guarantee that a multi-byte code
//! point split across network chunks survives.
//!
//! # The state a chunk stream forces
//!
//! Chat-completions has no block-open or block-close events, so three transitions
//! must be inferred:
//!
//! 1. **Reasoning blocks.** The first `delta.reasoning_content` opens a block; the
//!    first text delta or the finish reason closes it. The corpus shows the opening
//!    fragment is often the empty string, so opening must not depend on content
//!    being non-empty.
//! 2. **Tool calls.** A fragment carrying `function.name` opens a call; subsequent
//!    fragments carry only `arguments`. A change of `index` closes the previous
//!    call and opens the next, and the finish reason closes the last.
//! 3. **End of message.** Either a `finish_reason` or the `[DONE]` sentinel; both
//!    appear, and some vendors send only one.

use std::time::Duration;

use oc_error::ProviderError;
use oc_llm::registry::{FinishReason, StreamEvent};

use crate::wire::{ChatChunk, ChunkDelta, DONE_SENTINEL, WireError};

/// Turns chat-completions frames into [`StreamEvent`]s, holding the block state
/// the wire format leaves implicit.
#[derive(Debug)]
pub struct ChunkTranslator {
    provider: String,
    model: String,
    reasoning_open: bool,
    tool_open: bool,
    tool_index: Option<u32>,
    upstream_reported: bool,
    ended: bool,
    done: bool,
}

impl ChunkTranslator {
    /// A translator for one request.
    ///
    /// `provider` and `model` are carried only so a malformed frame produces an
    /// error that names them; they never influence translation.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            reasoning_open: false,
            tool_open: false,
            tool_index: None,
            upstream_reported: false,
            ended: false,
            done: false,
        }
    }

    /// Whether the stream has delivered its terminal frame.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// Translate one SSE `data:` payload.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ProviderError`] when the frame is an error object, or
    /// when it is not valid JSON. Classification reads the structured `code` and
    /// `type` fields; it never inspects a rendered message.
    pub fn frame(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let trimmed = data.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        if trimmed == DONE_SENTINEL {
            self.done = true;
            return Ok(self.close_open_blocks());
        }

        let chunk: ChatChunk = serde_json::from_str(trimmed).map_err(|source| {
            ProviderError::fatal(MalformedChunk {
                provider: self.provider.clone(),
                model: self.model.clone(),
                source,
            })
        })?;

        if let Some(error) = chunk.error {
            return Err(classify(&self.provider, &error));
        }

        let mut events = Vec::new();
        if let Some(upstream) = chunk.provider.as_deref()
            && !self.upstream_reported
            && !upstream.is_empty()
        {
            self.upstream_reported = true;
            events.push(StreamEvent::UpstreamProvider {
                provider: upstream.to_owned(),
            });
        }

        for choice in &chunk.choices {
            self.delta(&choice.delta, &mut events);
            if let Some(reason) = choice.finish_reason.as_deref() {
                events.extend(self.close_open_blocks());
                if !self.ended {
                    self.ended = true;
                    events.push(StreamEvent::MessageEnd {
                        stop_reason: Some(finish_reason(reason)),
                    });
                }
            }
        }

        if let Some(usage) = chunk.usage {
            events.push(StreamEvent::TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_read_input_tokens: usage
                    .prompt_tokens_details
                    .and_then(|details| details.cached_tokens),
                cache_write_input_tokens: None,
            });
        }

        Ok(events)
    }

    /// Close whatever the stream left open when it ends without a finish reason.
    ///
    /// A vendor that drops the connection after its last content chunk still has
    /// to leave a consumer with balanced blocks.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        let mut events = self.close_open_blocks();
        if !self.ended {
            self.ended = true;
            events.push(StreamEvent::MessageEnd { stop_reason: None });
        }
        events
    }

    fn delta(&mut self, delta: &ChunkDelta, events: &mut Vec<StreamEvent>) {
        if let Some(fragment) = delta.reasoning_fragment() {
            if !self.reasoning_open {
                self.reasoning_open = true;
                events.push(StreamEvent::ReasoningStart);
            }
            if !fragment.is_empty() {
                events.push(StreamEvent::ReasoningDelta(fragment.to_owned()));
            }
        }

        if let Some(text) = delta.content.as_deref()
            && !text.is_empty()
        {
            if self.reasoning_open {
                self.reasoning_open = false;
                events.push(StreamEvent::ReasoningEnd);
            }
            events.push(StreamEvent::TextDelta(text.to_owned()));
        }

        for call in &delta.tool_calls {
            let function = call.function.as_ref();
            let name = function.and_then(|function| function.name.as_deref());
            let starts_new = name.is_some() || (call.index != self.tool_index && !self.tool_open);
            if starts_new || (call.index.is_some() && call.index != self.tool_index) {
                if self.tool_open {
                    self.tool_open = false;
                    events.push(StreamEvent::ToolUseEnd);
                }
                if self.reasoning_open {
                    self.reasoning_open = false;
                    events.push(StreamEvent::ReasoningEnd);
                }
                self.tool_index = call.index;
                self.tool_open = true;
                events.push(StreamEvent::ToolUseStart {
                    id: call.id.clone().unwrap_or_default(),
                    name: name.unwrap_or_default().to_owned(),
                });
            }
            if let Some(arguments) = function.and_then(|function| function.arguments.as_deref())
                && !arguments.is_empty()
            {
                events.push(StreamEvent::ToolInputDelta(arguments.to_owned()));
            }
        }
    }

    fn close_open_blocks(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if self.tool_open {
            self.tool_open = false;
            events.push(StreamEvent::ToolUseEnd);
        }
        if self.reasoning_open {
            self.reasoning_open = false;
            events.push(StreamEvent::ReasoningEnd);
        }
        events
    }
}

/// Map a wire finish reason onto the shared vocabulary.
///
/// This is a match on an enumerated protocol value, not text classification: the
/// set is fixed by the chat-completions specification, and an unrecognized value
/// becomes [`FinishReason::Unknown`] rather than a guess.
#[must_use]
pub fn finish_reason(wire: &str) -> FinishReason {
    match wire {
        "stop" | "end_turn" => FinishReason::Stop,
        "length" | "max_tokens" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

/// Classify a structured error body into the typed taxonomy.
///
/// Reads, in order: the numeric status the body may carry, then the string code,
/// then the error class. Every one of those is a field on the wire.
/// [`WireError::message`] is attached as payload and never examined.
#[must_use]
pub fn classify(provider: &str, error: &WireError) -> ProviderError {
    if let Some("context_length_exceeded") = error.code_str() {
        return ProviderError::ContextLimit {
            limit_tokens: None,
            used_tokens: None,
        };
    }
    if let Some("content_filter") = error.code_str() {
        return ProviderError::Refused {
            provider: provider.to_owned(),
            provider_text: error.message.clone(),
        };
    }
    if error.kind.as_deref() == Some("content_filter") {
        return ProviderError::Refused {
            provider: provider.to_owned(),
            provider_text: error.message.clone(),
        };
    }
    if let Some(status) = error.status() {
        return ProviderError::from_status(provider, status);
    }
    if error.kind.as_deref() == Some("insufficient_quota") {
        return ProviderError::Fatal {
            status: None,
            source: None,
        };
    }
    ProviderError::Fatal {
        status: None,
        source: None,
    }
}

/// Parse a `Retry-After` header value into a delay.
///
/// Only the delta-seconds form is accepted; the HTTP-date form is rare from these
/// vendors and a wrong parse would produce a worse backoff than none. Returning
/// `None` lets the caller apply its own policy, which `oc-error` already owns.
#[must_use]
pub fn retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// A frame that was not valid JSON.
#[derive(Debug)]
struct MalformedChunk {
    provider: String,
    model: String,
    source: serde_json::Error,
}

impl std::fmt::Display for MalformedChunk {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "provider `{}` model `{}` sent a chat-completions chunk that is not valid JSON: {}",
            self.provider, self.model, self.source
        )
    }
}

impl std::error::Error for MalformedChunk {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_error::Recovery;

    fn translate(frames: &[&str]) -> Vec<StreamEvent> {
        let mut translator = ChunkTranslator::new("test", "model");
        let mut events = Vec::new();
        for frame in frames {
            events.extend(translator.frame(frame).expect("frame translates"));
        }
        events.extend(translator.finish());
        events
    }

    #[test]
    fn an_empty_reasoning_fragment_opens_the_block_without_emitting_a_delta() {
        let events = translate(&[
            r#"{"choices":[{"delta":{"reasoning_content":""}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"We"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hi"},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningStart,
                StreamEvent::ReasoningDelta("We".to_owned()),
                StreamEvent::ReasoningEnd,
                StreamEvent::TextDelta("Hi".to_owned()),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop)
                },
            ]
        );
    }

    #[test]
    fn two_tool_calls_are_bracketed_separately() {
        let events = translate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"f","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"g","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseStart {
                    id: "a".to_owned(),
                    name: "f".to_owned()
                },
                StreamEvent::ToolInputDelta("{\"x\":1}".to_owned()),
                StreamEvent::ToolUseEnd,
                StreamEvent::ToolUseStart {
                    id: "b".to_owned(),
                    name: "g".to_owned()
                },
                StreamEvent::ToolInputDelta("{}".to_owned()),
                StreamEvent::ToolUseEnd,
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls)
                },
            ]
        );
    }

    #[test]
    fn the_done_sentinel_ends_the_stream_without_a_second_message_end() {
        let mut translator = ChunkTranslator::new("test", "model");
        let mut events = translator
            .frame(r#"{"choices":[{"delta":{"content":"a"},"finish_reason":"stop"}]}"#)
            .expect("chunk");
        events.extend(translator.frame(DONE_SENTINEL).expect("sentinel"));
        events.extend(translator.finish());
        assert!(translator.is_done());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::MessageEnd { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn a_router_upstream_is_reported_once() {
        let events = translate(&[
            r#"{"provider":"Anthropic","choices":[{"delta":{"content":"a"}}]}"#,
            r#"{"provider":"Anthropic","choices":[{"delta":{"content":"b"},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::UpstreamProvider { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn usage_becomes_a_token_event_including_cache_reads() {
        let events = translate(&[r#"{"choices":[{"delta":{},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":3,
                         "prompt_tokens_details":{"cached_tokens":8}}}"#]);
        assert!(events.contains(&StreamEvent::TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(3),
            cache_read_input_tokens: Some(8),
            cache_write_input_tokens: None,
        }));
    }

    #[test]
    fn an_in_stream_error_is_classified_from_its_structured_code() {
        let mut translator = ChunkTranslator::new("groq", "llama");
        let error = translator
            .frame(r#"{"error":{"code":429,"message":"Rate limit reached"}}"#)
            .expect_err("an error frame is an error");
        assert_eq!(error.recovery(), Recovery::Retry { after: None });

        let mut translator = ChunkTranslator::new("groq", "llama");
        let overflow = translator
            .frame(r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#)
            .expect_err("an error frame is an error");
        assert_eq!(overflow.recovery(), Recovery::Compact);
    }

    #[test]
    fn a_refusal_is_not_retried() {
        let refused = classify(
            "openai",
            &WireError {
                message: Some("I can't help with that".to_owned()),
                code: Some(serde_json::json!("content_filter")),
                kind: None,
            },
        );
        assert_eq!(refused.recovery(), Recovery::Fail);
        assert!(matches!(
            refused,
            ProviderError::Refused {
                provider_text: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn malformed_json_names_the_provider_and_model_and_is_fatal() {
        let mut translator = ChunkTranslator::new("cerebras", "llama-3.3-70b");
        let error = translator.frame("{not json").expect_err("not JSON");
        assert_eq!(error.recovery(), Recovery::Fail);
        let rendered = format!("{:#}", ErrorChain(&error));
        assert!(rendered.contains("cerebras"), "{rendered}");
        assert!(rendered.contains("llama-3.3-70b"), "{rendered}");
    }

    #[test]
    fn retry_after_reads_only_delta_seconds() {
        assert_eq!(retry_after("30"), Some(Duration::from_secs(30)));
        assert_eq!(retry_after(" 5 "), Some(Duration::from_secs(5)));
        assert_eq!(retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(retry_after(""), None);
    }

    #[test]
    fn unknown_finish_reasons_do_not_become_stop() {
        assert_eq!(finish_reason("stop"), FinishReason::Stop);
        assert_eq!(finish_reason("tool_calls"), FinishReason::ToolCalls);
        assert_eq!(finish_reason("length"), FinishReason::Length);
        assert_eq!(finish_reason("content_filter"), FinishReason::ContentFilter);
        assert_eq!(finish_reason("something_new"), FinishReason::Unknown);
    }

    /// Renders an error together with its source chain, for assertions.
    struct ErrorChain<'a>(&'a ProviderError);

    impl std::fmt::Display for ErrorChain<'_> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            use std::error::Error as _;
            write!(formatter, "{}", self.0)?;
            let mut source = self.0.source();
            while let Some(error) = source {
                write!(formatter, ": {error}")?;
                source = error.source();
            }
            Ok(())
        }
    }
}
