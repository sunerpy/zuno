use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use serde_json::{Value, json};
use uuid::Uuid;
use zuno_engine::interrupt::InterruptSignal;

use crate::{Agent, ClientConnection, RequestId, RpcError};

const AUTH_METHOD: &str = "zuno-login";

#[derive(Debug, Clone)]
struct Session {
    cwd: String,
    title: String,
    mode: String,
    model: String,
    interrupt: Option<InterruptSignal>,
}

#[derive(Debug, Default)]
pub struct ConformanceAgent {
    sessions: Mutex<HashMap<String, Session>>,
}

impl ConformanceAgent {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn require_session(&self, params: &Value) -> Result<(String, Session), RpcError> {
        let session_id = required_string(params, "sessionId")?;
        let session = lock(&self.sessions)
            .get(&session_id)
            .cloned()
            .ok_or_else(|| RpcError::invalid_params(format!("unknown session {session_id}")))?;
        Ok((session_id, session))
    }

    fn lifecycle_response(session: &Session) -> Value {
        json!({
            "configOptions": config_options(session),
            "modes": mode_state(&session.mode),
            "models": model_state(&session.model),
        })
    }

    async fn prompt(&self, params: Value, client: ClientConnection) -> Result<Value, RpcError> {
        let (session_id, _) = self.require_session(&params)?;
        let prompt = params
            .get("prompt")
            .and_then(Value::as_array)
            .ok_or_else(|| RpcError::invalid_params("prompt must be an array"))?;
        let text = prompt
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<String>();
        let message_id = params.get("messageId").cloned();
        let interrupt = InterruptSignal::new();
        {
            let mut sessions = lock(&self.sessions);
            let session = sessions
                .get_mut(&session_id)
                .ok_or_else(|| RpcError::invalid_params("session closed"))?;
            if session.interrupt.is_some() {
                return Err(RpcError::invalid_request(
                    "session already has an active prompt",
                ));
            }
            session.interrupt = Some(interrupt.clone());
        }

        let result = if text.contains("wait for cancellation") {
            client
                .session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "agent_thought_chunk",
                        "content": { "type": "text", "text": "waiting" },
                        "_meta": { "phase": "cancel-ready" },
                    }),
                )
                .await?;
            interrupt.notified().await;
            client
                .session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "cancelled" },
                        "_meta": { "phase": "cancel-final" },
                    }),
                )
                .await?;
            prompt_response("cancelled", message_id)
        } else {
            client
                .session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "user_message_chunk",
                        "content": { "type": "text", "text": text },
                        "messageId": message_id,
                    }),
                )
                .await?;
            if text.contains("permission") {
                permission_turn(&client, &session_id).await?;
            }
            client
                .session_update(
                    &session_id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "done" },
                    }),
                )
                .await?;
            prompt_response("end_turn", message_id)
        };

        let mut sessions = lock(&self.sessions);
        if let Some(session) = sessions.get_mut(&session_id)
            && session
                .interrupt
                .as_ref()
                .is_some_and(|current| current.same_instance(&interrupt))
        {
            session.interrupt = None;
        }
        Ok(result)
    }
}

#[async_trait]
impl Agent for ConformanceAgent {
    async fn request(
        &self,
        method: &str,
        _request: &RequestId,
        params: Value,
        client: ClientConnection,
    ) -> Result<Value, RpcError> {
        match method {
            "initialize" => initialize(&params),
            "authenticate" => authenticate(&params),
            "session/new" => {
                let cwd = required_string(&params, "cwd")?;
                require_array(&params, "mcpServers")?;
                let session_id = format!("ses_{}", Uuid::new_v4().simple());
                let session = Session {
                    cwd,
                    title: "New ACP session".to_owned(),
                    mode: "build".to_owned(),
                    model: "test/model".to_owned(),
                    interrupt: None,
                };
                let mut response = Self::lifecycle_response(&session);
                response["sessionId"] = Value::String(session_id.clone());
                lock(&self.sessions).insert(session_id, session);
                Ok(response)
            }
            "session/load" | "session/resume" => {
                required_string(&params, "cwd")?;
                require_array(&params, "mcpServers")?;
                let (_, session) = self.require_session(&params)?;
                Ok(Self::lifecycle_response(&session))
            }
            "session/list" => {
                let cwd = optional_string(&params, "cwd")?;
                let sessions = lock(&self.sessions);
                let entries = sessions
                    .iter()
                    .filter(|(_, session)| cwd.as_ref().is_none_or(|cwd| cwd == &session.cwd))
                    .map(|(id, session)| {
                        json!({
                            "sessionId": id,
                            "cwd": session.cwd,
                            "title": session.title,
                            "updatedAt": "2026-08-06T00:00:00Z",
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json!({ "sessions": entries }))
            }
            "session/close" => {
                let session_id = required_string(&params, "sessionId")?;
                if let Some(session) = lock(&self.sessions).remove(&session_id)
                    && let Some(interrupt) = session.interrupt
                {
                    interrupt.fire();
                }
                Ok(json!({}))
            }
            "session/fork" => {
                required_string(&params, "cwd")?;
                let (_, mut session) = self.require_session(&params)?;
                session.interrupt = None;
                let session_id = format!("ses_{}", Uuid::new_v4().simple());
                let mut response = Self::lifecycle_response(&session);
                response["sessionId"] = Value::String(session_id.clone());
                lock(&self.sessions).insert(session_id, session);
                Ok(response)
            }
            "session/set_config_option" => {
                let (session_id, _) = self.require_session(&params)?;
                let config_id = required_string(&params, "configId")?;
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RpcError::invalid_params("value must be a string"))?
                    .to_owned();
                let mut sessions = lock(&self.sessions);
                let session = sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| RpcError::invalid_params("session closed"))?;
                match config_id.as_str() {
                    "mode" => session.mode = value,
                    "model" => session.model = value,
                    _ => return Err(RpcError::invalid_params("unknown config option")),
                }
                Ok(json!({ "configOptions": config_options(session) }))
            }
            "session/set_mode" => {
                let (session_id, _) = self.require_session(&params)?;
                let mode = required_string(&params, "modeId")?;
                let mut sessions = lock(&self.sessions);
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.mode = mode;
                }
                Ok(json!({}))
            }
            "session/set_model" => {
                let (session_id, _) = self.require_session(&params)?;
                let model = required_string(&params, "modelId")?;
                let mut sessions = lock(&self.sessions);
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.model = model;
                }
                Ok(json!({}))
            }
            "session/prompt" => self.prompt(params, client).await,
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(
        &self,
        method: &str,
        params: Value,
        _client: ClientConnection,
    ) -> Result<(), RpcError> {
        if method != "session/cancel" {
            return Err(RpcError::method_not_found(method));
        }
        let (session_id, _) = self.require_session(&params)?;
        if let Some(interrupt) = lock(&self.sessions)
            .get(&session_id)
            .and_then(|session| session.interrupt.clone())
        {
            interrupt.fire();
        }
        Ok(())
    }
}

fn initialize(params: &Value) -> Result<Value, RpcError> {
    if params
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err(RpcError::invalid_params("protocolVersion must be a number"));
    }
    Ok(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": true,
            "mcpCapabilities": { "stdio": true, "http": true, "sse": false },
            "promptCapabilities": { "embeddedContext": true, "image": true },
            "sessionCapabilities": {
                "close": {}, "fork": {}, "list": {}, "resume": {},
            },
        },
        "authMethods": [{
            "id": AUTH_METHOD,
            "name": "Login with Zuno",
            "description": "Run `zuno auth login` in the terminal",
        }],
        "agentInfo": { "name": "Zuno", "version": env!("CARGO_PKG_VERSION") },
    }))
}

fn authenticate(params: &Value) -> Result<Value, RpcError> {
    let method = required_string(params, "methodId")?;
    if method != AUTH_METHOD {
        return Err(RpcError::invalid_params("unknown authentication method"));
    }
    Ok(json!({}))
}

async fn permission_turn(client: &ClientConnection, session_id: &str) -> Result<(), RpcError> {
    client
        .session_update(
            session_id,
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_permission",
                "title": "Write a file",
                "kind": "edit",
                "status": "pending",
                "rawInput": { "path": "fixture.txt" },
            }),
        )
        .await?;
    let response = client
        .request_permission(json!({
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": "call_permission",
                "title": "Write a file",
                "kind": "edit",
                "status": "pending",
                "rawInput": { "path": "fixture.txt" },
            },
            "options": [
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" },
            ],
        }))
        .await?;
    if response.pointer("/outcome/outcome").and_then(Value::as_str) != Some("selected")
        || response
            .pointer("/outcome/optionId")
            .and_then(Value::as_str)
            != Some("allow_once")
    {
        return Err(RpcError::invalid_request("tool permission was denied"));
    }
    client
        .session_update(
            session_id,
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_permission",
                "status": "completed",
                "rawOutput": "fixture written",
                "content": [{
                    "type": "content",
                    "content": { "type": "text", "text": "fixture written" },
                }],
            }),
        )
        .await
}

fn prompt_response(stop_reason: &str, message_id: Option<Value>) -> Value {
    let mut response = json!({ "stopReason": stop_reason, "_meta": {} });
    if let Some(message_id) = message_id {
        response["userMessageId"] = message_id;
    }
    response
}

fn config_options(session: &Session) -> Value {
    json!([
        {
            "id": "mode",
            "name": "Mode",
            "category": "mode",
            "type": "select",
            "currentValue": session.mode,
            "options": [
                { "value": "build", "name": "Build" },
                { "value": "plan", "name": "Plan" },
            ],
        },
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": session.model,
            "options": [{ "value": "test/model", "name": "Test model" }],
        },
    ])
}

fn mode_state(current: &str) -> Value {
    json!({
        "currentModeId": current,
        "availableModes": [
            { "id": "build", "name": "Build" },
            { "id": "plan", "name": "Plan" },
        ],
    })
}

fn model_state(current: &str) -> Value {
    json!({
        "currentModelId": current,
        "availableModels": [{ "modelId": "test/model", "name": "Test model" }],
    })
}

fn required_string(params: &Value, field: &str) -> Result<String, RpcError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| RpcError::invalid_params(format!("{field} must be a non-empty string")))
}

fn optional_string(params: &Value, field: &str) -> Result<Option<String>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(RpcError::invalid_params(format!(
            "{field} must be a string"
        ))),
    }
}

fn require_array<'a>(params: &'a Value, field: &str) -> Result<&'a Vec<Value>, RpcError> {
    params
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params(format!("{field} must be an array")))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_presents_zunos_identity() {
        let response = initialize(&json!({ "protocolVersion": 1 })).expect("initialize response");
        assert_eq!(response["agentInfo"]["name"], "Zuno");
        assert_eq!(response["authMethods"][0]["id"], "zuno-login");
        assert_eq!(response["authMethods"][0]["name"], "Login with Zuno");
        assert_eq!(
            response["authMethods"][0]["description"],
            "Run `zuno auth login` in the terminal"
        );
    }
}
