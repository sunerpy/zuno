use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use zuno_engine::r#loop::{ToolDiff, TurnEvent};
use zuno_llm::event::StreamEvent;

#[derive(Debug, Default)]
pub struct TurnEventProjector {
    context_size: Option<u64>,
    raw_inputs: HashMap<String, String>,
    visible_tools: HashSet<String>,
}

impl TurnEventProjector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_context_size(context_size: u64) -> Self {
        Self {
            context_size: Some(context_size),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn project(&mut self, event: &TurnEvent) -> Option<Value> {
        self.project_inner(event)
    }

    #[must_use]
    fn project_inner(&mut self, event: &TurnEvent) -> Option<Value> {
        match event {
            TurnEvent::Provider {
                event: StreamEvent::TextDelta(text),
                ..
            } => Some(content_update("agent_message_chunk", text)),
            TurnEvent::Provider {
                event: StreamEvent::ReasoningDelta(text),
                ..
            } => Some(content_update("agent_thought_chunk", text)),
            TurnEvent::SessionTitleUpdated { title } => Some(json!({
                "sessionUpdate": "session_info_update",
                "title": title,
            })),
            TurnEvent::SessionCommandOutput { content, .. } => {
                Some(content_update("agent_message_chunk", content))
            }
            TurnEvent::Provider {
                event: StreamEvent::ToolUseStart { id, .. },
                ..
            } => {
                self.raw_inputs.entry(id.clone()).or_default();
                None
            }
            TurnEvent::Provider {
                event: StreamEvent::ToolInputDelta { id, delta },
                ..
            } => {
                let visible = self.visible_tools.contains(id);
                let raw_input = self.raw_inputs.entry(id.clone()).or_default();
                raw_input.push_str(delta);
                visible.then(|| {
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "rawInput": json_or_string(raw_input),
                    })
                })
            }
            TurnEvent::ToolCallStarted {
                call_id,
                display_name,
                name,
                ..
            } => {
                self.visible_tools.insert(call_id.clone());
                Some(tool_call(
                    call_id,
                    display_name,
                    name,
                    "pending",
                    self.raw_inputs
                        .get(call_id)
                        .map(|value| json_or_string(value)),
                ))
            }
            TurnEvent::ToolDispatchStarted {
                call_id,
                display_name,
                name,
                ..
            } => {
                self.visible_tools.insert(call_id.clone());
                Some(tool_call(call_id, display_name, name, "in_progress", None))
            }
            TurnEvent::ToolDispatchBlocked { call_id, kind, .. } => {
                self.raw_inputs.remove(call_id);
                self.visible_tools.remove(call_id);
                let kind = kind.as_str();
                Some(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "status": "failed",
                    "rawOutput": { "blocked": true, "kind": kind },
                    "content": [text_content(&format!("Tool dispatch blocked: {kind}"))],
                    "_meta": {
                        "zuno": {
                            "blockedKind": kind,
                        },
                    },
                }))
            }
            TurnEvent::ToolDispatchCompleted {
                call_id,
                display_name,
                title,
                output,
                diff,
                written_paths,
                is_error,
                ..
            } => {
                self.raw_inputs.remove(call_id);
                self.visible_tools.remove(call_id);
                Some(completed_tool_update(
                    call_id,
                    display_name,
                    title,
                    output,
                    diff.as_ref(),
                    written_paths,
                    *is_error,
                ))
            }
            TurnEvent::Provider {
                event:
                    StreamEvent::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    },
                ..
            } => {
                self.raw_inputs.remove(tool_use_id);
                self.visible_tools.remove(tool_use_id);
                Some(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_use_id,
                    "status": if *is_error { "failed" } else { "completed" },
                    "rawOutput": json_or_string(content),
                    "content": [text_content(content)],
                }))
            }
            TurnEvent::Provider {
                event:
                    StreamEvent::TokenUsage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_write_input_tokens,
                        accounting,
                    },
                ..
            } => self.context_size.map(|size| {
                let used = accounting
                    .prompt_total(
                        input_tokens.unwrap_or_default(),
                        cache_read_input_tokens.unwrap_or_default(),
                        cache_write_input_tokens.unwrap_or_default(),
                    )
                    .saturating_add(output_tokens.unwrap_or_default());
                json!({ "sessionUpdate": "usage_update", "used": used, "size": size })
            }),
            TurnEvent::Provider {
                event: StreamEvent::StatusDetail { detail },
                ..
            }
            | TurnEvent::Provider {
                event:
                    StreamEvent::Error {
                        message: detail, ..
                    },
                ..
            } => Some(content_update("agent_thought_chunk", detail)),
            _ => None,
        }
    }
}

#[must_use]
pub fn turn_event_update(event: &TurnEvent) -> Option<Value> {
    TurnEventProjector::new().project(event)
}

#[must_use]
pub fn tool_call(
    call_id: &str,
    title: &str,
    name: &str,
    status: &str,
    raw_input: Option<Value>,
) -> Value {
    let mut update = json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call_id,
        "title": title,
        "kind": tool_kind(name),
        "status": status,
    });
    if let Some(raw_input) = raw_input {
        update["rawInput"] = raw_input;
    }
    update
}

#[must_use]
pub fn completed_tool_update(
    call_id: &str,
    display_name: &str,
    title: &str,
    output: &str,
    diff: Option<&ToolDiff>,
    written_paths: &[String],
    is_error: bool,
) -> Value {
    let mut content = vec![text_content(output)];
    if let Some(diff) = diff {
        if diff.files().is_empty() {
            if let Some(unified) = diff.unified() {
                content.push(unified_diff_content(unified));
            }
        } else {
            content.extend(diff.files().iter().map(file_diff_content));
        }
    }
    let locations = tool_locations(written_paths, diff);
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": call_id,
        "title": if title.is_empty() { display_name } else { title },
        "status": if is_error { "failed" } else { "completed" },
        "rawOutput": json_or_string(output),
        "content": content,
    });
    if !locations.is_empty() {
        update["locations"] = Value::Array(locations);
    }
    update
}

fn content_update(kind: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": { "type": "text", "text": text },
    })
}

fn json_or_string(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
}

fn text_content(text: &str) -> Value {
    json!({
        "type": "content",
        "content": { "type": "text", "text": text },
    })
}

fn file_diff_content(diff: &zuno_tool::FileDiff) -> Value {
    json!({
        "type": "diff",
        "path": diff.path(),
        "oldText": diff.old_text(),
        "newText": diff.new_text(),
    })
}

fn unified_diff_content(diff: &str) -> Value {
    json!({
        "type": "content",
        "content": { "type": "text", "text": diff },
        "_meta": {
            "zuno": {
                "kind": "unified_diff",
            },
        },
    })
}

fn tool_locations(paths: &[String], diff: Option<&ToolDiff>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut locations = Vec::new();
    for path in paths {
        if seen.insert(path.as_str()) {
            locations.push(json!({ "path": path }));
        }
    }
    if let Some(diff) = diff {
        for file in diff.files() {
            if seen.insert(file.path()) {
                locations.push(json!({ "path": file.path() }));
            }
        }
    }
    locations
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "glob" => "read",
        "write" | "edit" | "apply_patch" => "edit",
        "delete" => "delete",
        "move" => "move",
        "grep" | "search" => "search",
        "shell" | "execute" => "execute",
        "fetch" | "webfetch" => "fetch",
        _ => "other",
    }
}
