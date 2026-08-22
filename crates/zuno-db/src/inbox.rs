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
    /// Inject at the next safe point of an active turn.
    Steer,
    /// Start or join the next driver step.
    NextStep,
}

impl InputDelivery {
    fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::NextStep => "next-step",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "steer" => Ok(Self::Steer),
            "next-step" => Ok(Self::NextStep),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown input delivery `{value}`"
            )))),
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
    /// Sequence of the admission event.
    pub admitted_sequence: i64,
    /// Sequence of the promotion event, once claimed.
    pub promoted_sequence: Option<i64>,
    /// Creation timestamp in milliseconds since the Unix epoch.
    pub time_created: i64,
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
                    "SELECT id, session_id, prompt, delivery, admitted_seq, \
                            promoted_seq, time_created \
                     FROM session_input \
                     WHERE session_id = ?1 AND id = ?2 AND promoted_seq IS NULL",
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
        let mut statement = connection
            .prepare(
                "SELECT id, session_id, prompt, delivery, admitted_seq, \
                        promoted_seq, time_created \
                 FROM session_input \
                 WHERE session_id = ?1 AND promoted_seq IS NULL \
                 ORDER BY admitted_seq",
            )
            .map_err(open::map_error)?;
        let rows = statement
            .query_map([session_id], decode_stored_input)
            .map_err(open::map_error)?;
        rows.map(|row| row.map_err(open::map_error).and_then(decode_input))
            .collect()
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
    transaction
        .execute(
            "INSERT INTO session_input \
             (id, session_id, prompt, delivery, admitted_seq, promoted_seq, time_created) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
            params![
                input.id,
                input.session_id,
                prompt,
                input.delivery.as_str(),
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
        admitted_sequence: event.sequence,
        promoted_sequence: None,
        time_created: input.time_created,
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
                "SELECT id, session_id, prompt, delivery, admitted_seq, \
                        promoted_seq, time_created \
                 FROM session_input \
                 WHERE session_id = ?1 AND promoted_seq IS NULL AND delivery = ?2 \
                 ORDER BY admitted_seq LIMIT 1",
                params![session_id, delivery.as_str()],
                decode_stored_input,
            )
            .optional()
            .map_err(open::map_error)?,
        None => transaction
            .query_row(
                "SELECT id, session_id, prompt, delivery, admitted_seq, \
                        promoted_seq, time_created \
                 FROM session_input \
                 WHERE session_id = ?1 AND promoted_seq IS NULL \
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
    let event = append_in(
        transaction,
        session_id,
        NewSessionEvent::new("session.input.promoted", event_properties(&input, false))?,
    )?;
    let changed = transaction
        .execute(
            "UPDATE session_input SET promoted_seq = ?1 \
             WHERE id = ?2 AND promoted_seq IS NULL",
            params![event.sequence, input.id],
        )
        .map_err(open::map_error)?;
    if changed == 0 {
        return Ok(None);
    }
    input.promoted_sequence = Some(event.sequence);
    Ok(Some(input))
}

struct StoredInput {
    id: String,
    session_id: String,
    prompt: String,
    delivery: String,
    admitted_sequence: i64,
    promoted_sequence: Option<i64>,
    time_created: i64,
}

fn decode_stored_input(row: &Row<'_>) -> rusqlite::Result<StoredInput> {
    Ok(StoredInput {
        id: row.get(0)?,
        session_id: row.get(1)?,
        prompt: row.get(2)?,
        delivery: row.get(3)?,
        admitted_sequence: row.get(4)?,
        promoted_sequence: row.get(5)?,
        time_created: row.get(6)?,
    })
}

fn decode_input(stored: StoredInput) -> Result<SessionInput, DbError> {
    Ok(SessionInput {
        id: stored.id,
        session_id: stored.session_id,
        prompt: serde_json::from_str(&stored.prompt).map_err(query_error)?,
        delivery: InputDelivery::parse(&stored.delivery)?,
        admitted_sequence: stored.admitted_sequence,
        promoted_sequence: stored.promoted_sequence,
        time_created: stored.time_created,
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
    ]
    .into_iter()
    .collect::<Map<_, _>>();
    if include_prompt {
        properties.insert("prompt".to_owned(), input.prompt.clone());
    }
    properties
}
