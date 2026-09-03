//! Durable FIFO input admission and promotion for session drivers.

use crate::event_log::{NewSessionEvent, append_in, query_error};
use crate::{Pool, open};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde_json::{Map, Value};
use std::sync::Arc;
use zuno_error::DbError;

/// When an admitted input should become model-visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDelivery {
    /// Remain FIFO-queued until the next driver turn.
    Queue,
    /// Inject at the next safe point of an active turn.
    Steer,
}

impl InputDelivery {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Steer => "steer",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "queue" => Ok(Self::Queue),
            "steer" => Ok(Self::Steer),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown input delivery `{value}`"
            )))),
        }
    }

    const fn admitted_state(self) -> SubmissionState {
        match self {
            Self::Queue => SubmissionState::Queued,
            Self::Steer => SubmissionState::Steering,
        }
    }
}

/// Durable lifecycle of one submitted input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionState {
    /// Transient caller state before SQLite confirms admission.
    Admitting,
    /// Persisted FIFO work for a future turn.
    Queued,
    /// Persisted input waiting for an active turn safe point.
    Steering,
    /// Claimed exactly once by a driver or active generation.
    Promoted,
    /// Persisted as model-visible user input.
    Consumed,
    /// Removed by the user before promotion.
    Cancelled,
    /// Could not be decoded or persisted after admission.
    Failed,
}

impl SubmissionState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitting => "admitting",
            Self::Queued => "queued",
            Self::Steering => "steering",
            Self::Promoted => "promoted",
            Self::Consumed => "consumed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "admitting" => Ok(Self::Admitting),
            "queued" => Ok(Self::Queued),
            "steering" => Ok(Self::Steering),
            "promoted" => Ok(Self::Promoted),
            "consumed" => Ok(Self::Consumed),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown input submission state `{value}`"
            )))),
        }
    }

    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Queued | Self::Steering)
    }
}

/// The published shape of one durable inbox prompt payload.
///
/// Every writer of `session_input.prompt` produces exactly one of these kinds.
/// Drivers match on the classification instead of re-deriving `kind` string tests,
/// so a surface that cannot run a shape can skip that row instead of failing the
/// whole session, and a new writer is a compile-visible addition here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableInputKind {
    /// A terminal-UI submission carrying its own structured payload.
    TuiPrompt,
    /// An ACP `session/prompt` submission carrying text plus content blocks.
    AcpPrompt,
    /// An HTTP prompt body, including its agent and model overrides.
    User,
    /// A settled subagent job report.
    SubagentReport,
    /// A settled product-agent report.
    ProductAgentReport,
    /// A settled workflow report.
    WorkflowReport,
    /// A settled council report.
    CouncilReport,
    /// A settled background execution report.
    BackgroundExecutionReport,
    /// An answered durable human request routed back into the session.
    HumanRequestAnswer,
    /// A turn host message admitted, promoted, and consumed in one transaction.
    ///
    /// This shape carries no `kind` and is never observed pending.
    HostMessage,
}

impl DurableInputKind {
    /// Classify one durable prompt payload, or `None` when no writer publishes it.
    #[must_use]
    pub fn classify(prompt: &Value) -> Option<Self> {
        match prompt.get("kind").and_then(Value::as_str) {
            Some("tuiPrompt") => Some(Self::TuiPrompt),
            Some("acpPrompt") => Some(Self::AcpPrompt),
            Some("user") => Some(Self::User),
            Some("subagentReport") => Some(Self::SubagentReport),
            Some("productAgentReport") => Some(Self::ProductAgentReport),
            Some("workflowReport") => Some(Self::WorkflowReport),
            Some("councilReport") => Some(Self::CouncilReport),
            Some("backgroundExecutionReport") => Some(Self::BackgroundExecutionReport),
            Some("humanRequestAnswer") => Some(Self::HumanRequestAnswer),
            Some(_) => None,
            None => prompt.get("message").is_some().then_some(Self::HostMessage),
        }
    }

    /// The `kind` discriminator this shape writes, when it has one.
    #[must_use]
    pub const fn as_str(self) -> Option<&'static str> {
        match self {
            Self::TuiPrompt => Some("tuiPrompt"),
            Self::AcpPrompt => Some("acpPrompt"),
            Self::User => Some("user"),
            Self::SubagentReport => Some("subagentReport"),
            Self::ProductAgentReport => Some("productAgentReport"),
            Self::WorkflowReport => Some("workflowReport"),
            Self::CouncilReport => Some("councilReport"),
            Self::BackgroundExecutionReport => Some("backgroundExecutionReport"),
            Self::HumanRequestAnswer => Some("humanRequestAnswer"),
            Self::HostMessage => None,
        }
    }

    /// Whether this shape is a settled report delivered by the idle wake path.
    #[must_use]
    pub const fn is_asynchronous_report(self) -> bool {
        matches!(
            self,
            Self::SubagentReport
                | Self::ProductAgentReport
                | Self::WorkflowReport
                | Self::CouncilReport
                | Self::BackgroundExecutionReport
        )
    }

    /// The whole model-visible text of this shape, when it is one plain string.
    ///
    /// Shapes that carry structured payloads only their own surface can render
    /// return `None`. `user` is deliberately excluded: its row also carries agent
    /// and model overrides, so driving it as bare text would silently drop them.
    #[must_use]
    pub fn plain_text(self, prompt: &Value) -> Option<&str> {
        match self {
            Self::AcpPrompt
            | Self::HumanRequestAnswer
            | Self::SubagentReport
            | Self::ProductAgentReport
            | Self::WorkflowReport
            | Self::CouncilReport
            | Self::BackgroundExecutionReport => prompt.get("text").and_then(Value::as_str),
            Self::TuiPrompt | Self::User | Self::HostMessage => None,
        }
    }

    /// The structured content blocks carried alongside [`Self::plain_text`].
    ///
    /// Encoded as opaque JSON here because the block type belongs to the model
    /// layer. A driver that ignores these blocks would drop admitted images.
    #[must_use]
    pub fn content_blocks(self, prompt: &Value) -> Option<&Vec<Value>> {
        match self {
            Self::AcpPrompt => prompt.get("content").and_then(Value::as_array),
            Self::TuiPrompt
            | Self::User
            | Self::HostMessage
            | Self::HumanRequestAnswer
            | Self::SubagentReport
            | Self::ProductAgentReport
            | Self::WorkflowReport
            | Self::CouncilReport
            | Self::BackgroundExecutionReport => None,
        }
    }
}

/// One input waiting to be admitted.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSessionInput {
    /// Opaque admission identifier.
    pub id: String,
    /// Session inbox that owns the input.
    pub session_id: String,
    /// Structured model-visible prompt.
    pub prompt: Value,
    /// Requested delivery behavior.
    pub delivery: InputDelivery,
    /// Creation timestamp in milliseconds since the Unix epoch.
    pub time_created: i64,
}

impl NewSessionInput {
    /// Create one admission request.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        session_id: impl Into<String>,
        prompt: Value,
        delivery: InputDelivery,
        time_created: i64,
    ) -> Self {
        Self {
            id: id.into(),
            session_id: session_id.into(),
            prompt,
            delivery,
            time_created,
        }
    }
}

/// One admitted input and its durable sequence positions.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInput {
    /// Opaque admission identifier.
    pub id: String,
    /// Session inbox that owns the input.
    pub session_id: String,
    /// Structured model-visible prompt.
    pub prompt: Value,
    /// Requested delivery behavior.
    pub delivery: InputDelivery,
    /// Current durable lifecycle state.
    pub state: SubmissionState,
    /// Optimistic revision used by queue edits and cancellation.
    pub revision: i64,
    /// Sequence of the admission event.
    pub admitted_sequence: i64,
    /// Sequence of the promotion event, once claimed.
    pub promoted_sequence: Option<i64>,
    /// Terminal diagnostic when the submission failed.
    pub error: Option<String>,
    /// Creation timestamp in milliseconds since the Unix epoch.
    pub time_created: i64,
    /// Last durable state-change timestamp.
    pub time_updated: i64,
}

/// Durable FIFO inbox over the session event stream.
#[derive(Clone)]
pub struct SessionInbox {
    pool: Arc<Pool>,
}

impl SessionInbox {
    /// Open an inbox over one initialized pool.
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Admit an input and its event in one transaction.
    ///
    /// # Errors
    ///
    /// Returns a database error when validation, event append, JSON encoding,
    /// or the inbox insert fails. No event remains committed on failure.
    pub fn admit(&self, input: NewSessionInput) -> Result<SessionInput, DbError> {
        validate_input(&input)?;
        self.pool
            .transaction(|transaction| admit_in(transaction, input))
    }

    /// Promote the oldest pending input, optionally filtered by delivery.
    ///
    /// # Errors
    ///
    /// Returns a database error when the candidate cannot be read, decoded,
    /// logged, or updated.
    pub fn promote_next(
        &self,
        session_id: &str,
        delivery: Option<InputDelivery>,
    ) -> Result<Option<SessionInput>, DbError> {
        self.pool.transaction(|transaction| {
            let input = select_next(transaction, session_id, delivery)?;
            promote_selected(transaction, session_id, input)
        })
    }

    /// Promote one pending input by its opaque id.
    ///
    /// This is used when a live turn injects an admitted steer at a safe point.
    /// Other FIFO inputs remain pending for the next driver step.
    ///
    /// # Errors
    ///
    /// Returns a database error when the input cannot be read, decoded, logged,
    /// or updated.
    pub fn promote_id(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<Option<SessionInput>, DbError> {
        self.pool.transaction(|transaction| {
            let stored = transaction
                .query_row(
                    "SELECT id, session_id, prompt, delivery, state, revision, admitted_seq, \
                            promoted_seq, error, time_created, time_updated \
                     FROM session_input \
                     WHERE session_id = ?1 AND id = ?2 AND state IN ('queued', 'steering')",
                    params![session_id, input_id],
                    decode_stored_input,
                )
                .optional()
                .map_err(open::map_error)?;
            let input = stored.map(decode_input).transpose()?;
            promote_selected(transaction, session_id, input)
        })
    }

    /// Read pending inputs in FIFO order.
    ///
    /// # Errors
    ///
    /// Returns a database error when rows cannot be queried or decoded.
    pub fn pending(&self, session_id: &str) -> Result<Vec<SessionInput>, DbError> {
        let connection = self.pool.get()?;
        pending_in(&connection, session_id)
    }

    /// Read one input regardless of lifecycle state.
    pub fn get(&self, session_id: &str, input_id: &str) -> Result<Option<SessionInput>, DbError> {
        let connection = self.pool.get()?;
        select_by_id(&connection, session_id, input_id)
    }

    /// Replace one still-pending prompt using optimistic concurrency.
    pub fn edit_pending(
        &self,
        session_id: &str,
        input_id: &str,
        expected_revision: i64,
        prompt: Value,
        time_updated: i64,
    ) -> Result<SessionInput, DbError> {
        self.pool.transaction(|transaction| {
            edit_pending_in(
                transaction,
                session_id,
                input_id,
                expected_revision,
                prompt,
                time_updated,
            )
        })
    }

    /// Cancel one still-pending input using optimistic concurrency.
    pub fn cancel_pending(
        &self,
        session_id: &str,
        input_id: &str,
        expected_revision: i64,
        time_updated: i64,
    ) -> Result<SessionInput, DbError> {
        self.pool.transaction(|transaction| {
            cancel_pending_in(
                transaction,
                session_id,
                input_id,
                expected_revision,
                time_updated,
            )
        })
    }

    /// Mark a promoted input as durably model-visible.
    pub fn mark_consumed(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<Option<SessionInput>, DbError> {
        self.pool.transaction(|transaction| {
            transition_in(
                transaction,
                session_id,
                input_id,
                &[SubmissionState::Promoted],
                SubmissionState::Consumed,
                None,
                "session.input.consumed",
            )
        })
    }

    /// Mark an admitted input failed without making it eligible for replay.
    pub fn mark_failed(
        &self,
        session_id: &str,
        input_id: &str,
        error: impl Into<String>,
    ) -> Result<Option<SessionInput>, DbError> {
        let error = error.into();
        self.pool.transaction(|transaction| {
            transition_in(
                transaction,
                session_id,
                input_id,
                &[
                    SubmissionState::Queued,
                    SubmissionState::Steering,
                    SubmissionState::Promoted,
                ],
                SubmissionState::Failed,
                Some(error.as_str()),
                "session.input.failed",
            )
        })
    }

    /// Return one orphaned promoted input to its original admitted lane.
    ///
    /// Detached turn recovery uses this only after the prior process can no longer
    /// own the promotion. Repeating the operation is idempotent for an already
    /// queued or steering input, while consumed and otherwise terminal inputs are
    /// left unchanged.
    pub fn recover_promoted(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<Option<SessionInput>, DbError> {
        self.pool
            .transaction(|transaction| recover_promoted_in(transaction, session_id, input_id))
    }
}

pub(crate) fn validate_input(input: &NewSessionInput) -> Result<(), DbError> {
    if input.id.trim().is_empty() || input.session_id.trim().is_empty() {
        return Err(query_error(std::io::Error::other(
            "input id and session id must not be empty",
        )));
    }
    Ok(())
}

pub(crate) fn admit_in(
    transaction: &Transaction<'_>,
    input: NewSessionInput,
) -> Result<SessionInput, DbError> {
    let event = append_in(
        transaction,
        &input.session_id,
        NewSessionEvent::new("session.input.admitted", event_properties_new(&input))?,
    )?;
    let prompt = serde_json::to_string(&input.prompt).map_err(query_error)?;
    let state = input.delivery.admitted_state();
    transaction
        .execute(
            "INSERT INTO session_input \
             (id, session_id, prompt, delivery, state, revision, admitted_seq, promoted_seq, \
              error, time_created, time_updated) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, NULL, NULL, ?7, ?7)",
            params![
                input.id,
                input.session_id,
                prompt,
                input.delivery.as_str(),
                state.as_str(),
                event.sequence,
                input.time_created
            ],
        )
        .map_err(open::map_error)?;
    Ok(SessionInput {
        id: input.id,
        session_id: input.session_id,
        prompt: input.prompt,
        delivery: input.delivery,
        state,
        revision: 1,
        admitted_sequence: event.sequence,
        promoted_sequence: None,
        error: None,
        time_created: input.time_created,
        time_updated: input.time_created,
    })
}

/// Admit and immediately promote one driver-owned input in the caller's transaction.
///
/// Interactive prompts use this together with the `session` and user-message inserts.
/// The durable FIFO therefore records the input before execution, while the immediate
/// promotion prevents the already-persisted user message from being injected a second
/// time by a later driver step.
///
/// # Errors
///
/// The same validation, event-log, encoding, and SQLite errors as [`SessionInbox::admit`]
/// and [`SessionInbox::promote_id`].
pub fn admit_and_promote_in(
    transaction: &Transaction<'_>,
    input: NewSessionInput,
) -> Result<SessionInput, DbError> {
    validate_input(&input)?;
    let session_id = input.session_id.clone();
    let admitted = admit_in(transaction, input)?;
    promote_selected(transaction, &session_id, Some(admitted))?.ok_or_else(|| {
        query_error(std::io::Error::other(
            "newly admitted input was not promotable",
        ))
    })
}

fn select_next(
    transaction: &Transaction<'_>,
    session_id: &str,
    delivery: Option<InputDelivery>,
) -> Result<Option<SessionInput>, DbError> {
    let stored = match delivery {
        Some(delivery) => transaction
            .query_row(
                "SELECT id, session_id, prompt, delivery, state, revision, admitted_seq, \
                        promoted_seq, error, time_created, time_updated \
                 FROM session_input \
                 WHERE session_id = ?1 AND state IN ('queued', 'steering') AND delivery = ?2 \
                 ORDER BY admitted_seq LIMIT 1",
                params![session_id, delivery.as_str()],
                decode_stored_input,
            )
            .optional()
            .map_err(open::map_error)?,
        None => transaction
            .query_row(
                "SELECT id, session_id, prompt, delivery, state, revision, admitted_seq, \
                        promoted_seq, error, time_created, time_updated \
                 FROM session_input \
                 WHERE session_id = ?1 AND state IN ('queued', 'steering') \
                 ORDER BY admitted_seq LIMIT 1",
                [session_id],
                decode_stored_input,
            )
            .optional()
            .map_err(open::map_error)?,
    };
    stored.map(decode_input).transpose()
}

fn promote_selected(
    transaction: &Transaction<'_>,
    session_id: &str,
    input: Option<SessionInput>,
) -> Result<Option<SessionInput>, DbError> {
    let Some(mut input) = input else {
        return Ok(None);
    };
    let previous_revision = input.revision;
    input.state = SubmissionState::Promoted;
    input.revision = input.revision.saturating_add(1);
    input.time_updated = crate::message::now_millis();
    let event = append_in(
        transaction,
        session_id,
        NewSessionEvent::new("session.input.promoted", event_properties(&input, false))?,
    )?;
    let changed = transaction
        .execute(
            "UPDATE session_input SET promoted_seq = ?1, state = ?2, revision = ?3, \
             time_updated = ?4 WHERE id = ?5 AND session_id = ?6 AND revision = ?7 \
             AND state IN ('queued', 'steering')",
            params![
                event.sequence,
                input.state.as_str(),
                input.revision,
                input.time_updated,
                input.id,
                session_id,
                previous_revision,
            ],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        return Err(conflict(
            &input.id,
            "state or revision changed while the input was being promoted",
        ));
    }
    input.promoted_sequence = Some(event.sequence);
    Ok(Some(input))
}

struct StoredInput {
    id: String,
    session_id: String,
    prompt: String,
    delivery: String,
    state: String,
    revision: i64,
    admitted_sequence: i64,
    promoted_sequence: Option<i64>,
    error: Option<String>,
    time_created: i64,
    time_updated: i64,
}

fn decode_stored_input(row: &Row<'_>) -> rusqlite::Result<StoredInput> {
    Ok(StoredInput {
        id: row.get(0)?,
        session_id: row.get(1)?,
        prompt: row.get(2)?,
        delivery: row.get(3)?,
        state: row.get(4)?,
        revision: row.get(5)?,
        admitted_sequence: row.get(6)?,
        promoted_sequence: row.get(7)?,
        error: row.get(8)?,
        time_created: row.get(9)?,
        time_updated: row.get(10)?,
    })
}

fn decode_input(stored: StoredInput) -> Result<SessionInput, DbError> {
    Ok(SessionInput {
        id: stored.id,
        session_id: stored.session_id,
        prompt: serde_json::from_str(&stored.prompt).map_err(query_error)?,
        delivery: InputDelivery::parse(&stored.delivery)?,
        state: SubmissionState::parse(&stored.state)?,
        revision: stored.revision,
        admitted_sequence: stored.admitted_sequence,
        promoted_sequence: stored.promoted_sequence,
        error: stored.error,
        time_created: stored.time_created,
        time_updated: stored.time_updated,
    })
}

fn event_properties_new(input: &NewSessionInput) -> Map<String, Value> {
    [
        ("inputID".to_owned(), Value::String(input.id.clone())),
        (
            "sessionID".to_owned(),
            Value::String(input.session_id.clone()),
        ),
        ("prompt".to_owned(), input.prompt.clone()),
        (
            "delivery".to_owned(),
            Value::String(input.delivery.as_str().to_owned()),
        ),
        (
            "state".to_owned(),
            Value::String(input.delivery.admitted_state().as_str().to_owned()),
        ),
        ("revision".to_owned(), Value::Number(1.into())),
        (
            "timeCreated".to_owned(),
            Value::Number(input.time_created.into()),
        ),
    ]
    .into_iter()
    .collect()
}

fn event_properties(input: &SessionInput, include_prompt: bool) -> Map<String, Value> {
    let mut properties = [
        ("inputID".to_owned(), Value::String(input.id.clone())),
        (
            "sessionID".to_owned(),
            Value::String(input.session_id.clone()),
        ),
        (
            "delivery".to_owned(),
            Value::String(input.delivery.as_str().to_owned()),
        ),
        (
            "state".to_owned(),
            Value::String(input.state.as_str().to_owned()),
        ),
        ("revision".to_owned(), Value::Number(input.revision.into())),
        (
            "timeUpdated".to_owned(),
            Value::Number(input.time_updated.into()),
        ),
    ]
    .into_iter()
    .collect::<Map<_, _>>();
    if include_prompt {
        properties.insert("prompt".to_owned(), input.prompt.clone());
    }
    if let Some(error) = &input.error {
        properties.insert("error".to_owned(), Value::String(error.clone()));
    }
    properties
}

fn select_by_id(
    connection: &rusqlite::Connection,
    session_id: &str,
    input_id: &str,
) -> Result<Option<SessionInput>, DbError> {
    connection
        .query_row(
            "SELECT id, session_id, prompt, delivery, state, revision, admitted_seq, \
                    promoted_seq, error, time_created, time_updated \
             FROM session_input WHERE session_id = ?1 AND id = ?2",
            params![session_id, input_id],
            decode_stored_input,
        )
        .optional()
        .map_err(open::map_error)?
        .map(decode_input)
        .transpose()
}

/// Read one input through a caller-owned SQLite connection or transaction.
///
/// This is the transactional counterpart to [`SessionInbox::get`]. It lets a
/// driver persist the model-visible message and settle the matching inbox row
/// against the exact same database snapshot.
pub fn read_in(
    connection: &rusqlite::Connection,
    session_id: &str,
    input_id: &str,
) -> Result<Option<SessionInput>, DbError> {
    select_by_id(connection, session_id, input_id)
}

/// Read pending inputs through a caller-owned SQLite connection or transaction.
pub fn pending_in(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<SessionInput>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT id, session_id, prompt, delivery, state, revision, admitted_seq, \
                    promoted_seq, error, time_created, time_updated \
             FROM session_input \
             WHERE session_id = ?1 AND state IN ('queued', 'steering') \
             ORDER BY admitted_seq",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([session_id], decode_stored_input)
        .map_err(open::map_error)?;
    rows.map(|row| row.map_err(open::map_error).and_then(decode_input))
        .collect()
}

/// Read every input that has not yet become model-visible history, in admission order.
///
/// This covers `queued`, `steering`, and `promoted` rows: everything a transcript
/// revert must retire because its prompt was aimed at the discarded tail.
/// Consumed, cancelled, and failed rows are settled history and are excluded.
pub(crate) fn unconsumed_in(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<SessionInput>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT id, session_id, prompt, delivery, state, revision, admitted_seq, \
                    promoted_seq, error, time_created, time_updated \
             FROM session_input \
             WHERE session_id = ?1 AND state IN ('queued', 'steering', 'promoted') \
             ORDER BY admitted_seq",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([session_id], decode_stored_input)
        .map_err(open::map_error)?;
    rows.map(|row| row.map_err(open::map_error).and_then(decode_input))
        .collect()
}

fn edit_pending_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    input_id: &str,
    expected_revision: i64,
    prompt: Value,
    time_updated: i64,
) -> Result<SessionInput, DbError> {
    let mut input = require_pending_revision(transaction, session_id, input_id, expected_revision)?;
    let encoded = serde_json::to_string(&prompt).map_err(query_error)?;
    input.prompt = prompt;
    input.revision = input.revision.saturating_add(1);
    input.time_updated = time_updated.max(input.time_updated);
    append_in(
        transaction,
        session_id,
        NewSessionEvent::new("session.input.edited", event_properties(&input, true))?,
    )?;
    let changed = transaction
        .execute(
            "UPDATE session_input SET prompt = ?1, revision = ?2, time_updated = ?3 \
             WHERE session_id = ?4 AND id = ?5 AND revision = ?6 \
             AND state IN ('queued', 'steering')",
            params![
                encoded,
                input.revision,
                input.time_updated,
                session_id,
                input_id,
                expected_revision,
            ],
        )
        .map_err(open::map_error)?;
    require_changed(input_id, changed)?;
    Ok(input)
}

fn cancel_pending_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    input_id: &str,
    expected_revision: i64,
    time_updated: i64,
) -> Result<SessionInput, DbError> {
    let mut input = require_pending_revision(transaction, session_id, input_id, expected_revision)?;
    input.state = SubmissionState::Cancelled;
    input.revision = input.revision.saturating_add(1);
    input.time_updated = time_updated.max(input.time_updated);
    append_in(
        transaction,
        session_id,
        NewSessionEvent::new("session.input.cancelled", event_properties(&input, false))?,
    )?;
    let changed = transaction
        .execute(
            "UPDATE session_input SET state = ?1, revision = ?2, time_updated = ?3 \
             WHERE session_id = ?4 AND id = ?5 AND revision = ?6 \
             AND state IN ('queued', 'steering')",
            params![
                input.state.as_str(),
                input.revision,
                input.time_updated,
                session_id,
                input_id,
                expected_revision,
            ],
        )
        .map_err(open::map_error)?;
    require_changed(input_id, changed)?;
    Ok(input)
}

fn require_pending_revision(
    transaction: &Transaction<'_>,
    session_id: &str,
    input_id: &str,
    expected_revision: i64,
) -> Result<SessionInput, DbError> {
    let input =
        select_by_id(transaction, session_id, input_id)?.ok_or_else(|| DbError::NotFound {
            table: "session_input".to_owned(),
            id: input_id.to_owned(),
        })?;
    if !input.state.is_pending() {
        return Err(conflict(
            input_id,
            format!("submission is already {}", input.state.as_str()),
        ));
    }
    if input.revision != expected_revision {
        return Err(conflict(
            input_id,
            format!(
                "revision conflict: expected {expected_revision}, found {}",
                input.revision
            ),
        ));
    }
    Ok(input)
}

/// Move one input to `target` when it is currently in an `allowed` state.
///
/// The transition logs `event_type` and advances the row's revision under an
/// optimistic guard. It returns `None` when the row is missing or not in an
/// allowed state, so callers that sweep a session — a transcript revert retiring
/// every unconsumed input, for example — can skip settled history without a
/// second read.
pub(crate) fn transition_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    input_id: &str,
    allowed: &[SubmissionState],
    target: SubmissionState,
    error: Option<&str>,
    event_type: &str,
) -> Result<Option<SessionInput>, DbError> {
    let Some(mut input) = select_by_id(transaction, session_id, input_id)? else {
        return Ok(None);
    };
    if !allowed.contains(&input.state) {
        return Ok(None);
    }
    let previous_revision = input.revision;
    input.state = target;
    input.revision = input.revision.saturating_add(1);
    input.error = error.map(str::to_owned);
    input.time_updated = crate::message::now_millis().max(input.time_updated);
    append_in(
        transaction,
        session_id,
        NewSessionEvent::new(event_type, event_properties(&input, false))?,
    )?;
    let changed = transaction
        .execute(
            "UPDATE session_input SET state = ?1, revision = ?2, error = ?3, time_updated = ?4 \
             WHERE session_id = ?5 AND id = ?6 AND revision = ?7",
            params![
                input.state.as_str(),
                input.revision,
                input.error,
                input.time_updated,
                session_id,
                input_id,
                previous_revision,
            ],
        )
        .map_err(open::map_error)?;
    require_changed(input_id, changed)?;
    Ok(Some(input))
}

/// Mark a promoted input consumed inside the caller transaction.
pub fn mark_consumed_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    input_id: &str,
) -> Result<Option<SessionInput>, DbError> {
    transition_in(
        transaction,
        session_id,
        input_id,
        &[SubmissionState::Promoted],
        SubmissionState::Consumed,
        None,
        "session.input.consumed",
    )
}

/// Supersede an unconsumed input inside the caller transaction.
///
/// Reconciliation uses this to retire an uncertain report before atomically
/// admitting the authoritative replacement. A consumed report remains immutable
/// history and therefore returns `None`.
pub(crate) fn supersede_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    input_id: &str,
) -> Result<Option<SessionInput>, DbError> {
    transition_in(
        transaction,
        session_id,
        input_id,
        &[
            SubmissionState::Queued,
            SubmissionState::Steering,
            SubmissionState::Promoted,
        ],
        SubmissionState::Cancelled,
        None,
        "session.input.superseded",
    )
}

/// Return an orphaned promoted input to its admitted delivery lane.
///
/// Recovery reuses the original row and admission event. Repeating it after the
/// first successful transition is a no-op, so a process restart cannot create a
/// second report or a second recovery event.
pub(crate) fn recover_promoted_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    input_id: &str,
) -> Result<Option<SessionInput>, DbError> {
    let Some(mut input) = select_by_id(transaction, session_id, input_id)? else {
        return Ok(None);
    };
    match input.state {
        SubmissionState::Queued | SubmissionState::Steering => return Ok(Some(input)),
        SubmissionState::Promoted => {}
        SubmissionState::Admitting
        | SubmissionState::Consumed
        | SubmissionState::Cancelled
        | SubmissionState::Failed => return Ok(None),
    }

    let previous_revision = input.revision;
    input.state = input.delivery.admitted_state();
    input.revision = input.revision.saturating_add(1);
    input.promoted_sequence = None;
    input.time_updated = crate::message::now_millis().max(input.time_updated);
    append_in(
        transaction,
        session_id,
        NewSessionEvent::new("session.input.recovered", event_properties(&input, false))?,
    )?;
    let changed = transaction
        .execute(
            "UPDATE session_input \
             SET state = ?1, revision = ?2, promoted_seq = NULL, time_updated = ?3 \
             WHERE session_id = ?4 AND id = ?5 AND revision = ?6 AND state = 'promoted'",
            params![
                input.state.as_str(),
                input.revision,
                input.time_updated,
                session_id,
                input_id,
                previous_revision,
            ],
        )
        .map_err(open::map_error)?;
    require_changed(input_id, changed)?;
    Ok(Some(input))
}

fn require_changed(input_id: &str, changed: usize) -> Result<(), DbError> {
    if changed == 1 {
        return Ok(());
    }
    Err(conflict(input_id, "submission changed concurrently"))
}

fn conflict(input_id: &str, detail: impl Into<String>) -> DbError {
    DbError::Conflict {
        table: "session_input".to_owned(),
        id: input_id.to_owned(),
        detail: detail.into(),
    }
}
