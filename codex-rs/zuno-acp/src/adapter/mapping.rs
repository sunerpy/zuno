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
    // The bridge calls no client-side method beyond `session/request_permission`
    // and `session/update`, which every ACP client implements, so the advertised
    // fs/terminal capabilities do not change its behaviour. They are still
    // validated so a malformed handshake fails here instead of on a later call.
    for (field, value) in [
        ("clientCapabilities", params.get("clientCapabilities")),
        ("clientInfo", params.get("clientInfo")),
    ] {
        match value {
            None | Some(Value::Null) | Some(Value::Object(_)) => {}
            Some(_) => {
                return Err(RpcError::invalid_params(format!(
                    "{field} must be an object"
                )));
            }
        }
    }
    if let Some(info) = params.get("clientInfo").and_then(Value::as_object) {
        let client = info
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let version = info.get("version").and_then(Value::as_str).unwrap_or("");
        tracing::debug!(client, version, "ACP client initialized");
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
        collaboration_mode: response
            .pointer("/collaborationMode/mode")
            .or_else(|| response.pointer("/thread/collaborationMode/mode"))
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_owned(),
        approval_policy: approval_policy_id(response.get("approvalPolicy")),
        approvals_reviewer: response
            .get("approvalsReviewer")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_owned(),
        sandbox_type: response
            .pointer("/sandbox/type")
            .or_else(|| response.pointer("/sandboxPolicy/type"))
            .and_then(Value::as_str)
            .unwrap_or("workspaceWrite")
            .to_owned(),
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

/// Parse a `model/list` response page into catalog entries (in server order).
pub(super) fn catalog_from_model_list(response: &Value) -> Vec<ModelEntry> {
    response
        .get("data")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    let id = model.get("id").and_then(Value::as_str)?;
                    let name = model
                        .get("displayName")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .unwrap_or(id);
                    Some(ModelEntry {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        description: model
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        efforts: model
                            .get("supportedReasoningEfforts")
                            .and_then(Value::as_array)
                            .map(|efforts| {
                                efforts
                                    .iter()
                                    .filter_map(|effort| {
                                        Some((
                                            effort.get("reasoningEffort")?.as_str()?.to_owned(),
                                            effort
                                                .get("description")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_owned(),
                                        ))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        hidden: model
                            .get("hidden")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `collaborationMode/list`: for every mode, the model and effort the
/// preset switches to. Efforts come as `reasoningEffort: null | "medium"`.
pub(super) fn collaboration_presets_from_list(response: &Value) -> Vec<CollaborationPreset> {
    response
        .get("data")
        .and_then(Value::as_array)
        .map(|presets| {
            presets
                .iter()
                .filter_map(|preset| {
                    let mode = preset.get("mode")?.as_str()?.to_owned();
                    let effort = match preset.get("reasoningEffort") {
                        None => None,
                        Some(Value::Null) => Some(None),
                        Some(Value::String(effort)) => Some(Some(effort.clone())),
                        Some(_) => None,
                    };
                    Some(CollaborationPreset {
                        mode,
                        model: preset
                            .get("model")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        effort,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The models a client may pick: every visible catalog entry, plus the
/// session's current model when the catalog does not list it (a custom model
/// from config, or a catalog the App Server could not provide).
fn selectable_models(route: &SessionRoute, catalog: &[ModelEntry]) -> Vec<ModelEntry> {
    let mut models: Vec<ModelEntry> = catalog
        .iter()
        .filter(|model| !model.hidden || model.id == route.model)
        .cloned()
        .collect();
    if !models.iter().any(|model| model.id == route.model) {
        models.insert(
            0,
            ModelEntry {
                id: route.model.clone(),
                name: route.model.clone(),
                description: String::new(),
                efforts: Vec::new(),
                hidden: false,
            },
        );
    }
    models
}

const EFFORT_LABELS: &[(&str, &str)] = &[
    ("none", "None"),
    ("minimal", "Minimal"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra high"),
    ("max", "Maximum"),
    ("ultra", "Ultra"),
];

fn effort_label(effort: &str) -> String {
    EFFORT_LABELS
        .iter()
        .find(|(id, _)| *id == effort)
        .map(|(_, label)| (*label).to_owned())
        .unwrap_or_else(|| effort.to_owned())
}

/// Reasoning efforts offered for the session's model: the catalog's list for
/// that model, or every known effort when the catalog does not describe it.
fn effort_options(route: &SessionRoute, catalog: &[ModelEntry]) -> Vec<Value> {
    let current = route.effort.as_deref();
    let mut options: Vec<Value> = catalog
        .iter()
        .find(|model| model.id == route.model)
        .filter(|model| !model.efforts.is_empty())
        .map(|model| {
            model
                .efforts
                .iter()
                .map(|(effort, description)| {
                    json!({ "value": effort, "name": effort_label(effort), "description": description })
                })
                .collect()
        })
        .unwrap_or_else(|| {
            EFFORT_LABELS
                .iter()
                .filter(|(id, _)| !matches!(*id, "none" | "minimal"))
                .map(|(id, label)| json!({ "value": id, "name": label }))
                .collect()
        });
    if let Some(current) = current
        && !options.iter().any(|option| option["value"] == current)
    {
        options.insert(
            0,
            json!({ "value": current, "name": effort_label(current) }),
        );
    }
    options
}

/// `approvalPolicy` is a string for the simple policies and an object
/// (`{"granular": {...}}`) for the granular one.
pub(super) fn approval_policy_id(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(policy)) => policy.clone(),
        Some(Value::Object(map)) => map
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "granular".to_owned()),
        _ => "on-request".to_owned(),
    }
}

pub(super) fn lifecycle_response(
    session_id: &str,
    route: &SessionRoute,
    catalog: &[ModelEntry],
) -> Value {
    let available_models: Vec<Value> = selectable_models(route, catalog)
        .iter()
        .map(|model| {
            json!({
                "modelId": model.id,
                "name": model.name,
                "description": model.description,
                "_meta": { "zuno": { "provider": route.model_provider } },
            })
        })
        .collect();
    json!({
        "sessionId": session_id,
        "configOptions": config_options(route, catalog),
        "modes": session_modes(route),
        "models": {
            "currentModelId": route.model,
            "availableModels": available_models,
        },
        "_meta": {
            "zuno": {
                "threadId": session_id,
                "cwd": route.cwd,
                "modelProvider": route.model_provider,
                "reasoningEffort": route.effort,
                "collaborationMode": route.collaboration_mode,
                "approvalPolicy": route.approval_policy,
                "approvalsReviewer": route.approvals_reviewer,
                "sandbox": route.sandbox_type,
            },
        },
    })
}

pub(super) fn config_options(route: &SessionRoute, catalog: &[ModelEntry]) -> Vec<Value> {
    let model_options: Vec<Value> = selectable_models(route, catalog)
        .iter()
        .map(|model| json!({ "value": model.id, "name": model.name, "description": model.description }))
        .collect();
    let modes = session_modes(route);
    let mode_options: Vec<Value> = modes["availableModes"]
        .as_array()
        .map(|modes| {
            modes
                .iter()
                .map(|mode| json!({ "value": mode["id"], "name": mode["name"], "description": mode["description"] }))
                .collect()
        })
        .unwrap_or_default();
    let collaboration_options: Vec<Value> = COLLABORATION_MODES
        .iter()
        .map(|(id, name, description)| json!({ "value": id, "name": name, "description": description }))
        .collect();
    vec![
        json!({
            "id": "mode", "name": "Mode", "category": "mode", "type": "select",
            "description": "Approval and sandboxing preset for the session",
            "currentValue": modes["currentModeId"],
            "options": mode_options,
        }),
        json!({
            "id": "collaboration_mode", "name": "Collaboration mode", "category": "collaboration_mode",
            "type": "select", "description": "How Zuno collaborates for the following turns",
            "currentValue": route.collaboration_mode,
            "options": collaboration_options,
        }),
        json!({
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": route.model,
            "options": model_options,
        }),
        json!({
            "id": "reasoning_effort", "name": "Reasoning effort", "category": "thought_level",
            "type": "select", "currentValue": route.effort.as_deref().unwrap_or("medium"),
            "options": effort_options(route, catalog),
        }),
    ]
}

/// Translate ACP prompt blocks into App Server `UserInput` items.
///
/// The text shapes match the official `codex-acp` adapter (`[@name](uri)` links
/// and `<context ref="uri">` wrappers) so a prompt means the same thing to the
/// model whichever Codex ACP agent a client talks to. Only block types the
/// `initialize` response advertises are accepted.
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
                // Always inline the client's bytes: App Server rejects remote
                // image URLs (`validate_user_input_image_urls`), so an optional
                // `uri` is informational only and must not replace `data`.
                let mime = required_string(block, "mimeType")?;
                let data = required_string(block, "data")?;
                input.push(json!({
                    "type": "image",
                    "url": format!("data:{mime};base64,{data}"),
                }));
            }
            Some("resource_link") => {
                let uri = required_string(block, "uri")?;
                let name = block.get("name").and_then(Value::as_str);
                input.push(text_input(uri_as_link(name, &uri)));
            }
            Some("resource") => {
                let resource = block
                    .get("resource")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        RpcError::invalid_params("resource block must contain resource")
                    })?;
                let uri = resource
                    .get("uri")
                    .and_then(Value::as_str)
                    .filter(|uri| !uri.is_empty())
                    .ok_or_else(|| RpcError::invalid_params("resource must contain uri"))?;
                let link = uri_as_link(None, uri);
                if let Some(text) = resource.get("text").and_then(Value::as_str) {
                    input.push(text_input(format!(
                        "{link}\n<context ref=\"{uri}\">\n{text}\n</context>"
                    )));
                } else if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
                    let mime = resource
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream");
                    if mime.starts_with("image/") {
                        input.push(json!({
                            "type": "image", "url": format!("data:{mime};base64,{blob}"),
                        }));
                    } else {
                        input.push(text_input(format!(
                            "{link}\n<context ref=\"{uri}\" mimeType=\"{mime}\" encoding=\"base64\">\n{blob}\n</context>"
                        )));
                    }
                } else {
                    return Err(RpcError::invalid_params(
                        "resource must contain text or a blob",
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

fn text_input(text: String) -> Value {
    json!({ "type": "text", "text": text, "textElements": [] })
}

/// `[@name](uri)`, defaulting the label to the file name of a `file://` URI.
fn uri_as_link(name: Option<&str>, uri: &str) -> String {
    match name.filter(|name| !name.is_empty()) {
        Some(name) => format!("[@{name}]({uri})"),
        None => match uri.strip_prefix("file://") {
            Some(path) => {
                let file_name = path
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(path);
                format!("[@{file_name}]({uri})")
            }
            None => uri.to_owned(),
        },
    }
}

/// `session/set_mode` is the dedicated ACP v1 mode method; the same change
/// (an approval/sandbox preset) is expressed through the `mode` config option,
/// so it is translated to that call.
pub(super) fn set_mode_as_config_option(params: &Value) -> Result<Value, RpcError> {
    let session_id = required_string(params, "sessionId")?;
    let mode_id = required_string(params, "modeId")?;
    Ok(json!({ "sessionId": session_id, "configId": "mode", "value": mode_id }))
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
