use std::sync::{Arc, OnceLock};

use oc_db::{Pool, TransactionBehavior, migration, open};
use oc_error::DbError;
use rusqlite::OptionalExtension;
use serde_json::{Map, Value};

use super::{EventCursor, EventStreamError, NewEvent, StreamEvent};

pub(super) struct Store {
    pool: Arc<Pool>,
    subscriber_capacity: usize,
    initialized: OnceLock<()>,
}

pub(super) struct Snapshot {
    pub(super) events: Vec<StreamEvent>,
    pub(super) boundary: i64,
}

struct StoredRow {
    id: String,
    sequence: i64,
    event_type: String,
    data: String,
}

impl Store {
    pub(super) const fn new(pool: Arc<Pool>, subscriber_capacity: usize) -> Self {
        Self {
            pool,
            subscriber_capacity,
            initialized: OnceLock::new(),
        }
    }

    pub(super) const fn subscriber_capacity(&self) -> usize {
        self.subscriber_capacity
    }

    pub(super) fn append(
        &self,
        session_id: &str,
        event: NewEvent,
    ) -> Result<StreamEvent, EventStreamError> {
        self.ensure_initialized()?;
        let aggregate_id = session_id.to_owned();
        let id = format!("evt_{}", uuid::Uuid::now_v7().simple());
        let data = serde_json::to_string(&event.properties)?;
        let stored_type = format!("{}.1", event.event_type);
        let sequence = self.pool.transaction(|transaction| {
            let latest = latest_sequence(transaction, &aggregate_id)?;
            let sequence = latest.checked_add(1).ok_or_else(|| DbError::Query {
                source: Box::new(std::io::Error::other("event sequence exhausted")),
            })?;
            transaction
                .execute(
                    "INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?1, ?2, NULL) \
                     ON CONFLICT(aggregate_id) DO UPDATE SET seq = excluded.seq",
                    rusqlite::params![aggregate_id, sequence],
                )
                .map_err(open::map_error)?;
            transaction
                .execute(
                    "INSERT INTO event (id, aggregate_id, seq, type, data) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        id,
                        aggregate_id,
                        sequence,
                        stored_type,
                        data
                    ],
                )
                .map_err(open::map_error)?;
            Ok(sequence)
        })?;
        Ok(StreamEvent {
            cursor: EventCursor {
                session_id: session_id.to_owned(),
                sequence,
            },
            id,
            event_type: event.event_type,
            version: 1,
            properties: event.properties,
        })
    }

    pub(super) fn replay(
        &self,
        session_id: &str,
        after: Option<i64>,
    ) -> Result<Vec<StreamEvent>, EventStreamError> {
        Ok(self.snapshot(session_id, after)?.events)
    }

    pub(super) fn snapshot(
        &self,
        session_id: &str,
        after: Option<i64>,
    ) -> Result<Snapshot, EventStreamError> {
        self.ensure_initialized()?;
        let aggregate_id = session_id.to_owned();
        let after = after.unwrap_or(-1);
        let (boundary, rows) =
            self.pool
                .transaction_with_behavior(TransactionBehavior::Deferred, |transaction| {
                    let boundary = latest_sequence(transaction, &aggregate_id)?;
                    let mut statement = transaction
                        .prepare(
                            "SELECT id, seq, type, data FROM event \
                         WHERE aggregate_id = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq ASC",
                        )
                        .map_err(open::map_error)?;
                    let rows = statement
                        .query_map(rusqlite::params![aggregate_id, after, boundary], |row| {
                            Ok(StoredRow {
                                id: row.get(0)?,
                                sequence: row.get(1)?,
                                event_type: row.get(2)?,
                                data: row.get(3)?,
                            })
                        })
                        .map_err(open::map_error)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(open::map_error)?;
                    Ok((boundary, rows))
                })?;
        let events = rows
            .into_iter()
            .map(|row| decode_row(session_id, row))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Snapshot { events, boundary })
    }

    fn ensure_initialized(&self) -> Result<(), DbError> {
        if self.initialized.get().is_some() {
            return Ok(());
        }
        let mut connection = self.pool.get()?;
        migration::apply(&mut connection)?;
        self.initialized.get_or_init(|| ());
        Ok(())
    }
}

fn latest_sequence(
    transaction: &oc_db::Transaction<'_>,
    aggregate_id: &str,
) -> Result<i64, DbError> {
    transaction
        .query_row(
            "SELECT seq FROM event_sequence WHERE aggregate_id = ?1",
            [aggregate_id],
            |row| row.get(0),
        )
        .optional()
        .map(|sequence| sequence.unwrap_or(-1))
        .map_err(open::map_error)
}

fn decode_row(session_id: &str, row: StoredRow) -> Result<StreamEvent, EventStreamError> {
    let properties = serde_json::from_str::<Map<String, Value>>(&row.data).map_err(|source| {
        DbError::Decode {
            table: "event".to_owned(),
            source,
        }
    })?;
    let (event_type, version) = row
        .event_type
        .rsplit_once('.')
        .and_then(|(event_type, version)| {
            version
                .parse::<u32>()
                .ok()
                .map(|version| (event_type.to_owned(), version))
        })
        .unwrap_or((row.event_type, 1));
    Ok(StreamEvent {
        cursor: EventCursor {
            session_id: session_id.to_owned(),
            sequence: row.sequence,
        },
        id: row.id,
        event_type,
        version,
        properties,
    })
}
