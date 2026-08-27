//! OpenAI Chat Completions and Responses request construction.

use std::fmt;

use serde_json::{Map, Value, json};
use zuno_error::ProviderError;
use zuno_llm::event::{Message, RequestContentBlock, Role};
use zuno_llm::registry::{ApiSurface, CompletionRequest};

use crate::provider::OpenAiConfig;

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
    match resolve_surface(request.surface) {
        ApiSurface::Chat => build_chat_body(request, config),
        ApiSurface::Responses => build_responses_body(request, config),
        ApiSurface::Messages | ApiSurface::Default => {
            Err(ProviderError::fatal(RequestShapeError::UnsupportedSurface))
        }
    }
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
                id, name, input, ..
            } => calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": input.to_string() },
            })),
            RequestContentBlock::ProviderEncryptedReasoning { .. } => {
                return Err(ProviderError::fatal(
                    RequestShapeError::EncryptedReasoningOnChat,
                ));
            }
            RequestContentBlock::SignedThinking { .. }
            | RequestContentBlock::ToolResult { .. }
            | RequestContentBlock::Image { .. } => {}
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

fn responses_assistant(message: &Message) -> Result<Vec<Value>, ProviderError> {
    let mut items = Vec::new();
    let mut output_content = Vec::new();
    for block in &message.content {
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
            RequestContentBlock::SignedThinking { .. } => {
                return Err(ProviderError::fatal(RequestShapeError::ForeignThinking));
            }
            RequestContentBlock::ToolResult { .. } | RequestContentBlock::Image { .. } => {}
        }
    }
    if !output_content.is_empty() {
        items.push(json!({ "role": "assistant", "content": output_content }));
    }
    Ok(items)
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
    MissingEncryptedReasoning,
    ForeignThinking,
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
            Self::MissingEncryptedReasoning => {
                formatter.write_str("OpenAI reasoning replay has no encrypted_content")
            }
            Self::ForeignThinking => formatter.write_str(
                "signed thinking from another provider cannot be sent to OpenAI Responses",
            ),
        }
    }
}

impl std::error::Error for RequestShapeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_llm::event::RequestContentBlock;

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

    #[test]
    fn encrypted_reasoning_replays_byte_for_byte_without_item_id() {
        let ciphertext = "gAAAAA_exact_provider_bytes";
        let request = CompletionRequest::new(
            "gpt-5.5",
            vec![Message::from_content(
                Role::Assistant,
                vec![RequestContentBlock::ProviderEncryptedReasoning {
                    id: "rs_not_replayed_when_store_false".to_owned(),
                    summary: vec!["brief summary".to_owned()],
                    encrypted_content: Some(ciphertext.to_owned()),
                    status: None,
                }],
            )],
        );
        let body =
            build_request_body(&request, &OpenAiConfig::stateless_reasoning()).expect("body");
        assert_eq!(body["input"][0]["encrypted_content"], ciphertext);
        assert!(body["input"][0].get("id").is_none());
        assert_eq!(
            body["input"][0]["summary"],
            json!([{ "type": "summary_text", "text": "brief summary" }])
        );
    }
}
