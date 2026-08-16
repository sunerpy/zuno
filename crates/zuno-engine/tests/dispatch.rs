use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;
use zuno_engine::dispatch::ToolRegistryDispatcher;
use zuno_engine::hooks::{PermissionHookDecision, ToolHooks};
use zuno_engine::interrupt::InterruptSignal;
use zuno_engine::r#loop::{DispatchRequest, ToolCall, ToolDispatcher};
use zuno_error::ToolError;
use zuno_llm::cache::McpToolStatus;
use zuno_permission::{PermissionAction, Rule};
use zuno_tool::{PermissionAsk, PermissionAsker, Tool, ToolContext, ToolOutput};

#[derive(Default)]
struct RecordingApprover {
    asks: Mutex<Vec<PermissionAsk>>,
}

impl RecordingApprover {
    fn asks(&self) -> Vec<PermissionAsk> {
        self.asks.lock().expect("ask lock").clone()
    }
}

#[async_trait]
impl PermissionAsker for RecordingApprover {
    async fn ask(&self, _tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        self.asks.lock().expect("ask lock").push(ask);
        Ok(())
    }
}

struct BlockingApprover {
    events: Arc<Mutex<Vec<String>>>,
    entered: Notify,
    release: Notify,
}

impl BlockingApprover {
    fn new(events: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            events,
            entered: Notify::new(),
            release: Notify::new(),
        }
    }
}

#[async_trait]
impl PermissionAsker for BlockingApprover {
    async fn ask(&self, _tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        self.events
            .lock()
            .expect("event lock")
            .push(format!("permission:{}", ask.patterns.join("|")));
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

struct RecordingTool {
    id: &'static str,
    calls: Arc<AtomicUsize>,
    events: Option<Arc<Mutex<Vec<String>>>>,
}

impl RecordingTool {
    fn new(id: &'static str, calls: Arc<AtomicUsize>) -> Self {
        Self {
            id,
            calls,
            events: None,
        }
    }

    fn with_events(
        id: &'static str,
        calls: Arc<AtomicUsize>,
        events: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            id,
            calls,
            events: Some(events),
        }
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn id(&self) -> &str {
        self.id
    }

    fn description(&self) -> &str {
        "Record one deterministic call."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(events) = &self.events {
            events
                .lock()
                .expect("event lock")
                .push("execute".to_owned());
        }
        Ok(ToolOutput::text(self.id, args["command"].to_string()))
    }
}

struct DetachedTool {
    started: Arc<Notify>,
    release: Arc<Notify>,
    completed: Arc<Notify>,
}

#[async_trait]
impl Tool for DetachedTool {
    fn id(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Wait until the test releases this tool."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.started.notify_one();
        self.release.notified().await;
        self.completed.notify_one();
        Ok(ToolOutput::text("bash", "finished"))
    }
}

fn allow_all_rule() -> Rule {
    Rule {
        permission: "*".to_owned(),
        pattern: "*".to_owned(),
        action: PermissionAction::Allow,
    }
}

fn deny_rule(permission: &str, pattern: &str) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: pattern.to_owned(),
        action: PermissionAction::Deny,
    }
}

fn request(
    dispatcher: &ToolRegistryDispatcher,
    id: &str,
    name: &str,
    input: Value,
) -> DispatchRequest {
    DispatchRequest {
        call: ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            raw_input: input.to_string(),
            input,
            input_error: None,
            thought_signature: None,
        },
        session_id: "ses_dispatch".to_owned(),
        message_id: "msg_dispatch".to_owned(),
        agent: "build".to_owned(),
        available_tools: dispatcher.available_tools().definitions.into(),
        interrupt: InterruptSignal::new(),
    }
}

fn dispatcher(
    tools: Vec<Arc<dyn Tool>>,
    rules: Vec<Rule>,
    approver: Arc<dyn PermissionAsker>,
    background: InterruptSignal,
) -> ToolRegistryDispatcher {
    ToolRegistryDispatcher::new(tools, rules, approver, background, McpToolStatus::Ready)
}

#[derive(Default)]
struct MutatingHooks;

#[async_trait]
impl ToolHooks for MutatingHooks {
    async fn before(
        &self,
        _tool: &str,
        _session_id: &str,
        _call_id: &str,
        args: &mut Value,
    ) -> Result<(), String> {
        args["command"] = json!("hooked");
        Ok(())
    }

    async fn permission(
        &self,
        _request: &zuno_permission::PermissionRequest,
    ) -> Result<PermissionHookDecision, String> {
        Ok(PermissionHookDecision::Allow)
    }

    async fn after(
        &self,
        _tool: &str,
        _session_id: &str,
        _call_id: &str,
        _args: &Value,
        output: &mut ToolOutput,
    ) -> Result<(), String> {
        output.title = "hooked title".to_owned();
        Ok(())
    }
}

#[tokio::test]
async fn production_dispatch_applies_before_permission_and_after_hooks() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![deny_rule("bash", "original")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        InterruptSignal::new(),
        McpToolStatus::Ready,
    )
    .with_hooks(Arc::new(MutatingHooks));

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-hooked",
            "bash",
            json!({"command": "original", "intent": "verify plugin hooks"}),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    assert_eq!(result.output.output, "\"hooked\"");
    assert_eq!(result.output.title, "hooked title");
    assert!(
        approver.asks().is_empty(),
        "plugin allow must bypass approval"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dispatch_aliases_bash_and_functions_bash_to_the_registered_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
        InterruptSignal::new(),
    );

    for (index, alias) in ["Bash", "functions.bash"].into_iter().enumerate() {
        let result = dispatcher
            .dispatch(request(
                &dispatcher,
                &format!("call_{index}"),
                alias,
                json!({ "command": "pwd", "intent": "inspect" }),
            ))
            .await;
        assert!(!result.is_error, "{alias}: {}", result.output.output);
    }

    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn dispatch_unknown_tool_returns_ranked_suggestion_and_available_list() {
    let dispatcher = dispatcher(
        vec![
            Arc::new(RecordingTool::new("bash", Arc::new(AtomicUsize::new(0)))),
            Arc::new(RecordingTool::new(
                "tool_search",
                Arc::new(AtomicUsize::new(0)),
            )),
        ],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
        InterruptSignal::new(),
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_unknown",
            "ToolSerch",
            json!({ "command": "unused", "intent": "recover" }),
        ))
        .await;

    assert!(result.is_error);
    assert!(
        result.output.output.contains("Did you mean: tool_search?"),
        "{}",
        result.output.output
    );
    assert!(
        result
            .output
            .output
            .contains("Available tools: bash, tool_search."),
        "{}",
        result.output.output
    );
}

#[tokio::test]
async fn dispatch_malformed_json_synthesizes_error_without_running_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
        InterruptSignal::new(),
    );
    let mut malformed = request(
        &dispatcher,
        "call_bad_json",
        "bash",
        Value::String("{\"command\":".to_owned()),
    );
    malformed.call.raw_input = "{\"command\":".to_owned();
    malformed.call.input_error = Some("EOF while parsing a value".to_owned());

    let result = dispatcher.dispatch(malformed).await;

    assert!(result.is_error);
    assert!(
        result
            .output
            .output
            .contains("Malformed arguments for tool `bash`")
    );
    assert!(result.output.output.contains("EOF while parsing a value"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    eprintln!(
        "FAILURE_QA malformed_input={:?} synthesized_result={:?} is_error={}",
        "{\"command\":", result.output.output, result.is_error
    );
}

#[tokio::test]
async fn dispatch_schema_error_is_a_result_and_does_not_run_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
        InterruptSignal::new(),
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_bad_args",
            "bash",
            json!({ "intent": "missing command" }),
        ))
        .await;

    assert!(result.is_error);
    assert!(
        result
            .output
            .output
            .contains("Invalid arguments for tool `bash`")
    );
    assert!(result.output.output.contains("command"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dispatch_denial_is_an_error_result_and_a_later_call_still_runs() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![allow_all_rule(), deny_rule("bash", "rm -rf /")],
        Arc::new(RecordingApprover::default()),
        InterruptSignal::new(),
    );

    let denied = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_denied",
            "bash",
            json!({ "command": "rm -rf /", "intent": "unsafe" }),
        ))
        .await;
    let continued = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_continued",
            "bash",
            json!({ "command": "git status", "intent": "inspect" }),
        ))
        .await;

    assert!(denied.is_error);
    assert!(denied.output.output.contains("denied"));
    assert!(!continued.is_error, "{}", continued.output.output);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dispatch_waits_for_argument_derived_permission_before_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let approver = Arc::new(BlockingApprover::new(Arc::clone(&events)));
    let dispatcher = Arc::new(dispatcher(
        vec![Arc::new(RecordingTool::with_events(
            "bash",
            Arc::clone(&calls),
            Arc::clone(&events),
        ))],
        Vec::new(),
        approver.clone(),
        InterruptSignal::new(),
    ));
    let task = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move {
            dispatcher
                .dispatch(request(
                    &dispatcher,
                    "call_permission",
                    "bash",
                    json!({ "command": "git push origin main", "intent": "publish" }),
                ))
                .await
        })
    };

    approver.entered.notified().await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    approver.release.notify_waiters();
    let result = task.await.expect("dispatch task");

    assert!(!result.is_error, "{}", result.output.output);
    assert_eq!(
        events.lock().expect("event lock").as_slice(),
        ["permission:git push origin main", "execute"]
    );
}

#[tokio::test]
async fn dispatch_passes_argument_pattern_to_permission_approver() {
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new(
            "bash",
            Arc::new(AtomicUsize::new(0)),
        ))],
        Vec::new(),
        approver.clone(),
        InterruptSignal::new(),
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_pattern",
            "bash",
            json!({ "command": "git push origin main", "intent": "publish" }),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    let asks = approver.asks();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].permission, "bash");
    assert_eq!(asks[0].patterns, ["git push origin main"]);
}

#[tokio::test]
async fn dispatch_background_signal_waits_for_grace_then_detaches_running_tool() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(Notify::new());
    let background = InterruptSignal::new();
    let dispatcher = Arc::new(dispatcher(
        vec![Arc::new(DetachedTool {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            completed: Arc::clone(&completed),
        })],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
        background.clone(),
    ));
    let began = Instant::now();
    let task = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move {
            dispatcher
                .dispatch(request(
                    &dispatcher,
                    "call_background",
                    "bash",
                    json!({ "command": "sleep", "intent": "wait" }),
                ))
                .await
        })
    };

    started.notified().await;
    background.fire();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("dispatch must detach after the grace period")
        .expect("dispatch task");

    assert!(!result.is_error, "{}", result.output.output);
    assert!(result.output.output.contains("background"));
    assert!(
        began.elapsed() >= Duration::from_millis(700),
        "the dispatcher skipped the grace period"
    );

    release.notify_waiters();
    tokio::time::timeout(Duration::from_secs(1), completed.notified())
        .await
        .expect("detached tool must keep running");
}

#[test]
fn available_tools_omits_unconditionally_denied_entries() {
    let dispatcher = dispatcher(
        vec![
            Arc::new(RecordingTool::new("bash", Arc::new(AtomicUsize::new(0)))),
            Arc::new(RecordingTool::new(
                "tool_search",
                Arc::new(AtomicUsize::new(0)),
            )),
        ],
        vec![deny_rule("bash", "*")],
        Arc::new(RecordingApprover::default()),
        InterruptSignal::new(),
    );

    let available = dispatcher.available_tools();

    assert_eq!(
        available
            .definitions
            .iter()
            .map(|definition| definition.id.as_str())
            .collect::<Vec<_>>(),
        ["tool_search"]
    );
}
