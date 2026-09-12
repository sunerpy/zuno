//! In-process run state and control for session turns.
//!
//! The registry is intentionally not persisted. A guard is the exclusive lease for
//! one session's live turn, and dropping it returns the session to idle. Control
//! handles retain a session id plus the registry. User cancellation must carry a
//! turn/input identity or a captured cancellation target through to the signal
//! boundary; a session-scoped handle alone cannot identify a delayed user's intent.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::interrupt::{
    HardInterruptRequest, HardInterruptSignal, InterruptSignal, SoftInterruptMessage,
};

/// The process-local state exposed to CLI, TUI, HTTP, and ACP surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Idle,
    Busy,
}

/// Where a hard interrupt request was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortDisposition {
    /// The current live turn's signal was fired.
    Active,
    /// No guard was live, so the next accepted turn will start interrupted.
    ArmedNext,
}

/// The action a turn loop takes after injecting queued messages at a safe point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftInterruptAction {
    Continue,
    SkipRemainingTools,
}

/// Soft interruptions removed from the active turn's queue at one safe point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftInterruptDelivery {
    pub messages: Vec<SoftInterruptMessage>,
    pub action: SoftInterruptAction,
}

impl SoftInterruptDelivery {
    fn empty() -> Self {
        Self {
            messages: Vec::new(),
            action: SoftInterruptAction::Continue,
        }
    }
}

/// Returned when a caller attempts to start a second loop for one session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("session `{session_id}` already has an active turn")]
pub struct SessionBusy {
    session_id: String,
}

impl SessionBusy {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Returned when a soft interruption has no live turn queue to target.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("session `{session_id}` has no active turn")]
pub struct SessionNotActive {
    session_id: String,
}

/// Why an exact operation could not target the caller's expected engine turn.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpectedTurnError {
    #[error("session `{session_id}` has no active turn")]
    NoActiveTurn { session_id: String },
    #[error("session `{session_id}` has an active lease that has not entered a steerable turn")]
    ActiveTurnNotIdentified { session_id: String },
    #[error("session `{session_id}` turn `{turn_id}` is already finishing")]
    Closing { session_id: String, turn_id: String },
    #[error(
        "session `{session_id}` is running turn `{actual_turn_id}`, not expected turn `{expected_turn_id}`"
    )]
    Mismatch {
        session_id: String,
        expected_turn_id: String,
        actual_turn_id: String,
    },
}

impl SessionNotActive {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Shared process-local registry for every session turn.
#[derive(Debug, Clone)]
pub struct SessionRunRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Debug)]
struct RegistryInner {
    next_token: AtomicU64,
    state: Mutex<RegistryState>,
    idle: Notify,
}

#[derive(Debug, Default)]
struct RegistryState {
    active: HashMap<String, ActiveSession>,
    recovering: HashSet<String>,
    pending_interrupts: HashMap<String, HardInterruptRequest>,
    diagnostic_notices: HashMap<String, HashSet<DiagnosticNoticeKey>>,
}

/// Identity of one process-local diagnostic notice.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiagnosticNoticeKey {
    pub context_epoch: i64,
    pub tool_name: String,
    pub stored_identity_sha256: String,
    pub current_identity_sha256: String,
}

#[derive(Debug)]
struct ActiveSession {
    token: u64,
    identity_epoch: u64,
    turn_id: Option<String>,
    input_id: Option<String>,
    accepting_input: bool,
    interrupt: HardInterruptSignal,
    soft_interrupt: InterruptSignal,
    soft_interrupts: VecDeque<SoftInterruptMessage>,
}

impl SessionRunRegistry {
    /// Creates an empty, process-local registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                next_token: AtomicU64::new(1),
                state: Mutex::new(RegistryState::default()),
                idle: Notify::new(),
            }),
        }
    }

    /// Acquires the exclusive live-turn lease for `session_id`.
    ///
    /// A competing prompt is rejected with [`SessionBusy`]. Rejecting instead of
    /// silently coalescing prevents a caller's distinct prompt work from being lost.
    pub fn begin_turn(
        &self,
        session_id: impl Into<String>,
    ) -> Result<SessionRunGuard, SessionBusy> {
        let session_id = session_id.into();
        let mut state = self.lock_state();
        if state.active.contains_key(&session_id) || state.recovering.contains(&session_id) {
            return Err(SessionBusy { session_id });
        }

        let token = self.inner.next_token.fetch_add(1, Ordering::Relaxed);
        let interrupt = HardInterruptSignal::new();
        let soft_interrupt = InterruptSignal::new();
        if let Some(request) = state.pending_interrupts.remove(&session_id) {
            let _accepted = interrupt.request(request);
        }
        state.active.insert(
            session_id.clone(),
            ActiveSession {
                token,
                identity_epoch: 0,
                turn_id: None,
                input_id: None,
                accepting_input: true,
                interrupt: interrupt.clone(),
                soft_interrupt: soft_interrupt.clone(),
                soft_interrupts: VecDeque::new(),
            },
        );

        Ok(SessionRunGuard {
            registry: self.clone(),
            session_id,
            token,
            interrupt,
            soft_interrupt,
        })
    }

    /// Reserve an idle session while repairing orphaned durable input claims.
    ///
    /// This lease does not create a turn or consume a pending hard interrupt. It
    /// only excludes a concurrent turn long enough for the caller to return a
    /// process-orphaned `promoted` input to its admitted lane.
    pub fn begin_recovery(
        &self,
        session_id: impl Into<String>,
    ) -> Result<SessionRecoveryGuard, SessionBusy> {
        let session_id = session_id.into();
        let mut state = self.lock_state();
        if state.active.contains_key(&session_id) || !state.recovering.insert(session_id.clone()) {
            return Err(SessionBusy { session_id });
        }
        Ok(SessionRecoveryGuard {
            registry: self.clone(),
            session_id,
        })
    }

    /// Creates a reusable session control handle.
    ///
    /// The handle deliberately captures no interrupt signal. User cancellation
    /// passes an expected turn/input id or a captured target to its exact method;
    /// retaining this session handle does not retain a turn's identity.
    #[must_use]
    pub fn control(&self, session_id: impl Into<String>) -> SessionControl {
        SessionControl {
            registry: self.clone(),
            session_id: session_id.into(),
        }
    }

    /// Returns the current process-local status for one session.
    #[must_use]
    pub fn status(&self, session_id: &str) -> SessionStatus {
        let state = self.lock_state();
        if state.active.contains_key(session_id) || state.recovering.contains(session_id) {
            SessionStatus::Busy
        } else {
            SessionStatus::Idle
        }
    }

    /// Current engine turn identity, once the live lease has entered `run_turn`.
    #[must_use]
    pub fn active_turn_id(&self, session_id: &str) -> Option<String> {
        self.lock_state()
            .active
            .get(session_id)
            .and_then(|active| active.turn_id.clone())
    }

    /// Input selected by this lease's native driver, including pre-model setup.
    #[must_use]
    pub fn active_input_id(&self, session_id: &str) -> Option<String> {
        self.lock_state()
            .active
            .get(session_id)
            .and_then(|active| active.input_id.clone())
    }

    /// Capture the current lease and identity generation once, before awaiting.
    ///
    /// An unidentified lease can be captured, but any subsequent turn or input
    /// binding invalidates that snapshot. No snapshot is manufactured while idle.
    #[must_use]
    pub fn cancel_target(&self, session_id: &str) -> Option<SessionCancelTarget> {
        let state = self.lock_state();
        let active = state.active.get(session_id)?;
        Some(SessionCancelTarget {
            registry: Arc::clone(&self.inner),
            session_id: session_id.to_owned(),
            token: active.token,
            identity_epoch: active.identity_epoch,
        })
    }

    /// Check a captured identity and fire its signal under the same registry lock.
    /// A stale target is a no-op and never arms a future turn.
    pub fn abort_target(
        &self,
        target: &SessionCancelTarget,
        request: HardInterruptRequest,
    ) -> bool {
        if !Arc::ptr_eq(&self.inner, &target.registry) {
            return false;
        }
        let state = self.lock_state();
        state.active.get(&target.session_id).is_some_and(|active| {
            if active.token != target.token || active.identity_epoch != target.identity_epoch {
                return false;
            }
            let _accepted = active.interrupt.request(request);
            true
        })
    }

    /// Cancel exactly the named live engine turn, including its closing phase.
    ///
    /// Identity validation and the interrupt request are one atomic boundary.
    /// Callers must retain their observed turn id across asynchronous work.
    pub fn abort_turn(
        &self,
        session_id: &str,
        expected_turn_id: &str,
        request: HardInterruptRequest,
    ) -> Result<(), ExpectedTurnError> {
        let state = self.lock_state();
        let active =
            state
                .active
                .get(session_id)
                .ok_or_else(|| ExpectedTurnError::NoActiveTurn {
                    session_id: session_id.to_owned(),
                })?;
        let actual_turn_id = active.turn_id.as_deref().ok_or_else(|| {
            ExpectedTurnError::ActiveTurnNotIdentified {
                session_id: session_id.to_owned(),
            }
        })?;
        if actual_turn_id != expected_turn_id {
            return Err(ExpectedTurnError::Mismatch {
                session_id: session_id.to_owned(),
                expected_turn_id: expected_turn_id.to_owned(),
                actual_turn_id: actual_turn_id.to_owned(),
            });
        }
        let _accepted = active.interrupt.request(request);
        Ok(())
    }

    /// Cancel only the input selected by the native driver, never another input
    /// sharing its session or an already-delivered steer owned by a different turn.
    pub fn abort_input(
        &self,
        session_id: &str,
        input_id: &str,
        request: HardInterruptRequest,
    ) -> bool {
        let state = self.lock_state();
        state.active.get(session_id).is_some_and(|active| {
            if active.input_id.as_deref() != Some(input_id) {
                return false;
            }
            let _accepted = active.interrupt.request(request);
            true
        })
    }

    /// Returns a stable snapshot of every process-local active session id.
    #[must_use]
    pub fn active_sessions(&self) -> BTreeSet<String> {
        self.lock_state().active.keys().cloned().collect()
    }

    /// Admit one session-scoped diagnostic identity exactly once in this process.
    ///
    /// Context compaction or either declaration identity changing creates a new key
    /// and therefore a new diagnostic. Process restart intentionally resets the set.
    pub fn admit_diagnostic_notice(&self, session_id: &str, key: DiagnosticNoticeKey) -> bool {
        self.lock_state()
            .diagnostic_notices
            .entry(session_id.to_owned())
            .or_default()
            .insert(key)
    }

    /// Wait until `session_id` has no live turn without polling.
    ///
    /// The waiter is registered before the status re-check, so a guard dropped
    /// between observation and suspension cannot lose its wake-up.
    ///
    /// The wait is deliberately unbounded, and that is the production contract:
    /// `wake`, an HTTP session wait, and an ACP prompt handoff must keep waiting
    /// for as long as a real turn legitimately runs, so no ceiling here could be
    /// both safe for a long turn and useful as a failure signal. A caller that
    /// must fail rather than hang — a test driving a fake executor, for instance —
    /// owns its own bound: wrap this call in `tokio::time::timeout` and report
    /// [`Self::status`] and [`Self::active_sessions`] as the diagnostic.
    pub async fn wait_until_idle(&self, session_id: &str) {
        loop {
            let mut notified = std::pin::pin!(self.inner.idle.notified());
            notified.as_mut().enable();
            if self.status(session_id) == SessionStatus::Idle {
                return;
            }
            notified.await;
        }
    }

    /// Fires the live turn's interrupt signal or arms the next accepted turn.
    ///
    /// The registry lock makes the handoff linearizable: a cancellation arriving after
    /// one guard is removed but before the accepted follow-up acquires the next guard is
    /// retained and that next guard starts interrupted.
    pub fn abort(&self, session_id: &str, request: HardInterruptRequest) -> AbortDisposition {
        let mut state = self.lock_state();
        if let Some(active) = state.active.get(session_id) {
            let _accepted = active.interrupt.request(request);
            AbortDisposition::Active
        } else {
            state
                .pending_interrupts
                .entry(session_id.to_owned())
                .or_insert(request);
            AbortDisposition::ArmedNext
        }
    }

    /// Abort only a currently live turn without arming a future one.
    ///
    /// Lifecycle teardown uses this variant: closing an already-idle surface must
    /// not poison the next process-local mount of the same durable session.
    pub fn abort_active(&self, session_id: &str, request: HardInterruptRequest) -> bool {
        let state = self.lock_state();
        state.active.get(session_id).is_some_and(|active| {
            let _accepted = active.interrupt.request(request);
            true
        })
    }

    /// Removes an interrupt armed for a future turn without touching a live turn.
    ///
    /// A surface that is permanently tearing down a session uses this after its
    /// prompt handoff has settled. It prevents a cancellation accepted during
    /// that handoff from leaking into a later, independent mount of the same
    /// durable session.
    pub fn clear_pending_abort(&self, session_id: &str) -> bool {
        self.lock_state()
            .pending_interrupts
            .remove(session_id)
            .is_some()
    }

    /// Queues a message for the live turn's next safe point without firing abort.
    pub fn queue_soft_interrupt(
        &self,
        session_id: &str,
        message: SoftInterruptMessage,
    ) -> Result<(), SessionNotActive> {
        let mut state = self.lock_state();
        let active = state
            .active
            .get_mut(session_id)
            .ok_or_else(|| SessionNotActive {
                session_id: session_id.to_owned(),
            })?;
        if !active.accepting_input {
            return Err(SessionNotActive {
                session_id: session_id.to_owned(),
            });
        }
        active.soft_interrupts.push_back(message);
        active.soft_interrupt.fire();
        Ok(())
    }

    /// Queue a soft interruption only while the expected engine turn still owns the lease.
    pub fn queue_soft_interrupt_for_turn(
        &self,
        session_id: &str,
        expected_turn_id: &str,
        message: SoftInterruptMessage,
    ) -> Result<(), ExpectedTurnError> {
        let mut state = self.lock_state();
        let active =
            state
                .active
                .get_mut(session_id)
                .ok_or_else(|| ExpectedTurnError::NoActiveTurn {
                    session_id: session_id.to_owned(),
                })?;
        let actual_turn_id = active.turn_id.as_deref().ok_or_else(|| {
            ExpectedTurnError::ActiveTurnNotIdentified {
                session_id: session_id.to_owned(),
            }
        })?;
        if actual_turn_id != expected_turn_id {
            return Err(ExpectedTurnError::Mismatch {
                session_id: session_id.to_owned(),
                expected_turn_id: expected_turn_id.to_owned(),
                actual_turn_id: actual_turn_id.to_owned(),
            });
        }
        if !active.accepting_input {
            return Err(ExpectedTurnError::Closing {
                session_id: session_id.to_owned(),
                turn_id: actual_turn_id.to_owned(),
            });
        }
        active.soft_interrupts.push_back(message);
        active.soft_interrupt.fire();
        Ok(())
    }

    fn set_turn_id(&self, session_id: &str, token: u64, turn_id: &str) -> bool {
        let mut state = self.lock_state();
        let Some(active) = state.active.get_mut(session_id) else {
            return false;
        };
        if active.token != token {
            return false;
        }
        active.turn_id = Some(turn_id.to_owned());
        active.identity_epoch += 1;
        active.accepting_input = true;
        true
    }

    fn set_input_id(&self, session_id: &str, token: u64, input_id: &str) -> bool {
        let mut state = self.lock_state();
        let Some(active) = state.active.get_mut(session_id) else {
            return false;
        };
        if active.token != token {
            return false;
        }
        active.input_id = Some(input_id.to_owned());
        active.identity_epoch += 1;
        true
    }

    fn clear_input_id(&self, session_id: &str, token: u64, input_id: &str) {
        let mut state = self.lock_state();
        if let Some(active) = state.active.get_mut(session_id)
            && active.token == token
            && active.input_id.as_deref() == Some(input_id)
        {
            active.input_id = None;
            active.identity_epoch += 1;
        }
    }

    fn try_finish_inputs(&self, session_id: &str, token: u64) -> bool {
        let mut state = self.lock_state();
        let Some(active) = state.active.get_mut(session_id) else {
            return true;
        };
        if active.token != token {
            return true;
        }
        if !active.soft_interrupts.is_empty() {
            return false;
        }
        active.accepting_input = false;
        true
    }

    fn clear_turn_id(&self, session_id: &str, token: u64, turn_id: &str) {
        let mut state = self.lock_state();
        let Some(active) = state.active.get_mut(session_id) else {
            return;
        };
        if active.token == token && active.turn_id.as_deref() == Some(turn_id) {
            active.turn_id = None;
            active.identity_epoch += 1;
        }
    }

    /// Remove one not-yet-delivered soft interrupt by its durable input id.
    pub fn cancel_soft_interrupt(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<bool, SessionNotActive> {
        let mut state = self.lock_state();
        let active = state
            .active
            .get_mut(session_id)
            .ok_or_else(|| SessionNotActive {
                session_id: session_id.to_owned(),
            })?;
        let epoch = active.soft_interrupt.epoch();
        let before = active.soft_interrupts.len();
        active
            .soft_interrupts
            .retain(|message| message.input_id.as_deref() != Some(input_id));
        let removed = active.soft_interrupts.len() != before;
        if removed && active.soft_interrupts.is_empty() {
            let _cleared = active.soft_interrupt.reset_if_epoch(epoch);
        }
        Ok(removed)
    }

    fn take_soft_interrupts(&self, session_id: &str, token: u64) -> SoftInterruptDelivery {
        let mut state = self.lock_state();
        let Some(active) = state.active.get_mut(session_id) else {
            return SoftInterruptDelivery::empty();
        };
        if active.token != token {
            return SoftInterruptDelivery::empty();
        }

        let signal_epoch = active.soft_interrupt.epoch();
        let messages: Vec<_> = active.soft_interrupts.drain(..).collect();
        let _cleared = active.soft_interrupt.reset_if_epoch(signal_epoch);
        let action = if messages.iter().any(|message| message.urgent) {
            SoftInterruptAction::SkipRemainingTools
        } else {
            SoftInterruptAction::Continue
        };
        SoftInterruptDelivery { messages, action }
    }

    fn unregister(&self, session_id: &str, token: u64) {
        let mut state = self.lock_state();
        if state
            .active
            .get(session_id)
            .is_some_and(|active| active.token == token)
        {
            state.active.remove(session_id);
            drop(state);
            self.inner.idle.notify_waiters();
        }
    }

    fn finish_recovery(&self, session_id: &str) {
        let mut state = self.lock_state();
        if state.recovering.remove(session_id) {
            drop(state);
            self.inner.idle.notify_waiters();
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, RegistryState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for SessionRunRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Opaque process-local cancellation identity captured before asynchronous work.
/// Bound to one registry, session, lease, and turn/input identity generation.
#[derive(Debug, Clone)]
pub struct SessionCancelTarget {
    registry: Arc<RegistryInner>,
    session_id: String,
    token: u64,
    identity_epoch: u64,
}

/// A session-scoped control object safe to retain across multiple turns.
#[derive(Debug, Clone)]
pub struct SessionControl {
    registry: SessionRunRegistry,
    session_id: String,
}

impl SessionControl {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The current process-local status of this session.
    ///
    /// Surfaces read this instead of keeping a private "a prompt is running" flag.
    /// A second exclusion mechanism can disagree with the registry, and the
    /// registry is the one that actually admits turns.
    #[must_use]
    pub fn status(&self) -> SessionStatus {
        self.registry.status(&self.session_id)
    }

    #[must_use]
    pub fn active_turn_id(&self) -> Option<String> {
        self.registry.active_turn_id(&self.session_id)
    }

    #[must_use]
    pub fn active_input_id(&self) -> Option<String> {
        self.registry.active_input_id(&self.session_id)
    }

    #[must_use]
    pub fn cancel_target(&self) -> Option<SessionCancelTarget> {
        self.registry.cancel_target(&self.session_id)
    }

    pub fn abort_target(
        &self,
        target: &SessionCancelTarget,
        request: HardInterruptRequest,
    ) -> bool {
        target.session_id == self.session_id && self.registry.abort_target(target, request)
    }

    pub fn abort_turn(
        &self,
        expected_turn_id: &str,
        request: HardInterruptRequest,
    ) -> Result<(), ExpectedTurnError> {
        self.registry
            .abort_turn(&self.session_id, expected_turn_id, request)
    }

    pub fn abort_input(&self, input_id: &str, request: HardInterruptRequest) -> bool {
        self.registry
            .abort_input(&self.session_id, input_id, request)
    }

    /// Aborts whichever turn is live now, not the turn that created this handle.
    pub fn abort(&self, request: HardInterruptRequest) -> AbortDisposition {
        self.registry.abort(&self.session_id, request)
    }

    /// Abort a live turn if one exists, without arming the next turn.
    #[must_use]
    pub fn abort_active(&self, request: HardInterruptRequest) -> bool {
        self.registry.abort_active(&self.session_id, request)
    }

    /// Wait until the current live turn, if any, releases this session.
    ///
    /// Unbounded for the reason given on [`SessionRunRegistry::wait_until_idle`].
    pub async fn wait_until_idle(&self) {
        self.registry.wait_until_idle(&self.session_id).await;
    }

    /// Clears a cancellation armed for a future turn during lifecycle teardown.
    #[must_use]
    pub fn clear_pending_abort(&self) -> bool {
        self.registry.clear_pending_abort(&self.session_id)
    }

    /// Queues a non-cancelling message for the live turn's next safe point.
    pub fn queue_soft_interrupt(
        &self,
        message: SoftInterruptMessage,
    ) -> Result<(), SessionNotActive> {
        self.registry
            .queue_soft_interrupt(&self.session_id, message)
    }

    pub fn queue_soft_interrupt_for_turn(
        &self,
        expected_turn_id: &str,
        message: SoftInterruptMessage,
    ) -> Result<(), ExpectedTurnError> {
        self.registry
            .queue_soft_interrupt_for_turn(&self.session_id, expected_turn_id, message)
    }

    pub fn send_queued(
        &self,
        inbox: zuno_db::inbox::SessionInbox,
        request: crate::admission::QueuedSendRequest,
        steering: crate::admission::SteeringContent,
    ) -> Result<crate::admission::QueuedSendAdmission, crate::admission::QueuedSendError> {
        if request.session_id != self.session_id {
            return Err(zuno_error::DbError::Conflict {
                table: "session_input".to_owned(),
                id: request.input_id,
                detail: "queued input belongs to another session control".to_owned(),
            }
            .into());
        }
        crate::admission::SessionInputAdmission::new(inbox, self.registry.clone())
            .send_queued(request, steering)
    }

    /// Cancels one not-yet-delivered soft interrupt by durable input id.
    pub fn cancel_soft_interrupt(&self, input_id: &str) -> Result<bool, SessionNotActive> {
        self.registry
            .cancel_soft_interrupt(&self.session_id, input_id)
    }
}

/// Exclusive live-turn lease returned by [`SessionRunRegistry::begin_turn`].
///
/// Keep this guard alive while calling `run_turn` and pass [`Self::interrupt_signal`]
/// into `TurnContext`. Dropping it marks the session idle on every exit path.
#[derive(Debug)]
pub struct SessionRunGuard {
    registry: SessionRunRegistry,
    session_id: String,
    token: u64,
    interrupt: HardInterruptSignal,
    soft_interrupt: InterruptSignal,
}

impl SessionRunGuard {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the live signal to pass directly into `TurnContext`.
    #[must_use]
    pub fn interrupt_signal(&self) -> &InterruptSignal {
        self.interrupt.signal()
    }

    /// Returns the first hard-interrupt request accepted for this turn.
    #[must_use]
    pub fn interrupt_request(&self) -> Option<HardInterruptRequest> {
        self.interrupt.request_snapshot()
    }

    /// Returns the wake-only signal used to stop a provider wait at a steering boundary.
    ///
    /// This is deliberately distinct from [`Self::interrupt_signal`]: firing it never
    /// cancels a tool or ends the turn. The loop checkpoints any partial model output,
    /// injects the queued durable input, and starts the next model step.
    #[must_use]
    pub fn soft_interrupt_signal(&self) -> &InterruptSignal {
        &self.soft_interrupt
    }

    /// Publish the engine turn id under this exact live lease.
    #[must_use]
    pub fn mark_turn_started(&self, turn_id: &str) -> Option<SessionTurnIdentityGuard> {
        self.registry
            .set_turn_id(&self.session_id, self.token, turn_id)
            .then(|| SessionTurnIdentityGuard {
                registry: self.registry.clone(),
                session_id: self.session_id.clone(),
                token: self.token,
                turn_id: turn_id.to_owned(),
            })
    }

    /// Bind the input before attempting its durable promotion. If cancellation
    /// loses the pending-row race, `abort_input` can still stop exactly this input.
    #[must_use]
    pub fn mark_input_started(&self, input_id: &str) -> Option<SessionInputIdentityGuard> {
        self.registry
            .set_input_id(&self.session_id, self.token, input_id)
            .then(|| SessionInputIdentityGuard {
                registry: self.registry.clone(),
                session_id: self.session_id.clone(),
                token: self.token,
                input_id: input_id.to_owned(),
            })
    }

    /// Atomically close admission only when no successfully accepted input remains.
    #[must_use]
    pub fn try_finish_inputs(&self) -> bool {
        self.registry
            .try_finish_inputs(&self.session_id, self.token)
    }

    /// Drains messages queued before this safe point in FIFO order.
    ///
    /// The caller injects `messages` into the transcript. When `action` is
    /// [`SoftInterruptAction::SkipRemainingTools`], it skips undispatched calls from
    /// the current tool batch and continues the turn with the injected message.
    #[must_use]
    pub fn take_soft_interrupts_at_safe_point(&self) -> SoftInterruptDelivery {
        self.registry
            .take_soft_interrupts(&self.session_id, self.token)
    }
}

/// Clears one engine turn id before the outer session lease is released.
#[derive(Debug)]
pub struct SessionTurnIdentityGuard {
    registry: SessionRunRegistry,
    session_id: String,
    token: u64,
    turn_id: String,
}

impl Drop for SessionTurnIdentityGuard {
    fn drop(&mut self) {
        self.registry
            .clear_turn_id(&self.session_id, self.token, &self.turn_id);
    }
}

/// Clears a driver's input binding before the outer session lease is released.
#[derive(Debug)]
pub struct SessionInputIdentityGuard {
    registry: SessionRunRegistry,
    session_id: String,
    token: u64,
    input_id: String,
}

impl Drop for SessionInputIdentityGuard {
    fn drop(&mut self) {
        self.registry
            .clear_input_id(&self.session_id, self.token, &self.input_id);
    }
}

impl Drop for SessionRunGuard {
    fn drop(&mut self) {
        // Capture before unregistering. If another cancel lands before cleanup,
        // reset_if_epoch refuses to erase that newer, not-yet-observed fire.
        let epoch = self.interrupt.signal().epoch();
        self.registry.unregister(&self.session_id, self.token);
        let _reset_applied = self.interrupt.signal().reset_if_epoch(epoch);
    }
}

/// Short process-local lease used only while recovering orphaned durable input.
#[derive(Debug)]
pub struct SessionRecoveryGuard {
    registry: SessionRunRegistry,
    session_id: String,
}

impl Drop for SessionRecoveryGuard {
    fn drop(&mut self) {
        self.registry.finish_recovery(&self.session_id);
    }
}
