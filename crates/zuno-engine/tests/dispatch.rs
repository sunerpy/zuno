use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
use zuno_tool::{PermissionAsk, PermissionAsker, Tool, ToolContext, ToolEffect, ToolOutput};

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
    effect: ToolEffect,
}

impl RecordingTool {
    fn new(id: &'static str, calls: Arc<AtomicUsize>) -> Self {
        Self {
            id,
            calls,
            events: None,
            effect: ToolEffect::SideEffecting,
        }
    }

    fn read_only(id: &'static str, calls: Arc<AtomicUsize>) -> Self {
        Self {
            id,
            calls,
            events: None,
            effect: ToolEffect::ReadOnly,
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
            effect: ToolEffect::SideEffecting,
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

    fn effect(&self, _args: &Value) -> ToolEffect {
        self.effect
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

struct InternallyGatedTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for InternallyGatedTool {
    fn id(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Exercise one tool-owned permission check after dispatch authorization."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        ctx.ask("bash", PermissionAsk::new("bash", "nested command"))
            .await?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("bash", "done"))
    }
}

struct ArgumentCapturingTool {
    received: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl Tool for ArgumentCapturingTool {
    fn id(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Record the arguments as the callee receives them."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        *self.received.lock().expect("received lock") = Some(args.clone());
        Ok(ToolOutput::text("bash", "captured"))
    }
}

struct BlockingDropTool {
    started: Arc<Notify>,
    drop_entered: Arc<AtomicUsize>,
    drop_release: Arc<(Mutex<bool>, Condvar)>,
}

struct BlockingDropGuard {
    drop_entered: Arc<AtomicUsize>,
    drop_release: Arc<(Mutex<bool>, Condvar)>,
}

impl Drop for BlockingDropGuard {
    fn drop(&mut self) {
        self.drop_entered.store(1, Ordering::SeqCst);
        let (released, changed) = &*self.drop_release;
        let mut released = released.lock().expect("drop release lock");
        while !*released {
            released = changed.wait(released).expect("drop release wait");
        }
    }
}

#[async_trait]
impl Tool for BlockingDropTool {
    fn id(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Block until the invocation is cancelled."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let _guard = BlockingDropGuard {
            drop_entered: Arc::clone(&self.drop_entered),
            drop_release: Arc::clone(&self.drop_release),
        };
        self.started.notify_one();
        std::future::pending().await
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
) -> ToolRegistryDispatcher {
    ToolRegistryDispatcher::new(
        tools,
        rules,
        approver,
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    )
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

/// A plugin that resolves the permission decision and leaves the arguments alone.
///
/// [`MutatingHooks`] cannot stand in for this: its `before` rewrites `command`, so
/// a rule keyed on the original argument stops matching before the permission layer
/// ever evaluates it. Keeping the arguments untouched is what makes an explicit
/// `deny` rule actually reachable in the test below.
#[derive(Default)]
struct AllowingHooks;

#[async_trait]
impl ToolHooks for AllowingHooks {
    async fn permission(
        &self,
        _request: &zuno_permission::PermissionRequest,
    ) -> Result<PermissionHookDecision, String> {
        Ok(PermissionHookDecision::Allow)
    }
}

#[tokio::test]
async fn strict_authorization_forces_fresh_manual_approval_despite_allows() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![allow_all_rule()],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Strict,
        McpToolStatus::Ready,
    )
    .with_hooks(Arc::new(AllowingHooks));

    for index in 0..2 {
        let result = dispatcher
            .dispatch(request(
                &dispatcher,
                &format!("call-strict-{index}"),
                "bash",
                json!({"command": "git status", "intent": "inspect"}),
            ))
            .await;
        assert!(!result.is_error, "{}", result.output.output);
    }

    let asks = approver.asks();
    assert_eq!(asks.len(), 2, "strict approval was remembered across calls");
    assert!(asks.iter().all(|ask| ask.manual));
    assert!(asks.iter().all(|ask| ask.always.is_empty()));
    assert!(
        asks.iter()
            .all(|ask| ask.tool_effect == Some(ToolEffect::SideEffecting))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn strict_authorization_does_not_add_prompts_to_read_only_tools() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::read_only(
            "grep",
            Arc::clone(&calls),
        ))],
        vec![allow_all_rule()],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Strict,
        McpToolStatus::Ready,
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-strict-read",
            "grep",
            json!({"command": "needle", "intent": "inspect"}),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    assert!(approver.asks().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn strict_authorization_keeps_explicit_denies_terminal() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![allow_all_rule(), deny_rule("bash", "rm -rf /")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Strict,
        McpToolStatus::Ready,
    )
    .with_hooks(Arc::new(AllowingHooks));

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-strict-deny",
            "bash",
            json!({"command": "rm -rf /", "intent": "must remain denied"}),
        ))
        .await;

    assert!(result.is_error);
    assert_eq!(
        result.blocked,
        Some(zuno_engine::r#loop::ToolBlockKind::Denied),
        "an explicit deny must be distinguishable from an execution failure"
    );
    assert!(approver.asks().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn strict_dispatch_approval_covers_the_same_tools_internal_gate_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(InternallyGatedTool {
            calls: Arc::clone(&calls),
        })],
        Vec::new(),
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Strict,
        McpToolStatus::Ready,
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-strict-internal",
            "bash",
            json!({"command": "printf ok", "intent": "run one command"}),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    assert_eq!(approver.asks().len(), 1, "the tool was prompted twice");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn strict_dispatch_approval_does_not_hide_a_later_explicit_resource_deny() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(InternallyGatedTool {
            calls: Arc::clone(&calls),
        })],
        vec![deny_rule("bash", "nested command")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Strict,
        McpToolStatus::Ready,
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-strict-internal-deny",
            "bash",
            json!({"command": "printf ok", "intent": "run one command"}),
        ))
        .await;

    assert!(result.is_error);
    assert_eq!(
        approver.asks().len(),
        1,
        "the top-level manual ask was lost"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an explicit nested resource deny was bypassed"
    );
}

#[tokio::test]
async fn a_plugin_allow_cannot_cross_an_explicit_deny_rule() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![allow_all_rule(), deny_rule("bash", "rm -rf /")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    )
    .with_hooks(Arc::new(AllowingHooks));

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-plugin-allow-versus-deny-rule",
            "bash",
            json!({"command": "rm -rf /", "intent": "prove the deny rule still holds"}),
        ))
        .await;

    assert!(
        result.is_error,
        "a plugin allow crossed the user's explicit deny rule: {}",
        result.output.output
    );
    assert_eq!(
        result.blocked,
        Some(zuno_engine::r#loop::ToolBlockKind::Denied),
        "the client must be able to render a refusal as blocked rather than failed"
    );
    assert!(
        result.output.output.contains("denied"),
        "the refusal must name the denial: {}",
        result.output.output
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the denied tool must never execute"
    );
    assert!(
        approver.asks().is_empty(),
        "an explicit deny is a refusal, not a prompt"
    );
}

/// The `deny_rule("bash", "original")` here is not testing denial.
///
/// `MutatingHooks::before` rewrites `command` from `original` to `hooked`, and the
/// permission patterns are derived from the arguments *after* that rewrite, so the
/// rule no longer matches. It used to be a pure decoy: the pre-latch dispatcher
/// skipped the rule set entirely whenever a plugin returned `Allow`, so the rule
/// could not have fired either way. Now that the rules are always consulted, the
/// rule earns its place — if the rewrite ever stopped reaching the permission layer,
/// the patterns would be `["original"]` and this call would be refused.
#[tokio::test]
async fn production_dispatch_rewrites_arguments_before_permission_and_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![deny_rule("bash", "original")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
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
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_plugin_allow_resolves_an_ask_without_prompting() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        Vec::new(),
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    )
    .with_hooks(Arc::new(AllowingHooks));

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-plugin-allow-resolves-ask",
            "bash",
            json!({"command": "git status", "intent": "inspect"}),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    assert!(
        approver.asks().is_empty(),
        "a plugin allow must still resolve a rule that left the decision at ask"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// The intent is what the approval prompt and the audit record are built from, so
/// removing it from the callee's arguments must not remove it from the ask. The two
/// halves are asserted together because either one alone permits the wrong fix:
/// stripping earlier would silence the prompt, and stripping later would leak.
#[tokio::test]
async fn the_intent_reaches_the_permission_layer_but_not_the_tool() {
    let received = Arc::new(Mutex::new(None));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = dispatcher(
        vec![Arc::new(ArgumentCapturingTool {
            received: Arc::clone(&received),
        })],
        Vec::new(),
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-intent-hand-off",
            "bash",
            json!({
                "command": "git status",
                "intent": "read the working tree state",
                "accept_large_output": true
            }),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);

    let asks = approver.asks();
    let ask = asks.first().expect("the permission layer was consulted");
    assert_eq!(
        ask.metadata["arguments"]["intent"], "read the working tree state",
        "the prompt and the audit record are built from this"
    );

    let arguments = received
        .lock()
        .expect("received lock")
        .clone()
        .expect("the tool ran");
    assert_eq!(arguments, json!({ "command": "git status" }));
}

#[tokio::test]
async fn dispatch_rejects_unregistered_tool_aliases_without_running_the_native_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
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
        assert!(result.is_error, "{alias}: {}", result.output.output);
        assert!(
            result
                .output
                .output
                .contains(&format!("Unknown tool: {alias}")),
            "{alias}: {}",
            result.output.output
        );
    }

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an unregistered alias must never execute the native tool"
    );
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
    assert_eq!(
        result.blocked,
        Some(zuno_engine::r#loop::ToolBlockKind::Unavailable),
        "a name with no registered implementation is unavailable, not an execution failure"
    );
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
    assert_eq!(
        result.blocked,
        Some(zuno_engine::r#loop::ToolBlockKind::InvalidArguments),
        "malformed JSON is refused before execution"
    );
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
    assert_eq!(
        result.blocked,
        Some(zuno_engine::r#loop::ToolBlockKind::InvalidArguments),
        "schema rejection happened before the tool ran"
    );
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
    assert_eq!(
        denied.blocked,
        Some(zuno_engine::r#loop::ToolBlockKind::Denied)
    );
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
async fn dispatch_interrupt_cancels_a_pending_permission_before_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let approver = Arc::new(BlockingApprover::new(events));
    let dispatcher = Arc::new(dispatcher(
        vec![Arc::new(RecordingTool::new("bash", Arc::clone(&calls)))],
        Vec::new(),
        approver.clone(),
    ));
    let call = request(
        &dispatcher,
        "call_cancel_permission",
        "bash",
        json!({ "command": "git push origin main", "intent": "publish" }),
    );
    let interrupt = call.interrupt.clone();
    let task = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move { dispatcher.dispatch(call).await })
    };

    approver.entered.notified().await;
    interrupt.fire();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("turn cancellation must wake a pending permission")
        .expect("dispatch task");

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(result.is_error);
    assert!(result.output.output.contains("interrupted"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_interrupt_joins_the_cancelled_tool_before_returning() {
    let started = Arc::new(Notify::new());
    let drop_entered = Arc::new(AtomicUsize::new(0));
    let drop_release = Arc::new((Mutex::new(false), Condvar::new()));
    let dispatcher = Arc::new(dispatcher(
        vec![Arc::new(BlockingDropTool {
            started: Arc::clone(&started),
            drop_entered: Arc::clone(&drop_entered),
            drop_release: Arc::clone(&drop_release),
        })],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
    ));
    let call = request(
        &dispatcher,
        "call_interrupt",
        "bash",
        json!({ "command": "wait", "intent": "wait" }),
    );
    let interrupt = call.interrupt.clone();
    let task = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move { dispatcher.dispatch(call).await })
    };

    started.notified().await;
    interrupt.fire();
    tokio::time::timeout(Duration::from_secs(1), async {
        while drop_entered.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled tool future reaches Drop");
    let returned_before_drop_finished = task.is_finished();
    {
        let (released, changed) = &*drop_release;
        *released.lock().expect("drop release lock") = true;
        changed.notify_all();
    }
    let result = task.await.expect("dispatch task");

    assert!(
        !returned_before_drop_finished,
        "dispatch reported interruption while the cancelled tool future was still live"
    );
    assert!(result.is_error);
    assert!(result.output.output.contains("interrupted"));
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

/// A tool whose failure carries a cause two links deep, like an MCP proxy relaying a
/// server's rejection of a call the transport had already refused.
struct NestedFailureTool;

#[derive(Debug, thiserror::Error)]
#[error("the browser refused the request")]
struct BrowserRefused(#[source] NoPage);

#[derive(Debug, thiserror::Error)]
#[error("no open page to attach to")]
struct NoPage;

#[async_trait]
impl Tool for NestedFailureTool {
    fn id(&self) -> &str {
        "chrome_devtools_list_pages"
    }

    fn description(&self) -> &str {
        "Fail with a nested cause."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed {
            tool: "chrome_devtools_list_pages".to_owned(),
            source: Box::new(BrowserRefused(NoPage)),
        })
    }
}

/// The model must receive every cause, not the category plus one link.
///
/// Unwrapping exactly one `source()` stopped here: `tool X failed: the browser refused
/// the request`, with the sentence naming what to do still one link below.
#[tokio::test]
async fn dispatch_hands_the_model_every_cause_beneath_a_failure() {
    let dispatcher = dispatcher(
        vec![Arc::new(NestedFailureTool)],
        Vec::new(),
        Arc::new(RecordingApprover::default()),
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_nested",
            "chrome_devtools_list_pages",
            json!({ "intent": "list the open pages" }),
        ))
        .await;

    assert!(
        result.is_error,
        "the tool failed, so the result must say so"
    );
    let reported = &result.output.output;
    assert!(
        reported.contains("tool chrome_devtools_list_pages failed"),
        "the category must still name the tool: {reported}"
    );
    assert!(
        reported.contains("the browser refused the request"),
        "the first cause must survive: {reported}"
    );
    assert!(
        reported.contains("no open page to attach to"),
        "the innermost cause is the diagnosis and must not be truncated: {reported}"
    );
}
