//! Revisioned feedback sidecars for durable assistant messages.

use crate::event_log::{NewSessionEvent, append_in, query_error};
use crate::{Pool, open};
use rusqlite::{OptionalExtension as _, Row, params};
use serde_json::{Map, Value};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::{FeedbackRating, MessageFeedbackProjection};

const COLUMNS: &str = "message_id, session_id, rating, note, revision, time_created, time_updated";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackUpdate {
    pub message_id: String,
    pub rating: FeedbackRating,
    pub note: Option<String>,
    /// Zero creates the first revision; later writes must name the current revision.
    pub expected_revision: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackWrite {
    Applied(MessageFeedbackProjection),
    Stale(MessageFeedbackProjection),
}

struct FeedbackRow {
    message_id: String,
    session_id: String,
    rating: i64,
    note: Option<String>,
    revision: i64,
    time_created: i64,
    time_updated: i64,
}

#[derive(Clone)]
pub struct FeedbackStore {
    pool: Arc<Pool>,
}

impl FeedbackStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Set feedback and append its audit event in the same transaction.
    pub fn set(&self, update: FeedbackUpdate) -> Result<FeedbackWrite, DbError> {
        if update.expected_revision < 0 {
            return Err(query_error(std::io::Error::other(
                "feedback expected_revision must not be negative",
            )));
        }
        self.pool.transaction(|transaction| {
            let session_id = transaction
                .query_row(
                    "SELECT session_id FROM message
                     WHERE id = ?1 AND json_extract(data, '$.role') = 'assistant'",
                    [&update.message_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(open::map_error)?
                .ok_or_else(|| DbError::NotFound {
                    table: "assistant message".to_owned(),
                    id: update.message_id.clone(),
                })?;
            let current = read_optional(transaction, &update.message_id)?;
            let current_revision = current.as_ref().map_or(0, |row| row.revision);
            if current_revision != update.expected_revision {
                return Ok(FeedbackWrite::Stale(current.expect(
                    "a positive current revision is backed by a feedback row",
                )));
            }

            let revision = current_revision
                .checked_add(1)
                .ok_or_else(|| query_error(std::io::Error::other("feedback revision exhausted")))?;
            transaction
                .execute(
                    "INSERT INTO message_feedback (
                       message_id, session_id, rating, note, revision, time_created, time_updated
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                     ON CONFLICT(message_id) DO UPDATE SET
                       rating = excluded.rating,
                       note = excluded.note,
                       revision = excluded.revision,
                       time_updated = excluded.time_updated",
                    params![
                        update.message_id,
                        session_id,
                        update.rating.as_i64(),
                        update.note,
                        revision,
                        update.time_updated,
                    ],
                )
                .map_err(open::map_error)?;
            let stored = read_required(transaction, &update.message_id)?;
            let mut properties = Map::new();
            properties.insert(
                "messageID".to_owned(),
                Value::String(stored.message_id.clone()),
            );
            properties.insert(
                "rating".to_owned(),
                Value::String(
                    match stored.rating {
                        FeedbackRating::Positive => "positive",
                        FeedbackRating::Negative => "negative",
                    }
                    .to_owned(),
                ),
            );
            properties.insert(
                "note".to_owned(),
                stored.note.clone().map_or(Value::Null, Value::String),
            );
            properties.insert("revision".to_owned(), Value::from(stored.revision));
            properties.insert("previousRevision".to_owned(), Value::from(current_revision));
            append_in(
                transaction,
                &session_id,
                NewSessionEvent::new("learning.feedback.changed", properties)?,
            )?;
            Ok(FeedbackWrite::Applied(stored))
        })
    }

    pub fn get(&self, message_id: &str) -> Result<Option<MessageFeedbackProjection>, DbError> {
        let connection = self.pool.get()?;
        read_optional(&connection, message_id)
    }

    pub fn list_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<MessageFeedbackProjection>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM message_feedback
                 WHERE session_id = ?1 ORDER BY time_updated DESC, message_id DESC"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map([session_id], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode))
            .collect()
    }
}

fn read_required(
    connection: &rusqlite::Connection,
    message_id: &str,
) -> Result<MessageFeedbackProjection, DbError> {
    read_optional(connection, message_id)?.ok_or_else(|| DbError::NotFound {
        table: "message_feedback".to_owned(),
        id: message_id.to_owned(),
    })
}

fn read_optional(
    connection: &rusqlite::Connection,
    message_id: &str,
) -> Result<Option<MessageFeedbackProjection>, DbError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM message_feedback WHERE message_id = ?1"),
            [message_id],
            decode_row,
        )
        .optional()
        .map_err(open::map_error)?
        .map(decode)
        .transpose()
}

fn decode_row(row: &Row<'_>) -> rusqlite::Result<FeedbackRow> {
    Ok(FeedbackRow {
        message_id: row.get(0)?,
        session_id: row.get(1)?,
        rating: row.get(2)?,
        note: row.get(3)?,
        revision: row.get(4)?,
        time_created: row.get(5)?,
        time_updated: row.get(6)?,
    })
}

fn decode(row: FeedbackRow) -> Result<MessageFeedbackProjection, DbError> {
    let rating = FeedbackRating::parse(row.rating).ok_or_else(|| {
        query_error(std::io::Error::other(format!(
            "unknown feedback rating `{}`",
            row.rating
        )))
    })?;
    Ok(MessageFeedbackProjection {
        message_id: row.message_id,
        session_id: row.session_id,
        rating,
        note: row.note,
        revision: row.revision,
        time_created: row.time_created,
        time_updated: row.time_updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration;
    use zuno_paths::DbLocation;

    fn store() -> FeedbackStore {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open pool"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection
                .execute_batch(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('project-1', '/workspace', 1, 1, '[]');
                     INSERT INTO session
                       (id, project_id, slug, directory, title, version, time_created, time_updated)
                     VALUES ('session-1', 'project-1', 'slug', '/workspace', 'title', '1', 1, 1);
                     INSERT INTO message (id, session_id, time_created, time_updated, data)
                     VALUES ('assistant-1', 'session-1', 1, 1, '{\"role\":\"assistant\"}');",
                )
                .expect("fixture");
        }
        FeedbackStore::new(pool)
    }

    #[test]
    fn stale_revision_is_rejected_and_every_change_is_audited() {
        let store = store();
        let first = store
            .set(FeedbackUpdate {
                message_id: "assistant-1".to_owned(),
                rating: FeedbackRating::Positive,
                note: Some("useful".to_owned()),
                expected_revision: 0,
                time_updated: 10,
            })
            .expect("first feedback");
        assert!(matches!(first, FeedbackWrite::Applied(ref row) if row.revision == 1));

        let stale = store
            .set(FeedbackUpdate {
                message_id: "assistant-1".to_owned(),
                rating: FeedbackRating::Negative,
                note: None,
                expected_revision: 0,
                time_updated: 11,
            })
            .expect("stale response");
        assert!(matches!(stale, FeedbackWrite::Stale(ref row) if row.revision == 1));

        let second = store
            .set(FeedbackUpdate {
                message_id: "assistant-1".to_owned(),
                rating: FeedbackRating::Negative,
                note: Some("regressed".to_owned()),
                expected_revision: 1,
                time_updated: 12,
            })
            .expect("second feedback");
        assert!(matches!(second, FeedbackWrite::Applied(ref row) if row.revision == 2));

        let events = crate::event_log::SessionEventLog::new(store.pool.clone())
            .read_after("session-1", None)
            .expect("events");
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| event.event_type == "learning.feedback.changed")
        );
    }
}
