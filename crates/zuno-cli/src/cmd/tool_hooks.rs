//! Everything the host does around a tool call, in one place.
//!
//! [`zuno_engine::hooks::ToolHooks`] is installed once per dispatcher, so a host with
//! two things to say about a tool call has to say them through one object. That is this
//! module. It owns no policy of its own: it holds the durable verification ledger and
//! the repository navigation gate, and decides only the order they run in and what
//! happens when they disagree.
//!
//! # Why the gate runs before and the ledger after
//!
//! The two hooks answer different questions. Navigation asks whether this call should
//! happen at all, which only has an answer before it runs. The ledger records what a
//! call did, which only has an answer afterwards. Neither could be moved to the other
//! seam without changing what it means.
//!
//! # Why an advisory is carried across the call
//!
//! `before` has no output to write to — the call has not produced one yet — and the
//! model reads nothing but the result text. A navigation advisory raised in `before`
//! is therefore parked under its call id and appended when `after` hands over the
//! output. The map holds at most one entry per session, because the policy reports a
//! session's first violation and then leaves it alone; an advisory whose call never
//! reaches `after` is dropped when the session's policy is dropped.
//!
//! # Why the gate is per session and not per host
//!
//! One dispatcher serves the host's own session and every session it delegates to. The
//! gate tracks whether whoever is navigating has consulted the index, and a delegated
//! child is a different model with its own context that never saw the parent's query.
//! Sharing one gate across them would let a parent's index check excuse every child
//! from ever making one, which is the opposite of what the policy is for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use zuno_db::event_log::{NewSessionEvent, SessionEventLog};
use zuno_engine::hooks::ToolHooks;
use zuno_tool::ToolOutput;
use zuno_tools::shell::ShellSyntax;
use zuno_tools::{NavigationDecision, NavigationMode, RepositoryNavigationPolicy};

use super::verification_ledger::{VerificationLedger, announce};

/// The host's tool hooks.
pub(crate) struct HostToolHooks {
    ledger: VerificationLedger,
    navigation: Option<Navigation>,
}

/// The navigation gate, its durable log, and the advisories in flight.
struct Navigation {
    mode: NavigationMode,
    indexed: bool,
    syntax: ShellSyntax,
    events: SessionEventLog,
    /// One gate per session; see the module documentation for why.
    gates: Mutex<HashMap<String, Arc<RepositoryNavigationPolicy>>>,
    /// Advisories raised in `before`, keyed by the call they belong to.
    pending: Mutex<HashMap<String, String>>,
}

impl HostToolHooks {
    /// Hooks that record receipts and expire evidence, with no navigation gate.
    pub(crate) const fn new(ledger: VerificationLedger) -> Self {
        Self {
            ledger,
            navigation: None,
        }
    }

    /// Also gate source navigation on the CodeGraph index.
    ///
    /// `indexed` is resolved once by the caller, from the worktree, because an index
    /// appearing mid-session must not change what an earlier call in the same session
    /// was judged against. `Off` still installs the gate: the mode is read per
    /// decision, and installing conditionally would mean a mode set in configuration
    /// took effect only after a restart.
    pub(crate) fn with_navigation(
        mut self,
        mode: NavigationMode,
        indexed: bool,
        syntax: ShellSyntax,
        events: SessionEventLog,
    ) -> Self {
        self.navigation = Some(Navigation {
            mode,
            indexed,
            syntax,
            events,
            gates: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        });
        self
    }
}

impl Navigation {
    /// The gate for `session_id`, created on first sight.
    ///
    /// A poisoned lock is recovered rather than propagated, for the reason the policy
    /// itself gives: the state behind it is a few booleans that no panic can leave
    /// torn, and refusing every later call because one panicked would turn an advisory
    /// mechanism into an outage.
    fn gate(&self, session_id: &str) -> Arc<RepositoryNavigationPolicy> {
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(gates.entry(session_id.to_owned()).or_insert_with(|| {
            Arc::new(
                RepositoryNavigationPolicy::new(self.mode, self.indexed)
                    .with_shell_syntax(self.syntax),
            )
        }))
    }

    /// Record one decision in the session's durable event stream.
    ///
    /// Durable because the interesting question is asked after the fact: a run that
    /// reached a wrong conclusion by reading a handful of files is diagnosed by
    /// noticing that it never queried the index, and a note that lived only in the
    /// transcript is gone by the time anybody asks. Failures are swallowed: this is
    /// observability, and a log write that failed must not decide whether a tool call
    /// runs.
    ///
    /// The append runs on a blocking thread. It is a synchronous SQLite transaction
    /// behind a process-wide writer mutex, and holding an async worker on a contended
    /// write would stall every other task on it — including the tool call this
    /// decision is about to let through.
    async fn record(&self, session_id: &str, tool: &str, decision: &NavigationDecision) {
        let Some(code) = decision.code() else {
            return;
        };
        let mut properties = Map::new();
        properties.insert("tool".to_owned(), Value::String(tool.to_owned()));
        properties.insert(
            "mode".to_owned(),
            Value::String(self.mode.as_str().to_owned()),
        );
        properties.insert("indexed".to_owned(), Value::Bool(self.indexed));
        properties.insert(
            "enforced".to_owned(),
            Value::Bool(matches!(decision, NavigationDecision::Refuse { .. })),
        );
        if let Some(detail) = decision.detail() {
            properties.insert("detail".to_owned(), Value::String(detail.to_owned()));
        }
        let Ok(event) = NewSessionEvent::new(code, properties) else {
            return;
        };
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        let _ignored = tokio::task::spawn_blocking(move || events.append(&session_id, event)).await;
    }

    /// Park an advisory until the call it belongs to produces output.
    fn park(&self, call_id: &str, detail: String) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call_id.to_owned(), detail);
    }

    /// Take the advisory parked for `call_id`, if there is one.
    fn take(&self, call_id: &str) -> Option<String> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(call_id)
    }
}

#[async_trait::async_trait]
impl ToolHooks for HostToolHooks {
    /// Judge the call against the navigation gate.
    ///
    /// # Errors
    ///
    /// Returns the gate's own sentence when the mode is `strict` and the call would
    /// read source before the index has been consulted. The dispatcher turns that into
    /// a failed result the model reads, which is the only channel a refusal has.
    async fn before(
        &self,
        tool: &str,
        session_id: &str,
        call_id: &str,
        args: &mut Value,
    ) -> Result<(), String> {
        let Some(navigation) = self.navigation.as_ref() else {
            return Ok(());
        };
        let decision = navigation.gate(session_id).observe(tool, args);
        if decision.is_allow() {
            return Ok(());
        }
        navigation.record(session_id, tool, &decision).await;
        match decision {
            NavigationDecision::Allow => Ok(()),
            NavigationDecision::Advise { detail, .. } => {
                navigation.park(call_id, detail);
                Ok(())
            }
            NavigationDecision::Refuse { detail, .. } => Err(detail),
        }
    }

    /// Report any parked advisory, then let the ledger record what the call did.
    ///
    /// # Errors
    ///
    /// Never. The ledger degrades its own claim rather than failing a call that has
    /// already run, and an advisory that could not be delivered is not worth failing a
    /// completed call over.
    async fn after(
        &self,
        tool: &str,
        session_id: &str,
        call_id: &str,
        args: &Value,
        output: &mut ToolOutput,
    ) -> Result<(), String> {
        if let Some(detail) = self
            .navigation
            .as_ref()
            .and_then(|navigation| navigation.take(call_id))
        {
            announce(output, "[navigation]", &detail);
        }
        self.ledger
            .after(tool, session_id, call_id, args, output)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_engine::hooks::ToolHooks as _;

    const SESSION: &str = "ses_navigation";
    const OTHER_SESSION: &str = "ses_navigation_child";

    /// A migrated in-memory database with two sessions the event log can write to.
    fn database() -> Arc<zuno_db::pool::Pool> {
        let pool = Arc::new(
            zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open memory database"),
        );
        let mut connection = pool.get().expect("schema connection");
        zuno_db::migration::apply(&mut connection).expect("apply schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project_navigation', '/workspace', 1, 1, '[]');
                 INSERT INTO session (
                     id, project_id, slug, directory, title, version, time_created, time_updated
                 ) VALUES (
                     'ses_navigation', 'project_navigation', 'navigation', '/workspace',
                     'Navigation', 'zuno', 1, 1
                 );
                 INSERT INTO session (
                     id, project_id, slug, directory, title, version, time_created, time_updated
                 ) VALUES (
                     'ses_navigation_child', 'project_navigation', 'navigation-child',
                     '/workspace', 'Navigation child', 'zuno', 1, 1
                 );",
            )
            .expect("seed sessions");
        drop(connection);
        pool
    }

    /// Hooks with a navigation gate in `mode` over an indexed worktree.
    fn hooks(pool: &Arc<zuno_db::pool::Pool>, mode: NavigationMode) -> HostToolHooks {
        let spill = std::env::temp_dir().join("zuno-tool-hooks-tests");
        let goals = Arc::new(
            zuno_goal::GoalStore::from_pool(Arc::clone(pool), spill).expect("open goal store"),
        );
        HostToolHooks::new(VerificationLedger::new(Arc::clone(pool), goals)).with_navigation(
            mode,
            true,
            ShellSyntax::Bash,
            SessionEventLog::new(Arc::clone(pool)),
        )
    }

    /// A grep for a symbol: the call the gate exists to notice.
    fn grep() -> Value {
        serde_json::json!({"pattern": "fn exit_contract", "path": "crates"})
    }

    /// Every recorded navigation event type for `session_id`, in order.
    fn events(pool: &Arc<zuno_db::pool::Pool>, session_id: &str) -> Vec<String> {
        SessionEventLog::new(Arc::clone(pool))
            .read_after(session_id, None)
            .expect("read events")
            .into_iter()
            .map(|event| event.event_type)
            .collect()
    }

    #[tokio::test]
    async fn an_advisory_raised_before_the_call_reaches_the_model_with_the_result() {
        let pool = database();
        let hooks = hooks(&pool, NavigationMode::Advise);
        let mut args = grep();

        hooks
            .before("grep", SESSION, "call_1", &mut args)
            .await
            .expect("advise never refuses");

        // `before` has no output to write to, so the advisory has to survive the call.
        // If it did not, the model would read a plain result and never learn that the
        // index it is ignoring would have answered the question outright.
        let mut output = zuno_tool::ToolOutput::text("Search", "3 matches");
        hooks
            .after("grep", SESSION, "call_1", &args, &mut output)
            .await
            .expect("after never fails");
        assert!(output.output.contains("[navigation]"), "{}", output.output);
        assert!(output.output.starts_with("3 matches"), "{}", output.output);
        assert_eq!(
            events(&pool, SESSION),
            vec![zuno_tools::NAVIGATION_INDEX_BYPASSED.to_owned()],
            "the advisory is durable, because the question is asked after the run"
        );

        // Advise reports once. A second call carries nothing.
        let mut second = zuno_tool::ToolOutput::text("Search", "1 match");
        hooks
            .before("grep", SESSION, "call_2", &mut args)
            .await
            .expect("advise never refuses");
        hooks
            .after("grep", SESSION, "call_2", &args, &mut second)
            .await
            .expect("after never fails");
        assert!(!second.output.contains("[navigation]"), "{}", second.output);
    }

    #[tokio::test]
    async fn strict_refuses_the_call_and_says_so_durably() {
        let pool = database();
        let hooks = hooks(&pool, NavigationMode::Strict);

        let refusal = hooks
            .before("grep", SESSION, "call_1", &mut grep())
            .await
            .expect_err("strict refuses");
        assert!(!refusal.is_empty());
        assert_eq!(
            events(&pool, SESSION),
            vec![zuno_tools::NAVIGATION_INDEX_BYPASSED.to_owned()]
        );
    }

    #[tokio::test]
    async fn a_delegated_session_earns_its_own_advisory() {
        let pool = database();
        let hooks = hooks(&pool, NavigationMode::Advise);

        hooks
            .before("grep", SESSION, "call_1", &mut grep())
            .await
            .expect("advise never refuses");
        // The parent has now been told. A child is a different model with its own
        // context that never saw that: sharing one gate would let the parent's single
        // notice excuse every session it delegates to.
        hooks
            .before("grep", OTHER_SESSION, "call_2", &mut grep())
            .await
            .expect("advise never refuses");

        assert_eq!(events(&pool, SESSION).len(), 1);
        assert_eq!(events(&pool, OTHER_SESSION).len(), 1);
    }

    #[tokio::test]
    async fn the_gate_off_leaves_every_call_alone() {
        let pool = database();
        let hooks = hooks(&pool, NavigationMode::Off);
        let mut args = grep();

        hooks
            .before("grep", SESSION, "call_1", &mut args)
            .await
            .expect("off never refuses");
        let mut output = zuno_tool::ToolOutput::text("Search", "3 matches");
        hooks
            .after("grep", SESSION, "call_1", &args, &mut output)
            .await
            .expect("after never fails");
        assert_eq!(output.output, "3 matches");
        assert!(events(&pool, SESSION).is_empty());
    }

    #[tokio::test]
    async fn without_a_gate_the_ledger_still_runs() {
        let pool = database();
        let spill = std::env::temp_dir().join("zuno-tool-hooks-tests");
        let goals = Arc::new(
            zuno_goal::GoalStore::from_pool(Arc::clone(&pool), spill).expect("open goal store"),
        );
        let hooks = HostToolHooks::new(VerificationLedger::new(Arc::clone(&pool), goals));
        let args = grep();

        let receipt = zuno_tool::VerificationReceipt::passed("cargo test");
        let mut output =
            zuno_tool::ToolOutput::text("Shell", "test result: ok").with_verification(&receipt);
        hooks
            .after("shell", SESSION, "call_1", &args, &mut output)
            .await
            .expect("after never fails");
        assert!(output.output.contains("rcp_"), "{}", output.output);
    }
}
