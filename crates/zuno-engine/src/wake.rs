//! Durable input delivery across active and idle session turns.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use zuno_db::inbox::{DurableInputKind, SessionInbox, SessionInput};

use crate::interrupt::{SoftInterruptMessage, SoftInterruptSource};
use crate::status::{SessionRunGuard, SessionRunRegistry};

/// Opens and drives one idle session from an already persisted input.
#[async_trait]
pub trait PendingInputDriver: Send + Sync + 'static {
    /// Drive `input` while owning `guard`.
    ///
    /// The implementation must leave `input` no longer pending before it returns: it
    /// either promotes and drives the row, or settles it as failed. A driver is free to
    /// claim the session's other pending rows of the same kind in the same turn, which
    /// is how a batch of settled reports reaches the model as one request instead of one
    /// request per report.
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
            let pending = self
                .inbox
                .pending(session_id)
                .map_err(|error| error.to_string())?;
            let Some(input) = pending.iter().find(|input| input.id == input_id).cloned() else {
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
                Err(_) => {
                    if self.steer_batch(session_id, &pending, &input, &message) {
                        self.runs.wait_until_idle(session_id).await;
                    } else {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
    }

    /// Offer the running turn every settled report this wake covers, not just one row.
    ///
    /// The idle path drives the whole pending report batch under one turn lease, and the
    /// active path injects every queued soft interrupt at the same safe point. Steering
    /// only the woken row would leave its batch mates for the next periodic scan, so one
    /// settled batch would still become a stream of turns that each announce a state a
    /// later report had already replaced.
    ///
    /// The woken row's own delivery decides the outcome; the batch mates are offered
    /// best-effort, because each of them keeps its own durable row and its own wake.
    /// Reports the active turn never reaches stay pending for the next scan.
    fn steer_batch(
        &self,
        session_id: &str,
        pending: &[SessionInput],
        woken: &SessionInput,
        message: &SoftInterruptMessage,
    ) -> bool {
        if self
            .runs
            .queue_soft_interrupt(session_id, message.clone())
            .is_err()
        {
            return false;
        }
        if !is_settled_report(woken) {
            return true;
        }
        for mate in pending.iter().filter(|input| input.id != woken.id) {
            let Some(message) = settled_report_message(mate) else {
                continue;
            };
            if let Err(error) = self.runs.queue_soft_interrupt(session_id, message) {
                tracing::debug!(
                    session_id,
                    input_id = %mate.id,
                    %error,
                    "leaving a batch mate pending for the next wake"
                );
            }
        }
        true
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

/// Whether this row is a settled report the wake path batches.
fn is_settled_report(input: &SessionInput) -> bool {
    DurableInputKind::classify(&input.prompt).is_some_and(DurableInputKind::is_asynchronous_report)
}

/// Project one pending report onto the message an active turn injects for it.
///
/// Only a shape whose whole model-visible text is already durable can be steered this
/// way. A row this refuses keeps its own wake instead of reaching the model through
/// content the coordinator invented.
fn settled_report_message(input: &SessionInput) -> Option<SoftInterruptMessage> {
    let kind = DurableInputKind::classify(&input.prompt)?;
    if !kind.is_asynchronous_report() {
        return None;
    }
    Some(SoftInterruptMessage {
        input_id: Some(input.id.clone()),
        content: kind.plain_text(&input.prompt)?.to_owned(),
        images: Vec::new(),
        attachments: Vec::new(),
        urgent: false,
        source: SoftInterruptSource::BackgroundTask,
    })
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
