//! What a tool call leaves in the log, in both directions.
//!
//! # Why this exists
//!
//! A session in which MCP tool calls failed repeatedly produced a log containing no
//! MCP entry at all, so the cause had to be recovered from the database by hand. The
//! record now emitted at [`zuno_tool::Tool::invoke`] is the fix, and it is only worth
//! anything if it fires when a call fails **and** stays silent when one succeeds.
//! Asserting one direction would pass against a build that logs every call, which is
//! the failure mode this project already lived through elsewhere: duplicate-skill
//! precedence at `WARN` put 189 lines demanding attention into a 202-line log.
//!
//! # Why a hand-written subscriber
//!
//! `tracing` caches callsite interest **process-wide**. A sibling that fires a
//! callsite while no subscriber is installed caches `Interest::never` for it, and a
//! thread-local subscriber installed later then observes nothing — measured elsewhere
//! in this workspace as three events alone and zero beside fifteen siblings
//! (`crates/zuno-catalog/tests/skill_log_level.rs:1-14`). Two defences are used
//! together here:
//!
//! 1. The subscriber is **global** and installed by every test before it runs a tool,
//!    through a [`OnceLock`] so the one permitted `set_global_default` is shared. Its
//!    `register_callsite` returns `Interest::always`, so no callsite can be cached off.
//! 2. Every test uses a **tool id no other test uses**, and filters the capture by it.
//!    Siblings therefore cannot silence, pad, or race each other's assertions, which
//!    is what lets the positive and negative directions live in one binary at all.
//!
//! It is written against `tracing` alone rather than `tracing-subscriber` because that
//! is what this crate depends on.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::fmt::Debug;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::field::{Field, Visit};
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, span};
use zuno_error::ToolError;
use zuno_tool::{NeverInterrupted, PermissionAsk, PermissionAsker, Tool, ToolContext, ToolOutput};

/// One captured event, reduced to what an assertion here reads.
#[derive(Clone, Debug)]
struct Captured {
    level: Level,
    message: String,
    tool: Option<String>,
    session: Option<String>,
    call_id: Option<String>,
}

#[derive(Default)]
struct Log(Mutex<Vec<Captured>>);

impl Log {
    fn about(&self, tool: &str) -> Vec<Captured> {
        self.0
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|event| event.tool.as_deref() == Some(tool))
            .cloned()
            .collect()
    }

    /// Every event that names no tool, used to prove nothing leaked in unlabelled.
    fn unattributed(&self) -> Vec<Captured> {
        self.0
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|event| event.tool.is_none())
            .cloned()
            .collect()
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    tool: Option<String>,
    session: Option<String>,
    call_id: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "tool.name" => self.tool = Some(value.to_owned()),
            "session.id" => self.session = Some(value.to_owned()),
            "tool.call_id" => self.call_id = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

/// A capture-only subscriber. Spans are accepted and discarded; only events are read.
struct Collector(Arc<Log>);

impl tracing::Subscriber for Collector {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::always()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.0.lock().expect("capture lock").push(Captured {
            level: *event.metadata().level(),
            message: fields.message,
            tool: fields.tool,
            session: fields.session,
            call_id: fields.call_id,
        });
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

fn log() -> Arc<Log> {
    static LOG: OnceLock<Arc<Log>> = OnceLock::new();
    Arc::clone(LOG.get_or_init(|| {
        let log = Arc::new(Log::default());
        tracing::subscriber::set_global_default(Collector(Arc::clone(&log)))
            .expect("this binary owns the global subscriber");
        log
    }))
}

/// A tool that fails with a cause two links deep, like an MCP proxy relaying a server.
struct Failing(&'static str);

#[async_trait]
impl Tool for Failing {
    fn id(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "Fail with a nested cause."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed {
            tool: self.0.to_owned(),
            source: Box::new(Outer(Inner)),
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the browser refused the request")]
struct Outer(#[source] Inner);

#[derive(Debug, thiserror::Error)]
#[error("no open page to attach to")]
struct Inner;

struct Succeeding(&'static str);

#[async_trait]
impl Tool for Succeeding {
    fn id(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "Succeed quietly."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("ok", "listed 3 pages"))
    }
}

/// A tool the permission layer refuses, which is a user decision and not a fault.
struct Denied(&'static str);

#[async_trait]
impl Tool for Denied {
    fn id(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "Be refused."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Denied {
            tool: self.0.to_owned(),
        })
    }
}

struct Allow;

#[async_trait]
impl PermissionAsker for Allow {
    async fn ask(&self, _tool: &str, _ask: PermissionAsk) -> Result<(), ToolError> {
        Ok(())
    }
}

fn context(call_id: &str) -> ToolContext {
    ToolContext::new(
        "ses_failure_log",
        "msg_failure_log",
        call_id,
        "build",
        Arc::new(Allow),
        Arc::new(NeverInterrupted),
    )
}

#[tokio::test]
async fn a_failed_tool_call_is_recorded_at_warn_naming_the_tool_and_every_cause() {
    let log = log();
    let tool = Failing("probe_failing_tool");

    let result = tool
        .invoke(json!({}), context("call_failing"))
        .await
        .expect_err("the probe tool always fails");

    let events = log.about("probe_failing_tool");
    assert_eq!(
        events.len(),
        1,
        "a failed call must leave exactly one record, not zero and not a stream: {events:#?}"
    );
    let event = &events[0];
    assert_eq!(
        event.level,
        Level::WARN,
        "a failed tool call must surface at the default level, or the user never sees it"
    );
    assert!(
        event.message.contains("tool probe_failing_tool failed"),
        "the record must name the tool that failed: {:?}",
        event.message
    );
    assert!(
        event.message.contains("the browser refused the request"),
        "the record must carry the reason, not just the category: {:?}",
        event.message
    );
    assert!(
        event.message.contains("no open page to attach to"),
        "the record must reach the innermost cause, which is the actual diagnosis: {:?}",
        event.message
    );
    assert_eq!(
        event.call_id.as_deref(),
        Some("call_failing"),
        "the call id is what locates the failing row without reading the database by hand"
    );
    assert_eq!(event.session.as_deref(), Some("ses_failure_log"));
    assert_eq!(
        result.tool(),
        "probe_failing_tool",
        "logging must not change what the caller receives"
    );
}

#[tokio::test]
async fn a_successful_tool_call_records_nothing() {
    let log = log();
    let tool = Succeeding("probe_succeeding_tool");

    let output = tool
        .invoke(json!({}), context("call_succeeding"))
        .await
        .expect("the probe tool always succeeds");

    assert_eq!(output.output, "listed 3 pages");
    let events = log.about("probe_succeeding_tool");
    assert!(
        events.is_empty(),
        "a successful call must be silent; one line per call is how a real signal \
         became unreadable at 189 lines a launch: {events:#?}"
    );
    assert!(
        log.unattributed()
            .iter()
            .all(|event| event.level > Level::INFO),
        "no unlabelled event may reach the default level on the success path: {:#?}",
        log.unattributed()
    );
}

#[tokio::test]
async fn a_permission_denial_is_recorded_below_the_default_level_and_stays_discoverable() {
    let log = log();
    let tool = Denied("probe_denied_tool");

    let _refused = tool
        .invoke(json!({}), context("call_denied"))
        .await
        .expect_err("the probe tool is always refused");

    let events = log.about("probe_denied_tool");
    assert_eq!(
        events.len(),
        1,
        "a denial is still recorded, just not loudly: {events:#?}"
    );
    assert_eq!(
        events[0].level,
        Level::DEBUG,
        "a denial is the permission layer obeying the user, so it must not claim the \
         attention a fault does — every declined prompt would emit a line"
    );
    assert!(
        events[0].message.contains("probe_denied_tool"),
        "a denial that is quiet must still name its tool for `--log-level debug`: {:?}",
        events[0].message
    );
}
