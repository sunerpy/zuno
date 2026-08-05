//! In-process run state and control for session turns.
//!
//! The registry is intentionally not persisted. A guard is the exclusive lease for
//! one session's live turn, and dropping it returns the session to idle. Control
//! handles retain only a session id plus the registry, so an old UI handle always
//! looks up the signal and soft-interrupt queue belonging to the current turn.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::interrupt::{InterruptSignal, SoftInterruptMessage};

/// The process-local state exposed to CLI, TUI, HTTP, and ACP surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Idle,
    Busy,
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
}

#[derive(Debug, Default)]
struct RegistryState {
    active: HashMap<String, ActiveSession>,
}

#[derive(Debug)]
struct ActiveSession {
    token: u64,
    interrupt: InterruptSignal,
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
        state.active.insert(
            session_id.clone(),
            ActiveSession {
                token,
                interrupt: interrupt.clone(),
                soft_interrupts: VecDeque::new(),
            },
        );

        Ok(SessionRunGuard {
            registry: self.clone(),
            session_id,
            token,
            interrupt,
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

    /// Fires the live turn's interrupt signal without waiting for an event consumer.
    ///
    /// The signal is fired while the registry lock still protects the active entry,
    /// making abort and guard removal linearizable. `false` means the session was idle.
    pub fn abort(&self, session_id: &str) -> bool {
        let state = self.lock_state();
        let Some(active) = state.active.get(session_id) else {
            return false;
        };
        active.interrupt.fire();
        true
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
        Ok(())
    }

    fn take_soft_interrupts(&self, session_id: &str, token: u64) -> SoftInterruptDelivery {
        let mut state = self.lock_state();
        let Some(active) = state.active.get_mut(session_id) else {
            return SoftInterruptDelivery::empty();
        };
        if active.token != token {
            return SoftInterruptDelivery::empty();
        }

        let messages: Vec<_> = active.soft_interrupts.drain(..).collect();
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
    pub fn abort(&self) -> bool {
        self.registry.abort(&self.session_id)
    }

    /// Queues a non-cancelling message for the live turn's next safe point.
    pub fn queue_soft_interrupt(
        &self,
        message: SoftInterruptMessage,
    ) -> Result<(), SessionNotActive> {
        self.registry
            .queue_soft_interrupt(&self.session_id, message)
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
