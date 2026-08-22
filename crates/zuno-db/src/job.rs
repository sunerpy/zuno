//! Durable background-agent jobs and atomic parent-report delivery.

use crate::event_log::{NewSessionEvent, append_in, query_error};
use crate::inbox::{NewSessionInput, SessionInput, admit_in, validate_input};
use crate::{Pool, open};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use zuno_error::DbError;

const TABLE: &str = "agent_job";
const SELECT_COLUMNS: &str = "id, parent_session_id, subject_kind, child_session_id, product_run_id, \
     product_kind, product_instance, product_tool, status, report_delivery, result, \
     error, report_input_id, created_seq, settled_seq, time_created, time_updated, \
     time_completed";

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

/// The durable subject one background job owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobSubject {
    /// A native Zuno child session.
    ChildSession {
        /// The child session that performs the turn.
        session_id: String,
    },
    /// A one-shot host-installed coding-agent product.
    ProductAgent {
        /// Unique invocation id, distinct from the durable job id.
        run_id: String,
        /// Product protocol (`codex` or `claude-code`).
        product: String,
        /// Configured product-agent instance name.
        instance: String,
        /// Static tool name that admitted the invocation.
        tool: String,
    },
}

impl JobSubject {
    /// A native child-session subject.
    #[must_use]
    pub fn child_session(session_id: impl Into<String>) -> Self {
        Self::ChildSession {
            session_id: session_id.into(),
        }
    }

    /// A product-agent subject.
    #[must_use]
    pub fn product_agent(
        run_id: impl Into<String>,
        product: impl Into<String>,
        instance: impl Into<String>,
        tool: impl Into<String>,
    ) -> Self {
        Self::ProductAgent {
            run_id: run_id.into(),
            product: product.into(),
            instance: instance.into(),
            tool: tool.into(),
        }
    }

    /// Stable JSON exposed in events, tools, and clients.
    #[must_use]
    pub fn as_json(&self) -> Value {
        match self {
            Self::ChildSession { session_id } => {
                json!({"kind":"childSession","sessionID":session_id})
            }
            Self::ProductAgent {
                run_id,
                product,
                instance,
                tool,
            } => json!({
                "kind":"productAgent",
                "runID":run_id,
                "product":product,
                "instance":instance,
                "tool":tool
            }),
        }
    }
}

/// Durable execution state for one background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// The background execution has not settled.
    Running,
    /// The execution produced a final answer.
    Completed,
    /// The execution produced an authoritative failure.
    Failed,
    /// Cancellation completed and the process/session stopped.
    Cancelled,
    /// Process or protocol loss left external side effects unknowable.
    Uncertain,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Uncertain => "uncertain",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "uncertain" => Ok(Self::Uncertain),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown job status `{value}`"
            )))),
        }
    }

    /// Whether no live executor may transition this state again.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// One new background execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentJob {
    pub id: String,
    pub parent_session_id: String,
    pub subject: JobSubject,
    pub report_delivery: ReportDelivery,
    pub time_created: i64,
}

impl NewAgentJob {
    /// Create a running job.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        parent_session_id: impl Into<String>,
        subject: JobSubject,
        report_delivery: ReportDelivery,
        time_created: i64,
    ) -> Self {
        Self {
            id: id.into(),
            parent_session_id: parent_session_id.into(),
            subject,
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
    pub subject: JobSubject,
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
    /// A successful result.
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

    /// An authoritative failure.
    #[must_use]
    pub fn failed(
        error: impl Into<String>,
        time_completed: i64,
        report: Option<NewSessionInput>,
    ) -> Self {
        Self::error(JobStatus::Failed, error, time_completed, report)
    }

    /// A confirmed cancellation.
    #[must_use]
    pub fn cancelled(
        error: impl Into<String>,
        time_completed: i64,
        report: Option<NewSessionInput>,
    ) -> Self {
        Self::error(JobStatus::Cancelled, error, time_completed, report)
    }

    /// A process/protocol loss whose side effects must not be replayed.
    #[must_use]
    pub fn uncertain(
        error: impl Into<String>,
        time_completed: i64,
        report: Option<NewSessionInput>,
    ) -> Self {
        Self::error(JobStatus::Uncertain, error, time_completed, report)
    }

    fn error(
        status: JobStatus,
        error: impl Into<String>,
        time_completed: i64,
        report: Option<NewSessionInput>,
    ) -> Self {
        Self {
            status,
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

    /// Read every job owned by one parent, oldest first.
    pub fn list_for_parent(&self, parent_session_id: &str) -> Result<Vec<AgentJob>, DbError> {
        let connection = self.pool.get()?;
        query_jobs(
            &connection,
            &format!(
                "SELECT {SELECT_COLUMNS} FROM agent_job \
                 WHERE parent_session_id = ?1 ORDER BY time_created, id"
            ),
            parent_session_id,
        )
    }

    /// Read running product invocations which cannot survive process loss.
    pub fn running_product_agents_for(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<AgentJob>, DbError> {
        let connection = self.pool.get()?;
        query_jobs(
            &connection,
            &format!(
                "SELECT {SELECT_COLUMNS} FROM agent_job \
                 WHERE parent_session_id = ?1 AND subject_kind = 'product-agent' \
                   AND status = 'running' ORDER BY time_created, id"
            ),
            parent_session_id,
        )
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
            "SELECT {} FROM agent_job AS j \
             JOIN session_input AS i ON i.id = j.report_input_id \
             WHERE j.status <> 'running' AND i.promoted_seq IS NULL{} \
             ORDER BY j.time_created, j.id",
            SELECT_COLUMNS
                .split(", ")
                .map(|column| format!("j.{column}"))
                .collect::<Vec<_>>()
                .join(", "),
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
    if job.id.trim().is_empty() || job.parent_session_id.trim().is_empty() {
        return Err(query_error(std::io::Error::other(
            "job id and parent session id must not be empty",
        )));
    }
    match &job.subject {
        JobSubject::ChildSession { session_id } => {
            if session_id.trim().is_empty() {
                return Err(query_error(std::io::Error::other(
                    "child session id must not be empty",
                )));
            }
            if job.parent_session_id == *session_id {
                return Err(query_error(std::io::Error::other(
                    "a background job's parent and child sessions must differ",
                )));
            }
        }
        JobSubject::ProductAgent {
            run_id,
            product,
            instance,
            tool,
        } if [run_id, product, instance, tool]
            .iter()
            .any(|value| value.trim().is_empty()) =>
        {
            return Err(query_error(std::io::Error::other(
                "product-agent run id, product, instance, and tool must not be empty",
            )));
        }
        JobSubject::ProductAgent { .. } => {}
    }
    Ok(())
}

fn create_in(transaction: &Transaction<'_>, job: NewAgentJob) -> Result<AgentJob, DbError> {
    let event = append_in(
        transaction,
        &job.parent_session_id,
        NewSessionEvent::new("agent.job.created", created_properties(&job))?,
    )?;
    let subject = subject_columns(&job.subject);
    transaction
        .execute(
            "INSERT INTO agent_job \
             (id, parent_session_id, subject_kind, child_session_id, product_run_id, \
              product_kind, product_instance, product_tool, status, report_delivery, \
              result, error, report_input_id, created_seq, settled_seq, time_created, \
              time_updated, time_completed) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'running', ?9, NULL, NULL, \
                     NULL, ?10, NULL, ?11, ?11, NULL)",
            params![
                job.id,
                job.parent_session_id,
                subject.kind,
                subject.child_session_id,
                subject.product_run_id,
                subject.product_kind,
                subject.product_instance,
                subject.product_tool,
                job.report_delivery.as_str(),
                event.sequence,
                job.time_created,
            ],
        )
        .map_err(open::map_error)?;
    Ok(AgentJob {
        id: job.id,
        parent_session_id: job.parent_session_id,
        subject: job.subject,
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

struct SubjectColumns<'a> {
    kind: &'static str,
    child_session_id: Option<&'a str>,
    product_run_id: Option<&'a str>,
    product_kind: Option<&'a str>,
    product_instance: Option<&'a str>,
    product_tool: Option<&'a str>,
}

fn subject_columns(subject: &JobSubject) -> SubjectColumns<'_> {
    match subject {
        JobSubject::ChildSession { session_id } => SubjectColumns {
            kind: "child-session",
            child_session_id: Some(session_id),
            product_run_id: None,
            product_kind: None,
            product_instance: None,
            product_tool: None,
        },
        JobSubject::ProductAgent {
            run_id,
            product,
            instance,
            tool,
        } => SubjectColumns {
            kind: "product-agent",
            child_session_id: None,
            product_run_id: Some(run_id),
            product_kind: Some(product),
            product_instance: Some(instance),
            product_tool: Some(tool),
        },
    }
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
        .expect("the settled job remains present until its parent is deleted");
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
        JobStatus::Failed | JobStatus::Cancelled | JobStatus::Uncertain
            if settlement.error.as_deref().is_none_or(str::is_empty) =>
        {
            Err(query_error(std::io::Error::other(
                "a failed, cancelled, or uncertain job requires an error",
            )))
        }
        JobStatus::Running => unreachable!("terminal status checked above"),
        JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled | JobStatus::Uncertain => {
            Ok(())
        }
    }
}

fn created_properties(job: &NewAgentJob) -> Map<String, Value> {
    [
        ("jobID".to_owned(), Value::String(job.id.clone())),
        (
            "parentSessionID".to_owned(),
            Value::String(job.parent_session_id.clone()),
        ),
        ("subject".to_owned(), job.subject.as_json()),
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
        ("subject".to_owned(), job.subject.as_json()),
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
    subject_kind: String,
    child_session_id: Option<String>,
    product_run_id: Option<String>,
    product_kind: Option<String>,
    product_instance: Option<String>,
    product_tool: Option<String>,
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
            &format!("SELECT {SELECT_COLUMNS} FROM agent_job WHERE id = ?1"),
            [job_id],
            decode_stored_job,
        )
        .optional()
        .map_err(open::map_error)?
        .map(decode_job)
        .transpose()
}

fn query_jobs(
    connection: &rusqlite::Connection,
    sql: &str,
    parameter: &str,
) -> Result<Vec<AgentJob>, DbError> {
    let mut statement = connection.prepare(sql).map_err(open::map_error)?;
    let rows = statement
        .query_map([parameter], decode_stored_job)
        .map_err(open::map_error)?;
    rows.map(|row| row.map_err(open::map_error).and_then(decode_job))
        .collect()
}

fn decode_stored_job(row: &Row<'_>) -> rusqlite::Result<StoredJob> {
    Ok(StoredJob {
        id: row.get(0)?,
        parent_session_id: row.get(1)?,
        subject_kind: row.get(2)?,
        child_session_id: row.get(3)?,
        product_run_id: row.get(4)?,
        product_kind: row.get(5)?,
        product_instance: row.get(6)?,
        product_tool: row.get(7)?,
        status: row.get(8)?,
        report_delivery: row.get(9)?,
        result: row.get(10)?,
        error: row.get(11)?,
        report_input_id: row.get(12)?,
        created_sequence: row.get(13)?,
        settled_sequence: row.get(14)?,
        time_created: row.get(15)?,
        time_updated: row.get(16)?,
        time_completed: row.get(17)?,
    })
}

fn decode_job(stored: StoredJob) -> Result<AgentJob, DbError> {
    let subject = match stored.subject_kind.as_str() {
        "child-session" => {
            JobSubject::child_session(required(stored.child_session_id, "child_session_id")?)
        }
        "product-agent" => JobSubject::product_agent(
            required(stored.product_run_id, "product_run_id")?,
            required(stored.product_kind, "product_kind")?,
            required(stored.product_instance, "product_instance")?,
            required(stored.product_tool, "product_tool")?,
        ),
        other => {
            return Err(query_error(std::io::Error::other(format!(
                "unknown job subject kind `{other}`"
            ))));
        }
    };
    Ok(AgentJob {
        id: stored.id,
        parent_session_id: stored.parent_session_id,
        subject,
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

fn required(value: Option<String>, column: &str) -> Result<String, DbError> {
    value.ok_or_else(|| {
        query_error(std::io::Error::other(format!(
            "job subject is missing `{column}`"
        )))
    })
}
