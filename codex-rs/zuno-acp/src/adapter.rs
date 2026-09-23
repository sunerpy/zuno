use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use chrono::Utc;
use codex_app_server_client::AppServerClient;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_client::InProcessClientStartArgs;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RequestId as AppRequestId;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use tokio::sync::oneshot;

use crate::transport::Agent;
use crate::transport::ClientConnection;
use crate::transport::RequestId;
use crate::transport::RpcError;
use crate::transport::ServeError;
use crate::transport::serve_stdio;

mod mapping;
mod projection;
mod session;

use mapping::*;

use projection::handle_app_server_event;
#[cfg(test)]
use projection::history_updates;
#[cfg(test)]
use projection::notification_updates;

const DEFAULT_LIST_LIMIT: u64 = 100;
const ACP_PROTOCOL_VERSION: u64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum AcpBridgeError {
    #[error(transparent)]
    Transport(#[from] ServeError),
    #[error("Codex App Server event stream closed")]
    AppServerClosed,
    #[error("Codex App Server event could not be encoded: {0}")]
    EventEncoding(#[from] serde_json::Error),
    #[error("Codex App Server request failed: {0}")]
    AppServerIo(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct CodexAcpAgent {
    requests: AppServerRequestHandle,
    state: Arc<BridgeState>,
}

#[derive(Default)]
struct BridgeState {
    sessions: Mutex<HashMap<String, SessionRoute>>,
    turns: Mutex<TurnRegistry>,
}

#[derive(Clone)]
struct SessionRoute {
    client: ClientConnection,
    cwd: String,
    model: String,
    model_provider: String,
    effort: Option<String>,
    mode: String,
    active_turn_id: Option<String>,
}

#[derive(Default)]
struct TurnRegistry {
    waiters: HashMap<String, oneshot::Sender<TurnOutcome>>,
    completed: HashMap<String, TurnOutcome>,
}

#[derive(Debug, Clone)]
struct TurnOutcome {
    status: String,
    error: Option<String>,
}

impl Agent for CodexAcpAgent {
    async fn request(
        &self,
        method: &str,
        _request: &RequestId,
        params: Value,
        client: ClientConnection,
    ) -> Result<Value, RpcError> {
        match method {
            "initialize" => initialize(&params),
            "authenticate" => Ok(json!({})),
            "session/new" => self.new_session(&params, &client).await,
            "session/load" | "session/resume" => self.resume_session(&params, &client).await,
            "session/fork" => self.fork_session(&params, &client).await,
            "session/list" => self.list_sessions(&params).await,
            "session/prompt" => self.prompt(&params, &client).await,
            "session/steer" => self.steer_session(&params, &client).await,
            "session/set_config_option" => self.set_option(&params).await,
            "session/set_mode" => self.set_mode(&params).await,
            "session/set_model" => self.set_model(&params).await,
            "session/close" => {
                let session_id = required_string(&params, "sessionId")?;
                self.cancel_session(&params).await?;
                lock(&self.state.sessions).remove(&session_id);
                Ok(json!({}))
            }
            "session/delete" => {
                let session_id = required_string(&params, "sessionId")?;
                self.cancel_session(&params).await?;
                let _response = self
                    .app_request("thread/archive", json!({ "threadId": session_id }))
                    .await?;
                lock(&self.state.sessions).remove(&session_id);
                Ok(json!({}))
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(
        &self,
        method: &str,
        params: Value,
        _client: ClientConnection,
    ) -> Result<(), RpcError> {
        match method {
            "session/cancel" => self.cancel_session(&params).await,
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn request_cancelled(&self, method: &str, _request: &RequestId, params: &Value) {
        if method == "session/prompt"
            && let Err(error) = self.cancel_session(params).await
        {
            tracing::warn!(%error, "failed to interrupt withdrawn ACP prompt");
        }
    }
}

/// Start an embedded Codex App Server and expose it through ACP v1 on stdio.
///
/// Startup policy (configuration layers, state database, environment manager,
/// executable paths, and permission defaults) is supplied by the Zuno CLI so
/// every frontend uses the same host policy.
pub async fn serve_in_process_stdio(args: InProcessClientStartArgs) -> Result<(), AcpBridgeError> {
    let app_server = InProcessAppServerClient::start(args).await?;
    serve_app_server_stdio(AppServerClient::InProcess(app_server)).await
}

/// Run ACP v1 over stdio while routing all execution through an existing Codex App Server.
pub async fn serve_app_server_stdio(mut app_server: AppServerClient) -> Result<(), AcpBridgeError> {
    let agent = CodexAcpAgent::new(app_server.request_handle());
    let state = Arc::clone(&agent.state);
    let mut transport = Box::pin(serve_stdio(agent));
    loop {
        tokio::select! {
            result = &mut transport => {
                app_server.shutdown().await?;
                return result.map_err(Into::into);
            }
            event = app_server.next_event() => {
                let Some(event) = event else {
                    return Err(AcpBridgeError::AppServerClosed);
                };
                handle_app_server_event(&mut app_server, &state, event).await?;
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_maps_text_images_and_resources_without_starting_a_second_loop() {
        let prompt = json!([
            {"type":"text","text":"inspect"},
            {"type":"image","mimeType":"image/png","data":"aGVsbG8="},
            {"type":"image","mimeType":"image/png","data":"ignored","uri":"https://example.invalid/a.png"},
            {"type":"resource_link","name":"spec","uri":"file:///tmp/spec.md"},
            {"type":"resource_link","uri":"file:///tmp/notes/plan.md"},
            {"type":"resource","resource":{"uri":"file:///tmp/ctx.txt","mimeType":"text/plain","text":"context"}},
            {"type":"resource","resource":{"uri":"file:///tmp/pixel.png","mimeType":"image/png","blob":"iVBORw0KGgo="}},
            {"type":"resource","resource":{"uri":"file:///tmp/archive.bin","mimeType":"application/octet-stream","blob":"AAEC"}},
        ]);
        let mapped = acp_prompt_to_codex(Some(&prompt)).expect("prompt maps");
        assert_eq!(mapped.len(), 8);
        assert_eq!(mapped[0]["type"], "text");
        assert_eq!(mapped[1]["url"], "data:image/png;base64,aGVsbG8=");
        // A fetchable image URI is passed through instead of re-encoding the data.
        assert_eq!(mapped[2]["url"], "https://example.invalid/a.png");
        // Links keep the same shape the official codex-acp adapter produces so
        // prompts behave identically across Codex ACP agents.
        assert_eq!(mapped[3]["text"], "[@spec](file:///tmp/spec.md)");
        assert_eq!(mapped[4]["text"], "[@plan.md](file:///tmp/notes/plan.md)");
        assert_eq!(
            mapped[5]["text"],
            "[@ctx.txt](file:///tmp/ctx.txt)\n<context ref=\"file:///tmp/ctx.txt\">\ncontext\n</context>"
        );
        assert_eq!(mapped[6]["type"], "image");
        assert_eq!(mapped[6]["url"], "data:image/png;base64,iVBORw0KGgo=");
        // A non-image blob is never presented to the model as an image.
        assert_eq!(mapped[7]["type"], "text");
        assert_eq!(
            mapped[7]["text"],
            "[@archive.bin](file:///tmp/archive.bin)\n<context ref=\"file:///tmp/archive.bin\" mimeType=\"application/octet-stream\" encoding=\"base64\">\nAAEC\n</context>"
        );
    }

    #[test]
    fn prompt_rejects_blocks_the_agent_never_advertised() {
        let error = acp_prompt_to_codex(Some(&json!([
            {"type":"audio","mimeType":"audio/wav","data":"AAEC"}
        ])))
        .expect_err("audio is not advertised");
        assert_eq!(error.code, -32602);
        let error = acp_prompt_to_codex(Some(&json!([
            {"type":"resource","resource":{"uri":"file:///x","mimeType":"application/pdf"}}
        ])))
        .expect_err("resource needs text or blob");
        assert_eq!(error.code, -32602);
    }

    #[test]
    fn bridge_error_codes_do_not_alias_acp_or_app_server_codes() {
        use crate::transport::SESSION_BUSY_CODE;
        use crate::transport::STEER_REJECTED_CODE;
        // ACP v1: -32000 auth required, -32002 resource not found; App Server:
        // -32001 overloaded. Both sides' codes are forwarded verbatim.
        for reserved in [
            -32000, -32001, -32002, -32600, -32601, -32602, -32603, -32800,
        ] {
            assert_ne!(SESSION_BUSY_CODE, reserved);
            assert_ne!(STEER_REJECTED_CODE, reserved);
        }
        assert_ne!(SESSION_BUSY_CODE, STEER_REJECTED_CODE);
    }

    #[test]
    fn set_mode_translates_to_the_mode_config_option() {
        let translated = set_mode_as_config_option(&json!({"sessionId":"s","modeId":"plan"}))
            .expect("translates");
        assert_eq!(translated["sessionId"], "s");
        assert_eq!(translated["configId"], "mode");
        assert_eq!(translated["value"], "plan");
        let error =
            set_mode_as_config_option(&json!({"sessionId":"s"})).expect_err("modeId required");
        assert_eq!(error.code, -32602);
    }

    #[test]
    fn mcp_servers_become_thread_scoped_codex_overrides() {
        let servers = json!([
            {"name":"local-tools","command":"node","args":["server.js"],"env":{"A":"B"}},
            {"name":"remote","url":"https://example.invalid/mcp","headers":{"X-Test":"1"}},
        ]);
        let config = mcp_server_config(Some(&servers)).expect("MCP maps");
        assert_eq!(config["mcp_servers.local-tools.command"], "node");
        assert_eq!(config["mcp_servers.local-tools.args"], json!(["server.js"]));
        assert_eq!(
            config["mcp_servers.remote.url"],
            "https://example.invalid/mcp"
        );
        assert_eq!(config["mcp_servers.remote.http_headers"]["X-Test"], "1");
    }

    #[test]
    fn notifications_project_chat_reasoning_plan_and_tools() {
        assert_eq!(
            notification_updates("item/agentMessage/delta", &json!({"delta":"answer"}),)[0]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(
            notification_updates(
                "item/reasoning/summaryTextDelta",
                &json!({"delta":"thinking"}),
            )[0]["sessionUpdate"],
            "agent_thought_chunk"
        );
        assert_eq!(
            notification_updates(
                "turn/plan/updated",
                &json!({"plan":[{"step":"build","status":"inProgress"}]}),
            )[0]["entries"][0]["status"],
            "in_progress"
        );
        let started = notification_updates(
            "item/started",
            &json!({"item":{"type":"commandExecution","id":"call-1","command":"cargo test"}}),
        );
        assert_eq!(started[0]["toolCallId"], "call-1");
        assert_eq!(started[0]["kind"], "execute");
    }

    #[test]
    fn unsupported_mcp_names_cannot_inject_config_paths() {
        let error = mcp_server_config(Some(&json!([
            {"name":"safe.command","command":"evil"}
        ])))
        .expect_err("dot must be rejected");
        assert_eq!(error.code, -32602);
    }

    #[test]
    fn resume_history_replays_user_agent_reasoning_and_tool_items() {
        let response = json!({
            "thread": {"turns": [{"items": [
                {"type":"userMessage","id":"u","content":[{"type":"text","text":"hello"}]},
                {"type":"reasoning","id":"r","summary":["reason"],"content":[]},
                {"type":"commandExecution","id":"c","command":"pwd","status":"completed"},
                {"type":"agentMessage","id":"a","text":"done"}
            ]}]}
        });
        let updates = history_updates(&response);
        assert_eq!(updates[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(updates[1]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(updates[2]["sessionUpdate"], "tool_call");
        assert_eq!(updates[3]["sessionUpdate"], "tool_call_update");
        assert_eq!(updates[4]["sessionUpdate"], "agent_message_chunk");
    }

    #[test]
    fn initialize_is_stable_acp_v1_and_identifies_zuno() {
        let response = initialize(&json!({"protocolVersion": 1})).expect("initialize");
        assert_eq!(response["protocolVersion"], 1);
        assert_eq!(response["agentInfo"]["name"], "Zuno");
        assert_eq!(response["agentCapabilities"]["loadSession"], true);
        let response = initialize(&json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": false}, "terminal": true},
            "clientInfo": {"name": "zed", "version": "1.0"},
        }))
        .expect("initialize with capabilities");
        assert_eq!(response["protocolVersion"], 1);
    }

    #[test]
    fn initialize_rejects_malformed_client_capabilities() {
        let error = initialize(&json!({"protocolVersion": 1, "clientCapabilities": "yes"}))
            .expect_err("capabilities must be an object");
        assert_eq!(error.code, -32602);
        let error = initialize(&json!({"protocolVersion": 1, "clientInfo": []}))
            .expect_err("clientInfo must be an object");
        assert_eq!(error.code, -32602);
    }
}
