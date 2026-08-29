use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use zuno_engine::r#loop::{INTERRUPTED_TURN_NOTICE, ToolDiff, ToolInterruption, TurnEvent};
use zuno_llm::event::StreamEvent;

use crate::presentation::{decorate_completed_tool_update, decorate_tool_call};

#[derive(Debug, Default)]
pub struct TurnEventProjector {
    context_size: Option<u64>,
    raw_inputs: HashMap<String, String>,
    tool_names: HashMap<String, String>,
    visible_tools: HashSet<String>,
}

/// ACP projection that exposes provider output only after its attempt is durable.
///
/// ACP message chunks are append-only. Holding attempt-scoped updates until
/// [`TurnEvent::AssistantCheckpointed`] is therefore the only protocol-safe way
/// to discard a failed partial stream when the engine emits `RetryRollback`.
#[derive(Debug, Default)]
pub struct AttemptBufferedTurnEventProjector {
    projector: TurnEventProjector,
    pending: Vec<Value>,
    buffering: bool,
}

impl AttemptBufferedTurnEventProjector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_context_size(context_size: u64) -> Self {
        Self {
            projector: TurnEventProjector::with_context_size(context_size),
            ..Self::default()
        }
    }

    /// Project one engine event into zero or more committed ACP updates.
    #[must_use]
    pub fn project(&mut self, event: &TurnEvent) -> Vec<Value> {
        match event {
            TurnEvent::ProviderRequestStarted { .. } => {
                self.pending.clear();
                self.projector.reset_attempt();
                self.buffering = true;
                Vec::new()
            }
            TurnEvent::Provider {
                event: StreamEvent::RetryRollback { .. },
                ..
            } => {
                self.pending.clear();
                self.projector.reset_attempt();
                Vec::new()
            }
            TurnEvent::AssistantCheckpointed { .. } => {
                if let Some(update) = self.projector.project(event) {
                    self.pending.push(update);
                }
                self.buffering = false;
                std::mem::take(&mut self.pending)
            }
            _ if self.buffering => {
                if let Some(update) = self.projector.project(event) {
                    self.pending.push(update);
                }
                Vec::new()
            }
            _ => self.projector.project(event).into_iter().collect(),
        }
    }
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

    fn reset_attempt(&mut self) {
        self.raw_inputs.clear();
        self.tool_names.clear();
        self.visible_tools.clear();
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
                event: StreamEvent::ToolUseStart { id, name },
                ..
            } => {
                self.raw_inputs.entry(id.clone()).or_default();
                self.tool_names.insert(id.clone(), name.clone());
                None
            }
            TurnEvent::Provider {
                event: StreamEvent::ToolInputDelta { id, delta },
                ..
            } => {
                let visible = self.visible_tools.contains(id);
                let raw_input = {
                    let raw_input = self.raw_inputs.entry(id.clone()).or_default();
                    raw_input.push_str(delta);
                    json_or_string(raw_input)
                };
                visible.then(|| {
                    let name = self.tool_names.get(id).map(String::as_str);
                    let command = name
                        .and_then(|name| shell_command(name, Some(&raw_input)))
                        .map(str::to_owned);
                    let mut update = json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "rawInput": raw_input,
                    });
                    if let Some(command) = command {
                        update["title"] = Value::String(command);
                    }
                    if let Some(name) = name {
                        let presentation_input = update.get("rawInput").cloned();
                        decorate_tool_call(&mut update, name, presentation_input.as_ref());
                    }
                    update
                })
            }
            TurnEvent::ToolCallStarted {
                call_id,
                display_name,
                name,
                ..
            } => {
                self.visible_tools.insert(call_id.clone());
                self.tool_names.insert(call_id.clone(), name.clone());
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
                self.tool_names.insert(call_id.clone(), name.clone());
                let mut update = tool_call(
                    call_id,
                    display_name,
                    name,
                    "in_progress",
                    self.raw_inputs
                        .get(call_id)
                        .map(|value| json_or_string(value)),
                );
                update["sessionUpdate"] = json!("tool_call_update");
                Some(update)
            }
            TurnEvent::ToolDispatchBlocked { call_id, kind, .. } => {
                self.raw_inputs.remove(call_id);
                self.tool_names.remove(call_id);
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
            TurnEvent::ToolDispatchInterrupted {
                call_id,
                display_name,
                name,
                title,
                output,
                interruption,
                ..
            } => {
                let raw_input = self
                    .raw_inputs
                    .remove(call_id)
                    .map(|value| json_or_string(&value));
                self.tool_names.remove(call_id);
                self.visible_tools.remove(call_id);
                Some(interrupted_tool_update(
                    CompletedToolUpdate {
                        call_id,
                        display_name,
                        name,
                        title,
                        raw_input: raw_input.as_ref(),
                        output,
                        diff: None,
                        written_paths: &[],
                        is_error: true,
                        metadata: None,
                    },
                    *interruption,
                ))
            }
            TurnEvent::ToolDispatchCompleted {
                call_id,
                display_name,
                name,
                title,
                output,
                diff,
                written_paths,
                is_error,
                ..
            } => {
                let raw_input = self
                    .raw_inputs
                    .remove(call_id)
                    .map(|value| json_or_string(&value));
                self.tool_names.remove(call_id);
                self.visible_tools.remove(call_id);
                Some(completed_tool_update(CompletedToolUpdate {
                    call_id,
                    display_name,
                    name,
                    title,
                    raw_input: raw_input.as_ref(),
                    output,
                    diff: diff.as_ref(),
                    written_paths,
                    is_error: *is_error,
                    metadata: None,
                }))
            }
            TurnEvent::TurnInterrupted { request, .. } => Some(interruption_update(
                request.map(|request| request.source),
                request.map(|request| request.reason),
            )),
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
                self.tool_names.remove(tool_use_id);
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
                event: StreamEvent::StatusDetail { .. },
                ..
            } => None,
            TurnEvent::Provider {
                event: StreamEvent::Error { .. },
                ..
            } => None,
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
    display_name: &str,
    name: &str,
    status: &str,
    raw_input: Option<Value>,
) -> Value {
    let presentation_input = raw_input.clone();
    let title = shell_command(name, raw_input.as_ref()).unwrap_or(display_name);
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
    add_shell_interpreter(&mut update, name, display_name);
    decorate_tool_call(&mut update, name, presentation_input.as_ref());
    update
}

pub(crate) struct CompletedToolUpdate<'a> {
    pub call_id: &'a str,
    pub display_name: &'a str,
    pub name: &'a str,
    pub title: &'a str,
    pub raw_input: Option<&'a Value>,
    pub output: &'a str,
    pub diff: Option<&'a ToolDiff>,
    pub written_paths: &'a [String],
    pub is_error: bool,
    pub metadata: Option<&'a serde_json::Map<String, Value>>,
}

#[must_use]
pub(crate) fn completed_tool_update(input: CompletedToolUpdate<'_>) -> Value {
    let CompletedToolUpdate {
        call_id,
        display_name,
        name,
        title,
        raw_input,
        output,
        diff,
        written_paths,
        is_error,
        metadata,
    } = input;
    let title = shell_command(name, raw_input).unwrap_or({
        if title.is_empty() {
            display_name
        } else {
            title
        }
    });
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
        "title": title,
        "status": if is_error { "failed" } else { "completed" },
        "rawOutput": json_or_string(output),
        "content": content,
    });
    if !locations.is_empty() {
        update["locations"] = Value::Array(locations);
    }
    add_shell_interpreter(&mut update, name, display_name);
    decorate_completed_tool_update(&mut update, name, raw_input, metadata, output, is_error);
    update
}

pub(crate) fn interrupted_tool_update(
    input: CompletedToolUpdate<'_>,
    interruption: ToolInterruption,
) -> Value {
    let mut metadata = input.metadata.cloned().unwrap_or_default();
    let presentation_state = if interruption.uncertain() {
        "uncertain"
    } else {
        "cancelled"
    };
    match input.name {
        "task" => {
            let subagent = metadata
                .entry("subagent".to_owned())
                .or_insert_with(|| json!({}));
            if !subagent.is_object() {
                *subagent = json!({});
            }
            subagent["state"] = json!(presentation_state);
        }
        "question" => {
            metadata.insert("questionStatus".to_owned(), json!(presentation_state));
        }
        _ => {}
    }
    let mut update = completed_tool_update(CompletedToolUpdate {
        metadata: Some(&metadata),
        ..input
    });
    let zuno = update
        .as_object_mut()
        .expect("tool update is an object")
        .entry("_meta")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("tool update metadata is an object")
        .entry("zuno")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("zuno tool metadata is an object");
    zuno.insert("outcome".to_owned(), json!("cancelled"));
    zuno.insert("cancelled".to_owned(), json!(true));
    zuno.insert("interruptionMode".to_owned(), json!(interruption.as_str()));
    zuno.insert("forced".to_owned(), json!(interruption.uncertain()));
    zuno.insert("uncertain".to_owned(), json!(interruption.uncertain()));
    update
}

fn shell_command<'a>(name: &str, raw_input: Option<&'a Value>) -> Option<&'a str> {
    if name != "shell" {
        return None;
    }
    raw_input?
        .as_object()?
        .get("command")?
        .as_str()
        .filter(|command| !command.is_empty())
}

fn add_shell_interpreter(update: &mut Value, name: &str, display_name: &str) {
    if name == "shell" {
        update["_meta"] = json!({
            "zuno": {
                "interpreter": display_name,
            },
        });
    }
}

fn content_update(kind: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": { "type": "text", "text": text },
    })
}

fn interruption_update(
    source: Option<zuno_engine::interrupt::HardInterruptSource>,
    reason: Option<zuno_engine::interrupt::HardInterruptReason>,
) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": INTERRUPTED_TURN_NOTICE },
        "_meta": {
            "zuno": {
                "kind": "turn_interrupted",
                "source": source,
                "reason": reason,
            },
        },
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
