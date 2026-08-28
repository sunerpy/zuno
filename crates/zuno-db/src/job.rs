//! Durable background-agent jobs and atomic parent-report delivery.

use crate::event_log::{NewSessionEvent, append_in, query_error};
use crate::inbox::{
    NewSessionInput, SessionInput, admit_in, recover_promoted_in, supersede_in, validate_input,
};
use crate::{Pool, open, session};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_orchestration::AttemptSnapshot;

const TABLE: &str = "agent_job";
const SELECT_COLUMNS: &str = "id, parent_session_id, logical_key, subject_kind, subject_payload, \
     orchestration_snapshot, evidence_start_rowid, status, report_delivery, result, error, \
     report_input_id, created_seq, settled_seq, time_created, time_updated, time_completed";

/// Whether a settled background job should wake its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportDelivery {
    /// Admit the report to the parent's next step.
    NextStep,
    /// Persist the outcome without adding parent input.
    Quiet,
}

impl ReportDelivery {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
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
    /// A configured multi-agent workflow run.
    Workflow {
        /// Unique workflow invocation id, distinct from the durable job id.
        run_id: String,
        /// Configured workflow template name.
        workflow: String,
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

    /// A workflow-run subject.
    #[must_use]
    pub fn workflow(run_id: impl Into<String>, workflow: impl Into<String>) -> Self {
        Self::Workflow {
            run_id: run_id.into(),
            workflow: workflow.into(),
        }
    }

    /// Stable durable discriminator.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::ChildSession { .. } => "child-session",
            Self::ProductAgent { .. } => "product-agent",
            Self::Workflow { .. } => "workflow",
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
            Self::Workflow { run_id, workflow } => json!({
                "kind":"workflow",
                "runID":run_id,
                "workflow":workflow
            }),
        }
    }
}

/// Durable execution state for one background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// The job is durably admitted but is waiting for execution capacity.
    Queued,
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
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Uncertain => "uncertain",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "queued" => Ok(Self::Queued),
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
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// One new background execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentJob {
    pub id: String,
    pub parent_session_id: String,
    pub logical_key: String,
    pub subject: JobSubject,
    pub orchestration_snapshot: Option<AttemptSnapshot>,
    pub evidence_start_rowid: i64,
    pub report_delivery: ReportDelivery,
    pub time_created: i64,
    initial_status: JobStatus,
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
        let id = id.into();
        Self {
            logical_key: id.clone(),
            id,
            parent_session_id: parent_session_id.into(),
            subject,
            orchestration_snapshot: None,
            evidence_start_rowid: 0,
            report_delivery,
            time_created,
            initial_status: JobStatus::Running,
        }
    }

    /// Persist this job as waiting for shared execution capacity.
    #[must_use]
    pub fn queued(mut self) -> Self {
        self.initial_status = JobStatus::Queued;
        self
    }

    /// Persist the immutable Attempt that admitted this background operation.
    #[must_use]
    pub fn with_orchestration_snapshot(mut self, snapshot: Option<AttemptSnapshot>) -> Self {
        self.orchestration_snapshot = snapshot;
        self
    }

    /// Pin the first child-session part that belongs to this execution.
    #[must_use]
    pub fn with_evidence_start_rowid(mut self, rowid: i64) -> Self {
        self.evidence_start_rowid = rowid;
        self
    }

    /// Use a semantic identity shared by retries or fresh child-session allocations.
    #[must_use]
    pub fn with_logical_key(mut self, logical_key: impl Into<String>) -> Self {
        self.logical_key = logical_key.into();
        self
    }
}

/// A stored background execution.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentJob {
    pub id: String,
    pub parent_session_id: String,
    pub logical_key: String,
    pub subject: JobSubject,
    pub orchestration_snapshot: Option<AttemptSnapshot>,
    pub evidence_start_rowid: i64,
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

/// Authoritative resolution of an uncertain external outcome.
///
/// The authority and evidence are durable audit data, not model-facing prose used
/// to infer state. Reconciliation never replays the original operation and may
/// happen exactly once.
#[derive(Debug, Clone, PartialEq)]
pub struct JobReconciliation {
    pub status: JobStatus,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub authority: String,
    pub evidence: String,
    pub time_completed: i64,
    pub report: Option<NewSessionInput>,
}

impl JobReconciliation {
    /// Confirm that the uncertain operation completed.
    #[must_use]
    pub fn completed(
        result: Value,
        authority: impl Into<String>,
        evidence: impl Into<String>,
        time_completed: i64,
        report: Option<NewSessionInput>,
    ) -> Self {
        Self {
            status: JobStatus::Completed,
            result: Some(result),
            error: None,
            authority: authority.into(),
            evidence: evidence.into(),
            time_completed,
            report,
        }
    }

    /// Confirm that the uncertain operation failed.
    #[must_use]
    pub fn failed(
        error: impl Into<String>,
        authority: impl Into<String>,
        evidence: impl Into<String>,
        time_completed: i64,
        report: Option<NewSessionInput>,
    ) -> Self {
        Self::error(
            JobStatus::Failed,
            error,
            authority,
            evidence,
            time_completed,
            report,
        )
    }

    /// Confirm that the uncertain operation was cancelled.
    #[must_use]
    pub fn cancelled(
        error: impl Into<String>,
        authority: impl Into<String>,
        evidence: impl Into<String>,
        time_completed: i64,
        report: Option<NewSessionInput>,
    ) -> Self {
        Self::error(
            JobStatus::Cancelled,
            error,
            authority,
            evidence,
            time_completed,
            report,
        )
    }

    /// Attach structured terminal evidence to a failed or cancelled resolution.
    #[must_use]
    pub fn with_result(mut self, result: Value) -> Self {
        self.result = Some(result);
        self
    }

    fn error(
        status: JobStatus,
        error: impl Into<String>,
        authority: impl Into<String>,
        evidence: impl Into<String>,
        time_completed: i64,
        report: Option<NewSessionInput>,
    ) -> Self {
        Self {
            status,
            result: None,
            error: Some(error.into()),
            authority: authority.into(),
            evidence: evidence.into(),
            time_completed,
            report,
        }
    }
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

    /// Attach structured output without changing this settlement's status or error.
    #[must_use]
    pub fn with_result(mut self, result: Value) -> Self {
        self.result = Some(result);
        self
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

    /// Insert one admitted job in its requested initial state.
    pub fn create(&self, job: NewAgentJob) -> Result<AgentJob, DbError> {
        validate_new_job(&job)?;
        self.pool
            .transaction(|transaction| create_in(transaction, job))
    }

    /// Insert one child-session job only when that logical child is reconciled.
    ///
    /// The guard and insert share one transaction. A queued, running, uncertain, or
    /// terminal child with an unconsumed next-step report therefore cannot be raced
    /// into a duplicate dispatch.
    pub fn create_child_if_reconciled(&self, job: NewAgentJob) -> Result<AgentJob, DbError> {
        validate_new_job(&job)?;
        let JobSubject::ChildSession { session_id } = &job.subject else {
            return Err(query_error(std::io::Error::other(
                "create_child_if_reconciled requires a child-session subject",
            )));
        };
        let child_session_id = session_id.clone();
        self.pool.transaction(|transaction| {
            ensure_child_reconciled_in(transaction, &job, &child_session_id)?;
            create_in(transaction, job)
        })
    }

    /// Atomically create a fresh child session and admit its first logical job.
    ///
    /// A duplicate logical task rolls the speculative session back with the job
    /// admission, so rejected dispatches never leave orphan child sessions.
    pub fn create_child_session_if_reconciled(
        &self,
        child: session::SessionCreate,
        job: NewAgentJob,
    ) -> Result<AgentJob, DbError> {
        validate_new_job(&job)?;
        let JobSubject::ChildSession { session_id } = &job.subject else {
            return Err(query_error(std::io::Error::other(
                "create_child_session_if_reconciled requires a child-session subject",
            )));
        };
        if child.id != *session_id {
            return Err(query_error(std::io::Error::other(format!(
                "child session input `{}` does not match job subject `{session_id}`",
                child.id
            ))));
        }
        if child.parent_id.as_deref() != Some(job.parent_session_id.as_str()) {
            return Err(query_error(std::io::Error::other(format!(
                "child session `{session_id}` must name job parent `{}`",
                job.parent_session_id
            ))));
        }
        let child_session_id = session_id.clone();
        self.pool.transaction(|transaction| {
            ensure_child_reconciled_in(transaction, &job, &child_session_id)?;
            session::create(transaction, &child)?;
            create_in(transaction, job)
        })
    }

    /// Read the job that currently prevents another dispatch to one child session.
    pub fn blocking_child_job(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
    ) -> Result<Option<AgentJob>, DbError> {
        let connection = self.pool.get()?;
        blocking_child_job_in(&connection, parent_session_id, child_session_id)
    }

    /// Atomically mark one queued job as running after capacity admission.
    pub fn start(&self, job_id: &str, time_started: i64) -> Result<AgentJob, DbError> {
        self.pool
            .transaction(|transaction| start_in(transaction, job_id, time_started))
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
        list_for_parent_in(&connection, parent_session_id)
    }

    /// Read running workflows which cannot survive process loss.
    pub fn running_workflows_for(&self, parent_session_id: &str) -> Result<Vec<AgentJob>, DbError> {
        let connection = self.pool.get()?;
        query_jobs(
            &connection,
            &format!(
                "SELECT {SELECT_COLUMNS} FROM agent_job \
                 WHERE parent_session_id = ?1 AND subject_kind = 'workflow' \
                   AND status = 'running' ORDER BY time_created, id"
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

    /// Read queued or running native child sessions which lost their process owner.
    pub fn active_child_sessions_for(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<AgentJob>, DbError> {
        self.active_subjects_for(parent_session_id, "child-session")
    }

    /// Read queued or running product invocations which lost their process owner.
    pub fn active_product_agents_for(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<AgentJob>, DbError> {
        self.active_subjects_for(parent_session_id, "product-agent")
    }

    fn active_subjects_for(
        &self,
        parent_session_id: &str,
        subject_kind: &str,
    ) -> Result<Vec<AgentJob>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {SELECT_COLUMNS} FROM agent_job \
                 WHERE parent_session_id = ?1 AND subject_kind = ?2 \
                   AND status IN ('queued', 'running') ORDER BY time_created, id"
            ))
            .map_err(open::map_error)?;
        let rows = statement
            .query_map(params![parent_session_id, subject_kind], decode_stored_job)
            .map_err(open::map_error)?;
        rows.map(|row| row.map_err(open::map_error).and_then(decode_job))
            .collect()
    }

    /// Settle one active job and atomically admit its parent report.
    pub fn settle(&self, job_id: &str, settlement: JobSettlement) -> Result<SettledJob, DbError> {
        self.pool
            .transaction(|transaction| settle_in(transaction, job_id, settlement))
    }

    /// Replace an uncertain outcome with authoritative external-state evidence.
    pub fn reconcile_uncertain(
        &self,
        job_id: &str,
        reconciliation: JobReconciliation,
    ) -> Result<SettledJob, DbError> {
        self.pool
            .transaction(|transaction| reconcile_uncertain_in(transaction, job_id, reconciliation))
    }

    /// Recover terminal jobs whose promised report has not been consumed.
    ///
    /// A report that was promoted before process loss is returned to its original
    /// delivery lane in the same transaction. The existing input row is reused,
    /// so recovery is idempotent and does not admit or wake a duplicate report.
    pub fn pending_reports(&self) -> Result<Vec<AgentJob>, DbError> {
        self.query_pending_reports(None)
    }

    /// Recover one parent's terminal jobs whose promised report is not consumed.
    pub fn pending_reports_for(&self, parent_session_id: &str) -> Result<Vec<AgentJob>, DbError> {
        self.query_pending_reports(Some(parent_session_id))
    }

    fn query_pending_reports(
        &self,
        parent_session_id: Option<&str>,
    ) -> Result<Vec<AgentJob>, DbError> {
        self.pool.transaction(|transaction| {
            let sql = format!(
                "SELECT {} FROM agent_job AS j \
                 JOIN session_input AS i \
                   ON i.id = j.report_input_id AND i.session_id = j.parent_session_id \
                 WHERE j.report_delivery = 'next-step' \
                   AND j.status IN ('completed', 'failed', 'cancelled', 'uncertain') \
                   AND i.state IN ('queued', 'steering', 'promoted'){} \
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
            let jobs = {
                let mut statement = transaction.prepare(&sql).map_err(open::map_error)?;
                let rows = match parent_session_id {
                    Some(parent) => statement
                        .query_map([parent], decode_stored_job)
                        .map_err(open::map_error)?,
                    None => statement
                        .query_map([], decode_stored_job)
                        .map_err(open::map_error)?,
                };
                rows.map(|row| row.map_err(open::map_error).and_then(decode_job))
                    .collect::<Result<Vec<_>, _>>()?
            };
            for job in &jobs {
                let input_id = job.report_input_id.as_deref().ok_or_else(|| {
                    query_error(std::io::Error::other(format!(
                        "job `{}` joined a report row without retaining its input id",
                        job.id
                    )))
                })?;
                recover_promoted_in(transaction, &job.parent_session_id, input_id)?.ok_or_else(
                    || {
                        query_error(std::io::Error::other(format!(
                            "report input `{input_id}` changed while recovering job `{}`",
                            job.id
                        )))
                    },
                )?;
            }
            Ok(jobs)
        })
    }
}

/// Read every job owned by one parent through a caller-owned SQLite snapshot.
pub fn list_for_parent_in(
    connection: &rusqlite::Connection,
    parent_session_id: &str,
) -> Result<Vec<AgentJob>, DbError> {
    query_jobs(
        connection,
        &format!(
            "SELECT {SELECT_COLUMNS} FROM agent_job \
             WHERE parent_session_id = ?1 ORDER BY time_created, id"
        ),
        parent_session_id,
    )
}

fn validate_new_job(job: &NewAgentJob) -> Result<(), DbError> {
    if job.id.trim().is_empty()
        || job.parent_session_id.trim().is_empty()
        || job.logical_key.trim().is_empty()
    {
        return Err(query_error(std::io::Error::other(
            "job id, parent session id, and logical key must not be empty",
        )));
    }
    if job.evidence_start_rowid < 0 {
        return Err(query_error(std::io::Error::other(
            "job evidence cursor must not be negative",
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
        JobSubject::Workflow { run_id, workflow }
            if run_id.trim().is_empty() || workflow.trim().is_empty() =>
        {
            return Err(query_error(std::io::Error::other(
                "workflow run id and workflow name must not be empty",
            )));
        }
        JobSubject::Workflow { .. } => {}
    }
    Ok(())
}

fn create_in(transaction: &Transaction<'_>, job: NewAgentJob) -> Result<AgentJob, DbError> {
    let event = append_in(
        transaction,
        &job.parent_session_id,
        NewSessionEvent::new("agent.job.created", created_properties(&job))?,
    )?;
    let subject_payload = serde_json::to_string(&job.subject.as_json()).map_err(query_error)?;
    let orchestration_snapshot = job
        .orchestration_snapshot
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(query_error)?;
    transaction
        .execute(
            "INSERT INTO agent_job \
             (id, parent_session_id, logical_key, subject_kind, subject_payload, \
              orchestration_snapshot, evidence_start_rowid, status, report_delivery, result, \
              error, report_input_id, created_seq, settled_seq, time_created, time_updated, \
              time_completed) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, NULL, ?10, NULL, ?11, \
                     ?11, NULL)",
            params![
                job.id,
                job.parent_session_id,
                job.logical_key,
                job.subject.kind(),
                subject_payload,
                orchestration_snapshot,
                job.evidence_start_rowid,
                job.initial_status.as_str(),
                job.report_delivery.as_str(),
                event.sequence,
                job.time_created,
            ],
        )
        .map_err(open::map_error)?;
    Ok(AgentJob {
        id: job.id,
        parent_session_id: job.parent_session_id,
        logical_key: job.logical_key,
        subject: job.subject,
        orchestration_snapshot: job.orchestration_snapshot,
        evidence_start_rowid: job.evidence_start_rowid,
        status: job.initial_status,
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

fn ensure_child_reconciled_in(
    transaction: &Transaction<'_>,
    job: &NewAgentJob,
    child_session_id: &str,
) -> Result<(), DbError> {
    if let Some(blocking) =
        blocking_child_job_in(transaction, &job.parent_session_id, child_session_id)?
    {
        return Err(query_error(std::io::Error::other(format!(
            "child session `{child_session_id}` already has unreconciled job `{}` in `{}` state",
            blocking.id,
            blocking.status.as_str()
        ))));
    }
    if let Some(blocking) = blocking_logical_job_in(
        transaction,
        &job.parent_session_id,
        &job.logical_key,
        job.orchestration_snapshot.as_ref(),
    )? {
        return Err(query_error(std::io::Error::other(format!(
            "logical task `{}` is already covered by job `{}` in `{}` state",
            job.logical_key,
            blocking.id,
            blocking.status.as_str()
        ))));
    }
    Ok(())
}

fn blocking_child_job_in(
    connection: &Connection,
    parent_session_id: &str,
    child_session_id: &str,
) -> Result<Option<AgentJob>, DbError> {
    let stored = connection
        .query_row(
            &format!(
                "SELECT {SELECT_COLUMNS} FROM agent_job AS j \
                 WHERE j.parent_session_id = ?1 \
                   AND j.subject_kind = 'child-session' \
                   AND json_extract(j.subject_payload, '$.sessionID') = ?2 \
                   AND ( \
                     j.status IN ('queued', 'running', 'uncertain') \
                     OR ( \
                       j.report_delivery = 'next-step' \
                       AND j.status IN ('completed', 'failed', 'cancelled') \
                       AND EXISTS ( \
                         SELECT 1 FROM session_input AS i \
                         WHERE i.id = j.report_input_id \
                           AND i.session_id = j.parent_session_id \
                           AND i.state IN ('queued', 'steering', 'promoted') \
                       ) \
                     ) \
                   ) \
                 ORDER BY j.time_created DESC, j.id DESC LIMIT 1"
            ),
            params![parent_session_id, child_session_id],
            decode_stored_job,
        )
        .optional()
        .map_err(open::map_error)?;
    stored.map(decode_job).transpose()
}

fn blocking_logical_job_in(
    connection: &Connection,
    parent_session_id: &str,
    logical_key: &str,
    current_attempt: Option<&AttemptSnapshot>,
) -> Result<Option<AgentJob>, DbError> {
    let attempt_turn_id = current_attempt.map(|attempt| attempt.turn_id.as_str());
    let attempt_step = current_attempt.map(|attempt| i64::from(attempt.step));
    let stored = connection
        .query_row(
            &format!(
                "SELECT {SELECT_COLUMNS} FROM agent_job AS j \
                 WHERE j.parent_session_id = ?1 \
                   AND j.logical_key = ?2 \
                   AND ( \
                     j.status IN ('queued', 'running', 'uncertain') \
                     OR ( \
                       j.report_delivery = 'next-step' \
                       AND j.status IN ('completed', 'failed', 'cancelled') \
                       AND EXISTS ( \
                         SELECT 1 FROM session_input AS i \
                         WHERE i.id = j.report_input_id \
                           AND i.session_id = j.parent_session_id \
                           AND i.state IN ('queued', 'steering', 'promoted') \
                       ) \
                     ) \
                     OR ( \
                       ?3 IS NOT NULL \
                       AND ?4 IS NOT NULL \
                       AND j.status IN ('completed', 'failed', 'cancelled') \
                       AND json_extract(j.orchestration_snapshot, '$.turnId') = ?3 \
                       AND json_extract(j.orchestration_snapshot, '$.step') = ?4 \
                     ) \
                   ) \
                 ORDER BY j.time_created DESC, j.id DESC LIMIT 1"
            ),
            params![
                parent_session_id,
                logical_key,
                attempt_turn_id,
                attempt_step
            ],
            decode_stored_job,
        )
        .optional()
        .map_err(open::map_error)?;
    stored.map(decode_job).transpose()
}

fn start_in(
    transaction: &Transaction<'_>,
    job_id: &str,
    time_started: i64,
) -> Result<AgentJob, DbError> {
    let queued = get_in(transaction, job_id)?.ok_or_else(|| DbError::NotFound {
        table: TABLE.to_owned(),
        id: job_id.to_owned(),
    })?;
    if queued.status != JobStatus::Queued {
        return Err(query_error(std::io::Error::other(format!(
            "job `{job_id}` is already {}",
            queued.status.as_str()
        ))));
    }
    append_in(
        transaction,
        &queued.parent_session_id,
        NewSessionEvent::new(
            "agent.job.started",
            [
                ("jobID".to_owned(), Value::String(queued.id.clone())),
                (
                    "parentSessionID".to_owned(),
                    Value::String(queued.parent_session_id.clone()),
                ),
                ("subject".to_owned(), queued.subject.as_json()),
                ("timeStarted".to_owned(), Value::Number(time_started.into())),
            ]
            .into_iter()
            .collect(),
        )?,
    )?;
    let changed = transaction
        .execute(
            "UPDATE agent_job SET status = 'running', time_updated = ?1 \
             WHERE id = ?2 AND status = 'queued'",
            params![time_started, job_id],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        return Err(query_error(std::io::Error::other(format!(
            "job `{job_id}` changed while starting"
        ))));
    }
    get_in(transaction, job_id)?.ok_or_else(|| DbError::NotFound {
        table: TABLE.to_owned(),
        id: job_id.to_owned(),
    })
}

fn settle_in(
    transaction: &Transaction<'_>,
    job_id: &str,
    settlement: JobSettlement,
) -> Result<SettledJob, DbError> {
    let active = get_in(transaction, job_id)?.ok_or_else(|| DbError::NotFound {
        table: TABLE.to_owned(),
        id: job_id.to_owned(),
    })?;
    if active.status.is_terminal() {
        return Err(query_error(std::io::Error::other(format!(
            "job `{job_id}` is already {}",
            active.status.as_str()
        ))));
    }
    if active.status == JobStatus::Queued && settlement.status != JobStatus::Cancelled {
        return Err(query_error(std::io::Error::other(format!(
            "queued job `{job_id}` may only be cancelled before it starts"
        ))));
    }
    validate_settlement(&active, &settlement)?;

    let settled_event = append_in(
        transaction,
        &active.parent_session_id,
        NewSessionEvent::new(
            "agent.job.settled",
            settled_properties(&active, &settlement),
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
             WHERE id = ?7 AND status IN ('queued', 'running')",
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

fn reconcile_uncertain_in(
    transaction: &Transaction<'_>,
    job_id: &str,
    reconciliation: JobReconciliation,
) -> Result<SettledJob, DbError> {
    let uncertain = get_in(transaction, job_id)?.ok_or_else(|| DbError::NotFound {
        table: TABLE.to_owned(),
        id: job_id.to_owned(),
    })?;
    if uncertain.status != JobStatus::Uncertain {
        return Err(query_error(std::io::Error::other(format!(
            "job `{job_id}` is {}, not uncertain",
            uncertain.status.as_str()
        ))));
    }
    validate_reconciliation(&uncertain, &reconciliation)?;

    if let Some(input_id) = uncertain.report_input_id.as_deref() {
        let _superseded = supersede_in(transaction, &uncertain.parent_session_id, input_id)?;
    }
    let reconciled_event = append_in(
        transaction,
        &uncertain.parent_session_id,
        NewSessionEvent::new(
            "agent.job.reconciled",
            reconciliation_properties(&uncertain, &reconciliation),
        )?,
    )?;
    let report = reconciliation
        .report
        .map(|report| admit_in(transaction, report))
        .transpose()?;
    let result = reconciliation
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
             WHERE id = ?7 AND status = 'uncertain'",
            params![
                reconciliation.status.as_str(),
                result,
                reconciliation.error,
                report_input_id,
                reconciled_event.sequence,
                reconciliation.time_completed,
                job_id,
            ],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        return Err(query_error(std::io::Error::other(format!(
            "job `{job_id}` changed while reconciling"
        ))));
    }
    let job = get_in(transaction, job_id)?
        .expect("the reconciled job remains present until its parent is deleted");
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
            if report.delivery != crate::inbox::InputDelivery::Queue {
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
        JobStatus::Queued | JobStatus::Running => unreachable!("terminal status checked above"),
        JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled | JobStatus::Uncertain => {
            Ok(())
        }
    }
}

fn validate_reconciliation(
    job: &AgentJob,
    reconciliation: &JobReconciliation,
) -> Result<(), DbError> {
    if reconciliation.authority.trim().is_empty() {
        return Err(query_error(std::io::Error::other(
            "job reconciliation requires a non-empty authority",
        )));
    }
    if reconciliation.evidence.trim().is_empty() {
        return Err(query_error(std::io::Error::other(
            "job reconciliation requires non-empty evidence",
        )));
    }
    match (job.report_delivery, reconciliation.report.as_ref()) {
        (ReportDelivery::NextStep, Some(report)) => {
            validate_input(report)?;
            if report.session_id != job.parent_session_id {
                return Err(query_error(std::io::Error::other(
                    "a reconciled job report must target its parent session",
                )));
            }
            if report.delivery != crate::inbox::InputDelivery::Queue {
                return Err(query_error(std::io::Error::other(
                    "a reconciled job report must use next-step delivery",
                )));
            }
        }
        (ReportDelivery::NextStep, None) => {
            return Err(query_error(std::io::Error::other(
                "next-step reconciliation requires a replacement parent input",
            )));
        }
        (ReportDelivery::Quiet, Some(_)) => {
            return Err(query_error(std::io::Error::other(
                "quiet reconciliation must not add parent input",
            )));
        }
        (ReportDelivery::Quiet, None) => {}
    }
    match reconciliation.status {
        JobStatus::Completed
            if reconciliation.result.is_none() || reconciliation.error.is_some() =>
        {
            Err(query_error(std::io::Error::other(
                "a completed reconciliation requires a result and no error",
            )))
        }
        JobStatus::Failed | JobStatus::Cancelled
            if reconciliation.error.as_deref().is_none_or(str::is_empty) =>
        {
            Err(query_error(std::io::Error::other(
                "a failed or cancelled reconciliation requires an error",
            )))
        }
        JobStatus::Queued | JobStatus::Running | JobStatus::Uncertain => Err(query_error(
            std::io::Error::other("reconciliation must resolve uncertainty to a final outcome"),
        )),
        JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled => Ok(()),
    }
}

fn created_properties(job: &NewAgentJob) -> Map<String, Value> {
    let mut properties = [
        ("jobID".to_owned(), Value::String(job.id.clone())),
        (
            "parentSessionID".to_owned(),
            Value::String(job.parent_session_id.clone()),
        ),
        (
            "logicalKey".to_owned(),
            Value::String(job.logical_key.clone()),
        ),
        ("subject".to_owned(), job.subject.as_json()),
        (
            "reportDelivery".to_owned(),
            Value::String(job.report_delivery.as_str().to_owned()),
        ),
        (
            "status".to_owned(),
            Value::String(job.initial_status.as_str().to_owned()),
        ),
        (
            "timeCreated".to_owned(),
            Value::Number(job.time_created.into()),
        ),
        (
            "evidenceStartRowid".to_owned(),
            Value::Number(job.evidence_start_rowid.into()),
        ),
    ]
    .into_iter()
    .collect::<Map<_, _>>();
    if let Some(snapshot) = &job.orchestration_snapshot {
        let identity = snapshot
            .identity()
            .expect("AttemptSnapshot contains only serializable identity data");
        properties.insert(
            "orchestrationSnapshotID".to_owned(),
            serde_json::to_value(identity).expect("snapshot identity is serializable"),
        );
        properties.insert(
            "orchestrationSnapshot".to_owned(),
            snapshot
                .canonical_value()
                .expect("AttemptSnapshot is serializable"),
        );
    }
    properties
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

fn reconciliation_properties(
    job: &AgentJob,
    reconciliation: &JobReconciliation,
) -> Map<String, Value> {
    let mut properties = [
        ("jobID".to_owned(), Value::String(job.id.clone())),
        (
            "parentSessionID".to_owned(),
            Value::String(job.parent_session_id.clone()),
        ),
        (
            "logicalKey".to_owned(),
            Value::String(job.logical_key.clone()),
        ),
        ("subject".to_owned(), job.subject.as_json()),
        (
            "status".to_owned(),
            Value::String(reconciliation.status.as_str().to_owned()),
        ),
        (
            "authority".to_owned(),
            Value::String(reconciliation.authority.clone()),
        ),
        (
            "evidence".to_owned(),
            Value::String(reconciliation.evidence.clone()),
        ),
        (
            "timeCompleted".to_owned(),
            Value::Number(reconciliation.time_completed.into()),
        ),
    ]
    .into_iter()
    .collect::<Map<_, _>>();
    if let Some(result) = &reconciliation.result {
        properties.insert("result".to_owned(), result.clone());
    }
    if let Some(error) = &reconciliation.error {
        properties.insert("error".to_owned(), Value::String(error.clone()));
    }
    properties
}

struct StoredJob {
    id: String,
    parent_session_id: String,
    logical_key: String,
    subject_kind: String,
    subject_payload: String,
    orchestration_snapshot: Option<String>,
    evidence_start_rowid: i64,
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
        logical_key: row.get(2)?,
        subject_kind: row.get(3)?,
        subject_payload: row.get(4)?,
        orchestration_snapshot: row.get(5)?,
        evidence_start_rowid: row.get(6)?,
        status: row.get(7)?,
        report_delivery: row.get(8)?,
        result: row.get(9)?,
        error: row.get(10)?,
        report_input_id: row.get(11)?,
        created_sequence: row.get(12)?,
        settled_sequence: row.get(13)?,
        time_created: row.get(14)?,
        time_updated: row.get(15)?,
        time_completed: row.get(16)?,
    })
}

fn decode_job(stored: StoredJob) -> Result<AgentJob, DbError> {
    let payload: Value = serde_json::from_str(&stored.subject_payload).map_err(query_error)?;
    let subject = match stored.subject_kind.as_str() {
        "child-session" => JobSubject::child_session(required_json(&payload, "sessionID")?),
        "product-agent" => JobSubject::product_agent(
            required_json(&payload, "runID")?,
            required_json(&payload, "product")?,
            required_json(&payload, "instance")?,
            required_json(&payload, "tool")?,
        ),
        "workflow" => JobSubject::workflow(
            required_json(&payload, "runID")?,
            required_json(&payload, "workflow")?,
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
        logical_key: stored.logical_key,
        subject,
        orchestration_snapshot: stored
            .orchestration_snapshot
            .map(|snapshot| serde_json::from_str(&snapshot).map_err(query_error))
            .transpose()?,
        evidence_start_rowid: stored.evidence_start_rowid,
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

fn required_json(value: &Value, field: &str) -> Result<String, DbError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            query_error(std::io::Error::other(format!(
                "job subject payload requires non-empty `{field}`"
            )))
        })
}
