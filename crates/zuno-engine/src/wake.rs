//! Durable input delivery across active and idle session turns.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use zuno_db::inbox::{DurableInputKind, SessionInbox, SessionInput};

use crate::interrupt::{SoftInterruptMessage, SoftInterruptSource};
use crate::report::ReportBatch;
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
    /// The batch is offered in admission order and carries the same render-time grouping
    /// the idle path uses, so a report the batch supersedes reads as superseded here too
    /// instead of arriving as a live state the parent must chase.
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
        if !is_settled_report(woken) {
            return self
                .runs
                .queue_soft_interrupt(session_id, message.clone())
                .is_ok();
        }
        let batch = ReportBatch::project(pending);
        let mut woken_queued = None;
        for report in batch.reports() {
            let is_woken = report.input_id == woken.id;
            let queued = if is_woken {
                SoftInterruptMessage {
                    content: report.text.clone(),
                    ..message.clone()
                }
            } else {
                SoftInterruptMessage {
                    input_id: Some(report.input_id.clone()),
                    content: report.text.clone(),
                    images: Vec::new(),
                    attachments: Vec::new(),
                    urgent: false,
                    source: SoftInterruptSource::BackgroundTask,
                }
            };
            let outcome = self.runs.queue_soft_interrupt(session_id, queued);
            if is_woken {
                woken_queued = Some(outcome.is_ok());
            }
            if let Err(error) = outcome {
                tracing::debug!(
                    session_id,
                    input_id = %report.input_id,
                    %error,
                    "leaving a batch member pending for the next wake"
                );
            }
        }
        // A report the projection cannot render is not part of the batch, but its own
        // delivery still decides this outcome, so it is offered exactly as its caller
        // built it rather than through content this coordinator invented.
        woken_queued.unwrap_or_else(|| {
            self.runs
                .queue_soft_interrupt(session_id, message.clone())
                .is_ok()
        })
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
