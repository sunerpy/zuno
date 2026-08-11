//! Assembling Chat Completions and Responses request bodies.
//!
//! # The three ordering and gating rules that are not cosmetic
//!
//! 1. **`thinking` is written after `extra_body`.** The models that require the
//!    reasoning-content protocol also require `thinking: {"type": "enabled"}`, and
//!    a caller that put a different `thinking` value in its options bag must not be
//!    able to disable it by accident. Writing it last makes the required shape the
//!    final word
//!    (`.omo/refs/claw-code/rust/crates/api/src/providers/openai_compat.rs:1226-1227`).
//! 2. **Sampling parameters are gated on a capability**, never on a model name.
//!    See [`crate::quirks`].
//! 3. **`reasoning_content` is echoed only when the model requires it.** Sending it
//!    unconditionally pays tokens on every later turn for a field most vendors did
//!    not send and some reject.
//!
//! # What `extra_body` may not overwrite
//!
//! [`PROTECTED_KEYS`] is the same set the reference implementation protects
//! (`openai_compat.rs:1234-1243`). These are the fields the profile derives from
//! the request itself; letting an options bag replace `messages` would make the
//! transcript and the wire disagree silently.

use oc_llm::event::{Message, RequestContentBlock, Role};
use oc_llm::registry::ApiSurface;
use serde_json::{Map, Value, json};

use crate::quirks::Quirks;

/// Body keys `extra_body` must not replace.
pub const PROTECTED_KEYS: &[&str] = &[
    "model",
    "messages",
    "input",
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
    /// The turn, in order.
    pub messages: Vec<Message>,
    /// Tool definitions, when the caller has any and the model accepts them.
    ///
    /// A JSON array, passed through rather than re-derived: hand-writing a schema
    /// per tool is the anti-pattern this crate is explicitly not repeating. The
    /// definitions come from `oc-tool`, which already owns them.
    pub tools: Option<Value>,
    /// Sampling parameters, subject to [`Quirks::accepts_sampling_params`].
    pub sampling: Sampling,
    /// Output cap, written as `max_tokens`.
    pub max_tokens: Option<u64>,
    /// The caller's own body keys, applied after everything derived.
    ///
    /// This is where reasoning-effort resolution lands: `oc-llm`'s
    /// [`EffortResolution::apply_to`](oc_llm::effort::EffortResolution::apply_to)
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

    fn build_chat(&self, quirks: &Quirks) -> Value {
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(self.model));
        body.insert(
            "messages".to_owned(),
            Value::Array(
                self.messages
                    .iter()
                    .flat_map(|message| translate_message(message, quirks))
                    .collect(),
            ),
        );
        body.insert("stream".to_owned(), json!(true));

        if let Some(tools) = &self.tools
            && quirks.accepts_tools()
        {
            body.insert("tools".to_owned(), tools.clone());
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
        let mut body = Map::new();
        body.insert("model".to_owned(), json!(self.model));
        body.insert(
            "input".to_owned(),
            Value::Array(
                self.messages
                    .iter()
                    .flat_map(|message| translate_response_message(message, quirks))
                    .collect(),
            ),
        );
        body.insert("stream".to_owned(), json!(true));

        if let Some(tools) = &self.tools
            && quirks.accepts_tools()
        {
            body.insert("tools".to_owned(), response_tools(tools));
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
            "role": "system",
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

fn translate_response_assistant(message: &Message) -> Vec<Value> {
    let mut items = Vec::new();
    let mut content = Vec::new();
    for block in &message.content {
        match block {
            RequestContentBlock::Text { text } => {
                content.push(json!({"type": "output_text", "text": text}));
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
                items.push(Value::Object(item));
            }
            RequestContentBlock::ToolUse {
                id, name, input, ..
            } => items.push(json!({
                "type": "function_call",
                "call_id": id,
                "name": name,
                "arguments": input.to_string(),
            })),
            RequestContentBlock::SignedThinking { .. }
            | RequestContentBlock::ToolResult { .. }
            | RequestContentBlock::Image { .. } => {}
        }
    }
    if !content.is_empty() {
        items.push(json!({"role": "assistant", "content": content}));
    }
    items
}

fn translate_response_tool_results(message: &Message, quirks: &Quirks) -> Vec<Value> {
    let images = if quirks.accepts_attachments() {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                RequestContentBlock::Image { media_type, data } => Some(json!({
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
    message
        .content
        .iter()
        .filter_map(|block| match block {
            RequestContentBlock::Text { text } => Some(json!({"type": "input_text", "text": text})),
            RequestContentBlock::Image { media_type, data } if quirks.accepts_attachments() => {
                Some(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                }))
            }
            _ => None,
        })
        .collect()
}

fn joined_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            RequestContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
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
            RequestContentBlock::Image { media_type, data } if quirks.accepts_attachments() => {
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
    use oc_llm::registry::{ApiSurface, Capabilities};

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

    #[test]
    fn extra_body_reaches_unprotected_keys() {
        let mut body = RequestBody::new("gpt-oss-20b", vec![Message::new(Role::User, "hi")]);
        body.extra_body
            .insert("reasoning_effort".to_owned(), json!("high"));
        let built = body.build(&quirks(false, true));
        assert_eq!(built["reasoning_effort"], json!("high"));
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
