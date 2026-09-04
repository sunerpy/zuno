//! Translating Chat Completions and Responses frames into shared events.
//!
//! # What this module is not
//!
//! It is not an SSE parser. [`zuno_llm::sse::SseParser`] owns framing and UTF-8
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

use serde::Deserialize;
use zuno_error::{ProviderError, ProviderProtocolFailure, ProviderStreamFailure};
use zuno_llm::event::PromptAccounting;
use zuno_llm::registry::{ApiSurface, FinishReason, StreamEvent};
use zuno_llm::sse::{
    StreamLimits, append_tool_input, ensure_tool_input_size, upstream_stream_incomplete,
};

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
        Self::with_tool_input_limit(
            provider,
            model,
            surface,
            StreamLimits::from_environment().max_tool_input_bytes(),
        )
    }

    pub(crate) fn with_tool_input_limit(
        provider: impl Into<String>,
        model: impl Into<String>,
        surface: ApiSurface,
        tool_input_limit: usize,
    ) -> Self {
        let provider = provider.into();
        let model = model.into();
        if surface == ApiSurface::Responses {
            Self::Responses(ResponsesTranslator::with_tool_input_limit(
                provider,
                model,
                tool_input_limit,
            ))
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
    ///
    /// # Errors
    ///
    /// [`ProviderError::Stream`] with
    /// [`ProviderStreamFailure::UpstreamStreamIncomplete`] when the byte stream
    /// ended before this surface's terminator arrived.
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
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
    tools: BTreeMap<u32, ActiveChatTool>,
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
            tools: BTreeMap::new(),
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
            let mut events = self.close_open_blocks();
            if !self.ended {
                // `[DONE]` terminates the message on its own. Several
                // OpenAI-compatible vendors send it without ever sending a
                // `finish_reason`, so demanding both markers would turn every one
                // of those providers into a permanent failure.
                self.ended = true;
                events.push(StreamEvent::MessageEnd { stop_reason: None });
            }
            return Ok(events);
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
                accounting: PromptAccounting::CacheInsideInput,
            });
        }

        Ok(events)
    }

    /// Close whatever the stream left open when it ends.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Stream`] with
    /// [`ProviderStreamFailure::UpstreamStreamIncomplete`] when neither a
    /// `finish_reason` nor `[DONE]` ever arrived. A vendor that drops the
    /// connection after its last content chunk has not produced a turn: this used
    /// to synthesize `MessageEnd`, which made the engine commit the truncated half
    /// of the assistant message as complete durable history with no retry.
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.ended {
            return Ok(self.close_open_blocks());
        }
        Err(upstream_stream_incomplete(&self.provider, &self.model))
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
            let index = call.index.unwrap_or(0);
            let function = call.function.as_ref();
            let name = function.and_then(|function| function.name.as_deref());
            if !self.tools.contains_key(&index) {
                if self.reasoning_open {
                    self.reasoning_open = false;
                    events.push(StreamEvent::ReasoningEnd);
                }
                let id = tool_call_identity(call.id.clone(), index);
                let name = name.unwrap_or_default().to_owned();
                events.push(StreamEvent::ToolUseStart {
                    id: id.clone(),
                    name: name.clone(),
                });
                self.tools.insert(index, ActiveChatTool { id });
            }
            // An id arriving on a later fragment does not rename the call. `ToolUseStart`
            // already went out under the identity chosen above, and the engine correlates
            // every later `ToolInputDelta` and `ToolUseEnd` by that exact string, so a
            // rename would turn the rest of this call into events for a call nobody
            // started and fail the turn.
            if let Some(arguments) = function.and_then(|function| function.arguments.as_deref())
                && !arguments.is_empty()
                && let Some(tool) = self.tools.get(&index)
            {
                events.push(StreamEvent::ToolInputDelta {
                    id: tool.id.clone(),
                    delta: arguments.to_owned(),
                });
            }
        }
    }

    fn close_open_blocks(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for (_, tool) in std::mem::take(&mut self.tools) {
            events.push(StreamEvent::ToolUseEnd { id: tool.id });
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
    tool_input_limit: usize,
    reasoning: BTreeMap<String, ActiveReasoning>,
    /// Open function-call items, keyed by the position their call identity was chosen
    /// from: the item's own id, or the ordinal of an item whose id is empty. The identity
    /// and the key are derived from one value on purpose — keying by the raw item id let
    /// two id-less items collide on `""`, so the second overwrote the first and every
    /// later event for either landed on whichever had been added last. The two kinds of
    /// position are distinct variants rather than two spellings of one string, because a
    /// gateway may issue an item id spelled exactly like a synthesized position
    /// (`item-1`) and then the wire item and the first id-less item collided the same way.
    tools: BTreeMap<ToolKey, ActiveTool>,
    /// Function-call items seen whose own `id` was empty, so a synthesized call identity
    /// for them is numbered rather than derived from an item id two items could share.
    unnamed_items: u32,
    /// The ordinal of the one open function-call item whose own id is empty: its key in
    /// `tools` is [`ToolKey::Unnamed`] of this value.
    ///
    /// Its arguments delta and its `done` arrive under `item_id: ""`, which can only be
    /// routed while exactly one such item is open. A second id-less item opening before
    /// this one closes is refused as a protocol failure rather than guessed at.
    open_unnamed_item: Option<u32>,
    saw_tool: bool,
    ended: bool,
    done: bool,
}

impl ResponsesTranslator {
    /// A translator for one Responses request.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_tool_input_limit(
            provider,
            model,
            StreamLimits::from_environment().max_tool_input_bytes(),
        )
    }

    fn with_tool_input_limit(
        provider: impl Into<String>,
        model: impl Into<String>,
        tool_input_limit: usize,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            tool_input_limit,
            reasoning: BTreeMap::new(),
            tools: BTreeMap::new(),
            unnamed_items: 0,
            open_unnamed_item: None,
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
            if self.ended {
                return Ok(Vec::new());
            }
            // `[DONE]` also terminates this surface: a compatible vendor may send
            // it in place of `response.completed`.
            return Ok(self.terminal(ResponseEnvelope::default(), FinishReason::Unknown));
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
                if let Some(key) = self.tool_key(&item_id)
                    && let Some(tool) = self.tools.get_mut(&key)
                {
                    append_tool_input(
                        &mut tool.arguments,
                        &delta,
                        &self.provider,
                        &self.model,
                        self.tool_input_limit,
                    )?;
                    return Ok(vec![StreamEvent::ToolInputDelta {
                        id: tool.call_id.clone(),
                        delta,
                    }]);
                }
                Ok(Vec::new())
            }
            ResponsesEvent::OutputItemDone { item } => self.item_done(item),
            ResponsesEvent::Completed { response } => Ok(self.complete(response, false)),
            ResponsesEvent::Incomplete { response } => {
                Ok(self.terminal(response, FinishReason::Length))
            }
            ResponsesEvent::Failed { response } => Err(classify(
                &self.provider,
                &response.error.unwrap_or_else(|| WireError {
                    message: Some(
                        "Responses stream ended with response.failed without an error body"
                            .to_owned(),
                    ),
                    code: None,
                    kind: Some("response_failed".to_owned()),
                }),
            )),
            ResponsesEvent::Error { error } => Err(classify(&self.provider, &error)),
            ResponsesEvent::Other => Ok(Vec::new()),
        }
    }

    /// Close a Responses stream.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Stream`] with
    /// [`ProviderStreamFailure::UpstreamStreamIncomplete`] when none of
    /// `response.completed`, `response.incomplete`, `response.failed`, or `[DONE]`
    /// arrived before EOF.
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.ended {
            return Ok(Vec::new());
        }
        Err(upstream_stream_incomplete(&self.provider, &self.model))
    }

    fn item_added(&mut self, item: ResponseItem) -> Result<Vec<StreamEvent>, ProviderError> {
        match item.kind.as_str() {
            "reasoning" => {
                self.reasoning.entry(item.id).or_default();
                Ok(Vec::new())
            }
            "function_call" => {
                self.saw_tool = true;
                let unnamed = item.id.is_empty();
                if unnamed && self.open_unnamed_item.is_some() {
                    return Err(self.unresolvable_function_call_item(
                        "a second function_call item with an empty id was added while \
                         another is still open, so their arguments and completions cannot \
                         be told apart",
                    ));
                }
                let key = if unnamed {
                    self.unnamed_items = self.unnamed_items.saturating_add(1);
                    ToolKey::Unnamed(self.unnamed_items)
                } else {
                    ToolKey::Wire(item.id)
                };
                let call_id = tool_call_identity(item.call_id, &key);
                let name = item.name.unwrap_or_default();
                let arguments = item.arguments.unwrap_or_default();
                ensure_tool_input_size(
                    arguments.len(),
                    &self.provider,
                    &self.model,
                    self.tool_input_limit,
                )?;
                if let ToolKey::Unnamed(ordinal) = key {
                    self.open_unnamed_item = Some(ordinal);
                }
                self.tools.insert(
                    key,
                    ActiveTool {
                        call_id: call_id.clone(),
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
                if let Some(arguments) = item.arguments.as_deref() {
                    ensure_tool_input_size(
                        arguments.len(),
                        &self.provider,
                        &self.model,
                        self.tool_input_limit,
                    )?;
                }
                let tracked = self
                    .tool_key(&item.id)
                    .and_then(|key| self.tools.remove(&key));
                if item.id.is_empty() {
                    self.open_unnamed_item = None;
                }
                let call_id = match tracked {
                    Some(tool) => tool.call_id,
                    // An item the stream never added still names its call through its
                    // own id, which is the protocol's correlation key. One with no id at
                    // all names nothing: before, this fell through to
                    // `tool_call_identity(call_id, "")` and ended `zuno-unnamed-call-`,
                    // the bare prefix, for a call nobody started.
                    None if item.id.is_empty() => {
                        return Err(self.unresolvable_function_call_item(
                            "a function_call item with an empty id was completed while no \
                             such item is open",
                        ));
                    }
                    None => tool_call_identity(item.call_id, &item.id),
                };
                Ok(vec![StreamEvent::ToolUseEnd { id: call_id }])
            }
            _ => Ok(Vec::new()),
        }
    }

    /// The `tools` key an event's `item_id` refers to.
    ///
    /// A non-empty item id is its own key. The empty id can only mean the one open
    /// id-less item, and means nothing when there is none.
    fn tool_key(&self, item_id: &str) -> Option<ToolKey> {
        if item_id.is_empty() {
            self.open_unnamed_item.map(ToolKey::Unnamed)
        } else {
            Some(ToolKey::Wire(item_id.to_owned()))
        }
    }

    fn unresolvable_function_call_item(&self, detail: &'static str) -> ProviderError {
        ProviderError::Protocol {
            code: ProviderProtocolFailure::InvalidUpstreamToolCall,
            source: Some(Box::new(UnresolvableFunctionCallItem {
                provider: self.provider.clone(),
                model: self.model.clone(),
                detail,
            })),
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
                accounting: PromptAccounting::CacheInsideInput,
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
struct ActiveChatTool {
    id: String,
}

#[derive(Debug)]
struct ActiveTool {
    call_id: String,
    arguments: String,
}

/// The position a Responses function-call item is tracked under, and the value its
/// synthesized identity is spelled from when the gateway sent no `call_id`.
///
/// A wire item id and a synthesized ordinal are different variants, not two spellings of
/// one string: an item id is whatever the gateway chose, so no string could be reserved for
/// the ordinals without a gateway being able to send it. The `Display` spelling of an
/// ordinal is `item-<n>`, the same position text as before, so the identity an id-less
/// call carries into the engine — `zuno-unnamed-call-item-<n>` — is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ToolKey {
    /// The item's own non-empty `id`.
    Wire(String),
    /// The 1-based ordinal of an item whose `id` was empty, in order of arrival.
    Unnamed(u32),
}

impl std::fmt::Display for ToolKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(id) => formatter.write_str(id),
            Self::Unnamed(ordinal) => write!(formatter, "item-{ordinal}"),
        }
    }
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
    #[serde(default)]
    error: Option<WireError>,
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
/// Reads the structured string code, error class, and numeric status carried by
/// the body. Every one of those is a field on the wire.
/// [`WireError::message`] is attached as payload and never examined.
#[must_use]
pub fn classify(provider: &str, error: &WireError) -> ProviderError {
    if let Some(code) = error.code_str().and_then(ProviderStreamFailure::from_code) {
        return ProviderError::Stream {
            code,
            source: Some(Box::new(ReportedWireError::new(provider, error))),
        };
    }
    if let Some(code) = error
        .code_str()
        .and_then(ProviderProtocolFailure::from_code)
    {
        return ProviderError::Protocol {
            code,
            source: Some(Box::new(ReportedWireError::new(provider, error))),
        };
    }
    if error.code_str() == Some("upstream_error") {
        return ProviderError::fatal(ReportedWireError::new(provider, error));
    }
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
    if matches!(
        error.kind.as_deref(),
        Some("authentication_error" | "permission_error")
    ) {
        return ProviderError::Auth {
            provider: provider.to_owned(),
            source: Some(Box::new(ReportedWireError::new(provider, error))),
        };
    }
    if error.kind.as_deref() == Some("rate_limit_error") {
        return ProviderError::RateLimited { retry_after: None };
    }
    if error.kind.as_deref() == Some("server_error") {
        return ProviderError::Transient {
            status: None,
            source: Some(Box::new(ReportedWireError::new(provider, error))),
        };
    }
    if let Some(status) = error.status() {
        return ProviderError::from_status(provider, status);
    }
    if error.kind.as_deref() == Some("insufficient_quota") {
        return ProviderError::fatal(ReportedWireError::new(provider, error));
    }
    ProviderError::fatal(ReportedWireError::new(provider, error))
}

#[derive(Debug)]
struct ReportedWireError {
    provider: String,
    message: Option<String>,
    code: Option<serde_json::Value>,
    kind: Option<String>,
}

impl ReportedWireError {
    fn new(provider: &str, error: &WireError) -> Self {
        Self {
            provider: provider.to_owned(),
            message: error.message.clone(),
            code: error.code.clone(),
            kind: error.kind.clone(),
        }
    }
}

impl std::fmt::Display for ReportedWireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "provider `{}` reported an error", self.provider)?;
        if let Some(kind) = self.kind.as_deref() {
            write!(formatter, " type={kind}")?;
        }
        if let Some(code) = self.code.as_ref() {
            write!(formatter, " code={code}")?;
        }
        if let Some(message) = self.message.as_deref() {
            write!(formatter, ": {message}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ReportedWireError {}

/// Parse a `Retry-After` header value into a delay.
///
/// Delegates to the single shared parser in [`zuno_llm::http`]. The HTTP-date form
/// used to be discarded here, which silently replaced a peer's stated interval
/// with local backoff; the shared parser accepts it, along with the integer and
/// fractional delta-seconds forms.
#[must_use]
pub fn retry_after(value: &str) -> Option<Duration> {
    zuno_llm::http::parse_retry_after(value)
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

/// A Responses function_call item whose events cannot be correlated with a call.
///
/// The item id is the protocol's own correlation key. When it is empty, the stream can
/// still be followed while exactly one such item is open; the shapes described by
/// `detail` have no reading that does not misattribute an arguments delta or a
/// completion, so they are refused as a typed protocol failure instead of guessed at.
#[derive(Debug)]
struct UnresolvableFunctionCallItem {
    provider: String,
    model: String,
    detail: &'static str,
}

impl std::fmt::Display for UnresolvableFunctionCallItem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "provider `{}` model `{}` sent a Responses stream this client cannot follow: {}",
            self.provider, self.model, self.detail
        )
    }
}

impl std::error::Error for UnresolvableFunctionCallItem {}

/// Prefix of the identity synthesized for a tool call whose gateway supplied no id.
///
/// Distinct from anything a gateway issues (`call_…`, `fc_…`, `toolu_…`) so a synthesized
/// identity can never be mistaken for a real one in a durable row or a log, and never
/// collides with one in the same response.
const SYNTHESIZED_TOOL_CALL_ID_PREFIX: &str = "zuno-unnamed-call-";

/// The identity a tool call carries into the engine and into its durable row.
///
/// A gateway that omits the id, or sends it empty, still gets a call that can name
/// itself. The engine correlates `ToolInputDelta` and `ToolUseEnd` with `ToolUseStart`
/// by this exact string, persists it as the row's `callID`, and replays it on both sides
/// of the next request — `tool_calls[].id` with `tool_call_id`, or `call_id` on both
/// Responses items — so a value invented here is self-consistent on the wire and needs
/// no echo from the peer. Before, an empty id was admitted as `""`: two such calls in one
/// response collided and failed the turn as a duplicate start, and the durable row could
/// not name its own call. `position` is the stream's own correlation key for the call
/// (the chat `index`, or the Responses [`ToolKey`] — the item id, spelled `item-<n>` for
/// the n-th item whose id is empty), and on the Responses surface it is also the key the
/// call is tracked under, so each synthesized identity is distinct within the response
/// and stable for every event of the call.
fn tool_call_identity(wire: Option<String>, position: impl std::fmt::Display) -> String {
    match wire {
        Some(id) if !id.is_empty() => id,
        _ => format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}{position}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_error::Recovery;

    fn translate(frames: &[&str]) -> Vec<StreamEvent> {
        let mut translator = ChunkTranslator::new("test", "model");
        let mut events = Vec::new();
        for frame in frames {
            events.extend(translator.frame(frame).expect("frame translates"));
        }
        events.extend(translator.finish().expect("the stream terminates"));
        events
    }

    fn translate_responses(frames: &[&str]) -> Vec<StreamEvent> {
        let mut translator = SurfaceTranslator::new("test", "model", ApiSurface::Responses);
        let mut events = Vec::new();
        for frame in frames {
            events.extend(translator.frame(frame).expect("frame translates"));
        }
        events.extend(translator.finish().expect("the stream terminates"));
        events
    }

    #[test]
    fn a_chat_stream_cut_off_before_any_terminator_is_a_retryable_stream_failure() {
        let mut translator = ChunkTranslator::new("groq", "llama-4-scout");
        let events = translator
            .frame(r#"{"choices":[{"delta":{"content":"partial"}}]}"#)
            .expect("frame translates");
        assert_eq!(events, vec![StreamEvent::TextDelta("partial".to_owned())]);

        let error = translator
            .finish()
            .expect_err("a truncated chat stream must not be committed as a complete turn");
        let ProviderError::Stream {
            code: ProviderStreamFailure::UpstreamStreamIncomplete,
            ..
        } = &error
        else {
            panic!("expected a typed incomplete-stream failure, got {error:?}");
        };
        assert!(matches!(error.recovery(), Recovery::Retry { .. }));
        assert!(error.permits_partial_output_retry());
    }

    #[test]
    fn a_responses_stream_cut_off_before_any_terminator_is_a_retryable_stream_failure() {
        let mut translator =
            SurfaceTranslator::new("kiro-local", "gpt-5.6-sol", ApiSurface::Responses);
        let events = translator
            .frame(r#"{"type":"response.output_text.delta","delta":"partial"}"#)
            .expect("frame translates");
        assert_eq!(events, vec![StreamEvent::TextDelta("partial".to_owned())]);

        let error = translator
            .finish()
            .expect_err("a truncated responses stream must not be committed as a complete turn");
        assert!(matches!(
            error,
            ProviderError::Stream {
                code: ProviderStreamFailure::UpstreamStreamIncomplete,
                ..
            }
        ));
    }

    #[test]
    fn a_chat_stream_with_only_a_done_sentinel_completes() {
        let events = translate(&[r#"{"choices":[{"delta":{"content":"hi"}}]}"#, "[DONE]"]);
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("hi".to_owned()),
                StreamEvent::MessageEnd { stop_reason: None },
            ]
        );
    }

    #[test]
    fn a_chat_stream_with_only_a_finish_reason_completes() {
        let events =
            translate(&[r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#]);
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("hi".to_owned()),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ]
        );
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
                StreamEvent::ToolInputDelta {
                    id: "call_1".to_owned(),
                    delta: "{\"q\":1}".to_owned(),
                },
                StreamEvent::ToolUseEnd {
                    id: "call_1".to_owned(),
                },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
                StreamEvent::TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(4),
                    cache_read_input_tokens: Some(3),
                    cache_write_input_tokens: None,
                    accounting: PromptAccounting::CacheInsideInput,
                },
            ]
        );
    }

    #[test]
    fn responses_failed_preserves_the_structured_provider_error() {
        let mut translator =
            SurfaceTranslator::new("kiro-local", "claude-opus-5", ApiSurface::Responses);
        let error = translator
            .frame(
                r#"{"type":"response.failed","response":{"status":"failed","error":{"message":"tool projection was rejected","type":"invalid_request_error","code":"unsupported_tool_projection"}}}"#,
            )
            .expect_err("response.failed must not become an ordinary message end");

        let ProviderError::Fatal {
            status: None,
            source: Some(source),
        } = error
        else {
            panic!("structured failed response must remain a fatal provider error");
        };
        let rendered = source.to_string();
        assert!(rendered.contains("invalid_request_error"));
        assert!(rendered.contains("unsupported_tool_projection"));
        assert!(rendered.contains("tool projection was rejected"));
    }

    #[test]
    fn responses_failed_server_error_without_status_is_transient() {
        let mut translator =
            SurfaceTranslator::new("kiro-local", "gpt-5.6-sol", ApiSurface::Responses);
        let error = translator
            .frame(
                r#"{"type":"response.failed","response":{"status":"failed","error":{"message":"upstream temporarily unavailable","type":"server_error","code":null}}}"#,
            )
            .expect_err("response.failed must remain a typed provider error");

        let ProviderError::Transient {
            status: None,
            source: Some(source),
        } = error
        else {
            panic!("type-only server_error must be transient");
        };
        assert!(source.to_string().contains("server_error"));
    }

    #[test]
    fn kiro_stream_failure_codes_are_retryable_replacement_attempts() {
        let cases = [
            (
                "upstream_stream_error",
                ProviderStreamFailure::UpstreamStreamError,
            ),
            (
                "upstream_stream_incomplete",
                ProviderStreamFailure::UpstreamStreamIncomplete,
            ),
            (
                "upstream_stream_idle_timeout",
                ProviderStreamFailure::UpstreamStreamIdleTimeout,
            ),
            (
                "malformed_upstream_tool_arguments",
                ProviderStreamFailure::MalformedUpstreamToolArguments,
            ),
            (
                "request_deadline_exceeded",
                ProviderStreamFailure::RequestDeadlineExceeded,
            ),
        ];

        for (wire_code, expected) in cases {
            let error = classify(
                "kiro-local",
                &WireError {
                    message: Some("stream failed".to_owned()),
                    code: Some(serde_json::json!(wire_code)),
                    kind: Some("upstream_error".to_owned()),
                },
            );
            assert!(matches!(
                error,
                ProviderError::Stream { code, .. } if code == expected
            ));
            assert_eq!(error.recovery(), Recovery::Retry { after: None });
            assert!(error.permits_partial_output_retry());
        }
    }

    #[test]
    fn kiro_protocol_failure_codes_are_terminal() {
        let cases = [
            (
                "upstream_protocol_error",
                ProviderProtocolFailure::UpstreamProtocolError,
            ),
            (
                "invalid_upstream_tool_call",
                ProviderProtocolFailure::InvalidUpstreamToolCall,
            ),
            (
                "invalid_upstream_reasoning",
                ProviderProtocolFailure::InvalidUpstreamReasoning,
            ),
        ];

        for (wire_code, expected) in cases {
            let error = classify(
                "kiro-local",
                &WireError {
                    message: Some("protocol failed".to_owned()),
                    code: Some(serde_json::json!(wire_code)),
                    kind: Some("upstream_protocol_error".to_owned()),
                },
            );
            assert!(matches!(
                error,
                ProviderError::Protocol { code, .. } if code == expected
            ));
            assert_eq!(error.recovery(), Recovery::Fail);
            assert!(!error.permits_partial_output_retry());
        }
    }

    #[test]
    fn legacy_generic_upstream_error_remains_terminal() {
        let error = classify(
            "kiro-local",
            &WireError {
                message: Some("ambiguous legacy error".to_owned()),
                code: Some(serde_json::json!("upstream_error")),
                kind: Some("server_error".to_owned()),
            },
        );

        assert!(matches!(error, ProviderError::Fatal { .. }));
        assert_eq!(error.recovery(), Recovery::Fail);
    }

    #[test]
    fn responses_failed_rate_limit_error_without_status_is_rate_limited() {
        let mut translator =
            SurfaceTranslator::new("kiro-local", "gpt-5.6-sol", ApiSurface::Responses);
        let error = translator
            .frame(
                r#"{"type":"response.failed","response":{"status":"failed","error":{"message":"slow down","type":"rate_limit_error","code":null}}}"#,
            )
            .expect_err("response.failed must remain a typed provider error");

        assert!(matches!(
            error,
            ProviderError::RateLimited { retry_after: None }
        ));
    }

    #[test]
    fn responses_failed_authentication_error_without_status_is_auth() {
        let mut translator =
            SurfaceTranslator::new("kiro-local", "gpt-5.6-sol", ApiSurface::Responses);
        let error = translator
            .frame(
                r#"{"type":"response.failed","response":{"status":"failed","error":{"message":"token expired","type":"authentication_error","code":null}}}"#,
            )
            .expect_err("response.failed must remain a typed provider error");

        let ProviderError::Auth {
            provider,
            source: Some(source),
        } = error
        else {
            panic!("type-only authentication_error must be an auth failure");
        };
        assert_eq!(provider, "kiro-local");
        assert!(source.to_string().contains("authentication_error"));
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
                StreamEvent::ToolInputDelta {
                    id: "a".to_owned(),
                    delta: "{\"x\":1}".to_owned(),
                },
                StreamEvent::ToolUseStart {
                    id: "b".to_owned(),
                    name: "g".to_owned()
                },
                StreamEvent::ToolInputDelta {
                    id: "b".to_owned(),
                    delta: "{}".to_owned(),
                },
                StreamEvent::ToolUseEnd { id: "a".to_owned() },
                StreamEvent::ToolUseEnd { id: "b".to_owned() },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls)
                },
            ]
        );
    }

    /// The reviewer's input: a gateway that sends no id, absent on one call and the empty
    /// string on the other. Both calls must still name themselves, distinctly, and under
    /// the same identity on every one of their events. Before, both opened as `""` and
    /// the engine refused the second start as a duplicate of the first.
    #[test]
    fn a_chat_tool_call_without_an_id_gets_a_distinct_stable_synthesized_identity() {
        let events = translate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"","function":{"name":"g","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let first = format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}0");
        let second = format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}1");
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseStart {
                    id: first.clone(),
                    name: "f".to_owned()
                },
                StreamEvent::ToolInputDelta {
                    id: first.clone(),
                    delta: "{\"x\":1}".to_owned(),
                },
                StreamEvent::ToolUseStart {
                    id: second.clone(),
                    name: "g".to_owned()
                },
                StreamEvent::ToolInputDelta {
                    id: second.clone(),
                    delta: "{}".to_owned(),
                },
                StreamEvent::ToolUseEnd { id: first },
                StreamEvent::ToolUseEnd { id: second },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls)
                },
            ]
        );
        assert_no_empty_tool_call_id(&events);
    }

    /// An id that only arrives on a later fragment does not rename a call the stream has
    /// already announced; the engine would read the renamed events as a call nobody
    /// started.
    #[test]
    fn a_late_chat_tool_call_id_does_not_rename_the_call_already_announced() {
        let events = translate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_late","function":{"arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        let id = format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}0");
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseStart {
                    id: id.clone(),
                    name: "f".to_owned()
                },
                StreamEvent::ToolInputDelta {
                    id: id.clone(),
                    delta: "{}".to_owned(),
                },
                StreamEvent::ToolUseEnd { id },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls)
                },
            ]
        );
    }

    /// A gateway-issued id is carried through untouched: synthesis is a fallback, never a
    /// rewrite of an identity the peer will recognise.
    #[test]
    fn a_gateway_issued_tool_call_id_is_never_replaced() {
        let events = translate(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_real","function":{"name":"f","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert!(
            events.iter().all(|event| match event {
                StreamEvent::ToolUseStart { id, .. }
                | StreamEvent::ToolInputDelta { id, .. }
                | StreamEvent::ToolUseEnd { id } => id == "call_real",
                _ => true,
            }),
            "{events:#?}"
        );
    }

    /// The Responses surface has the same hole: `call_id` empty on one item and absent
    /// on the next. The item id is the protocol's own correlation key, so it seeds the
    /// synthesized identity; an item whose own id is also empty is numbered instead.
    #[test]
    fn a_responses_function_call_without_a_call_id_gets_a_stable_synthesized_identity() {
        let events = translate_responses(&[
            r#"{"type":"response.output_item.added","item":{"id":"fc_9","type":"function_call","call_id":"","name":"lookup","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_9","delta":"{\"q\":1}"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"fc_9","type":"function_call","call_id":"","name":"lookup","arguments":"{\"q\":1}"}}"#,
            r#"{"type":"response.output_item.added","item":{"id":"fc_10","type":"function_call","name":"lookup","arguments":""}}"#,
            r#"{"type":"response.output_item.done","item":{"id":"fc_10","type":"function_call","name":"lookup","arguments":"{}"}}"#,
            r#"{"type":"response.output_item.added","item":{"id":"","type":"function_call","call_id":"","name":"lookup","arguments":""}}"#,
            r#"{"type":"response.output_item.done","item":{"id":"","type":"function_call","call_id":"","name":"lookup","arguments":"{}"}}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ]);
        let first = format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}fc_9");
        let second = format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}fc_10");
        let third = format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}item-1");
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseStart {
                    id: first.clone(),
                    name: "lookup".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: first.clone(),
                    delta: "{\"q\":1}".to_owned(),
                },
                StreamEvent::ToolUseEnd { id: first },
                StreamEvent::ToolUseStart {
                    id: second.clone(),
                    name: "lookup".to_owned(),
                },
                StreamEvent::ToolUseEnd { id: second },
                StreamEvent::ToolUseStart {
                    id: third.clone(),
                    name: "lookup".to_owned(),
                },
                StreamEvent::ToolUseEnd { id: third },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
            ]
        );
        assert_no_empty_tool_call_id(&events);
    }

    /// The reviewer's batched shape: two function_call items whose own ids are both empty
    /// are opened before either closes. Their arguments delta and their completions all
    /// arrive under `item_id: ""`, so nothing in the stream says which call each belongs
    /// to. Before, the tracking map was keyed by that raw empty id: the second item
    /// overwrote the first, the delta landed on the wrong call, the first `done` ended the
    /// second call, and the second `done` fell through to an `End` for
    /// `zuno-unnamed-call-` — an identity nobody started. An ambiguity the protocol cannot
    /// resolve is refused as a typed failure at the moment it arises, never guessed.
    #[test]
    fn a_second_open_function_call_item_without_an_id_is_a_typed_protocol_failure() {
        let (events, error) = translate_responses_until_error(&[
            r#"{"type":"response.output_item.added","item":{"id":"","type":"function_call","call_id":"","name":"a","arguments":""}}"#,
            r#"{"type":"response.output_item.added","item":{"id":"","type":"function_call","call_id":"","name":"b","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"","delta":"{\"for\":\"a\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"","type":"function_call","call_id":"","name":"a","arguments":"{\"for\":\"a\"}"}}"#,
            r#"{"type":"response.output_item.done","item":{"id":"","type":"function_call","call_id":"","name":"b","arguments":"{}"}}"#,
        ]);
        assert_eq!(
            events,
            vec![StreamEvent::ToolUseStart {
                id: format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}item-1"),
                name: "a".to_owned(),
            }],
            "only the first, unambiguous item may open"
        );
        let error = error.expect("a second open id-less item cannot be told from the first");
        assert!(
            matches!(
                error,
                ProviderError::Protocol {
                    code: ProviderProtocolFailure::InvalidUpstreamToolCall,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(error.recovery(), Recovery::Fail);
    }

    /// The same id-less items one after the other are unambiguous: while exactly one
    /// id-less item is open, `item_id: ""` can only mean that one, so its delta and its
    /// `done` are routed to it, and the next id-less item gets the next number.
    #[test]
    fn sequential_function_call_items_without_ids_are_routed_to_the_one_open_item() {
        let events = translate_responses(&[
            r#"{"type":"response.output_item.added","item":{"id":"","type":"function_call","call_id":"","name":"a","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"","delta":"{\"for\":\"a\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"","type":"function_call","call_id":"","name":"a","arguments":"{\"for\":\"a\"}"}}"#,
            r#"{"type":"response.output_item.added","item":{"id":"","type":"function_call","call_id":"","name":"b","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"","delta":"{\"for\":\"b\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"","type":"function_call","call_id":"","name":"b","arguments":"{\"for\":\"b\"}"}}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ]);
        let first = format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}item-1");
        let second = format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}item-2");
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseStart {
                    id: first.clone(),
                    name: "a".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: first.clone(),
                    delta: "{\"for\":\"a\"}".to_owned(),
                },
                StreamEvent::ToolUseEnd { id: first },
                StreamEvent::ToolUseStart {
                    id: second.clone(),
                    name: "b".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: second.clone(),
                    delta: "{\"for\":\"b\"}".to_owned(),
                },
                StreamEvent::ToolUseEnd { id: second },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
            ]
        );
        assert_no_empty_tool_call_id(&events);
    }

    /// The reviewer's collision: a gateway whose real item id is literally `item-1` — the
    /// spelling the translator synthesizes for the first id-less item — is open alongside
    /// an id-less item. Before, both were tracked under the one string `"item-1"`: the
    /// id-less item overwrote the wire one, `a`'s arguments delta and `done` landed on
    /// `b`'s synthesized identity, and `b`'s own `done` then found nothing open and failed
    /// the turn. A wire id and a synthesized position are different variants of one typed
    /// key, so no spelling a gateway can send is equal to a synthesized one. `b` is the
    /// first id-less item, so it is `item-1` in its own namespace regardless of what the
    /// wire ids around it are called.
    #[test]
    fn a_wire_item_id_spelled_like_a_synthesized_position_does_not_collide_with_it() {
        let events = translate_responses(&[
            r#"{"type":"response.output_item.added","item":{"id":"item-1","type":"function_call","call_id":"call_a","name":"a","arguments":""}}"#,
            r#"{"type":"response.output_item.added","item":{"id":"","type":"function_call","name":"b","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"{\"for\":\"a\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"item-1","type":"function_call","call_id":"call_a","name":"a","arguments":"{\"for\":\"a\"}"}}"#,
            r#"{"type":"response.output_item.done","item":{"id":"","type":"function_call","name":"b","arguments":"{}"}}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ]);
        let unnamed = format!("{SYNTHESIZED_TOOL_CALL_ID_PREFIX}item-1");
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseStart {
                    id: "call_a".to_owned(),
                    name: "a".to_owned(),
                },
                StreamEvent::ToolUseStart {
                    id: unnamed.clone(),
                    name: "b".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "call_a".to_owned(),
                    delta: "{\"for\":\"a\"}".to_owned(),
                },
                StreamEvent::ToolUseEnd {
                    id: "call_a".to_owned(),
                },
                StreamEvent::ToolUseEnd { id: unnamed },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
            ]
        );
        assert_no_empty_tool_call_id(&events);
    }

    /// A `done` for an id-less function_call item while no id-less item is open names a
    /// call the stream never started. Before, it fell through to
    /// `tool_call_identity("", "")` and emitted `ToolUseEnd { id: "zuno-unnamed-call-" }`:
    /// the bare prefix, ending a call under an identity nobody announced.
    #[test]
    fn a_done_for_an_unnamed_function_call_item_nobody_opened_is_a_typed_protocol_failure() {
        let (events, error) = translate_responses_until_error(&[
            r#"{"type":"response.output_item.done","item":{"id":"","type":"function_call","call_id":"","name":"lookup","arguments":"{}"}}"#,
        ]);
        assert_eq!(
            events,
            Vec::new(),
            "nothing was started, so nothing may end"
        );
        let error = error.expect("an unnamed item nobody opened cannot be closed");
        assert!(
            matches!(
                error,
                ProviderError::Protocol {
                    code: ProviderProtocolFailure::InvalidUpstreamToolCall,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    fn translate_responses_until_error(
        frames: &[&str],
    ) -> (Vec<StreamEvent>, Option<ProviderError>) {
        let mut translator = SurfaceTranslator::new("test", "model", ApiSurface::Responses);
        let mut events = Vec::new();
        for frame in frames {
            match translator.frame(frame) {
                Ok(batch) => events.extend(batch),
                Err(error) => return (events, Some(error)),
            }
        }
        (events, None)
    }

    fn assert_no_empty_tool_call_id(events: &[StreamEvent]) {
        for event in events {
            if let StreamEvent::ToolUseStart { id, .. }
            | StreamEvent::ToolInputDelta { id, .. }
            | StreamEvent::ToolUseEnd { id } = event
            {
                assert!(
                    !id.is_empty(),
                    "an empty tool-call id reached the engine: {event:?}"
                );
            }
        }
    }

    #[test]
    fn the_done_sentinel_ends_the_stream_without_a_second_message_end() {
        let mut translator = ChunkTranslator::new("test", "model");
        let mut events = translator
            .frame(r#"{"choices":[{"delta":{"content":"a"},"finish_reason":"stop"}]}"#)
            .expect("chunk");
        events.extend(translator.frame(DONE_SENTINEL).expect("sentinel"));
        events.extend(translator.finish().expect("the stream terminates"));
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
            accounting: PromptAccounting::CacheInsideInput,
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
    fn retry_after_reads_both_rfc_9110_forms() {
        assert_eq!(retry_after("30"), Some(Duration::from_secs(30)));
        assert_eq!(retry_after(" 5 "), Some(Duration::from_secs(5)));
        assert_eq!(retry_after(""), None);
        assert_eq!(retry_after("soon"), None);
        // The date form used to return `None` here, which discarded the interval
        // the peer actually asked for.
        assert_eq!(
            retry_after("Fri, 01 Jan 2100 00:00:00 GMT"),
            Some(zuno_llm::http::MAX_RETRY_AFTER)
        );
        assert_eq!(retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
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
