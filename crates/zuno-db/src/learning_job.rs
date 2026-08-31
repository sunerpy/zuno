//! Durable, idempotent work queue for extraction, aggregation, evaluation, and Skill changes.

use crate::event_log::query_error;
use crate::{Pool, open};
use rusqlite::{OptionalExtension as _, Row, params};
use serde_json::Value;
use std::sync::Arc;
use zuno_error::DbError;

const COLUMNS: &str = "id, project_id, session_id, source_message_id, kind, extractor_version, \
    idempotency_key, status, attempt, owner_id, lease_expires, scheduled_at, payload, result, \
    error, time_created, time_updated, time_completed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LearningJobKind {
    Extraction,
    ProjectAggregation,
    GlobalAggregation,
    Evaluation,
    SkillApply,
    SkillUndo,
}

impl LearningJobKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Extraction => "extraction",
            Self::ProjectAggregation => "project_aggregation",
            Self::GlobalAggregation => "global_aggregation",
            Self::Evaluation => "evaluation",
            Self::SkillApply => "skill_apply",
            Self::SkillUndo => "skill_undo",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "extraction" => Ok(Self::Extraction),
            "project_aggregation" => Ok(Self::ProjectAggregation),
            "global_aggregation" => Ok(Self::GlobalAggregation),
            "evaluation" => Ok(Self::Evaluation),
            "skill_apply" => Ok(Self::SkillApply),
            "skill_undo" => Ok(Self::SkillUndo),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown learning job kind `{value}`"
            )))),
        }
    }

    const fn replay_safe_after_lease_loss(self) -> bool {
        matches!(
            self,
            Self::Extraction
                | Self::ProjectAggregation
                | Self::GlobalAggregation
                | Self::Evaluation
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LearningJobStatus {
    Queued,
    Running,
    Completed,
    Skipped,
    Failed,
    Uncertain,
}

impl LearningJobStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "skipped" => Ok(Self::Skipped),
            "failed" => Ok(Self::Failed),
            "uncertain" => Ok(Self::Uncertain),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown learning job status `{value}`"
            )))),
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Skipped | Self::Failed | Self::Uncertain
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LearningJobRecord {
    pub id: String,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub source_message_id: Option<String>,
    pub kind: LearningJobKind,
    pub extractor_version: Option<String>,
    pub idempotency_key: String,
    pub status: LearningJobStatus,
    pub attempt: u32,
    pub owner_id: Option<String>,
    pub lease_expires: Option<i64>,
    pub scheduled_at: i64,
    pub payload: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
    pub time_completed: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewLearningJob {
    pub id: String,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub source_message_id: Option<String>,
    pub kind: LearningJobKind,
    pub extractor_version: Option<String>,
    pub idempotency_key: String,
    pub scheduled_at: i64,
    pub payload: Option<Value>,
    pub time_created: i64,
}

impl NewLearningJob {
    #[must_use]
    pub fn extraction(
        id: impl Into<String>,
        project_id: impl Into<String>,
        session_id: impl Into<String>,
        source_message_id: impl Into<String>,
        extractor_version: impl Into<String>,
        payload: Value,
        now: i64,
    ) -> Self {
        let session_id = session_id.into();
        let source_message_id = source_message_id.into();
        let extractor_version = extractor_version.into();
        let idempotency_key =
            format!("extraction:{session_id}:{source_message_id}:{extractor_version}");
        Self {
            id: id.into(),
            project_id: Some(project_id.into()),
            session_id: Some(session_id),
            source_message_id: Some(source_message_id),
            kind: LearningJobKind::Extraction,
            extractor_version: Some(extractor_version),
            idempotency_key,
            scheduled_at: now,
            payload: Some(payload),
            time_created: now,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LearningJobInsert {
    pub record: LearningJobRecord,
    pub inserted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseReconciliation {
    pub requeued: usize,
    pub uncertain: usize,
}

#[derive(Clone)]
pub struct LearningJobStore {
    pool: Arc<Pool>,
}

impl LearningJobStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn enqueue(&self, job: NewLearningJob) -> Result<LearningJobInsert, DbError> {
        validate_new(&job)?;
        let payload = job
            .payload
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(query_error)?;
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "INSERT INTO learning_job (
                       id, project_id, session_id, source_message_id, kind, extractor_version,
                       idempotency_key, status, attempt, scheduled_at, payload,
                       time_created, time_updated
                     ) VALUES (
                       ?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', 0, ?8, ?9, ?10, ?10
                     )
                     ON CONFLICT(idempotency_key) DO NOTHING",
                    params![
                        job.id,
                        job.project_id,
                        job.session_id,
                        job.source_message_id,
                        job.kind.as_str(),
                        job.extractor_version,
                        job.idempotency_key,
                        job.scheduled_at,
                        payload,
                        job.time_created,
                    ],
                )
                .map_err(open::map_error)?;
            Ok(LearningJobInsert {
                record: read_by_key(transaction, &job.idempotency_key)?,
                inserted: changed == 1,
            })
        })
    }

    /// Claim the oldest due job. A single SQLite write transaction owns selection.
    pub fn claim_due(
        &self,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
    ) -> Result<Option<LearningJobRecord>, DbError> {
        if owner_id.trim().is_empty() || lease_expires <= now {
            return Err(query_error(std::io::Error::other(
                "learning job lease must have a non-empty owner and future deadline",
            )));
        }
        self.pool.transaction(|transaction| {
            let id = transaction
                .query_row(
                    "SELECT id FROM learning_job
                     WHERE status = 'queued' AND scheduled_at <= ?1
                     ORDER BY scheduled_at, time_created, id LIMIT 1",
                    [now],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(open::map_error)?;
            let Some(id) = id else {
                return Ok(None);
            };
            let changed = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'running', attempt = attempt + 1, owner_id = ?2,
                         lease_expires = ?3, error = NULL, time_updated = ?4
                     WHERE id = ?1 AND status = 'queued'",
                    params![id, owner_id, lease_expires, now],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Ok(None);
            }
            read_required(transaction, &id).map(Some)
        })
    }

    /// Claim replay-safe learning work for one project without consuming
    /// another project's extraction or companion-Skill aggregation.
    ///
    /// Global aggregation has no project-owned filesystem destination and may
    /// therefore be claimed by any active project worker.
    pub fn claim_due_for_project(
        &self,
        project_id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
    ) -> Result<Option<LearningJobRecord>, DbError> {
        if project_id.trim().is_empty() || owner_id.trim().is_empty() || lease_expires <= now {
            return Err(query_error(std::io::Error::other(
                "project learning claim requires project and owner ids plus a future deadline",
            )));
        }
        self.pool.transaction(|transaction| {
            let id = transaction
                .query_row(
                    "SELECT id FROM learning_job
                     WHERE status = 'queued' AND scheduled_at <= ?1
                       AND kind IN ('extraction','project_aggregation','global_aggregation')
                       AND (project_id = ?2 OR kind = 'global_aggregation')
                     ORDER BY scheduled_at, time_created, id LIMIT 1",
                    params![now, project_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(open::map_error)?;
            let Some(id) = id else {
                return Ok(None);
            };
            let changed = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'running', attempt = attempt + 1, owner_id = ?2,
                         lease_expires = ?3, error = NULL, time_updated = ?4
                     WHERE id = ?1 AND status = 'queued'",
                    params![id, owner_id, lease_expires, now],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Ok(None);
            }
            read_required(transaction, &id).map(Some)
        })
    }

    /// Claim one known queued job. Callers that just admitted a specific
    /// extraction must not accidentally consume an older aggregation or Skill
    /// job from the shared queue.
    pub fn claim(
        &self,
        id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
    ) -> Result<Option<LearningJobRecord>, DbError> {
        if id.trim().is_empty() || owner_id.trim().is_empty() || lease_expires <= now {
            return Err(query_error(std::io::Error::other(
                "learning job claim requires an id, non-empty owner, and future deadline",
            )));
        }
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'running', attempt = attempt + 1, owner_id = ?2,
                         lease_expires = ?3, error = NULL, time_updated = ?4
                     WHERE id = ?1 AND status = 'queued' AND scheduled_at <= ?4",
                    params![id, owner_id, lease_expires, now],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Ok(None);
            }
            read_required(transaction, id).map(Some)
        })
    }

    pub fn settle(
        &self,
        id: &str,
        owner_id: &str,
        status: LearningJobStatus,
        result: Option<&Value>,
        error: Option<&str>,
        now: i64,
    ) -> Result<LearningJobRecord, DbError> {
        if !status.is_terminal() {
            return Err(query_error(std::io::Error::other(
                "learning job settlement must be terminal",
            )));
        }
        let result = result
            .map(serde_json::to_string)
            .transpose()
            .map_err(query_error)?;
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = ?3, result = ?4, error = ?5, owner_id = NULL,
                         lease_expires = NULL, time_updated = ?6, time_completed = ?6
                     WHERE id = ?1 AND owner_id = ?2 AND status = 'running'",
                    params![id, owner_id, status.as_str(), result, error, now],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Err(query_error(std::io::Error::other(format!(
                    "learning job `{id}` is not running for owner `{owner_id}`"
                ))));
            }
            read_required(transaction, id)
        })
    }

    /// Recover pure jobs, but mark side-effectful Skill jobs uncertain.
    pub fn reconcile_expired(&self, now: i64) -> Result<LeaseReconciliation, DbError> {
        self.pool.transaction(|transaction| {
            let requeued = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'queued', owner_id = NULL, lease_expires = NULL,
                         error = 'worker lease expired before a durable result',
                         scheduled_at = ?1, time_updated = ?1
                     WHERE status = 'running' AND lease_expires <= ?1
                       AND kind IN ('extraction','project_aggregation','global_aggregation','evaluation')",
                    [now],
                )
                .map_err(open::map_error)?;
            let uncertain = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'uncertain', owner_id = NULL, lease_expires = NULL,
                         error = 'side-effectful worker lease expired; inspect authoritative state',
                         time_updated = ?1, time_completed = ?1
                     WHERE status = 'running' AND lease_expires <= ?1
                       AND kind IN ('skill_apply','skill_undo')",
                    [now],
                )
                .map_err(open::map_error)?;
            Ok(LeaseReconciliation {
                requeued,
                uncertain,
            })
        })
    }

    pub fn get(&self, id: &str) -> Result<LearningJobRecord, DbError> {
        let connection = self.pool.get()?;
        read_required(&connection, id)
    }

    pub fn list_for_project(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<LearningJobRecord>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM learning_job
                 WHERE project_id = ?1 ORDER BY time_created DESC, id DESC LIMIT ?2"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map(params![project_id, limit as i64], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode))
            .collect()
    }
}

fn validate_new(job: &NewLearningJob) -> Result<(), DbError> {
    if job.id.trim().is_empty() || job.idempotency_key.trim().is_empty() {
        return Err(query_error(std::io::Error::other(
            "learning job id and idempotency key must not be empty",
        )));
    }
    if job.kind == LearningJobKind::Extraction
        && (job.session_id.is_none()
            || job.source_message_id.is_none()
            || job
                .extractor_version
                .as_deref()
                .is_none_or(|version| version.trim().is_empty()))
    {
        return Err(query_error(std::io::Error::other(
            "extraction jobs require session, source message, and extractor version",
        )));
    }
    Ok(())
}

fn read_required(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<LearningJobRecord, DbError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM learning_job WHERE id = ?1"),
            [id],
            decode_row,
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: "learning_job".to_owned(),
            id: id.to_owned(),
        })
        .and_then(decode)
}

fn read_by_key(connection: &rusqlite::Connection, key: &str) -> Result<LearningJobRecord, DbError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM learning_job WHERE idempotency_key = ?1"),
            [key],
            decode_row,
        )
        .map_err(open::map_error)
        .and_then(decode)
}

type StoredJob = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    String,
    String,
    i64,
    Option<String>,
    Option<i64>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<i64>,
);

fn decode_row(row: &Row<'_>) -> rusqlite::Result<StoredJob> {
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
    ))
}

fn decode(row: StoredJob) -> Result<LearningJobRecord, DbError> {
    let attempt = u32::try_from(row.8).map_err(query_error)?;
    let payload = row
        .12
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(query_error)?;
    let result = row
        .13
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(query_error)?;
    let kind = LearningJobKind::parse(&row.4)?;
    debug_assert!(
        kind.replay_safe_after_lease_loss()
            || matches!(
                kind,
                LearningJobKind::SkillApply | LearningJobKind::SkillUndo
            )
    );
    Ok(LearningJobRecord {
        id: row.0,
        project_id: row.1,
        session_id: row.2,
        source_message_id: row.3,
        kind,
        extractor_version: row.5,
        idempotency_key: row.6,
        status: LearningJobStatus::parse(&row.7)?,
        attempt,
        owner_id: row.9,
        lease_expires: row.10,
        scheduled_at: row.11,
        payload,
        result,
        error: row.14,
        time_created: row.15,
        time_updated: row.16,
        time_completed: row.17,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration;
    use serde_json::json;
    use zuno_paths::DbLocation;

    fn store() -> LearningJobStore {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
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
        LearningJobStore::new(pool)
    }

    #[test]
    fn extraction_identity_is_idempotent_and_restart_requeues_it() {
        let store = store();
        let first = store
            .enqueue(NewLearningJob::extraction(
                "job-1",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({"transcript": "durable"}),
                10,
            ))
            .expect("enqueue");
        assert!(first.inserted);
        let duplicate = store
            .enqueue(NewLearningJob::extraction(
                "job-2",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({"transcript": "ignored duplicate"}),
                11,
            ))
            .expect("duplicate");
        assert!(!duplicate.inserted);
        assert_eq!(duplicate.record.id, "job-1");

        let claimed = store
            .claim_due("worker-1", 12, 20)
            .expect("claim")
            .expect("job");
        assert_eq!(claimed.attempt, 1);
        let reconciled = store.reconcile_expired(20).expect("reconcile");
        assert_eq!(reconciled.requeued, 1);
        assert_eq!(reconciled.uncertain, 0);
        let reclaimed = store
            .claim_due("worker-2", 21, 30)
            .expect("reclaim")
            .expect("job");
        assert_eq!(reclaimed.id, "job-1");
        assert_eq!(reclaimed.attempt, 2);
    }

    #[test]
    fn project_worker_does_not_claim_another_projects_learning_job() {
        let store = store();
        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('project-2', '/workspace-2', 1, 1, '[]')",
                    [],
                )
                .expect("second project");
        }
        store
            .enqueue(NewLearningJob {
                id: "job-project-2".to_owned(),
                project_id: Some("project-2".to_owned()),
                session_id: None,
                source_message_id: None,
                kind: LearningJobKind::ProjectAggregation,
                extractor_version: None,
                idempotency_key: "project-2-aggregation".to_owned(),
                scheduled_at: 10,
                payload: Some(json!({"since": 0})),
                time_created: 10,
            })
            .expect("foreign project job");

        assert!(
            store
                .claim_due_for_project("project-1", "worker-1", 11, 20)
                .expect("claim")
                .is_none()
        );
        assert_eq!(
            store
                .claim_due_for_project("project-2", "worker-2", 11, 20)
                .expect("claim")
                .expect("project-2 job")
                .id,
            "job-project-2"
        );
    }
}
