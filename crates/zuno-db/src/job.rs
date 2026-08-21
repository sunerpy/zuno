//! Durable background-agent jobs and atomic parent-report delivery.

use crate::event_log::{NewSessionEvent, append_in, query_error};
use crate::inbox::{NewSessionInput, SessionInput, admit_in, validate_input};
use crate::{Pool, open};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde_json::{Map, Value};
use std::sync::Arc;
use zuno_error::DbError;

const TABLE: &str = "agent_job";

/// Whether a settled background job should wake its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportDelivery {
    /// Admit the report to the parent's next step.
    NextStep,
    /// Persist the outcome without adding parent input.
    Quiet,
}

impl ReportDelivery {
    fn as_str(self) -> &'static str {
        match self {
            Self::NextStep => "next-step",
            Self::Quiet => "quiet",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "next-step" => Ok(Self::NextStep),
            "quiet" => Ok(Self::Quiet),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown report delivery `{value}`"
            )))),
        }
    }
}

/// Durable execution state for one background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// The child turn has not settled.
    Running,
    /// The child turn produced a final answer.
    Completed,
    /// The child turn failed.
    Failed,
    /// The child turn was cancelled.
    Cancelled,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown job status `{value}`"
            )))),
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// One new background execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentJob {
    pub id: String,
    pub parent_session_id: String,
    pub child_session_id: String,
    pub report_delivery: ReportDelivery,
    pub time_created: i64,
}

impl NewAgentJob {
    /// Create a running job.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        parent_session_id: impl Into<String>,
        child_session_id: impl Into<String>,
        report_delivery: ReportDelivery,
        time_created: i64,
    ) -> Self {
        Self {
            id: id.into(),
            parent_session_id: parent_session_id.into(),
            child_session_id: child_session_id.into(),
            report_delivery,
            time_created,
        }
    }
}

/// A stored background execution.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentJob {
    pub id: String,
    pub parent_session_id: String,
    pub child_session_id: String,
    pub status: JobStatus,
    pub report_delivery: ReportDelivery,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub report_input_id: Option<String>,
    pub created_sequence: i64,
    pub settled_sequence: Option<i64>,
    pub time_created: i64,
    pub time_updated: i64,
    pub time_completed: Option<i64>,
}

/// Terminal data for one running job.
#[derive(Debug, Clone, PartialEq)]
pub struct JobSettlement {
    pub status: JobStatus,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub time_completed: i64,
    pub report: Option<NewSessionInput>,
}

impl JobSettlement {
    /// A successful child result.
    #[must_use]
    pub fn completed(result: Value, time_completed: i64, report: Option<NewSessionInput>) -> Self {
        Self {
            status: JobStatus::Completed,
            result: Some(result),
            error: None,
            time_completed,
            report,
        }
    }

    /// A failed child result.
    #[must_use]
    pub fn failed(
        error: impl Into<String>,
        time_completed: i64,
        report: Option<NewSessionInput>,
    ) -> Self {
        Self {
            status: JobStatus::Failed,
            result: None,
            error: Some(error.into()),
            time_completed,
            report,
        }
    }
}

/// The committed terminal state and optional parent input.
#[derive(Debug, Clone, PartialEq)]
pub struct SettledJob {
    pub job: AgentJob,
    pub report: Option<SessionInput>,
}

/// Durable background-job access over an initialized pool.
#[derive(Clone)]
pub struct AgentJobStore {
    pool: Arc<Pool>,
}

impl AgentJobStore {
    /// Open the store.
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Insert one running job.
    pub fn create(&self, job: NewAgentJob) -> Result<AgentJob, DbError> {
        validate_new_job(&job)?;
        self.pool
            .transaction(|transaction| create_in(transaction, job))
    }

    /// Read one job by id.
    pub fn get(&self, job_id: &str) -> Result<AgentJob, DbError> {
        let connection = self.pool.get()?;
        get_in(&connection, job_id)?.ok_or_else(|| DbError::NotFound {
            table: TABLE.to_owned(),
            id: job_id.to_owned(),
        })
    }

    /// Settle one running job and atomically admit its parent report.
    pub fn settle(&self, job_id: &str, settlement: JobSettlement) -> Result<SettledJob, DbError> {
        self.pool
            .transaction(|transaction| settle_in(transaction, job_id, settlement))
    }

    /// Read terminal jobs whose promised report is still pending.
    pub fn pending_reports(&self) -> Result<Vec<AgentJob>, DbError> {
        self.query_pending_reports(None)
    }

    /// Read one parent's terminal jobs whose promised report is still pending.
    pub fn pending_reports_for(&self, parent_session_id: &str) -> Result<Vec<AgentJob>, DbError> {
        self.query_pending_reports(Some(parent_session_id))
    }

    fn query_pending_reports(
        &self,
        parent_session_id: Option<&str>,
    ) -> Result<Vec<AgentJob>, DbError> {
        let connection = self.pool.get()?;
        let sql = format!(
            "SELECT j.id, j.parent_session_id, j.child_session_id, j.status, \
                    j.report_delivery, j.result, j.error, j.report_input_id, \
                    j.created_seq, j.settled_seq, j.time_created, j.time_updated, \
                    j.time_completed \
             FROM agent_job AS j \
             JOIN session_input AS i ON i.id = j.report_input_id \
             WHERE j.status <> 'running' AND i.promoted_seq IS NULL{} \
             ORDER BY j.time_created, j.id",
            if parent_session_id.is_some() {
                " AND j.parent_session_id = ?1"
            } else {
                ""
            }
        );
        let mut statement = connection.prepare(&sql).map_err(open::map_error)?;
        let rows = match parent_session_id {
            Some(parent) => statement
                .query_map([parent], decode_stored_job)
                .map_err(open::map_error)?,
            None => statement
                .query_map([], decode_stored_job)
                .map_err(open::map_error)?,
        };
        rows.map(|row| row.map_err(open::map_error).and_then(decode_job))
            .collect()
    }
}

fn validate_new_job(job: &NewAgentJob) -> Result<(), DbError> {
    if job.id.trim().is_empty()
        || job.parent_session_id.trim().is_empty()
        || job.child_session_id.trim().is_empty()
    {
        return Err(query_error(std::io::Error::other(
            "job id, parent session id, and child session id must not be empty",
        )));
    }
    if job.parent_session_id == job.child_session_id {
        return Err(query_error(std::io::Error::other(
            "a background job's parent and child sessions must differ",
        )));
    }
    Ok(())
}

fn create_in(transaction: &Transaction<'_>, job: NewAgentJob) -> Result<AgentJob, DbError> {
    let event = append_in(
        transaction,
        &job.parent_session_id,
        NewSessionEvent::new("agent.job.created", created_properties(&job))?,
    )?;
    transaction
        .execute(
            "INSERT INTO agent_job \
             (id, parent_session_id, child_session_id, status, report_delivery, \
              result, error, report_input_id, created_seq, settled_seq, \
              time_created, time_updated, time_completed) \
             VALUES (?1, ?2, ?3, 'running', ?4, NULL, NULL, NULL, ?5, NULL, ?6, ?6, NULL)",
            params![
                job.id,
                job.parent_session_id,
                job.child_session_id,
                job.report_delivery.as_str(),
                event.sequence,
                job.time_created,
            ],
        )
        .map_err(open::map_error)?;
    Ok(AgentJob {
        id: job.id,
        parent_session_id: job.parent_session_id,
        child_session_id: job.child_session_id,
        status: JobStatus::Running,
        report_delivery: job.report_delivery,
        result: None,
        error: None,
        report_input_id: None,
        created_sequence: event.sequence,
        settled_sequence: None,
        time_created: job.time_created,
        time_updated: job.time_created,
        time_completed: None,
    })
}

fn settle_in(
    transaction: &Transaction<'_>,
    job_id: &str,
    settlement: JobSettlement,
) -> Result<SettledJob, DbError> {
    let running = get_in(transaction, job_id)?.ok_or_else(|| DbError::NotFound {
        table: TABLE.to_owned(),
        id: job_id.to_owned(),
    })?;
    if running.status != JobStatus::Running {
        return Err(query_error(std::io::Error::other(format!(
            "job `{job_id}` is already {}",
            running.status.as_str()
        ))));
    }
    validate_settlement(&running, &settlement)?;

    let settled_event = append_in(
        transaction,
        &running.parent_session_id,
        NewSessionEvent::new(
            "agent.job.settled",
            settled_properties(&running, &settlement),
        )?,
    )?;
    let report = settlement
        .report
        .map(|report| admit_in(transaction, report))
        .transpose()?;
    let result = settlement
        .result
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(query_error)?;
    let report_input_id = report.as_ref().map(|input| input.id.as_str());
    let changed = transaction
        .execute(
            "UPDATE agent_job \
             SET status = ?1, result = ?2, error = ?3, report_input_id = ?4, \
                 settled_seq = ?5, time_updated = ?6, time_completed = ?6 \
             WHERE id = ?7 AND status = 'running'",
            params![
                settlement.status.as_str(),
                result,
                settlement.error,
                report_input_id,
                settled_event.sequence,
                settlement.time_completed,
                job_id,
            ],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        return Err(query_error(std::io::Error::other(format!(
            "job `{job_id}` changed while settling"
        ))));
    }
    let job = get_in(transaction, job_id)?
        .expect("the settled job remains present until its parent or child is deleted");
    Ok(SettledJob { job, report })
}

fn validate_settlement(job: &AgentJob, settlement: &JobSettlement) -> Result<(), DbError> {
    if !settlement.status.is_terminal() {
        return Err(query_error(std::io::Error::other(
            "a job settlement must be terminal",
        )));
    }
    match (job.report_delivery, settlement.report.as_ref()) {
        (ReportDelivery::NextStep, Some(report)) => {
            validate_input(report)?;
            if report.session_id != job.parent_session_id {
                return Err(query_error(std::io::Error::other(
                    "a job report must target its parent session",
                )));
            }
            if report.delivery != crate::inbox::InputDelivery::NextStep {
                return Err(query_error(std::io::Error::other(
                    "a job report must use next-step delivery",
                )));
            }
        }
        (ReportDelivery::NextStep, None) => {
            return Err(query_error(std::io::Error::other(
                "next-step report delivery requires a parent input",
            )));
        }
        (ReportDelivery::Quiet, Some(_)) => {
            return Err(query_error(std::io::Error::other(
                "quiet report delivery must not add parent input",
            )));
        }
        (ReportDelivery::Quiet, None) => {}
    }
    match settlement.status {
        JobStatus::Completed if settlement.result.is_none() || settlement.error.is_some() => {
            Err(query_error(std::io::Error::other(
                "a completed job requires a result and no error",
            )))
        }
        JobStatus::Failed | JobStatus::Cancelled
            if settlement.error.as_deref().is_none_or(str::is_empty) =>
        {
            Err(query_error(std::io::Error::other(
                "a failed or cancelled job requires an error",
            )))
        }
        JobStatus::Running => unreachable!("terminal status checked above"),
        JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled => Ok(()),
    }
}

fn created_properties(job: &NewAgentJob) -> Map<String, Value> {
    [
        ("jobID".to_owned(), Value::String(job.id.clone())),
        (
            "parentSessionID".to_owned(),
            Value::String(job.parent_session_id.clone()),
        ),
        (
            "childSessionID".to_owned(),
            Value::String(job.child_session_id.clone()),
        ),
        (
            "reportDelivery".to_owned(),
            Value::String(job.report_delivery.as_str().to_owned()),
        ),
        (
            "timeCreated".to_owned(),
            Value::Number(job.time_created.into()),
        ),
    ]
    .into_iter()
    .collect()
}

fn settled_properties(job: &AgentJob, settlement: &JobSettlement) -> Map<String, Value> {
    let mut properties = [
        ("jobID".to_owned(), Value::String(job.id.clone())),
        (
            "parentSessionID".to_owned(),
            Value::String(job.parent_session_id.clone()),
        ),
        (
            "childSessionID".to_owned(),
            Value::String(job.child_session_id.clone()),
        ),
        (
            "status".to_owned(),
            Value::String(settlement.status.as_str().to_owned()),
        ),
        (
            "timeCompleted".to_owned(),
            Value::Number(settlement.time_completed.into()),
        ),
    ]
    .into_iter()
    .collect::<Map<_, _>>();
    if let Some(result) = &settlement.result {
        properties.insert("result".to_owned(), result.clone());
    }
    if let Some(error) = &settlement.error {
        properties.insert("error".to_owned(), Value::String(error.clone()));
    }
    properties
}

struct StoredJob {
    id: String,
    parent_session_id: String,
    child_session_id: String,
    status: String,
    report_delivery: String,
    result: Option<String>,
    error: Option<String>,
    report_input_id: Option<String>,
    created_sequence: i64,
    settled_sequence: Option<i64>,
    time_created: i64,
    time_updated: i64,
    time_completed: Option<i64>,
}

fn get_in(connection: &rusqlite::Connection, job_id: &str) -> Result<Option<AgentJob>, DbError> {
    connection
        .query_row(
            "SELECT id, parent_session_id, child_session_id, status, report_delivery, \
                    result, error, report_input_id, created_seq, settled_seq, \
                    time_created, time_updated, time_completed \
             FROM agent_job WHERE id = ?1",
            [job_id],
            decode_stored_job,
        )
        .optional()
        .map_err(open::map_error)?
        .map(decode_job)
        .transpose()
}

fn decode_stored_job(row: &Row<'_>) -> rusqlite::Result<StoredJob> {
    Ok(StoredJob {
        id: row.get(0)?,
        parent_session_id: row.get(1)?,
        child_session_id: row.get(2)?,
        status: row.get(3)?,
        report_delivery: row.get(4)?,
        result: row.get(5)?,
        error: row.get(6)?,
        report_input_id: row.get(7)?,
        created_sequence: row.get(8)?,
        settled_sequence: row.get(9)?,
        time_created: row.get(10)?,
        time_updated: row.get(11)?,
        time_completed: row.get(12)?,
    })
}

fn decode_job(stored: StoredJob) -> Result<AgentJob, DbError> {
    Ok(AgentJob {
        id: stored.id,
        parent_session_id: stored.parent_session_id,
        child_session_id: stored.child_session_id,
        status: JobStatus::parse(&stored.status)?,
        report_delivery: ReportDelivery::parse(&stored.report_delivery)?,
        result: stored
            .result
            .map(|result| serde_json::from_str(&result).map_err(query_error))
            .transpose()?,
        error: stored.error,
        report_input_id: stored.report_input_id,
        created_sequence: stored.created_sequence,
        settled_sequence: stored.settled_sequence,
        time_created: stored.time_created,
        time_updated: stored.time_updated,
        time_completed: stored.time_completed,
    })
}
