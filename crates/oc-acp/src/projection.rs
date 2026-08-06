use oc_engine::r#loop::TurnEvent;
use oc_llm::event::StreamEvent;
use serde_json::{Value, json};

#[must_use]
pub fn turn_event_update(event: &TurnEvent) -> Option<Value> {
    match event {
        TurnEvent::Provider {
            event: StreamEvent::TextDelta(text),
            ..
        } => Some(content_update("agent_message_chunk", text)),
        TurnEvent::Provider {
            event: StreamEvent::ReasoningDelta(text),
            ..
        } => Some(content_update("agent_thought_chunk", text)),
        TurnEvent::Provider {
            event: StreamEvent::ToolUseStart { id, name },
            ..
        } => Some(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": name,
            "kind": tool_kind(name),
            "status": "pending",
        })),
        TurnEvent::ToolDispatchStarted { call_id, name, .. } => Some(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": call_id,
            "title": name,
            "kind": tool_kind(name),
            "status": "in_progress",
        })),
        TurnEvent::ToolDispatchCompleted {
            call_id,
            title,
            output,
            is_error,
            ..
        } => Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id,
            "title": title,
            "status": if *is_error { "failed" } else { "completed" },
            "rawOutput": output,
            "content": [{
                "type": "content",
                "content": { "type": "text", "text": output },
            }],
        })),
        TurnEvent::Provider {
            event:
                StreamEvent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                },
            ..
        } => Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": tool_use_id,
            "status": if *is_error { "failed" } else { "completed" },
            "rawOutput": content,
            "content": [{
                "type": "content",
                "content": { "type": "text", "text": content },
            }],
        })),
        TurnEvent::Provider {
            event: StreamEvent::StatusDetail { detail },
            ..
        }
        | TurnEvent::Provider {
            event: StreamEvent::Error {
                message: detail, ..
            },
            ..
        } => Some(content_update("agent_thought_chunk", detail)),
        _ => None,
    }
}

fn content_update(kind: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": { "type": "text", "text": text },
    })
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "glob" => "read",
        "write" | "edit" | "apply_patch" => "edit",
        "delete" => "delete",
        "move" => "move",
        "grep" | "search" => "search",
        "bash" | "execute" => "execute",
        "fetch" | "webfetch" => "fetch",
        _ => "other",
    }
}
