//! Durable input delivery across active and idle session turns.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use zuno_db::inbox::{SessionInbox, SessionInput};

use crate::interrupt::SoftInterruptMessage;
use crate::status::{SessionRunGuard, SessionRunRegistry};

/// Opens and drives one idle session from an already persisted input.
#[async_trait]
pub trait PendingInputDriver: Send + Sync + 'static {
    /// Drive `input` while owning `guard`.
    async fn drive(&self, input: SessionInput, guard: SessionRunGuard) -> Result<(), String>;
}

/// How one durable wake request reached the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOutcome {
    /// Another process-local delivery already owns this exact durable input.
    AlreadyInFlight,
    /// The active turn claimed the input at a safe point.
    ClaimedByActiveTurn,
    /// This coordinator acquired an idle lease and drove the input.
    Driven,
}

/// Coordinates one durable inbox with the process-local run registry.
#[derive(Clone)]
pub struct SessionWakeCoordinator {
    inbox: SessionInbox,
    runs: SessionRunRegistry,
    driver: Arc<dyn PendingInputDriver>,
    in_flight: Arc<Mutex<HashSet<(String, String)>>>,
}

impl SessionWakeCoordinator {
    /// Construct a coordinator over the session's durable and live state.
    #[must_use]
    pub fn new(
        inbox: SessionInbox,
        runs: SessionRunRegistry,
        driver: Arc<dyn PendingInputDriver>,
    ) -> Self {
        Self {
            inbox,
            runs,
            driver,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Deliver one already admitted input without losing an active-to-idle race.
    pub async fn deliver(
        &self,
        session_id: &str,
        input_id: &str,
        message: SoftInterruptMessage,
    ) -> Result<WakeOutcome, String> {
        if message.input_id.as_deref() != Some(input_id) {
            return Err(format!(
                "wake message input id does not match durable input `{input_id}`"
            ));
        }
        let Some(_lease) = WakeLease::acquire(
            Arc::clone(&self.in_flight),
            session_id.to_owned(),
            input_id.to_owned(),
        ) else {
            return Ok(WakeOutcome::AlreadyInFlight);
        };

        loop {
            let Some(input) = self.pending_input(session_id, input_id)? else {
                return Ok(WakeOutcome::ClaimedByActiveTurn);
            };
            match self.runs.begin_turn(session_id) {
                Ok(guard) => {
                    self.driver.drive(input, guard).await?;
                    if self.pending_input(session_id, input_id)?.is_some() {
                        return Err(format!(
                            "pending-input driver returned without claiming `{input_id}`"
                        ));
                    }
                    return Ok(WakeOutcome::Driven);
                }
                Err(_) => match self.runs.queue_soft_interrupt(session_id, message.clone()) {
                    Ok(()) => self.runs.wait_until_idle(session_id).await,
                    Err(_) => tokio::task::yield_now().await,
                },
            }
        }
    }

    fn pending_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<Option<SessionInput>, String> {
        self.inbox
            .pending(session_id)
            .map_err(|error| error.to_string())
            .map(|pending| pending.into_iter().find(|input| input.id == input_id))
    }
}

struct WakeLease {
    in_flight: Arc<Mutex<HashSet<(String, String)>>>,
    key: (String, String),
}

impl WakeLease {
    fn acquire(
        in_flight: Arc<Mutex<HashSet<(String, String)>>>,
        session_id: String,
        input_id: String,
    ) -> Option<Self> {
        let key = (session_id, input_id);
        let inserted = in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.clone());
        inserted.then_some(Self { in_flight, key })
    }
}

impl Drop for WakeLease {
    fn drop(&mut self) {
        self.in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.key);
    }
}
