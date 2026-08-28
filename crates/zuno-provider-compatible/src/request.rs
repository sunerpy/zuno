//! Assembling Chat Completions and Responses request bodies.
//!
//! # The three ordering and gating rules that are not cosmetic
//!
//! 1. **`thinking` is written after `extra_body`.** The models that require the
//!    reasoning-content protocol also require `thinking: {"type": "enabled"}`, and
//!    a caller that put a different `thinking` value in its options bag must not be
//!    able to disable it by accident. Writing it last makes the required shape the
//!    final word
//!    (`claw-code`).
//! 2. **Sampling parameters are gated on a capability**, never on a model name.
//!    See [`crate::quirks`].
//! 3. **`reasoning_content` is echoed only when the model requires it.** Sending it
//!    unconditionally pays tokens on every later turn for a field most vendors did
//!    not send and some reject.
//! 4. **Visible text never shares an assistant message with tool calls**, unless
//!    reasoning is echoed in band by rule 3. A gateway fronting an Anthropic-family
//!    model has nowhere in this wire shape to put the reasoning it sealed, so it
//!    seals it *into the tool call's id* and re-expands it immediately before that
//!    call on replay. The reconstructed turn is therefore `content`, then
//!    `[thinking, tool_use]` — and Anthropic requires replayed thinking to be the
//!    **first** content of its assistant message. Merging the two makes the
//!    reasoning un-first, which such a gateway rejects outright:
//!
//!    ```text
//!    HTTP 400 assistant reasoning prefix does not match the reasoning capsule
//!    ```
//!
//!    Measured against a live gateway: the merged shape is a deterministic 400,
//!    while the split shape and a text-dropping shape both return 200. Splitting is
//!    what this module does, because dropping the text would silently discard the
//!    model's own words mid-turn. When rule 3 applies the split is not needed and
//!    not taken — `reasoning_content` positions the reasoning explicitly, so that
//!    vendor has no capsule to re-expand and wants text on the same message.
//!
//!    The condition is structural, not a model or gateway name: any turn that has
//!    both text and tool calls is replayed this way, for every provider on this
//!    surface.
//!
//! # What `extra_body` may not overwrite
//!
//! [`PROTECTED_KEYS`] is the same set the reference implementation protects
//! (`openai_compat.rs:1234-1243`). These are the fields the profile derives from
//! the request itself; letting an options bag replace `messages` would make the
//! transcript and the wire disagree silently.

use serde_json::{Map, Value, json};
use zuno_llm::event::{Message, RequestContentBlock, Role};
use zuno_llm::registry::ApiSurface;

use crate::quirks::Quirks;

/// Body keys `extra_body` must not replace.
pub const PROTECTED_KEYS: &[&str] = &[
    "model",
    "messages",
    "input",
    "instructions",
    "stream",
    "tools",
    "tool_choice",
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
];

/// Sampling parameters, applied only when the model accepts them.
///
/// Every field is `Option` so "unset" and "set to the default" stay distinct: a
/// `temperature` of `0` is a real instruction and must survive.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sampling {
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling cutoff.
    pub top_p: Option<f64>,
    /// Repetition penalty on frequency.
    pub frequency_penalty: Option<f64>,
    /// Repetition penalty on presence.
    pub presence_penalty: Option<f64>,
}

impl Sampling {
    /// Whether any parameter is set.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.top_p.is_none()
            && self.frequency_penalty.is_none()
            && self.presence_penalty.is_none()
    }
}

/// Everything the profile needs to write one body.
///
/// A struct rather than a ten-argument function so adding a field is not a
/// signature change at every call site, and so a test can build one directly.
#[derive(Debug, Clone, Default)]
pub struct RequestBody {
    /// The model id as the vendor names it.
    pub model: String,
    /// The durable turn, in order.
    pub messages: Vec<Message>,
    /// Volatile non-user context appended as independent protocol items.
    pub developer_context: Vec<String>,
    /// Tool definitions, when the caller has any and the model accepts them.
    ///
    /// A JSON array, passed through rather than re-derived: hand-writing a schema
    /// per tool is the anti-pattern this crate is explicitly not repeating. The
    /// definitions come from `zuno-tool`, which already owns them.
    pub tools: Option<Value>,
    /// Sampling parameters, subject to [`Quirks::accepts_sampling_params`].
    pub sampling: Sampling,
    /// Output cap, written as `max_tokens` on Chat and `max_output_tokens` on
    /// Responses.
    pub max_tokens: Option<u64>,
    /// Which tool, if any, the model must call.
    ///
    /// `None` is not the same as `"auto"` even though the two request the same
    /// behaviour: OpenAI documents `auto` as *the default when tools are present*
    /// and `none` as the default when they are not
    /// (`POST /v1/chat/completions`, `tool_choice`), so omitting the field asks for
    /// exactly what the caller wanted and asks it of endpoints that reject an
    /// explicit value they do not implement. Sending `"auto"` unprompted would be a
    /// wire change with no behavioural one.
    pub tool_choice: Option<Value>,
    /// The caller's own body keys, applied after everything derived.
    ///
    /// This is where reasoning-effort resolution lands: `zuno-llm`'s
    /// [`EffortResolution::apply_to`](zuno_llm::effort::EffortResolution::apply_to)
    /// writes into a `Map`, and that map is this field. Effort policy therefore
    /// stays in one place for every family instead of being re-expressed here.
    pub extra_body: Map<String, Value>,
}

impl RequestBody {
    /// A body for `model` carrying `messages`.
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            ..Self::default()
        }
    }

    /// Serialize this body under the resolved quirks.
    #[must_use]
    pub fn build(&self, quirks: &Quirks) -> Value {
        if quirks.surface == ApiSurface::Responses {
            self.build_responses(quirks)
        } else {
            self.build_chat(quirks)
        }
    }

    /// Write [`tool_choice`](Self::tool_choice), if set.
    ///
    /// Called only from inside the `tools` branch: a `tool_choice` with no `tools`
    /// to choose from is rejected by both surfaces, so a caller that configured one
    /// on a model whose capabilities refuse tools gets no tools *and* no choice,
    /// rather than a request that cannot be served.
    fn insert_tool_choice(&self, body: &mut Map<String, Value>) {
        if let Some(tool_choice) = &self.tool_choice {
            body.insert("tool_choice".to_owned(), tool_choice.clone());
        }
    }

    fn build_chat(&self, quirks: &Quirks) -> Value {
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(self.model));
        let mut messages = self
            .messages
            .iter()
            .flat_map(|message| translate_message(message, quirks))
            .collect::<Vec<_>>();
        messages.extend(
            self.developer_context
                .iter()
                .filter(|content| !content.trim().is_empty())
                .map(|content| json!({"role": "system", "content": content})),
        );
        body.insert("messages".to_owned(), Value::Array(messages));
        body.insert("stream".to_owned(), json!(true));

        if let Some(tools) = &self.tools
            && quirks.accepts_tools()
        {
            body.insert("tools".to_owned(), tools.clone());
            self.insert_tool_choice(&mut body);
        }

        if quirks.accepts_sampling_params() {
            insert_number(&mut body, "temperature", self.sampling.temperature);
            insert_number(&mut body, "top_p", self.sampling.top_p);
            insert_number(
                &mut body,
                "frequency_penalty",
                self.sampling.frequency_penalty,
            );
            insert_number(
                &mut body,
                "presence_penalty",
                self.sampling.presence_penalty,
            );
        }

        if let Some(max_tokens) = self.max_tokens {
            body.insert("max_tokens".to_owned(), json!(max_tokens));
        }

        // Asked for explicitly, because a streaming chat completion does not report usage
        // otherwise: measured against a live gateway, the stream without this field ends
        // `finish_reason` then `[DONE]` with no usage anywhere, and with it a further
        // frame carries `usage` after the finish. Both halves are required — the engine
        // reading past `MessageEnd` has nothing to read unless this is sent, and sending
        // this achieves nothing if the engine stops at `MessageEnd`.
        //
        // Inserted before `extra_body` and deliberately absent from [`PROTECTED_KEYS`],
        // so a provider that rejects the field can be told to omit it from configuration
        // rather than needing a code change.
        body.insert(
            "stream_options".to_owned(),
            json!({ "include_usage": true }),
        );

        for (key, value) in &self.extra_body {
            if PROTECTED_KEYS.contains(&key.as_str()) {
                continue;
            }
            body.insert(key.clone(), value.clone());
        }

        // Rule 1: after `extra_body`, so the required shape cannot be overridden.
        if quirks.reasoning_protocol {
            body.insert("thinking".to_owned(), json!({"type": "enabled"}));
        }

        Value::Object(body)
    }

    fn build_responses(&self, quirks: &Quirks) -> Value {
        let mut input = Vec::new();
        let mut instructions = None;
        for message in &self.messages {
            if message.role == Role::System && instructions.is_none() {
                instructions = Some(joined_text(message));
            } else {
                input.extend(translate_response_message(message, quirks));
            }
        }
        input.extend(
            self.developer_context
                .iter()
                .filter(|content| !content.trim().is_empty())
                .map(|content| json!({"role": "developer", "content": content})),
        );

        let mut body = Map::new();
        body.insert("model".to_owned(), json!(self.model));
        if let Some(instructions) = instructions.filter(|instructions| !instructions.is_empty()) {
            body.insert("instructions".to_owned(), Value::String(instructions));
        }
        body.insert("input".to_owned(), Value::Array(input));
        body.insert("stream".to_owned(), json!(true));

        if let Some(tools) = &self.tools
            && quirks.accepts_tools()
        {
            body.insert("tools".to_owned(), response_tools(tools));
            self.insert_tool_choice(&mut body);
        }

        if quirks.accepts_sampling_params() {
            insert_number(&mut body, "temperature", self.sampling.temperature);
            insert_number(&mut body, "top_p", self.sampling.top_p);
            insert_number(
                &mut body,
                "frequency_penalty",
                self.sampling.frequency_penalty,
            );
            insert_number(
                &mut body,
                "presence_penalty",
                self.sampling.presence_penalty,
            );
        }

        if let Some(max_tokens) = self.max_tokens {
            body.insert("max_output_tokens".to_owned(), json!(max_tokens));
        }

        for (key, value) in &self.extra_body {
            if PROTECTED_KEYS.contains(&key.as_str()) {
                continue;
            }
            body.insert(key.clone(), value.clone());
        }

        Value::Object(body)
    }
}

fn response_tools(tools: &Value) -> Value {
    let Some(tools) = tools.as_array() else {
        return tools.clone();
    };
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                tool.get("function").map_or_else(
                    || tool.clone(),
                    |function| {
                        let mut response = function.as_object().cloned().unwrap_or_default();
                        response.insert("type".to_owned(), json!("function"));
                        Value::Object(response)
                    },
                )
            })
            .collect(),
    )
}

fn insert_number(body: &mut Map<String, Value>, key: &str, value: Option<f64>) {
    if let Some(value) = value {
        body.insert(key.to_owned(), json!(value));
    }
}

/// Translate one message into zero or more wire messages.
///
/// Zero when an assistant turn carried nothing replayable, and more than one when
/// a turn carried several tool results — the chat-completions shape requires one
/// `role: "tool"` message per result, so they cannot be merged.
#[must_use]
pub fn translate_message(message: &Message, quirks: &Quirks) -> Vec<Value> {
    match message.role {
        Role::Assistant => translate_assistant(message, quirks),
        Role::Tool => translate_tool_results(message),
        Role::System | Role::User => translate_plain(message, quirks),
    }
}

fn translate_response_message(message: &Message, quirks: &Quirks) -> Vec<Value> {
    match message.role {
        Role::System => vec![json!({
            "role": "developer",
            "content": joined_text(message),
        })],
        Role::User => vec![json!({
            "role": "user",
            "content": response_content(message, quirks),
        })],
        Role::Assistant => translate_response_assistant(message),
        Role::Tool => translate_response_tool_results(message, quirks),
    }
}

/// Project one assistant turn onto the Responses `input` list, in source order.
///
/// `input` is an ordered history, so "called a tool, then spoke" and "spoke, then
/// called a tool" are different turns as far as the model is concerned. Accumulated
/// text is therefore flushed as its own assistant item *before* each non-text
/// block rather than appended once at the end — the same `flushText()` rule the
/// oracle applies in `packages/llm/src/protocols/openai-responses.ts:381-430`.
fn translate_response_assistant(message: &Message) -> Vec<Value> {
    let mut items = Vec::new();
    let mut content = Vec::new();
    for block in &message.content {
        match block {
            RequestContentBlock::Text { text } => {
                content.push(json!({"type": "output_text", "text": text}));
            }
            RequestContentBlock::ResourceLink { .. } => {
                let Some(text) = block.provider_text() else {
                    unreachable!("resource links always have a provider text projection")
                };
                content.push(json!({"type": "output_text", "text": text.as_ref()}));
            }
            RequestContentBlock::ProviderEncryptedReasoning {
                summary,
                encrypted_content,
                status,
                ..
            } => {
                let Some(encrypted_content) = encrypted_content else {
                    continue;
                };
                let mut item = Map::new();
                item.insert("type".to_owned(), json!("reasoning"));
                item.insert(
                    "summary".to_owned(),
                    Value::Array(
                        summary
                            .iter()
                            .map(|text| json!({"type": "summary_text", "text": text}))
                            .collect(),
                    ),
                );
                item.insert("encrypted_content".to_owned(), json!(encrypted_content));
                if let Some(status) = status {
                    item.insert("status".to_owned(), json!(status));
                }
                flush_response_assistant_text(&mut items, &mut content);
                items.push(Value::Object(item));
            }
            RequestContentBlock::ToolUse {
                id, name, input, ..
            } => {
                flush_response_assistant_text(&mut items, &mut content);
                items.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": input.to_string(),
                }));
            }
            RequestContentBlock::SignedThinking { .. }
            | RequestContentBlock::ToolResult { .. }
            | RequestContentBlock::Image { .. } => {}
        }
    }
    flush_response_assistant_text(&mut items, &mut content);
    items
}

/// Emit the text collected so far as one assistant item, if there is any.
///
/// Called immediately before each emitted item rather than before each non-text
/// *block*, so a block that turns out to contribute nothing — a capsule with no
/// ciphertext, a signature-only thinking block — does not split the surrounding
/// text into two assistant items.
fn flush_response_assistant_text(items: &mut Vec<Value>, content: &mut Vec<Value>) {
    if content.is_empty() {
        return;
    }
    items.push(json!({"role": "assistant", "content": std::mem::take(content)}));
}

fn translate_response_tool_results(message: &Message, quirks: &Quirks) -> Vec<Value> {
    let images = if quirks.accepts_attachments() {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                RequestContentBlock::Image {
                    media_type, data, ..
                } => Some(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                })),
                _ => None,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    message
        .content
        .iter()
        .filter_map(|block| match block {
            RequestContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                let output = if images.is_empty() {
                    Value::String(content.clone())
                } else {
                    let mut parts = vec![json!({"type": "input_text", "text": content})];
                    parts.extend(images.clone());
                    Value::Array(parts)
                };
                Some(json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": output,
                }))
            }
            _ => None,
        })
        .collect()
}

fn response_content(message: &Message, quirks: &Quirks) -> Vec<Value> {
    if quirks.requires_single_response_text_block() {
        return single_text_response_content(message, quirks);
    }
    message
        .content
        .iter()
        .filter_map(|block| match block {
            RequestContentBlock::Text { text } => Some(json!({"type": "input_text", "text": text})),
            RequestContentBlock::ResourceLink { .. } => block
                .provider_text()
                .map(|text| json!({"type": "input_text", "text": text.as_ref()})),
            RequestContentBlock::Image {
                media_type, data, ..
            } if quirks.accepts_attachments() => Some(json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
            })),
            _ => None,
        })
        .collect()
}

const RESPONSE_TEXT_BLOCK_SEPARATOR: &str = concat!("\n", "\n");

/// Lower one typed message for an endpoint exposing only one upstream text field.
///
/// Typed blocks remain unchanged in durable history. Only the provider boundary
/// joins their text projections, using a stable blank-line delimiter. Images stay
/// separate and the joined text occupies the position of the first text fragment.
fn single_text_response_content(message: &Message, quirks: &Quirks) -> Vec<Value> {
    let mut content = Vec::new();
    let mut text = Vec::new();
    let mut text_position = None;
    for block in &message.content {
        match block {
            RequestContentBlock::Text { .. } | RequestContentBlock::ResourceLink { .. } => {
                let Some(fragment) = block.provider_text() else {
                    unreachable!("text and resource links always have a text projection")
                };
                if fragment.is_empty() {
                    continue;
                }
                text_position.get_or_insert(content.len());
                text.push(fragment.into_owned());
            }
            RequestContentBlock::Image {
                media_type, data, ..
            } if quirks.accepts_attachments() => content.push(json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
            })),
            _ => {}
        }
    }
    if !text.is_empty() {
        content.insert(
            text_position.unwrap_or_default(),
            json!({"type": "input_text", "text": text.join(RESPONSE_TEXT_BLOCK_SEPARATOR)}),
        );
    }
    content
}

fn joined_text(message: &Message) -> String {
    let mut text = String::new();
    for block in &message.content {
        if let Some(fragment) = block.provider_text() {
            text.push_str(fragment.as_ref());
        }
    }
    text
}

fn translate_plain(message: &Message, quirks: &Quirks) -> Vec<Value> {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    let mut parts = Vec::new();
    let mut text = String::new();
    let mut has_attachment = false;
    for block in &message.content {
        match block {
            RequestContentBlock::Text { text: value } => {
                text.push_str(value);
                parts.push(json!({"type": "text", "text": value}));
            }
            RequestContentBlock::ResourceLink { .. } => {
                let Some(value) = block.provider_text() else {
                    unreachable!("resource links always have a provider text projection")
                };
                text.push_str(value.as_ref());
                parts.push(json!({"type": "text", "text": value.as_ref()}));
            }
            RequestContentBlock::Image {
                media_type, data, ..
            } if quirks.accepts_attachments() => {
                has_attachment = true;
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{media_type};base64,{data}") }
                }));
            }
            // An attachment a text-only model cannot read is dropped rather than
            // sent: `Capabilities::attachments` exists so this is a decision and
            // not a 400 from the vendor.
            RequestContentBlock::Image { .. }
            | RequestContentBlock::SignedThinking { .. }
            | RequestContentBlock::ProviderEncryptedReasoning { .. }
            | RequestContentBlock::ToolUse { .. }
            | RequestContentBlock::ToolResult { .. } => {}
        }
    }

    if parts.is_empty() {
        return Vec::new();
    }
    // A string content field is what every vendor in the corpus accepts; the
    // parts array is only required once a non-text part is present.
    let content = if has_attachment {
        Value::Array(parts)
    } else {
        json!(text)
    };
    vec![json!({"role": role, "content": content})]
}

fn translate_assistant(message: &Message, quirks: &Quirks) -> Vec<Value> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    for block in &message.content {
        match block {
            RequestContentBlock::Text { text: value } => text.push_str(value),
            RequestContentBlock::ResourceLink { .. } => {
                let Some(value) = block.provider_text() else {
                    unreachable!("resource links always have a provider text projection")
                };
                text.push_str(value.as_ref());
            }
            // The only replayable reasoning shapes `RequestContentBlock` can
            // carry. Both are collected here and then either echoed or dropped;
            // the decision is rule 3, below.
            RequestContentBlock::SignedThinking { thinking, .. } => reasoning.push_str(thinking),
            RequestContentBlock::ProviderEncryptedReasoning { summary, .. } => {
                for line in summary {
                    reasoning.push_str(line);
                }
            }
            RequestContentBlock::ToolUse {
                id, name, input, ..
            } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": input.to_string() }
            })),
            RequestContentBlock::ToolResult { .. } | RequestContentBlock::Image { .. } => {}
        }
    }

    if text.is_empty() && tool_calls.is_empty() && reasoning.is_empty() {
        return Vec::new();
    }

    let mut wire = Map::new();
    wire.insert("role".to_owned(), json!("assistant"));

    // Rule 3: echo reasoning only when this model requires it.
    let echo = quirks.reasoning_protocol && !reasoning.is_empty();

    // Rule 4: text never shares a message with tool calls unless reasoning is
    // echoed in band. See the module header.
    if !echo && !text.is_empty() && !tool_calls.is_empty() {
        let mut calls = Map::new();
        calls.insert("role".to_owned(), json!("assistant"));
        calls.insert("content".to_owned(), Value::Null);
        calls.insert("tool_calls".to_owned(), Value::Array(tool_calls));
        wire.insert("content".to_owned(), json!(text));
        return vec![Value::Object(wire), Value::Object(calls)];
    }

    if !text.is_empty() {
        wire.insert("content".to_owned(), json!(text));
    } else if !echo {
        // An assistant turn that is only tool calls needs an explicit null
        // content; when the reasoning protocol is on, `reasoning_content` is the
        // content and a null would be rejected.
        wire.insert("content".to_owned(), Value::Null);
    }
    if echo {
        wire.insert("reasoning_content".to_owned(), json!(reasoning));
    }
    // Only when non-empty: several vendors reject an explicit empty array
    // (`openai_compat.rs:1296-1299`).
    if !tool_calls.is_empty() {
        wire.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }
    vec![Value::Object(wire)]
}

fn translate_tool_results(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            RequestContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some(json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                "content": content
            })),
            RequestContentBlock::Text { .. }
            | RequestContentBlock::ResourceLink { .. }
            | RequestContentBlock::SignedThinking { .. }
            | RequestContentBlock::ProviderEncryptedReasoning { .. }
            | RequestContentBlock::ToolUse { .. }
            | RequestContentBlock::Image { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_llm::effort::{
        DeclaredVariants, EffortCapabilities, ProviderFamily, ReasoningEffort, resolve_effort,
    };
    use zuno_llm::registry::{ApiSurface, Capabilities, CompletionRequest};

    fn quirks(reasoning_protocol: bool, sampling: bool) -> Quirks {
        Quirks {
            surface: ApiSurface::Chat,
            capabilities: Capabilities {
                reasoning: true,
                tool_calls: true,
                prompt_cache: false,
                attachments: false,
                sampling_params: sampling,
            },
            reasoning_protocol,
            routes_upstreams: false,
            responses_text_blocks: crate::quirks::ResponsesTextBlocks::Multiple,
        }
    }

    fn responses_quirks() -> Quirks {
        Quirks {
            surface: ApiSurface::Responses,
            ..quirks(false, true)
        }
    }

    fn assistant_with_reasoning() -> Message {
        Message::from_content(
            Role::Assistant,
            vec![
                RequestContentBlock::SignedThinking {
                    thinking: "the user wants Paris".to_owned(),
                    signature: String::new(),
                },
                RequestContentBlock::Text {
                    text: "It is sunny.".to_owned(),
                },
            ],
        )
    }

    #[test]
    fn a_streaming_chat_request_asks_for_usage_because_the_wire_omits_it_otherwise() {
        // Measured against a live OpenAI-compatible gateway: without `stream_options`
        // the stream is content chunks, a `finish_reason` chunk, then `[DONE]` — no usage
        // frame exists at all, so every token counter downstream reads zero no matter how
        // far past `MessageEnd` the engine reads. Two earlier attempts each fixed one half
        // and concluded the other half was the provider's fault.
        let body = RequestBody::new(
            "a-model",
            vec![Message::from_content(
                Role::User,
                vec![RequestContentBlock::Text {
                    text: "hi".to_owned(),
                }],
            )],
        );
        let built = body.build(&quirks(false, true));
        assert_eq!(
            built["stream_options"]["include_usage"],
            json!(true),
            "a streaming request that does not ask for usage will never be told any"
        );
    }

    #[test]
    fn a_provider_that_rejects_stream_options_can_override_it_from_configuration() {
        // `stream_options` is deliberately not in `PROTECTED_KEYS`, so a gateway that
        // 400s on the field is a configuration change rather than a code change. If this
        // fails because the key became protected, the escape hatch is gone.
        assert!(
            !PROTECTED_KEYS.contains(&"stream_options"),
            "protecting `stream_options` removes the only way to disable it per provider"
        );
        let mut body = RequestBody::new(
            "a-model",
            vec![Message::from_content(
                Role::User,
                vec![RequestContentBlock::Text {
                    text: "hi".to_owned(),
                }],
            )],
        );
        body.extra_body
            .insert("stream_options".to_owned(), Value::Null);
        assert_eq!(
            body.build(&quirks(false, true))["stream_options"],
            Value::Null,
            "configuration could not override the shipped default"
        );
    }

    #[test]
    fn reasoning_is_echoed_only_when_the_model_requires_it() {
        let body = RequestBody::new("a-protocol-model", vec![assistant_with_reasoning()]);

        let echoed = body.build(&quirks(true, true));
        let message = &echoed["messages"][0];
        assert_eq!(
            message["reasoning_content"],
            json!("the user wants Paris"),
            "the protocol is on, so reasoning must be replayed"
        );

        let dropped = body.build(&quirks(false, true));
        assert!(
            dropped["messages"][0].get("reasoning_content").is_none(),
            "the protocol is off, so reasoning must not be paid for"
        );
        assert_eq!(dropped["messages"][0]["content"], json!("It is sunny."));
    }

    #[test]
    fn thinking_is_written_after_extra_body_and_wins() {
        let mut body = RequestBody::new("a-protocol-model", vec![assistant_with_reasoning()]);
        body.extra_body
            .insert("thinking".to_owned(), json!({"type": "disabled"}));

        let built = body.build(&quirks(true, true));
        assert_eq!(
            built["thinking"],
            json!({"type": "enabled"}),
            "extra_body must not be able to disable a required opt-in"
        );

        let without = body.build(&quirks(false, true));
        assert_eq!(
            without["thinking"],
            json!({"type": "disabled"}),
            "with the protocol off, the caller's value is respected"
        );
    }

    #[test]
    fn sampling_params_are_stripped_when_the_model_rejects_them() {
        let mut body = RequestBody::new("o3", vec![Message::new(Role::User, "hi")]);
        body.sampling = Sampling {
            temperature: Some(0.0),
            top_p: Some(0.9),
            frequency_penalty: Some(0.1),
            presence_penalty: Some(0.2),
        };

        let permitted = body.build(&quirks(false, true));
        assert_eq!(permitted["temperature"], json!(0.0));
        assert_eq!(permitted["top_p"], json!(0.9));

        let stripped = body.build(&quirks(false, false));
        for key in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
        ] {
            assert!(stripped.get(key).is_none(), "{key} must be stripped");
        }
    }

    #[test]
    fn extra_body_cannot_replace_a_derived_field() {
        let mut body = RequestBody::new("groq/llama", vec![Message::new(Role::User, "hi")]);
        for key in PROTECTED_KEYS {
            body.extra_body.insert((*key).to_owned(), json!("hijacked"));
        }
        let built = body.build(&quirks(false, true));
        assert_eq!(built["model"], json!("groq/llama"));
        assert_eq!(built["stream"], json!(true));
        assert!(built["messages"].is_array());
        assert!(built.get("tools").is_none());
    }

    /// `extraBody` is a verbatim escape hatch for whatever an endpoint accepts.
    ///
    /// The key changed from `reasoning_effort` to `service_tier`. Nothing about the
    /// old assertion was false — a hand-inserted `reasoning_effort` did survive —
    /// but naming an effort field here made the suite *look* as though it covered
    /// reasoning effort, while the session path was in fact emitting
    /// `reasoningEffort`. This test never had anything to do with effort
    /// resolution; the coverage it appeared to give is now real and lives in
    /// [`the_sessions_effort_reaches_the_chat_body_as_reasoning_effort`].
    #[test]
    fn extra_body_reaches_unprotected_keys() {
        let mut body = RequestBody::new("gpt-oss-20b", vec![Message::new(Role::User, "hi")]);
        body.extra_body
            .insert("service_tier".to_owned(), json!("flex"));
        let built = body.build(&quirks(false, true));
        assert_eq!(built["service_tier"], json!("flex"));
    }

    /// The chosen level must reach a Chat body as `reasoning_effort`.
    ///
    /// Every input is production: [`resolve_effort`] builds the options exactly as
    /// `session_reasoning_options` does, and [`RequestBody::build`] is the same
    /// builder the provider ships. Nothing here is hand-written except the level.
    #[test]
    fn the_sessions_effort_reaches_the_chat_body_as_reasoning_effort() {
        let resolved = resolve_effort(
            ProviderFamily::OpenAi,
            ReasoningEffort::High,
            EffortCapabilities::default(),
            &DeclaredVariants::new(),
        );
        let request = CompletionRequest::new("gpt-oss-20b", vec![Message::new(Role::User, "hi")])
            .on_surface(ApiSurface::Chat);
        let mut request = request;
        request.parameters = resolved.options.clone();

        let mut built = RequestBody::new("gpt-oss-20b", vec![Message::new(Role::User, "hi")])
            .build(&quirks(false, true));
        request.apply_parameters(&mut built, ApiSurface::Chat);

        assert_eq!(built["reasoning_effort"], json!("high"));
        assert!(
            built.get("reasoningEffort").is_none(),
            "the SDK option name must not reach a chat body: {built}"
        );
    }

    /// The same level must reach a Responses body as `reasoning.effort`.
    #[test]
    fn the_sessions_effort_reaches_the_responses_body_as_nested_reasoning() {
        let resolved = resolve_effort(
            ProviderFamily::OpenAi,
            ReasoningEffort::High,
            EffortCapabilities::default(),
            &DeclaredVariants::new(),
        );
        let mut request = CompletionRequest::new("gpt-5", vec![Message::new(Role::User, "hi")])
            .on_surface(ApiSurface::Responses);
        request.parameters = resolved.options;

        let mut built = RequestBody::new("gpt-5", vec![Message::new(Role::User, "hi")])
            .build(&responses_quirks());
        request.apply_parameters(&mut built, ApiSurface::Responses);

        assert_eq!(built["reasoning"], json!({"effort": "high"}));
        assert!(
            built.get("reasoningEffort").is_none() && built.get("reasoning_effort").is_none(),
            "the Responses surface takes the level nested, not flat: {built}"
        );
    }

    #[test]
    fn responses_surface_lifts_native_instructions_and_preserves_developer_context() {
        let mut request = RequestBody::new(
            "gpt-5",
            vec![
                Message::new(Role::System, "native kernel"),
                Message::new(Role::System, "project policy"),
                Message::new(Role::User, "exact user text"),
            ],
        );
        request.developer_context = vec!["active goal".to_owned(), "memory".to_owned()];
        let built = request.build(&responses_quirks());

        assert_eq!(built["instructions"], json!("native kernel"));
        assert_eq!(built["input"][0]["role"], json!("developer"));
        assert_eq!(built["input"][0]["content"], json!("project policy"));
        assert_eq!(built["input"][1]["role"], json!("user"));
        assert_eq!(
            built["input"][1]["content"][0]["text"],
            json!("exact user text")
        );
        assert_eq!(
            built["input"][2],
            json!({"role": "developer", "content": "active goal"})
        );
        assert_eq!(
            built["input"][3],
            json!({"role": "developer", "content": "memory"})
        );
    }

    #[test]
    fn responses_surface_uses_input_and_max_output_tokens_not_chat_fields() {
        let mut body = RequestBody::new("gpt-5", vec![Message::new(Role::User, "hello")]);
        body.max_tokens = Some(321);

        let built = body.build(&responses_quirks());

        assert!(
            built["input"].is_array(),
            "Responses requires an input array"
        );
        assert_eq!(built["input"][0]["role"], json!("user"));
        assert_eq!(built["input"][0]["content"][0]["type"], json!("input_text"));
        assert_eq!(built["input"][0]["content"][0]["text"], json!("hello"));
        assert_eq!(built["max_output_tokens"], json!(321));
        assert!(built.get("messages").is_none());
        assert!(built.get("max_tokens").is_none());
    }

    /// The Responses `input` list must preserve the assistant turn's source order.
    ///
    /// The content is exactly what `project_history_owned` hands over for a turn
    /// that reasoned, spoke, then called a tool — the same three blocks in the same
    /// order that the engine's own
    /// `reasoning_replay::production_round_trip` test proves come out of the
    /// database. So this asserts the second half of the round trip on the shape
    /// the first half produces, not on an arrangement invented here.
    ///
    /// Before the fix `input` came out as `function_call, assistant` with no
    /// reasoning item at all: the capsule was never persisted, and the text was
    /// appended after the loop instead of at its own position.
    #[test]
    fn responses_input_keeps_the_reasoning_then_text_then_call_order() {
        let message = Message::from_content(
            Role::Assistant,
            vec![
                RequestContentBlock::ProviderEncryptedReasoning {
                    id: "rs_capsule".to_owned(),
                    summary: vec!["Deciding to read the fixture.".to_owned()],
                    encrypted_content: Some("ENCRYPTED-CAPSULE".to_owned()),
                    status: Some("completed".to_owned()),
                },
                RequestContentBlock::Text {
                    text: "Let me check that file.".to_owned(),
                },
                RequestContentBlock::ToolUse {
                    id: "call_read".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"filePath": "/tmp/fixture.txt"}),
                    thought_signature: None,
                },
            ],
        );
        let built = RequestBody::new("gpt-5", vec![message]).build(&responses_quirks());
        let items = built["input"].as_array().expect("an input array");
        let kinds: Vec<&str> = items
            .iter()
            .map(|item| {
                item.get("type")
                    .or_else(|| item.get("role"))
                    .and_then(Value::as_str)
                    .expect("every item is typed or roled")
            })
            .collect();
        assert_eq!(
            kinds,
            ["reasoning", "assistant", "function_call"],
            "the Responses input list is ordered history: {built}"
        );
        assert_eq!(items[0]["encrypted_content"], json!("ENCRYPTED-CAPSULE"));
        assert_eq!(
            items[0]["summary"][0],
            json!({"type": "summary_text", "text": "Deciding to read the fixture."})
        );
        assert_eq!(
            items[1]["content"][0]["text"],
            json!("Let me check that file.")
        );
        assert_eq!(items[2]["call_id"], json!("call_read"));
    }

    /// Text that follows a tool call must stay after it.
    ///
    /// The mirror of the previous test, and the one that fails if the flush is
    /// merely moved to the top of the function rather than performed per item.
    #[test]
    fn responses_input_keeps_text_after_a_preceding_call() {
        let message = Message::from_content(
            Role::Assistant,
            vec![
                RequestContentBlock::ToolUse {
                    id: "call_first".to_owned(),
                    name: "read".to_owned(),
                    input: json!({}),
                    thought_signature: None,
                },
                RequestContentBlock::Text {
                    text: "and now I will explain".to_owned(),
                },
            ],
        );
        let built = RequestBody::new("gpt-5", vec![message]).build(&responses_quirks());
        let items = built["input"].as_array().expect("an input array");
        let kinds: Vec<&str> = items
            .iter()
            .map(|item| {
                item.get("type")
                    .or_else(|| item.get("role"))
                    .and_then(Value::as_str)
                    .expect("every item is typed or roled")
            })
            .collect();
        assert_eq!(kinds, ["function_call", "assistant"], "{built}");
    }

    /// A capsule with no ciphertext must not split the text around it.
    ///
    /// It contributes no item, so flushing on it would break one assistant item
    /// into two and change the turn the model sees.
    #[test]
    fn a_dropped_capsule_does_not_split_the_surrounding_text() {
        let message = Message::from_content(
            Role::Assistant,
            vec![
                RequestContentBlock::Text {
                    text: "first half ".to_owned(),
                },
                RequestContentBlock::ProviderEncryptedReasoning {
                    id: "rs_unsealed".to_owned(),
                    summary: Vec::new(),
                    encrypted_content: None,
                    status: None,
                },
                RequestContentBlock::Text {
                    text: "second half".to_owned(),
                },
            ],
        );
        let built = RequestBody::new("gpt-5", vec![message]).build(&responses_quirks());
        let items = built["input"].as_array().expect("an input array");
        assert_eq!(items.len(), 1, "expected one assistant item: {built}");
        assert_eq!(items[0]["role"], json!("assistant"));
        assert_eq!(items[0]["content"].as_array().expect("content").len(), 2);
    }

    #[test]
    fn a_tool_only_assistant_turn_gets_null_content_and_no_empty_array() {
        let message = Message::from_content(
            Role::Assistant,
            vec![RequestContentBlock::ToolUse {
                id: "call_1".to_owned(),
                name: "get_weather".to_owned(),
                input: json!({"city": "Paris"}),
                thought_signature: None,
            }],
        );
        let built = RequestBody::new("groq/llama", vec![message]).build(&quirks(false, true));
        let wire = &built["messages"][0];
        assert_eq!(wire["content"], Value::Null);
        assert_eq!(
            wire["tool_calls"][0]["function"]["name"],
            json!("get_weather")
        );
        assert_eq!(
            wire["tool_calls"][0]["function"]["arguments"],
            json!("{\"city\":\"Paris\"}")
        );
    }

    /// An assistant turn shaped like the one the live 400 was produced by: the
    /// model's visible text, then a tool call whose id is a gateway reasoning
    /// capsule rather than a plain id.
    fn text_then_tool_call(id: &str) -> Message {
        Message::from_content(
            Role::Assistant,
            vec![
                RequestContentBlock::Text {
                    text: "The go shim is broken. Let me check for a toolchain.".to_owned(),
                },
                RequestContentBlock::ToolUse {
                    id: id.to_owned(),
                    name: "shell".to_owned(),
                    input: json!({"command": "command -v go"}),
                    thought_signature: None,
                },
            ],
        )
    }

    #[test]
    fn text_and_tool_calls_are_split_so_replayed_reasoning_can_stay_first() {
        // Rule 4. A gateway fronting an Anthropic-family model seals the turn's
        // reasoning into the tool call's id and re-expands it just before that call,
        // so merging text into the same message makes the reasoning un-first and the
        // gateway answers `400 assistant reasoning prefix does not match the
        // reasoning capsule`. Splitting keeps the text *and* keeps reasoning first.
        let capsule_id = "brtc_v1.eyJ2ZXJzaW9uIjoxLCJ0b29sX3VzZV9pZCI6InRvb2x1c2VfMSJ9";
        let built = RequestBody::new("gateway/claude", vec![text_then_tool_call(capsule_id)])
            .build(&quirks(false, true));
        let messages = built["messages"]
            .as_array()
            .expect("the chat surface carries messages");

        assert_eq!(
            messages.len(),
            2,
            "text and tool calls must not share one assistant message: {messages:#?}"
        );
        assert_eq!(messages[0]["role"], json!("assistant"));
        assert_eq!(
            messages[0]["content"],
            json!("The go shim is broken. Let me check for a toolchain."),
            "the model's own words must survive the split"
        );
        assert!(
            messages[0].get("tool_calls").is_none(),
            "the text message must not carry the calls too"
        );

        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(
            messages[1]["content"],
            Value::Null,
            "the call-bearing message must leave room for the re-expanded reasoning"
        );
        assert_eq!(
            messages[1]["tool_calls"][0]["id"],
            json!(capsule_id),
            "the capsule travels in the id and must be echoed byte-for-byte"
        );
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["name"],
            json!("shell")
        );
    }

    #[test]
    fn the_split_does_not_apply_when_reasoning_is_echoed_in_band() {
        // With rule 3 in force the vendor takes reasoning in `reasoning_content`, so
        // its position is explicit, there is no capsule to re-expand, and that wire
        // wants the text on the same message. Splitting here would be churn at best.
        let message = Message::from_content(
            Role::Assistant,
            vec![
                RequestContentBlock::SignedThinking {
                    thinking: "the user wants a toolchain".to_owned(),
                    signature: String::new(),
                },
                RequestContentBlock::Text {
                    text: "Checking now.".to_owned(),
                },
                RequestContentBlock::ToolUse {
                    id: "call_1".to_owned(),
                    name: "shell".to_owned(),
                    input: json!({"command": "command -v go"}),
                    thought_signature: None,
                },
            ],
        );
        let built = RequestBody::new("a-protocol-model", vec![message]).build(&quirks(true, true));
        let messages = built["messages"]
            .as_array()
            .expect("the chat surface carries messages");
        assert_eq!(messages.len(), 1, "rule 3 keeps the turn on one message");
        assert_eq!(messages[0]["content"], json!("Checking now."));
        assert_eq!(
            messages[0]["reasoning_content"],
            json!("the user wants a toolchain")
        );
        assert_eq!(messages[0]["tool_calls"][0]["id"], json!("call_1"));
    }

    #[test]
    fn a_plain_assistant_turn_carries_no_tool_calls_key_at_all() {
        let message = Message::new(Role::Assistant, "hello");
        let built = RequestBody::new("m", vec![message]).build(&quirks(false, true));
        assert!(built["messages"][0].get("tool_calls").is_none());
    }

    #[test]
    fn each_tool_result_becomes_its_own_wire_message() {
        let message = Message::from_content(
            Role::Tool,
            vec![
                RequestContentBlock::ToolResult {
                    tool_use_id: "a".to_owned(),
                    content: "one".to_owned(),
                    is_error: None,
                },
                RequestContentBlock::ToolResult {
                    tool_use_id: "b".to_owned(),
                    content: "two".to_owned(),
                    is_error: Some(true),
                },
            ],
        );
        let built = RequestBody::new("m", vec![message]).build(&quirks(false, true));
        let messages = built["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["tool_call_id"], json!("a"));
        assert_eq!(messages[1]["tool_call_id"], json!("b"));
        // `is_error` is deliberately absent: it is not a chat-completions field,
        // and vendors reject it (`openai_compat.rs:1246-1258`).
        assert!(messages[1].get("is_error").is_none());
    }

    #[test]
    fn an_image_is_dropped_for_a_text_only_model_and_sent_otherwise() {
        let message = Message::from_content(
            Role::User,
            vec![
                RequestContentBlock::Text {
                    text: "what is this".to_owned(),
                },
                RequestContentBlock::Image {
                    filename: None,
                    media_type: "image/png".to_owned(),
                    data: "AAAA".to_owned(),
                },
            ],
        );
        let body = RequestBody::new("m", vec![message]);

        let text_only = body.build(&quirks(false, true));
        assert_eq!(text_only["messages"][0]["content"], json!("what is this"));

        let mut visual = quirks(false, true);
        visual.capabilities.attachments = true;
        let built = body.build(&visual);
        let parts = built["messages"][0]["content"].as_array().expect("parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[1]["image_url"]["url"],
            json!("data:image/png;base64,AAAA")
        );
    }

    #[test]
    fn tools_are_omitted_for_a_model_that_cannot_use_them() {
        let mut body = RequestBody::new("m", vec![Message::new(Role::User, "hi")]);
        body.tools = Some(json!([{"type": "function", "function": {"name": "f"}}]));

        let capable = body.build(&quirks(false, true));
        assert!(capable["tools"].is_array());

        let mut incapable = quirks(false, true);
        incapable.capabilities.tool_calls = false;
        assert!(body.build(&incapable).get("tools").is_none());
    }
}
