//! OpenAI Chat Completions and Responses request construction.

use std::fmt;

use serde_json::{Map, Value, json};
use zuno_error::ProviderError;
use zuno_llm::event::{Message, RequestContentBlock, Role, tool_arguments_text};
use zuno_llm::registry::{ApiSurface, CompletionRequest, sealed_item_has_following_output};

use crate::provider::OpenAiConfig;

const ZUNO_SESSION_METADATA_KEY: &str = "zuno_session_id";

/// Sampling parameters understood by genuine OpenAI models.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sampling {
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff.
    pub top_p: Option<f64>,
    /// Frequency penalty.
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    pub presence_penalty: Option<f64>,
}

/// Resolve the wire surface. OpenCode's genuine OpenAI loader always constructs
/// `sdk.responses(modelID)`, so `Default` means Responses here, not Chat.
#[must_use]
pub const fn resolve_surface(surface: ApiSurface) -> ApiSurface {
    match surface {
        ApiSurface::Default | ApiSurface::Responses => ApiSurface::Responses,
        ApiSurface::Chat => ApiSurface::Chat,
        ApiSurface::Messages => ApiSurface::Messages,
    }
}

/// Whether an OpenAI model rejects sampling parameters such as `temperature`.
///
/// This mirrors the OpenAI Responses SDK's model classification: the o-series,
/// non-chat GPT-5 family, Codex family, and computer-use models are reasoning
/// models. Prefixes are matched after an optional provider namespace.
#[must_use]
pub fn is_reasoning_model(model_id: &str) -> bool {
    let id = model_id
        .rsplit_once('/')
        .map_or(model_id, |(_, unqualified)| unqualified)
        .to_ascii_lowercase();
    if id.starts_with("gpt-5-chat") || id.contains("-chat-") {
        return false;
    }
    id.starts_with('o')
        || id.starts_with("gpt-5")
        || id.starts_with("codex-")
        || id.starts_with("computer-use")
}

/// Build the JSON body for the selected OpenAI surface.
///
/// # Errors
///
/// Returns a fatal provider error when the request selects the unsupported
/// Messages surface, contains a non-finite sampling number, or tries to replay a
/// provider-private block on the wrong OpenAI surface.
pub fn build_request_body(
    request: &CompletionRequest,
    config: &OpenAiConfig,
) -> Result<Value, ProviderError> {
    request
        .validate_tool_arguments()
        .map_err(ProviderError::fatal)?;
    reject_reserved_session_metadata(request)?;
    let surface = resolve_surface(request.surface);
    // An endpoint that seals its reasoning is a Responses endpoint: `include` and
    // the sealed item exist nowhere on Chat Completions. Refusing here means the
    // misconfiguration is a first-request error rather than a whole session that
    // silently runs without the capability that was asked for.
    if surface == ApiSurface::Chat && config.reasoning_replay().requests_encrypted() {
        return Err(ProviderError::fatal(
            RequestShapeError::EncryptedReasoningReplayOnChat,
        ));
    }
    let mut body = match surface {
        ApiSurface::Chat => build_chat_body(request, config),
        ApiSurface::Responses => build_responses_body(request, config),
        ApiSurface::Messages | ApiSurface::Default => {
            Err(ProviderError::fatal(RequestShapeError::UnsupportedSurface))
        }
    }?;
    request.apply_parameters(&mut body, surface);
    if surface == ApiSurface::Responses {
        // After `apply_parameters`, not before: a per-request parameter bag is a
        // `Record<string, any>`, so a model variant may carry its own `include`, and
        // a non-object value replaces whatever the body held. Re-merging here keeps
        // the author's entries and restores the entry the declared capability cannot
        // lose. `insert_include` is idempotent, so the earlier pass is not undone.
        if let Value::Object(map) = &mut body {
            config.reasoning_replay().insert_include(map);
        }
        project_session_affinity(request, &mut body)?;
    }
    Ok(body)
}

fn reject_reserved_session_metadata(request: &CompletionRequest) -> Result<(), ProviderError> {
    let overrides_reserved_key = request
        .parameters
        .get("metadata")
        .and_then(Value::as_object)
        .is_some_and(|metadata| metadata.contains_key(ZUNO_SESSION_METADATA_KEY));
    if overrides_reserved_key {
        Err(ProviderError::fatal(
            RequestShapeError::ReservedSessionMetadata,
        ))
    } else {
        Ok(())
    }
}

fn project_session_affinity(
    request: &CompletionRequest,
    body: &mut Value,
) -> Result<(), ProviderError> {
    let Some(identity) = request
        .request_context()
        .and_then(|context| context.session_identity())
    else {
        return Ok(());
    };
    let root = body
        .as_object_mut()
        .expect("OpenAI request builders always return an object");
    let metadata = root
        .entry("metadata")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(metadata) = metadata.as_object_mut() else {
        return Err(ProviderError::fatal(
            RequestShapeError::MetadataMustBeObject,
        ));
    };
    metadata.insert(
        ZUNO_SESSION_METADATA_KEY.to_owned(),
        Value::String(identity.as_str().to_owned()),
    );
    Ok(())
}

fn build_chat_body(
    request: &CompletionRequest,
    config: &OpenAiConfig,
) -> Result<Value, ProviderError> {
    let mut root = Map::new();
    root.insert("model".to_owned(), json!(request.model_id));
    let mut messages = request
        .messages
        .iter()
        .flat_map(chat_message)
        .collect::<Result<Vec<_>, _>>()?;
    messages.extend(
        request
            .developer_context
            .iter()
            .filter(|content| !content.trim().is_empty())
            .map(|content| json!({"role": "developer", "content": content})),
    );
    root.insert("messages".to_owned(), Value::Array(messages));
    insert_tools(&mut root, config);
    root.insert("stream".to_owned(), Value::Bool(true));
    root.insert(
        "stream_options".to_owned(),
        json!({ "include_usage": true }),
    );
    if let Some(max_tokens) = config.max_tokens() {
        root.insert("max_tokens".to_owned(), json!(max_tokens));
    }
    insert_sampling(&mut root, request, config)?;
    Ok(Value::Object(root))
}

fn build_responses_body(
    request: &CompletionRequest,
    config: &OpenAiConfig,
) -> Result<Value, ProviderError> {
    let mut input = Vec::new();
    let mut instructions = None;
    for message in &request.messages {
        if message.role == Role::System && instructions.is_none() {
            instructions = Some(joined_text(message));
        } else {
            input.extend(responses_message(message)?);
        }
    }
    input.extend(
        request
            .developer_context
            .iter()
            .filter(|content| !content.trim().is_empty())
            .map(|content| json!({"role": "developer", "content": content})),
    );

    let mut root = Map::new();
    root.insert("model".to_owned(), json!(request.model_id));
    if let Some(instructions) = instructions.filter(|instructions| !instructions.is_empty()) {
        root.insert("instructions".to_owned(), Value::String(instructions));
    }
    root.insert("input".to_owned(), Value::Array(input));
    insert_tools(&mut root, config);
    if let Some(store) = config.store() {
        root.insert("store".to_owned(), Value::Bool(store));
    }
    if let Some(include) = config.include() {
        root.insert("include".to_owned(), Value::Array(include.to_vec()));
    }
    if let Some(reasoning) = config.reasoning_options() {
        root.insert("reasoning".to_owned(), reasoning.clone());
    }
    if let Some(text) = config.text() {
        root.insert("text".to_owned(), text.clone());
    }
    // After the raw `include` passthrough above, so a declared sealing endpoint
    // keeps the one entry that makes the envelope arrive at all. Merged, so a
    // hand-written `include` keeps its other entries.
    config.reasoning_replay().insert_include(&mut root);
    if let Some(max_tokens) = config.max_tokens() {
        root.insert("max_output_tokens".to_owned(), json!(max_tokens));
    }
    root.insert("stream".to_owned(), Value::Bool(true));
    insert_sampling(&mut root, request, config)?;
    Ok(Value::Object(root))
}

fn insert_tools(root: &mut Map<String, Value>, config: &OpenAiConfig) {
    if !config.tools().is_empty() {
        root.insert("tools".to_owned(), Value::Array(config.tools().to_vec()));
    }
    if let Some(tool_choice) = config.tool_choice() {
        root.insert("tool_choice".to_owned(), tool_choice.clone());
    }
}

fn insert_sampling(
    root: &mut Map<String, Value>,
    request: &CompletionRequest,
    config: &OpenAiConfig,
) -> Result<(), ProviderError> {
    if is_reasoning_model(&request.model_id) {
        return Ok(());
    }
    let sampling = config.sampling();
    insert_number(root, "temperature", sampling.temperature)?;
    insert_number(root, "top_p", sampling.top_p)?;
    insert_number(root, "frequency_penalty", sampling.frequency_penalty)?;
    insert_number(root, "presence_penalty", sampling.presence_penalty)?;
    Ok(())
}

fn insert_number(
    root: &mut Map<String, Value>,
    key: &'static str,
    value: Option<f64>,
) -> Result<(), ProviderError> {
    let Some(value) = value else {
        return Ok(());
    };
    let number = if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        serde_json::Number::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).ok_or_else(|| {
            ProviderError::fatal(RequestShapeError::InvalidSampling { key, value })
        })?
    };
    root.insert(key.to_owned(), Value::Number(number));
    Ok(())
}

fn chat_message(message: &Message) -> Vec<Result<Value, ProviderError>> {
    match message.role {
        Role::Tool => message
            .content
            .iter()
            .filter_map(|block| match block {
                RequestContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some(Ok(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }))),
                _ => None,
            })
            .collect(),
        Role::Assistant => vec![chat_assistant(message)],
        Role::System | Role::User => vec![chat_plain(message)],
    }
}

fn chat_plain(message: &Message) -> Result<Value, ProviderError> {
    let role = if message.role == Role::System {
        "system"
    } else {
        "user"
    };
    let mut text = String::new();
    let mut parts = Vec::new();
    let mut has_image = false;
    for block in &message.content {
        match block {
            RequestContentBlock::Text { text: fragment } => {
                text.push_str(fragment);
                parts.push(json!({ "type": "text", "text": fragment }));
            }
            RequestContentBlock::ResourceLink { .. } => {
                let Some(fragment) = block.provider_text() else {
                    unreachable!("resource links always have a provider text projection")
                };
                text.push_str(fragment.as_ref());
                parts.push(json!({ "type": "text", "text": fragment.as_ref() }));
            }
            RequestContentBlock::Image {
                media_type, data, ..
            } => {
                has_image = true;
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{media_type};base64,{data}") },
                }));
            }
            RequestContentBlock::SignedThinking { .. }
            | RequestContentBlock::ProviderEncryptedReasoning { .. }
            | RequestContentBlock::ToolUse { .. }
            | RequestContentBlock::ToolResult { .. } => {}
            RequestContentBlock::ImageAttachment { .. } => {
                unreachable!(
                    "attachment references must be resolved before provider request shaping"
                )
            }
        }
    }
    let content = if has_image {
        Value::Array(parts)
    } else {
        Value::String(text)
    };
    Ok(json!({ "role": role, "content": content }))
}

fn chat_assistant(message: &Message) -> Result<Value, ProviderError> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in &message.content {
        match block {
            RequestContentBlock::Text { text: fragment } => text.push_str(fragment),
            RequestContentBlock::ResourceLink { .. } => {
                let Some(fragment) = block.provider_text() else {
                    unreachable!("resource links always have a provider text projection")
                };
                text.push_str(fragment.as_ref());
            }
            RequestContentBlock::ToolUse {
                id,
                name,
                input,
                raw_arguments,
                ..
            } => calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": tool_arguments_text(input, raw_arguments.as_deref()) },
            })),
            RequestContentBlock::ProviderEncryptedReasoning { .. } => {
                return Err(ProviderError::fatal(
                    RequestShapeError::EncryptedReasoningOnChat,
                ));
            }
            RequestContentBlock::SignedThinking { .. }
            | RequestContentBlock::ToolResult { .. }
            | RequestContentBlock::Image { .. } => {}
            RequestContentBlock::ImageAttachment { .. } => {
                unreachable!(
                    "attachment references must be resolved before provider request shaping"
                )
            }
        }
    }
    let mut value = Map::new();
    value.insert("role".to_owned(), json!("assistant"));
    value.insert(
        "content".to_owned(),
        if text.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !calls.is_empty() {
        value.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    Ok(Value::Object(value))
}

fn responses_message(message: &Message) -> Result<Vec<Value>, ProviderError> {
    match message.role {
        Role::System => Ok(vec![json!({
            "role": "developer",
            "content": joined_text(message),
        })]),
        Role::User => Ok(vec![json!({
            "role": "user",
            "content": responses_content(message, "input_text", "input_image"),
        })]),
        Role::Assistant => responses_assistant(message),
        Role::Tool => Ok(responses_tool_results(message)),
    }
}

/// Project one assistant turn onto Responses `input` items, in source order.
///
/// # Why the text is flushed at its own position
///
/// A sealed reasoning envelope is bound to the output it produced, and a Responses
/// endpoint validates the pairing positionally: the reasoning item has to sit
/// immediately before that output. Accumulating every text block and appending it
/// after the loop reordered the turn — `[reasoning, text, call]` went out as
/// `[reasoning, call, text]` — which reads as a reasoning item that produced the
/// wrong output. Flushing before each emitted item, and once at the end, keeps the
/// wire order the order the model streamed.
fn responses_assistant(message: &Message) -> Result<Vec<Value>, ProviderError> {
    let mut items = Vec::new();
    let mut output_content = Vec::new();
    for (index, block) in message.content.iter().enumerate() {
        match block {
            RequestContentBlock::Text { text } => {
                output_content.push(json!({ "type": "output_text", "text": text }));
            }
            RequestContentBlock::ResourceLink { .. } => {
                let Some(text) = block.provider_text() else {
                    unreachable!("resource links always have a provider text projection")
                };
                output_content.push(json!({ "type": "output_text", "text": text.as_ref() }));
            }
            RequestContentBlock::ProviderEncryptedReasoning {
                summary,
                encrypted_content,
                status,
                ..
            } => {
                let encrypted_content = encrypted_content.as_ref().ok_or_else(|| {
                    ProviderError::fatal(RequestShapeError::MissingEncryptedReasoning)
                })?;
                // The endpoint validates the pairing positionally, so an item with no
                // output after it is a permanent 400 rather than a degraded answer.
                if !sealed_item_has_following_output(&message.content[index + 1..]) {
                    continue;
                }
                let mut item = Map::new();
                item.insert("type".to_owned(), json!("reasoning"));
                item.insert(
                    "summary".to_owned(),
                    Value::Array(
                        summary
                            .iter()
                            .map(|text| json!({ "type": "summary_text", "text": text }))
                            .collect(),
                    ),
                );
                item.insert("encrypted_content".to_owned(), json!(encrypted_content));
                if let Some(status) = status {
                    item.insert("status".to_owned(), json!(status));
                }
                flush_assistant_text(&mut items, &mut output_content);
                items.push(Value::Object(item));
            }
            RequestContentBlock::ToolUse {
                id,
                name,
                input,
                raw_arguments,
                ..
            } => {
                flush_assistant_text(&mut items, &mut output_content);
                items.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": tool_arguments_text(input, raw_arguments.as_deref()),
                }));
            }
            RequestContentBlock::SignedThinking { .. } => {
                return Err(ProviderError::fatal(RequestShapeError::ForeignThinking));
            }
            RequestContentBlock::ToolResult { .. } | RequestContentBlock::Image { .. } => {}
            RequestContentBlock::ImageAttachment { .. } => {
                unreachable!(
                    "attachment references must be resolved before provider request shaping"
                )
            }
        }
    }
    flush_assistant_text(&mut items, &mut output_content);
    Ok(items)
}

/// Emit the text collected so far as one assistant item, if there is any.
///
/// Called before each emitted item rather than before each non-text *block*, so a
/// block that contributes nothing to the wire does not split the surrounding text
/// into two assistant items.
fn flush_assistant_text(items: &mut Vec<Value>, output_content: &mut Vec<Value>) {
    if output_content.is_empty() {
        return;
    }
    items.push(json!({
        "role": "assistant",
        "content": std::mem::take(output_content),
    }));
}

fn responses_tool_results(message: &Message) -> Vec<Value> {
    let images = message
        .content
        .iter()
        .filter_map(|block| match block {
            RequestContentBlock::Image {
                media_type, data, ..
            } => Some(json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}"),
            })),
            RequestContentBlock::ImageAttachment { .. } => {
                unreachable!(
                    "attachment references must be resolved before provider request shaping"
                )
            }
            _ => None,
        })
        .collect::<Vec<_>>();
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
                    let mut parts = vec![json!({ "type": "input_text", "text": content })];
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

fn responses_content(message: &Message, text_type: &str, image_type: &str) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            RequestContentBlock::Text { text } => Some(json!({ "type": text_type, "text": text })),
            RequestContentBlock::ResourceLink { .. } => block
                .provider_text()
                .map(|text| json!({ "type": text_type, "text": text.as_ref() })),
            RequestContentBlock::Image {
                media_type, data, ..
            } => Some(json!({
                "type": image_type,
                "image_url": format!("data:{media_type};base64,{data}"),
            })),
            RequestContentBlock::ImageAttachment { .. } => {
                unreachable!(
                    "attachment references must be resolved before provider request shaping"
                )
            }
            _ => None,
        })
        .collect()
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

#[derive(Debug)]
enum RequestShapeError {
    UnsupportedSurface,
    InvalidSampling { key: &'static str, value: f64 },
    EncryptedReasoningOnChat,
    EncryptedReasoningReplayOnChat,
    MissingEncryptedReasoning,
    ForeignThinking,
    ReservedSessionMetadata,
    MetadataMustBeObject,
}

impl fmt::Display for RequestShapeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSurface => {
                formatter.write_str("OpenAI supports Chat or Responses, not Messages")
            }
            Self::InvalidSampling { key, value } => {
                write!(
                    formatter,
                    "OpenAI sampling parameter `{key}` is not finite: {value}"
                )
            }
            Self::EncryptedReasoningOnChat => formatter.write_str(
                "OpenAI encrypted Responses reasoning cannot be sent to Chat Completions",
            ),
            Self::EncryptedReasoningReplayOnChat => formatter.write_str(
                "`reasoningReplay: \"encrypted\"` is an OpenAI Responses feature, but this \
                 request resolved to Chat Completions; set `surface: \"responses\"` for this \
                 provider or model",
            ),
            Self::MissingEncryptedReasoning => {
                formatter.write_str("OpenAI reasoning replay has no encrypted_content")
            }
            Self::ForeignThinking => formatter.write_str(
                "signed thinking from another provider cannot be sent to OpenAI Responses",
            ),
            Self::ReservedSessionMetadata => formatter.write_str(
                "OpenAI request parameter `metadata.zuno_session_id` is reserved for Zuno's typed durable session affinity",
            ),
            Self::MetadataMustBeObject => formatter.write_str(
                "OpenAI request parameter `metadata` must be an object when Zuno session affinity is attached",
            ),
        }
    }
}

impl std::error::Error for RequestShapeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use zuno_llm::event::RequestContentBlock;
    use zuno_llm::registry::{
        ProviderRequestContext, ProviderSessionIdentity, ReasoningReplay, ReasoningReplayPolicy,
    };

    fn main_turn_context() -> ProviderRequestContext {
        ProviderRequestContext::MainTurn(
            ProviderSessionIdentity::parse("ses_openai_affinity").expect("valid session id"),
        )
    }

    #[test]
    fn responses_projects_typed_session_affinity_into_reserved_metadata() {
        let request = CompletionRequest::new(
            "gpt-5.6-sol",
            vec![Message::new(Role::User, "keep routing out of the prompt")],
        )
        .with_request_context(main_turn_context());

        let body = build_request_body(&request, &OpenAiConfig::default()).expect("body");

        assert_eq!(body["metadata"]["zuno_session_id"], "ses_openai_affinity");
        assert!(
            !body["input"].to_string().contains("ses_openai_affinity"),
            "routing identity must not become model-visible input: {body}"
        );
    }

    #[test]
    fn chat_does_not_fabricate_a_session_affinity_field() {
        let request =
            CompletionRequest::new("gpt-4.1", vec![Message::new(Role::User, "plain chat")])
                .on_surface(ApiSurface::Chat)
                .with_request_context(main_turn_context());

        let body = build_request_body(&request, &OpenAiConfig::default()).expect("body");

        assert!(
            body.get("metadata").is_none(),
            "Chat Completions has no Zuno affinity projection: {body}"
        );
    }

    #[test]
    fn responses_rejects_an_attempt_to_override_the_reserved_affinity_key() {
        let mut request = CompletionRequest::new(
            "gpt-5.6-sol",
            vec![Message::new(Role::User, "do not override routing")],
        )
        .with_request_context(main_turn_context());
        request.parameters.insert(
            "metadata".to_owned(),
            json!({
                "tenant": "kept",
                "zuno_session_id": "ses_attacker_controlled"
            }),
        );

        let error = build_request_body(&request, &OpenAiConfig::default())
            .expect_err("the reserved metadata key must be rejected locally");

        let source = error.source().expect("request-shape source is preserved");
        assert!(
            source.to_string().contains("zuno_session_id"),
            "the local error must name the reserved field: {source}"
        );
    }

    #[test]
    fn responses_preserves_unrelated_request_metadata_beside_affinity() {
        let mut request = CompletionRequest::new(
            "gpt-5.6-sol",
            vec![Message::new(Role::User, "keep caller metadata")],
        )
        .with_request_context(main_turn_context());
        request
            .parameters
            .insert("metadata".to_owned(), json!({"tenant": "tenant-a"}));

        let body = build_request_body(&request, &OpenAiConfig::default()).expect("body");

        assert_eq!(body["metadata"]["tenant"], "tenant-a");
        assert_eq!(body["metadata"]["zuno_session_id"], "ses_openai_affinity");
    }

    #[test]
    fn responses_uses_native_instructions_and_developer_context() {
        let request = CompletionRequest::new(
            "gpt-5.6-sol",
            vec![
                Message::new(Role::System, "KERNEL AND AGENT ROLE"),
                Message::new(Role::System, "GLOBAL RULES"),
                Message::new(Role::System, "PROJECT RULES"),
                Message::new(Role::System, "SELECTED SKILL"),
                Message::new(Role::User, "keep this exact"),
            ],
        )
        .with_developer_context(vec!["ACTIVE GOAL".to_owned(), "MEMORY".to_owned()]);

        let body = build_request_body(&request, &OpenAiConfig::default()).expect("body");

        assert_eq!(body["instructions"], "KERNEL AND AGENT ROLE");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"], "GLOBAL RULES");
        assert_eq!(body["input"][1]["role"], "developer");
        assert_eq!(body["input"][1]["content"], "PROJECT RULES");
        assert_eq!(body["input"][2]["role"], "developer");
        assert_eq!(body["input"][2]["content"], "SELECTED SKILL");
        assert_eq!(body["input"][3]["role"], "user");
        assert_eq!(body["input"][3]["content"][0]["text"], "keep this exact");
        assert_eq!(body["input"][4]["role"], "developer");
        assert_eq!(body["input"][4]["content"], "ACTIVE GOAL");
        assert_eq!(body["input"][5]["role"], "developer");
        assert_eq!(body["input"][5]["content"], "MEMORY");
    }
    #[test]
    fn every_known_reasoning_model_strips_all_sampling_parameters() {
        let reasoning_models = [
            "o1",
            "o1-pro",
            "o3",
            "o3-mini",
            "o3-pro",
            "o4-mini",
            "o3-deep-research",
            "o4-mini-deep-research",
            "gpt-5.1",
            "gpt-5.4",
            "gpt-5.5",
            "gpt-5.4-pro",
            "gpt-5.5-pro",
            "gpt-5-codex",
            "gpt-5.1-codex",
            "gpt-5.1-codex-max",
            "gpt-5.2-codex",
            "gpt-5.3-codex",
            "gpt-5.3-codex-max",
            "codex-mini-latest",
            "computer-use-preview",
        ];
        let config = OpenAiConfig::default().with_sampling(Sampling {
            temperature: Some(0.0),
            top_p: Some(0.9),
            frequency_penalty: Some(0.1),
            presence_penalty: Some(0.2),
        });
        for model in reasoning_models {
            let body = build_request_body(
                &CompletionRequest::new(model, vec![Message::new(Role::User, "hi")]),
                &config,
            )
            .expect("body");
            for key in [
                "temperature",
                "top_p",
                "frequency_penalty",
                "presence_penalty",
            ] {
                assert!(body.get(key).is_none(), "{model} retained {key}");
            }
        }
    }

    #[test]
    fn chat_models_retain_sampling_parameters() {
        let config = OpenAiConfig::default().with_sampling(Sampling {
            temperature: Some(0.0),
            top_p: Some(0.9),
            ..Sampling::default()
        });
        for model in ["gpt-4o-mini", "gpt-4.1", "gpt-5-chat-latest"] {
            let body = build_request_body(
                &CompletionRequest::new(model, vec![Message::new(Role::User, "hi")]),
                &config,
            )
            .expect("body");
            assert_eq!(body["temperature"], json!(0), "{model}");
            assert_eq!(body["top_p"], json!(0.9), "{model}");
        }
    }

    fn sealing_config() -> OpenAiConfig {
        OpenAiConfig::default().with_reasoning_replay(ReasoningReplayPolicy {
            mode: ReasoningReplay::Encrypted,
            max_age: None,
        })
    }

    /// The request field without which no reasoning is ever replayable.
    #[test]
    fn a_sealing_endpoint_is_asked_for_the_encrypted_reasoning_envelope() {
        let request =
            CompletionRequest::new("gpt-5.6-sol", vec![Message::new(Role::User, "hello")])
                .on_surface(ApiSurface::Responses);

        let sealed = build_request_body(&request, &sealing_config()).expect("body");
        assert_eq!(sealed["include"], json!(["reasoning.encrypted_content"]));

        let plain = build_request_body(&request, &OpenAiConfig::default()).expect("body");
        assert!(
            plain.get("include").is_none(),
            "an endpoint that did not declare the capability is never asked for an \
             envelope: {plain}"
        );
    }

    #[test]
    fn a_declared_include_keeps_its_entries_and_gains_the_sealed_one() {
        let request =
            CompletionRequest::new("gpt-5.6-sol", vec![Message::new(Role::User, "hello")])
                .on_surface(ApiSurface::Responses);
        let config = sealing_config().with_include(vec![json!("file_search_call.results")]);

        let body = build_request_body(&request, &config).expect("body");

        assert_eq!(
            body["include"],
            json!(["file_search_call.results", "reasoning.encrypted_content"])
        );
    }

    /// A model variant's own options cannot remove the sealed `include` entry.
    ///
    /// `models.<id>.variants.<effort>` is a free-form bag that reaches the request as
    /// `parameters`, and `apply_parameters` replaces a non-object value outright. An
    /// author who adds their own `include` would otherwise turn the declared
    /// capability off for every request without any diagnostic.
    #[test]
    fn a_request_parameter_cannot_remove_the_sealed_include_entry() {
        for parameter in [
            json!(["message.output_text.logprobs"]),
            json!(null),
            json!("reasoning"),
        ] {
            let mut parameters = Map::new();
            parameters.insert("include".to_owned(), parameter.clone());
            let request =
                CompletionRequest::new("gpt-5.6-sol", vec![Message::new(Role::User, "hello")])
                    .on_surface(ApiSurface::Responses)
                    .with_parameters(parameters);

            let body = build_request_body(&request, &sealing_config()).expect("body");

            let include = body["include"]
                .as_array()
                .unwrap_or_else(|| panic!("`include` stays an array for {parameter}: {body}"));
            assert!(
                include
                    .iter()
                    .any(|entry| entry == "reasoning.encrypted_content"),
                "the declared capability survives `include: {parameter}`: {body}"
            );
        }
    }

    /// The author's own entries survive beside the sealed one.
    #[test]
    fn a_request_parameter_include_keeps_its_own_entries() {
        let mut parameters = Map::new();
        parameters.insert(
            "include".to_owned(),
            json!(["message.output_text.logprobs"]),
        );
        let request =
            CompletionRequest::new("gpt-5.6-sol", vec![Message::new(Role::User, "hello")])
                .on_surface(ApiSurface::Responses)
                .with_parameters(parameters);

        let body = build_request_body(&request, &sealing_config()).expect("body");

        assert_eq!(
            body["include"],
            json!([
                "message.output_text.logprobs",
                "reasoning.encrypted_content"
            ])
        );
    }

    #[test]
    fn encrypted_replay_on_the_chat_surface_is_refused_before_the_wire() {
        let request = CompletionRequest::new("gpt-4o", vec![Message::new(Role::User, "hello")])
            .on_surface(ApiSurface::Chat);

        let error = build_request_body(&request, &sealing_config())
            .expect_err("Chat Completions has no `include` and no sealed item shape");
        let rendered = error
            .source()
            .expect("shape source is preserved")
            .to_string();
        assert!(
            rendered.contains("responses"),
            "the refusal must name the surface to set: {rendered}"
        );
    }

    /// A sealed envelope is validated against the output that follows it.
    ///
    /// So an assistant turn that reasoned, spoke, called a tool, spoke again and
    /// called a second tool has to reach the wire in exactly that order. Before this
    /// fix every text block was accumulated and appended once after the loop, which
    /// put both calls before all of the text.
    #[test]
    fn a_mixed_assistant_turn_keeps_its_streamed_order() {
        let request = CompletionRequest::new(
            "gpt-5.6-sol",
            vec![Message::from_content(
                Role::Assistant,
                vec![
                    RequestContentBlock::ProviderEncryptedReasoning {
                        id: "rs_1".to_owned(),
                        summary: Vec::new(),
                        encrypted_content: Some("kr1_sealed".to_owned()),
                        status: None,
                    },
                    RequestContentBlock::Text {
                        text: "first I will read the file".to_owned(),
                    },
                    RequestContentBlock::ToolUse {
                        id: "call_read".to_owned(),
                        name: "read".to_owned(),
                        input: json!({ "path": "a.rs" }),
                        raw_arguments: None,
                        thought_signature: None,
                    },
                    RequestContentBlock::Text {
                        text: "now the second one".to_owned(),
                    },
                    RequestContentBlock::ToolUse {
                        id: "call_grep".to_owned(),
                        name: "grep".to_owned(),
                        input: json!({ "pattern": "fn main" }),
                        raw_arguments: None,
                        thought_signature: None,
                    },
                ],
            )],
        )
        .on_surface(ApiSurface::Responses);

        let body = build_request_body(&request, &sealing_config()).expect("body");
        let input = body["input"].as_array().expect("input array");

        let shape: Vec<String> = input
            .iter()
            .map(|item| {
                item["type"]
                    .as_str()
                    .map_or_else(|| format!("role:{}", item["role"]), ToOwned::to_owned)
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                "reasoning",
                "role:\"assistant\"",
                "function_call",
                "role:\"assistant\"",
                "function_call"
            ],
            "the sealed item must stay immediately before the output it produced: {body}"
        );
        assert_eq!(input[0]["encrypted_content"], json!("kr1_sealed"));
        assert_eq!(
            input[1]["content"][0]["text"],
            json!("first I will read the file")
        );
        assert_eq!(input[2]["call_id"], json!("call_read"));
        assert_eq!(input[3]["content"][0]["text"], json!("now the second one"));
        assert_eq!(input[4]["call_id"], json!("call_grep"));
    }

    /// The sealed bytes are replayed; the endpoint's item identifier is not.
    ///
    /// The recorded OpenAI continuation
    /// (`openai-responses-gpt-5-5-reasoning-continuation`) replays the item as
    /// `type`, `summary` and `encrypted_content` only, and that request was accepted.
    /// An `id` names an item on the endpoint's side, which a `store: false` endpoint
    /// does not have.
    #[test]
    fn encrypted_reasoning_replays_byte_for_byte_without_item_id() {
        let ciphertext = "gAAAAA_exact_provider_bytes";
        let request = CompletionRequest::new(
            "gpt-5.5",
            vec![Message::from_content(
                Role::Assistant,
                vec![
                    RequestContentBlock::ProviderEncryptedReasoning {
                        id: "rs_sealed_item".to_owned(),
                        summary: vec!["brief summary".to_owned()],
                        encrypted_content: Some(ciphertext.to_owned()),
                        status: None,
                    },
                    RequestContentBlock::Text {
                        text: "the answer".to_owned(),
                    },
                ],
            )],
        );
        let config = OpenAiConfig::default()
            .with_store(false)
            .with_reasoning_replay(ReasoningReplayPolicy {
                mode: ReasoningReplay::Encrypted,
                max_age: None,
            });
        let body = build_request_body(&request, &config).expect("body");
        assert_eq!(body["input"][0]["encrypted_content"], ciphertext);
        assert!(
            body["input"][0].get("id").is_none(),
            "the endpoint owns item identifiers, not Zuno: {body}"
        );
        assert_eq!(
            body["input"][0]["summary"],
            json!([{ "type": "summary_text", "text": "brief summary" }])
        );
    }

    /// A sealed item with nothing after it is a permanent wire error, so it stays home.
    ///
    /// This is the durable shape a step interrupted right after its reasoning item
    /// leaves behind. Replaying it would fail every later request to the same model
    /// with `Item 'rs_...' of type 'reasoning' was provided without its required
    /// following item`.
    #[test]
    fn a_sealed_item_with_no_following_output_is_not_replayed() {
        let request = CompletionRequest::new(
            "gpt-5.5",
            vec![Message::from_content(
                Role::Assistant,
                vec![RequestContentBlock::ProviderEncryptedReasoning {
                    id: "rs_lonely".to_owned(),
                    summary: Vec::new(),
                    encrypted_content: Some("gAAAAA".to_owned()),
                    status: None,
                }],
            )],
        );
        let config = OpenAiConfig::default().with_reasoning_replay(ReasoningReplayPolicy {
            mode: ReasoningReplay::Encrypted,
            max_age: None,
        });

        let body = build_request_body(&request, &config).expect("body");

        assert_eq!(
            body["input"].as_array().expect("an input array").len(),
            0,
            "a sealed item with no output to explain must not reach the wire: {body}"
        );
    }

    /// An endpoint that seals reasoning fingerprints the tool-argument BYTES.
    ///
    /// `input.to_string()` re-serializes the decoded value and drops the spacing
    /// the endpoint hashed, which is why replaying it drew
    /// `HTTP 400 reasoning_replay_context_mismatch` from kiro-provider on the very
    /// request that carried the sealed envelope.
    #[test]
    fn a_replayed_tool_call_carries_the_provider_argument_bytes_verbatim() {
        let raw = r#"{"command": "python3 -c \"print(1)\"", "intent": "compute"}"#;
        let assistant = Message::from_content(
            Role::Assistant,
            vec![RequestContentBlock::ToolUse {
                id: "call_shell".to_owned(),
                name: "shell".to_owned(),
                input: serde_json::from_str(raw).expect("the captured bytes decode"),
                raw_arguments: Some(raw.to_owned()),
                thought_signature: None,
            }],
        );
        let config = OpenAiConfig::default();

        let responses = build_request_body(
            &CompletionRequest::new("gpt-5.5", vec![assistant.clone()]),
            &config,
        )
        .expect("responses body");
        assert_eq!(
            responses["input"][0]["arguments"],
            json!(raw),
            "the sealed turn's fingerprint covers these bytes: {responses}"
        );

        let chat = build_request_body(
            &CompletionRequest::new("gpt-4.1", vec![assistant]).on_surface(ApiSurface::Chat),
            &config,
        )
        .expect("chat body");
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["function"]["arguments"],
            json!(raw),
            "the Chat surface replays the same bytes: {chat}"
        );
    }

    /// Bytes that no longer decode to the executed value are not sent.
    #[test]
    fn a_rewritten_tool_call_replays_the_executed_value_not_the_stale_bytes() {
        let request = CompletionRequest::new(
            "gpt-5.5",
            vec![Message::from_content(
                Role::Assistant,
                vec![RequestContentBlock::ToolUse {
                    id: "call_shell".to_owned(),
                    name: "shell".to_owned(),
                    input: json!({"command": "ls"}),
                    raw_arguments: Some(r#"{"command": "rm -rf /"}"#.to_owned()),
                    thought_signature: None,
                }],
            )],
        );

        let body = build_request_body(&request, &OpenAiConfig::default()).expect("body");

        assert_eq!(body["input"][0]["arguments"], json!(r#"{"command":"ls"}"#));
    }
}
