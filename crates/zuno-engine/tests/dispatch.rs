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
use zuno_tool::{
    PermissionAsk, PermissionAsker, PermissionOrigin, Tool, ToolConcurrencyPolicy, ToolContext,
    ToolEffect, ToolOutput, ToolReplayPolicy,
};

#[derive(Default)]
struct RecordingApprover {
    asks: Mutex<Vec<PermissionAsk>>,
    origins: Mutex<Vec<(String, String, String)>>,
}

impl RecordingApprover {
    fn asks(&self) -> Vec<PermissionAsk> {
        self.asks.lock().expect("ask lock").clone()
    }

    fn origins(&self) -> Vec<(String, String, String)> {
        self.origins.lock().expect("origin lock").clone()
    }
}

#[async_trait]
impl PermissionAsker for RecordingApprover {
    async fn ask(
        &self,
        origin: PermissionOrigin<'_>,
        _tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        self.origins.lock().expect("origin lock").push((
            origin.session_id().to_owned(),
            origin.message_id().to_owned(),
            origin.call_id().to_owned(),
        ));
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
    async fn ask(
        &self,
        _origin: PermissionOrigin<'_>,
        _tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
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

struct ActionPolicyTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ActionPolicyTool {
    fn id(&self) -> &str {
        "mixed"
    }

    fn description(&self) -> &str {
        "Exercise argument-dependent scheduling and replay policy."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["read", "write"]
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn replay_policy_for(&self, args: &Value) -> ToolReplayPolicy {
        if args["action"] == "read" {
            ToolReplayPolicy::Safe
        } else {
            ToolReplayPolicy::Never
        }
    }

    fn concurrency_policy_for(&self, args: &Value) -> ToolConcurrencyPolicy {
        if args["action"] == "read" {
            ToolConcurrencyPolicy::ParallelSafe
        } else {
            ToolConcurrencyPolicy::Exclusive
        }
    }

    fn effect(&self, args: &Value) -> ToolEffect {
        if args["action"] == "read" {
            ToolEffect::ReadOnly
        } else {
            ToolEffect::SideEffecting
        }
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("mixed", "executed"))
    }
}

struct InternallyGatedTool {
    calls: Arc<AtomicUsize>,
    manual: bool,
}

#[async_trait]
impl Tool for InternallyGatedTool {
    fn id(&self) -> &str {
        "shell"
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
        let ask = PermissionAsk::new("shell", "nested command");
        let ask = if self.manual {
            ask.require_manual()
        } else {
            ask
        };
        ctx.ask("shell", ask).await?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("shell", "done"))
    }
}

struct ArgumentCapturingTool {
    received: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl Tool for ArgumentCapturingTool {
    fn id(&self) -> &str {
        "shell"
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
        Ok(ToolOutput::text("shell", "captured"))
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
        "shell"
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

struct CooperativeInterruptTool {
    started: Arc<Notify>,
    cleaned: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CooperativeInterruptTool {
    fn id(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "A long-running tool that acknowledges cancellation before returning."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.started.notify_one();
        ctx.interrupt.notified().await;
        self.cleaned.store(1, Ordering::SeqCst);
        Ok(ToolOutput::text("task", "child supervisor settled"))
    }
}

/// A tool that settles a cancelled call whose outcome it cannot decide.
///
/// `shell` is the real one: a command killed between starting a write and reporting it
/// has produced output but decided nothing. The tool says so on its result, and the
/// dispatcher is expected to record the interruption as uncertain rather than as a
/// clean cooperative return.
struct UndecidedCancellationTool {
    started: Arc<Notify>,
}

#[async_trait]
impl Tool for UndecidedCancellationTool {
    fn id(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "A tool that preserves what it produced and admits its outcome is undecided."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.started.notify_one();
        ctx.interrupt.notified().await;
        Ok(ToolOutput::text("task", "partial progress").with_metadata(
            "cancellation",
            json!({
                "cancelled": true,
                "authoritative": false,
                "uncertain": true,
            }),
        ))
    }
}

struct IgnoringInterruptTool {
    started: Arc<Notify>,
}

#[async_trait]
impl Tool for IgnoringInterruptTool {
    fn id(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "A broken tool that never acknowledges cancellation."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
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
        orchestration_snapshot: None,
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

#[derive(Default)]
struct EscalatingActionHooks;

#[async_trait]
impl ToolHooks for EscalatingActionHooks {
    async fn before(
        &self,
        _tool: &str,
        _session_id: &str,
        _call_id: &str,
        args: &mut Value,
    ) -> Result<(), String> {
        args["action"] = json!("write");
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

#[derive(Default)]
struct FailingAfterHooks;

#[async_trait]
impl ToolHooks for FailingAfterHooks {
    async fn after(
        &self,
        _tool: &str,
        _session_id: &str,
        _call_id: &str,
        _args: &Value,
        _output: &mut ToolOutput,
    ) -> Result<(), String> {
        Err(String::from("after hook failed"))
    }
}

#[tokio::test]
async fn strict_authorization_forces_fresh_manual_approval_despite_allows() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
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
                "shell",
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
async fn allow_all_skips_hitl_for_side_effecting_tools() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
        Vec::new(),
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::AllowAll,
        McpToolStatus::Ready,
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-allow-all",
            "shell",
            json!({"command": "chmod +x scripts/install.sh", "intent": "prepare installer"}),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    assert!(approver.asks().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn allow_all_skips_tool_owned_non_manual_gate() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(InternallyGatedTool {
            calls: Arc::clone(&calls),
            manual: false,
        })],
        Vec::new(),
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::AllowAll,
        McpToolStatus::Ready,
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-allow-all-internal",
            "shell",
            json!({"command": "printf ok", "intent": "run one command"}),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    assert!(
        approver.asks().is_empty(),
        "allow_all must cover non-manual permission checks owned by the tool"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn allow_all_skips_tool_owned_manual_gate() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(InternallyGatedTool {
            calls: Arc::clone(&calls),
            manual: true,
        })],
        Vec::new(),
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::AllowAll,
        McpToolStatus::Ready,
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-allow-all-manual",
            "shell",
            json!({"command": "rm -f /tmp/existing", "intent": "clean up"}),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    assert!(
        approver.asks().is_empty(),
        "allow_all must not surface a manual approval prompt"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn allow_all_keeps_explicit_denies_terminal() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
        vec![deny_rule("shell", "rm -rf /")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::AllowAll,
        McpToolStatus::Ready,
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-allow-all-deny",
            "shell",
            json!({"command": "rm -rf /", "intent": "must remain denied"}),
        ))
        .await;

    assert!(result.is_error);
    assert_eq!(
        result.blocked,
        Some(zuno_engine::r#loop::ToolBlockKind::Denied)
    );
    assert!(approver.asks().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn strict_authorization_keeps_explicit_denies_terminal() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
        vec![allow_all_rule(), deny_rule("shell", "rm -rf /")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Strict,
        McpToolStatus::Ready,
    )
    .with_hooks(Arc::new(AllowingHooks));

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-strict-deny",
            "shell",
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
            manual: false,
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
            "shell",
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
            manual: false,
        })],
        vec![deny_rule("shell", "nested command")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Strict,
        McpToolStatus::Ready,
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-strict-internal-deny",
            "shell",
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
async fn manual_tool_gate_requires_fresh_approval_despite_standard_allow() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(InternallyGatedTool {
            calls: Arc::clone(&calls),
            manual: true,
        })],
        vec![allow_all_rule()],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    );

    for index in 0..2 {
        let result = dispatcher
            .dispatch(request(
                &dispatcher,
                &format!("call-manual-{index}"),
                "shell",
                json!({"command": "rm -f /tmp/existing", "intent": "clean up"}),
            ))
            .await;
        assert!(!result.is_error, "{}", result.output.output);
    }

    let asks = approver.asks();
    assert_eq!(
        asks.len(),
        2,
        "a human-only risk decision was bypassed or remembered"
    );
    assert!(asks.iter().all(|ask| ask.manual));
    assert!(asks.iter().all(|ask| ask.always.is_empty()));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_plugin_allow_cannot_cross_an_explicit_deny_rule() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
        vec![allow_all_rule(), deny_rule("shell", "rm -rf /")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    )
    .with_hooks(Arc::new(AllowingHooks));

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-plugin-allow-versus-deny-rule",
            "shell",
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

/// The `deny_rule("shell", "original")` here is not testing denial.
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
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
        vec![deny_rule("shell", "original")],
        Arc::clone(&approver) as Arc<dyn PermissionAsker>,
        zuno_engine::dispatch::AuthorizationPolicy::Standard,
        McpToolStatus::Ready,
    )
    .with_hooks(Arc::new(MutatingHooks));

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call-hooked",
            "shell",
            json!({"command": "original", "intent": "verify plugin hooks"}),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    assert_eq!(result.output.output, "\"hooked\"");
    assert_eq!(result.output.title, "hooked title");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_hook_cannot_escalate_an_argument_dependent_policy_after_scheduling() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![Arc::new(ActionPolicyTool {
            calls: Arc::clone(&calls),
        })],
        Vec::new(),
        Arc::new(RecordingApprover::default()),
    )
    .with_hooks(Arc::new(EscalatingActionHooks));
    let request = request(
        &dispatcher,
        "call-policy-escalation",
        "mixed",
        json!({"action": "read"}),
    );
    assert_eq!(
        dispatcher.concurrency_policy(&request),
        ToolConcurrencyPolicy::ParallelSafe
    );

    let result = dispatcher.dispatch(request).await;

    assert!(result.is_error);
    assert!(
        result
            .output
            .output
            .contains("changed its concurrency policy after scheduling")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_plugin_allow_resolves_an_ask_without_prompting() {
    let calls = Arc::new(AtomicUsize::new(0));
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
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
            "shell",
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
            "shell",
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
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
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
            Arc::new(RecordingTool::new("shell", Arc::new(AtomicUsize::new(0)))),
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
            .contains("Available tools: shell, tool_search."),
        "{}",
        result.output.output
    );
}

#[tokio::test]
async fn dispatch_malformed_json_synthesizes_error_without_running_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
    );
    let mut malformed = request(
        &dispatcher,
        "call_bad_json",
        "shell",
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
            .contains("Malformed arguments for tool `shell`")
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
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_bad_args",
            "shell",
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
            .contains("Invalid arguments for tool `shell`")
    );
    assert!(result.output.output.contains("command"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dispatch_denial_is_an_error_result_and_a_later_call_still_runs() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
        vec![allow_all_rule(), deny_rule("shell", "rm -rf /")],
        Arc::new(RecordingApprover::default()),
    );

    let denied = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_denied",
            "shell",
            json!({ "command": "rm -rf /", "intent": "unsafe" }),
        ))
        .await;
    let continued = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_continued",
            "shell",
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
            "shell",
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
                    "shell",
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
        vec![Arc::new(RecordingTool::new("shell", Arc::clone(&calls)))],
        Vec::new(),
        approver.clone(),
    ));
    let call = request(
        &dispatcher,
        "call_cancel_permission",
        "shell",
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
async fn dispatch_interrupt_waits_for_cooperative_tool_cleanup() {
    let started = Arc::new(Notify::new());
    let cleaned = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(dispatcher(
        vec![Arc::new(CooperativeInterruptTool {
            started: Arc::clone(&started),
            cleaned: Arc::clone(&cleaned),
        })],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
    ));
    let call = request(&dispatcher, "call_cooperative", "task", json!({}));
    let interrupt = call.interrupt.clone();
    let task = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move { dispatcher.dispatch(call).await })
    };

    started.notified().await;
    interrupt.fire();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cooperative cleanup must finish inside the grace period")
        .expect("dispatch task");

    assert_eq!(
        cleaned.load(Ordering::SeqCst),
        1,
        "dispatch dropped the tool future before it could settle its owned work"
    );
    assert!(result.is_error);
    assert_eq!(
        result.interruption,
        Some(zuno_engine::r#loop::ToolInterruption::Cooperative)
    );
    assert_eq!(
        result.output.output, "child supervisor settled",
        "cooperative cancellation must preserve the tool's terminal report"
    );
    assert_eq!(
        result.output.metadata["interruption"]["mode"],
        "cooperative"
    );
    assert_eq!(result.output.metadata["interruption"]["uncertain"], false);
}

/// A cooperative return is not automatically a certain one.
///
/// The dispatcher used to record every cooperative cancellation as certain, so a tool
/// that had been stopped partway through a side effect was reported as having completed
/// its cleanup. The claim now travels from the tool to the recorded interruption.
#[tokio::test]
async fn dispatch_records_an_undecided_cooperative_cancellation_as_uncertain() {
    let started = Arc::new(Notify::new());
    let dispatcher = Arc::new(dispatcher(
        vec![Arc::new(UndecidedCancellationTool {
            started: Arc::clone(&started),
        })],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
    ));
    let call = request(&dispatcher, "call_undecided", "task", json!({}));
    let interrupt = call.interrupt.clone();
    let task = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move { dispatcher.dispatch(call).await })
    };

    started.notified().await;
    interrupt.fire();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cooperative cleanup must finish inside the grace period")
        .expect("dispatch task");

    assert_eq!(
        result.interruption,
        Some(zuno_engine::r#loop::ToolInterruption::Cooperative)
    );
    assert!(
        result.output.output.starts_with("partial progress"),
        "what the tool preserved is what the model reads first: {}",
        result.output.output
    );
    assert!(
        result
            .output
            .output
            .contains("inspect authoritative state before retrying"),
        "an undecided cancellation has to say so in the text the model is answered with: {}",
        result.output.output
    );
    assert_eq!(
        result.output.metadata["interruption"]["mode"],
        "cooperative"
    );
    assert_eq!(result.output.metadata["interruption"]["uncertain"], true);
    assert_eq!(
        result.output.metadata["interruption"]["forced"], false,
        "an undecided outcome is not the same claim as a forced abort"
    );
}

#[tokio::test]
async fn dispatch_interrupt_keeps_typed_cancellation_when_after_hook_fails() {
    let started = Arc::new(Notify::new());
    let cleaned = Arc::new(AtomicUsize::new(0));
    let dispatcher = Arc::new(
        dispatcher(
            vec![Arc::new(CooperativeInterruptTool {
                started: Arc::clone(&started),
                cleaned: Arc::clone(&cleaned),
            })],
            vec![allow_all_rule()],
            Arc::new(RecordingApprover::default()),
        )
        .with_hooks(Arc::new(FailingAfterHooks)),
    );
    let call = request(&dispatcher, "call_after_hook", "task", json!({}));
    let interrupt = call.interrupt.clone();
    let task = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move { dispatcher.dispatch(call).await })
    };

    started.notified().await;
    interrupt.fire();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cooperative cleanup must finish inside the grace period")
        .expect("dispatch task");

    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    assert_eq!(
        result.interruption,
        Some(zuno_engine::r#loop::ToolInterruption::Cooperative)
    );
    assert_eq!(result.output.output, "child supervisor settled");
    assert_eq!(
        result.output.metadata["afterHookError"]["message"],
        "after hook failed"
    );
}

#[tokio::test(start_paused = true)]
async fn dispatch_interrupt_forces_and_marks_a_tool_that_ignores_cancellation() {
    let started = Arc::new(Notify::new());
    let dispatcher = Arc::new(dispatcher(
        vec![Arc::new(IgnoringInterruptTool {
            started: Arc::clone(&started),
        })],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
    ));
    let call = request(&dispatcher, "call_forced", "task", json!({}));
    let interrupt = call.interrupt.clone();
    let task = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move { dispatcher.dispatch(call).await })
    };

    started.notified().await;
    interrupt.fire();
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "dispatch forced the tool before the cooperative cleanup grace elapsed"
    );

    tokio::time::advance(Duration::from_secs(2) + Duration::from_millis(1)).await;
    let result = task.await.expect("dispatch task");

    assert!(result.is_error);
    assert_eq!(
        result.interruption,
        Some(zuno_engine::r#loop::ToolInterruption::Forced)
    );
    assert_eq!(result.output.metadata["interruption"]["mode"], "forced");
    assert_eq!(result.output.metadata["interruption"]["forced"], true);
    assert_eq!(result.output.metadata["interruption"]["uncertain"], true);
}

#[tokio::test]
async fn dispatch_passes_argument_pattern_to_permission_approver() {
    let approver = Arc::new(RecordingApprover::default());
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new(
            "shell",
            Arc::new(AtomicUsize::new(0)),
        ))],
        Vec::new(),
        approver.clone(),
    );

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_pattern",
            "shell",
            json!({ "command": "git push origin main", "intent": "publish" }),
        ))
        .await;

    assert!(!result.is_error, "{}", result.output.output);
    let asks = approver.asks();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].permission, "shell");
    assert_eq!(asks[0].patterns, ["git push origin main"]);
    assert_eq!(
        approver.origins(),
        [(
            String::from("ses_dispatch"),
            String::from("msg_dispatch"),
            String::from("call_pattern"),
        )],
        "the rule asker must forward the ToolContext origin unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_forced_interrupt_marks_uncertainty_while_the_abort_is_reaped() {
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
        "shell",
        json!({ "command": "wait", "intent": "wait" }),
    );
    let interrupt = call.interrupt.clone();
    let task = {
        let dispatcher = Arc::clone(&dispatcher);
        tokio::spawn(async move { dispatcher.dispatch(call).await })
    };

    started.notified().await;
    interrupt.fire();
    tokio::time::timeout(Duration::from_secs(3), async {
        while drop_entered.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled tool future reaches Drop");
    {
        let (released, changed) = &*drop_release;
        *released.lock().expect("drop release lock") = true;
        changed.notify_all();
    }
    let result = task.await.expect("dispatch task");

    assert!(result.is_error);
    assert_eq!(
        result.interruption,
        Some(zuno_engine::r#loop::ToolInterruption::Forced)
    );
    assert!(
        result.output.output.contains("force-aborted"),
        "{}",
        result.output.output
    );
    assert_eq!(result.output.metadata["interruption"]["mode"], "forced");
    assert_eq!(result.output.metadata["interruption"]["uncertain"], true);
}

#[test]
fn available_tools_omits_unconditionally_denied_entries() {
    let dispatcher = dispatcher(
        vec![
            Arc::new(RecordingTool::new("shell", Arc::new(AtomicUsize::new(0)))),
            Arc::new(RecordingTool::new(
                "tool_search",
                Arc::new(AtomicUsize::new(0)),
            )),
        ],
        vec![deny_rule("shell", "*")],
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

#[tokio::test]
async fn deferred_tools_are_discovered_monotonically_before_they_become_callable() {
    let docs_calls = Arc::new(AtomicUsize::new(0));
    let issue_calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![
            Arc::new(RecordingTool::read_only(
                "read",
                Arc::new(AtomicUsize::new(0)),
            )),
            Arc::new(RecordingTool::read_only(
                "mcp_docs_search",
                Arc::clone(&docs_calls),
            )),
            Arc::new(RecordingTool::read_only(
                "mcp_issue_lookup",
                Arc::clone(&issue_calls),
            )),
        ],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
    )
    .with_deferred_tools(vec![
        "mcp_docs_search".to_owned(),
        "mcp_issue_lookup".to_owned(),
    ]);

    let initial = dispatcher.available_tools();
    assert_eq!(initial.revision, 0);
    assert_eq!(
        initial
            .definitions
            .iter()
            .map(|definition| definition.id.as_str())
            .collect::<Vec<_>>(),
        ["read", "tool_search"]
    );

    let hidden = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_hidden",
            "mcp_docs_search",
            json!({ "command": "docs" }),
        ))
        .await;
    assert!(hidden.is_error);
    assert_eq!(docs_calls.load(Ordering::SeqCst), 0);

    let first_search = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_search_docs",
            "tool_search",
            json!({ "query": "docs" }),
        ))
        .await;
    assert!(!first_search.is_error, "{}", first_search.output.output);
    assert_eq!(
        first_search.output.metadata["newlyExposedTools"],
        json!(["mcp_docs_search"])
    );

    let after_docs = dispatcher.available_tools();
    assert_eq!(after_docs.revision, 1);
    assert_eq!(
        after_docs
            .definitions
            .iter()
            .map(|definition| definition.id.as_str())
            .collect::<Vec<_>>(),
        ["read", "mcp_docs_search", "tool_search"]
    );
    let docs = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_docs",
            "mcp_docs_search",
            json!({ "command": "docs" }),
        ))
        .await;
    assert!(!docs.is_error, "{}", docs.output.output);
    assert_eq!(docs_calls.load(Ordering::SeqCst), 1);

    let second_search = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_search_issues",
            "tool_search",
            json!({ "query": "issue" }),
        ))
        .await;
    assert!(!second_search.is_error, "{}", second_search.output.output);

    let after_issues = dispatcher.available_tools();
    assert_eq!(after_issues.revision, 2);
    assert_eq!(
        after_issues
            .definitions
            .iter()
            .map(|definition| definition.id.as_str())
            .collect::<Vec<_>>(),
        ["read", "mcp_docs_search", "mcp_issue_lookup", "tool_search"]
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

#[tokio::test]
async fn a_failed_after_hook_keeps_a_settled_tool_result_and_its_status() {
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = dispatcher(
        vec![Arc::new(RecordingTool::new("task", Arc::clone(&calls)))],
        vec![allow_all_rule()],
        Arc::new(RecordingApprover::default()),
    )
    .with_hooks(Arc::new(FailingAfterHooks));

    let result = dispatcher
        .dispatch(request(
            &dispatcher,
            "call_after_hook_settled",
            "task",
            json!({"command": "ls"}),
        ))
        .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the tool ran, so whatever it changed is real"
    );
    assert!(
        !result.is_error,
        "a hook that failed to post-process the output must not report the tool itself as \
         failed; the model would then repeat a side effect that already happened: {result:?}"
    );
    assert_eq!(
        result.output.output, "\"ls\"",
        "the tool's own output is what the model sees"
    );
    assert_eq!(
        result.output.metadata["afterHookError"]["message"], "after hook failed",
        "the hook failure travels with the result instead of replacing it"
    );
}
