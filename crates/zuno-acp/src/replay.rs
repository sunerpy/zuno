//! Project durable Zuno messages onto ACP `session/update` notifications.

use serde_json::{Map, Value, json};
use zuno_db::message::{MessageRole, MessageWithParts, PartKind, PartRecord};
use zuno_db::session::{MessageUsage, TokenAccounting};
use zuno_engine::r#loop::ToolDiff;
use zuno_tool::ToolOutput;
use zuno_types::WorkStateProjection;

use crate::projection::{completed_tool_update, tool_call};

#[must_use]
pub fn durable_updates(history: &[MessageWithParts]) -> Vec<Value> {
    history.iter().flat_map(message_updates).collect::<Vec<_>>()
}

/// Replays the last provider-confirmed context usage, never cumulative session tokens.
#[must_use]
pub fn durable_usage_update(
    history: &[MessageWithParts],
    context_size: u64,
    cumulative_cost: f64,
) -> Option<Value> {
    if context_size == 0 {
        return None;
    }
    let usage = history
        .iter()
        .rev()
        .filter(|message| message.info.role == MessageRole::Assistant)
        .map(|message| MessageUsage::from_data(&message.info.data))
        .find(|usage| usage.reported)?;
    let prompt = match usage.accounting? {
        TokenAccounting::CacheInsideInput => usage.tokens.input,
        TokenAccounting::CacheBesideInput => usage
            .tokens
            .input
            .saturating_add(usage.tokens.cache_read)
            .saturating_add(usage.tokens.cache_write),
    };
    let used = u64::try_from(prompt.saturating_add(usage.tokens.output).max(0)).ok()?;
    let mut update = json!({
        "sessionUpdate": "usage_update",
        "used": used,
        "size": context_size,
    });
    if cumulative_cost.is_finite() && cumulative_cost >= 0.0 {
        update["cost"] = json!({ "amount": cumulative_cost, "currency": "USD" });
    }
    Some(update)
}

/// Replays the complete current plan snapshot in stable ACP's three-state vocabulary.
#[must_use]
pub fn durable_plan_update(work: &WorkStateProjection) -> Option<Value> {
    let plan = work.plan.as_ref()?;
    let mut entries = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        let status = match step.status.as_str() {
            "pending" => "pending",
            "in_progress" => "in_progress",
            "completed" => "completed",
            _ => return None,
        };
        entries.push(json!({
            "content": step.title,
            "priority": plan_step_priority(work, &step.id),
            "status": status,
            "_meta": { "zuno": { "stepId": step.id } },
        }));
    }
    Some(json!({
        "sessionUpdate": "plan",
        "entries": entries,
        "_meta": {
            "zuno": {
                "planId": plan.id,
                "revision": plan.revision,
                "title": plan.title,
            }
        },
    }))
}

fn plan_step_priority<'a>(work: &'a WorkStateProjection, step_id: &str) -> &'a str {
    let mut resolved = None;
    for item in work
        .todos
        .iter()
        .filter(|item| item.plan_step_id.as_deref() == Some(step_id))
    {
        match item.priority.as_str() {
            "high" => return "high",
            "medium" => resolved = Some("medium"),
            "low" if resolved.is_none() => resolved = Some("low"),
            _ => {}
        }
    }
    resolved.unwrap_or("medium")
}

fn message_updates(stored: &MessageWithParts) -> Vec<Value> {
    let mut updates = Vec::new();
    for part in &stored.parts {
        match part.kind {
            PartKind::Text => {
                if let Some(text) = non_empty_string(&part.data, "text") {
                    updates.push(message_content_update(
                        message_kind(stored.info.role),
                        json!({ "type": "text", "text": text }),
                        &stored.info.id,
                    ));
                }
            }
            PartKind::Reasoning => {
                if let Some(text) = non_empty_string(&part.data, "text") {
                    updates.push(message_content_update(
                        "agent_thought_chunk",
                        json!({ "type": "text", "text": text }),
                        &stored.info.id,
                    ));
                }
            }
            PartKind::File => {
                if let Some(content) = stored_file_content(&part.data) {
                    updates.push(message_content_update(
                        message_kind(stored.info.role),
                        content,
                        &stored.info.id,
                    ));
                }
            }
            PartKind::Tool => updates.extend(tool_updates(part)),
            PartKind::StepStart
            | PartKind::StepFinish
            | PartKind::Snapshot
            | PartKind::Patch
            | PartKind::Agent
            | PartKind::Subtask
            | PartKind::Retry
            | PartKind::Compaction => {}
        }
    }
    updates
}

fn message_kind(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user_message_chunk",
        MessageRole::Assistant => "agent_message_chunk",
    }
}

fn message_content_update(kind: &str, content: Value, message_id: &str) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": content,
        "messageId": message_id,
    })
}

fn tool_updates(part: &PartRecord) -> Vec<Value> {
    let Some(call_id) = non_empty_string(&part.data, "callID") else {
        return Vec::new();
    };
    let Some(name) = non_empty_string(&part.data, "tool") else {
        return Vec::new();
    };
    let display_name = non_empty_string(&part.data, "displayName").unwrap_or(name);
    let state = part.data.get("state").and_then(Value::as_object);
    let status = state
        .and_then(|state| state.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("pending");
    let raw_input = state
        .and_then(|state| state.get("raw"))
        .and_then(Value::as_str)
        .map(json_or_string)
        .or_else(|| state.and_then(|state| state.get("input")).cloned());
    let initial_status = if status == "running" {
        "in_progress"
    } else {
        "pending"
    };
    let mut updates = vec![tool_call(
        call_id,
        display_name,
        name,
        initial_status,
        raw_input,
    )];
    if !matches!(status, "completed" | "error") {
        return updates;
    }

    let output = state
        .and_then(|state| state.get("output").or_else(|| state.get("error")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = state
        .and_then(|state| state.get("title"))
        .and_then(Value::as_str)
        .unwrap_or(display_name);
    let metadata = state
        .and_then(|state| state.get("metadata"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut durable_output = ToolOutput::text(title, output);
    durable_output.metadata = metadata;
    let written_paths = durable_output
        .written_paths()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let diff = ToolDiff::from_output(&durable_output);
    let mut completed = completed_tool_update(
        call_id,
        display_name,
        title,
        output,
        diff.as_ref(),
        &written_paths,
        status == "error",
    );
    if let Some(content) = completed.get_mut("content").and_then(Value::as_array_mut) {
        content.extend(
            state
                .and_then(|state| state.get("attachments"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(tool_attachment_content),
        );
    }
    if state
        .and_then(|state| state.get("outcome"))
        .and_then(Value::as_str)
        == Some("blocked")
    {
        let kind = state
            .and_then(|state| state.get("blockKind"))
            .and_then(Value::as_str)
            .unwrap_or("blocked");
        completed["_meta"] = json!({ "zuno": { "blockedKind": kind } });
    }
    updates.push(completed);
    updates
}

fn stored_file_content(data: &Map<String, Value>) -> Option<Value> {
    let mime = non_empty_string(data, "mime");
    let url = non_empty_string(data, "url");
    if mime.is_some_and(|mime| mime.starts_with("image/")) {
        let mime = mime?;
        if let Some(payload) = non_empty_string(data, "data")
            .or_else(|| url.and_then(|url| data_url_payload(url, mime)))
        {
            let mut content = Map::new();
            content.insert("type".to_owned(), json!("image"));
            content.insert("data".to_owned(), json!(payload));
            content.insert("mimeType".to_owned(), json!(mime));
            if let Some(url) = url {
                content.insert("uri".to_owned(), json!(url));
            }
            return Some(Value::Object(content));
        }
    }
    let uri = url?;
    let name = non_empty_string(data, "filename")
        .or_else(|| uri.rsplit('/').next().filter(|name| !name.is_empty()))
        .unwrap_or("attachment");
    let mut content = Map::new();
    content.insert("type".to_owned(), json!("resource_link"));
    content.insert("name".to_owned(), json!(name));
    content.insert("uri".to_owned(), json!(uri));
    if let Some(title) = non_empty_string(data, "title") {
        content.insert("title".to_owned(), json!(title));
    }
    if let Some(description) = non_empty_string(data, "description") {
        content.insert("description".to_owned(), json!(description));
    }
    if let Some(mime) = mime {
        content.insert("mimeType".to_owned(), json!(mime));
    }
    if let Some(size) = data.get("size").and_then(Value::as_u64) {
        content.insert("size".to_owned(), json!(size));
    }
    Some(Value::Object(content))
}

fn tool_attachment_content(value: &Value) -> Option<Value> {
    let content = stored_file_content(value.as_object()?)?;
    Some(json!({ "type": "content", "content": content }))
}

fn non_empty_string<'a>(data: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn data_url_payload<'a>(url: &'a str, mime: &str) -> Option<&'a str> {
    let (header, payload) = url.split_once(',')?;
    (header == format!("data:{mime};base64") && !payload.is_empty()).then_some(payload)
}

fn json_or_string(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zuno_db::message::{MessageRecord, MessageWithParts, PartRecord};

    use super::*;

    fn message(id: &str, role: &str, parts: Vec<PartRecord>) -> MessageWithParts {
        MessageWithParts {
            info: MessageRecord::from_json(json!({
                "id": id,
                "sessionID": "ses",
                "role": role,
                "time": { "created": 1 },
            }))
            .expect("message fixture"),
            parts,
        }
    }

    fn part(id: &str, message_id: &str, data: Value) -> PartRecord {
        let mut data = data.as_object().expect("part object").clone();
        data.insert("id".to_owned(), Value::String(id.to_owned()));
        data.insert("sessionID".to_owned(), Value::String("ses".to_owned()));
        data.insert("messageID".to_owned(), Value::String(message_id.to_owned()));
        PartRecord::from_json(Value::Object(data), 1).expect("part fixture")
    }

    #[test]
    fn replay_preserves_content_tools_typed_diffs_attachments_and_order() {
        let user = message(
            "msg-user",
            "user",
            vec![
                part("p-text", "msg-user", json!({"type":"text","text":"hello"})),
                part(
                    "p-image",
                    "msg-user",
                    json!({
                        "type": "file",
                        "filename": "pixel.png",
                        "mime": "image/png",
                        "data": "aGVsbG8=",
                        "url": "data:image/png;base64,aGVsbG8=",
                    }),
                ),
            ],
        );
        let assistant = message(
            "msg-assistant",
            "assistant",
            vec![
                part(
                    "p-thought",
                    "msg-assistant",
                    json!({"type":"reasoning","text":"thinking"}),
                ),
                part(
                    "p-tool",
                    "msg-assistant",
                    json!({
                        "type": "tool",
                        "callID": "call-edit",
                        "tool": "edit",
                        "displayName": "Edit file",
                        "state": {
                            "status": "completed",
                            "raw": "{\"filePath\":\"/work/demo.rs\"}",
                            "input": {"filePath":"/work/demo.rs"},
                            "title": "Updated demo.rs",
                            "output": "ok",
                            "metadata": {
                                "diff": "@@ -1 +1 @@\n-old\n+new\n",
                                "fileDiffs": [{
                                    "path": "/work/demo.rs",
                                    "oldText": "old\n",
                                    "newText": "new\n"
                                }],
                                "writtenPaths": ["/work/demo.rs"]
                            },
                            "attachments": [{
                                "type": "file",
                                "mime": "image/png",
                                "filename": "preview.png",
                                "url": "data:image/png;base64,cHJldmlldw=="
                            }]
                        }
                    }),
                ),
                part(
                    "p-answer",
                    "msg-assistant",
                    json!({"type":"text","text":"done"}),
                ),
            ],
        );

        let updates = durable_updates(&[user, assistant]);
        assert_eq!(updates.len(), 6, "every durable visible part is replayed");
        assert_eq!(updates[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(updates[0]["messageId"], "msg-user");
        assert_eq!(updates[0]["content"]["text"], "hello");
        assert_eq!(updates[1]["content"]["type"], "image");
        assert_eq!(updates[1]["content"]["mimeType"], "image/png");
        assert_eq!(updates[2]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(updates[2]["messageId"], "msg-assistant");
        assert_eq!(updates[3]["sessionUpdate"], "tool_call");
        assert_eq!(updates[3]["rawInput"]["filePath"], "/work/demo.rs");
        assert_eq!(updates[4]["sessionUpdate"], "tool_call_update");
        assert_eq!(updates[4]["status"], "completed");
        assert_eq!(updates[4]["content"][1]["type"], "diff");
        assert_eq!(updates[4]["content"][2]["content"]["type"], "image");
        assert_eq!(updates[4]["locations"], json!([{"path":"/work/demo.rs"}]));
        assert_eq!(updates[5]["content"]["text"], "done");
        assert_eq!(updates[5]["messageId"], "msg-assistant");
    }

    #[test]
    fn replay_usage_is_the_latest_context_not_cumulative_session_tokens() {
        let mut assistant = message("msg-usage", "assistant", Vec::new());
        assistant.info.data.insert(
            "tokens".to_owned(),
            json!({
                "input": 100,
                "output": 25,
                "reasoning": 0,
                "cache": {"read": 40, "write": 10},
                "accounting": "cache-beside-input"
            }),
        );
        let update = durable_usage_update(&[assistant], 200_000, 1.25)
            .expect("reported latest-message usage");
        assert_eq!(update["sessionUpdate"], "usage_update");
        assert_eq!(update["used"], 175);
        assert_eq!(update["size"], 200_000);
        assert_eq!(update["cost"], json!({"amount":1.25,"currency":"USD"}));
    }

    #[test]
    fn replay_plan_is_a_complete_stable_snapshot_with_todo_priority() {
        let work = zuno_types::WorkStateProjection {
            plan: Some(zuno_types::PlanProjection {
                id: "plan-1".to_owned(),
                goal_id: None,
                revision: 3,
                title: "Ship ACP".to_owned(),
                steps: vec![
                    zuno_types::PlanStepProjection {
                        id: "implement".to_owned(),
                        title: "Implement replay".to_owned(),
                        status: "in_progress".to_owned(),
                    },
                    zuno_types::PlanStepProjection {
                        id: "verify".to_owned(),
                        title: "Verify in Zed".to_owned(),
                        status: "pending".to_owned(),
                    },
                ],
                span: zuno_types::ExecutionSpan::default(),
                time_created: 1,
                time_updated: 2,
            }),
            todos: vec![zuno_types::TodoProjection {
                id: "todo-1".to_owned(),
                goal_id: None,
                plan_step_id: Some("implement".to_owned()),
                parent_id: None,
                subject: "Implement durable projector".to_owned(),
                description: String::new(),
                active_form: None,
                status: "in_progress".to_owned(),
                priority: "high".to_owned(),
                dependencies: Vec::new(),
                owner: None,
                revision: 1,
                span: zuno_types::ExecutionSpan::default(),
                time_created: 1,
                time_updated: 2,
            }],
            ..zuno_types::WorkStateProjection::default()
        };
        let update = durable_plan_update(&work).expect("durable plan");
        assert_eq!(update["sessionUpdate"], "plan");
        assert_eq!(update["entries"].as_array().map(Vec::len), Some(2));
        assert_eq!(update["entries"][0]["status"], "in_progress");
        assert_eq!(update["entries"][0]["priority"], "high");
        assert_eq!(update["entries"][1]["priority"], "medium");
        assert_eq!(update["_meta"]["zuno"]["planId"], "plan-1");
        assert_eq!(update["_meta"]["zuno"]["revision"], 3);
    }

    #[test]
    fn replay_plan_fails_closed_on_an_unrepresentable_status() {
        let work = zuno_types::WorkStateProjection {
            plan: Some(zuno_types::PlanProjection {
                id: "plan-1".to_owned(),
                goal_id: None,
                revision: 1,
                title: "Invalid".to_owned(),
                steps: vec![zuno_types::PlanStepProjection {
                    id: "blocked".to_owned(),
                    title: "Blocked".to_owned(),
                    status: "blocked".to_owned(),
                }],
                span: zuno_types::ExecutionSpan::default(),
                time_created: 1,
                time_updated: 1,
            }),
            ..zuno_types::WorkStateProjection::default()
        };
        assert!(durable_plan_update(&work).is_none());
    }
}
