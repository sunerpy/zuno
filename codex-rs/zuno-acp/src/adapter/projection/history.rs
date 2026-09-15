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

fn tool_call_started(item: &Value) -> Option<Value> {
    let item_type = item.get("type")?.as_str()?;
    if matches!(
        item_type,
        "userMessage" | "agentMessage" | "reasoning" | "plan" | "hookPrompt"
    ) {
        return None;
    }
    let id = item.get("id")?.as_str()?;
    let (kind, title) = tool_identity(item_type, item);
    Some(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": id,
        "title": title,
        "kind": kind,
        "status": "in_progress",
        "rawInput": item,
    }))
}

fn tool_call_completed(item: &Value) -> Option<Value> {
    let item_type = item.get("type")?.as_str()?;
    if matches!(
        item_type,
        "userMessage" | "agentMessage" | "reasoning" | "plan" | "hookPrompt"
    ) {
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

fn tool_identity(item_type: &str, item: &Value) -> (&'static str, String) {
    match item_type {
        "commandExecution" => (
            "execute",
            item.get("command")
                .and_then(Value::as_str)
                .unwrap_or("Run command")
                .to_owned(),
        ),
        "fileChange" => ("edit", "Apply file changes".to_owned()),
        "mcpToolCall" => (
            "other",
            format!(
                "{}.{}",
                item.get("server").and_then(Value::as_str).unwrap_or("MCP"),
                item.get("tool").and_then(Value::as_str).unwrap_or("tool")
            ),
        ),
        "dynamicToolCall" => (
            "other",
            item.get("tool")
                .and_then(Value::as_str)
                .unwrap_or("Tool")
                .to_owned(),
        ),
        "collabAgentToolCall" | "subAgentActivity" => (
            "think",
            item.get("tool")
                .and_then(Value::as_str)
                .unwrap_or("Sub-agent")
                .to_owned(),
        ),
        "webSearch" => ("search", "Web search".to_owned()),
        _ => ("other", item_type.to_owned()),
    }
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
