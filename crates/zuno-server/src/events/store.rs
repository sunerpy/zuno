use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rusqlite::OptionalExtension;
use serde_json::{Map, Value};
use zuno_db::{Pool, TransactionBehavior, event_log, open};
use zuno_error::DbError;
use zuno_types::context_usage::ContextUsageSnapshot;

use super::{EventCursor, EventStreamError, NewEvent, StreamEvent};

pub(super) struct Store {
    pool: Arc<Pool>,
    subscriber_capacity: usize,
    initialized: OnceLock<()>,
    initializing: Mutex<()>,
}

pub(super) struct Snapshot {
    pub(super) events: Vec<StreamEvent>,
    pub(super) boundary: i64,
    pub(super) context_usage: Option<ContextUsageSnapshot>,
}

pub(super) struct Page {
    pub(super) events: Vec<StreamEvent>,
    pub(super) has_more: bool,
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
            initializing: Mutex::new(()),
        }
    }

    pub(super) const fn subscriber_capacity(&self) -> usize {
        self.subscriber_capacity
    }

    /// The application database this log writes through.
    ///
    /// Exposed for the one caller that has to commit two durable rows of its own in
    /// a single transaction and has no event to attach them to; see
    /// [`super::EventService::application_pool`].
    pub(super) fn pool(&self) -> Arc<Pool> {
        Arc::clone(&self.pool)
    }

    /// Commit one event in its own transaction.
    ///
    /// The row layout, the identifier scheme, the version suffix, and the
    /// sequence upsert all belong to [`zuno_db::event_log`]; this is only the
    /// transaction boundary. A caller that has to commit an event together with
    /// the state that event asserts uses [`Store::append_in`] instead, because a
    /// published event whose state never committed is a durable lie about the
    /// session.
    pub(super) fn append(
        &self,
        session_id: &str,
        event: NewEvent,
    ) -> Result<StreamEvent, EventStreamError> {
        self.ensure_initialized()?;
        let session_id = session_id.to_owned();
        let appended = self.transaction(TransactionBehavior::Immediate, |transaction| {
            Self::append_in(transaction, &session_id, event)
        })?;
        Ok(appended)
    }

    /// Insert one event inside a transaction the caller owns.
    ///
    /// The returned event is only publishable after that transaction commits.
    pub(super) fn append_in(
        transaction: &rusqlite::Transaction<'_>,
        session_id: &str,
        event: NewEvent,
    ) -> Result<StreamEvent, DbError> {
        let appended = event_log::append_in(
            transaction,
            session_id,
            event_log::NewSessionEvent::new(event.event_type, event.properties)?,
        )?;
        Ok(StreamEvent {
            cursor: EventCursor {
                session_id: appended.session_id,
                sequence: appended.sequence,
            },
            id: appended.id,
            event_type: appended.event_type,
            version: appended.version,
            properties: appended.properties,
        })
    }

    /// Commit `mutate` and one event in the same transaction.
    ///
    /// `mutate` runs first, so a failure there rolls the event back with it. The
    /// returned event is committed but not yet published; fan-out is the caller's
    /// step, after this returns.
    pub(super) fn append_with<F>(
        &self,
        session_id: &str,
        event: NewEvent,
        mutate: F,
    ) -> Result<StreamEvent, EventStreamError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<(), DbError>,
    {
        self.ensure_initialized()?;
        let session_id = session_id.to_owned();
        let appended = self.transaction(TransactionBehavior::Immediate, |transaction| {
            mutate(transaction)?;
            Self::append_in(transaction, &session_id, event)
        })?;
        Ok(appended)
    }

    /// Append only a foreground owner's validated context projection.
    pub(super) fn append_context_usage(
        &self,
        snapshot: ContextUsageSnapshot,
    ) -> Result<StreamEvent, EventStreamError> {
        self.ensure_initialized()?;
        snapshot.validate().map_err(|error| DbError::Query {
            source: Box::new(error),
        })?;
        Ok(
            self.transaction(TransactionBehavior::Immediate, |transaction| {
                let session = zuno_db::session::find(transaction, &snapshot.session_id)?
                    .ok_or_else(|| DbError::NotFound {
                        table: "session".to_owned(),
                        id: snapshot.session_id.clone(),
                    })?;
                if snapshot.source != zuno_engine::context_usage::session_context_source(&session) {
                    return Err(DbError::Conflict {
                        table: "session_context_usage".to_owned(),
                        id: snapshot.session_id.clone(),
                        detail: "context snapshot belongs to another request source".to_owned(),
                    });
                }
                let persisted = zuno_db::context_usage::read_source_in(
                    transaction,
                    &snapshot.session_id,
                    snapshot.source,
                )?;
                if persisted.as_ref().is_none_or(|persisted| {
                    snapshot.revision > persisted.snapshot().revision
                        || snapshot.context_epoch > persisted.snapshot().context_epoch
                        || (snapshot.revision == persisted.snapshot().revision
                            && &snapshot != persisted.snapshot())
                }) {
                    return Err(DbError::Conflict {
                        table: "session_context_usage".to_owned(),
                        id: snapshot.session_id.clone(),
                        detail: "context projection has no matching durable state".to_owned(),
                    });
                }
                let revision =
                    i64::try_from(snapshot.revision).map_err(|source| DbError::Query {
                        source: Box::new(source),
                    })?;
                let existing = transaction
                    .query_row(
                        "SELECT id, seq, data FROM event \
                 WHERE aggregate_id = ?1 AND type = 'session.context.usage.1' \
                   AND json_extract(data, '$.snapshot.source') = ?2 \
                   AND json_extract(data, '$.snapshot.revision') = ?3 \
                 ORDER BY seq DESC LIMIT 1",
                        rusqlite::params![snapshot.session_id, snapshot.source.as_str(), revision],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(open::map_error)?;
                if let Some((id, sequence, data)) = existing {
                    let properties: Map<String, Value> =
                        serde_json::from_str(&data).map_err(|source| DbError::Decode {
                            table: "event".to_owned(),
                            source,
                        })?;
                    if properties.get("snapshot") != Some(&serde_json::json!(snapshot)) {
                        return Err(DbError::Conflict {
                            table: "event".to_owned(),
                            id,
                            detail: "context revision has conflicting persisted event data"
                                .to_owned(),
                        });
                    }
                    return Ok(StreamEvent {
                        cursor: EventCursor {
                            session_id: snapshot.session_id.clone(),
                            sequence,
                        },
                        id,
                        event_type: "session.context.usage".to_owned(),
                        version: 1,
                        properties,
                    });
                }
                Self::append_in(
                    transaction,
                    &snapshot.session_id,
                    NewEvent {
                        event_type: "session.context.usage".to_owned(),
                        properties: serde_json::json!({"snapshot": snapshot})
                            .as_object()
                            .expect("fixed snapshot envelope")
                            .clone(),
                    },
                )
            })?,
        )
    }

    /// Whether the session table has a row for `session_id`.
    pub(super) fn session_exists(&self, session_id: &str) -> Result<bool, EventStreamError> {
        self.ensure_initialized()?;
        Ok(self
            .read(|transaction| Ok(zuno_db::session::find(transaction, session_id)?.is_some()))?)
    }

    pub(super) fn replay(
        &self,
        session_id: &str,
        after: Option<i64>,
    ) -> Result<Vec<StreamEvent>, EventStreamError> {
        Ok(self.snapshot(session_id, after)?.events)
    }

    /// Reconstruct question notification coverage after a live receiver lagged.
    pub(super) fn question_sessions(&self) -> Result<Vec<String>, EventStreamError> {
        self.ensure_initialized()?;
        Ok(self.read(|transaction| {
            let mut statement = transaction
                .prepare(
                    "SELECT DISTINCT aggregate_id FROM event \
                 WHERE type LIKE 'question.opened.%' \
                    OR type LIKE 'question.updated.%' \
                    OR type LIKE 'question.authorization.%' \
                 ORDER BY aggregate_id",
                )
                .map_err(open::map_error)?;
            statement
                .query_map([], |row| row.get(0))
                .map_err(open::map_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(open::map_error)
        })?)
    }

    pub(super) fn page(
        &self,
        session_id: &str,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Page, EventStreamError> {
        self.ensure_initialized()?;
        let after = after.unwrap_or(-1);
        let row_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let rows = self.read(|transaction| {
            let mut statement = transaction
                .prepare(
                    "SELECT id, seq, type, data FROM event \
                 WHERE aggregate_id = ?1 AND seq > ?2 AND type NOT LIKE 'session.created.%' \
                 ORDER BY seq ASC LIMIT ?3",
                )
                .map_err(open::map_error)?;
            statement
                .query_map(rusqlite::params![session_id, after, row_limit], |row| {
                    Ok(StoredRow {
                        id: row.get(0)?,
                        sequence: row.get(1)?,
                        event_type: row.get(2)?,
                        data: row.get(3)?,
                    })
                })
                .map_err(open::map_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(open::map_error)
        })?;
        let has_more = rows.len() > limit;
        let events = rows
            .into_iter()
            .take(limit)
            .map(|row| decode_row(session_id, row))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page { events, has_more })
    }

    pub(super) fn snapshot(
        &self,
        session_id: &str,
        after: Option<i64>,
    ) -> Result<Snapshot, EventStreamError> {
        self.ensure_initialized()?;
        let aggregate_id = session_id.to_owned();
        let after = after.unwrap_or(-1);
        let (boundary, rows, context_usage) = self.read(|transaction| {
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
            let context_usage = zuno_db::session::find(transaction, &aggregate_id)?
                .map(|session| -> Result<Option<ContextUsageSnapshot>, DbError> {
                    let source = zuno_engine::context_usage::session_context_source(&session);
                    if let Some(stored) =
                        zuno_db::context_usage::read_source_in(transaction, &aggregate_id, source)?
                    {
                        return Ok(Some(stored.snapshot().clone()));
                    }
                    let recovered =
                        zuno_engine::context_usage::read_context_usage(transaction, &session)?;
                    Ok((recovered.request.is_some()
                        || recovered.context_epoch > 0
                        || recovered.cumulative_usage.total() > 0)
                        .then_some(recovered))
                })
                .transpose()?
                .flatten();
            Ok((boundary, rows, context_usage))
        })?;
        let events = rows
            .into_iter()
            .map(|row| decode_row(session_id, row))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Snapshot {
            events,
            boundary,
            context_usage,
        })
    }

    /// Use the same pool boundary as replay, including connection checkout.
    ///
    /// Even read-only projections must not open and configure a shared-cache
    /// connection while another pooled transaction holds a read lock. Its pragmas
    /// can return SQLITE_LOCKED before the SELECT starts, terminating a forwarder.
    /// A deferred transaction takes no SQLite write reservation, while the pool's
    /// lock serializes its checkout with other in-process snapshots and writers.
    fn read<T>(
        &self,
        read: impl FnOnce(&zuno_db::Transaction<'_>) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        self.transaction(TransactionBehavior::Deferred, read)
    }

    /// Retry only connection setup that failed before caller work was entered.
    ///
    /// A request-list projection can hold a raw pooled read connection outside
    /// the transaction lock. In shared-cache memory databases a second checkout
    /// then fails in its pragmas with SQLITE_LOCKED, which SQLite's busy handler
    /// does not wait for. Once `work` starts, even an identical error is returned:
    /// caller mutations, commit failures and unknown outcomes are never replayed.
    fn transaction<T>(
        &self,
        behavior: TransactionBehavior,
        work: impl FnOnce(&zuno_db::Transaction<'_>) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut work = Some(work);
        let mut retry = CheckoutRetry::new();
        loop {
            let result = self
                .pool
                .transaction_with_behavior(behavior, |transaction| {
                    work.take().expect("transaction work is entered only once")(transaction)
                });
            if let Err(error) = &result
                && work.is_some()
                && retry.wait(error)
            {
                continue;
            }
            return result;
        }
    }

    fn ensure_initialized(&self) -> Result<(), DbError> {
        if self.initialized.get().is_some() {
            return Ok(());
        }
        let _initializing = self
            .initializing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.initialized.get().is_some() {
            return Ok(());
        }
        let mut retry = CheckoutRetry::new();
        loop {
            match self.pool.initialize() {
                Ok(()) => break,
                Err(error) if retry.wait(&error) => {}
                Err(error) => return Err(error),
            }
        }
        self.initialized.get_or_init(|| ());
        Ok(())
    }
}

struct CheckoutRetry {
    deadline: Instant,
    delay: Duration,
}

impl CheckoutRetry {
    fn new() -> Self {
        Self {
            deadline: Instant::now()
                + Duration::from_millis(
                    u64::try_from(open::BUSY_TIMEOUT_MS)
                        .expect("the database busy timeout is non-negative"),
                ),
            delay: Duration::from_millis(1),
        }
    }

    fn wait(&mut self, error: &DbError) -> bool {
        let DbError::Open { source, .. } = error else {
            return false;
        };
        if !matches!(
            source.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseLocked
        ) {
            return false;
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(self.delay.min(remaining));
        self.delay = (self.delay * 2).min(Duration::from_millis(20));
        true
    }
}

fn latest_sequence(
    transaction: &zuno_db::Transaction<'_>,
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
