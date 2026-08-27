use async_trait::async_trait;
use serde_json::{Value, json};
use zuno_error::ToolError;
use zuno_tool::{PermissionAsk, PermissionAsker, PermissionOrigin};

use crate::ClientConnection;

#[derive(Debug, Clone)]
pub struct AcpPermissionAsker {
    client: ClientConnection,
    title: String,
}

impl AcpPermissionAsker {
    #[must_use]
    pub fn new(client: ClientConnection, title: impl Into<String>) -> Self {
        Self {
            client,
            title: title.into(),
        }
    }
}

#[async_trait]
impl PermissionAsker for AcpPermissionAsker {
    async fn ask(
        &self,
        origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        let allow_always = !ask.always.is_empty();
        let response = self
            .client
            .request_permission(json!({
                "sessionId": origin.session_id(),
                "toolCall": {
                    "toolCallId": origin.call_id(),
                    "title": self.title,
                    "kind": tool_kind(tool),
                    "status": "pending",
                    "rawInput": {
                        "permission": ask.permission,
                        "patterns": ask.patterns,
                        "metadata": ask.metadata,
                    },
                },
                "options": permission_options(allow_always),
            }))
            .await
            .map_err(|_| ToolError::Denied {
                tool: tool.to_owned(),
            })?;
        match permission_resolution(&response, allow_always) {
            PermissionResolution::Allowed => Ok(()),
            PermissionResolution::Denied | PermissionResolution::Cancelled => {
                Err(ToolError::Denied {
                    tool: tool.to_owned(),
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionResolution {
    Allowed,
    Denied,
    Cancelled,
}

fn permission_resolution(response: &Value, allow_always: bool) -> PermissionResolution {
    match (
        response.pointer("/outcome/outcome").and_then(Value::as_str),
        response
            .pointer("/outcome/optionId")
            .and_then(Value::as_str),
    ) {
        (Some("cancelled"), _) => PermissionResolution::Cancelled,
        (Some("selected"), Some("allow_once")) => PermissionResolution::Allowed,
        (Some("selected"), Some("allow_always")) if allow_always => PermissionResolution::Allowed,
        _ => PermissionResolution::Denied,
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
    if always {
        options.push(json!({
            "optionId": "reject_always",
            "name": "Always reject",
            "kind": "reject_always",
        }));
    }
    options
}

fn tool_kind(tool: &str) -> &'static str {
    match tool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn option_ids(always: bool) -> Vec<Value> {
        permission_options(always)
            .into_iter()
            .map(|option| option["optionId"].clone())
            .collect()
    }

    #[test]
    fn standing_permission_options_are_symmetric() {
        assert_eq!(
            option_ids(false),
            vec![json!("allow_once"), json!("reject_once")],
        );
        assert_eq!(
            option_ids(true),
            vec![
                json!("allow_once"),
                json!("allow_always"),
                json!("reject_once"),
                json!("reject_always"),
            ],
        );
    }

    #[test]
    fn permission_responses_fail_closed_and_recognize_cancellation() {
        let selected =
            |option: &str| json!({ "outcome": { "outcome": "selected", "optionId": option } });
        assert_eq!(
            permission_resolution(&selected("allow_once"), false),
            PermissionResolution::Allowed
        );
        assert_eq!(
            permission_resolution(&selected("allow_always"), true),
            PermissionResolution::Allowed
        );
        assert_eq!(
            permission_resolution(&selected("allow_always"), false),
            PermissionResolution::Denied,
            "a client cannot select an option that was not offered"
        );
        for option in ["reject_once", "reject_always", "unknown"] {
            assert_eq!(
                permission_resolution(&selected(option), true),
                PermissionResolution::Denied
            );
        }
        assert_eq!(
            permission_resolution(&json!({ "outcome": { "outcome": "cancelled" } }), true),
            PermissionResolution::Cancelled
        );
        for malformed in [
            json!({}),
            json!({ "outcome": null }),
            json!({ "outcome": { "outcome": "selected" } }),
        ] {
            assert_eq!(
                permission_resolution(&malformed, true),
                PermissionResolution::Denied
            );
        }
    }
}
