//! In-process run state and control for session turns.
//!
//! The registry is intentionally not persisted. A guard is the exclusive lease for
//! one session's live turn, and dropping it returns the session to idle. Control
//! handles retain only a session id plus the registry, so an old UI handle always
//! looks up the signal and soft-interrupt queue belonging to the current turn.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::interrupt::{InterruptSignal, SoftInterruptMessage};

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
    pending_interrupts: BTreeSet<String>,
}

#[derive(Debug)]
struct ActiveSession {
    token: u64,
    interrupt: InterruptSignal,
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
        if state.active.contains_key(&session_id) {
            return Err(SessionBusy { session_id });
        }

        let token = self.inner.next_token.fetch_add(1, Ordering::Relaxed);
        let interrupt = InterruptSignal::new();
        let soft_interrupt = InterruptSignal::new();
        if state.pending_interrupts.remove(&session_id) {
            interrupt.fire();
        }
        state.active.insert(
            session_id.clone(),
            ActiveSession {
                token,
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

    /// Creates a reusable session control handle.
    ///
    /// The handle deliberately captures no interrupt signal. Every operation resolves
    /// the current active entry by session id, which keeps stale handles effective.
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
        if self.lock_state().active.contains_key(session_id) {
            SessionStatus::Busy
        } else {
            SessionStatus::Idle
        }
    }

    /// Returns a stable snapshot of every process-local active session id.
    #[must_use]
    pub fn active_sessions(&self) -> BTreeSet<String> {
        self.lock_state().active.keys().cloned().collect()
    }

    /// Wait until `session_id` has no live turn without polling.
    ///
    /// The waiter is registered before the status re-check, so a guard dropped
    /// between observation and suspension cannot lose its wake-up.
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
    pub fn abort(&self, session_id: &str) -> AbortDisposition {
        let mut state = self.lock_state();
        if let Some(active) = state.active.get(session_id) {
            active.interrupt.fire();
            AbortDisposition::Active
        } else {
            state.pending_interrupts.insert(session_id.to_owned());
            AbortDisposition::ArmedNext
        }
    }

    /// Abort only a currently live turn without arming a future one.
    ///
    /// Lifecycle teardown uses this variant: closing an already-idle surface must
    /// not poison the next process-local mount of the same durable session.
    pub fn abort_active(&self, session_id: &str) -> bool {
        let state = self.lock_state();
        state.active.get(session_id).is_some_and(|active| {
            active.interrupt.fire();
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
        self.lock_state().pending_interrupts.remove(session_id)
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
        active.soft_interrupts.push_back(message);
        active.soft_interrupt.fire();
        Ok(())
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

    /// Aborts whichever turn is live now, not the turn that created this handle.
    pub fn abort(&self) -> AbortDisposition {
        self.registry.abort(&self.session_id)
    }

    /// Abort a live turn if one exists, without arming the next turn.
    #[must_use]
    pub fn abort_active(&self) -> bool {
        self.registry.abort_active(&self.session_id)
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
    interrupt: InterruptSignal,
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
        &self.interrupt
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

impl Drop for SessionRunGuard {
    fn drop(&mut self) {
        // Capture before unregistering. If another cancel lands before cleanup,
        // reset_if_epoch refuses to erase that newer, not-yet-observed fire.
        let epoch = self.interrupt.epoch();
        self.registry.unregister(&self.session_id, self.token);
        let _reset_applied = self.interrupt.reset_if_epoch(epoch);
    }
}
