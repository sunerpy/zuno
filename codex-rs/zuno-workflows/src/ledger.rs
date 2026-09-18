mod rows;
mod schema;
mod types;

pub use types::AcceptCallOutcome;
pub use types::AcceptRunOutcome;
pub use types::AcceptWorkflowCall;
pub use types::AcceptWorkflowRun;
pub use types::RecoverySummary;
pub use types::WORKFLOW_LEDGER_DB_FILENAME;
pub use types::WorkflowCallCompletion;
pub use types::WorkflowCallStatus;
pub use types::WorkflowLedgerCall;
pub use types::WorkflowLedgerError;
pub use types::WorkflowLedgerRun;
pub use types::WorkflowRunCompletion;
pub use types::WorkflowRunStatus;

use self::rows::mark_run_uncertain_in_transaction;
use self::rows::read_call_in_transaction;
use self::rows::read_run_in_transaction;
use self::rows::read_run_row;
use self::schema::initialize_schema;
#[cfg(test)]
use crate::WorkflowCallIdentity;
use crate::WorkflowRunId;
#[cfg(test)]
use crate::WorkflowSourceIdentity;
use codex_state::SqliteConfig;
use serde_json::Value as JsonValue;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[derive(Clone)]
pub struct WorkflowLedger {
    pool: SqlitePool,
    path: PathBuf,
}

impl WorkflowLedger {
    pub async fn open(sqlite: &SqliteConfig) -> Result<Self, WorkflowLedgerError> {
        tokio::fs::create_dir_all(sqlite.home())
            .await
            .map_err(|error| WorkflowLedgerError::InvalidValue(error.to_string()))?;
        let path = sqlite.home().join(WORKFLOW_LEDGER_DB_FILENAME);
        let pool = sqlite.open_read_write_pool(&path).await?;
        match Self::from_pool(pool, path.clone()).await {
            Ok(ledger) => Ok(ledger),
            Err(error) => Err(error),
        }
    }

    /// Begins a transaction that takes SQLite's write lock up front.
    ///
    /// Every mutation below reads the current row before updating it. With a
    /// deferred `BEGIN`, SQLite returns `SQLITE_BUSY` at the read-to-write
    /// upgrade without invoking the busy handler when another writer is active,
    /// which surfaced as "database is locked" failures while the engine and
    /// concurrent `workflow/run/read` calls shared the ledger. `BEGIN IMMEDIATE`
    /// waits for the lock within the pool's busy timeout instead.
    async fn begin_write(&self) -> Result<Transaction<'static, Sqlite>, sqlx::Error> {
        self.pool.begin_with("BEGIN IMMEDIATE").await
    }

    pub async fn from_pool(pool: SqlitePool, path: PathBuf) -> Result<Self, WorkflowLedgerError> {
        if let Err(error) = initialize_schema(&pool).await {
            pool.close().await;
            return Err(error);
        }
        Ok(Self { pool, path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn accept_run(
        &self,
        request: AcceptWorkflowRun,
    ) -> Result<AcceptRunOutcome, WorkflowLedgerError> {
        let request_digest = request.request_digest()?;
        let binding_digest = request.binding_digest()?;
        let bindings_json = request.canonical_bindings_json()?;
        let args_json = serde_json::to_string(&request.args)?;
        let now = now_millis()?;
        let initial_status = if request.pending_approval {
            WorkflowRunStatus::PendingApproval
        } else {
            WorkflowRunStatus::Queued
        };
        let mut transaction = self.begin_write().await?;
        let inserted = sqlx::query(
            r#"
INSERT INTO workflow_runs (
    run_id,
    workflow_source,
    workflow_name,
    workflow_version,
    workflow_source_digest,
    executable_digest,
    engine,
    engine_revision,
    binding_digest,
    bindings_json,
    parent_thread_id,
    request_digest,
    args_json,
    status,
    agents_started,
    created_at_ms,
    updated_at_ms
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)
ON CONFLICT(run_id) DO NOTHING
            "#,
        )
        .bind(request.run_id.as_str())
        .bind(&request.workflow.source)
        .bind(&request.workflow.name)
        .bind(&request.workflow.version)
        .bind(&request.workflow.digest)
        .bind(&request.executable_digest)
        .bind(request.engine.to_string())
        .bind(&request.engine_revision)
        .bind(&binding_digest)
        .bind(bindings_json)
        .bind(&request.parent_thread_id)
        .bind(&request_digest)
        .bind(args_json)
        .bind(initial_status.as_str())
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        let record = read_run_in_transaction(&mut transaction, &request.run_id).await?;
        if record.request_digest != request_digest {
            return Err(WorkflowLedgerError::RunReplayDiverged {
                run_id: request.run_id.as_str().to_string(),
                recorded: record.request_digest,
                actual: request_digest,
            });
        }
        transaction.commit().await?;
        Ok(if inserted {
            AcceptRunOutcome::Created(record)
        } else {
            AcceptRunOutcome::Existing(record)
        })
    }

    pub async fn get_run(
        &self,
        run_id: &WorkflowRunId,
    ) -> Result<WorkflowLedgerRun, WorkflowLedgerError> {
        let mut transaction = self.pool.begin().await?;
        let record = read_run_in_transaction(&mut transaction, run_id).await?;
        transaction.commit().await?;
        Ok(record)
    }

    pub async fn mark_run_running(
        &self,
        run_id: &WorkflowRunId,
    ) -> Result<WorkflowLedgerRun, WorkflowLedgerError> {
        self.transition_run(run_id, WorkflowRunStatus::Running, None, None, None)
            .await
    }

    pub async fn approve_run(
        &self,
        run_id: &WorkflowRunId,
    ) -> Result<WorkflowLedgerRun, WorkflowLedgerError> {
        self.transition_run(run_id, WorkflowRunStatus::Queued, None, None, None)
            .await
    }

    pub async fn request_cancel(
        &self,
        run_id: &WorkflowRunId,
        reason: Option<&str>,
    ) -> Result<WorkflowLedgerRun, WorkflowLedgerError> {
        let now = now_millis()?;
        let mut transaction = self.begin_write().await?;
        let current = read_run_row(&mut transaction, run_id).await?;
        if current.status.is_terminal() {
            transaction.commit().await?;
            return self.get_run(run_id).await;
        }
        let (status, completed_at) = match current.status {
            WorkflowRunStatus::PendingApproval | WorkflowRunStatus::Queued => {
                (WorkflowRunStatus::Cancelled, Some(now))
            }
            WorkflowRunStatus::Running => (WorkflowRunStatus::Running, None),
            status => {
                return Err(WorkflowLedgerError::InvalidRunTransition {
                    run_id: run_id.as_str().to_string(),
                    from: status,
                    to: WorkflowRunStatus::Cancelled,
                });
            }
        };
        sqlx::query(
            r#"
UPDATE workflow_runs
SET status = ?, cancel_requested_at_ms = COALESCE(cancel_requested_at_ms, ?),
    cancel_reason = COALESCE(cancel_reason, ?), updated_at_ms = ?,
    completed_at_ms = COALESCE(completed_at_ms, ?),
    error = CASE WHEN ? = 'cancelled' THEN COALESCE(error, ?) ELSE error END
WHERE run_id = ?
            "#,
        )
        .bind(status.as_str())
        .bind(now)
        .bind(reason)
        .bind(now)
        .bind(completed_at)
        .bind(status.as_str())
        .bind(reason)
        .bind(run_id.as_str())
        .execute(&mut *transaction)
        .await?;
        let record = read_run_in_transaction(&mut transaction, run_id).await?;
        transaction.commit().await?;
        Ok(record)
    }

    pub async fn accept_call(
        &self,
        request: AcceptWorkflowCall,
    ) -> Result<AcceptCallOutcome, WorkflowLedgerError> {
        request.identity.verify_replay(&request.request)?;
        if request.operation.trim().is_empty() {
            return Err(WorkflowLedgerError::InvalidValue(
                "workflow call operation must not be empty".to_string(),
            ));
        }
        let now = now_millis()?;
        let request_json = serde_json::to_string(&request.request)?;
        let mut transaction = self.begin_write().await?;
        let run = read_run_row(&mut transaction, &request.run_id).await?;
        if run.status != WorkflowRunStatus::Running {
            return Err(WorkflowLedgerError::InvalidRunTransition {
                run_id: request.run_id.as_str().to_string(),
                from: run.status,
                to: WorkflowRunStatus::Running,
            });
        }
        if run.cancel_requested_at.is_some() {
            return Err(WorkflowLedgerError::RunCancellationRequested {
                run_id: request.run_id.as_str().to_string(),
            });
        }
        let inserted = sqlx::query(
            r#"
INSERT INTO workflow_calls (
    run_id, call_id, request_digest, operation, request_json, status, started_at_ms
) VALUES (?, ?, ?, ?, ?, 'running', ?)
ON CONFLICT(run_id, call_id) DO NOTHING
            "#,
        )
        .bind(request.run_id.as_str())
        .bind(&request.identity.id)
        .bind(&request.identity.request_digest)
        .bind(&request.operation)
        .bind(request_json)
        .bind(now)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        let call =
            read_call_in_transaction(&mut transaction, &request.run_id, &request.identity.id)
                .await?;
        if call.request_digest != request.identity.request_digest
            || call.operation != request.operation
        {
            return Err(WorkflowLedgerError::CallReplayDiverged {
                run_id: request.run_id.as_str().to_string(),
                call_id: request.identity.id,
                recorded: call.request_digest,
                actual: request.identity.request_digest,
            });
        }
        if inserted {
            sqlx::query("UPDATE workflow_runs SET updated_at_ms = ? WHERE run_id = ?")
                .bind(now)
                .bind(request.run_id.as_str())
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(if inserted {
            AcceptCallOutcome::Dispatch(call)
        } else {
            AcceptCallOutcome::Existing(call)
        })
    }

    pub async fn complete_call(
        &self,
        run_id: &WorkflowRunId,
        call_id: &str,
        completion: WorkflowCallCompletion,
    ) -> Result<WorkflowLedgerCall, WorkflowLedgerError> {
        let now = now_millis()?;
        let (status, result, error) = completion.fields();
        let result_json = result.map(serde_json::to_string).transpose()?;
        let mut transaction = self.begin_write().await?;
        let current = read_call_in_transaction(&mut transaction, run_id, call_id).await?;
        if current.status.is_terminal() {
            let same = current.status == status
                && current.result.as_ref() == result
                && current.error.as_deref() == error;
            if same {
                transaction.commit().await?;
                return Ok(current);
            }
            return Err(WorkflowLedgerError::CallCompletionConflict {
                run_id: run_id.as_str().to_string(),
                call_id: call_id.to_string(),
                recorded: current.status,
                actual: status,
            });
        }
        sqlx::query(
            r#"
UPDATE workflow_calls
SET status = ?, result_json = ?, error = ?, completed_at_ms = ?
WHERE run_id = ? AND call_id = ? AND status = 'running'
            "#,
        )
        .bind(status.as_str())
        .bind(result_json)
        .bind(error)
        .bind(now)
        .bind(run_id.as_str())
        .bind(call_id)
        .execute(&mut *transaction)
        .await?;
        if status == WorkflowCallStatus::Uncertain {
            mark_run_uncertain_in_transaction(
                &mut transaction,
                run_id,
                error.unwrap_or("workflow call has an uncertain outcome"),
                now,
            )
            .await?;
        } else {
            sqlx::query("UPDATE workflow_runs SET updated_at_ms = ? WHERE run_id = ?")
                .bind(now)
                .bind(run_id.as_str())
                .execute(&mut *transaction)
                .await?;
        }
        let record = read_call_in_transaction(&mut transaction, run_id, call_id).await?;
        transaction.commit().await?;
        Ok(record)
    }

    pub async fn complete_run(
        &self,
        run_id: &WorkflowRunId,
        completion: WorkflowRunCompletion,
    ) -> Result<WorkflowLedgerRun, WorkflowLedgerError> {
        let (status, result, error, agents_started) = completion.fields();
        self.transition_run(
            run_id,
            status,
            result.cloned(),
            error.map(str::to_string),
            Some(agents_started),
        )
        .await
    }

    /// Conservatively closes process-owned execution gaps after a restart.
    ///
    /// A call is marked `running` before dispatch, so a process loss cannot prove
    /// whether its side effect happened. Neither calls nor their owning runs are
    /// replayed mechanically; they become `uncertain` for authoritative inspection.
    pub async fn recover_interrupted(&self) -> Result<RecoverySummary, WorkflowLedgerError> {
        let now = now_millis()?;
        let mut transaction = self.begin_write().await?;
        let uncertain_calls = sqlx::query(
            r#"
UPDATE workflow_calls
SET status = 'uncertain', error = COALESCE(error, 'process exited while call outcome was unknown'),
    completed_at_ms = ?
WHERE status = 'running'
            "#,
        )
        .bind(now)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        let uncertain_runs = sqlx::query(
            r#"
UPDATE workflow_runs
SET status = 'uncertain', error = COALESCE(error, 'process exited while workflow outcome was unknown'),
    updated_at_ms = ?, completed_at_ms = COALESCE(completed_at_ms, ?)
WHERE status = 'running'
            "#,
        )
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        transaction.commit().await?;
        Ok(RecoverySummary {
            uncertain_runs,
            uncertain_calls,
        })
    }

    async fn transition_run(
        &self,
        run_id: &WorkflowRunId,
        target: WorkflowRunStatus,
        result: Option<JsonValue>,
        error: Option<String>,
        agents_started: Option<u32>,
    ) -> Result<WorkflowLedgerRun, WorkflowLedgerError> {
        let now = now_millis()?;
        let result_json = result.as_ref().map(serde_json::to_string).transpose()?;
        let mut transaction = self.begin_write().await?;
        let current = read_run_row(&mut transaction, run_id).await?;
        if current.status == target {
            let full = read_run_in_transaction(&mut transaction, run_id).await?;
            let same_payload = match target {
                WorkflowRunStatus::Completed => full.result == result,
                WorkflowRunStatus::Failed
                | WorkflowRunStatus::Cancelled
                | WorkflowRunStatus::Uncertain => full.error == error,
                _ => result.is_none() && error.is_none(),
            } && agents_started.is_none_or(|value| full.agents_started == value);
            if same_payload {
                transaction.commit().await?;
                return Ok(full);
            }
        }
        if !valid_run_transition(current.status, target) {
            return Err(WorkflowLedgerError::InvalidRunTransition {
                run_id: run_id.as_str().to_string(),
                from: current.status,
                to: target,
            });
        }
        let completed_at = target.is_terminal().then_some(now);
        sqlx::query(
            r#"
UPDATE workflow_runs
SET status = ?, result_json = ?, error = ?, agents_started = COALESCE(?, agents_started),
    updated_at_ms = ?, completed_at_ms = ?
WHERE run_id = ?
            "#,
        )
        .bind(target.as_str())
        .bind(result_json)
        .bind(error)
        .bind(agents_started.map(i64::from))
        .bind(now)
        .bind(completed_at)
        .bind(run_id.as_str())
        .execute(&mut *transaction)
        .await?;
        let record = read_run_in_transaction(&mut transaction, run_id).await?;
        transaction.commit().await?;
        Ok(record)
    }
}

fn valid_run_transition(from: WorkflowRunStatus, to: WorkflowRunStatus) -> bool {
    matches!(
        (from, to),
        (
            WorkflowRunStatus::PendingApproval,
            WorkflowRunStatus::Queued | WorkflowRunStatus::Cancelled | WorkflowRunStatus::Failed
        ) | (
            WorkflowRunStatus::Queued,
            WorkflowRunStatus::Running | WorkflowRunStatus::Cancelled | WorkflowRunStatus::Failed
        ) | (
            WorkflowRunStatus::Running,
            WorkflowRunStatus::Completed
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Cancelled
                | WorkflowRunStatus::Uncertain
        )
    )
}

fn now_millis() -> Result<i64, WorkflowLedgerError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| WorkflowLedgerError::InvalidValue(error.to_string()))?
        .as_millis();
    i64::try_from(millis).map_err(|_| {
        WorkflowLedgerError::InvalidValue(
            "system time does not fit in i64 milliseconds".to_string(),
        )
    })
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
