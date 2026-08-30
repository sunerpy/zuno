//! Durable proposals for resident-memory changes.

use crate::{Pool, open};
use rusqlite::{OptionalExtension as _, Row, params};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::{
    MemoryAction, MemoryCandidateProjection, MemoryCandidateStatus, MemoryScope, MemorySource,
};

const TABLE: &str = "memory_candidate";
const COLUMNS: &str = "id, target, target_path, action, content, old_text, reason, confidence, \
    source_kind, source_session_id, source_message_id, fingerprint, status, before_entries, \
    after_entries, error, time_created, time_updated, time_applied";

/// A validated candidate waiting to be inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMemoryCandidate {
    pub id: String,
    pub target: MemoryScope,
    pub target_path: String,
    pub action: MemoryAction,
    pub content: Option<String>,
    pub old_text: Option<String>,
    pub reason: String,
    pub confidence: u16,
    pub source: MemorySource,
    pub source_session_id: Option<String>,
    pub source_message_id: Option<String>,
    pub fingerprint: Option<String>,
    pub time_created: i64,
}

/// Stored candidate, including snapshots used for apply/undo reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCandidateRecord {
    pub projection: MemoryCandidateProjection,
    pub target_path: String,
    pub fingerprint: Option<String>,
    pub before_entries: Option<Vec<String>>,
    pub after_entries: Option<Vec<String>>,
    pub time_applied: Option<i64>,
}

impl MemoryCandidateRecord {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.projection.id
    }
}

/// Whether an idempotent candidate call inserted a new row or reused its source twin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCandidateInsert {
    pub record: MemoryCandidateRecord,
    pub inserted: bool,
}

/// Candidate access over the initialized session database.
#[derive(Clone)]
pub struct MemoryCandidateStore {
    pool: Arc<Pool>,
}

impl MemoryCandidateStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn create(&self, candidate: NewMemoryCandidate) -> Result<MemoryCandidateRecord, DbError> {
        self.create_or_get(candidate).map(|insert| insert.record)
    }

    pub fn create_or_get(
        &self,
        candidate: NewMemoryCandidate,
    ) -> Result<MemoryCandidateInsert, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "INSERT INTO memory_candidate (
                        id, target, target_path, action, content, old_text, reason, confidence,
                        source_kind, source_session_id, source_message_id, fingerprint, status,
                        time_created, time_updated
                     ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                        'pending', ?13, ?13
                     )
                     ON CONFLICT (source_session_id, source_message_id, fingerprint)
                     WHERE source_kind = 'reflection' AND fingerprint IS NOT NULL
                     DO NOTHING",
                    params![
                        candidate.id,
                        candidate.target.as_str(),
                        candidate.target_path,
                        candidate.action.as_str(),
                        candidate.content,
                        candidate.old_text,
                        candidate.reason,
                        i64::from(candidate.confidence),
                        candidate.source.as_str(),
                        candidate.source_session_id,
                        candidate.source_message_id,
                        candidate.fingerprint,
                        candidate.time_created,
                    ],
                )
                .map_err(open::map_error)?;
            if changed == 1 {
                return Ok(MemoryCandidateInsert {
                    record: read_required(transaction, &candidate.id)?,
                    inserted: true,
                });
            }
            let fingerprint = candidate.fingerprint.as_deref().ok_or_else(|| {
                query_error(std::io::Error::other(
                    "memory candidate insert changed no rows without an idempotency fingerprint",
                ))
            })?;
            Ok(MemoryCandidateInsert {
                record: read_by_fingerprint(
                    transaction,
                    candidate.source_session_id.as_deref(),
                    candidate.source_message_id.as_deref(),
                    fingerprint,
                )?,
                inserted: false,
            })
        })
    }

    pub fn get(&self, id: &str) -> Result<MemoryCandidateRecord, DbError> {
        let connection = self.pool.get()?;
        read_required(&connection, id)
    }

    /// Candidates relevant to this process's two resident stores, newest first.
    pub fn list_for_paths(
        &self,
        global_path: &str,
        project_path: &str,
    ) -> Result<Vec<MemoryCandidateRecord>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM {TABLE}
                 WHERE target_path IN (?1, ?2)
                 ORDER BY CASE status
                    WHEN 'pending' THEN 0
                    WHEN 'applying' THEN 1
                    WHEN 'undoing' THEN 2
                    WHEN 'uncertain' THEN 3
                    ELSE 4
                 END, time_created DESC, id DESC"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map(params![global_path, project_path], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode_record))
            .collect()
    }

    pub fn list_inflight_for_paths(
        &self,
        global_path: &str,
        project_path: &str,
    ) -> Result<Vec<MemoryCandidateRecord>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM {TABLE}
                 WHERE target_path IN (?1, ?2) AND status IN ('applying','undoing')
                 ORDER BY time_created, id"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map(params![global_path, project_path], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode_record))
            .collect()
    }

    pub fn edit_pending(
        &self,
        id: &str,
        content: Option<&str>,
        old_text: Option<&str>,
        reason: &str,
        confidence: u16,
        time_updated: i64,
    ) -> Result<MemoryCandidateRecord, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE memory_candidate
                     SET content = ?2, old_text = ?3, reason = ?4, confidence = ?5,
                         fingerprint = NULL, error = NULL, time_updated = ?6
                     WHERE id = ?1 AND status IN ('pending','failed')",
                    params![
                        id,
                        content,
                        old_text,
                        reason,
                        i64::from(confidence),
                        time_updated
                    ],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id)?;
            read_required(transaction, id)
        })
    }

    pub fn begin_apply(
        &self,
        id: &str,
        before: &[String],
        after: &[String],
        time_updated: i64,
    ) -> Result<MemoryCandidateRecord, DbError> {
        let before = serde_json::to_string(before).map_err(query_error)?;
        let after = serde_json::to_string(after).map_err(query_error)?;
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE memory_candidate
                     SET status = 'applying', before_entries = ?2, after_entries = ?3,
                         error = NULL, time_updated = ?4
                     WHERE id = ?1 AND status IN ('pending','failed')",
                    params![id, before, after, time_updated],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id)?;
            read_required(transaction, id)
        })
    }

    pub fn begin_undo(
        &self,
        id: &str,
        time_updated: i64,
    ) -> Result<MemoryCandidateRecord, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE memory_candidate
                     SET status = 'undoing', error = NULL, time_updated = ?2
                     WHERE id = ?1 AND status = 'applied'",
                    params![id, time_updated],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id)?;
            read_required(transaction, id)
        })
    }

    pub fn set_status(
        &self,
        id: &str,
        status: MemoryCandidateStatus,
        error: Option<&str>,
        time_updated: i64,
    ) -> Result<MemoryCandidateRecord, DbError> {
        self.pool.transaction(|transaction| {
            let applied = matches!(status, MemoryCandidateStatus::Applied).then_some(time_updated);
            let changed = transaction
                .execute(
                    "UPDATE memory_candidate
                     SET status = ?2, error = ?3, time_updated = ?4,
                         time_applied = COALESCE(?5, time_applied)
                     WHERE id = ?1",
                    params![id, status.as_str(), error, time_updated, applied],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id)?;
            read_required(transaction, id)
        })
    }
}

fn read_required(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<MemoryCandidateRecord, DbError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM {TABLE} WHERE id = ?1"),
            [id],
            decode_row,
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: TABLE.to_owned(),
            id: id.to_owned(),
        })
        .and_then(decode_record)
}

fn read_by_fingerprint(
    connection: &rusqlite::Connection,
    source_session_id: Option<&str>,
    source_message_id: Option<&str>,
    fingerprint: &str,
) -> Result<MemoryCandidateRecord, DbError> {
    let (Some(source_session_id), Some(source_message_id)) = (source_session_id, source_message_id)
    else {
        return Err(query_error(std::io::Error::other(
            "reflection candidate fingerprint requires source session and message ids",
        )));
    };
    connection
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM {TABLE}
                 WHERE source_kind = 'reflection' AND source_session_id = ?1
                   AND source_message_id = ?2 AND fingerprint = ?3"
            ),
            params![source_session_id, source_message_id, fingerprint],
            decode_row,
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| {
            query_error(std::io::Error::other(
                "idempotent memory candidate disappeared during insertion",
            ))
        })
        .and_then(decode_record)
}

type StoredRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<i64>,
);

fn decode_row(row: &Row<'_>) -> rusqlite::Result<StoredRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
        row.get(18)?,
    ))
}

fn decode_record(row: StoredRow) -> Result<MemoryCandidateRecord, DbError> {
    let (
        id,
        target,
        target_path,
        action,
        content,
        old_text,
        reason,
        confidence,
        source,
        source_session_id,
        source_message_id,
        fingerprint,
        status,
        before_entries,
        after_entries,
        error,
        time_created,
        time_updated,
        time_applied,
    ) = row;
    let target = MemoryScope::parse(&target)
        .ok_or_else(|| query_error(std::io::Error::other("unknown memory target")))?;
    let action = MemoryAction::parse(&action)
        .ok_or_else(|| query_error(std::io::Error::other("unknown memory action")))?;
    let source = MemorySource::parse(&source)
        .ok_or_else(|| query_error(std::io::Error::other("unknown memory source")))?;
    let status = MemoryCandidateStatus::parse(&status)
        .ok_or_else(|| query_error(std::io::Error::other("unknown memory candidate status")))?;
    let confidence = u16::try_from(confidence)
        .ok()
        .filter(|value| *value <= 10_000)
        .ok_or_else(|| query_error(std::io::Error::other("invalid memory confidence")))?;
    Ok(MemoryCandidateRecord {
        projection: MemoryCandidateProjection {
            id,
            scope: target,
            action,
            content,
            old_text,
            reason,
            confidence,
            source,
            source_session_id,
            source_message_id,
            status,
            error,
            time_created,
            time_updated,
        },
        target_path,
        fingerprint,
        before_entries: decode_entries(before_entries)?,
        after_entries: decode_entries(after_entries)?,
        time_applied,
    })
}

fn decode_entries(value: Option<String>) -> Result<Option<Vec<String>>, DbError> {
    value
        .map(|value| {
            serde_json::from_str(&value).map_err(|source| DbError::Decode {
                table: TABLE.to_owned(),
                source,
            })
        })
        .transpose()
}

fn require_changed(changed: usize, id: &str) -> Result<(), DbError> {
    if changed == 0 {
        return Err(query_error(std::io::Error::other(format!(
            "memory candidate {id} is not in a mutable state"
        ))));
    }
    Ok(())
}

fn query_error(error: impl std::error::Error + Send + Sync + 'static) -> DbError {
    DbError::Query {
        source: Box::new(error),
    }
}
