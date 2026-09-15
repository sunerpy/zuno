use super::*;

pub(super) fn initialize(params: &Value) -> Result<Value, RpcError> {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("protocolVersion must be a number"))?;
    if requested < ACP_PROTOCOL_VERSION {
        return Err(RpcError::invalid_params(format!(
            "unsupported ACP protocol version {requested}"
        )));
    }
    Ok(json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "mcpCapabilities": { "stdio": true, "http": true, "sse": false },
            "promptCapabilities": { "embeddedContext": true, "image": true },
            "sessionCapabilities": {
                "close": {}, "fork": {}, "list": {}, "resume": {},
            },
        },
        "authMethods": [],
        "agentInfo": { "name": "Zuno", "version": env!("CARGO_PKG_VERSION") },
    }))
}

pub(super) fn route_from_lifecycle(
    response: &Value,
    fallback_cwd: &str,
    client: &ClientConnection,
) -> Result<SessionRoute, RpcError> {
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| response.pointer("/thread/model").and_then(Value::as_str))
        .ok_or_else(|| RpcError::internal("thread lifecycle response omitted model"))?
        .to_owned();
    Ok(SessionRoute {
        client: client.session_scoped(),
        cwd: response
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or(fallback_cwd)
            .to_owned(),
        model,
        model_provider: response
            .get("modelProvider")
            .and_then(Value::as_str)
            .or_else(|| {
                response
                    .pointer("/thread/modelProvider")
                    .and_then(Value::as_str)
            })
            .unwrap_or("unknown")
            .to_owned(),
        effort: response
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(str::to_owned),
        mode: "build".to_owned(),
        active_turn_id: response
            .pointer("/thread/turns")
            .and_then(Value::as_array)
            .and_then(|turns| {
                turns
                    .iter()
                    .rev()
                    .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
            })
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

pub(super) fn lifecycle_response(session_id: &str, route: &SessionRoute) -> Value {
    json!({
        "sessionId": session_id,
        "configOptions": config_options(route),
        "modes": {
            "currentModeId": route.mode,
            "availableModes": [
                { "id": "build", "name": "Build" },
                { "id": "plan", "name": "Plan" },
            ],
        },
        "models": {
            "currentModelId": route.model,
            "availableModels": [{
                "modelId": route.model,
                "name": route.model,
                "_meta": { "zuno": { "provider": route.model_provider } },
            }],
        },
        "_meta": {
            "zuno": {
                "threadId": session_id,
                "cwd": route.cwd,
                "modelProvider": route.model_provider,
                "reasoningEffort": route.effort,
            },
        },
    })
}

pub(super) fn config_options(route: &SessionRoute) -> Vec<Value> {
    vec![
        json!({
            "id": "mode", "name": "Mode", "category": "mode", "type": "select",
            "currentValue": route.mode,
            "options": [
                { "value": "build", "name": "Build" },
                { "value": "plan", "name": "Plan" },
            ],
        }),
        json!({
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": route.model,
            "options": [{ "value": route.model, "name": route.model }],
        }),
        json!({
            "id": "reasoning_effort", "name": "Reasoning effort", "category": "thought_level",
            "type": "select", "currentValue": route.effort.as_deref().unwrap_or("medium"),
            "options": [
                { "value": "low", "name": "Low" },
                { "value": "medium", "name": "Medium" },
                { "value": "high", "name": "High" },
                { "value": "xhigh", "name": "Extra high" },
                { "value": "max", "name": "Maximum" },
                { "value": "ultra", "name": "Ultra" },
            ],
        }),
    ]
}

pub(super) fn acp_prompt_to_codex(prompt: Option<&Value>) -> Result<Vec<Value>, RpcError> {
    let blocks = prompt
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("prompt must be an array"))?;
    let mut input = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = required_string(block, "text")?;
                input.push(json!({ "type": "text", "text": text, "textElements": [] }));
            }
            Some("image") => {
                let mime = required_string(block, "mimeType")?;
                let data = required_string(block, "data")?;
                input.push(json!({
                    "type": "image",
                    "url": format!("data:{mime};base64,{data}"),
                }));
            }
            Some("resource_link") => {
                let uri = required_string(block, "uri")?;
                let name = block.get("name").and_then(Value::as_str).unwrap_or(&uri);
                input.push(json!({
                    "type": "text",
                    "text": format!("Resource {name}: {uri}"),
                    "textElements": [],
                }));
            }
            Some("resource") => {
                let resource = block
                    .get("resource")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        RpcError::invalid_params("resource block must contain resource")
                    })?;
                if let Some(text) = resource.get("text").and_then(Value::as_str) {
                    input.push(json!({ "type": "text", "text": text, "textElements": [] }));
                } else if let (Some(mime), Some(blob)) = (
                    resource.get("mimeType").and_then(Value::as_str),
                    resource.get("blob").and_then(Value::as_str),
                ) {
                    input.push(json!({
                        "type": "image", "url": format!("data:{mime};base64,{blob}"),
                    }));
                } else {
                    return Err(RpcError::invalid_params(
                        "resource must contain text or a typed blob",
                    ));
                }
            }
            Some(other) => {
                return Err(RpcError::invalid_params(format!(
                    "unsupported ACP prompt block type {other}"
                )));
            }
            None => return Err(RpcError::invalid_params("prompt block omitted type")),
        }
    }
    if input.is_empty() {
        return Err(RpcError::invalid_params(
            "prompt must contain text, an image, or a resource",
        ));
    }
    Ok(input)
}

pub(super) fn mcp_server_config(value: Option<&Value>) -> Result<Map<String, Value>, RpcError> {
    let Some(value) = value else {
        return Ok(Map::new());
    };
    let servers = value
        .as_array()
        .ok_or_else(|| RpcError::invalid_params("mcpServers must be an array"))?;
    let mut config = Map::new();
    for server in servers {
        let name = required_string(server, "name")?;
        if !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        {
            return Err(RpcError::invalid_params(
                "MCP server name may contain only ASCII letters, digits, '-' and '_'",
            ));
        }
        let prefix = format!("mcp_servers.{name}");
        if let Some(command) = server.get("command").and_then(Value::as_str) {
            config.insert(format!("{prefix}.command"), json!(command));
            if let Some(args) = server.get("args") {
                config.insert(format!("{prefix}.args"), args.clone());
            }
            if let Some(env) = server.get("env") {
                config.insert(format!("{prefix}.env"), env.clone());
            }
        } else if let Some(url) = server.get("url").and_then(Value::as_str) {
            config.insert(format!("{prefix}.url"), json!(url));
            if let Some(headers) = server.get("headers") {
                config.insert(format!("{prefix}.http_headers"), headers.clone());
            }
        } else {
            return Err(RpcError::invalid_params(format!(
                "MCP server {name} must contain command or url"
            )));
        }
    }
    Ok(config)
}

pub(super) fn session_list_entry(thread: &Value) -> Option<Value> {
    let id = thread.get("id")?.as_str()?;
    let cwd = thread.get("cwd")?.as_str()?;
    let title = thread
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .or_else(|| thread.get("preview").and_then(Value::as_str))
        .unwrap_or("Zuno session");
    let updated_at = thread
        .get("updatedAt")
        .and_then(Value::as_i64)
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| Utc::now().to_rfc3339());
    Some(json!({
        "sessionId": id,
        "cwd": cwd,
        "title": title,
        "updatedAt": updated_at,
    }))
}

pub(super) fn prompt_response(stop_reason: &str, message_id: Option<String>) -> Value {
    let mut response = json!({ "stopReason": stop_reason, "_meta": {} });
    if let Some(message_id) = message_id {
        response["userMessageId"] = Value::String(message_id);
    }
    response
}

pub(super) fn message_id(params: &Value) -> Result<Option<String>, RpcError> {
    let value = params
        .get("messageId")
        .or_else(|| params.pointer("/_meta/zuno/messageId"));
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() && value.len() <= 256 => {
            Ok(Some(value.clone()))
        }
        _ => Err(RpcError::invalid_params(
            "messageId must be a non-empty string of at most 256 bytes",
        )),
    }
}

pub(super) fn zuno_meta(params: &Value) -> Result<&Map<String, Value>, RpcError> {
    match params.pointer("/_meta/zuno") {
        None | Some(Value::Null) => {
            static EMPTY: std::sync::LazyLock<Map<String, Value>> =
                std::sync::LazyLock::new(Map::new);
            Ok(&EMPTY)
        }
        Some(Value::Object(meta)) => Ok(meta),
        Some(_) => Err(RpcError::invalid_params("_meta.zuno must be an object")),
    }
}

pub(super) fn copy_optional_string(
    source: &Map<String, Value>,
    source_key: &str,
    target: &mut Map<String, Value>,
    target_key: &str,
) -> Result<(), RpcError> {
    match source.get(source_key) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(value)) if !value.is_empty() => {
            target.insert(target_key.to_owned(), Value::String(value.clone()));
            Ok(())
        }
        Some(_) => Err(RpcError::invalid_params(format!(
            "_meta.zuno.{source_key} must be a non-empty string"
        ))),
    }
}

pub(super) fn copy_optional_value(
    source: &Map<String, Value>,
    source_key: &str,
    target: &mut Map<String, Value>,
    target_key: &str,
) {
    if let Some(value) = source.get(source_key).filter(|value| !value.is_null()) {
        target.insert(target_key.to_owned(), value.clone());
    }
}

pub(super) fn validate_effort(value: &str) -> Result<(), RpcError> {
    if matches!(
        value,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
    ) {
        Ok(())
    } else {
        Err(RpcError::invalid_params(format!(
            "unsupported reasoning effort {value}"
        )))
    }
}

pub(super) fn required_string(params: &Value, field: &str) -> Result<String, RpcError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| RpcError::invalid_params(format!("{field} must be a non-empty string")))
}

pub(super) fn app_server_rpc_error(method: &str, error: JSONRPCErrorError) -> RpcError {
    RpcError::downstream(
        error.code,
        format!("{method} failed: {}", error.message),
        error.data,
    )
}
