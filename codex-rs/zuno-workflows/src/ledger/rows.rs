use super::WorkflowCallStatus;
use super::WorkflowLedgerCall;
use super::WorkflowLedgerError;
use super::WorkflowLedgerRun;
use super::WorkflowRunId;
use super::WorkflowRunStatus;
use crate::WorkflowEngine;
use crate::WorkflowSourceIdentity;
use serde_json::Value as JsonValue;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::Transaction;

pub(super) struct RunRow {
    pub(super) run_id: WorkflowRunId,
    pub(super) workflow: WorkflowSourceIdentity,
    pub(super) executable_digest: String,
    pub(super) engine: WorkflowEngine,
    pub(super) engine_revision: String,
    pub(super) binding_digest: String,
    pub(super) bindings: JsonValue,
    pub(super) parent_thread_id: String,
    pub(super) request_digest: String,
    pub(super) args: JsonValue,
    pub(super) status: WorkflowRunStatus,
    pub(super) result: Option<JsonValue>,
    pub(super) error: Option<String>,
    pub(super) agents_started: u32,
    pub(super) cancel_requested_at: Option<i64>,
    pub(super) cancel_reason: Option<String>,
    pub(super) created_at: i64,
    pub(super) updated_at: i64,
    pub(super) completed_at: Option<i64>,
}

impl RunRow {
    fn into_record(self, calls: Vec<WorkflowLedgerCall>) -> WorkflowLedgerRun {
        WorkflowLedgerRun {
            run_id: self.run_id,
            workflow: self.workflow,
            executable_digest: self.executable_digest,
            engine: self.engine,
            engine_revision: self.engine_revision,
            binding_digest: self.binding_digest,
            bindings: self.bindings,
            parent_thread_id: self.parent_thread_id,
            request_digest: self.request_digest,
            args: self.args,
            status: self.status,
            result: self.result,
            error: self.error,
            agents_started: self.agents_started,
            cancel_requested_at: self.cancel_requested_at,
            cancel_reason: self.cancel_reason,
            created_at: self.created_at,
            updated_at: self.updated_at,
            completed_at: self.completed_at,
            calls,
        }
    }
}

pub(super) async fn read_run_row(
    transaction: &mut Transaction<'_, Sqlite>,
    run_id: &WorkflowRunId,
) -> Result<RunRow, WorkflowLedgerError> {
    let row = sqlx::query(
        r#"
SELECT run_id, workflow_source, workflow_name, workflow_version,
       workflow_source_digest, executable_digest, engine, engine_revision,
       binding_digest, bindings_json, parent_thread_id, request_digest, args_json,
       status, result_json, error, agents_started,
       cancel_requested_at_ms, cancel_reason, created_at_ms, updated_at_ms,
       completed_at_ms
FROM workflow_runs WHERE run_id = ?
        "#,
    )
    .bind(run_id.as_str())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| WorkflowLedgerError::RunNotFound {
        run_id: run_id.as_str().to_string(),
    })?;
    let stored_run_id: String = row.try_get("run_id")?;
    let bindings_json: String = row.try_get("bindings_json")?;
    let args_json: String = row.try_get("args_json")?;
    let result_json: Option<String> = row.try_get("result_json")?;
    let agents_started: i64 = row.try_get("agents_started")?;
    Ok(RunRow {
        run_id: WorkflowRunId::new(stored_run_id)?,
        workflow: WorkflowSourceIdentity {
            source: row.try_get("workflow_source")?,
            name: row.try_get("workflow_name")?,
            version: row.try_get("workflow_version")?,
            digest: row.try_get("workflow_source_digest")?,
        },
        executable_digest: row.try_get("executable_digest")?,
        engine: parse_engine(&row.try_get::<String, _>("engine")?)?,
        engine_revision: row.try_get("engine_revision")?,
        binding_digest: row.try_get("binding_digest")?,
        bindings: serde_json::from_str(&bindings_json)?,
        parent_thread_id: row.try_get("parent_thread_id")?,
        request_digest: row.try_get("request_digest")?,
        args: serde_json::from_str(&args_json)?,
        status: WorkflowRunStatus::parse(row.try_get::<String, _>("status")?.as_str())?,
        result: result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        error: row.try_get("error")?,
        agents_started: u32::try_from(agents_started).map_err(|_| {
            WorkflowLedgerError::InvalidValue(format!(
                "invalid agents_started value {agents_started} for run {}",
                run_id.as_str()
            ))
        })?,
        cancel_requested_at: row.try_get("cancel_requested_at_ms")?,
        cancel_reason: row.try_get("cancel_reason")?,
        created_at: row.try_get("created_at_ms")?,
        updated_at: row.try_get("updated_at_ms")?,
        completed_at: row.try_get("completed_at_ms")?,
    })
}

pub(super) async fn read_run_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    run_id: &WorkflowRunId,
) -> Result<WorkflowLedgerRun, WorkflowLedgerError> {
    let row = read_run_row(transaction, run_id).await?;
    let call_rows = sqlx::query(
        r#"
SELECT run_id, call_id, request_digest, operation, request_json, status,
       result_json, error, started_at_ms, completed_at_ms
FROM workflow_calls WHERE run_id = ?
ORDER BY started_at_ms ASC, call_id ASC
        "#,
    )
    .bind(run_id.as_str())
    .fetch_all(&mut **transaction)
    .await?;
    let calls = call_rows
        .into_iter()
        .map(call_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(row.into_record(calls))
}

pub(super) async fn read_call_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    run_id: &WorkflowRunId,
    call_id: &str,
) -> Result<WorkflowLedgerCall, WorkflowLedgerError> {
    let row = sqlx::query(
        r#"
SELECT run_id, call_id, request_digest, operation, request_json, status,
       result_json, error, started_at_ms, completed_at_ms
FROM workflow_calls WHERE run_id = ? AND call_id = ?
        "#,
    )
    .bind(run_id.as_str())
    .bind(call_id)
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| WorkflowLedgerError::CallNotFound {
        run_id: run_id.as_str().to_string(),
        call_id: call_id.to_string(),
    })?;
    call_from_row(row)
}

fn call_from_row(row: sqlx::sqlite::SqliteRow) -> Result<WorkflowLedgerCall, WorkflowLedgerError> {
    let stored_run_id: String = row.try_get("run_id")?;
    let request_json: String = row.try_get("request_json")?;
    let result_json: Option<String> = row.try_get("result_json")?;
    Ok(WorkflowLedgerCall {
        run_id: WorkflowRunId::new(stored_run_id)?,
        call_id: row.try_get("call_id")?,
        request_digest: row.try_get("request_digest")?,
        operation: row.try_get("operation")?,
        request: serde_json::from_str(&request_json)?,
        status: WorkflowCallStatus::parse(row.try_get::<String, _>("status")?.as_str())?,
        result: result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        error: row.try_get("error")?,
        started_at: row.try_get("started_at_ms")?,
        completed_at: row.try_get("completed_at_ms")?,
    })
}

pub(super) async fn mark_run_uncertain_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    run_id: &WorkflowRunId,
    error: &str,
    now: i64,
) -> Result<(), WorkflowLedgerError> {
    let run = read_run_row(transaction, run_id).await?;
    if run.status == WorkflowRunStatus::Running {
        sqlx::query(
            r#"
UPDATE workflow_runs
SET status = 'uncertain', error = COALESCE(error, ?), updated_at_ms = ?,
    completed_at_ms = COALESCE(completed_at_ms, ?)
WHERE run_id = ? AND status = 'running'
            "#,
        )
        .bind(error)
        .bind(now)
        .bind(now)
        .bind(run_id.as_str())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

fn parse_engine(value: &str) -> Result<WorkflowEngine, WorkflowLedgerError> {
    match value {
        "graph/v1" => Ok(WorkflowEngine::GraphV1),
        "javascript/v1" => Ok(WorkflowEngine::JavaScriptV1),
        "node-worker/v1" => Ok(WorkflowEngine::NodeWorkerV1),
        value => Err(WorkflowLedgerError::InvalidValue(format!(
            "unknown workflow engine `{value}`"
        ))),
    }
}
