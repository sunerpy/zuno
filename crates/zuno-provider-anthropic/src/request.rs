//! Anthropic Messages request construction.

use std::fmt;

use serde_json::{Map, Value, json};
use zuno_error::ProviderError;
use zuno_llm::cache::StaticSystemPrompt;
use zuno_llm::event::{Message, RequestContentBlock, Role};
use zuno_llm::registry::CompletionRequest;

use crate::provider::AnthropicConfig;

/// Build the JSON body sent to Anthropic's Messages endpoint.
///
/// System-role text is frozen through [`StaticSystemPrompt`] and placed in the
/// dedicated `system` field. With prompt caching enabled, its final block gets
/// the cache breakpoint; when no system prefix exists, the final message block
/// gets the fallback breakpoint used by Anthropic's SDK.
pub fn build_request_body(
    request: &CompletionRequest,
    config: &AnthropicConfig,
) -> Result<Value, ProviderError> {
    let (system, messages) = split_system(&request.messages)?;
    let mut wire_messages = messages
        .into_iter()
        .map(message_value)
        .collect::<Result<Vec<_>, _>>()?;

    let mut root = Map::new();
    root.insert("model".to_owned(), Value::String(request.model_id.clone()));
    root.insert(
        "max_tokens".to_owned(),
        Value::Number(config.max_tokens().into()),
    );
    root.insert("messages".to_owned(), Value::Array(wire_messages.clone()));
    root.insert("stream".to_owned(), Value::Bool(true));

    if let Some(system) = system {
        let mut block = json!({ "type": "text", "text": system.as_str() });
        if config.prompt_cache() {
            add_cache_control(&mut block);
        }
        root.insert("system".to_owned(), Value::Array(vec![block]));
    } else if config.prompt_cache() && add_last_message_cache_breakpoint(&mut wire_messages) {
        root.insert("messages".to_owned(), Value::Array(wire_messages));
    }

    if let Some(temperature) = config.temperature() {
        let number = if temperature.fract() == 0.0
            && temperature >= i64::MIN as f64
            && temperature <= i64::MAX as f64
        {
            serde_json::Number::from(temperature as i64)
        } else {
            serde_json::Number::from_f64(temperature).ok_or_else(|| {
                ProviderError::fatal(RequestShapeError::InvalidTemperature(temperature))
            })?
        };
        root.insert("temperature".to_owned(), Value::Number(number));
    }
    if !config.tools().is_empty() {
        root.insert("tools".to_owned(), Value::Array(config.tools().to_vec()));
    }
    if let Some(tool_choice) = config.tool_choice() {
        root.insert("tool_choice".to_owned(), tool_choice.clone());
    }
    if let Some(thinking) = config.thinking() {
        root.insert("thinking".to_owned(), thinking.clone());
    }

    Ok(Value::Object(root))
}

fn split_system(
    messages: &[Message],
) -> Result<(Option<StaticSystemPrompt>, Vec<&Message>), ProviderError> {
    let mut system_text = Vec::new();
    let mut conversational = Vec::new();

    for message in messages {
        if message.role != Role::System {
            conversational.push(message);
            continue;
        }
        for block in &message.content {
            match block {
                RequestContentBlock::Text { text } => system_text.push(text.as_str()),
                _ => {
                    return Err(ProviderError::fatal(RequestShapeError::NonTextSystemBlock));
                }
            }
        }
    }

    let system = if system_text.is_empty() {
        None
    } else {
        Some(StaticSystemPrompt::new(system_text.join("\n\n")))
    };
    Ok((system, conversational))
}

fn message_value(message: &Message) -> Result<Value, ProviderError> {
    let role = match message.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
        Role::System => {
            return Err(ProviderError::fatal(
                RequestShapeError::SystemMessageInConversation,
            ));
        }
    };
    let content = message
        .content
        .iter()
        .map(content_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "role": role, "content": content }))
}

fn content_value(block: &RequestContentBlock) -> Result<Value, ProviderError> {
    match block {
        RequestContentBlock::Text { text } => Ok(json!({ "type": "text", "text": text })),
        RequestContentBlock::SignedThinking {
            thinking,
            signature,
        } => Ok(json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature,
        })),
        RequestContentBlock::ProviderEncryptedReasoning {
            encrypted_content,
            status,
            ..
        } if status.as_deref() == Some("redacted_thinking") => {
            let data = encrypted_content.as_deref().ok_or_else(|| {
                ProviderError::fatal(RequestShapeError::MissingRedactedThinkingData)
            })?;
            Ok(json!({ "type": "redacted_thinking", "data": data }))
        }
        RequestContentBlock::ProviderEncryptedReasoning { .. } => Err(ProviderError::fatal(
            RequestShapeError::ForeignEncryptedReasoning,
        )),
        RequestContentBlock::ToolUse {
            id, name, input, ..
        } => Ok(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })),
        RequestContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut value = json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
            });
            if let Some(is_error) = is_error {
                value["is_error"] = Value::Bool(*is_error);
            }
            Ok(value)
        }
        RequestContentBlock::Image { media_type, data } => Ok(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        })),
    }
}

fn add_last_message_cache_breakpoint(messages: &mut [Value]) -> bool {
    let Some(last_message) = messages.last_mut() else {
        return false;
    };
    let Some(content) = last_message
        .get_mut("content")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let Some(last_block) = content.last_mut() else {
        return false;
    };
    add_cache_control(last_block);
    true
}

fn add_cache_control(block: &mut Value) {
    if let Some(object) = block.as_object_mut() {
        object.insert("cache_control".to_owned(), json!({ "type": "ephemeral" }));
    }
}

#[derive(Debug)]
enum RequestShapeError {
    NonTextSystemBlock,
    SystemMessageInConversation,
    MissingRedactedThinkingData,
    ForeignEncryptedReasoning,
    InvalidTemperature(f64),
}

impl fmt::Display for RequestShapeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonTextSystemBlock => {
                formatter.write_str("Anthropic system messages may contain text only")
            }
            Self::SystemMessageInConversation => {
                formatter.write_str("Anthropic system message was not extracted")
            }
            Self::MissingRedactedThinkingData => {
                formatter.write_str("Anthropic redacted thinking has no encrypted data")
            }
            Self::ForeignEncryptedReasoning => formatter.write_str(
                "provider-encrypted reasoning from another provider cannot be sent to Anthropic",
            ),
            Self::InvalidTemperature(value) => {
                write!(formatter, "Anthropic temperature is not finite: {value}")
            }
        }
    }
}

impl std::error::Error for RequestShapeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_llm::event::ThoughtSignature;

    fn config() -> AnthropicConfig {
        AnthropicConfig::default().with_max_tokens(1_024)
    }

    #[test]
    fn signed_thinking_round_trips_into_the_next_request_unchanged() {
        let request = CompletionRequest::new(
            "claude-sonnet-4-6",
            vec![Message::from_content(
                Role::Assistant,
                vec![RequestContentBlock::SignedThinking {
                    thinking: "careful reasoning".to_owned(),
                    signature: "sig_exact_bytes_123".to_owned(),
                }],
            )],
        );
        let body = build_request_body(&request, &config()).expect("request");
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({
                "type": "thinking",
                "thinking": "careful reasoning",
                "signature": "sig_exact_bytes_123",
                "cache_control": { "type": "ephemeral" },
            })
        );
    }

    #[test]
    fn static_system_prefix_gets_the_cache_breakpoint_not_dynamic_messages() {
        let request = CompletionRequest::new(
            "claude-haiku-4-5-20251001",
            vec![
                Message::new(Role::System, "stable system bytes"),
                Message::new(Role::User, "volatile turn"),
            ],
        );
        let body = build_request_body(&request, &config()).expect("request");
        assert_eq!(
            body["system"],
            json!([{
                "type": "text",
                "text": "stable system bytes",
                "cache_control": { "type": "ephemeral" },
            }])
        );
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn anthropic_ignores_gemini_thought_signatures_on_tool_blocks() {
        let request = CompletionRequest::new(
            "claude-haiku-4-5-20251001",
            vec![Message::from_content(
                Role::Assistant,
                vec![RequestContentBlock::ToolUse {
                    id: "toolu_1".to_owned(),
                    name: "weather".to_owned(),
                    input: json!({ "city": "Paris" }),
                    thought_signature: Some(ThoughtSignature::new("gemini-only")),
                }],
            )],
        );
        let body = build_request_body(&request, &config()).expect("request");
        assert_eq!(body["messages"][0]["content"][0]["type"], "tool_use");
        assert!(
            body["messages"][0]["content"][0]
                .get("thought_signature")
                .is_none()
        );
    }
}
