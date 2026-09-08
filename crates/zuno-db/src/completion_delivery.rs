//! Exactly-once arbitration between asynchronous callbacks and synchronous waits.

use crate::inbox::{NewSessionInput, SessionInput, admit_in};
use crate::{Pool, open};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::execution::{CompletionEnvelope, CompletionSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionOwner {
    Callback,
    Inline,
}

impl CompletionOwner {
    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "callback" => Ok(Self::Callback),
            "inline" => Ok(Self::Inline),
            _ => Err(query_error(format!("unknown completion owner `{value}`"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompletionDelivery {
    pub envelope: CompletionEnvelope,
    pub owner: Option<CompletionOwner>,
    pub input_id: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
}

#[derive(Clone)]
pub struct CompletionDeliveryStore {
    pool: Arc<Pool>,
}

impl CompletionDeliveryStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn publish(
        &self,
        envelope: CompletionEnvelope,
        at_ms: i64,
    ) -> Result<CompletionDelivery, DbError> {
        self.pool
            .transaction(|transaction| publish_in(transaction, envelope, at_ms))
    }

    pub fn claim_inline(
        &self,
        source_key: &str,
        at_ms: i64,
    ) -> Result<Option<CompletionDelivery>, DbError> {
        self.pool
            .transaction(|transaction| claim_inline_in(transaction, source_key, at_ms))
    }

    pub fn claim_callback(
        &self,
        source_key: &str,
        input: NewSessionInput,
        at_ms: i64,
    ) -> Result<Option<(CompletionDelivery, SessionInput)>, DbError> {
        self.pool
            .transaction(|transaction| claim_callback_in(transaction, source_key, input, at_ms))
    }

    pub fn get(&self, source_key: &str) -> Result<Option<CompletionDelivery>, DbError> {
        let connection = self.pool.get()?;
        read_in(&connection, source_key)
    }

    /// List terminal completions this session has published but no consumer owns.
    pub fn unclaimed_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<CompletionDelivery>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(
                "SELECT source_key, session_id, source, terminal_revision, cycle_id, payload, \
                        owner, input_id, time_created, time_updated \
                 FROM completion_delivery \
                 WHERE session_id = ?1 AND owner IS NULL \
                 ORDER BY time_created, source_key",
            )
            .map_err(open::map_error)?;
        let rows = statement
            .query_map([session_id], decode_stored)
            .map_err(open::map_error)?;
        rows.map(|row| row.map_err(open::map_error).and_then(decode))
            .collect()
    }
}

pub fn publish_in(
    transaction: &Transaction<'_>,
    envelope: CompletionEnvelope,
    at_ms: i64,
) -> Result<CompletionDelivery, DbError> {
    validate_envelope(&envelope)?;
    if let Some(existing) = read_in(transaction, &envelope.source_key)? {
        if existing.envelope != envelope {
            return Err(DbError::Conflict {
                table: "completion_delivery".to_owned(),
                id: envelope.source_key,
                detail: "source key was reused for a different terminal completion".to_owned(),
            });
        }
        return Ok(existing);
    }
    let payload =
        serde_json::to_string(&envelope.payload).map_err(|error| query_error(error.to_string()))?;
    let terminal_revision = i64::try_from(envelope.terminal_revision)
        .map_err(|_| query_error("completion terminal revision exceeds SQLite INTEGER"))?;
    transaction
        .execute(
            "INSERT INTO completion_delivery \
             (source_key, session_id, source, terminal_revision, cycle_id, payload, owner, \
              input_id, time_created, time_updated) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, ?7)",
            params![
                envelope.source_key,
                envelope.parent_session_id,
                completion_source_str(envelope.source),
                terminal_revision,
                envelope.cycle_id,
                payload,
                at_ms,
            ],
        )
        .map_err(open::map_error)?;
    Ok(CompletionDelivery {
        envelope,
        owner: None,
        input_id: None,
        time_created: at_ms,
        time_updated: at_ms,
    })
}

pub fn claim_inline_in(
    transaction: &Transaction<'_>,
    source_key: &str,
    at_ms: i64,
) -> Result<Option<CompletionDelivery>, DbError> {
    let Some(mut delivery) = read_in(transaction, source_key)? else {
        return Ok(None);
    };
    if delivery.owner.is_some() {
        return Ok(None);
    }
    let changed = transaction
        .execute(
            "UPDATE completion_delivery SET owner = 'inline', time_updated = ?1 \
             WHERE source_key = ?2 AND owner IS NULL",
            params![at_ms, source_key],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        return Ok(None);
    }
    delivery.owner = Some(CompletionOwner::Inline);
    delivery.time_updated = at_ms;
    Ok(Some(delivery))
}

pub fn claim_callback_in(
    transaction: &Transaction<'_>,
    source_key: &str,
    mut input: NewSessionInput,
    at_ms: i64,
) -> Result<Option<(CompletionDelivery, SessionInput)>, DbError> {
    let Some(mut delivery) = read_in(transaction, source_key)? else {
        return Ok(None);
    };
    if delivery.owner.is_some() {
        return Ok(None);
    }
    if input.session_id != delivery.envelope.parent_session_id {
        return Err(query_error(
            "completion callback input belongs to another parent session",
        ));
    }
    input.source_key = Some(source_key.to_owned());
    let input = admit_in(transaction, input)?;
    let changed = transaction
        .execute(
            "UPDATE completion_delivery SET owner = 'callback', input_id = ?1, time_updated = ?2 \
             WHERE source_key = ?3 AND owner IS NULL",
            params![input.id, at_ms, source_key],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        return Err(DbError::Conflict {
            table: "completion_delivery".to_owned(),
            id: source_key.to_owned(),
            detail: "completion delivery owner changed during callback claim".to_owned(),
        });
    }
    delivery.owner = Some(CompletionOwner::Callback);
    delivery.input_id = Some(input.id.clone());
    delivery.time_updated = at_ms;
    Ok(Some((delivery, input)))
}

fn read_in(
    connection: &rusqlite::Connection,
    source_key: &str,
) -> Result<Option<CompletionDelivery>, DbError> {
    connection
        .query_row(
            "SELECT source_key, session_id, source, terminal_revision, cycle_id, payload, \
                    owner, input_id, time_created, time_updated \
             FROM completion_delivery WHERE source_key = ?1",
            [source_key],
            decode_stored,
        )
        .optional()
        .map_err(open::map_error)?
        .map(decode)
        .transpose()
}

struct StoredDelivery {
    source_key: String,
    session_id: String,
    source: String,
    terminal_revision: i64,
    cycle_id: Option<String>,
    payload: String,
    owner: Option<String>,
    input_id: Option<String>,
    time_created: i64,
    time_updated: i64,
}

fn decode_stored(row: &Row<'_>) -> rusqlite::Result<StoredDelivery> {
    Ok(StoredDelivery {
        source_key: row.get(0)?,
        session_id: row.get(1)?,
        source: row.get(2)?,
        terminal_revision: row.get(3)?,
        cycle_id: row.get(4)?,
        payload: row.get(5)?,
        owner: row.get(6)?,
        input_id: row.get(7)?,
        time_created: row.get(8)?,
        time_updated: row.get(9)?,
    })
}

fn decode(stored: StoredDelivery) -> Result<CompletionDelivery, DbError> {
    Ok(CompletionDelivery {
        envelope: CompletionEnvelope {
            source_key: stored.source_key,
            source: parse_completion_source(&stored.source)?,
            terminal_revision: u64::try_from(stored.terminal_revision)
                .map_err(|_| query_error("stored completion revision is negative"))?,
            parent_session_id: stored.session_id,
            cycle_id: stored.cycle_id,
            payload: serde_json::from_str(&stored.payload)
                .map_err(|error| query_error(error.to_string()))?,
        },
        owner: stored
            .owner
            .map(|owner| CompletionOwner::parse(&owner))
            .transpose()?,
        input_id: stored.input_id,
        time_created: stored.time_created,
        time_updated: stored.time_updated,
    })
}

fn validate_envelope(envelope: &CompletionEnvelope) -> Result<(), DbError> {
    if envelope.source_key.trim().is_empty() || envelope.source_key.chars().count() > 512 {
        return Err(query_error(
            "completion source_key must contain 1 to 512 characters",
        ));
    }
    if envelope.parent_session_id.trim().is_empty() {
        return Err(query_error("completion parent session id is required"));
    }
    if envelope
        .cycle_id
        .as_deref()
        .is_some_and(|cycle| cycle.trim().is_empty() || cycle.chars().count() > 128)
    {
        return Err(query_error(
            "completion cycle_id must contain 1 to 128 characters",
        ));
    }
    Ok(())
}

const fn completion_source_str(source: CompletionSource) -> &'static str {
    match source {
        CompletionSource::BackgroundExecution => "background_execution",
        CompletionSource::AgentJob => "agent_job",
        CompletionSource::Workflow => "workflow",
        CompletionSource::ProductAgent => "product_agent",
    }
}

fn parse_completion_source(value: &str) -> Result<CompletionSource, DbError> {
    match value {
        "background_execution" => Ok(CompletionSource::BackgroundExecution),
        "agent_job" => Ok(CompletionSource::AgentJob),
        "workflow" => Ok(CompletionSource::Workflow),
        "product_agent" => Ok(CompletionSource::ProductAgent),
        _ => Err(query_error(format!("unknown completion source `{value}`"))),
    }
}

fn query_error(detail: impl Into<String>) -> DbError {
    DbError::Query {
        source: Box::new(std::io::Error::other(detail.into())),
    }
}
