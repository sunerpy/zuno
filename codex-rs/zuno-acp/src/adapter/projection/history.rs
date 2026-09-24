use super::*;

pub(in crate::adapter) async fn replay_thread(
    response: &Value,
    session_id: &str,
    client: &ClientConnection,
) -> Result<(), RpcError> {
    for update in history_updates(response) {
        client.session_update(session_id, update).await?;
    }
    Ok(())
}

pub(in crate::adapter) fn history_updates(response: &Value) -> Vec<Value> {
    let mut updates = Vec::new();
    let turns = response
        .pointer("/thread/turns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for item in turns.flat_map(|turn| {
        turn.get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    }) {
        match item.get("type").and_then(Value::as_str) {
            Some("userMessage") => {
                for content in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let text = match content.get("type").and_then(Value::as_str) {
                        Some("text") => content.get("text").and_then(Value::as_str),
                        Some("image") => content.get("url").and_then(Value::as_str),
                        Some("localImage") => content.get("path").and_then(Value::as_str),
                        _ => None,
                    };
                    if let Some(text) = text {
                        updates.push(chunk_update("user_message_chunk", text));
                    }
                }
            }
            Some("agentMessage") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    updates.push(chunk_update("agent_message_chunk", text));
                }
            }
            Some("reasoning") => {
                for text in ["summary", "content"].into_iter().flat_map(|field| {
                    item.get(field)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                }) {
                    if let Some(text) = text.as_str() {
                        updates.push(chunk_update("agent_thought_chunk", text));
                    }
                }
            }
            Some("plan") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    updates.push(chunk_update("agent_thought_chunk", text));
                }
            }
            Some(_) => {
                if let Some(started) = tool_call_started(item) {
                    updates.push(started);
                }
                if let Some(completed) = tool_call_completed(item) {
                    updates.push(completed);
                }
            }
            None => {}
        }
    }
    updates
}

pub(in crate::adapter) fn notification_updates(method: &str, params: &Value) -> Vec<Value> {
    match method {
        "item/agentMessage/delta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|text| vec![chunk_update("agent_message_chunk", text)])
            .unwrap_or_default(),
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" | "item/plan/delta" => {
            params
                .get("delta")
                .and_then(Value::as_str)
                .map(|text| vec![chunk_update("agent_thought_chunk", text)])
                .unwrap_or_default()
        }
        "turn/plan/updated" => {
            let entries = params
                .get("plan")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|step| {
                    json!({
                        "content": step.get("step").and_then(Value::as_str).unwrap_or(""),
                        "priority": "medium",
                        "status": normalize_plan_status(
                            step.get("status").and_then(Value::as_str).unwrap_or("pending")
                        ),
                    })
                })
                .collect::<Vec<_>>();
            vec![json!({ "sessionUpdate": "plan", "entries": entries })]
        }
        "item/started" => params
            .get("item")
            .and_then(tool_call_started)
            .into_iter()
            .collect(),
        "item/completed" => params
            .get("item")
            .and_then(tool_call_completed)
            .into_iter()
            .collect(),
        "item/commandExecution/outputDelta" => {
            let Some(item_id) = params.get("itemId").and_then(Value::as_str) else {
                return Vec::new();
            };
            let Some(delta) = params.get("delta").and_then(Value::as_str) else {
                return Vec::new();
            };
            vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": item_id,
                "status": "in_progress",
                "content": [{ "type": "content", "content": { "type": "text", "text": delta } }],
            })]
        }
        "error" => params
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .map(|text| vec![chunk_update("agent_thought_chunk", text)])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// App Server items that are conversation content or lifecycle markers, not
/// tool invocations. Everything else is projected as an ACP `tool_call`.
fn is_tool_item(item_type: &str) -> bool {
    !matches!(
        item_type,
        "userMessage"
            | "agentMessage"
            | "reasoning"
            | "plan"
            | "hookPrompt"
            | "functionCallOutput"
            | "contextCompaction"
            | "enteredReviewMode"
            | "exitedReviewMode"
    )
}

fn tool_call_started(item: &Value) -> Option<Value> {
    let item_type = item.get("type")?.as_str()?;
    if !is_tool_item(item_type) {
        return None;
    }
    let id = item.get("id")?.as_str()?;
    let (kind, title, name) = tool_identity(item_type, item);
    // `name` is the ACP 1.8 first-class tool identifier; clients use it for
    // fallback labels and to recognise sub-agent spawns (`spawn_agent`).
    Some(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": id,
        "title": title,
        "name": name,
        "kind": kind,
        "status": "in_progress",
        "rawInput": item,
    }))
}

fn tool_call_completed(item: &Value) -> Option<Value> {
    let item_type = item.get("type")?.as_str()?;
    if !is_tool_item(item_type) {
        return None;
    }
    let id = item.get("id")?.as_str()?;
    let status = if item_failed(item) {
        "failed"
    } else {
        "completed"
    };
    Some(json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": status,
        "rawOutput": item,
    }))
}

/// ACP `kind`, human title, and first-class tool `name` for an App Server item.
fn tool_identity(item_type: &str, item: &Value) -> (&'static str, String, String) {
    let tool = |fallback: &str| {
        item.get("tool")
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .to_owned()
    };
    match item_type {
        "commandExecution" => (
            "execute",
            item.get("command")
                .and_then(Value::as_str)
                .unwrap_or("Run command")
                .to_owned(),
            "shell".to_owned(),
        ),
        "fileChange" => (
            "edit",
            "Apply file changes".to_owned(),
            "apply_patch".to_owned(),
        ),
        "mcpToolCall" => {
            let server = item.get("server").and_then(Value::as_str).unwrap_or("MCP");
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
            ("other", format!("{server}.{tool}"), tool.to_owned())
        }
        "dynamicToolCall" => ("other", tool("Tool"), tool("tool")),
        // The wire tool is camelCase (`spawnAgent`, `wait`, `closeAgent`, ...);
        // the first-class name is the snake_case tool name, so only a real
        // spawn is named `spawn_agent` (clients recognise sub-agents by it).
        "collabAgentToolCall" => (
            "think",
            tool("Sub-agent"),
            snake_case(&tool("collab_agent")),
        ),
        "subAgentActivity" => (
            "think",
            "Sub-agent activity".to_owned(),
            "sub_agent_activity".to_owned(),
        ),
        "webSearch" => ("search", "Web search".to_owned(), "web_search".to_owned()),
        "imageView" => ("read", "View image".to_owned(), "view_image".to_owned()),
        "imageGeneration" => (
            "other",
            "Generate image".to_owned(),
            "image_generation".to_owned(),
        ),
        "sleep" => ("other", "Wait".to_owned(), "sleep".to_owned()),
        _ => ("other", item_type.to_owned(), item_type.to_owned()),
    }
}

fn snake_case(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    for ch in value.chars() {
        if ch.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn item_failed(item: &Value) -> bool {
    item.get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "failed" | "declined" | "error"))
        || item.get("error").is_some_and(|error| !error.is_null())
        || item.get("success").and_then(Value::as_bool) == Some(false)
}

fn chunk_update(kind: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": { "type": "text", "text": text },
    })
}

fn normalize_plan_status(status: &str) -> &'static str {
    match status {
        "inProgress" | "in_progress" => "in_progress",
        "completed" => "completed",
        _ => "pending",
    }
}
