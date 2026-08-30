//! Durable cadence and at-most-once lifecycle for post-turn memory reflection.

use crate::event_log::query_error;
use crate::{Pool, open};
use rusqlite::{OptionalExtension as _, Row, params};
use std::sync::Arc;
use zuno_error::DbError;

const JOB_COLUMNS: &str = "id, session_id, source_message_id, trigger, status, owner_id, \
    lease_expires, error, time_created, time_updated, time_completed";

/// Why one delivered turn was selected for reflection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflectionTrigger {
    Periodic,
    Recovery,
    PeriodicRecovery,
}

impl ReflectionTrigger {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Periodic => "periodic",
            Self::Recovery => "recovery",
            Self::PeriodicRecovery => "periodic-recovery",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "periodic" => Ok(Self::Periodic),
            "recovery" => Ok(Self::Recovery),
            "periodic-recovery" => Ok(Self::PeriodicRecovery),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown memory reflection trigger `{value}`"
            )))),
        }
    }
}

/// Durable state of one isolated reflection request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflectionJobStatus {
    Running,
    Completed,
    Failed,
    Uncertain,
}

impl ReflectionJobStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "uncertain" => Ok(Self::Uncertain),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown memory reflection job status `{value}`"
            )))),
        }
    }

    const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// One persisted reflection execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryReflectionJob {
    pub id: String,
    pub session_id: String,
    pub source_message_id: String,
    pub trigger: ReflectionTrigger,
    pub status: ReflectionJobStatus,
    pub owner_id: String,
    pub lease_expires: i64,
    pub error: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
    pub time_completed: Option<i64>,
}

/// Inputs atomically recorded when a delivered turn reaches the reflection gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionAdmission {
    pub job_id: String,
    pub session_id: String,
    pub source_message_id: String,
    pub turn_interval: u64,
    pub recovered: bool,
    pub negative_learning: bool,
    pub owner_id: String,
    pub lease_expires: i64,
    pub time_created: i64,
}

/// Result of admitting one source message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectionAdmissionResult {
    /// This exact durable message was already counted; no work was replayed.
    AlreadyRecorded {
        ordinal: u64,
        job: Option<MemoryReflectionJob>,
    },
    /// The turn advanced the durable cadence but did not meet a safe trigger.
    NotScheduled { ordinal: u64 },
    /// The turn advanced the cadence and atomically created a running job.
    Started {
        ordinal: u64,
        job: MemoryReflectionJob,
    },
}

/// SQLite-backed reflection cadence and job lifecycle.
#[derive(Clone)]
pub struct MemoryReflectionStore {
    pool: Arc<Pool>,
}

impl MemoryReflectionStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Count one delivered message exactly once and start work only when eligible.
    pub fn admit_and_start(
        &self,
        admission: ReflectionAdmission,
    ) -> Result<ReflectionAdmissionResult, DbError> {
        validate_admission(&admission)?;
        self.pool.transaction(|transaction| {
            let existing_ordinal = transaction
                .query_row(
                    "SELECT ordinal FROM memory_reflection_delivery
                     WHERE session_id = ?1 AND source_message_id = ?2",
                    params![admission.session_id, admission.source_message_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(open::map_error)?;
            if let Some(ordinal) = existing_ordinal {
                return Ok(ReflectionAdmissionResult::AlreadyRecorded {
                    ordinal: to_ordinal(ordinal)?,
                    job: job_for_source(
                        transaction,
                        &admission.session_id,
                        &admission.source_message_id,
                    )?,
                });
            }

            let ordinal = transaction
                .query_row(
                    "SELECT COALESCE(MAX(ordinal), 0) + 1
                     FROM memory_reflection_delivery WHERE session_id = ?1",
                    [&admission.session_id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(open::map_error)?;
            transaction
                .execute(
                    "INSERT INTO memory_reflection_delivery (
                        session_id, source_message_id, ordinal, recovered,
                        negative_learning, time_created
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        admission.session_id,
                        admission.source_message_id,
                        ordinal,
                        admission.recovered,
                        admission.negative_learning,
                        admission.time_created,
                    ],
                )
                .map_err(open::map_error)?;
            let ordinal = to_ordinal(ordinal)?;
            let periodic =
                admission.turn_interval != 0 && ordinal.is_multiple_of(admission.turn_interval);
            if admission.negative_learning || (!periodic && !admission.recovered) {
                return Ok(ReflectionAdmissionResult::NotScheduled { ordinal });
            }
            let trigger = match (periodic, admission.recovered) {
                (true, true) => ReflectionTrigger::PeriodicRecovery,
                (true, false) => ReflectionTrigger::Periodic,
                (false, true) => ReflectionTrigger::Recovery,
                (false, false) => unreachable!("the ineligible branch returned above"),
            };
            transaction
                .execute(
                    "INSERT INTO memory_reflection_job (
                        id, session_id, source_message_id, trigger, status, owner_id,
                        lease_expires, error, time_created, time_updated, time_completed
                     ) VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, NULL, ?7, ?7, NULL)",
                    params![
                        admission.job_id,
                        admission.session_id,
                        admission.source_message_id,
                        trigger.as_str(),
                        admission.owner_id,
                        admission.lease_expires,
                        admission.time_created,
                    ],
                )
                .map_err(open::map_error)?;
            Ok(ReflectionAdmissionResult::Started {
                ordinal,
                job: get_required(transaction, &admission.job_id)?,
            })
        })
    }

    /// Settle one running job owned by this process.
    pub fn settle(
        &self,
        job_id: &str,
        owner_id: &str,
        status: ReflectionJobStatus,
        error: Option<&str>,
        time_completed: i64,
    ) -> Result<MemoryReflectionJob, DbError> {
        if !status.is_terminal() {
            return Err(query_error(std::io::Error::other(
                "memory reflection settlement must be terminal",
            )));
        }
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE memory_reflection_job
                     SET status = ?1, error = ?2, time_updated = ?3, time_completed = ?3
                     WHERE id = ?4 AND owner_id = ?5 AND status = 'running'",
                    params![status.as_str(), error, time_completed, job_id, owner_id],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Err(query_error(std::io::Error::other(format!(
                    "memory reflection job `{job_id}` is not running for owner `{owner_id}`"
                ))));
            }
            get_required(transaction, job_id)
        })
    }

    /// Mark expired running jobs uncertain without replaying their model request.
    pub fn reconcile_expired(&self, now: i64) -> Result<usize, DbError> {
        self.pool.transaction(|transaction| {
            transaction
                .execute(
                    "UPDATE memory_reflection_job
                     SET status = 'uncertain',
                         error = 'reflection owner disappeared without an authoritative outcome',
                         time_updated = ?1,
                         time_completed = ?1
                     WHERE status = 'running' AND lease_expires <= ?1",
                    [now],
                )
                .map_err(open::map_error)
        })
    }

    pub fn get(&self, job_id: &str) -> Result<MemoryReflectionJob, DbError> {
        let connection = self.pool.get()?;
        get_required(&connection, job_id)
    }

    pub fn delivery_count(&self, session_id: &str) -> Result<u64, DbError> {
        let connection = self.pool.get()?;
        let count = connection
            .query_row(
                "SELECT COUNT(*) FROM memory_reflection_delivery WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(open::map_error)?;
        u64::try_from(count).map_err(query_error)
    }

    pub fn list_for_session(&self, session_id: &str) -> Result<Vec<MemoryReflectionJob>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM memory_reflection_job
                 WHERE session_id = ?1 ORDER BY time_created, id"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map([session_id], decode_job)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode_stored_job))
            .collect()
    }
}

fn validate_admission(admission: &ReflectionAdmission) -> Result<(), DbError> {
    if [
        admission.job_id.as_str(),
        admission.session_id.as_str(),
        admission.source_message_id.as_str(),
        admission.owner_id.as_str(),
    ]
    .iter()
    .any(|value| value.trim().is_empty())
    {
        return Err(query_error(std::io::Error::other(
            "reflection job, session, source message, and owner ids must not be empty",
        )));
    }
    if admission.lease_expires <= admission.time_created {
        return Err(query_error(std::io::Error::other(
            "reflection lease must expire after creation",
        )));
    }
    Ok(())
}

fn to_ordinal(value: i64) -> Result<u64, DbError> {
    u64::try_from(value).map_err(query_error)
}

fn job_for_source(
    connection: &rusqlite::Connection,
    session_id: &str,
    source_message_id: &str,
) -> Result<Option<MemoryReflectionJob>, DbError> {
    connection
        .query_row(
            &format!(
                "SELECT {JOB_COLUMNS} FROM memory_reflection_job
                 WHERE session_id = ?1 AND source_message_id = ?2"
            ),
            params![session_id, source_message_id],
            decode_job,
        )
        .optional()
        .map_err(open::map_error)?
        .map(decode_stored_job)
        .transpose()
}

fn get_required(
    connection: &rusqlite::Connection,
    job_id: &str,
) -> Result<MemoryReflectionJob, DbError> {
    connection
        .query_row(
            &format!("SELECT {JOB_COLUMNS} FROM memory_reflection_job WHERE id = ?1"),
            [job_id],
            decode_job,
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: "memory_reflection_job".to_owned(),
            id: job_id.to_owned(),
        })
        .and_then(decode_stored_job)
}

type StoredJob = (
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<String>,
    i64,
    i64,
    Option<i64>,
);

fn decode_job(row: &Row<'_>) -> rusqlite::Result<StoredJob> {
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
    ))
}

fn decode_stored_job(stored: StoredJob) -> Result<MemoryReflectionJob, DbError> {
    Ok(MemoryReflectionJob {
        id: stored.0,
        session_id: stored.1,
        source_message_id: stored.2,
        trigger: ReflectionTrigger::parse(&stored.3)?,
        status: ReflectionJobStatus::parse(&stored.4)?,
        owner_id: stored.5,
        lease_expires: stored.6,
        error: stored.7,
        time_created: stored.8,
        time_updated: stored.9,
        time_completed: stored.10,
    })
}
