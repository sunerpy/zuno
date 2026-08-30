//! Anthropic SSE event decoding and per-stream accumulation.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;
use zuno_error::ProviderError;
use zuno_llm::event::{
    ConnectionPhase, FinishReason, PromptAccounting, RequestContentBlock, StreamEvent,
};
use zuno_llm::sse::{SseEvent, SseParser, append_tool_input, ensure_tool_input_size};

use crate::error::{AnthropicErrorBody, map_stream_error};

/// Incremental Anthropic stream decoder.
///
/// Raw network bytes are delegated to [`SseParser`]. This type owns only
/// Anthropic protocol state: active content blocks, complete tool JSON, complete
/// signed thinking, served-model comparison, and token counters.
#[derive(Debug)]
pub struct AnthropicDecoder {
    parser: SseParser,
    state: StreamState,
    completed: Vec<RequestContentBlock>,
    finished: bool,
}

impl AnthropicDecoder {
    /// Start decoding a response for `requested_model`.
    #[must_use]
    pub fn new(provider: impl Into<String>, requested_model: impl Into<String>) -> Self {
        let provider = provider.into();
        let requested_model = requested_model.into();
        Self {
            parser: SseParser::for_stream(provider.clone(), requested_model.clone()),
            state: StreamState::new(provider, requested_model),
            completed: Vec::new(),
            finished: false,
        }
    }

    /// Feed one raw network chunk to the shared SSE parser.
    ///
    /// Each returned item preserves event order. If a later frame in the same
    /// chunk is invalid, valid earlier events remain ahead of that error.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Result<StreamEvent, ProviderError>> {
        if self.finished {
            return vec![Err(ProviderError::fatal(ProtocolError::AlreadyFinished))];
        }
        let frames = self.parser.push(chunk);
        self.decode_frames(frames)
    }

    /// Finish SSE framing and append one final token-usage event.
    pub fn finish(&mut self) -> Vec<Result<StreamEvent, ProviderError>> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let frames = self.parser.finish();
        let mut output = self.decode_frames(frames);
        if self.state.failed {
            return output;
        }
        if !self.state.active.is_empty() {
            output.push(Err(ProviderError::fatal(
                ProtocolError::IncompleteContentBlocks(self.state.active.keys().copied().collect()),
            )));
            self.state.failed = true;
            return output;
        }
        if self.state.usage.has_any() {
            output.push(Ok(StreamEvent::TokenUsage {
                input_tokens: self.state.usage.input_tokens,
                output_tokens: self.state.usage.output_tokens,
                cache_read_input_tokens: self.state.usage.cache_read_input_tokens,
                cache_write_input_tokens: self.state.usage.cache_creation_input_tokens,
                accounting: PromptAccounting::CacheBesideInput,
            }));
        }
        output
    }

    /// Fully accumulated assistant blocks suitable for transcript storage and a
    /// later request. Thinking appears here only after its signature completed.
    #[must_use]
    pub fn completed_blocks(&self) -> &[RequestContentBlock] {
        &self.completed
    }

    /// Consume all fully accumulated assistant blocks.
    #[must_use]
    pub fn into_completed_blocks(self) -> Vec<RequestContentBlock> {
        self.completed
    }

    fn decode_frames(
        &mut self,
        frames: Vec<Result<SseEvent, ProviderError>>,
    ) -> Vec<Result<StreamEvent, ProviderError>> {
        let mut output = Vec::new();
        for frame in frames {
            if self.state.failed {
                break;
            }
            let frame = match frame {
                Ok(frame) => frame,
                Err(error) => {
                    self.state.failed = true;
                    output.push(Err(error));
                    break;
                }
            };
            match self.decode_frame(&frame) {
                Ok(events) => output.extend(events.into_iter().map(Ok)),
                Err(error) => {
                    self.state.failed = true;
                    output.push(Err(error));
                }
            }
        }
        output
    }

    fn decode_frame(&mut self, frame: &SseEvent) -> Result<Vec<StreamEvent>, ProviderError> {
        let event: ApiEvent =
            frame.deserialize(&self.state.provider, &self.state.requested_model)?;
        match event {
            ApiEvent::MessageStart { message } => Ok(self.message_start(message)),
            ApiEvent::ContentBlockStart {
                index,
                content_block,
            } => self.content_block_start(index, content_block),
            ApiEvent::ContentBlockDelta { index, delta } => self.content_block_delta(index, delta),
            ApiEvent::ContentBlockStop { index } => self.content_block_stop(index),
            ApiEvent::MessageDelta { delta, usage } => {
                if let Some(usage) = usage {
                    self.state.usage.update(usage);
                }
                Ok(delta
                    .stop_reason
                    .map(|reason| StreamEvent::MessageEnd {
                        stop_reason: Some(finish_reason(&reason)),
                    })
                    .into_iter()
                    .collect())
            }
            ApiEvent::Ping => Ok(vec![StreamEvent::ConnectionPhase {
                phase: ConnectionPhase::Streaming,
            }]),
            ApiEvent::Error { error } => Err(map_stream_error(&self.state.provider, error)),
            ApiEvent::MessageStop | ApiEvent::Unknown => Ok(Vec::new()),
        }
    }

    fn message_start(&mut self, message: ApiMessage) -> Vec<StreamEvent> {
        self.state.usage.update(message.usage);
        let Some(served_model) = message.model else {
            return Vec::new();
        };
        let requested_base = model_base(&self.state.requested_model);
        let served_base = model_base(&served_model);
        if requested_base == served_base || self.state.warned_model_substitution {
            return Vec::new();
        }
        self.state.warned_model_substitution = true;
        tracing::warn!(
            provider = %self.state.provider,
            requested_model = %requested_base,
            served_model = %served_base,
            "Anthropic served a different model than requested"
        );
        vec![StreamEvent::StatusDetail {
            detail: format!(
                "⚠ Anthropic served '{served_base}' instead of requested '{requested_base}' (requested model unavailable)"
            ),
        }]
    }

    fn content_block_start(
        &mut self,
        index: u64,
        block: ApiContentBlock,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.state.active.contains_key(&index) {
            return Err(ProviderError::fatal(ProtocolError::DuplicateBlock(index)));
        }
        let mut events = Vec::new();
        let active = match block {
            ApiContentBlock::Text { text } => {
                if !text.is_empty() {
                    events.push(StreamEvent::TextDelta(text.clone()));
                }
                ActiveBlock::Text { text }
            }
            ApiContentBlock::Thinking {
                thinking,
                signature,
            } => {
                events.push(StreamEvent::ReasoningStart);
                if !thinking.is_empty() {
                    events.push(StreamEvent::ReasoningDelta(thinking.clone()));
                }
                if !signature.is_empty() {
                    events.push(StreamEvent::ReasoningSignatureDelta(signature.clone()));
                }
                ActiveBlock::Thinking {
                    thinking,
                    signature,
                }
            }
            ApiContentBlock::RedactedThinking { data } => {
                events.push(StreamEvent::ProviderReasoningItem {
                    id: format!("anthropic-redacted-{index}"),
                    summary: Vec::new(),
                    encrypted_content: Some(data.clone()),
                    status: Some("redacted_thinking".to_owned()),
                });
                ActiveBlock::RedactedThinking { data }
            }
            ApiContentBlock::ToolUse { id, name, input } => {
                ensure_tool_input_size(
                    input.to_string().len(),
                    &self.state.provider,
                    &self.state.requested_model,
                    self.parser.limits().max_tool_input_bytes(),
                )?;
                events.push(StreamEvent::ToolUseStart {
                    id: id.clone(),
                    name: name.clone(),
                });
                ActiveBlock::ToolUse {
                    id,
                    name,
                    initial_input: input,
                    input_json: String::new(),
                }
            }
            ApiContentBlock::Unknown => ActiveBlock::Unknown,
        };
        self.state.active.insert(index, active);
        Ok(events)
    }

    fn content_block_delta(
        &mut self,
        index: u64,
        delta: ApiDelta,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let active = self
            .state
            .active
            .get_mut(&index)
            .ok_or_else(|| ProviderError::fatal(ProtocolError::DeltaWithoutBlock(index)))?;

        match (active, delta) {
            (ActiveBlock::Text { text: complete }, ApiDelta::Text { text }) => {
                complete.push_str(&text);
                Ok(vec![StreamEvent::TextDelta(text)])
            }
            (ActiveBlock::ToolUse { id, input_json, .. }, ApiDelta::InputJson { partial_json }) => {
                append_tool_input(
                    input_json,
                    &partial_json,
                    &self.state.provider,
                    &self.state.requested_model,
                    self.parser.limits().max_tool_input_bytes(),
                )?;
                Ok(vec![StreamEvent::ToolInputDelta {
                    id: id.clone(),
                    delta: partial_json,
                }])
            }
            (
                ActiveBlock::Thinking {
                    thinking: complete, ..
                },
                ApiDelta::Thinking { thinking },
            ) => {
                complete.push_str(&thinking);
                Ok(vec![StreamEvent::ReasoningDelta(thinking)])
            }
            (
                ActiveBlock::Thinking {
                    signature: complete,
                    ..
                },
                ApiDelta::Signature { signature },
            ) => {
                complete.push_str(&signature);
                Ok(vec![StreamEvent::ReasoningSignatureDelta(signature)])
            }
            (_, ApiDelta::Unknown) => Ok(Vec::new()),
            (block, delta) => Err(ProviderError::fatal(ProtocolError::MismatchedDelta {
                index,
                block: block.kind(),
                delta: delta.kind(),
            })),
        }
    }

    fn content_block_stop(&mut self, index: u64) -> Result<Vec<StreamEvent>, ProviderError> {
        let block = self
            .state
            .active
            .remove(&index)
            .ok_or_else(|| ProviderError::fatal(ProtocolError::StopWithoutBlock(index)))?;
        match block {
            ActiveBlock::Text { text } => {
                if !text.is_empty() {
                    self.completed.push(RequestContentBlock::Text { text });
                }
                Ok(Vec::new())
            }
            ActiveBlock::Thinking {
                thinking,
                signature,
            } => {
                if signature.is_empty() {
                    return Err(ProviderError::fatal(ProtocolError::UnsignedThinking(index)));
                }
                self.completed.push(RequestContentBlock::SignedThinking {
                    thinking,
                    signature,
                });
                Ok(vec![StreamEvent::ReasoningEnd])
            }
            ActiveBlock::RedactedThinking { data } => {
                self.completed
                    .push(RequestContentBlock::ProviderEncryptedReasoning {
                        id: format!("anthropic-redacted-{index}"),
                        summary: Vec::new(),
                        encrypted_content: Some(data),
                        status: Some("redacted_thinking".to_owned()),
                    });
                Ok(Vec::new())
            }
            ActiveBlock::ToolUse {
                id,
                name,
                initial_input,
                input_json,
            } => {
                let input = if input_json.is_empty() {
                    initial_input
                } else {
                    serde_json::from_str(&input_json).map_err(|source| {
                        ProviderError::fatal(ToolInputError {
                            tool_use_id: id.clone(),
                            source,
                        })
                    })?
                };
                self.completed.push(RequestContentBlock::ToolUse {
                    id: id.clone(),
                    name,
                    input,
                    thought_signature: None,
                });
                Ok(vec![StreamEvent::ToolUseEnd { id }])
            }
            ActiveBlock::Unknown => Ok(Vec::new()),
        }
    }
}

#[derive(Debug)]
struct StreamState {
    provider: String,
    requested_model: String,
    warned_model_substitution: bool,
    active: BTreeMap<u64, ActiveBlock>,
    usage: Usage,
    failed: bool,
}

impl StreamState {
    fn new(provider: String, requested_model: String) -> Self {
        Self {
            provider,
            requested_model,
            warned_model_substitution: false,
            active: BTreeMap::new(),
            usage: Usage::default(),
            failed: false,
        }
    }
}

#[derive(Debug)]
enum ActiveBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        initial_input: Value,
        input_json: String,
    },
    Unknown,
}

impl ActiveBlock {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Thinking { .. } => "thinking",
            Self::RedactedThinking { .. } => "redacted_thinking",
            Self::ToolUse { .. } => "tool_use",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiEvent {
    MessageStart {
        message: ApiMessage,
    },
    ContentBlockStart {
        index: u64,
        content_block: ApiContentBlock,
    },
    ContentBlockDelta {
        index: u64,
        delta: ApiDelta,
    },
    ContentBlockStop {
        index: u64,
    },
    MessageDelta {
        delta: ApiMessageDelta,
        #[serde(default)]
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        error: AnthropicErrorBody,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ApiMessage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(other)]
    Unknown,
}

impl ApiDelta {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text_delta",
            Self::InputJson { .. } => "input_json_delta",
            Self::Thinking { .. } => "thinking_delta",
            Self::Signature { .. } => "signature_delta",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ApiMessageDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

impl Usage {
    fn update(&mut self, update: Self) {
        if update.input_tokens.is_some() {
            self.input_tokens = update.input_tokens;
        }
        if update.output_tokens.is_some() {
            self.output_tokens = update.output_tokens;
        }
        if update.cache_read_input_tokens.is_some() {
            self.cache_read_input_tokens = update.cache_read_input_tokens;
        }
        if update.cache_creation_input_tokens.is_some() {
            self.cache_creation_input_tokens = update.cache_creation_input_tokens;
        }
    }

    const fn has_any(self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_input_tokens.is_some()
            || self.cache_creation_input_tokens.is_some()
    }
}

fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => FinishReason::Stop,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn model_base(model: &str) -> String {
    model
        .strip_suffix("-1m")
        .unwrap_or(model)
        .to_ascii_lowercase()
}

#[derive(Debug)]
enum ProtocolError {
    AlreadyFinished,
    DuplicateBlock(u64),
    DeltaWithoutBlock(u64),
    StopWithoutBlock(u64),
    UnsignedThinking(u64),
    IncompleteContentBlocks(Vec<u64>),
    MismatchedDelta {
        index: u64,
        block: &'static str,
        delta: &'static str,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyFinished => {
                formatter.write_str("Anthropic SSE decoder is already finished")
            }
            Self::DuplicateBlock(index) => {
                write!(formatter, "Anthropic started content block {index} twice")
            }
            Self::DeltaWithoutBlock(index) => {
                write!(formatter, "Anthropic sent a delta for absent block {index}")
            }
            Self::StopWithoutBlock(index) => {
                write!(formatter, "Anthropic stopped absent block {index}")
            }
            Self::UnsignedThinking(index) => write!(
                formatter,
                "Anthropic thinking block {index} ended without its required signature"
            ),
            Self::IncompleteContentBlocks(indices) => write!(
                formatter,
                "Anthropic stream ended with incomplete content blocks {indices:?}"
            ),
            Self::MismatchedDelta {
                index,
                block,
                delta,
            } => write!(
                formatter,
                "Anthropic sent {delta} for {block} content block {index}"
            ),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug, thiserror::Error)]
#[error("Anthropic tool `{tool_use_id}` ended with invalid input JSON: {source}")]
struct ToolInputError {
    tool_use_id: String,
    #[source]
    source: serde_json::Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authored_sse(frames: &[(&str, Value)]) -> Vec<u8> {
        let mut body = String::new();
        for (event, data) in frames {
            body.push_str("event: ");
            body.push_str(event);
            body.push_str("\ndata: ");
            body.push_str(&serde_json::to_string(data).expect("json"));
            body.push_str("\n\n");
        }
        body.into_bytes()
    }

    #[test]
    fn authored_interleaved_thinking_text_and_two_tools_accumulates_exactly() {
        let bytes = authored_sse(&[
            (
                "message_start",
                serde_json::json!({
                    "type": "message_start",
                    "message": { "model": "claude-sonnet-4-6", "usage": { "input_tokens": 12 } }
                }),
            ),
            (
                "content_block_start",
                serde_json::json!({
                    "type": "content_block_start", "index": 0,
                    "content_block": { "type": "thinking", "thinking": "", "signature": "" }
                }),
            ),
            (
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": { "type": "thinking_delta", "thinking": "check both tools" }
                }),
            ),
            (
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": { "type": "signature_delta", "signature": "sig-abc-123" }
                }),
            ),
            (
                "content_block_stop",
                serde_json::json!({ "type": "content_block_stop", "index": 0 }),
            ),
            (
                "content_block_start",
                serde_json::json!({
                    "type": "content_block_start", "index": 1,
                    "content_block": { "type": "text", "text": "" }
                }),
            ),
            (
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta", "index": 1,
                    "delta": { "type": "text_delta", "text": "Using two tools." }
                }),
            ),
            (
                "content_block_stop",
                serde_json::json!({ "type": "content_block_stop", "index": 1 }),
            ),
            (
                "content_block_start",
                serde_json::json!({
                    "type": "content_block_start", "index": 2,
                    "content_block": { "type": "tool_use", "id": "toolu_a", "name": "alpha", "input": {} }
                }),
            ),
            (
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta", "index": 2,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"x\":1}" }
                }),
            ),
            (
                "content_block_stop",
                serde_json::json!({ "type": "content_block_stop", "index": 2 }),
            ),
            (
                "content_block_start",
                serde_json::json!({
                    "type": "content_block_start", "index": 3,
                    "content_block": { "type": "tool_use", "id": "toolu_b", "name": "beta", "input": {} }
                }),
            ),
            (
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta", "index": 3,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"y\":2}" }
                }),
            ),
            (
                "content_block_stop",
                serde_json::json!({ "type": "content_block_stop", "index": 3 }),
            ),
            (
                "message_delta",
                serde_json::json!({
                    "type": "message_delta", "delta": { "stop_reason": "tool_use" },
                    "usage": { "output_tokens": 20 }
                }),
            ),
            (
                "message_stop",
                serde_json::json!({ "type": "message_stop" }),
            ),
        ]);

        let mut decoder = AnthropicDecoder::new("anthropic", "claude-sonnet-4-6");
        let events = decoder
            .push(&bytes)
            .into_iter()
            .chain(decoder.finish())
            .collect::<Result<Vec<_>, _>>()
            .expect("decode");

        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningStart,
                StreamEvent::ReasoningDelta("check both tools".to_owned()),
                StreamEvent::ReasoningSignatureDelta("sig-abc-123".to_owned()),
                StreamEvent::ReasoningEnd,
                StreamEvent::TextDelta("Using two tools.".to_owned()),
                StreamEvent::ToolUseStart {
                    id: "toolu_a".to_owned(),
                    name: "alpha".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "toolu_a".to_owned(),
                    delta: "{\"x\":1}".to_owned(),
                },
                StreamEvent::ToolUseEnd {
                    id: "toolu_a".to_owned(),
                },
                StreamEvent::ToolUseStart {
                    id: "toolu_b".to_owned(),
                    name: "beta".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "toolu_b".to_owned(),
                    delta: "{\"y\":2}".to_owned(),
                },
                StreamEvent::ToolUseEnd {
                    id: "toolu_b".to_owned(),
                },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
                StreamEvent::TokenUsage {
                    input_tokens: Some(12),
                    output_tokens: Some(20),
                    cache_read_input_tokens: None,
                    cache_write_input_tokens: None,
                    accounting: PromptAccounting::CacheBesideInput,
                },
            ]
        );
        assert_eq!(
            decoder.completed_blocks(),
            [
                RequestContentBlock::SignedThinking {
                    thinking: "check both tools".to_owned(),
                    signature: "sig-abc-123".to_owned(),
                },
                RequestContentBlock::Text {
                    text: "Using two tools.".to_owned(),
                },
                RequestContentBlock::ToolUse {
                    id: "toolu_a".to_owned(),
                    name: "alpha".to_owned(),
                    input: serde_json::json!({ "x": 1 }),
                    thought_signature: None,
                },
                RequestContentBlock::ToolUse {
                    id: "toolu_b".to_owned(),
                    name: "beta".to_owned(),
                    input: serde_json::json!({ "y": 2 }),
                    thought_signature: None,
                },
            ]
        );
    }

    #[test]
    fn authored_model_substitution_surfaces_one_warning_event() {
        let bytes = authored_sse(&[
            (
                "message_start",
                serde_json::json!({
                    "type": "message_start",
                    "message": { "model": "claude-haiku-4-5", "usage": {} }
                }),
            ),
            (
                "message_start",
                serde_json::json!({
                    "type": "message_start",
                    "message": { "model": "claude-haiku-4-5", "usage": {} }
                }),
            ),
        ]);
        let mut decoder = AnthropicDecoder::new("anthropic", "claude-fable-5");
        let events = decoder
            .push(&bytes)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("decode");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            StreamEvent::StatusDetail { detail }
                if detail == "⚠ Anthropic served 'claude-haiku-4-5' instead of requested 'claude-fable-5' (requested model unavailable)"
        ));
    }
}
