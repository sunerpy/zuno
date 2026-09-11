//! Durable canonical context state, including the active attempt's rollback point.
//!
//! Mount [`SCHEMA_SQL`] in the normal atomic schema/migration transaction. This
//! module never creates tables, resets corrupt rows, or reads a model/session
//! transcript implicitly.

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};
use zuno_error::DbError;
use zuno_types::context_usage::{ContextUsageSnapshot, ContextUsageSource, ContextUsageTracker};

use crate::{Pool, open};

/// Couple a changed tracker revision with its durable public snapshot event.
pub fn commit_in(
    transaction: &rusqlite::Transaction<'_>,
    update: &zuno_types::context_usage::ContextUsageWrite,
) -> Result<Option<ContextUsageSnapshot>, DbError> {
    if !write_in(transaction, update.expected_revision, &update.tracker)? {
        return Ok(None);
    }
    let snapshot = update.tracker.snapshot().clone();
    crate::event_log::append_in(
        transaction,
        &snapshot.session_id,
        crate::event_log::NewSessionEvent::new(
            "session.context.usage",
            serde_json::json!({"snapshot": snapshot})
                .as_object()
                .expect("fixed envelope")
                .clone(),
        )?,
    )?;
    Ok(Some(snapshot))
}

pub const SCHEMA_SQL: &str = include_str!("schema/context_usage.sql");
const TABLE: &str = "session_context_usage";

#[derive(Clone)]
pub struct ContextUsageStore {
    pool: Arc<Pool>,
}

impl ContextUsageStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Read the main context for a client surface.
    pub fn get(&self, session_id: &str) -> Result<Option<ContextUsageSnapshot>, DbError> {
        Ok(self
            .load(session_id)?
            .map(|tracker| tracker.snapshot().clone()))
    }

    /// Restore runtime state with its complete retry checkpoint.
    pub fn load(&self, session_id: &str) -> Result<Option<ContextUsageTracker>, DbError> {
        let connection = self.pool.get()?;
        read_in(&connection, session_id)
    }

    pub fn load_source(
        &self,
        session_id: &str,
        source: ContextUsageSource,
    ) -> Result<Option<ContextUsageTracker>, DbError> {
        let connection = self.pool.get()?;
        read_source_in(&connection, session_id, source)
    }

    /// Save one revision atomically; exact retries return `false`.
    pub fn save(
        &self,
        expected_revision: Option<u64>,
        tracker: &ContextUsageTracker,
    ) -> Result<bool, DbError> {
        self.pool
            .transaction(|transaction| write_in(transaction, expected_revision, tracker))
    }
}

pub fn read_in(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<ContextUsageTracker>, DbError> {
    read_source_in(connection, session_id, ContextUsageSource::Main)
}

/// Sources have separate rows even when auxiliary work names the same session.
pub fn read_source_in(
    connection: &Connection,
    session_id: &str,
    source: ContextUsageSource,
) -> Result<Option<ContextUsageTracker>, DbError> {
    let row = connection
        .query_row(
            "SELECT revision, context_epoch, state_json, time_updated \
             FROM session_context_usage WHERE session_id = ?1 AND source = ?2",
            params![session_id, source.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(open::map_error)?;
    let Some((revision, epoch, state_json, time_updated)) = row else {
        return Ok(None);
    };
    decode_stored(
        session_id,
        source,
        revision,
        epoch,
        &state_json,
        time_updated,
    )
    .map(Some)
}

/// Batch the persisted part of a session-list projection without hydrating history
/// or issuing one query for every session.
pub fn read_many_in(
    connection: &Connection,
    session_ids: &[&str],
    source: ContextUsageSource,
) -> Result<BTreeMap<String, ContextUsageTracker>, DbError> {
    let mut result = BTreeMap::new();
    for chunk in session_ids.chunks(128) {
        let placeholders = (2..=chunk.len() + 1)
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT session_id, revision, context_epoch, state_json, time_updated \
             FROM session_context_usage WHERE source = ?1 AND session_id IN ({placeholders})"
        );
        let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
        let parameters = std::iter::once(source.as_str()).chain(chunk.iter().copied());
        let rows = statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(open::map_error)?;
        for row in rows {
            let (session_id, revision, epoch, state_json, time_updated) =
                row.map_err(open::map_error)?;
            let tracker = decode_stored(
                &session_id,
                source,
                revision,
                epoch,
                &state_json,
                time_updated,
            )?;
            result.insert(session_id, tracker);
        }
    }
    Ok(result)
}

fn decode_stored(
    session_id: &str,
    source: ContextUsageSource,
    revision: i64,
    epoch: i64,
    state_json: &str,
    time_updated: i64,
) -> Result<ContextUsageTracker, DbError> {
    let revision = u64::try_from(revision).map_err(invalid_state)?;
    let epoch = u64::try_from(epoch).map_err(invalid_state)?;
    let tracker: ContextUsageTracker =
        serde_json::from_str(state_json).map_err(|source| DbError::Decode {
            table: TABLE.to_owned(),
            source,
        })?;
    tracker.validate().map_err(invalid_state)?;
    let snapshot = tracker.snapshot();
    if snapshot.session_id != session_id
        || snapshot.source != source
        || snapshot.revision != revision
        || snapshot.context_epoch != epoch
        || snapshot.time_updated != time_updated
    {
        return Err(invalid_state(
            "context state disagrees with its row identity",
        ));
    }
    Ok(tracker)
}

/// Save in the caller's transaction/connection with optimistic revision checking.
///
/// `None` expects an absent row. `Some(revision)` expects that exact revision.
/// Duplicate identical writes are harmless even after a lost success response.
/// Conflicting same-revision writes and epoch regressions fail without mutation.
pub fn validate_update(
    current: Option<&ContextUsageTracker>,
    expected_revision: Option<u64>,
    tracker: &ContextUsageTracker,
) -> Result<bool, DbError> {
    tracker.validate().map_err(invalid_state)?;
    let snapshot = tracker.snapshot();
    if current == Some(tracker) {
        return Ok(false);
    }
    match current {
        None if expected_revision.is_some() => {
            Err(conflict(snapshot, "expected context row is absent"))
        }
        Some(current) => {
            let previous = current.snapshot();
            if previous.session_id != snapshot.session_id
                || previous.source != snapshot.source
                || expected_revision != Some(previous.revision)
                || snapshot.revision <= previous.revision
                || snapshot.context_epoch < previous.context_epoch
                || snapshot.time_updated < previous.time_updated
            {
                Err(conflict(
                    snapshot,
                    "context revision, epoch, time, or identity changed",
                ))
            } else {
                Ok(true)
            }
        }
        None => Ok(true),
    }
}

pub fn write_in(
    connection: &Connection,
    expected_revision: Option<u64>,
    tracker: &ContextUsageTracker,
) -> Result<bool, DbError> {
    tracker.validate().map_err(invalid_state)?;
    let snapshot = tracker.snapshot();
    let revision = sqlite_integer(snapshot.revision)?;
    let epoch = sqlite_integer(snapshot.context_epoch)?;
    let state_json = serde_json::to_string(tracker).map_err(invalid_state)?;
    let current = read_source_in(connection, &snapshot.session_id, snapshot.source)?;
    if !validate_update(current.as_ref(), expected_revision, tracker)? {
        return Ok(false);
    }
    let changed = match current {
        None => {
            if expected_revision.is_some() {
                return Err(conflict(snapshot, "expected context row is absent"));
            }
            connection
                .execute(
                    "INSERT INTO session_context_usage \
                       (session_id, source, revision, context_epoch, state_json, time_updated) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT(session_id, source) DO NOTHING",
                    params![
                        snapshot.session_id,
                        snapshot.source.as_str(),
                        revision,
                        epoch,
                        state_json,
                        snapshot.time_updated,
                    ],
                )
                .map_err(open::map_error)?
        }
        Some(current) => {
            let previous = current.snapshot();
            if expected_revision != Some(previous.revision)
                || snapshot.revision <= previous.revision
                || snapshot.context_epoch < previous.context_epoch
                || snapshot.time_updated < previous.time_updated
            {
                return Err(conflict(
                    snapshot,
                    "context revision, epoch, or time regressed",
                ));
            }
            connection
                .execute(
                    "UPDATE session_context_usage SET \
                       revision = ?1, context_epoch = ?2, state_json = ?3, time_updated = ?4 \
                     WHERE session_id = ?5 AND source = ?6 AND revision = ?7 \
                       AND context_epoch <= ?2 AND time_updated <= ?4",
                    params![
                        revision,
                        epoch,
                        state_json,
                        snapshot.time_updated,
                        snapshot.session_id,
                        snapshot.source.as_str(),
                        sqlite_integer(previous.revision)?,
                    ],
                )
                .map_err(open::map_error)?
        }
    };
    if changed == 1 {
        return Ok(true);
    }
    if read_source_in(connection, &snapshot.session_id, snapshot.source)?.as_ref() == Some(tracker)
    {
        return Ok(false);
    }
    Err(conflict(snapshot, "context state changed concurrently"))
}

/// Invalidate a foreground window during an explicit transcript rewrite.
///
/// Call this in the rewrite transaction, before deleting message rows. It keeps
/// observed consumption, including usage for messages the rewrite will remove.
/// The existing history checkpoint may remain/reset to zero; the canonical usage
/// generation still advances. This does not alter compaction strategy.
pub fn invalidate_window_in(
    transaction: &crate::Transaction<'_>,
    session_id: &str,
    at_ms: i64,
) -> Result<ContextUsageSnapshot, DbError> {
    let session = crate::session::get(transaction, session_id)?;
    let source = if session.parent_id.is_some() {
        ContextUsageSource::Child
    } else {
        ContextUsageSource::Main
    };
    let stored = read_source_in(transaction, session_id, source)?;
    let expected = stored.as_ref().map(|tracker| tracker.snapshot().revision);
    let mut tracker = stored.unwrap_or_else(|| {
        let mut tracker = ContextUsageTracker::for_source(session_id, source);
        let usage = session.usage.snapshot();
        tracker.seed_cumulative(
            zuno_types::context_usage::ContextUsageTotals {
                input: usage.confirmed.input,
                output: usage.confirmed.output,
                reasoning: usage.confirmed.reasoning,
                cache_read: usage.confirmed.cache_read,
                cache_write: usage.confirmed.cache_write,
                unclassified: usage.confirmed.unclassified,
            },
            usage.confirmed_known && usage.confirmed.is_empty(),
            at_ms,
        );
        tracker
    });
    let next_epoch = tracker.snapshot().context_epoch.saturating_add(1);
    tracker.observe_history_epoch(0, at_ms);
    tracker.reset_epoch(next_epoch, at_ms);
    write_in(transaction, expected, &tracker)?;
    let snapshot = tracker.snapshot().clone();
    crate::event_log::append_in(
        transaction,
        session_id,
        crate::event_log::NewSessionEvent::new(
            "session.context.usage",
            serde_json::json!({"snapshot": snapshot})
                .as_object()
                .expect("fixed context envelope")
                .clone(),
        )?,
    )?;
    Ok(snapshot)
}

fn sqlite_integer(value: u64) -> Result<i64, DbError> {
    i64::try_from(value).map_err(invalid_state)
}

fn invalid_state(error: impl std::fmt::Display) -> DbError {
    DbError::Query {
        source: Box::new(std::io::Error::other(error.to_string())),
    }
}

fn conflict(snapshot: &ContextUsageSnapshot, detail: &str) -> DbError {
    DbError::Conflict {
        table: TABLE.to_owned(),
        id: format!("{}:{}", snapshot.session_id, snapshot.source.as_str()),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_types::context_usage::{
        ContextRequestIdentity, ContextTokenAccounting, ContextUsageCounters,
    };

    fn database() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        // Small synthetic fixture: migration ownership remains with the schema
        // integration. These tests exercise this table's persistence contract.
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON; \
                 CREATE TABLE session (id TEXT PRIMARY KEY); \
                 INSERT INTO session (id) VALUES ('ses_synthetic'), ('ses_other');",
            )
            .unwrap();
        connection.execute_batch(SCHEMA_SQL).unwrap();
        connection
    }

    fn request(sequence: u64) -> ContextRequestIdentity {
        ContextRequestIdentity {
            request_id: format!("synthetic-request-{sequence}"),
            request_sequence: sequence,
            attempt: 1,
            context_epoch: 0,
            provider_id: "synthetic-provider".to_owned(),
            model_id: "synthetic-model".to_owned(),
            source: ContextUsageSource::Main,
            turn_id: Some("synthetic-turn".to_owned()),
            time_started: i64::try_from(sequence).unwrap(),
            request_context_tokens: None,
            history_prefix: None,
        }
    }

    fn tracker() -> ContextUsageTracker {
        let mut tracker = ContextUsageTracker::new("ses_synthetic");
        tracker.start_request(request(1), Some(70_000), Some(0), Some(200_000), 1);
        tracker.observe_usage(
            &request(1),
            ContextUsageCounters {
                input_tokens: Some(125_350),
                output_tokens: Some(40),
                accounting: ContextTokenAccounting::CacheInsideInput,
                ..ContextUsageCounters::default()
            },
            2,
        );
        tracker.commit_request(&request(1), 3);
        tracker
    }

    #[test]
    fn persisted_tracker_resumes_and_rolls_back_partial_usage() {
        let connection = database();
        let mut tracker = tracker();
        assert!(write_in(&connection, None, &tracker).unwrap());
        let saved_revision = tracker.snapshot().revision;
        tracker.start_request(request(2), Some(73_948), Some(2_000), Some(200_000), 4);
        tracker.observe_usage(
            &request(2),
            ContextUsageCounters {
                input_tokens: Some(149_501),
                accounting: ContextTokenAccounting::CacheInsideInput,
                ..ContextUsageCounters::default()
            },
            5,
        );
        write_in(&connection, Some(saved_revision), &tracker).unwrap();
        let mut resumed = read_in(&connection, "ses_synthetic").unwrap().unwrap();
        assert_eq!(resumed, tracker);
        let saved_revision = resumed.snapshot().revision;
        assert!(resumed.rollback_request(&request(2), 2, 6));
        assert_eq!(resumed.snapshot().used_tokens, Some(127_390));
        assert_eq!(resumed.snapshot().cumulative_usage.total(), 274_891);
        assert!(!resumed.snapshot().cumulative_known);
        write_in(&connection, Some(saved_revision), &resumed).unwrap();
        assert_eq!(
            read_in(&connection, "ses_synthetic").unwrap(),
            Some(resumed)
        );
    }

    #[test]
    fn duplicate_writes_are_idempotent_and_stale_writers_cannot_replace_context() {
        let connection = database();
        let original = tracker();
        assert!(write_in(&connection, None, &original).unwrap());
        assert!(!write_in(&connection, None, &original).unwrap());
        let mut next = original.clone();
        next.start_request(request(2), Some(73_948), Some(3_000), Some(200_000), 4);
        assert!(write_in(&connection, Some(original.snapshot().revision), &next).unwrap());
        assert!(!write_in(&connection, Some(original.snapshot().revision), &next).unwrap());
        assert!(matches!(
            write_in(&connection, Some(original.snapshot().revision), &original),
            Err(DbError::Conflict { .. })
        ));
        let mut conflicting = original.clone();
        conflicting.set_estimated_tail(Some(99), 4);
        assert!(matches!(
            write_in(&connection, Some(next.snapshot().revision), &conflicting),
            Err(DbError::Conflict { .. })
        ));
        assert_eq!(read_in(&connection, "ses_synthetic").unwrap(), Some(next));
    }

    #[test]
    fn compaction_is_persisted_and_cannot_be_undone_by_an_old_epoch() {
        let connection = database();
        let original = tracker();
        write_in(&connection, None, &original).unwrap();
        let mut compacted = original.clone();
        compacted.reset_epoch(10, 4);
        write_in(&connection, Some(original.snapshot().revision), &compacted).unwrap();
        let mut stale = original;
        for tail in 1..8 {
            stale.set_estimated_tail(Some(tail), 20);
        }
        assert!(stale.snapshot().revision > compacted.snapshot().revision);
        assert!(matches!(
            write_in(&connection, Some(compacted.snapshot().revision), &stale),
            Err(DbError::Conflict { .. })
        ));
        let restored = read_in(&connection, "ses_synthetic").unwrap().unwrap();
        assert_eq!(restored.snapshot().used_tokens, None);
        assert_eq!(restored.snapshot().cumulative_usage.total(), 125_390);
    }

    #[test]
    fn rows_are_isolated_by_session_and_source_and_delete_with_the_session() {
        let connection = database();
        let main = tracker();
        write_in(&connection, None, &main).unwrap();
        let mut learning =
            ContextUsageTracker::for_source("ses_synthetic", ContextUsageSource::Learning);
        let mut learning_request = request(1);
        learning_request.source = ContextUsageSource::Learning;
        learning.start_request(learning_request, Some(15), Some(0), Some(100), 1);
        write_in(&connection, None, &learning).unwrap();
        assert_eq!(read_in(&connection, "ses_synthetic").unwrap(), Some(main));
        assert_eq!(
            read_source_in(&connection, "ses_synthetic", ContextUsageSource::Learning).unwrap(),
            Some(learning)
        );
        assert_eq!(read_in(&connection, "ses_other").unwrap(), None);
        connection
            .execute("DELETE FROM session WHERE id = 'ses_synthetic'", [])
            .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM session_context_usage", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
    }

    #[test]
    fn corrupt_identity_and_inconsistent_snapshot_fail_closed_without_repair() {
        let connection = database();
        let tracker = tracker();
        write_in(&connection, None, &tracker).unwrap();
        connection
            .execute(
                "UPDATE session_context_usage SET state_json = \
                 json_set(state_json, '$.snapshot.usedTokens', 73948)",
                [],
            )
            .unwrap();
        let corrupt: String = connection
            .query_row("SELECT state_json FROM session_context_usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(read_in(&connection, "ses_synthetic").is_err());
        assert!(write_in(&connection, Some(tracker.snapshot().revision), &tracker).is_err());
        let after: String = connection
            .query_row("SELECT state_json FROM session_context_usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(after, corrupt);
    }

    #[test]
    fn failed_outer_transaction_leaves_no_context_projection() {
        let mut connection = database();
        {
            let transaction = connection.transaction().unwrap();
            write_in(&transaction, None, &tracker()).unwrap();
            // Simulate another write in the atomic checkpoint failing.
            assert!(
                transaction
                    .execute("INSERT INTO missing_table VALUES (1)", [])
                    .is_err()
            );
        }
        assert_eq!(read_in(&connection, "ses_synthetic").unwrap(), None);
    }

    #[test]
    fn store_reopens_a_temporary_database_with_request_identity_and_checkpoint_intact() {
        let temporary = tempfile::tempdir().unwrap();
        let location = zuno_paths::DbLocation::File(temporary.path().join("context.sqlite"));
        let pool = Arc::new(Pool::open(&location).unwrap());
        {
            let connection = pool.get().unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE session (id TEXT PRIMARY KEY); \
                     INSERT INTO session (id) VALUES ('ses_synthetic');",
                )
                .unwrap();
            connection.execute_batch(SCHEMA_SQL).unwrap();
        }
        let store = ContextUsageStore::new(Arc::clone(&pool));
        let mut pending = tracker();
        pending.start_request(request(2), Some(73_948), Some(2_000), Some(200_000), 4);
        pending.observe_usage(
            &request(2),
            ContextUsageCounters {
                input_tokens: Some(149_501),
                output_tokens: Some(9),
                accounting: ContextTokenAccounting::CacheInsideInput,
                ..ContextUsageCounters::default()
            },
            5,
        );
        store.save(None, &pending).unwrap();
        drop(store);
        drop(pool);

        let reopened = ContextUsageStore::new(Arc::new(Pool::open(&location).unwrap()));
        let mut restored = reopened.load("ses_synthetic").unwrap().unwrap();
        assert_eq!(restored, pending);
        assert!(!reopened.save(None, &pending).unwrap());
        let revision = restored.snapshot().revision;
        assert!(restored.rollback_request(&request(2), 2, 6));
        reopened.save(Some(revision), &restored).unwrap();
        assert_eq!(
            reopened.get("ses_synthetic").unwrap().unwrap().used_tokens,
            Some(127_390)
        );
    }

    #[test]
    fn schema_mount_has_the_declared_index_and_enforces_session_ownership() {
        let connection = database();
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'session_context_usage_updated_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);
        let tracker = ContextUsageTracker::new("ses_missing");
        assert!(write_in(&connection, None, &tracker).is_err());
        assert_eq!(read_in(&connection, "ses_missing").unwrap(), None);
    }

    #[test]
    fn batched_reads_preserve_source_identity_and_skip_missing_rows() {
        let connection = database();
        let expected = tracker();
        write_in(&connection, None, &expected).unwrap();
        let mut ids = vec!["ses_synthetic"; 257];
        ids.extend(["ses_other", "ses_missing"]);
        let rows = read_many_in(&connection, &ids, ContextUsageSource::Main).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows["ses_synthetic"], expected);
        assert!(
            read_many_in(&connection, &ids, ContextUsageSource::Learning)
                .unwrap()
                .is_empty()
        );
        assert!(
            read_many_in(&connection, &[], ContextUsageSource::Main)
                .unwrap()
                .is_empty()
        );
    }
}
