use async_trait::async_trait;
use oc_error::ToolError;
use oc_tool::{PermissionAsk, PermissionAsker};
use serde_json::{Value, json};

use crate::ClientConnection;

#[derive(Debug, Clone)]
pub struct AcpPermissionAsker {
    client: ClientConnection,
    session_id: String,
    call_id: String,
    title: String,
}

impl AcpPermissionAsker {
    #[must_use]
    pub fn new(
        client: ClientConnection,
        session_id: impl Into<String>,
        call_id: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        Self {
            client,
            session_id: session_id.into(),
            call_id: call_id.into(),
            title: title.into(),
        }
    }
}

#[async_trait]
impl PermissionAsker for AcpPermissionAsker {
    async fn ask(&self, tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        let response = self
            .client
            .request_permission(json!({
                "sessionId": self.session_id,
                "toolCall": {
                    "toolCallId": self.call_id,
                    "title": self.title,
                    "kind": tool_kind(tool),
                    "status": "pending",
                    "rawInput": {
                        "permission": ask.permission,
                        "patterns": ask.patterns,
                        "metadata": ask.metadata,
                    },
                },
                "options": permission_options(!ask.always.is_empty()),
            }))
            .await
            .map_err(|_| ToolError::Denied {
                tool: tool.to_owned(),
            })?;
        let selected =
            response.pointer("/outcome/outcome").and_then(Value::as_str) == Some("selected");
        let option = response
            .pointer("/outcome/optionId")
            .and_then(Value::as_str);
        if selected && matches!(option, Some("allow_once" | "allow_always")) {
            return Ok(());
        }
        Err(ToolError::Denied {
            tool: tool.to_owned(),
        })
    }
}

fn permission_options(always: bool) -> Vec<Value> {
    let mut options = vec![json!({
        "optionId": "allow_once",
        "name": "Allow once",
        "kind": "allow_once",
    })];
    if always {
        options.push(json!({
            "optionId": "allow_always",
            "name": "Always allow",
            "kind": "allow_always",
        }));
    }
    options.push(json!({
        "optionId": "reject_once",
        "name": "Reject",
        "kind": "reject_once",
    }));
    options
}

fn tool_kind(tool: &str) -> &'static str {
    match tool {
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
