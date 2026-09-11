//! Native request-boundary waiting for the current logical foreground operation.
//!
//! Parent integration:
//! - declare this module in `turn.rs`;
//! - create one `Arc<ForegroundTurnBudget>` after cycle admission, before the
//!   first model request, and retain it across recovery/foreground continuations;
//! - install that same budget as `TurnContext::with_budget_policy`;
//! - install `ForegroundTurnHooks::with_foreground` on that context: its
//!   `before_provider_request` hook waits BEFORE history/inbox hydration, so a
//!   Shell attention yield cannot create unchanged paid-model polling;
//! - retain the original `SessionRunGuard`, defer success terminal events and
//!   use `wait` after a driver stop as a final host-completion barrier;
//! - on that barrier's `Completed` or `Steering`, use the existing
//!   `TurnStart::Recovery` path in the SAME cycle, before Plan/Goal reconciliation;
//! - on hard interruption or budget stop, call `stop` to durably drain terminal
//!   observations; a dropped request hook only requests process cancellation;
//! - map `BudgetLimited` to the existing typed turn-budget pause, and never
//!   route these completions through the detached notification driver.
//!
//! The generic engine hook remains a no-op without a host implementation;
//! `AgentDriver` itself does not acquire PTY or durable-publication dependencies.
//!
//! Reference: pinned Codex eaa8b6d917, `codex-rs/core/src/unified_exec/process_manager.rs`
//! (`write_stdin`, `collect_output_until_deadline`) and
//! `codex-rs/core/src/session/turn_input.rs` (`Session::steer_input`). Reuse their
//! same-process, notification and exact-turn principles. The inspected Codex
//! `Session::on_task_finished` can finish with live terminals: Zuno's foreground
//! barrier, durable receipt and inline ownership are deliberate adaptations,
//! not claims of identical Codex persistence or callback semantics.

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt as _};
use rusqlite::{Transaction, params};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
use zuno_db::completion_delivery::{CompletionOwner, claim_inline_in, publish_in};
use zuno_db::event_log::{NewSessionEvent, append_in};
use zuno_db::message::{MessageStore, PartRecord};
use zuno_engine::budget::{
    BudgetDecision, BudgetPolicyError, BudgetStop, BudgetStopKind, TurnAllowance, TurnBudgetPolicy,
    TurnUsageSnapshot,
};
use zuno_engine::interrupt::{HardInterruptRequest, InterruptSignal};
use zuno_engine::r#loop::{TurnError, TurnEvent, TurnEventSender};
use zuno_engine::status::SessionRunGuard;
use zuno_error::{DbError, ToolError, UncertainCause};
use zuno_pty::{
    BackgroundExecutionError, BackgroundExecutionId, BackgroundExecutionStatus, ForegroundExecution,
};
use zuno_tool::{OutputLimits, ReceiptOutcome, ToolOutput, ToolOutputStore, VerificationReceipt};

pub const COMPLETED_EVENT: &str = "session.foreground.completed";
pub const WAIT_STOPPED_EVENT: &str = "session.foreground.wait-stopped";
const CANCELLATION_GRACE: Duration = Duration::from_secs(5);
const MAX_RESUME_INSTRUCTION_BYTES: usize = 8 * 1024;

/// One budget for a logical host operation, including every foreground wait.
///
/// Token charging remains with the wrapped policy. Tool counts are monotonic
/// per engine turn, summed across continuation turns, and never double-counted
/// when a policy hook sees the same snapshot again. Elapsed time starts once.
pub struct ForegroundTurnBudget {
    session_id: String,
    cycle_id: String,
    started_at: Instant,
    allowance: TurnAllowance,
    inner: Arc<dyn TurnBudgetPolicy>,
    tools: Mutex<BTreeMap<String, u32>>,
    stop: watch::Sender<Option<BudgetStop>>,
    input_ready: AtomicBool,
    instruction: Arc<Mutex<Option<String>>>,
}

impl ForegroundTurnBudget {
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        cycle_id: impl Into<String>,
        allowance: TurnAllowance,
        inner: Arc<dyn TurnBudgetPolicy>,
    ) -> Self {
        let (stop, _) = watch::channel(None);
        Self {
            session_id: session_id.into(),
            cycle_id: cycle_id.into(),
            started_at: Instant::now(),
            allowance,
            inner,
            tools: Mutex::new(BTreeMap::new()),
            stop,
            input_ready: AtomicBool::new(false),
            instruction: Arc::new(Mutex::new(None)),
        }
    }

    /// Also accepts a host budget decision made while no provider is running.
    /// Once stopped, only a new explicitly admitted operation gets fresh limits.
    pub fn stop(&self, reason: BudgetStop) {
        self.stop.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(reason);
                true
            }
        });
    }

    pub fn instruction(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.instruction)
    }

    /// The engine's count at a request boundary includes the last dispatched
    /// group, even when no further provider request has been made in that turn.
    pub fn observe_tool_calls(&self, turn_id: &str, dispatched: u32) {
        let mut tools = self
            .tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = tools.entry(turn_id.to_owned()).or_default();
        *count = (*count).max(dispatched);
        let total = tools.values().copied().fold(0_u32, u32::saturating_add);
        if self
            .allowance
            .max_tool_calls
            .is_some_and(|limit| total >= limit.get())
        {
            self.stop(BudgetStop {
                kind: BudgetStopKind::ToolCallBudget,
                detail: format!("the logical foreground operation dispatched {total} tool calls"),
            });
        }
    }

    #[must_use]
    pub fn current_stop(&self) -> Option<BudgetStop> {
        if let Some(stop) = self.stop.borrow().clone() {
            return Some(stop);
        }
        if let Some(limit) = self.allowance.max_duration
            && self.started_at.elapsed() >= limit
        {
            self.stop(BudgetStop {
                kind: BudgetStopKind::TimeBudget,
                detail: format!(
                    "the logical foreground operation reached its {:.3}s wall-time allowance",
                    limit.as_secs_f64()
                ),
            });
        }
        self.stop.borrow().clone()
    }

    async fn stopped(&self) -> BudgetStop {
        let mut changed = self.stop.subscribe();
        let deadline = self
            .allowance
            .max_duration
            .and_then(|duration| self.started_at.checked_add(duration));
        loop {
            if let Some(stop) = self.current_stop() {
                return stop;
            }
            tokio::select! {
                _ = changed.changed() => {}
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }
        }
    }

    fn snapshot<'a>(
        &self,
        snapshot: &TurnUsageSnapshot<'a>,
    ) -> Result<TurnUsageSnapshot<'a>, BudgetPolicyError> {
        if snapshot.session_id != self.session_id {
            return Err(BudgetPolicyError::Permanent(
                "foreground budget belongs to another session".to_owned(),
            ));
        }
        let mut tools = self
            .tools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = tools.entry(snapshot.turn_id.to_owned()).or_default();
        *count = (*count).max(snapshot.tool_calls_dispatched);
        let mut adjusted = snapshot.clone();
        adjusted.tool_calls_dispatched = tools.values().copied().fold(0_u32, u32::saturating_add);
        adjusted.elapsed_seconds = adjusted
            .elapsed_seconds
            .max(self.started_at.elapsed().as_secs());
        if let Some(stop) = self.allowance.ceiling_reached(&adjusted) {
            self.stop(stop);
        }
        Ok(adjusted)
    }

    fn decision(&self, decision: BudgetDecision) -> BudgetDecision {
        if let BudgetDecision::Stop(stop) = &decision {
            self.stop(stop.clone());
        }
        self.current_stop().map_or(decision, BudgetDecision::Stop)
    }
}

#[async_trait]
impl TurnBudgetPolicy for ForegroundTurnBudget {
    async fn before_request(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        let snapshot = self.snapshot(snapshot)?;
        if let Some(stop) = self.current_stop() {
            return Ok(BudgetDecision::Stop(stop));
        }
        Ok(self.decision(self.inner.before_request(&snapshot).await?))
    }

    async fn after_response(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        let snapshot = self.snapshot(snapshot)?;
        // A response already paid for must still be charged even if its arrival
        // coincides with the wall deadline or a previously latched stop.
        Ok(self.decision(self.inner.after_response(&snapshot).await?))
    }
}

/// Account for the final tool group even when a driver yields before its next
/// budget snapshot. One appended result corresponds to one completed dispatch
/// in the native engine; replaying the same event cannot charge it twice.
pub struct ForegroundTurnHooks {
    budget: Arc<ForegroundTurnBudget>,
    turn_id: String,
    appended: Mutex<BTreeSet<(u32, String)>>,
    boundary: Option<ForegroundHookContext>,
    failure: Mutex<Option<ForegroundError>>,
}

pub struct ForegroundHookContext {
    pub coordinator: ForegroundWaitCoordinator,
    pub interrupt: InterruptSignal,
    pub steering: InterruptSignal,
    pub events: TurnEventSender,
    pub instruction: Arc<Mutex<Option<String>>>,
}

impl ForegroundTurnHooks {
    #[must_use]
    pub fn new(budget: Arc<ForegroundTurnBudget>, turn_id: String) -> Self {
        Self {
            budget,
            turn_id,
            appended: Mutex::new(BTreeSet::new()),
            boundary: None,
            failure: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn with_foreground(mut self, context: ForegroundHookContext) -> Self {
        self.boundary = Some(context);
        self
    }

    pub fn take_failure(&self) -> Option<ForegroundError> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

#[async_trait]
impl zuno_engine::hooks::TurnHooks for ForegroundTurnHooks {
    async fn event(&self, event: &TurnEvent) -> Result<(), String> {
        match event {
            // The engine emits this only after real durable input commits.
            TurnEvent::InputConsumed { .. } => {
                self.budget.input_ready.store(true, Ordering::Release)
            }
            TurnEvent::ProviderRequestStarted { .. } => {
                self.budget.input_ready.store(false, Ordering::Release)
            }
            _ => {}
        }
        if let TurnEvent::ToolResultAppended { step, call_id, .. } = event {
            let mut appended = self
                .appended
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            appended.insert((*step, call_id.clone()));
            self.budget.observe_tool_calls(
                &self.turn_id,
                u32::try_from(appended.len()).unwrap_or(u32::MAX),
            );
        }
        Ok(())
    }

    async fn before_provider_request(
        &self,
        session_id: &str,
        turn_id: &str,
        next_step: u32,
    ) -> Result<(), String> {
        let Some(boundary) = &self.boundary else {
            return Ok(());
        };
        if session_id != self.budget.session_id || turn_id != self.turn_id {
            return Err("foreground request hook belongs to another turn".to_owned());
        }
        // A genuine steer may run once while the original command remains live.
        // Keep the permit across hydration/compaction, until a request commits.
        if self.budget.input_ready.load(Ordering::Acquire) && self.budget.current_stop().is_none() {
            return Ok(());
        }
        let scope = WaitScope {
            guard: GuardView {
                session_id,
                interrupt: &boundary.interrupt,
                steering: &boundary.steering,
                owner: None,
            },
            turn_id,
            budget: &self.budget,
        };
        let mut cancel = CancelWaitOnDrop {
            coordinator: boundary.coordinator.clone(),
            session_id: session_id.to_owned(),
            cycle_id: self.budget.cycle_id.clone(),
            armed: true,
        };
        let result = async {
            let result = boundary.coordinator.wait_scoped(&scope).await?;
            if let Some(instruction) = boundary
                .coordinator
                .resume_instruction(session_id, &result)?
            {
                *boundary
                    .instruction
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(instruction);
            }
            for publication in &result.publications {
                boundary
                    .events
                    .publish(publication.event(next_step.saturating_sub(1)))
                    .await
                    .map_err(ForegroundError::Event)?;
            }
            match result.wake {
                ForegroundWake::BudgetLimited(stop) => {
                    self.budget.stop(stop);
                    return Err(ForegroundError::BudgetStopped);
                }
                ForegroundWake::Interrupted { .. } => return Err(ForegroundError::WaitInterrupted),
                _ => {}
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                cancel.armed = false;
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                *self
                    .failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
                Err(message)
            }
        }
    }
}

pub struct ForegroundWaitScope<'a> {
    pub guard: &'a SessionRunGuard,
    pub turn_id: &'a str,
    pub budget: &'a ForegroundTurnBudget,
}

#[derive(Clone, Copy)]
struct GuardView<'a> {
    session_id: &'a str,
    interrupt: &'a InterruptSignal,
    steering: &'a InterruptSignal,
    owner: Option<&'a SessionRunGuard>,
}

impl GuardView<'_> {
    fn session_id(&self) -> &str {
        self.session_id
    }
    fn interrupt_signal(&self) -> &InterruptSignal {
        self.interrupt
    }
    fn soft_interrupt_signal(&self) -> &InterruptSignal {
        self.steering
    }
    fn interrupt_request(&self) -> Option<HardInterruptRequest> {
        self.owner.and_then(SessionRunGuard::interrupt_request)
    }
}

struct WaitScope<'a> {
    guard: GuardView<'a>,
    turn_id: &'a str,
    budget: &'a ForegroundTurnBudget,
}

impl<'a> From<&ForegroundWaitScope<'a>> for WaitScope<'a> {
    fn from(scope: &ForegroundWaitScope<'a>) -> Self {
        Self {
            guard: GuardView {
                session_id: scope.guard.session_id(),
                interrupt: scope.guard.interrupt_signal(),
                steering: scope.guard.soft_interrupt_signal(),
                owner: Some(scope.guard),
            },
            turn_id: scope.turn_id,
            budget: scope.budget,
        }
    }
}

struct CancelWaitOnDrop {
    coordinator: ForegroundWaitCoordinator,
    session_id: String,
    cycle_id: String,
    armed: bool,
}

impl Drop for CancelWaitOnDrop {
    fn drop(&mut self) {
        if self.armed {
            for execution in self
                .coordinator
                .processes
                .foreground_for_session(&self.session_id)
            {
                if execution.info.cycle_id.as_deref() == Some(self.cycle_id.as_str()) {
                    let _ = self.coordinator.processes.cancel(&execution.info.id);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForegroundWake {
    /// No unconsumed foreground result belongs to this operation.
    Idle,
    /// Real terminal results were published. Resume once from those facts.
    Completed,
    /// Leave the durable FIFO and soft signal for the engine's next safe point.
    Steering,
    Interrupted {
        request: Option<HardInterruptRequest>,
    },
    BudgetLimited(BudgetStop),
}

#[derive(Debug)]
pub struct ForegroundWaitResult {
    pub wake: ForegroundWake,
    pub publications: Vec<ForegroundPublication>,
    /// Cancellation observation may expire; these handles remain inspectable
    /// and must never be replaced by rerunning their commands.
    pub pending: Vec<BackgroundExecutionId>,
}

impl ForegroundWaitResult {
    fn idle() -> Self {
        Self {
            wake: ForegroundWake::Idle,
            publications: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// A host may attach this event-backed text to its existing recovery prompt.
    /// It survives compaction even when the original tool part is outside the
    /// retained provider history. Actual prompt assembly is still logged by the engine.
    fn resume_instruction(&self) -> Result<Option<String>, serde_json::Error> {
        if self.publications.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "Recorded foreground terminal observations for the current work cycle:\n{}\n\
             These are final results of the original Shell calls, not permission to rerun them. \
             Continue only from the recorded results. Unknown outcomes require authoritative \
             state inspection before replay; remoteObserver exits require an authoritative \
             remote-state recheck using the original stable identifier.",
            serde_json::to_string(
                &self
                    .publications
                    .iter()
                    .map(|publication| json!({
                        "eventID": publication.event_id,
                        "executionID": publication.execution_id,
                        "callID": publication.original_call_id,
                        "cycleID": publication.cycle_id,
                        "verification": publication.output.metadata.get("verification"),
                        "outputPaths": publication.output.output_paths(),
                    }))
                    .collect::<Vec<_>>()
            )?
        )))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForegroundPublication {
    pub event_id: String,
    pub source_key: String,
    pub execution_id: BackgroundExecutionId,
    pub original_call_id: String,
    pub part_id: String,
    pub cycle_id: String,
    pub output: ToolOutput,
}

impl ForegroundPublication {
    pub fn event(&self, step: u32) -> TurnEvent {
        TurnEvent::ToolDispatchCompleted {
            step,
            call_id: self.original_call_id.clone(),
            display_name: "shell".to_owned(),
            name: "shell".to_owned(),
            title: self.output.title.clone(),
            output: self.output.output.clone(),
            diff: zuno_engine::r#loop::ToolDiff::from_output(&self.output),
            written_paths: self
                .output
                .written_paths()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            is_error: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ForegroundError {
    #[error(transparent)]
    Process(#[from] BackgroundExecutionError),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Output(#[from] ToolError),
    #[error("foreground wait scope does not match the live session lease")]
    ScopeMismatch,
    #[error("foreground publication worker did not settle: {0}")]
    PublicationWorker(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Event(TurnError),
    #[error("the foreground operation exhausted its logical budget")]
    BudgetStopped,
    #[error("the foreground operation was interrupted")]
    WaitInterrupted,
}

impl ForegroundError {
    /// Process loss and failed evidence publication never request a model/tool
    /// replay. SQLite retains its existing typed retry classification.
    pub fn into_turn_error(self) -> TurnError {
        match self {
            Self::Event(error) => error,
            Self::Database(error) => TurnError::Database(error),
            Self::Process(error) => TurnError::BudgetLimited {
                kind: BudgetStopKind::UncertainSideEffect,
                detail: format!("foreground process observation is uncertain: {error}"),
            },
            Self::Output(ToolError::Uncertain { source, .. }) => TurnError::BudgetLimited {
                kind: BudgetStopKind::UncertainSideEffect,
                detail: format!("foreground output is uncertain: {source}"),
            },
            other => TurnError::Hook(other.to_string()),
        }
    }
}

/// Uses the existing process owner. No provider, agent or detached watcher is spawned.
#[derive(Clone)]
pub struct ForegroundWaitCoordinator {
    processes: Arc<zuno_pty::BackgroundExecutionService>,
    database: Arc<zuno_db::Pool>,
    output_store: ToolOutputStore,
    output_limits: OutputLimits,
    /// Supply the existing `verification_ledger::receipt_id` function so a
    /// terminal result preserves the citation assigned to its original call.
    receipt_id: fn(&str, &str) -> String,
    cancellation_grace: Duration,
}

impl ForegroundWaitCoordinator {
    #[must_use]
    pub fn new(
        processes: Arc<zuno_pty::BackgroundExecutionService>,
        database: Arc<zuno_db::Pool>,
        output_store: ToolOutputStore,
        output_limits: OutputLimits,
        receipt_id: fn(&str, &str) -> String,
    ) -> Self {
        Self {
            processes,
            database,
            output_store,
            output_limits,
            receipt_id,
            cancellation_grace: CANCELLATION_GRACE,
        }
    }

    /// Bound the developer-side continuation manifest. Command output stays in
    /// its native tool part/artifact; it is never pasted into developer instructions.
    pub fn resume_instruction(
        &self,
        session_id: &str,
        result: &ForegroundWaitResult,
    ) -> Result<Option<String>, ForegroundError> {
        let Some(instruction) = result
            .resume_instruction()
            .map_err(|source| DbError::Decode {
                table: "foreground_manifest".to_owned(),
                source,
            })?
        else {
            return Ok(None);
        };
        if instruction.len() <= MAX_RESUME_INSTRUCTION_BYTES {
            return Ok(Some(instruction));
        }
        let artifact = self
            .output_store
            .persist("foreground", session_id, &instruction)?;
        let instruction = format!(
            "{} foreground executions produced durably recorded terminal results. \
             Read their receipt manifest with `bg` action=\"artifact\", outputPath={}, \
             cursor=0 and a bounded limit. Original commands must not be rerun to recover output.",
            result.publications.len(),
            serde_json::to_string(&artifact.path.to_string_lossy()).map_err(|source| {
                DbError::Decode {
                    table: "foreground_manifest".to_owned(),
                    source,
                }
            })?,
        );
        if instruction.len() > MAX_RESUME_INSTRUCTION_BYTES {
            return Err(ForegroundError::Database(conflict(
                "foreground_manifest",
                session_id,
                "artifact locator exceeds the context budget",
            )));
        }
        Ok(Some(instruction))
    }

    pub async fn stop(
        &self,
        scope: &ForegroundWaitScope<'_>,
        stop: Option<BudgetStop>,
    ) -> Result<ForegroundWaitResult, ForegroundError> {
        self.validate_scope(scope)?;
        let wake = match stop {
            Some(stop) => {
                scope.budget.stop(stop.clone());
                ForegroundWake::BudgetLimited(scope.budget.current_stop().unwrap_or(stop))
            }
            None => ForegroundWake::Interrupted {
                request: scope.guard.interrupt_request(),
            },
        };
        if self.pending(&WaitScope::from(scope)).is_empty() {
            return Ok(ForegroundWaitResult {
                wake,
                publications: Vec::new(),
                pending: Vec::new(),
            });
        }
        self.cancel_and_drain(&WaitScope::from(scope), wake, Vec::new())
            .await
    }

    /// Wait on process state and the existing hard/soft control signals.
    ///
    /// There is no observation timer and no model polling. Each process retains
    /// its original hard ceiling. The original turn id remains steerable while
    /// the same outer session guard keeps the logical host operation busy.
    pub async fn wait(
        &self,
        scope: &ForegroundWaitScope<'_>,
    ) -> Result<ForegroundWaitResult, ForegroundError> {
        self.validate_scope(scope)?;
        if self.pending(&WaitScope::from(scope)).is_empty() {
            return Ok(ForegroundWaitResult::idle());
        }
        let _identity = scope
            .guard
            .mark_turn_started(scope.turn_id)
            .ok_or(ForegroundError::ScopeMismatch)?;
        self.wait_active(scope).await
    }

    /// Before-provider boundary: the engine already owns the turn identity.
    /// Do not create a nested identity guard whose drop would clear that binding.
    pub async fn wait_active(
        &self,
        scope: &ForegroundWaitScope<'_>,
    ) -> Result<ForegroundWaitResult, ForegroundError> {
        self.validate_scope(scope)?;
        self.wait_scoped(&WaitScope::from(scope)).await
    }

    async fn wait_scoped(
        &self,
        scope: &WaitScope<'_>,
    ) -> Result<ForegroundWaitResult, ForegroundError> {
        let mut publications = Vec::new();
        loop {
            if scope.guard.interrupt_signal().is_set() {
                return self
                    .cancel_and_drain(
                        scope,
                        ForegroundWake::Interrupted {
                            request: scope.guard.interrupt_request(),
                        },
                        publications,
                    )
                    .await;
            }
            if let Some(stop) = scope.budget.current_stop() {
                return self
                    .cancel_and_drain(scope, ForegroundWake::BudgetLimited(stop), publications)
                    .await;
            }
            if scope.guard.soft_interrupt_signal().is_set() {
                return Ok(ForegroundWaitResult {
                    wake: ForegroundWake::Steering,
                    publications,
                    pending: self.pending_ids(scope),
                });
            }
            let pending = self.pending(scope);
            if pending.is_empty() {
                return Ok(ForegroundWaitResult {
                    wake: if publications.is_empty() {
                        ForegroundWake::Idle
                    } else {
                        ForegroundWake::Completed
                    },
                    publications,
                    pending: Vec::new(),
                });
            }
            if let Some(execution) = pending.iter().find(|item| item.info.status.is_terminal()) {
                if let Some(publication) = self
                    .publish(execution.clone(), scope.turn_id.to_owned())
                    .await?
                {
                    publications.push(publication);
                }
                // Recheck controls and budget between durable publications.
                continue;
            }
            let mut terminal = FuturesUnordered::new();
            for execution in pending {
                let processes = Arc::clone(&self.processes);
                terminal.push(async move { processes.wait(&execution.info.id, None).await });
            }
            tokio::select! {
                biased;
                () = scope.guard.interrupt_signal().notified() => {}
                _ = scope.budget.stopped() => {}
                () = scope.guard.soft_interrupt_signal().notified() => {}
                result = terminal.next() => {
                    if let Some(result) = result {
                        result?;
                    }
                }
            }
        }
    }

    fn validate_scope(&self, scope: &ForegroundWaitScope<'_>) -> Result<(), ForegroundError> {
        if scope.guard.session_id() != scope.budget.session_id
            || scope.turn_id.is_empty()
            || scope.budget.cycle_id.is_empty()
        {
            Err(ForegroundError::ScopeMismatch)
        } else {
            Ok(())
        }
    }

    fn pending(&self, scope: &WaitScope<'_>) -> Vec<ForegroundExecution> {
        self.processes
            .foreground_for_session(scope.guard.session_id())
            .into_iter()
            .filter(|execution| execution.info.cycle_id.as_deref() == Some(&scope.budget.cycle_id))
            .collect()
    }

    fn pending_ids(&self, scope: &WaitScope<'_>) -> Vec<BackgroundExecutionId> {
        self.pending(scope)
            .into_iter()
            .map(|execution| execution.info.id)
            .collect()
    }

    async fn cancel_and_drain(
        &self,
        scope: &WaitScope<'_>,
        wake: ForegroundWake,
        mut publications: Vec<ForegroundPublication>,
    ) -> Result<ForegroundWaitResult, ForegroundError> {
        let pending = self.pending(scope);
        let mut terminal = FuturesUnordered::new();
        // Always request every cancellation, even when one handle is lost or foreign.
        for execution in pending {
            if self.processes.cancel(&execution.info.id).is_ok() {
                let processes = Arc::clone(&self.processes);
                terminal.push(async move { processes.wait(&execution.info.id, None).await });
            }
        }
        let deadline = Instant::now() + self.cancellation_grace;
        loop {
            for ready in self
                .pending(scope)
                .into_iter()
                .filter(|execution| execution.info.status.is_terminal())
            {
                if let Some(publication) = self.publish(ready, scope.turn_id.to_owned()).await? {
                    publications.push(publication);
                }
            }
            if terminal.is_empty() {
                break;
            }
            match tokio::time::timeout_at(deadline, terminal.next()).await {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        let pending = self.pending_ids(scope);
        let reason = match &wake {
            ForegroundWake::BudgetLimited(stop) => {
                json!({"kind":"turn_budget","code":stop.kind.code(),"detail":stop.detail})
            }
            ForegroundWake::Interrupted { request } => {
                json!({"kind":"interrupted","request":request})
            }
            _ => unreachable!("only explicit stopping paths cancel"),
        };
        let properties = Map::from_iter([
            ("cycleId".to_owned(), json!(scope.budget.cycle_id)),
            ("turnId".to_owned(), json!(scope.turn_id)),
            ("reason".to_owned(), reason),
            ("pending".to_owned(), json!(pending)),
        ]);
        zuno_db::event_log::SessionEventLog::new(Arc::clone(&self.database)).append(
            scope.guard.session_id(),
            NewSessionEvent::new(WAIT_STOPPED_EVENT, properties)?,
        )?;
        Ok(ForegroundWaitResult {
            wake,
            publications,
            pending,
        })
    }

    async fn publish(
        &self,
        execution: ForegroundExecution,
        observed_turn: String,
    ) -> Result<Option<ForegroundPublication>, ForegroundError> {
        let coordinator = self.clone();
        tokio::task::spawn_blocking(move || coordinator.publish_blocking(execution, &observed_turn))
            .await?
    }

    fn publish_blocking(
        &self,
        execution: ForegroundExecution,
        observed_turn: &str,
    ) -> Result<Option<ForegroundPublication>, ForegroundError> {
        let completion = self
            .processes
            .foreground_completion(&execution.info.id, &execution.info.session_id)?;
        let mut output = zuno_tools::shell::foreground_completion_output(
            &completion,
            self.output_store.clone(),
            self.output_limits,
        )?;
        // Preserve a tool-readable source even if compaction later excludes the
        // original part or process retention evicts the acknowledged handle.
        if output.output_paths().is_empty() {
            let artifact = self.output_store.persist_bytes(
                "shell",
                &execution.info.session_id,
                &completion.output,
            )?;
            output.record_output_path(&artifact.path);
        }
        let receipt = VerificationReceipt::from_metadata(&output.metadata)
            .map_err(|source| DbError::Decode {
                table: "foreground_receipt".to_owned(),
                source,
            })?
            .ok_or_else(|| {
                conflict(
                    "verification_receipt",
                    &execution.context.call_id,
                    "foreground result has no receipt",
                )
            })?;
        let envelope =
            zuno_tools::bg::foreground_completion_envelope(&completion.execution, &receipt);
        let id = (self.receipt_id)(&execution.info.session_id, &execution.context.call_id);
        output
            .metadata
            .get_mut("verification")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                conflict(
                    "verification_receipt",
                    &id,
                    "receipt metadata is not an object",
                )
            })?
            .insert("receiptId".to_owned(), json!(id));
        output
            .output
            .push_str(&format!("\n\nVerification receipt: `{id}`."));
        let now = zuno_db::message::now_millis();
        let publication = self.database.transaction(|transaction| {
            let current =
                zuno_db::session_execution::read_in(transaction, &execution.info.session_id)?;
            if current.as_ref().and_then(|state| state.cycle_id.as_ref())
                != execution.info.cycle_id.as_ref()
            {
                return Err(conflict(
                    "session_execution",
                    &execution.info.session_id,
                    "foreground result belongs to another work cycle",
                ));
            }
            let delivered = publish_in(transaction, envelope.clone(), now)?;
            if delivered.owner == Some(CompletionOwner::Callback) {
                return Err(conflict(
                    "completion_delivery",
                    &envelope.source_key,
                    "foreground result was incorrectly assigned to a detached callback",
                ));
            }
            if claim_inline_in(transaction, &envelope.source_key, now)?.is_none() {
                return Ok(None);
            }
            let part_id =
                update_original_part(transaction, &completion.execution, &output, &receipt, now)?;
            zuno_db::verification::record(
                transaction,
                &stored_receipt(&id, &completion.execution, &receipt, now),
            )?;
            let properties = Map::from_iter([
                ("sourceKey".to_owned(), json!(envelope.source_key)),
                ("executionId".to_owned(), json!(execution.info.id)),
                (
                    "originalCallId".to_owned(),
                    json!(execution.context.call_id),
                ),
                ("partId".to_owned(), json!(part_id)),
                ("cycleId".to_owned(), json!(execution.info.cycle_id)),
                ("observedTurnId".to_owned(), json!(observed_turn)),
                (
                    "output".to_owned(),
                    serde_json::to_value(&output).map_err(|source| DbError::Decode {
                        table: "foreground_output".to_owned(),
                        source,
                    })?,
                ),
            ]);
            let event = append_in(
                transaction,
                &execution.info.session_id,
                NewSessionEvent::new(COMPLETED_EVENT, properties)?,
            )?;
            Ok(Some(ForegroundPublication {
                event_id: event.id,
                source_key: envelope.source_key.clone(),
                execution_id: execution.info.id.clone(),
                original_call_id: execution.context.call_id.clone(),
                part_id,
                cycle_id: execution.info.cycle_id.clone().unwrap_or_default(),
                output: output.clone(),
            }))
        })?;
        // A previously committed inline receipt also repairs a crash between
        // the transaction and this acknowledgement, without republishing output.
        self.processes
            .consume_foreground(&execution.info.id, &execution.info.session_id)?;
        Ok(publication)
    }
}

fn conflict(table: &str, id: &str, detail: &str) -> DbError {
    DbError::Conflict {
        table: table.to_owned(),
        id: id.to_owned(),
        detail: detail.to_owned(),
    }
}

fn update_original_part(
    transaction: &Transaction<'_>,
    execution: &ForegroundExecution,
    output: &ToolOutput,
    receipt: &VerificationReceipt,
    now: i64,
) -> Result<String, DbError> {
    let mut query = transaction
        .prepare(
            "SELECT id FROM part WHERE session_id=?1
         AND json_extract(data,'$.type')='tool'
         AND json_extract(data,'$.tool')='shell'
         AND json_extract(data,'$.callID')=?2
         AND json_extract(data,'$.state.metadata.task_id')=?3 LIMIT 2",
        )
        .map_err(zuno_db::open::map_error)?;
    let rows = query
        .query_map(
            params![
                execution.info.session_id,
                execution.context.call_id,
                execution.info.id.as_str()
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(zuno_db::open::map_error)?;
    let ids = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(zuno_db::open::map_error)?;
    if ids.len() != 1 {
        return Err(conflict(
            "part",
            &execution.context.call_id,
            "foreground completion needs exactly one original Shell part",
        ));
    }
    let store = MessageStore::new(transaction);
    let mut part: PartRecord = store.part(&ids[0])?;
    let state = part
        .data
        .get_mut("state")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| conflict("part", &ids[0], "original Shell part has no state"))?;
    state.insert("status".to_owned(), json!("completed"));
    state.insert("title".to_owned(), json!(output.title));
    state.insert("output".to_owned(), json!(output.output));
    state.insert("metadata".to_owned(), json!(output.metadata));
    state.insert("attachments".to_owned(), json!(output.attachments));
    state.remove("error");
    let uncertain = receipt.outcome == ReceiptOutcome::Unknown
        && !execution.info.purpose.requires_authoritative_refresh();
    if uncertain {
        state.insert("outcome".to_owned(), json!("uncertain"));
        state.insert(
            "uncertain".to_owned(),
            json!({
                "tool":"shell", "callID":execution.context.call_id,
                "appliedPaths": output.written_paths(),
                "cause": if execution.info.status == BackgroundExecutionStatus::Cancelled {
                    UncertainCause::Interrupted.as_str()
                } else { UncertainCause::LostOutcome.as_str() },
                "observedAtMs":now,
            }),
        );
    } else {
        state.remove("outcome");
        state.remove("uncertain");
    }
    if let Some(time) = state.get_mut("time").and_then(Value::as_object_mut) {
        time.insert(
            "end".to_owned(),
            json!(execution.info.time_completed.unwrap_or(now)),
        );
    }
    store.put_part_at(&part, now)?;
    Ok(part.id)
}

fn stored_receipt(
    id: &str,
    execution: &ForegroundExecution,
    receipt: &VerificationReceipt,
    now: i64,
) -> zuno_db::verification::NewVerificationReceipt {
    zuno_db::verification::NewVerificationReceipt {
        id: id.to_owned(),
        session_id: execution.info.session_id.clone(),
        turn_id: None,
        tool_call_id: execution.context.call_id.clone(),
        tool_id: "shell".to_owned(),
        summary: receipt.summary.clone(),
        workdir: receipt.workdir.clone(),
        exit_code: receipt.exit_code,
        exit_authority: match receipt.exit_authority {
            zuno_tool::ExitAuthority::Authoritative => {
                zuno_db::verification::ExitAuthority::Authoritative
            }
            zuno_tool::ExitAuthority::Derived => zuno_db::verification::ExitAuthority::Derived,
            zuno_tool::ExitAuthority::Absent => zuno_db::verification::ExitAuthority::Absent,
        },
        outcome: match receipt.outcome {
            ReceiptOutcome::Passed => zuno_db::verification::ReceiptOutcome::Passed,
            ReceiptOutcome::Failed => zuno_db::verification::ReceiptOutcome::Failed,
            ReceiptOutcome::Unknown => zuno_db::verification::ReceiptOutcome::Unknown,
        },
        git_head: receipt.git_head.clone(),
        output_digest: receipt.output_digest.clone(),
        detail: receipt.detail.clone(),
        time_created: now,
    }
}
