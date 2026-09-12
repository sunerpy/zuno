//! Shared semantic projection from durable public facts. The allowlist below is
//! intentional: copying a provider/message metadata map would expose replay
//! capsules, signatures, execution grants or private prompt material.

use serde_json::{Map, Value};
use zuno_types::activity::*;
use zuno_types::identity::InvocationId;

use crate::ApplicationError;
use async_trait::async_trait;
use zuno_types::identity::SessionId;

#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    pub limit: crate::PageSize,
    pub before: Option<Counter>,
    /// Fixed snapshot boundary returned by the first page.
    pub through: Option<Counter>,
}
#[derive(Debug, Clone, Default)]
pub struct FrameQuery {
    pub limit: crate::PageSize,
    pub after: Counter,
}

/// Principal-bound client reads, with authorization refreshed for every page.
#[async_trait]
pub trait ActivityPersistence: Send + Sync {
    async fn history(
        &self,
        session: &SessionId,
        query: HistoryQuery,
    ) -> Result<HistoryPage, ApplicationError>;
    async fn frames(
        &self,
        session: &SessionId,
        query: FrameQuery,
    ) -> Result<FramePage, ApplicationError>;
}

pub fn message_id(id: &str) -> String {
    format!("message:{id}")
}
pub fn part_id(id: &str) -> String {
    format!("part:{id}")
}

pub fn message(
    id: &str,
    at: i64,
    data: &Map<String, Value>,
    usage: Option<NormalizedUsage>,
) -> Result<ItemRecord, ApplicationError> {
    let role = role(data)?;
    let state = if role == MessageRole::User
        && data.get("activityState").and_then(Value::as_str) == Some("cancelled")
    {
        MessageState::Interrupted
    } else if role == MessageRole::User
        && data.get("activityState").and_then(Value::as_str) == Some("failed")
    {
        MessageState::Failed
    } else if role == MessageRole::User
        && matches!(
            data.get("activityState").and_then(Value::as_str),
            Some("queued" | "steering" | "promoted")
        )
    {
        MessageState::Pending
    } else if data.get("error").is_some_and(|value| !value.is_null()) {
        if data
            .get("error")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            == Some("AbortError")
        {
            MessageState::Interrupted
        } else {
            MessageState::Failed
        }
    } else if role == MessageRole::User
        || data
            .get("time")
            .and_then(|time| time.get("completed"))
            .and_then(Value::as_i64)
            .is_some()
    {
        MessageState::Complete
    } else {
        MessageState::Pending
    };
    Ok(ItemRecord {
        id: message_id(id),
        parent_id: data.get("parentID").and_then(Value::as_str).map(message_id),
        created_at: timestamp(at)?,
        item: SessionItem::Message {
            role,
            origin: origin(role, data),
            state,
            content: data
                .get("activityText")
                .and_then(Value::as_str)
                .map(text_block)
                .into_iter()
                .collect(),
            usage,
        },
        actions: Vec::new(),
    })
}

pub fn role(data: &Map<String, Value>) -> Result<MessageRole, ApplicationError> {
    match data.get("role").and_then(Value::as_str) {
        Some("user") => Ok(MessageRole::User),
        Some("assistant") => Ok(MessageRole::Assistant),
        _ => Err(invalid("invalid message role")),
    }
}

fn origin(role: MessageRole, data: &Map<String, Value>) -> MessageOrigin {
    if role == MessageRole::Assistant {
        MessageOrigin::Model
    } else {
        match data.get("activityOrigin").and_then(Value::as_str) {
            Some("completion" | "delegation") => MessageOrigin::AgentReport,
            Some("runtime") => MessageOrigin::Runtime,
            _ => MessageOrigin::UserInput,
        }
    }
}

pub fn part(
    id: &str,
    parent: &str,
    at: i64,
    role: MessageRole,
    data: &Map<String, Value>,
) -> Result<Option<ItemRecord>, ApplicationError> {
    let item = match data.get("type").and_then(Value::as_str) {
        Some("text") => {
            if role == MessageRole::User && data.contains_key("activityText") {
                return Ok(None);
            }
            let text = data.get("text").and_then(Value::as_str).unwrap_or_default();
            if text.is_empty() {
                return Ok(None);
            }
            SessionItem::Message {
                role,
                origin: origin(role, data),
                state: MessageState::Complete,
                content: vec![text_block(text)],
                usage: None,
            }
        }
        Some("reasoning") => {
            // The server stores a provider-approved summary in `text`; encrypted
            // bodies and replay signatures live in metadata and are never copied.
            let text = data.get("text").and_then(Value::as_str).unwrap_or_default();
            if text.is_empty() {
                return Ok(None);
            }
            let (text, truncated) = bounded_text(text);
            SessionItem::Thinking {
                text,
                collapsed: true,
                truncated,
            }
        }
        Some("tool") => SessionItem::Invocation {
            invocation: Box::new(invocation(data)?),
        },
        Some("compaction") => SessionItem::Compaction {
            automatic: data.get("auto").and_then(Value::as_bool).unwrap_or(false),
        },
        // These source kinds need their own authorized artifact/job adapters.
        // A host path or arbitrary metadata object is not a public resource.
        Some(
            "file" | "snapshot" | "patch" | "agent" | "subtask" | "step-start" | "step-finish"
            | "retry",
        ) => return Ok(None),
        _ => return Err(invalid("unknown durable part kind")),
    };
    Ok(Some(ItemRecord {
        id: part_id(id),
        parent_id: Some(message_id(parent)),
        created_at: timestamp(at)?,
        item,
        actions: Vec::new(),
    }))
}

fn invocation(data: &Map<String, Value>) -> Result<Invocation, ApplicationError> {
    let id = data
        .get("callID")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing invocation ID"))?;
    let name = data
        .get("displayName")
        .and_then(Value::as_str)
        .or_else(|| data.get("tool").and_then(Value::as_str))
        .ok_or_else(|| invalid("missing tool name"))?;
    let raw = data
        .get("state")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("missing invocation state"))?;
    let mut presentation = data
        .get("presentation")
        .map(|value| serde_json::from_value::<InvocationPresentation>(value.clone()))
        .transpose()
        .map_err(ApplicationError::storage)?
        .unwrap_or_default();
    if presentation == InvocationPresentation::default()
        && data.get("uiIntent").and_then(Value::as_str) == Some("subagent")
    {
        presentation.action = InvocationAction::Agent;
    }
    let state = match raw.get("outcome").and_then(Value::as_str) {
        Some("uncertain") => InvocationState::Uncertain,
        Some("blocked") => InvocationState::Denied,
        _ => match raw.get("status").and_then(Value::as_str) {
            Some("pending") if raw.contains_key("dispatchedAtMs") => InvocationState::Running,
            Some("pending") => InvocationState::Queued,
            Some("running") => InvocationState::Running,
            Some("completed") => InvocationState::Succeeded,
            Some("error") => InvocationState::Failed,
            Some("cancelled") => InvocationState::Cancelled,
            _ => return Err(invalid("unknown durable invocation state")),
        },
    };
    let output = raw
        .get("output")
        .or_else(|| raw.get("error"))
        .and_then(Value::as_str);
    let content = output
        .map(|output| {
            if presentation.action == InvocationAction::Process {
                let (text, truncated) = bounded_text(output);
                ContentBlock::Terminal {
                    text,
                    channel: TerminalChannel::Combined,
                    truncated,
                }
            } else {
                text_block(output)
            }
        })
        .into_iter()
        .collect();
    let input = raw.get("input").cloned().unwrap_or(Value::Null);
    let input = bounded_json(input, 64 * 1024)?;
    Ok(Invocation {
        id: InvocationId::new(id).map_err(ApplicationError::storage)?,
        name: display_name(name),
        presentation,
        state,
        input,
        content,
        waiting_for: None,
        location: ExecutionLocation::Unknown,
        isolation: Isolation::Unknown,
        denial: (state == InvocationState::Denied).then(|| {
            match raw.get("blockKind").and_then(Value::as_str) {
                Some("denied") => DenialReason::Permission,
                Some("unavailable") => DenialReason::Unavailable,
                _ => DenialReason::Unknown,
            }
        }),
    })
}

fn display_name(name: &str) -> ActivityName {
    // Historical display labels were not wire identifiers. Preserve readable
    // legacy names without making a new client label invalidate old user state.
    let mut label: String = name
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    label = label.trim().to_owned();
    if label.len() > 256 {
        let mut end = 253;
        while !label.is_char_boundary(end) {
            end -= 1;
        }
        label.truncate(end);
        label.push_str("...");
    }
    ActivityName::new(label)
        .unwrap_or_else(|_| ActivityName::new("Unknown tool").expect("static name"))
}

pub fn text_block(text: &str) -> ContentBlock {
    let (text, truncated) = bounded_text(text);
    ContentBlock::Text { text, truncated }
}
pub fn bounded_text(text: &str) -> (String, bool) {
    let mut limit = text.len().min(MAX_ACTIVITY_TEXT_BYTES);
    while !text.is_char_boundary(limit) {
        limit -= 1;
    }
    (text[..limit].to_owned(), limit != text.len())
}
pub fn bounded_json(value: Value, maximum: usize) -> Result<Value, ApplicationError> {
    let bytes = serde_json::to_vec(&value)
        .map_err(ApplicationError::storage)?
        .len();
    Ok(if bytes > maximum {
        serde_json::json!({"previewOmitted":true,"bytes":bytes.to_string()})
    } else {
        value
    })
}
fn timestamp(at: i64) -> Result<Counter, ApplicationError> {
    Ok(Counter(
        u64::try_from(at).map_err(|_| invalid("negative public timestamp"))?,
    ))
}
fn invalid(message: impl Into<String>) -> ApplicationError {
    ApplicationError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn private_replay_and_tool_metadata_never_enter_the_client_projection() {
        let message = message(
            "msg",
            1,
            json!({
                "role":"assistant","time":{"created":1,"completed":2},
                "providerOptions":{"secret":"private-key"},"prompt":"private-system-prompt"
            })
            .as_object()
            .unwrap(),
            None,
        )
        .unwrap();
        let reasoning = part("reasoning", "msg", 1, MessageRole::Assistant, json!({
            "type":"reasoning","text":"A visible summary",
            "metadata":{"signature":"private-signature","providerReasoning":{"encryptedContent":"private-ciphertext"}}
        }).as_object().unwrap()).unwrap().unwrap();
        let tool = part("tool", "msg", 1, MessageRole::Assistant, json!({
            "type":"tool","callID":"call","tool":"custom-agent","uiIntent":"subagent",
            "metadata":{"thoughtSignature":"private-signature"},
            "state":{"status":"completed","input":{"question":"public"},"output":"public result",
                "metadata":{"lease":"private-lease","environment":{"hostPath":"/private/runtime"}}}
        }).as_object().unwrap()).unwrap().unwrap();
        let rendered = serde_json::to_string(&[message, reasoning, tool]).unwrap();
        assert!(rendered.contains("A visible summary"));
        assert!(rendered.contains("public result"));
        for secret in [
            "private-key",
            "private-system-prompt",
            "private-signature",
            "private-ciphertext",
            "private-lease",
            "/private/runtime",
        ] {
            assert!(!rendered.contains(secret), "leaked {secret}");
        }
        assert!(
            rendered.contains("\"source\":{\"kind\":\"unknown\"}"),
            "a name or UI hint must not forge a built-in/MCP source"
        );
    }

    #[test]
    fn unknown_and_uncertain_tools_remain_explicit_and_large_content_is_bounded() {
        let value = json!({"type":"tool","callID":"call","tool":"plugin",
            "state":{"status":"error","outcome":"uncertain","input":{},"error":"Response lost"}});
        let record = part(
            "p",
            "m",
            1,
            MessageRole::Assistant,
            value.as_object().unwrap(),
        )
        .unwrap()
        .unwrap();
        let SessionItem::Invocation { invocation } = record.item else {
            panic!("invocation")
        };
        assert_eq!(invocation.state, InvocationState::Uncertain);
        let text = "中".repeat(MAX_ACTIVITY_TEXT_BYTES);
        let (bounded, truncated) = bounded_text(&text);
        assert!(truncated);
        assert!(bounded.len() <= MAX_ACTIVITY_TEXT_BYTES);
        assert!(
            bounded_json(json!({"large":text}), 64).unwrap()["previewOmitted"]
                .as_bool()
                .unwrap()
        );
    }
}
