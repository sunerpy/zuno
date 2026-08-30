//! Durable provider retry deadlines used to recover Goal continuation safely.

use crate::{Pool, open};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zuno_error::DbError;

const TABLE: &str = "provider_retry_backoff";
const COLUMNS: &str = "session_id, request_id, turn_id, failed_attempt, next_attempt, \
    max_attempts, reason, delay_ms, retry_at_ms, scheduled_at_ms";

/// One retry delay committed before the process starts sleeping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderBackoffCheckpoint {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "requestID")]
    pub request_id: String,
    #[serde(rename = "turnID")]
    pub turn_id: String,
    pub failed_attempt: u32,
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub reason: String,
    pub delay_ms: i64,
    pub retry_at_ms: i64,
    pub scheduled_at_ms: i64,
}

/// Read access used by Goal continuation.
#[derive(Debug, Clone)]
pub struct ProviderBackoffStore {
    pool: Arc<Pool>,
}

impl ProviderBackoffStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn get(&self, session_id: &str) -> Result<Option<ProviderBackoffCheckpoint>, DbError> {
        let connection = self.pool.get()?;
        get(&connection, session_id)
    }
}

/// Upsert a checkpoint before sleeping.
pub fn schedule(
    connection: &Connection,
    checkpoint: &ProviderBackoffCheckpoint,
) -> Result<(), DbError> {
    connection
        .execute(
            "INSERT INTO provider_retry_backoff \
             (session_id, request_id, turn_id, failed_attempt, next_attempt, max_attempts, \
              reason, delay_ms, retry_at_ms, scheduled_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(session_id) DO UPDATE SET \
               request_id=excluded.request_id, turn_id=excluded.turn_id, \
               failed_attempt=excluded.failed_attempt, next_attempt=excluded.next_attempt, \
               max_attempts=excluded.max_attempts, reason=excluded.reason, \
               delay_ms=excluded.delay_ms, retry_at_ms=excluded.retry_at_ms, \
               scheduled_at_ms=excluded.scheduled_at_ms",
            params![
                checkpoint.session_id,
                checkpoint.request_id,
                checkpoint.turn_id,
                checkpoint.failed_attempt,
                checkpoint.next_attempt,
                checkpoint.max_attempts,
                checkpoint.reason,
                checkpoint.delay_ms,
                checkpoint.retry_at_ms,
                checkpoint.scheduled_at_ms,
            ],
        )
        .map_err(open::map_error)?;
    Ok(())
}

/// Remove a checkpoint only when it still belongs to this provider request.
pub fn clear_request(
    connection: &Connection,
    session_id: &str,
    request_id: &str,
) -> Result<(), DbError> {
    connection
        .execute(
            "DELETE FROM provider_retry_backoff WHERE session_id = ?1 AND request_id = ?2",
            params![session_id, request_id],
        )
        .map_err(open::map_error)?;
    Ok(())
}

/// Remove any fulfilled or stale checkpoint before a new provider attempt starts.
pub fn clear_session(connection: &Connection, session_id: &str) -> Result<(), DbError> {
    connection
        .execute(
            "DELETE FROM provider_retry_backoff WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(open::map_error)?;
    Ok(())
}

/// Read the latest checkpoint for a session.
pub fn get(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<ProviderBackoffCheckpoint>, DbError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM {TABLE} WHERE session_id = ?1"),
            params![session_id],
            from_row,
        )
        .optional()
        .map_err(open::map_error)
}

fn from_row(row: &Row<'_>) -> Result<ProviderBackoffCheckpoint, rusqlite::Error> {
    Ok(ProviderBackoffCheckpoint {
        session_id: row.get("session_id")?,
        request_id: row.get("request_id")?,
        turn_id: row.get("turn_id")?,
        failed_attempt: row.get("failed_attempt")?,
        next_attempt: row.get("next_attempt")?,
        max_attempts: row.get("max_attempts")?,
        reason: row.get("reason")?,
        delay_ms: row.get("delay_ms")?,
        retry_at_ms: row.get("retry_at_ms")?,
        scheduled_at_ms: row.get("scheduled_at_ms")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration;
    use zuno_paths::DbLocation;

    fn checkpoint(request_id: &str) -> ProviderBackoffCheckpoint {
        ProviderBackoffCheckpoint {
            session_id: "ses_retry".to_owned(),
            request_id: request_id.to_owned(),
            turn_id: "turn_retry".to_owned(),
            failed_attempt: 1,
            next_attempt: 2,
            max_attempts: 3,
            reason: "rate_limited".to_owned(),
            delay_ms: 30_000,
            retry_at_ms: 31_000,
            scheduled_at_ms: 1_000,
        }
    }

    #[test]
    fn checkpoint_survives_store_reopen_and_only_its_request_can_clear_it() {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open pool"));
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("schema");
        schedule(&connection, &checkpoint("req_exact")).expect("schedule");
        drop(connection);

        let reopened = ProviderBackoffStore::new(Arc::clone(&pool));
        assert_eq!(
            reopened
                .get("ses_retry")
                .expect("read checkpoint")
                .expect("checkpoint")
                .request_id,
            "req_exact"
        );
        let connection = pool.get().expect("connection");
        clear_request(&connection, "ses_retry", "req_other").expect("ignore other request");
        assert!(get(&connection, "ses_retry").expect("read").is_some());
        clear_request(&connection, "ses_retry", "req_exact").expect("clear exact request");
        assert_eq!(get(&connection, "ses_retry").expect("read"), None);
    }
}
