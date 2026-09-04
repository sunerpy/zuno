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
    request
        .validate_tool_arguments()
        .map_err(ProviderError::fatal)?;
    let (system, messages) = split_system(&request.messages)?;
    let mut wire_messages = messages
        .into_iter()
        .map(message_value)
        .collect::<Result<Vec<_>, _>>()?;

    let mut system_blocks = Vec::new();
    if let Some(system) = system {
        let mut block = json!({ "type": "text", "text": system.as_str() });
        if config.prompt_cache() {
            add_cache_control(&mut block);
        }
        system_blocks.push(block);
    }
    system_blocks.extend(
        request
            .developer_context
            .iter()
            .filter(|content| !content.trim().is_empty())
            .map(|content| json!({"type": "text", "text": content})),
    );

    let mut root = Map::new();
    root.insert("model".to_owned(), Value::String(request.model_id.clone()));
    root.insert(
        "max_tokens".to_owned(),
        Value::Number(config.max_tokens().into()),
    );
    root.insert("messages".to_owned(), Value::Array(wire_messages.clone()));
    root.insert("stream".to_owned(), Value::Bool(true));

    if !system_blocks.is_empty() {
        root.insert("system".to_owned(), Value::Array(system_blocks));
    } else if config.prompt_cache() && add_last_message_cache_breakpoint(&mut wire_messages) {
        root.insert("messages".to_owned(), Value::Array(wire_messages));
    }

    for (field, value) in [
        ("temperature", config.temperature()),
        ("top_p", config.top_p()),
    ] {
        if let Some(value) = value {
            root.insert(
                field.to_owned(),
                Value::Number(finite_number(field, value)?),
            );
        }
    }
    let tools = tool_definitions(request, config);
    if !tools.is_empty() {
        // `tool_choice` is only legal alongside `tools`, and a `{"type":"tool"}` choice is
        // only legal when that tool is one of the entries being sent: the Messages API
        // answers either mismatch with a permanent 400, so the documented option has to
        // stay unsent rather than end the turn as a provider failure. Withholding it can
        // only leave the model free to answer in prose — it can never produce a call the
        // request did not offer.
        let tool_choice = config
            .tool_choice()
            .filter(|choice| tool_choice_is_satisfiable(choice, &tools))
            .cloned();
        root.insert("tools".to_owned(), Value::Array(tools));
        if let Some(tool_choice) = tool_choice {
            root.insert("tool_choice".to_owned(), tool_choice);
        }
    }
    if let Some(thinking) = config.thinking() {
        root.insert("thinking".to_owned(), thinking.clone());
    }

    Ok(Value::Object(root))
}

/// The `tools` array this request carries, in Anthropic's top-level shape.
///
/// The per-turn snapshot in [`CompletionRequest::tools`] is authoritative for the tools
/// it can express: it is what the turn loop locked and what tool dispatch is checked
/// against, so a configured *custom* declaration — one carrying its own `input_schema` —
/// is superseded rather than shown beside it. A configured entry with no `input_schema`
/// is an Anthropic-run server or client tool (`{"type":"web_search_20250305","name":
/// "web_search"}`), which no snapshot can ever contain and which 0.6.6 put on the wire;
/// dropping it in favour of the snapshot both disabled a supported configuration and
/// stranded a `toolChoice` naming it. Those entries travel with the snapshot, minus any
/// whose name a locked tool already occupies, because two entries sharing one name is
/// itself a permanent 400.
fn tool_definitions(request: &CompletionRequest, config: &AnthropicConfig) -> Vec<Value> {
    if request.tools.is_empty() {
        return config.tools().to_vec();
    }
    let mut tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect::<Vec<_>>();
    tools.extend(
        config
            .tools()
            .iter()
            .filter(|tool| tool.get("input_schema").is_none())
            .filter(|tool| match tool.get("name").and_then(Value::as_str) {
                Some(name) => !request.tools.iter().any(|locked| locked.name == name),
                None => true,
            })
            .cloned(),
    );
    tools
}

/// Whether a configured `tool_choice` names a tool the body actually carries.
///
/// Only the `{"type":"tool","name":N}` form names anything; `auto`, `any` and `none`
/// constrain the whole array and are satisfiable whenever it is non-empty. A name the
/// array does not carry is not resolvable from anything this request trusts, so the
/// directive is dropped rather than sent as a name Anthropic will reject.
fn tool_choice_is_satisfiable(choice: &Value, tools: &[Value]) -> bool {
    let Some(chosen) = choice.get("name").and_then(Value::as_str) else {
        return true;
    };
    tools
        .iter()
        .any(|tool| tool.get("name").and_then(Value::as_str) == Some(chosen))
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
            let Some(text) = block.provider_text() else {
                return Err(ProviderError::fatal(RequestShapeError::NonTextSystemBlock));
            };
            system_text.push(text.into_owned());
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
        RequestContentBlock::ResourceLink { .. } => {
            let Some(text) = block.provider_text() else {
                unreachable!("resource links always have a provider text projection")
            };
            Ok(json!({ "type": "text", "text": text.as_ref() }))
        }
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
        RequestContentBlock::Image {
            media_type, data, ..
        } => Ok(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        })),
        RequestContentBlock::ImageAttachment { .. } => {
            unreachable!("attachment references must be resolved before provider request shaping")
        }
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

/// `value` as a JSON number, preferring an integer encoding when it is whole.
///
/// The integer branch is not cosmetic: `serde_json` renders `1.0` as `1.0`, and a
/// `temperature` of `1.0` therefore reached the wire as a float where the vendor's
/// own SDK sends `1`. The `i64` range check guards the cast, and a non-finite value
/// has no JSON encoding at all, so it is refused by name rather than silently
/// serialised as `null`.
fn finite_number(field: &'static str, value: f64) -> Result<serde_json::Number, ProviderError> {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the branch is entered only for a whole value already range-checked \
                  against i64"
    )]
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        return Ok(serde_json::Number::from(value as i64));
    }
    serde_json::Number::from_f64(value)
        .ok_or_else(|| ProviderError::fatal(RequestShapeError::NotFinite(field, value)))
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
    NotFinite(&'static str, f64),
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
            Self::NotFinite(field, value) => {
                write!(formatter, "Anthropic {field} is not finite: {value}")
            }
        }
    }
}

impl std::error::Error for RequestShapeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_llm::event::ThoughtSignature;
    use zuno_llm::registry::ToolSchema;

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
        )
        .with_developer_context(vec!["active goal".to_owned(), "memory".to_owned()]);
        let body = build_request_body(&request, &config()).expect("request");
        assert_eq!(
            body["system"],
            json!([
                {
                    "type": "text",
                    "text": "stable system bytes",
                    "cache_control": { "type": "ephemeral" },
                },
                {"type": "text", "text": "active goal"},
                {"type": "text", "text": "memory"},
            ])
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
                    raw_arguments: None,
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

    /// The body a provider configured from an option bag sends.
    ///
    /// Through `AnthropicConfig::from_spec` rather than the `with_*` builders,
    /// because the bag is what the composition root writes: a test using the
    /// builders would keep passing if `from_spec` stopped reading a key.
    fn body_from_options(options: serde_json::Value) -> Value {
        let mut spec = zuno_llm::registry::Spec::new("anthropic");
        for (name, value) in options.as_object().expect("options are an object") {
            spec = spec.with_option(name.clone(), value.clone());
        }
        let request = CompletionRequest::new(
            "claude-sonnet-4-6",
            vec![Message::new(Role::User, "Say hello.")],
        );
        build_request_body(&request, &AnthropicConfig::from_spec(spec)).expect("request")
    }

    #[test]
    fn the_generation_controls_reach_the_messages_body() {
        let body = body_from_options(json!({
            "maxTokens": 8_192,
            "temperature": 0.3,
            "topP": 0.9
        }));

        assert_eq!(body["max_tokens"], json!(8_192));
        assert_eq!(body["temperature"], json!(0.3));
        assert_eq!(
            body["top_p"],
            json!(0.9),
            "`top_p` is one of the four sampling fields `anthropic-messages.ts:546-549` \
             writes, and this build had no field to carry it, so a configured cutoff was \
             accepted and dropped"
        );
    }

    #[test]
    fn a_whole_sampling_value_is_sent_as_an_integer() {
        let body = body_from_options(json!({"temperature": 1.0, "topP": 1.0}));

        assert_eq!(
            serde_json::to_string(&body["temperature"]).expect("temperature serialises"),
            "1",
            "`1.0` renders as `1.0` unless the whole-number branch converts it, and the \
             vendor's own SDK sends `1`"
        );
        assert_eq!(
            serde_json::to_string(&body["top_p"]).expect("top_p serialises"),
            "1"
        );
    }

    /// The turn loop locks its tool snapshot into `CompletionRequest::tools`, so a body
    /// built from a request that carries one must show the model those tools; anything
    /// else advertises `tool_calls` while making a tool call impossible.
    #[test]
    fn the_locked_tool_snapshot_reaches_the_messages_body() {
        let request = CompletionRequest::new(
            "claude-sonnet-4-6",
            vec![Message::new(Role::User, "Read the file.")],
        )
        .with_tools(vec![ToolSchema {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        }]);

        let body = build_request_body(&request, &config()).expect("request");

        assert_eq!(
            body["tools"],
            json!([{
                "name": "read",
                "description": "Read a file",
                "input_schema": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                },
            }])
        );
    }

    /// The Messages API rejects `tool_choice` with no `tools` as a permanent 400, so a
    /// configured `toolChoice` on a request with no tools must stay off the wire.
    #[test]
    fn tool_choice_is_withheld_when_the_request_offers_no_tools() {
        let body = body_from_options(json!({"toolChoice": {"type": "any"}}));

        assert!(
            body.get("tool_choice").is_none(),
            "`tool_choice` without `tools` is a permanent request-shape error"
        );
    }

    #[test]
    fn tool_choice_accompanies_a_non_empty_tool_snapshot() {
        let mut spec = zuno_llm::registry::Spec::new("anthropic");
        spec = spec.with_option("toolChoice".to_owned(), json!({"type": "any"}));
        let request = CompletionRequest::new(
            "claude-sonnet-4-6",
            vec![Message::new(Role::User, "Read the file.")],
        )
        .with_tools(vec![ToolSchema {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: json!({"type": "object"}),
        }]);

        let body =
            build_request_body(&request, &AnthropicConfig::from_spec(spec)).expect("request");

        assert_eq!(body["tool_choice"], json!({"type": "any"}));
    }

    /// A configured Anthropic server tool and a `toolChoice` naming it.
    ///
    /// The exact configuration round 2 broke: `tools` carries a server tool the locked
    /// snapshot can never contain (there is no `input_schema` for a tool Anthropic runs
    /// itself), and `toolChoice` names it. With the snapshot simply superseding the
    /// option, the body named `web_search` in `tool_choice` while `tools` held only
    /// `read` — a permanent 400 no retry can clear, on a configuration that worked in
    /// 0.6.6. The oracle is set membership between the two fields, not the presence of
    /// either.
    #[test]
    fn a_configured_server_tool_travels_with_the_snapshot_that_supersedes_custom_tools() {
        let mut spec = zuno_llm::registry::Spec::new("anthropic");
        spec = spec.with_option(
            "tools".to_owned(),
            json!([{"type": "web_search_20250305", "name": "web_search", "max_uses": 3}]),
        );
        spec = spec.with_option(
            "toolChoice".to_owned(),
            json!({"type": "tool", "name": "web_search"}),
        );
        let request = CompletionRequest::new(
            "claude-sonnet-4-6",
            vec![Message::new(Role::User, "Search the web.")],
        )
        .with_tools(vec![ToolSchema {
            name: "read".to_owned(),
            description: "Read".to_owned(),
            parameters: json!({"type": "object"}),
        }]);

        let body =
            build_request_body(&request, &AnthropicConfig::from_spec(spec)).expect("request");

        let sent = tool_names(&body);
        assert!(
            sent.contains(&"read".to_owned()),
            "the locked snapshot still decides which custom tools the model sees: {sent:?}"
        );
        assert_eq!(
            body["tools"][1],
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3}),
            "a server tool is forwarded byte for byte, as 0.6.6 forwarded it"
        );
        let chosen = body["tool_choice"]["name"]
            .as_str()
            .expect("the configured choice names a tool")
            .to_owned();
        assert!(
            sent.contains(&chosen),
            "`tool_choice` names `{chosen}` while `tools` carries {sent:?}: Messages \
             answers that pair with a permanent 400"
        );
    }

    /// A `toolChoice` whose tool is in neither the snapshot nor the option bag.
    ///
    /// Nothing can satisfy it, so the directive is not resolvable and stays off the wire
    /// rather than travelling as a name the array does not carry. Dropping it can only
    /// free the model to answer in prose; it can never cause a call the user did not
    /// offer.
    #[test]
    fn tool_choice_naming_a_tool_the_body_never_carries_stays_off_the_wire() {
        let mut spec = zuno_llm::registry::Spec::new("anthropic");
        spec = spec.with_option(
            "toolChoice".to_owned(),
            json!({"type": "tool", "name": "web_search"}),
        );
        let request = CompletionRequest::new(
            "claude-sonnet-4-6",
            vec![Message::new(Role::User, "Search the web.")],
        )
        .with_tools(vec![ToolSchema {
            name: "read".to_owned(),
            description: "Read".to_owned(),
            parameters: json!({"type": "object"}),
        }]);

        let body =
            build_request_body(&request, &AnthropicConfig::from_spec(spec)).expect("request");

        assert_eq!(tool_names(&body), vec!["read".to_owned()]);
        assert!(
            body.get("tool_choice").is_none(),
            "an unsatisfiable `tool_choice` is a permanent 400, so it is not sent"
        );
    }

    /// A configured custom tool is still superseded: the snapshot is what dispatch
    /// checks, so a duplicate `read` declaration cannot reach the wire beside it.
    #[test]
    fn a_configured_custom_tool_never_joins_the_snapshot_that_supersedes_it() {
        let mut spec = zuno_llm::registry::Spec::new("anthropic");
        spec = spec.with_option(
            "tools".to_owned(),
            json!([
                {"name": "read", "description": "stale", "input_schema": {"type": "object"}},
                {"name": "shell", "description": "other", "input_schema": {"type": "object"}},
            ]),
        );
        let request = CompletionRequest::new(
            "claude-sonnet-4-6",
            vec![Message::new(Role::User, "Read the file.")],
        )
        .with_tools(vec![ToolSchema {
            name: "read".to_owned(),
            description: "Read".to_owned(),
            parameters: json!({"type": "object"}),
        }]);

        let body =
            build_request_body(&request, &AnthropicConfig::from_spec(spec)).expect("request");

        assert_eq!(tool_names(&body), vec!["read".to_owned()]);
        assert_eq!(body["tools"][0]["description"], json!("Read"));
    }

    /// A server tool whose name collides with a locked tool keeps the snapshot's entry:
    /// two entries sharing one name is itself a 400.
    #[test]
    fn a_server_tool_named_like_a_locked_tool_does_not_duplicate_it() {
        let mut spec = zuno_llm::registry::Spec::new("anthropic");
        spec = spec.with_option(
            "tools".to_owned(),
            json!([{"type": "bash_20250124", "name": "read"}]),
        );
        let request = CompletionRequest::new(
            "claude-sonnet-4-6",
            vec![Message::new(Role::User, "Read the file.")],
        )
        .with_tools(vec![ToolSchema {
            name: "read".to_owned(),
            description: "Read".to_owned(),
            parameters: json!({"type": "object"}),
        }]);

        let body =
            build_request_body(&request, &AnthropicConfig::from_spec(spec)).expect("request");

        assert_eq!(tool_names(&body), vec!["read".to_owned()]);
    }

    /// The `name` of every entry in the assembled `tools` array.
    fn tool_names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .expect("the body carries a tools array")
            .iter()
            .map(|tool| {
                tool["name"]
                    .as_str()
                    .expect("every tool entry is named")
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn an_unset_sampling_control_writes_no_field() {
        let body = body_from_options(json!({"maxTokens": 8_192}));

        assert!(
            body.get("temperature").is_none() && body.get("top_p").is_none(),
            "an omitted control must stay omitted, or every request would pin a value \
             the user never chose"
        );
    }
}
