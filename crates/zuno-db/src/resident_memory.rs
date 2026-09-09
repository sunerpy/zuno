//! Versioned resident-memory authority and its independently recoverable file projection.

use crate::event_log::query_error;
use crate::{Pool, open};
use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::MemoryScope;

const COLUMNS: &str = "path, scope, revision, entries, content_digest, projected_revision, \
    projection_error, time_created, time_updated";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentMemoryDocument {
    pub path: String,
    pub scope: MemoryScope,
    pub revision: i64,
    pub entries: Vec<String>,
    pub content_digest: String,
    pub projected_revision: i64,
    pub projection_error: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentMemoryOperation {
    Apply,
    Undo,
}

pub struct ResidentMemoryCommit<'a> {
    pub path: &'a str,
    pub scope: MemoryScope,
    pub expected_revision: i64,
    pub before: &'a [String],
    pub after: &'a [String],
    pub candidate_id: &'a str,
    pub operation: ResidentMemoryOperation,
    pub now: i64,
    pub learning_authority: Option<(&'a str, &'a crate::learning_job::LearningLease)>,
}

#[derive(Clone)]
pub struct ResidentMemoryStore {
    pool: Arc<Pool>,
}

impl ResidentMemoryStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn get(&self, path: &str) -> Result<Option<ResidentMemoryDocument>, DbError> {
        let connection = self.pool.get()?;
        read(&connection, path)
    }

    /// Import one previously published file exactly once. Existing authority wins.
    pub fn adopt(
        &self,
        path: &str,
        scope: MemoryScope,
        entries: &[String],
        now: i64,
    ) -> Result<ResidentMemoryDocument, DbError> {
        if path.trim().is_empty() {
            return Err(query_error(std::io::Error::other("memory path is empty")));
        }
        let serialized = serde_json::to_string(entries).map_err(query_error)?;
        let digest = content_digest(&serialized);
        self.pool.transaction(|transaction| {
            if let Some(existing) = read(transaction, path)? {
                if existing.scope != scope {
                    return Err(conflict(path, "resident scope changed"));
                }
                return Ok(existing);
            }
            transaction
                .execute(
                    "INSERT INTO resident_memory_document
                 (path, scope, revision, entries, content_digest, projected_revision,
                  time_created, time_updated)
                 VALUES (?1, ?2, 1, ?3, ?4, 1, ?5, ?5)",
                    params![path, scope.as_str(), serialized, digest, now],
                )
                .map_err(open::map_error)?;
            transaction
                .execute(
                    "INSERT INTO resident_memory_revision
                 (path, revision, entries, content_digest, operation, time_created)
                 VALUES (?1, 1, ?2, ?3, 'import', ?4)",
                    params![path, serialized, digest, now],
                )
                .map_err(open::map_error)?;
            required(transaction, path)
        })
    }

    /// Commit entries, revision history, and the candidate terminal state together.
    ///
    /// File publication is derived work. Losing that publication never loses this
    /// committed document or leaves an accepted candidate without its contents.
    pub fn commit(
        &self,
        input: ResidentMemoryCommit<'_>,
    ) -> Result<ResidentMemoryDocument, DbError> {
        let before = serde_json::to_string(input.before).map_err(query_error)?;
        let after = serde_json::to_string(input.after).map_err(query_error)?;
        let digest = content_digest(&after);
        self.pool.transaction(|transaction| {
            if let Some((job_id, lease)) = input.learning_authority {
                let authorized: bool = transaction
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM learning_job j
                       LEFT JOIN session_memory_policy p ON p.session_id=j.session_id
                       JOIN memory_candidate c ON c.source_session_id=j.session_id
                       WHERE j.id=?1 AND j.status='running' AND j.kind='extraction'
                         AND j.owner_id=?2 AND j.lease_token=?3 AND j.lease_expires>?4
                         AND COALESCE(p.generation,'enabled')='enabled' AND c.id=?5)",
                        params![
                            job_id,
                            lease.owner_id,
                            lease.token,
                            input.now,
                            input.candidate_id
                        ],
                        |row| row.get(0),
                    )
                    .map_err(open::map_error)?;
                if !authorized {
                    return Err(conflict(
                        job_id,
                        "learning lease or generation policy no longer authorizes memory",
                    ));
                }
            }
            let current = required(transaction, input.path)?;
            if current.scope != input.scope
                || current.revision != input.expected_revision
                || current.entries != input.before
            {
                return Err(conflict(
                    input.path,
                    "resident memory revision or contents changed",
                ));
            }
            let revision = current
                .revision
                .checked_add(1)
                .ok_or_else(|| conflict(input.path, "resident revision exhausted"))?;
            let (operation, status, allowed) = match input.operation {
                ResidentMemoryOperation::Apply => ("apply", "applied", "'pending','failed'"),
                ResidentMemoryOperation::Undo => ("undo", "undone", "'applied'"),
            };
            let changed = transaction
                .execute(
                    &format!(
                        "UPDATE memory_candidate
                     SET status = ?2,
                         before_entries = CASE WHEN ?3 = 'apply' THEN ?4 ELSE before_entries END,
                         after_entries = CASE WHEN ?3 = 'apply' THEN ?5 ELSE after_entries END,
                         time_applied = CASE WHEN ?3 = 'apply' THEN ?6 ELSE time_applied END,
                         time_updated = ?6, error = NULL
                     WHERE id = ?1 AND target = ?7 AND status IN ({allowed})"
                    ),
                    params![
                        input.candidate_id,
                        status,
                        operation,
                        before,
                        after,
                        input.now,
                        input.scope.as_str()
                    ],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Err(DbError::Conflict {
                    table: "memory_candidate".to_owned(),
                    id: input.candidate_id.to_owned(),
                    detail: "candidate is no longer eligible for this resident mutation".to_owned(),
                });
            }
            if input.before == input.after {
                return Ok(current);
            }
            let changed = transaction
                .execute(
                    "UPDATE resident_memory_document
                 SET revision = ?3, entries = ?4, content_digest = ?5,
                     projection_error = NULL, time_updated = ?6
                 WHERE path = ?1 AND revision = ?2",
                    params![
                        input.path,
                        input.expected_revision,
                        revision,
                        after,
                        digest,
                        input.now
                    ],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Err(conflict(
                    input.path,
                    "resident revision compare-and-set failed",
                ));
            }
            transaction
                .execute(
                    "INSERT INTO resident_memory_revision
                 (path, revision, entries, content_digest, operation, candidate_id, time_created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        input.path,
                        revision,
                        after,
                        digest,
                        operation,
                        input.candidate_id,
                        input.now
                    ],
                )
                .map_err(open::map_error)?;
            required(transaction, input.path)
        })
    }

    pub fn revision_entries(&self, path: &str, revision: i64) -> Result<Vec<String>, DbError> {
        let serialized: String = self
            .pool
            .get()?
            .query_row(
                "SELECT entries FROM resident_memory_revision WHERE path = ?1 AND revision = ?2",
                params![path, revision],
                |row| row.get(0),
            )
            .optional()
            .map_err(open::map_error)?
            .ok_or_else(|| conflict(path, "resident projection revision is missing"))?;
        serde_json::from_str(&serialized).map_err(query_error)
    }

    /// Explicit user import of an edited projection. Never called by reconciliation.
    pub fn import_projection(
        &self,
        path: &str,
        scope: MemoryScope,
        entries: &[String],
        now: i64,
    ) -> Result<ResidentMemoryDocument, DbError> {
        let current = self.get(path)?;
        let Some(current) = current else {
            return self.adopt(path, scope, entries, now);
        };
        if current.scope != scope {
            return Err(conflict(path, "resident scope changed"));
        }
        if current.entries == entries {
            return Ok(current);
        }
        let serialized = serde_json::to_string(entries).map_err(query_error)?;
        let digest = content_digest(&serialized);
        self.pool.transaction(|transaction| {
            let revision = current
                .revision
                .checked_add(1)
                .ok_or_else(|| conflict(path, "resident revision exhausted"))?;
            let changed = transaction
                .execute(
                    "UPDATE resident_memory_document
                 SET entries=?3,content_digest=?4,revision=?5,projected_revision=?5,
                     projection_error=NULL,time_updated=?6 WHERE path=?1 AND revision=?2",
                    params![path, current.revision, serialized, digest, revision, now],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Err(conflict(path, "resident changed before explicit import"));
            }
            transaction
                .execute(
                    "INSERT INTO resident_memory_revision
                 (path,revision,entries,content_digest,operation,time_created)
                 VALUES (?1,?2,?3,?4,'import',?5)",
                    params![path, revision, serialized, digest, now],
                )
                .map_err(open::map_error)?;
            required(transaction, path)
        })
    }

    /// A stale projector cannot mark a newer document as published.
    pub fn record_projection(
        &self,
        path: &str,
        revision: i64,
        error: Option<&str>,
    ) -> Result<bool, DbError> {
        self.pool.transaction(|transaction| {
            transaction
                .execute(
                    "UPDATE resident_memory_document
                 SET projected_revision = CASE WHEN ?3 IS NULL THEN ?2 ELSE projected_revision END,
                     projection_error = ?3
                 WHERE path = ?1 AND revision = ?2
                   AND ((?3 IS NULL AND (projected_revision <> ?2 OR projection_error IS NOT NULL))
                     OR (?3 IS NOT NULL AND projection_error IS NOT ?3))",
                    params![path, revision, error],
                )
                .map(|changed| changed == 1)
                .map_err(open::map_error)
        })
    }
}

fn content_digest(serialized: &str) -> String {
    hex::encode(Sha256::digest(serialized.as_bytes()))
}

fn read(connection: &Connection, path: &str) -> Result<Option<ResidentMemoryDocument>, DbError> {
    type Stored = (
        String,
        String,
        i64,
        String,
        String,
        i64,
        Option<String>,
        i64,
        i64,
    );
    let stored: Option<Stored> = connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM resident_memory_document WHERE path = ?1"),
            [path],
            |row| {
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
                ))
            },
        )
        .optional()
        .map_err(open::map_error)?;
    stored
        .map(|row| {
            let scope = match row.1.as_str() {
                "global" => MemoryScope::Global,
                "project" => MemoryScope::Project,
                _ => return Err(conflict(path, "unknown resident memory scope")),
            };
            let entries = serde_json::from_str(&row.3).map_err(query_error)?;
            if content_digest(&row.3) != row.4 || row.2 < 1 || !(0..=row.2).contains(&row.5) {
                return Err(conflict(
                    path,
                    "resident memory digest or revision is corrupt",
                ));
            }
            Ok(ResidentMemoryDocument {
                path: row.0,
                scope,
                revision: row.2,
                entries,
                content_digest: row.4,
                projected_revision: row.5,
                projection_error: row.6,
                time_created: row.7,
                time_updated: row.8,
            })
        })
        .transpose()
}

fn required(connection: &Connection, path: &str) -> Result<ResidentMemoryDocument, DbError> {
    read(connection, path)?.ok_or_else(|| DbError::NotFound {
        table: "resident_memory_document".to_owned(),
        id: path.to_owned(),
    })
}

fn conflict(path: &str, detail: &str) -> DbError {
    DbError::Conflict {
        table: "resident_memory_document".to_owned(),
        id: path.to_owned(),
        detail: detail.to_owned(),
    }
}
