//! Append-only, per-session event log shared by runtime capabilities.

use crate::{Pool, open};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::{Map, Value};
use std::error::Error;
use std::sync::Arc;
use uuid::Uuid;
use zuno_error::DbError;

const EVENT_VERSION: u32 = 1;

/// One event waiting to be appended to a session stream.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSessionEvent {
    /// Stable event type without a version suffix.
    pub event_type: String,
    /// JSON properties stored with the event.
    pub properties: Map<String, Value>,
}

impl NewSessionEvent {
    /// Create a validated event.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Query`] when the type is empty or contains a line
    /// break.
    pub fn new(
        event_type: impl Into<String>,
        properties: Map<String, Value>,
    ) -> Result<Self, DbError> {
        let event_type = event_type.into();
        validate_event_type(&event_type)?;
        Ok(Self {
            event_type,
            properties,
        })
    }
}

fn validate_event_type(event_type: &str) -> Result<(), DbError> {
    if event_type.is_empty() || event_type.contains(['\r', '\n']) {
        return Err(query_error(std::io::Error::other(
            "invalid session event type",
        )));
    }
    Ok(())
}

/// One committed event in a session's monotonic stream.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEvent {
    /// Opaque event identifier.
    pub id: String,
    /// Session stream that owns the event.
    pub session_id: String,
    /// Zero-based sequence within the session stream.
    pub sequence: i64,
    /// Stable event type without its stored version suffix.
    pub event_type: String,
    /// Stored event schema version.
    pub version: u32,
    /// JSON properties stored with the event.
    pub properties: Map<String, Value>,
}

/// Append and replay access to durable session events.
#[derive(Clone)]
pub struct SessionEventLog {
    pool: Arc<Pool>,
}

impl SessionEventLog {
    /// Open the log over one initialized pool.
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Append one event atomically.
    ///
    /// # Errors
    ///
    /// Returns a database error when the sequence cannot be allocated, the
    /// properties cannot be encoded, or SQLite rejects the transaction.
    pub fn append(
        &self,
        session_id: &str,
        event: NewSessionEvent,
    ) -> Result<SessionEvent, DbError> {
        self.pool
            .transaction(|transaction| append_in(transaction, session_id, event))
    }

    /// Read events strictly after `sequence`, or from the start for `None`.
    ///
    /// # Errors
    ///
    /// Returns a database error when the query fails or a committed event
    /// cannot be decoded.
    pub fn read_after(
        &self,
        session_id: &str,
        sequence: Option<i64>,
    ) -> Result<Vec<SessionEvent>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(
                "SELECT id, aggregate_id, seq, type, data \
                 FROM event \
                 WHERE aggregate_id = ?1 AND seq > COALESCE(?2, -1) \
                 ORDER BY seq",
            )
            .map_err(open::map_error)?;
        let rows = statement
            .query_map(params![session_id, sequence], decode_stored_row)
            .map_err(open::map_error)?;

        rows.map(|row| row.map_err(open::map_error).and_then(decode_event))
            .collect()
    }

    /// Read every stored version of one event type strictly after `sequence`,
    /// or from the start for `None`.
    ///
    /// See [`read_of_type_after_in`] for the version rule.
    ///
    /// # Errors
    ///
    /// Returns a database error when `event_type` is invalid, the query fails,
    /// or a committed event cannot be decoded.
    pub fn read_of_type_after(
        &self,
        session_id: &str,
        event_type: &str,
        sequence: Option<i64>,
    ) -> Result<Vec<SessionEvent>, DbError> {
        let connection = self.pool.get()?;
        read_of_type_after_in(&connection, session_id, event_type, sequence)
    }

    /// The newest event of one type across every stored version, if any.
    ///
    /// See [`latest_of_type_in`].
    ///
    /// # Errors
    ///
    /// Returns a database error when `event_type` is invalid, the query fails,
    /// or a committed event cannot be decoded.
    pub fn latest_of_type(
        &self,
        session_id: &str,
        event_type: &str,
    ) -> Result<Option<SessionEvent>, DbError> {
        let connection = self.pool.get()?;
        latest_of_type_in(&connection, session_id, event_type)
    }
}

/// Read every stored version of `event_type` strictly after `sequence` through a
/// caller-owned connection or transaction.
///
/// Stored types are `<type>.<version>`, so the query bounds the indexed `type`
/// column to the `<type>.` prefix — served by the `(aggregate_id, type, seq)`
/// index — and then keeps only rows whose decoded type is exactly `event_type`.
/// Every version ever written for the type is returned, not only the version
/// this build appends, so a projection rebuilt after an event-schema bump still
/// sees the events an older release logged.
///
/// # Errors
///
/// Returns a database error when `event_type` is invalid, the query fails, or a
/// committed event cannot be decoded.
pub fn read_of_type_after_in(
    connection: &Connection,
    session_id: &str,
    event_type: &str,
    sequence: Option<i64>,
) -> Result<Vec<SessionEvent>, DbError> {
    let (lower, upper) = type_bounds(event_type)?;
    let mut statement = connection
        .prepare(
            "SELECT id, aggregate_id, seq, type, data \
             FROM event \
             WHERE aggregate_id = ?1 AND type >= ?2 AND type < ?3 \
               AND seq > COALESCE(?4, -1) \
             ORDER BY seq",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map(
            params![session_id, lower, upper, sequence],
            decode_stored_row,
        )
        .map_err(open::map_error)?;
    rows.map(|row| row.map_err(open::map_error).and_then(decode_event))
        .filter(|event| match event {
            Ok(event) => event.event_type == event_type,
            Err(_) => true,
        })
        .collect()
}

/// The newest event of `event_type` across every stored version, through a
/// caller-owned connection or transaction.
///
/// Rows are visited newest first and the first whose decoded type is exactly
/// `event_type` wins, so a sibling type that merely shares the prefix can never
/// shadow the real latest event.
///
/// # Errors
///
/// Returns a database error when `event_type` is invalid, the query fails, or a
/// committed event cannot be decoded.
pub fn latest_of_type_in(
    connection: &Connection,
    session_id: &str,
    event_type: &str,
) -> Result<Option<SessionEvent>, DbError> {
    let (lower, upper) = type_bounds(event_type)?;
    let mut statement = connection
        .prepare(
            "SELECT id, aggregate_id, seq, type, data \
             FROM event \
             WHERE aggregate_id = ?1 AND type >= ?2 AND type < ?3 \
             ORDER BY seq DESC",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map(params![session_id, lower, upper], decode_stored_row)
        .map_err(open::map_error)?;
    for row in rows {
        let event = decode_event(row.map_err(open::map_error)?)?;
        if event.event_type == event_type {
            return Ok(Some(event));
        }
    }
    Ok(None)
}

/// The half-open `type` range holding every stored version of `event_type`.
///
/// `'/'` is the byte after `'.'`, so `[<type>., <type>/)` covers exactly the
/// stored strings with the `<type>.` prefix under SQLite's default `BINARY`
/// collation, whatever version suffix follows.
fn type_bounds(event_type: &str) -> Result<(String, String), DbError> {
    validate_event_type(event_type)?;
    Ok((format!("{event_type}."), format!("{event_type}/")))
}

/// Append one event through a caller-owned connection.
///
/// This is the engine-facing form: the turn loop already owns the session
/// connection and must commit the prompt snapshot before sending the provider
/// request. Opening a second pooled connection here would introduce an avoidable
/// writer race.
///
/// # Errors
///
/// Returns a database error when the transaction cannot begin, append, or commit.
pub fn append_with_connection(
    connection: &mut Connection,
    session_id: &str,
    event: NewSessionEvent,
) -> Result<SessionEvent, DbError> {
    let transaction = open::immediate_transaction(connection)?;
    let appended = append_in(&transaction, session_id, event)?;
    transaction.commit().map_err(open::map_error)?;
    Ok(appended)
}

struct StoredEvent {
    id: String,
    session_id: String,
    sequence: i64,
    stored_type: String,
    data: String,
}

fn decode_stored_row(row: &Row<'_>) -> rusqlite::Result<StoredEvent> {
    Ok(StoredEvent {
        id: row.get(0)?,
        session_id: row.get(1)?,
        sequence: row.get(2)?,
        stored_type: row.get(3)?,
        data: row.get(4)?,
    })
}

pub fn append_in(
    transaction: &Transaction<'_>,
    session_id: &str,
    event: NewSessionEvent,
) -> Result<SessionEvent, DbError> {
    let latest = transaction
        .query_row(
            "SELECT seq FROM event_sequence WHERE aggregate_id = ?1",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(open::map_error)?
        .unwrap_or(-1);
    let sequence = latest
        .checked_add(1)
        .ok_or_else(|| query_error(std::io::Error::other("event sequence exhausted")))?;
    let id = format!("evt_{}", Uuid::now_v7().simple());
    let data = serde_json::to_string(&event.properties).map_err(query_error)?;
    let stored_type = format!("{}.{}", event.event_type, EVENT_VERSION);

    transaction
        .execute(
            "INSERT INTO event_sequence (aggregate_id, seq, owner_id) \
             VALUES (?1, ?2, NULL) \
             ON CONFLICT(aggregate_id) DO UPDATE SET seq = excluded.seq",
            params![session_id, sequence],
        )
        .map_err(open::map_error)?;
    transaction
        .execute(
            "INSERT INTO event (id, aggregate_id, seq, type, data) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, session_id, sequence, stored_type, data],
        )
        .map_err(open::map_error)?;

    Ok(SessionEvent {
        id,
        session_id: session_id.to_owned(),
        sequence,
        event_type: event.event_type,
        version: EVENT_VERSION,
        properties: event.properties,
    })
}

fn decode_event(stored: StoredEvent) -> Result<SessionEvent, DbError> {
    let (event_type, version) = stored
        .stored_type
        .rsplit_once('.')
        .ok_or_else(|| query_error(std::io::Error::other("event type has no version suffix")))?;
    let version = version.parse::<u32>().map_err(query_error)?;
    let properties =
        serde_json::from_str::<Map<String, Value>>(&stored.data).map_err(query_error)?;
    Ok(SessionEvent {
        id: stored.id,
        session_id: stored.session_id,
        sequence: stored.sequence,
        event_type: event_type.to_owned(),
        version,
        properties,
    })
}

pub(crate) fn query_error(error: impl Error + Send + Sync + 'static) -> DbError {
    DbError::Query {
        source: Box::new(error),
    }
}
