//! Durable, idempotent work queue for extraction, aggregation, evaluation, and Skill changes.

use crate::event_log::query_error;
use crate::{Pool, open};
use rusqlite::{Connection, OptionalExtension as _, Row, params};
use serde_json::Value;
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::SessionMemoryGeneration;

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

/// Transactional admission result for one extraction job.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractionJobInsert {
    Admitted(Box<LearningJobInsert>),
    Blocked(SessionMemoryGeneration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseReconciliation {
    pub requeued: usize,
    pub skipped: usize,
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
        self.pool
            .transaction(|transaction| enqueue_in(transaction, &job, payload.as_deref()))
    }

    /// Admit extraction only while the session's durable generation policy is enabled.
    ///
    /// The policy read and queue insert share one `IMMEDIATE` transaction, so a
    /// concurrent disable either skips an already queued row or wins before this
    /// method and prevents the row from being created.
    pub fn enqueue_extraction_if_enabled(
        &self,
        job: NewLearningJob,
    ) -> Result<ExtractionJobInsert, DbError> {
        validate_new(&job)?;
        if job.kind != LearningJobKind::Extraction {
            return Err(query_error(std::io::Error::other(
                "session generation admission is only valid for extraction jobs",
            )));
        }
        let session_id = job
            .session_id
            .as_deref()
            .expect("validated extraction jobs have a session id");
        let payload = job
            .payload
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(query_error)?;
        self.pool.transaction(|transaction| {
            let generation = transaction
                .query_row(
                    "SELECT generation FROM session_memory_policy WHERE session_id = ?1",
                    [session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(open::map_error)?
                .map(|value| {
                    SessionMemoryGeneration::parse(&value).ok_or_else(|| {
                        query_error(std::io::Error::other(format!(
                            "unknown session memory generation `{value}`"
                        )))
                    })
                })
                .transpose()?
                .unwrap_or_default();
            if generation != SessionMemoryGeneration::Enabled {
                return Ok(ExtractionJobInsert::Blocked(generation));
            }
            enqueue_in(transaction, &job, payload.as_deref())
                .map(Box::new)
                .map(ExtractionJobInsert::Admitted)
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
                       AND (
                         kind <> 'extraction'
                         OR NOT EXISTS (
                           SELECT 1 FROM session_memory_policy
                           WHERE session_memory_policy.session_id = learning_job.session_id
                             AND session_memory_policy.generation <> 'enabled'
                         )
                       )
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
                       AND (
                         kind <> 'extraction'
                         OR NOT EXISTS (
                           SELECT 1 FROM session_memory_policy
                           WHERE session_memory_policy.session_id = learning_job.session_id
                             AND session_memory_policy.generation <> 'enabled'
                         )
                       )
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

    /// Claim due project work while admitting automatic extraction only after
    /// the source session is idle, has no pending input, and still permits generation.
    pub fn claim_due_for_project_eligible(
        &self,
        project_id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
        idle_before: i64,
    ) -> Result<Option<LearningJobRecord>, DbError> {
        self.claim_due_for_project_eligible_excluding(
            project_id,
            owner_id,
            now,
            lease_expires,
            idle_before,
            &[],
        )
    }

    /// Claim eligible project work while excluding process-local live sessions.
    pub fn claim_due_for_project_eligible_excluding(
        &self,
        project_id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
        idle_before: i64,
        busy_session_ids: &[String],
    ) -> Result<Option<LearningJobRecord>, DbError> {
        if project_id.trim().is_empty() || owner_id.trim().is_empty() || lease_expires <= now {
            return Err(query_error(std::io::Error::other(
                "project learning claim requires project and owner ids plus a future deadline",
            )));
        }
        let busy_session_ids = serde_json::to_string(busy_session_ids).map_err(query_error)?;
        self.pool.transaction(|transaction| {
            let id = transaction
                .query_row(
                    "SELECT learning_job.id FROM learning_job
                     WHERE learning_job.status = 'queued'
                       AND learning_job.scheduled_at <= ?1
                       AND learning_job.kind IN (
                         'extraction','project_aggregation','global_aggregation'
                       )
                       AND (
                         learning_job.project_id = ?2
                         OR learning_job.kind = 'global_aggregation'
                       )
                       AND (
                         learning_job.kind <> 'extraction'
                         OR (
                           NOT EXISTS (
                             SELECT 1 FROM session_memory_policy
                             WHERE session_memory_policy.session_id = learning_job.session_id
                               AND session_memory_policy.generation <> 'enabled'
                           )
                           AND NOT EXISTS (
                             SELECT 1 FROM json_each(?4)
                             WHERE json_each.value = learning_job.session_id
                           )
                           AND (
                             json_extract(learning_job.payload, '$.trigger') = 'manual'
                             OR (
                               COALESCE(
                                 json_extract(learning_job.payload, '$.trigger'),
                                 'automatic_post_turn'
                               ) = 'automatic_post_turn'
                               AND EXISTS (
                                 SELECT 1 FROM session
                                 WHERE session.id = learning_job.session_id
                                   AND session.time_updated <= ?3
                               )
                               AND NOT EXISTS (
                                 SELECT 1 FROM session_input
                                 WHERE session_input.session_id = learning_job.session_id
                                   AND session_input.state IN ('queued','steering','promoted')
                               )
                             )
                           )
                         )
                       )
                     ORDER BY learning_job.scheduled_at, learning_job.time_created,
                              learning_job.id
                     LIMIT 1",
                    params![now, project_id, idle_before, busy_session_ids],
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
                     WHERE id = ?1 AND status = 'queued' AND scheduled_at <= ?4
                       AND (
                         kind <> 'extraction'
                         OR NOT EXISTS (
                           SELECT 1 FROM session_memory_policy
                           WHERE session_memory_policy.session_id = learning_job.session_id
                             AND session_memory_policy.generation <> 'enabled'
                         )
                       )",
                    params![id, owner_id, lease_expires, now],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Ok(None);
            }
            read_required(transaction, id).map(Some)
        })
    }

    /// Claim one automatic extraction only when its session remains idle and eligible.
    pub fn claim_automatic_extraction(
        &self,
        id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
        idle_before: i64,
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
                     WHERE id = ?1 AND kind = 'extraction' AND status = 'queued'
                       AND scheduled_at <= ?4
                       AND EXISTS (
                         SELECT 1 FROM session
                         WHERE session.id = learning_job.session_id
                           AND session.time_updated <= ?5
                       )
                       AND NOT EXISTS (
                         SELECT 1 FROM session_memory_policy
                         WHERE session_memory_policy.session_id = learning_job.session_id
                           AND session_memory_policy.generation <> 'enabled'
                       )
                       AND NOT EXISTS (
                         SELECT 1 FROM session_input
                         WHERE session_input.session_id = learning_job.session_id
                           AND session_input.state IN ('queued','steering','promoted')
                       )",
                    params![id, owner_id, lease_expires, now, idle_before],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Ok(None);
            }
            read_required(transaction, id).map(Some)
        })
    }

    /// Turn automatic or explicitly retried extraction into immediately runnable manual work.
    ///
    /// Completed, running, and uncertain rows remain immutable. A skipped or
    /// failed row may be revived only by this explicit manual path; the durable
    /// idempotency identity remains unchanged.
    pub fn expedite_manual_extraction(
        &self,
        id: &str,
        payload: &Value,
        now: i64,
    ) -> Result<LearningJobRecord, DbError> {
        if id.trim().is_empty() {
            return Err(query_error(std::io::Error::other(
                "manual extraction expedite requires a job id",
            )));
        }
        let payload = serde_json::to_string(payload).map_err(query_error)?;
        self.pool.transaction(|transaction| {
            transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'queued', attempt = 0, owner_id = NULL,
                         lease_expires = NULL, payload = ?2, scheduled_at = ?3,
                         result = NULL, error = NULL, time_updated = ?3,
                         time_completed = NULL
                     WHERE id = ?1 AND kind = 'extraction'
                       AND NOT EXISTS (
                         SELECT 1 FROM session_memory_policy
                         WHERE session_memory_policy.session_id = learning_job.session_id
                           AND session_memory_policy.generation <> 'enabled'
                       )
                       AND (
                         (status = 'queued' AND attempt = 0)
                         OR status IN ('skipped','failed')
                       )",
                    params![id, payload, now],
                )
                .map_err(open::map_error)?;
            read_required(transaction, id)
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

    /// Return one owned running job to the queue after a typed retryable failure.
    pub fn retry(
        &self,
        id: &str,
        owner_id: &str,
        error: &str,
        scheduled_at: i64,
        now: i64,
    ) -> Result<LearningJobRecord, DbError> {
        if id.trim().is_empty()
            || owner_id.trim().is_empty()
            || error.trim().is_empty()
            || scheduled_at < now
        {
            return Err(query_error(std::io::Error::other(
                "learning job retry requires ids, an error, and a non-past deadline",
            )));
        }
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'queued', owner_id = NULL, lease_expires = NULL,
                         scheduled_at = ?4, error = ?3, time_updated = ?5,
                         time_completed = NULL
                     WHERE id = ?1 AND owner_id = ?2 AND status = 'running'",
                    params![id, owner_id, error, scheduled_at, now],
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
            let skipped = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'skipped', owner_id = NULL, lease_expires = NULL,
                         result = '{\"kind\":\"sessionMemoryPolicy\",\"generation\":\"excluded\",\
\"reason\":\"worker lease expired after session exclusion\",\
\"source\":\"lease_reconciliation\"}',
                         error = NULL, time_updated = ?1, time_completed = ?1
                     WHERE status = 'running' AND lease_expires <= ?1
                       AND kind = 'extraction'
                       AND EXISTS (
                         SELECT 1 FROM session_memory_policy
                         WHERE session_memory_policy.session_id = learning_job.session_id
                           AND session_memory_policy.generation = 'excluded'
                       )",
                    [now],
                )
                .map_err(open::map_error)?;
            let requeued = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'queued', owner_id = NULL, lease_expires = NULL,
                         error = 'worker lease expired before a durable result',
                         scheduled_at = ?1, time_updated = ?1
                     WHERE status = 'running' AND lease_expires <= ?1
                       AND kind IN ('extraction','project_aggregation','global_aggregation','evaluation')
                       AND (
                         kind <> 'extraction'
                         OR NOT EXISTS (
                           SELECT 1 FROM session_memory_policy
                           WHERE session_memory_policy.session_id = learning_job.session_id
                             AND session_memory_policy.generation = 'excluded'
                         )
                       )",
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
                skipped,
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

fn enqueue_in(
    connection: &Connection,
    job: &NewLearningJob,
    payload: Option<&str>,
) -> Result<LearningJobInsert, DbError> {
    let changed = connection
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
        record: read_by_key(connection, &job.idempotency_key)?,
        inserted: changed == 1,
    })
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
        assert_eq!(reconciled.skipped, 0);
        assert_eq!(reconciled.uncertain, 0);
        let reclaimed = store
            .claim_due("worker-2", 21, 30)
            .expect("reclaim")
            .expect("job");
        assert_eq!(reclaimed.id, "job-1");
        assert_eq!(reclaimed.attempt, 2);
    }

    #[test]
    fn typed_retry_requeues_with_a_future_deadline_without_spending_an_extra_attempt() {
        let store = store();
        store
            .enqueue(NewLearningJob::extraction(
                "job-retry",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({"transcript":"durable"}),
                10,
            ))
            .expect("enqueue");
        store
            .claim("job-retry", "worker-1", 11, 30)
            .expect("claim")
            .expect("running job");
        let retried = store
            .retry("job-retry", "worker-1", "provider unavailable", 20, 12)
            .expect("retry");
        assert_eq!(retried.status, LearningJobStatus::Queued);
        assert_eq!(retried.attempt, 1);
        assert_eq!(retried.scheduled_at, 20);
        assert_eq!(retried.error.as_deref(), Some("provider unavailable"));
        assert!(
            store
                .claim("job-retry", "worker-2", 19, 30)
                .expect("early claim")
                .is_none()
        );
        assert_eq!(
            store
                .claim("job-retry", "worker-2", 20, 30)
                .expect("due claim")
                .expect("retried job")
                .attempt,
            2
        );
    }

    #[test]
    fn expired_extraction_is_skipped_instead_of_requeued_after_exclusion() {
        let store = store();
        store
            .enqueue(NewLearningJob::extraction(
                "job-excluded-lease",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({"trigger":"automatic_post_turn","request":{"transcript":"durable"}}),
                10,
            ))
            .expect("enqueue");
        store
            .claim("job-excluded-lease", "worker-1", 11, 20)
            .expect("claim")
            .expect("running job");
        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "INSERT INTO session_memory_policy (
                       session_id, use_memories, generation, reason, source, revision,
                       time_created, time_updated
                     ) VALUES (
                       'session-1', 1, 'excluded', 'external context', 'test', 1, 12, 12
                     )",
                    [],
                )
                .expect("exclude session");
        }

        let reconciled = store.reconcile_expired(20).expect("reconcile");
        assert_eq!(reconciled.requeued, 0);
        assert_eq!(reconciled.skipped, 1);
        assert_eq!(reconciled.uncertain, 0);
        let job = store.get("job-excluded-lease").expect("job");
        assert_eq!(job.status, LearningJobStatus::Skipped);
        assert_eq!(
            job.result.expect("skip result")["generation"],
            json!("excluded")
        );
    }

    #[test]
    fn extraction_admission_is_atomic_with_the_session_generation_policy() {
        let store = store();
        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "INSERT INTO session_memory_policy (
                       session_id, use_memories, generation, reason, source, revision,
                       time_created, time_updated
                     ) VALUES (
                       'session-1', 1, 'disabled', 'user choice', 'test', 1, 1, 1
                     )",
                    [],
                )
                .expect("disable generation");
        }

        let disabled = store
            .enqueue_extraction_if_enabled(NewLearningJob::extraction(
                "job-disabled",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-disabled",
                json!({"trigger": "manual", "request": {"transcript": "blocked"}}),
                10,
            ))
            .expect("disabled admission");
        assert_eq!(
            disabled,
            ExtractionJobInsert::Blocked(SessionMemoryGeneration::Disabled)
        );
        assert!(matches!(
            store.get("job-disabled"),
            Err(DbError::NotFound { ref table, .. }) if table == "learning_job"
        ));

        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "UPDATE session_memory_policy
                     SET generation = 'excluded', revision = 2, time_updated = 2
                     WHERE session_id = 'session-1'",
                    [],
                )
                .expect("exclude generation");
        }
        let excluded = store
            .enqueue_extraction_if_enabled(NewLearningJob::extraction(
                "job-excluded",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-excluded",
                json!({"trigger": "automatic_post_turn", "request": {"transcript": "blocked"}}),
                11,
            ))
            .expect("excluded admission");
        assert_eq!(
            excluded,
            ExtractionJobInsert::Blocked(SessionMemoryGeneration::Excluded)
        );
        assert!(matches!(
            store.get("job-excluded"),
            Err(DbError::NotFound { ref table, .. }) if table == "learning_job"
        ));
    }

    #[test]
    fn every_extraction_claim_rechecks_the_durable_generation_policy() {
        let store = store();
        store
            .enqueue(NewLearningJob::extraction(
                "job-manual-race",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({"trigger": "manual", "request": {"transcript": "durable"}}),
                10,
            ))
            .expect("enqueue before policy change");
        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "INSERT INTO session_memory_policy (
                       session_id, use_memories, generation, reason, source, revision,
                       time_created, time_updated
                     ) VALUES (
                       'session-1', 1, 'disabled', 'user choice', 'test', 1, 1, 1
                     )",
                    [],
                )
                .expect("disable generation");
        }

        assert!(
            store
                .claim("job-manual-race", "worker-1", 11, 30)
                .expect("known claim")
                .is_none()
        );
        assert!(
            store
                .claim_due_for_project_eligible("project-1", "worker-1", 11, 30, 11)
                .expect("project claim")
                .is_none()
        );
        assert_eq!(
            store.get("job-manual-race").expect("queued job").attempt,
            0,
            "a rejected policy claim must not spend an attempt"
        );

        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "UPDATE session_memory_policy
                     SET generation = 'enabled', revision = 2, time_updated = 2
                     WHERE session_id = 'session-1'",
                    [],
                )
                .expect("enable generation");
        }
        let claimed = store
            .claim("job-manual-race", "worker-2", 12, 30)
            .expect("enabled claim")
            .expect("manual job");
        assert_eq!(claimed.attempt, 1);
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

    #[test]
    fn automatic_claim_requires_idle_session_no_pending_input_and_enabled_policy() {
        let store = store();
        let mut job = NewLearningJob::extraction(
            "job-idle",
            "project-1",
            "session-1",
            "assistant-1",
            "extractor-v1",
            json!({
                "trigger": "automatic_post_turn",
                "request": {"transcript": "durable"}
            }),
            10,
        );
        job.scheduled_at = 20;
        store.enqueue(job).expect("enqueue");
        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "UPDATE session SET time_updated = 50 WHERE id = 'session-1'",
                    [],
                )
                .expect("mark recent activity");
        }

        assert!(
            store
                .claim_due_for_project_eligible("project-1", "worker-1", 60, 80, 40)
                .expect("active claim")
                .is_none(),
            "activity newer than idle_before must block the claim"
        );

        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "INSERT INTO session_input (
                       id, session_id, prompt, delivery, state, revision, admitted_seq,
                       time_created, time_updated
                     ) VALUES (
                       'input-1', 'session-1', '{}', 'queue', 'queued', 1, 1, 55, 55
                     )",
                    [],
                )
                .expect("queue input");
        }
        assert!(
            store
                .claim_due_for_project_eligible("project-1", "worker-1", 60, 80, 50)
                .expect("pending input claim")
                .is_none(),
            "pending input must block automatic extraction"
        );

        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "UPDATE session_input SET state = 'consumed' WHERE id = 'input-1'",
                    [],
                )
                .expect("consume input");
            connection
                .execute(
                    "INSERT INTO session_memory_policy (
                       session_id, use_memories, generation, reason, source, revision,
                       time_created, time_updated
                     ) VALUES (
                       'session-1', 1, 'disabled', 'user choice', 'test', 1, 1, 1
                     )",
                    [],
                )
                .expect("disable generation");
        }
        assert!(
            store
                .claim_due_for_project_eligible("project-1", "worker-1", 60, 80, 50)
                .expect("disabled policy claim")
                .is_none(),
            "disabled session policy must block automatic extraction"
        );

        {
            let connection = store.pool.get().expect("connection");
            connection
                .execute(
                    "UPDATE session_memory_policy SET generation = 'enabled'
                     WHERE session_id = 'session-1'",
                    [],
                )
                .expect("enable generation");
        }
        assert!(
            store
                .claim_due_for_project_eligible_excluding(
                    "project-1",
                    "worker-1",
                    60,
                    80,
                    50,
                    &["session-1".to_owned()],
                )
                .expect("busy-session claim")
                .is_none(),
            "a process-local live session must block extraction without spending an attempt"
        );
        assert_eq!(store.get("job-idle").expect("queued job").attempt, 0);
        let claimed = store
            .claim_due_for_project_eligible("project-1", "worker-1", 60, 80, 50)
            .expect("eligible claim")
            .expect("automatic job");
        assert_eq!(claimed.id, "job-idle");
        assert_eq!(claimed.attempt, 1);
    }

    #[test]
    fn manual_expedite_makes_a_future_automatic_job_due_without_resetting_attempts() {
        let store = store();
        let mut job = NewLearningJob::extraction(
            "job-manual",
            "project-1",
            "session-1",
            "assistant-1",
            "extractor-v1",
            json!({"trigger": "automatic_post_turn", "request": {"transcript": "old"}}),
            10,
        );
        job.scheduled_at = 1_000;
        store.enqueue(job).expect("enqueue");

        let updated = store
            .expedite_manual_extraction(
                "job-manual",
                &json!({"trigger": "manual", "request": {"transcript": "current"}}),
                20,
            )
            .expect("expedite");
        assert_eq!(updated.scheduled_at, 20);
        assert_eq!(updated.attempt, 0);
        assert_eq!(
            updated.payload.as_ref().expect("payload")["trigger"],
            "manual"
        );

        let claimed = store
            .claim("job-manual", "worker-1", 20, 40)
            .expect("claim")
            .expect("manual job");
        assert_eq!(claimed.attempt, 1);
        assert_eq!(
            claimed.payload.as_ref().expect("payload")["request"]["transcript"],
            "current"
        );
    }

    #[test]
    fn explicit_manual_reflection_revives_a_skipped_automatic_identity() {
        let store = store();
        store
            .enqueue(NewLearningJob::extraction(
                "job-revive",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({"trigger":"automatic_post_turn","request":{"transcript":"old"}}),
                10,
            ))
            .expect("enqueue");
        store
            .claim("job-revive", "worker-1", 10, 30)
            .expect("claim")
            .expect("running job");
        store
            .settle(
                "job-revive",
                "worker-1",
                LearningJobStatus::Skipped,
                Some(&json!({"reason":"automatic generation was disabled"})),
                None,
                11,
            )
            .expect("skip automatic job");

        let revived = store
            .expedite_manual_extraction(
                "job-revive",
                &json!({"trigger":"manual","request":{"transcript":"current"}}),
                20,
            )
            .expect("revive skipped identity");
        assert_eq!(revived.status, LearningJobStatus::Queued);
        assert_eq!(revived.attempt, 0);
        assert_eq!(revived.scheduled_at, 20);
        assert!(revived.result.is_none());
        assert!(revived.time_completed.is_none());
        assert_eq!(
            revived.payload.expect("manual payload")["trigger"],
            json!("manual")
        );
    }
}
