//! Durable-first admission of one session input.
//!
//! Every client surface admits user input through this service. The durable inbox
//! row is written *before* the process-local live-turn lease is contended for, so a
//! prompt that arrives while the session is busy is recorded, then steered into the
//! running turn. Taking the lease first — and returning early when it is held — is
//! what loses a user's prompt with no durable trace.

use zuno_attachment::ImageAttachmentRef;
use zuno_db::inbox::{NewSessionInput, SessionInbox, SessionInput};
use zuno_error::DbError;

use crate::interrupt::{SoftInterruptMessage, SoftInterruptSource};
use crate::status::{ExpectedTurnError, SessionRunGuard, SessionRunRegistry};

/// Whether the admitting caller wants to own the turn that runs this input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnLease {
    /// The caller drives the input itself when the session is idle.
    Acquire,
    /// A separate driver loop owns every turn for this session.
    ///
    /// The caller never receives a lease, so it can admit input concurrently with
    /// the turn its own driver is running.
    Deferred,
}

/// The model-visible projection a caller offers to an already running turn.
///
/// The durable input id is deliberately absent: [`SessionInputAdmission`] fills it
/// from the row it just wrote, so a caller cannot label a steer with an id that no
/// durable row carries.
///
/// There is no inline-image field either. The durable inbox stores normalized
/// attachment references, so inline bytes injected here would be model-visible
/// content with no durable record. Callers admit images into the attachment store
/// first and steer with the resulting references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringContent {
    /// Text the running turn injects at its next safe point.
    pub content: String,
    /// Durable normalized image references admitted before the inbox write.
    pub attachments: Vec<ImageAttachmentRef>,
    /// Whether the turn loop may skip remaining tool calls before injecting.
    pub urgent: bool,
    /// Which producer the injected message is attributed to.
    pub source: SoftInterruptSource,
}

impl SteeringContent {
    /// Steer a running turn with plain user text.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            attachments: Vec::new(),
            urgent: false,
            source: SoftInterruptSource::User,
        }
    }

    /// Attach durable image references to the injected message.
    #[must_use]
    pub fn with_attachments(mut self, attachments: Vec<ImageAttachmentRef>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Project this content onto an already admitted durable row.
    ///
    /// Used when a client re-steers a row it edited: the durable input already
    /// exists, so only the injected message is rebuilt.
    #[must_use]
    pub fn into_message(self, input_id: &str) -> SoftInterruptMessage {
        SoftInterruptMessage {
            input_id: Some(input_id.to_owned()),
            content: self.content,
            images: Vec::new(),
            attachments: self.attachments,
            urgent: self.urgent,
            source: self.source,
        }
    }
}

/// A precise steering request either binds its expected turn or admits nothing.
#[derive(Debug, thiserror::Error)]
pub enum SteerAdmissionError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Turn(#[from] ExpectedTurnError),
}

/// What a caller must do with an input that is already durable.
///
/// Every variant carries the admitted row, so no outcome can be reported to a user
/// without the durable evidence that the input was accepted.
#[derive(Debug)]
pub enum InputAdmission {
    /// The caller owns the exclusive live-turn lease and must drive the input.
    Drive {
        input: SessionInput,
        guard: SessionRunGuard,
    },
    /// A live turn accepted the input for its next safe point.
    Steered { input: SessionInput },
    /// The input stays durably pending until the next FIFO promotion claims it.
    Pending { input: SessionInput },
}

impl InputAdmission {
    /// The durable row written before any lease was contended for.
    #[must_use]
    pub const fn input(&self) -> &SessionInput {
        match self {
            Self::Drive { input, .. } | Self::Steered { input } | Self::Pending { input } => input,
        }
    }

    /// Whether a live turn accepted this input for its next safe point.
    #[must_use]
    pub const fn steered(&self) -> bool {
        matches!(self, Self::Steered { .. })
    }
}

/// How many busy-to-idle flips one admission will chase before leaving the row pending.
///
/// Each iteration requires the session to become idle after `begin_turn` was
/// refused and then busy again before `queue_soft_interrupt` runs, so the bound is
/// a contention ceiling rather than a timeout. Exhausting it is not a failure: the
/// durable row is the queue, and the next turn promotes it in FIFO order.
const LEASE_HANDOFF_ATTEMPTS: usize = 8;

/// Writes one durable inbox row, then resolves how it reaches the model.
#[derive(Clone)]
pub struct SessionInputAdmission {
    inbox: SessionInbox,
    runs: SessionRunRegistry,
}

impl SessionInputAdmission {
    /// Construct the service over the session's durable inbox and run registry.
    #[must_use]
    pub const fn new(inbox: SessionInbox, runs: SessionRunRegistry) -> Self {
        Self { inbox, runs }
    }

    /// The durable inbox this service admits into.
    #[must_use]
    pub const fn inbox(&self) -> &SessionInbox {
        &self.inbox
    }

    /// Persist `input`, then resolve how it reaches the model.
    ///
    /// `steering` is the projection offered to a turn that is already running. When
    /// it is `None` a busy session leaves the row pending instead of injecting it.
    ///
    /// # Errors
    ///
    /// Returns a database error only when the durable admission itself fails. A
    /// contended lease is an outcome, never an error: the row is already committed.
    pub fn admit(
        &self,
        input: NewSessionInput,
        lease: TurnLease,
        steering: Option<SteeringContent>,
    ) -> Result<InputAdmission, DbError> {
        let input = self.inbox.admit(input)?;
        Ok(self.route_admitted(input, lease, steering))
    }

    /// Admit a steer only if `expected_turn_id` still owns the live session.
    ///
    /// The database writer is acquired before checking the registry. The pending
    /// row therefore cannot escape into a later turn if this turn ends while the
    /// admission waits for SQLite. Safe-point promotion also takes the database
    /// writer, so it cannot observe the queued signal before admission commits.
    ///
    /// # Errors
    ///
    /// Rejects a stale/non-steerable turn or a failed database write without leaving
    /// a pending durable input. A failed commit also retires its process-local signal.
    pub fn admit_steer(
        &self,
        input: NewSessionInput,
        expected_turn_id: &str,
        steering: SteeringContent,
    ) -> Result<SessionInput, SteerAdmissionError> {
        let session_id = input.session_id.clone();
        let input_id = input.id.clone();
        let mut queued = false;
        let admitted = self.inbox.admit_with(input, |input| {
            self.runs.queue_soft_interrupt_for_turn(
                &session_id,
                expected_turn_id,
                steering.into_message(&input.id),
            )?;
            queued = true;
            Ok::<(), SteerAdmissionError>(())
        });
        if admitted.is_err() && queued {
            let _retired = self.runs.cancel_soft_interrupt(&session_id, &input_id);
        }
        admitted
    }

    /// Resolve turn ownership for an input a caller already committed durably.
    ///
    /// Used when the first input and its new Session row must share one
    /// caller-owned transaction. This performs no database write.
    #[must_use]
    pub fn route_admitted(
        &self,
        input: SessionInput,
        lease: TurnLease,
        steering: Option<SteeringContent>,
    ) -> InputAdmission {
        let session_id = input.session_id.clone();
        let message = steering.map(|steering| steering.into_message(&input.id));

        if lease == TurnLease::Deferred {
            return match message {
                Some(message) => match self.runs.queue_soft_interrupt(&session_id, message) {
                    Ok(()) => InputAdmission::Steered { input },
                    Err(_) => InputAdmission::Pending { input },
                },
                None => InputAdmission::Pending { input },
            };
        }

        for _ in 0..LEASE_HANDOFF_ATTEMPTS {
            match self.runs.begin_turn(session_id.clone()) {
                Ok(guard) => return InputAdmission::Drive { input, guard },
                Err(_busy) => {
                    let Some(message) = message.clone() else {
                        return InputAdmission::Pending { input };
                    };
                    if self.runs.queue_soft_interrupt(&session_id, message).is_ok() {
                        return InputAdmission::Steered { input };
                    }
                    // The turn ended between the refused lease and the steer. Try
                    // to own the next turn rather than reporting a lost race.
                }
            }
        }
        InputAdmission::Pending { input }
    }
}
