//! Translating Chat Completions and Responses frames into shared events.
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

use std::collections::BTreeMap;
use std::time::Duration;

use oc_error::ProviderError;
use oc_llm::registry::{ApiSurface, FinishReason, StreamEvent};
use serde::Deserialize;

use crate::wire::{ChatChunk, ChunkDelta, DONE_SENTINEL, WireError};

/// Selects the decoder that matches the resolved request surface.
#[derive(Debug)]
pub enum SurfaceTranslator {
    /// Chat Completions `choices[].delta` frames.
    Chat(ChunkTranslator),
    /// Responses typed `response.*` events.
    Responses(ResponsesTranslator),
}

impl SurfaceTranslator {
    /// A translator for one resolved request surface.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>, surface: ApiSurface) -> Self {
        let provider = provider.into();
        let model = model.into();
        if surface == ApiSurface::Responses {
            Self::Responses(ResponsesTranslator::new(provider, model))
        } else {
            Self::Chat(ChunkTranslator::new(provider, model))
        }
    }

    /// Translate one complete SSE `data:` payload.
    pub fn frame(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        match self {
            Self::Chat(translator) => translator.frame(data),
            Self::Responses(translator) => translator.frame(data),
        }
    }

    /// Close any protocol state left open at EOF.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        match self {
            Self::Chat(translator) => translator.finish(),
            Self::Responses(translator) => translator.finish(),
        }
    }
}

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

/// Turns typed Responses events into [`StreamEvent`]s.
#[derive(Debug)]
pub struct ResponsesTranslator {
    provider: String,
    model: String,
    reasoning: BTreeMap<String, ActiveReasoning>,
    tools: BTreeMap<String, ActiveTool>,
    saw_tool: bool,
    ended: bool,
    done: bool,
}

impl ResponsesTranslator {
    /// A translator for one Responses request.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            reasoning: BTreeMap::new(),
            tools: BTreeMap::new(),
            saw_tool: false,
            ended: false,
            done: false,
        }
    }

    /// Translate one complete SSE `data:` payload.
    pub fn frame(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let trimmed = data.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        if trimmed == DONE_SENTINEL {
            self.done = true;
            return Ok(self.finish());
        }
        let event: ResponsesEvent = serde_json::from_str(trimmed).map_err(|source| {
            ProviderError::fatal(MalformedResponsesEvent {
                provider: self.provider.clone(),
                model: self.model.clone(),
                source,
            })
        })?;
        match event {
            ResponsesEvent::OutputTextDelta { delta } => Ok(vec![StreamEvent::TextDelta(delta)]),
            ResponsesEvent::ReasoningSummaryPartAdded { item_id } => {
                let reasoning = self.reasoning.entry(item_id).or_default();
                if reasoning.open {
                    Ok(Vec::new())
                } else {
                    reasoning.open = true;
                    Ok(vec![StreamEvent::ReasoningStart])
                }
            }
            ResponsesEvent::ReasoningSummaryTextDelta { item_id, delta } => {
                let reasoning = self.reasoning.entry(item_id).or_default();
                let mut events = Vec::new();
                if !reasoning.open {
                    reasoning.open = true;
                    events.push(StreamEvent::ReasoningStart);
                }
                reasoning.current_summary.push_str(&delta);
                events.push(StreamEvent::ReasoningDelta(delta));
                Ok(events)
            }
            ResponsesEvent::ReasoningSummaryTextDone { item_id, text } => {
                let reasoning = self.reasoning.entry(item_id).or_default();
                let summary = if text.is_empty() {
                    std::mem::take(&mut reasoning.current_summary)
                } else {
                    reasoning.current_summary.clear();
                    text
                };
                if !summary.is_empty() {
                    reasoning.summary.push(summary);
                }
                Ok(Vec::new())
            }
            ResponsesEvent::OutputItemAdded { item } => self.item_added(item),
            ResponsesEvent::FunctionCallArgumentsDelta { item_id, delta } => {
                if let Some(tool) = self.tools.get_mut(&item_id) {
                    tool.arguments.push_str(&delta);
                }
                Ok(vec![StreamEvent::ToolInputDelta(delta)])
            }
            ResponsesEvent::OutputItemDone { item } => self.item_done(item),
            ResponsesEvent::Completed { response } => Ok(self.complete(response, false)),
            ResponsesEvent::Incomplete { response } => {
                Ok(self.terminal(response, FinishReason::Length))
            }
            ResponsesEvent::Failed { response } => Ok(self.terminal(response, FinishReason::Error)),
            ResponsesEvent::Error { error } => Err(classify(&self.provider, &error)),
            ResponsesEvent::Other => Ok(Vec::new()),
        }
    }

    /// Close a Responses stream that ended without a terminal event.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        if self.ended {
            Vec::new()
        } else {
            self.terminal(ResponseEnvelope::default(), FinishReason::Unknown)
        }
    }

    fn item_added(&mut self, item: ResponseItem) -> Result<Vec<StreamEvent>, ProviderError> {
        match item.kind.as_str() {
            "reasoning" => {
                self.reasoning.entry(item.id).or_default();
                Ok(Vec::new())
            }
            "function_call" => {
                self.saw_tool = true;
                let item_id = item.id;
                let call_id = item.call_id.unwrap_or_default();
                let name = item.name.unwrap_or_default();
                self.tools.insert(
                    item_id,
                    ActiveTool {
                        arguments: item.arguments.unwrap_or_default(),
                    },
                );
                Ok(vec![StreamEvent::ToolUseStart { id: call_id, name }])
            }
            _ => Ok(Vec::new()),
        }
    }

    fn item_done(&mut self, item: ResponseItem) -> Result<Vec<StreamEvent>, ProviderError> {
        match item.kind.as_str() {
            "reasoning" => {
                let mut reasoning = self.reasoning.remove(&item.id).unwrap_or_default();
                let mut events = Vec::new();
                if reasoning.open {
                    events.push(StreamEvent::ReasoningEnd);
                }
                if let Some(summary) = item.summary {
                    reasoning.summary = summary.into_iter().filter_map(|part| part.text).collect();
                } else if !reasoning.current_summary.is_empty() {
                    reasoning.summary.push(reasoning.current_summary);
                }
                events.push(StreamEvent::ProviderReasoningItem {
                    id: item.id,
                    summary: reasoning.summary,
                    encrypted_content: item.encrypted_content,
                    status: item.status,
                });
                Ok(events)
            }
            "function_call" => {
                self.tools.remove(&item.id);
                Ok(vec![StreamEvent::ToolUseEnd])
            }
            _ => Ok(Vec::new()),
        }
    }

    fn complete(&mut self, response: ResponseEnvelope, force_stop: bool) -> Vec<StreamEvent> {
        let reason = if self.saw_tool && !force_stop {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };
        self.terminal(response, reason)
    }

    fn terminal(&mut self, response: ResponseEnvelope, reason: FinishReason) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        self.done = true;
        let mut events = vec![StreamEvent::MessageEnd {
            stop_reason: Some(reason),
        }];
        if let Some(usage) = response.usage {
            events.push(StreamEvent::TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_input_tokens: usage
                    .input_tokens_details
                    .and_then(|details| details.cached_tokens),
                cache_write_input_tokens: None,
            });
        }
        events
    }
}

#[derive(Debug, Default)]
struct ActiveReasoning {
    open: bool,
    current_summary: String,
    summary: Vec<String>,
}

#[derive(Debug)]
struct ActiveTool {
    arguments: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ResponsesEvent {
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded { item_id: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta { item_id: String, delta: String },
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone { item_id: String, text: String },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { item: ResponseItem },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta { item_id: String, delta: String },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { item: ResponseItem },
    #[serde(rename = "response.completed")]
    Completed { response: ResponseEnvelope },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: ResponseEnvelope },
    #[serde(rename = "response.failed")]
    Failed { response: ResponseEnvelope },
    #[serde(rename = "error")]
    Error {
        #[serde(flatten)]
        error: WireError,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct ResponseItem {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    encrypted_content: Option<String>,
    #[serde(default)]
    summary: Option<Vec<SummaryPart>>,
}

#[derive(Debug, Deserialize)]
struct SummaryPart {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ResponseEnvelope {
    #[serde(default)]
    usage: Option<ResponseUsage>,
}

#[derive(Debug, Deserialize)]
struct ResponseUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<ResponseTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct ResponseTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug)]
struct MalformedResponsesEvent {
    provider: String,
    model: String,
    source: serde_json::Error,
}

impl std::fmt::Display for MalformedResponsesEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "provider `{}` model `{}` sent a Responses event that is not valid JSON: {}",
            self.provider, self.model, self.source
        )
    }
}

impl std::error::Error for MalformedResponsesEvent {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
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

    fn translate_responses(frames: &[&str]) -> Vec<StreamEvent> {
        let mut translator = SurfaceTranslator::new("test", "model", ApiSurface::Responses);
        let mut events = Vec::new();
        for frame in frames {
            events.extend(translator.frame(frame).expect("frame translates"));
        }
        events.extend(translator.finish());
        events
    }

    #[test]
    fn responses_events_decode_text_reasoning_tools_and_usage() {
        let events = translate_responses(&[
            r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Think"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"Think"}],"encrypted_content":"opaque","status":"completed"}}"#,
            r#"{"type":"response.output_text.delta","delta":"Hello"}"#,
            r#"{"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"lookup","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"q\":1}"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":3}}}}"#,
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningStart,
                StreamEvent::ReasoningDelta("Think".to_owned()),
                StreamEvent::ReasoningEnd,
                StreamEvent::ProviderReasoningItem {
                    id: "rs_1".to_owned(),
                    summary: vec!["Think".to_owned()],
                    encrypted_content: Some("opaque".to_owned()),
                    status: Some("completed".to_owned()),
                },
                StreamEvent::TextDelta("Hello".to_owned()),
                StreamEvent::ToolUseStart {
                    id: "call_1".to_owned(),
                    name: "lookup".to_owned(),
                },
                StreamEvent::ToolInputDelta("{\"q\":1}".to_owned()),
                StreamEvent::ToolUseEnd,
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
                StreamEvent::TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(4),
                    cache_read_input_tokens: Some(3),
                    cache_write_input_tokens: None,
                },
            ]
        );
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
