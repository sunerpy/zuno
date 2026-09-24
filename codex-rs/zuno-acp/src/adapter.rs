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
use tokio::task::JoinSet;

use crate::transport::Agent;
use crate::transport::ClientConnection;
use crate::transport::RequestId;
use crate::transport::RpcError;
use crate::transport::ServeError;
use crate::transport::serve_stdio;

mod commands;
mod mapping;
mod projection;
mod session;

use commands::*;
use mapping::*;

#[cfg(test)]
use projection::answers_from_elicitation;
#[cfg(test)]
use projection::elicitation_form_request;
use projection::handle_app_server_event;
#[cfg(test)]
use projection::history_updates;
#[cfg(test)]
use projection::notification_updates;
use projection::settle_server_request;

const DEFAULT_LIST_LIMIT: u64 = 100;
/// `model/list` page size and the page cap that bounds a runaway cursor.
const MODEL_LIST_PAGE_SIZE: u64 = 200;
const MODEL_LIST_MAX_PAGES: usize = 10;
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
    #[error("bridging a Codex App Server request panicked: {0}")]
    BridgeTask(#[from] tokio::task::JoinError),
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
    /// `clientCapabilities` from `initialize`; decides which client-side
    /// methods (for example `elicitation/create`) the bridge may call.
    client_capabilities: Mutex<Value>,
    /// The App Server model catalog (`model/list`), refreshed on every session
    /// lifecycle call so `availableModels` and the `model` / `reasoning_effort`
    /// config options offer what the TUI's `/model` picker offers.
    models: Mutex<Vec<ModelEntry>>,
    /// `collaborationMode/list` presets: the effort (and model) a mode switches
    /// to, so ACP applies the same preset the TUI applies when entering plan mode.
    collaboration_presets: Mutex<Option<Vec<CollaborationPreset>>>,
}

/// A server collaboration-mode preset: what switching to `mode` changes.
#[derive(Clone, Debug, PartialEq)]
struct CollaborationPreset {
    mode: String,
    model: Option<String>,
    /// `Some(None)` clears the effort, `Some(Some(e))` sets it, `None` keeps it.
    effort: Option<Option<String>>,
}

/// One entry of the App Server model catalog as the bridge needs it.
#[derive(Clone, Debug, PartialEq)]
struct ModelEntry {
    id: String,
    name: String,
    description: String,
    /// `(effort id, description)` in catalog order.
    efforts: Vec<(String, String)>,
    hidden: bool,
}

impl BridgeState {
    fn client_supports_form_elicitation(&self) -> bool {
        // The schema advertises a mode as an (possibly empty) object; null or a
        // missing key means unsupported.
        lock(&self.client_capabilities)
            .pointer("/elicitation/form")
            .is_some_and(Value::is_object)
    }
}

#[derive(Clone)]
struct SessionRoute {
    client: ClientConnection,
    cwd: String,
    model: String,
    model_provider: String,
    effort: Option<String>,
    /// `default` or `plan` (Codex collaboration mode).
    collaboration_mode: String,
    /// Thread permission settings, matched against the ACP mode presets.
    approval_policy: String,
    approvals_reviewer: String,
    sandbox_type: String,
    active_turn_id: Option<String>,
}

#[derive(Default)]
struct TurnRegistry {
    waiters: HashMap<String, oneshot::Sender<TurnOutcome>>,
    completed: HashMap<String, TurnOutcome>,
    /// Waiters for "the next turn of this thread settles", used by commands
    /// such as `/compact` whose turn id the App Server does not return.
    thread_waiters: HashMap<String, oneshot::Sender<TurnOutcome>>,
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
            "initialize" => {
                let response = initialize(&params)?;
                *lock(&self.state.client_capabilities) = params
                    .get("clientCapabilities")
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(response)
            }
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
///
/// Three things are polled together: the ACP transport (which reads client
/// frames, including the answers to requests the bridge sent), the App Server
/// event stream, and the bridged App Server requests that are waiting for such
/// an answer. Those requests run as tasks so that waiting for the client never
/// stops the transport from being read.
pub async fn serve_app_server_stdio(mut app_server: AppServerClient) -> Result<(), AcpBridgeError> {
    let agent = CodexAcpAgent::new(app_server.request_handle());
    let state = Arc::clone(&agent.state);
    let mut transport = Box::pin(serve_stdio(agent));
    let mut bridged = JoinSet::new();
    loop {
        tokio::select! {
            result = &mut transport => {
                bridged.shutdown().await;
                app_server.shutdown().await?;
                return result.map_err(Into::into);
            }
            event = app_server.next_event() => {
                let Some(event) = event else {
                    return Err(AcpBridgeError::AppServerClosed);
                };
                handle_app_server_event(&state, event, &mut bridged).await?;
            }
            Some(settled) = bridged.join_next(), if !bridged.is_empty() => {
                settle_server_request(&app_server, settled?).await?;
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
            {"type":"image","mimeType":"image/png","data":"aGk=","uri":"https://example.invalid/a.png"},
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
        // An optional `uri` never replaces the bytes: App Server rejects remote
        // image URLs, so the client's data is always inlined.
        assert_eq!(mapped[2]["url"], "data:image/png;base64,aGk=");
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

    fn test_route(
        model: &str,
        effort: Option<&str>,
        approval: &str,
        reviewer: &str,
        sandbox: &str,
    ) -> SessionRoute {
        route_from_lifecycle(
            &json!({
                "thread": { "id": "t1" },
                "model": model,
                "modelProvider": "kiro-local",
                "cwd": "/work",
                "reasoningEffort": effort,
                "approvalPolicy": approval,
                "approvalsReviewer": reviewer,
                "sandbox": { "type": sandbox },
            }),
            "/fallback",
            &ClientConnection::detached_for_tests(),
        )
        .expect("route")
    }

    fn sample_catalog() -> Vec<ModelEntry> {
        catalog_from_model_list(&json!({
            "data": [
                {"id": "gpt-5.6-sol", "displayName": "GPT 5.6 Sol", "description": "Balanced", "hidden": false,
                 "supportedReasoningEfforts": [
                    {"reasoningEffort": "medium", "description": "Default"},
                    {"reasoningEffort": "ultra", "description": "Deepest"}]},
                {"id": "gpt-6-astra", "displayName": "GPT 6 Astra", "description": "Frontier", "hidden": false,
                 "supportedReasoningEfforts": [{"reasoningEffort": "xhigh", "description": ""}]},
                {"id": "gpt-5.6-sol-hidden", "displayName": "Hidden", "description": "", "hidden": true,
                 "supportedReasoningEfforts": []},
            ],
            "nextCursor": null,
        }))
    }

    #[test]
    fn session_advertises_the_model_catalog_and_the_models_efforts() {
        let catalog = sample_catalog();
        assert_eq!(catalog.len(), 3);
        let route = test_route(
            "gpt-5.6-sol",
            Some("ultra"),
            "on-request",
            "user",
            "workspaceWrite",
        );
        let response = lifecycle_response("t1", &route, &catalog);
        let models = response["models"]["availableModels"]
            .as_array()
            .expect("models");
        assert_eq!(
            models
                .iter()
                .map(|m| m["modelId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["gpt-5.6-sol", "gpt-6-astra"],
            "hidden models are not offered"
        );
        assert_eq!(models[0]["name"], "GPT 5.6 Sol");
        assert_eq!(response["models"]["currentModelId"], "gpt-5.6-sol");
        let options = config_options(&route, &catalog);
        let model_option = options
            .iter()
            .find(|o| o["id"] == "model")
            .expect("model option");
        assert_eq!(model_option["options"].as_array().unwrap().len(), 2);
        let effort_option = options
            .iter()
            .find(|o| o["id"] == "reasoning_effort")
            .expect("effort");
        assert_eq!(
            effort_option["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["value"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["medium", "ultra"],
            "efforts come from the catalog entry of the current model"
        );
        assert_eq!(effort_option["currentValue"], "ultra");

        // A model the catalog does not know (config-only) stays selectable and
        // gets the generic effort list; a current effort outside the catalog's
        // list is kept as an option so the client can display it.
        let custom = test_route(
            "my-gateway/custom",
            Some("max"),
            "on-request",
            "user",
            "workspaceWrite",
        );
        let response = lifecycle_response("t1", &custom, &catalog);
        assert_eq!(
            response["models"]["availableModels"][0]["modelId"],
            "my-gateway/custom"
        );
        let options = config_options(&custom, &catalog);
        let efforts = options
            .iter()
            .find(|o| o["id"] == "reasoning_effort")
            .unwrap();
        assert!(
            efforts["options"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["value"] == "max")
        );
        // An empty catalog (model/list failed) still yields the current model.
        let response = lifecycle_response("t1", &route, &[]);
        assert_eq!(
            response["models"]["availableModels"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn session_modes_are_permission_presets_and_collaboration_is_a_config_option() {
        let route = test_route("gpt-5.6-sol", None, "on-request", "user", "workspaceWrite");
        let modes = session_modes(&route);
        assert_eq!(modes["currentModeId"], "workspace-write");
        let ids: Vec<&str> = modes["availableModes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec![
                "read-only",
                "workspace-write",
                "agent",
                "strict",
                "agent-full-access"
            ]
        );
        assert_eq!(
            permission_mode_id(&test_route("m", None, "never", "user", "dangerFullAccess")),
            "agent-full-access"
        );
        assert_eq!(
            permission_mode_id(&test_route(
                "m",
                None,
                "on-request",
                "auto_review",
                "workspaceWrite"
            )),
            "agent"
        );
        assert_eq!(
            permission_mode_id(&test_route(
                "m",
                None,
                "untrusted",
                "user",
                "workspaceWrite"
            )),
            "strict"
        );
        // Settings that match no preset show up as a custom entry the client can
        // display but not select again.
        let granular = route_from_lifecycle(
            &json!({"thread": {"id": "t"}, "model": "m", "approvalPolicy": {"granular": {}}, "sandbox": {"type": "externalSandbox"}}),
            "/w",
            &ClientConnection::detached_for_tests(),
        )
        .unwrap();
        let modes = session_modes(&granular);
        assert_eq!(modes["currentModeId"], "custom");
        assert_eq!(modes["availableModes"][0]["id"], "custom");
        assert!(permission_preset("custom").is_none());
        let options = config_options(&route, &[]);
        let collaboration = options
            .iter()
            .find(|o| o["id"] == "collaboration_mode")
            .expect("collaboration option");
        assert_eq!(collaboration["currentValue"], "default");
        assert_eq!(
            collaboration["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["value"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["default", "plan"]
        );
        assert_eq!(sandbox_policy_json("readOnly")["type"], "readOnly");
        assert_eq!(
            sandbox_policy_json("dangerFullAccess"),
            json!({"type": "dangerFullAccess"})
        );
        assert_eq!(
            sandbox_policy_json("workspaceWrite")["writableRoots"],
            json!([])
        );
    }

    #[test]
    fn slash_commands_are_parsed_and_advertised() {
        assert_eq!(
            parse_slash_command(Some(
                &json!([{"type": "text", "text": "  /Review-Branch  main  "}])
            )),
            Some(("review-branch".to_owned(), "main".to_owned()))
        );
        assert_eq!(
            parse_slash_command(Some(&json!([{"type": "text", "text": "/status"}]))),
            Some(("status".to_owned(), String::new()))
        );
        assert_eq!(
            parse_slash_command(Some(&json!([{"type": "text", "text": "$deploy prod"}]))),
            None
        );
        assert_eq!(
            parse_slash_command(Some(&json!([{"type": "text", "text": "/"}]))),
            None
        );
        assert_eq!(
            parse_slash_command(Some(
                &json!([{"type": "image", "data": "x"}, {"type": "text", "text": "/plan"}])
            )),
            None
        );
        assert_eq!(parse_slash_command(None), None);

        let update = available_commands_update(&[
            ("deploy".to_owned(), "Ship it".to_owned()),
            ("plan".to_owned(), "a skill named like a command".to_owned()),
        ]);
        assert_eq!(update["sessionUpdate"], "available_commands_update");
        let names: Vec<&str> = update["availableCommands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        for builtin in [
            "plan",
            "compact",
            "review",
            "review-branch",
            "review-commit",
            "status",
            "skills",
            "mcp",
            "goal",
            "rename",
            "logout",
        ] {
            assert!(names.contains(&builtin), "{builtin} missing from {names:?}");
        }
        assert!(names.contains(&"$deploy"));
        assert!(
            names.contains(&"$plan"),
            "skills are namespaced with $ so they never shadow a command"
        );
        let review = update["availableCommands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "review")
            .unwrap();
        assert_eq!(review["input"]["hint"], "optional review instructions");
        let skills = skills_from_list(&json!({"data": [{"cwd": "/w", "skills": [
            {"name": "deploy", "description": "Long", "shortDescription": "Ship it", "enabled": true},
            {"name": "off", "description": "disabled", "enabled": false}
        ], "errors": []}]}));
        assert_eq!(skills, vec![("deploy".to_owned(), "Ship it".to_owned())]);
    }

    #[test]
    fn collaboration_presets_carry_the_servers_effort_for_plan_mode() {
        let presets = collaboration_presets_from_list(&json!({"data": [
            {"name": "Plan", "mode": "plan", "model": null, "reasoningEffort": "medium"},
            {"name": "Default", "mode": "default", "model": null, "reasoningEffort": null},
        ]}));
        assert_eq!(presets.len(), 2);
        assert_eq!(
            presets[0].effort,
            Some(Some("medium".to_owned())),
            "plan switches to medium like the TUI preset"
        );
        assert_eq!(
            presets[1].effort,
            Some(None),
            "default clears the preset effort"
        );
        assert_eq!(presets[0].model, None);
    }

    #[test]
    fn set_mode_translates_to_the_mode_config_option() {
        let translated =
            set_mode_as_config_option(&json!({"sessionId":"s","modeId":"agent-full-access"}))
                .expect("translates");
        assert_eq!(translated["sessionId"], "s");
        assert_eq!(translated["configId"], "mode");
        assert_eq!(translated["value"], "agent-full-access");
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
        assert_eq!(started[0]["name"], "shell");
        let spawned = notification_updates(
            "item/started",
            &json!({"item":{"type":"collabAgentToolCall","id":"call-2","tool":"spawnAgent"}}),
        );
        assert_eq!(spawned[0]["name"], "spawn_agent");
        assert_eq!(spawned[0]["kind"], "think");
        // Other collab tools keep their own names so clients do not mistake a
        // wait or close for a newly spawned sub-agent.
        for (wire, expected) in [
            ("wait", "wait"),
            ("closeAgent", "close_agent"),
            ("sendInput", "send_input"),
        ] {
            let update = notification_updates(
                "item/started",
                &json!({"item":{"type":"collabAgentToolCall","id":"call-x","tool":wire}}),
            );
            assert_eq!(update[0]["name"], expected, "{wire}");
        }
        let activity = notification_updates(
            "item/started",
            &json!({"item":{"type":"subAgentActivity","id":"act-1","kind":"started","agentThreadId":"t","agentPath":"a"}}),
        );
        assert_eq!(activity[0]["name"], "sub_agent_activity");
        let mcp = notification_updates(
            "item/started",
            &json!({"item":{"type":"mcpToolCall","id":"call-3","server":"docs","tool":"search"}}),
        );
        assert_eq!(mcp[0]["title"], "docs.search");
        assert_eq!(mcp[0]["name"], "search");
    }

    #[test]
    fn lifecycle_items_are_not_projected_as_tool_calls() {
        for item_type in [
            "contextCompaction",
            "enteredReviewMode",
            "exitedReviewMode",
            "functionCallOutput",
            "plan",
        ] {
            let updates =
                notification_updates("item/started", &json!({"item":{"type":item_type,"id":"x"}}));
            assert!(
                updates
                    .iter()
                    .all(|update| update["sessionUpdate"] != "tool_call"),
                "{item_type} must not become a tool call: {updates:?}"
            );
            let completed = notification_updates(
                "item/completed",
                &json!({"item":{"type":item_type,"id":"x"}}),
            );
            assert!(
                completed
                    .iter()
                    .all(|update| update["sessionUpdate"] != "tool_call_update"),
                "{item_type} must not complete a tool call: {completed:?}"
            );
        }
    }

    #[test]
    fn tool_questions_become_a_form_elicitation_that_keeps_other_answers() {
        // Production `request_user_input` questions always carry options and
        // `isOther: true` (core normalizes them that way); the legacy shapes
        // without `isOther` or without options are still accepted.
        let params = json!({
            "itemId": "call-9",
            "questions": [
                {"id":"strategy","header":"Strategy","question":"How should I proceed?","isOther":true,
                 "options":[{"label":"Careful (Recommended)","description":"slow"},{"label":"Fast","description":"quick"}]},
                {"id":"mode","header":"Mode","question":"Which mode?","isOther":false,
                 "options":[{"label":"build","description":""},{"label":"plan","description":""}]},
                {"id":"notes","header":"Notes","question":"Anything else?","options":null},
            ]
        });
        let request = elicitation_form_request("sess-1", &params).expect("form request");
        assert_eq!(request["sessionId"], "sess-1");
        assert_eq!(request["toolCallId"], "call-9");
        assert_eq!(request["mode"], "form");
        let strategy = &request["requestedSchema"]["properties"]["strategy"];
        assert_eq!(strategy["type"], "string");
        assert!(
            strategy.get("enum").is_none(),
            "an open question must not be a closed enum"
        );
        assert_eq!(
            strategy["description"],
            "How should I proceed?\nOptions: Careful (Recommended) (slow); Fast (quick). Or type another answer."
        );
        assert_eq!(
            request["requestedSchema"]["properties"]["mode"]["enum"],
            json!(["build", "plan"])
        );
        assert_eq!(
            request["requestedSchema"]["properties"]["notes"]["type"],
            "string"
        );
        assert_eq!(
            request["requestedSchema"]["required"],
            json!(["strategy", "mode", "notes"])
        );
        let answers = answers_from_elicitation(
            &params,
            &json!({"action":"accept","content":{"strategy":"Try both in a worktree","mode":"plan","notes":"ship it"}}),
        )
        .expect("answers");
        assert_eq!(
            answers["answers"]["strategy"]["answers"],
            json!(["Try both in a worktree"])
        );
        assert_eq!(answers["answers"]["mode"]["answers"], json!(["plan"]));
        assert_eq!(answers["answers"]["notes"]["answers"], json!(["ship it"]));
        let declined =
            answers_from_elicitation(&params, &json!({"action":"decline"})).expect_err("declined");
        assert_eq!(declined.code, -32800);
        let secret = elicitation_form_request(
            "sess-1",
            &json!({"questions":[{"id":"token","header":"Token","question":"API token?","isSecret":true}]}),
        )
        .expect_err("secrets never go through form mode");
        assert_eq!(secret.code, -32600);
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
