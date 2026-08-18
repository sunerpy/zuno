//! OpenAI Chat Completions and Responses SSE decoding.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;
use zuno_error::ProviderError;
use zuno_llm::event::{FinishReason, PromptAccounting, RequestContentBlock, StreamEvent};
use zuno_llm::registry::ApiSurface;
use zuno_llm::sse::{SseEvent, SseParser, append_tool_input, ensure_tool_input_size};

use crate::error::{OpenAiErrorBody, map_stream_error};
use crate::request::resolve_surface;

/// Incremental decoder for both genuine OpenAI streaming protocols.
#[derive(Debug)]
pub struct OpenAiDecoder {
    parser: SseParser,
    protocol: ProtocolDecoder,
    finished: bool,
    failed: bool,
}

impl OpenAiDecoder {
    /// Construct a decoder for one request surface.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>, surface: ApiSurface) -> Self {
        let provider = provider.into();
        let model = model.into();
        let parser = SseParser::for_stream(provider.clone(), model.clone());
        let tool_input_limit = parser.limits().max_tool_input_bytes();
        let protocol = match resolve_surface(surface) {
            ApiSurface::Chat => {
                ProtocolDecoder::Chat(ChatDecoder::new(provider, model, tool_input_limit))
            }
            ApiSurface::Responses | ApiSurface::Default => {
                ProtocolDecoder::Responses(ResponsesDecoder::new(provider, model, tool_input_limit))
            }
            ApiSurface::Messages => ProtocolDecoder::Unsupported,
        };
        Self {
            parser,
            protocol,
            finished: false,
            failed: false,
        }
    }

    /// Feed one arbitrary network chunk through the shared SSE parser.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Result<StreamEvent, ProviderError>> {
        if self.finished {
            return vec![Err(ProviderError::fatal(ProtocolError::AlreadyFinished))];
        }
        if self.failed {
            return Vec::new();
        }
        let frames = self.parser.push(chunk);
        self.decode_frames(frames)
    }

    /// Finish framing and close inferred Chat blocks when needed.
    pub fn finish(&mut self) -> Vec<Result<StreamEvent, ProviderError>> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if self.failed {
            return Vec::new();
        }
        let frames = self.parser.finish();
        let mut output = self.decode_frames(frames);
        if !self.failed {
            output.extend(self.protocol.finish().into_iter().map(Ok));
        }
        output
    }

    /// Fully accumulated replay-safe assistant blocks.
    #[must_use]
    pub fn completed_blocks(&self) -> &[RequestContentBlock] {
        self.protocol.completed_blocks()
    }

    /// Consume fully accumulated replay-safe assistant blocks.
    #[must_use]
    pub fn into_completed_blocks(self) -> Vec<RequestContentBlock> {
        self.protocol.into_completed_blocks()
    }

    fn decode_frames(
        &mut self,
        frames: Vec<Result<SseEvent, ProviderError>>,
    ) -> Vec<Result<StreamEvent, ProviderError>> {
        let mut output = Vec::new();
        for frame in frames {
            let frame = match frame {
                Ok(frame) => frame,
                Err(error) => {
                    self.failed = true;
                    output.push(Err(error));
                    break;
                }
            };
            match self.protocol.frame(&frame) {
                Ok(events) => output.extend(events.into_iter().map(Ok)),
                Err(error) => {
                    self.failed = true;
                    output.push(Err(error));
                    break;
                }
            }
        }
        output
    }
}

#[derive(Debug)]
enum ProtocolDecoder {
    Chat(ChatDecoder),
    Responses(ResponsesDecoder),
    Unsupported,
}

impl ProtocolDecoder {
    fn frame(&mut self, frame: &SseEvent) -> Result<Vec<StreamEvent>, ProviderError> {
        match self {
            Self::Chat(decoder) => decoder.frame(frame),
            Self::Responses(decoder) => decoder.frame(frame),
            Self::Unsupported => Err(ProviderError::fatal(ProtocolError::UnsupportedSurface)),
        }
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        match self {
            Self::Chat(decoder) => decoder.finish(),
            Self::Responses(decoder) => decoder.finish(),
            Self::Unsupported => Vec::new(),
        }
    }

    fn completed_blocks(&self) -> &[RequestContentBlock] {
        match self {
            Self::Chat(decoder) => &decoder.completed,
            Self::Responses(decoder) => &decoder.completed,
            Self::Unsupported => &[],
        }
    }

    fn into_completed_blocks(self) -> Vec<RequestContentBlock> {
        match self {
            Self::Chat(decoder) => decoder.completed,
            Self::Responses(decoder) => decoder.completed,
            Self::Unsupported => Vec::new(),
        }
    }
}

#[derive(Debug)]
struct ChatDecoder {
    provider: String,
    model: String,
    tool_input_limit: usize,
    tools: BTreeMap<u32, ChatTool>,
    text: String,
    completed: Vec<RequestContentBlock>,
    ended: bool,
}

impl ChatDecoder {
    fn new(provider: String, model: String, tool_input_limit: usize) -> Self {
        Self {
            provider,
            model,
            tool_input_limit,
            tools: BTreeMap::new(),
            text: String::new(),
            completed: Vec::new(),
            ended: false,
        }
    }

    fn frame(&mut self, frame: &SseEvent) -> Result<Vec<StreamEvent>, ProviderError> {
        if frame.data.trim() == "[DONE]" {
            return Ok(self.close_message(None));
        }
        let chunk: ChatChunk = frame.deserialize(&self.provider, &self.model)?;
        if let Some(error) = chunk.error {
            return Err(map_stream_error(&self.provider, error));
        }
        let mut events = Vec::new();
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content
                && !text.is_empty()
            {
                self.text.push_str(&text);
                events.push(StreamEvent::TextDelta(text));
            }
            for call in choice.delta.tool_calls {
                let index = call.index.unwrap_or(0);
                if let Some(function) = call.function {
                    if let Some(name) = function.name {
                        if !self.tools.contains_key(&index) {
                            events.push(StreamEvent::ToolUseStart {
                                id: call.id.clone().unwrap_or_default(),
                                name: name.clone(),
                            });
                        }
                        let tool = self.tools.entry(index).or_default();
                        tool.id = call.id.unwrap_or_else(|| tool.id.clone());
                        tool.name = name;
                    }
                    if let Some(arguments) = function.arguments
                        && !arguments.is_empty()
                    {
                        append_tool_input(
                            &mut self.tools.entry(index).or_default().arguments,
                            &arguments,
                            &self.provider,
                            &self.model,
                            self.tool_input_limit,
                        )?;
                        events.push(StreamEvent::ToolInputDelta(arguments));
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                events.extend(self.close_message(Some(chat_finish_reason(&reason))));
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
                accounting: PromptAccounting::CacheInsideInput,
            });
        }
        Ok(events)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        self.close_message(None)
    }

    fn close_message(&mut self, reason: Option<FinishReason>) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        let mut events = Vec::new();
        if !self.text.is_empty() {
            self.completed.push(RequestContentBlock::Text {
                text: std::mem::take(&mut self.text),
            });
        }
        for (_, tool) in std::mem::take(&mut self.tools) {
            let input = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
            self.completed.push(RequestContentBlock::ToolUse {
                id: tool.id,
                name: tool.name,
                input,
                thought_signature: None,
            });
            events.push(StreamEvent::ToolUseEnd);
        }
        events.push(StreamEvent::MessageEnd {
            stop_reason: reason,
        });
        events
    }
}

#[derive(Debug, Default)]
struct ChatTool {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug)]
struct ResponsesDecoder {
    provider: String,
    model: String,
    tool_input_limit: usize,
    active_reasoning: BTreeMap<String, ActiveReasoning>,
    active_tools: BTreeMap<String, ActiveTool>,
    text: String,
    completed: Vec<RequestContentBlock>,
    saw_tool: bool,
    ended: bool,
}

impl ResponsesDecoder {
    fn new(provider: String, model: String, tool_input_limit: usize) -> Self {
        Self {
            provider,
            model,
            tool_input_limit,
            active_reasoning: BTreeMap::new(),
            active_tools: BTreeMap::new(),
            text: String::new(),
            completed: Vec::new(),
            saw_tool: false,
            ended: false,
        }
    }

    fn frame(&mut self, frame: &SseEvent) -> Result<Vec<StreamEvent>, ProviderError> {
        let event: ResponsesEvent = frame.deserialize(&self.provider, &self.model)?;
        match event {
            ResponsesEvent::OutputTextDelta { delta } => {
                self.text.push_str(&delta);
                Ok(vec![StreamEvent::TextDelta(delta)])
            }
            ResponsesEvent::ReasoningSummaryPartAdded { item_id, .. } => {
                let reasoning = self.active_reasoning.entry(item_id).or_default();
                if reasoning.open {
                    Ok(Vec::new())
                } else {
                    reasoning.open = true;
                    Ok(vec![StreamEvent::ReasoningStart])
                }
            }
            ResponsesEvent::ReasoningSummaryTextDelta { item_id, delta, .. } => {
                let reasoning = self.active_reasoning.entry(item_id).or_default();
                let mut events = Vec::new();
                if !reasoning.open {
                    reasoning.open = true;
                    events.push(StreamEvent::ReasoningStart);
                }
                reasoning.current_summary.push_str(&delta);
                events.push(StreamEvent::ReasoningDelta(delta));
                Ok(events)
            }
            ResponsesEvent::ReasoningSummaryTextDone { item_id, text, .. } => {
                let reasoning = self.active_reasoning.entry(item_id).or_default();
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
                if let Some(tool) = self.active_tools.get_mut(&item_id) {
                    append_tool_input(
                        &mut tool.arguments,
                        &delta,
                        &self.provider,
                        &self.model,
                        self.tool_input_limit,
                    )?;
                }
                Ok(vec![StreamEvent::ToolInputDelta(delta)])
            }
            ResponsesEvent::OutputItemDone { item } => self.item_done(item),
            ResponsesEvent::Completed { response } => Ok(self.completed_response(response)),
            ResponsesEvent::Incomplete { response } => {
                Ok(self.terminal_response(response, FinishReason::Length))
            }
            ResponsesEvent::Failed { response } => {
                Ok(self.terminal_response(response, FinishReason::Error))
            }
            ResponsesEvent::Error { error } => Err(map_stream_error(&self.provider, error)),
            ResponsesEvent::Other => Ok(Vec::new()),
        }
    }

    fn item_added(&mut self, item: ResponseItem) -> Result<Vec<StreamEvent>, ProviderError> {
        match item.kind.as_str() {
            "reasoning" => {
                self.active_reasoning.entry(item.id).or_default();
                Ok(Vec::new())
            }
            "function_call" => {
                self.saw_tool = true;
                let id = item.id;
                let call_id = item.call_id.unwrap_or_default();
                let name = item.name.unwrap_or_default();
                let arguments = item.arguments.unwrap_or_default();
                ensure_tool_input_size(
                    arguments.len(),
                    &self.provider,
                    &self.model,
                    self.tool_input_limit,
                )?;
                self.active_tools.insert(
                    id,
                    ActiveTool {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments,
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
                let mut reasoning = self.active_reasoning.remove(&item.id).unwrap_or_default();
                let mut events = Vec::new();
                if reasoning.open {
                    events.push(StreamEvent::ReasoningEnd);
                }
                if let Some(summary) = item.summary {
                    reasoning.summary = summary.into_iter().filter_map(|part| part.text).collect();
                }
                let block = RequestContentBlock::ProviderEncryptedReasoning {
                    id: item.id.clone(),
                    summary: reasoning.summary.clone(),
                    encrypted_content: item.encrypted_content.clone(),
                    status: item.status.clone(),
                };
                self.completed.push(block);
                events.push(StreamEvent::ProviderReasoningItem {
                    id: item.id,
                    summary: reasoning.summary,
                    encrypted_content: item.encrypted_content,
                    status: item.status,
                });
                Ok(events)
            }
            "function_call" => {
                let active = self.active_tools.remove(&item.id).unwrap_or(ActiveTool {
                    call_id: item.call_id.clone().unwrap_or_default(),
                    name: item.name.clone().unwrap_or_default(),
                    arguments: item.arguments.clone().unwrap_or_default(),
                });
                let arguments = item.arguments.unwrap_or(active.arguments);
                ensure_tool_input_size(
                    arguments.len(),
                    &self.provider,
                    &self.model,
                    self.tool_input_limit,
                )?;
                let input = serde_json::from_str(&arguments).map_err(|source| {
                    ProviderError::fatal(ToolInputError {
                        tool_use_id: active.call_id.clone(),
                        source,
                    })
                })?;
                self.completed.push(RequestContentBlock::ToolUse {
                    id: active.call_id,
                    name: active.name,
                    input,
                    thought_signature: None,
                });
                Ok(vec![StreamEvent::ToolUseEnd])
            }
            _ => Ok(Vec::new()),
        }
    }

    fn completed_response(&mut self, response: ResponseEnvelope) -> Vec<StreamEvent> {
        let reason = if self.saw_tool {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };
        self.terminal_response(response, reason)
    }

    fn terminal_response(
        &mut self,
        response: ResponseEnvelope,
        reason: FinishReason,
    ) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        if !self.text.is_empty() {
            self.completed.push(RequestContentBlock::Text {
                text: std::mem::take(&mut self.text),
            });
        }
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
                accounting: PromptAccounting::CacheInsideInput,
            });
        }
        events
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.ended {
            Vec::new()
        } else {
            self.terminal_response(ResponseEnvelope::default(), FinishReason::Unknown)
        }
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
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
    #[serde(default)]
    error: Option<OpenAiErrorBody>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatToolDelta {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<TokenDetails>,
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
        error: OpenAiErrorBody,
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
    input_tokens_details: Option<TokenDetails>,
}

#[derive(Debug, Deserialize)]
struct TokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

fn chat_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

#[derive(Debug)]
enum ProtocolError {
    AlreadyFinished,
    UnsupportedSurface,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyFinished => formatter.write_str("OpenAI SSE decoder is already finished"),
            Self::UnsupportedSurface => {
                formatter.write_str("OpenAI SSE decoder does not support the Messages surface")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug, thiserror::Error)]
#[error("OpenAI tool `{tool_use_id}` ended with invalid input JSON: {source}")]
struct ToolInputError {
    tool_use_id: String,
    #[source]
    source: serde_json::Error,
}
