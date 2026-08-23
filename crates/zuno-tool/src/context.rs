//! The execution context handed to every tool, and the abstractions it carries.
//!
//! Mirrors the oracle's `Tool.Context`
//! (`packages/opencode/src/tool/tool.ts:36-46`): the session, message and call
//! identifiers, the agent, the permission ask, and the cancellation signal.
//!
//! # Why the interrupt is a trait and not `zuno_engine::InterruptSignal`
//!
//! It would be a dependency cycle. `zuno-engine` dispatches tools, so
//! `zuno-engine → zuno-tool` is certain (todo 33's `dispatch.rs` cannot name
//! `dyn Tool` without it), and cargo rejects the reverse edge. Since
//! `InterruptSignal` lives in `zuno-engine/src/interrupt.rs`, this crate names the
//! two operations it needs instead, and `zuno-engine` supplies the impl:
//!
//! ```ignore
//! #[async_trait]
//! impl zuno_tool::InterruptHandle for InterruptSignal {
//!     fn is_set(&self) -> bool { self.is_set() }
//!     async fn notified(&self) { self.notified().await }
//! }
//! ```
//!
//! The method names and signatures are chosen to match `InterruptSignal`'s exactly
//! so that impl stays a forwarding shim with nowhere to introduce a discrepancy.
//! Critically, [`InterruptHandle::is_set`] is synchronous, preserving the property
//! todo 3 built the signal for: blocking tool code can poll cancellation with no
//! Tokio runtime in scope.

use async_trait::async_trait;
use serde_json::{Map, Value};
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_permission::{PermissionRequest, ToolCall};

use crate::ToolEffect;

/// The cancellation signal a running tool observes.
///
/// See the module docs for why this is a trait rather than `zuno-engine`'s concrete
/// `InterruptSignal`, and for the forwarding impl that connects them.
#[async_trait]
pub trait InterruptHandle: Send + Sync {
    /// Reads the fired state. Synchronous, so blocking tool code can poll it.
    fn is_set(&self) -> bool;

    /// Sleeps until fired, returning immediately when already fired.
    async fn notified(&self);
}

/// An interrupt that never fires.
///
/// For direct (non-turn) execution and for tests. Its [`InterruptHandle::notified`]
/// never completes, which is the honest reading of "never interrupted" — a caller
/// that awaits it is meant to be racing it against real work.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverInterrupted;

#[async_trait]
impl InterruptHandle for NeverInterrupted {
    fn is_set(&self) -> bool {
        false
    }

    async fn notified(&self) {
        std::future::pending::<()>().await
    }
}

/// A permission request a tool raises, minus the fields its context already knows.
///
/// The oracle types the tool-facing ask as
/// `Omit<PermissionV1.Request, "id" | "sessionID" | "tool">`
/// (`tool.ts:45`); this is the same subtraction, and
/// [`PermissionAsk::into_request`] performs the addition, so the two shapes are
/// checked against `zuno-permission`'s real request type by the compiler rather than
/// by hand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionAsk {
    /// The permission key being requested, after alias mapping.
    ///
    /// Use [`zuno_permission::visibility::permission_key`] to map a tool id onto it;
    /// `edit`, `write` and `apply_patch` share one key, and the three MCP resource
    /// tools share `read`.
    pub permission: String,
    /// The concrete patterns to match rules against, derived from the arguments.
    ///
    /// Derived from arguments and never from the tool name alone: a path that
    /// escapes the workspace is a different permission from one inside it, and only
    /// the arguments know which this is.
    pub patterns: Vec<String>,
    /// Extra detail for the approval prompt.
    pub metadata: Map<String, Value>,
    /// Patterns an "always" reply should install a standing grant for.
    pub always: Vec<String>,
    /// Effect of the tool invocation this ask protects, when known.
    ///
    /// Tool-internal resource checks leave this unset. Dispatch and composed-call
    /// boundaries set it so strict authorization can distinguish reads from
    /// mutations without guessing from the permission name.
    pub tool_effect: Option<ToolEffect>,
    /// Require a fresh attached-user decision and forbid standing or automatic approval.
    pub manual: bool,
}

impl PermissionAsk {
    /// A request for one pattern with no metadata and no standing-grant offer.
    #[must_use]
    pub fn new(permission: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            permission: permission.into(),
            patterns: vec![pattern.into()],
            metadata: Map::new(),
            always: Vec::new(),
            tool_effect: None,
            manual: false,
        }
    }

    /// Attach the classified effect of the invocation this ask protects.
    #[must_use]
    pub fn with_tool_effect(mut self, effect: ToolEffect) -> Self {
        self.tool_effect = Some(effect);
        self
    }

    /// Convert this ask into a fresh human-only decision.
    #[must_use]
    pub fn require_manual(mut self) -> Self {
        self.manual = true;
        self.always.clear();
        self
    }

    /// Completes the ask into the request the permission engine evaluates.
    #[must_use]
    pub fn into_request(
        self,
        id: impl Into<String>,
        session_id: impl Into<String>,
        tool: Option<ToolCall>,
    ) -> PermissionRequest {
        PermissionRequest {
            id: id.into(),
            session_id: session_id.into(),
            permission: self.permission,
            patterns: self.patterns,
            metadata: self.metadata,
            always: self.always,
            tool,
        }
    }
}

/// The permission decision point a tool calls before doing anything observable.
///
/// Implemented by the dispatch layer over `zuno-permission`; a tool only ever sees
/// this narrow view. Returning `Ok(())` means the call may proceed. A refusal is
/// [`ToolError::Denied`], which is deliberately neither retryable nor
/// model-correctable: it needs a grant, not a better call.
#[async_trait]
pub trait PermissionAsker: Send + Sync {
    /// Asks for authorization, blocking until it resolves.
    async fn ask(&self, tool: &str, ask: PermissionAsk) -> Result<(), ToolError>;
}

/// An asker that authorizes everything. For tests and for explicitly ungated paths.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

#[async_trait]
impl PermissionAsker for AllowAll {
    async fn ask(&self, _tool: &str, _ask: PermissionAsk) -> Result<(), ToolError> {
        Ok(())
    }
}

/// An asker that refuses everything. For tests that must prove a gate is consulted.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

#[async_trait]
impl PermissionAsker for DenyAll {
    async fn ask(&self, tool: &str, _ask: PermissionAsk) -> Result<(), ToolError> {
        Err(ToolError::Denied {
            tool: tool.to_owned(),
        })
    }
}

/// Everything a tool needs about the call it is serving.
///
/// Cheap to clone: the two behavioural collaborators are behind `Arc`, so cloning
/// shares one permission decision point and one cancellation signal rather than
/// forking them.
#[derive(Clone)]
pub struct ToolContext {
    /// The session this call belongs to.
    pub session_id: String,
    /// The assistant message that requested the call.
    pub message_id: String,
    /// The provider's identifier for this specific call.
    pub call_id: String,
    /// The agent whose configuration is in force.
    pub agent: String,
    /// How deep inside composed tool calls this one is; `0` at the turn level.
    ///
    /// [`ToolContext::for_subcall`] increments it. A composing tool (todo 70's
    /// `execute`) re-enters the registry, so without a depth a tool that composes
    /// itself would recurse until the stack ran out. The limit is the composer's to
    /// choose; recording the depth is this crate's job, because this crate owns the
    /// only place a child context is created.
    pub depth: u32,
    /// The permission decision point.
    pub permission: Arc<dyn PermissionAsker>,
    /// The cancellation signal to poll at every safe point.
    pub interrupt: Arc<dyn InterruptHandle>,
}

impl ToolContext {
    /// A turn-level context at depth zero.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        message_id: impl Into<String>,
        call_id: impl Into<String>,
        agent: impl Into<String>,
        permission: Arc<dyn PermissionAsker>,
        interrupt: Arc<dyn InterruptHandle>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            message_id: message_id.into(),
            call_id: call_id.into(),
            agent: agent.into(),
            depth: 0,
            permission,
            interrupt,
        }
    }

    /// Derives the context for a tool call made *by* this tool call.
    ///
    /// Only the call id changes, and the depth increments. Everything else is shared
    /// deliberately: a sub-call runs in the same session for the same agent under the
    /// same permission rules and the same cancellation signal, so a denied edit stays
    /// denied and one abort stops the whole tree. Anything a composing tool could
    /// vary here would be a way to launder a sub-call past a gate the parent could
    /// not pass. Pattern from
    /// `.omo/refs/jcode/crates/jcode-tool-core/src/lib.rs:124-135`, plus the depth.
    #[must_use]
    pub fn for_subcall(&self, call_id: impl Into<String>) -> Self {
        Self {
            session_id: self.session_id.clone(),
            message_id: self.message_id.clone(),
            call_id: call_id.into(),
            agent: self.agent.clone(),
            depth: self.depth.saturating_add(1),
            permission: Arc::clone(&self.permission),
            interrupt: Arc::clone(&self.interrupt),
        }
    }

    /// Whether cancellation has been requested. Safe to poll from blocking code.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.interrupt.is_set()
    }

    /// Asks for authorization for this call.
    pub async fn ask(&self, tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        self.permission.ask(tool, ask).await
    }

    /// The tool-call coordinates the permission engine records a request against.
    #[must_use]
    pub fn tool_call(&self) -> ToolCall {
        ToolCall {
            message_id: self.message_id.clone(),
            call_id: self.call_id.clone(),
        }
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("session_id", &self.session_id)
            .field("message_id", &self.message_id)
            .field("call_id", &self.call_id)
            .field("agent", &self.agent)
            .field("depth", &self.depth)
            .field("interrupted", &self.interrupt.is_set())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Shaped exactly like `zuno_engine::InterruptSignal`: a sync `is_set` over shared
    /// state plus an async `notified`. Standing in for it proves the forwarding impl
    /// in the module docs needs no adaptation.
    #[derive(Default)]
    struct Firable(Arc<AtomicBool>);

    #[async_trait]
    impl InterruptHandle for Firable {
        fn is_set(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }

        async fn notified(&self) {
            while !self.is_set() {
                tokio::task::yield_now().await;
            }
        }
    }

    fn context() -> ToolContext {
        ToolContext::new(
            "ses_1",
            "msg_1",
            "call_1",
            "build",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    #[test]
    fn subcall_inherits_everything_but_the_call_id_and_deepens() {
        let parent = context();
        let child = parent.for_subcall("call_2");

        assert_eq!(child.session_id, parent.session_id);
        assert_eq!(child.message_id, parent.message_id);
        assert_eq!(child.agent, parent.agent);
        assert_eq!(child.call_id, "call_2");
        assert_eq!(parent.depth, 0);
        assert_eq!(child.depth, 1);
        assert_eq!(child.for_subcall("call_3").depth, 2);
    }

    #[test]
    fn subcall_shares_one_permission_point_and_one_interrupt() {
        let parent = context();
        let child = parent.for_subcall("call_2");

        assert!(Arc::ptr_eq(&parent.permission, &child.permission));
        assert!(Arc::ptr_eq(&parent.interrupt, &child.interrupt));
    }

    #[test]
    fn interrupt_is_visible_to_a_subcall_without_a_runtime() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut parent = context();
        parent.interrupt = Arc::new(Firable(Arc::clone(&flag)));
        let child = parent.for_subcall("call_2");

        assert!(!child.is_interrupted());
        flag.store(true, Ordering::SeqCst);
        assert!(child.is_interrupted(), "one abort must stop the whole tree");
    }

    #[tokio::test]
    async fn deny_all_names_the_tool_it_refused() {
        let mut ctx = context();
        ctx.permission = Arc::new(DenyAll);

        let error = ctx
            .ask("bash", PermissionAsk::new("bash", "rm -rf /"))
            .await
            .expect_err("DenyAll must refuse");

        assert!(matches!(error, ToolError::Denied { .. }));
        assert_eq!(error.tool(), "bash");
        assert!(!error.is_retryable());
        assert!(!error.is_model_correctable());
    }

    #[test]
    fn ask_completes_into_the_engines_request_shape() {
        let ctx = context();
        let mut ask = PermissionAsk::new("edit", "src/lib.rs");
        ask.always.push("src/**".to_owned());
        ask.metadata
            .insert("reason".to_owned(), Value::String("rename".to_owned()));

        let request = ask.into_request("per_1", &ctx.session_id, Some(ctx.tool_call()));

        assert_eq!(request.id, "per_1");
        assert_eq!(request.session_id, "ses_1");
        assert_eq!(request.permission, "edit");
        assert_eq!(request.patterns, vec!["src/lib.rs".to_owned()]);
        assert_eq!(request.always, vec!["src/**".to_owned()]);
        assert_eq!(request.metadata["reason"], "rename");
        let call = request.tool.expect("tool coordinates");
        assert_eq!(call.message_id, "msg_1");
        assert_eq!(call.call_id, "call_1");
    }

    #[test]
    fn debug_reports_the_call_coordinates() {
        let rendered = format!("{:?}", context());

        assert!(rendered.contains("ses_1"));
        assert!(rendered.contains("call_1"));
        assert!(rendered.contains("depth: 0"));
    }
}
