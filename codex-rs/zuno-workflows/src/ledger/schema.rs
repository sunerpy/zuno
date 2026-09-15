use super::WorkflowLedgerError;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const WORKFLOW_LEDGER_SCHEMA_VERSION: i64 = 2;

pub(super) async fn initialize_schema(pool: &SqlitePool) -> Result<(), WorkflowLedgerError> {
    let mut transaction = pool.begin().await?;
    let marker_exists = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'workflow_ledger_schema'",
    )
    .fetch_one(&mut *transaction)
    .await?
        == 1;
    if marker_exists {
        let versions = sqlx::query_scalar::<_, i64>(
            "SELECT version FROM workflow_ledger_schema WHERE singleton = 1",
        )
        .fetch_all(&mut *transaction)
        .await?;
        let [version] = versions.as_slice() else {
            return Err(WorkflowLedgerError::CorruptSchemaMarker(format!(
                "expected exactly one marker row, found {}",
                versions.len()
            )));
        };
        if *version > WORKFLOW_LEDGER_SCHEMA_VERSION {
            return Err(WorkflowLedgerError::FutureSchema {
                found: *version,
                supported: WORKFLOW_LEDGER_SCHEMA_VERSION,
            });
        }
        match *version {
            WORKFLOW_LEDGER_SCHEMA_VERSION => {}
            1 => migrate_v1_to_v2(&mut transaction).await?,
            found => {
                return Err(WorkflowLedgerError::UnsupportedSchema {
                    found,
                    supported: WORKFLOW_LEDGER_SCHEMA_VERSION,
                });
            }
        }
        verify_schema(&mut transaction).await?;
        if *version != WORKFLOW_LEDGER_SCHEMA_VERSION {
            // The marker is deliberately updated after the complete migration
            // and structural verification. A marker-only edit is corruption,
            // not a successfully advanced durable format.
            sqlx::query("UPDATE workflow_ledger_schema SET version = ? WHERE singleton = 1")
                .bind(WORKFLOW_LEDGER_SCHEMA_VERSION)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        return Ok(());
    }

    let application_tables = sqlx::query_scalar::<_, i64>(
        r#"
SELECT COUNT(*) FROM sqlite_master
WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
        "#,
    )
    .fetch_one(&mut *transaction)
    .await?;
    if application_tables != 0 {
        return Err(WorkflowLedgerError::MissingSchemaMarker {
            table_count: application_tables,
        });
    }

    sqlx::query(
        r#"
CREATE TABLE workflow_runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    workflow_source TEXT NOT NULL,
    workflow_name TEXT NOT NULL,
    workflow_version TEXT NOT NULL,
    workflow_source_digest TEXT NOT NULL,
    executable_digest TEXT NOT NULL,
    engine TEXT NOT NULL CHECK(engine IN ('graph/v1', 'javascript/v1', 'node-worker/v1')),
    engine_revision TEXT NOT NULL,
    binding_digest TEXT NOT NULL,
    bindings_json TEXT NOT NULL,
    parent_thread_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    args_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'pending_approval', 'queued', 'running', 'completed', 'failed', 'cancelled', 'uncertain'
    )),
    result_json TEXT,
    error TEXT,
    agents_started INTEGER NOT NULL DEFAULT 0 CHECK(agents_started >= 0),
    cancel_requested_at_ms INTEGER,
    cancel_reason TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER
)
        "#,
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
CREATE TABLE workflow_calls (
    run_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    operation TEXT NOT NULL,
    request_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'running', 'completed', 'failed', 'cancelled', 'uncertain'
    )),
    result_json TEXT,
    error TEXT,
    started_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    PRIMARY KEY (run_id, call_id),
    FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id) ON DELETE CASCADE
)
        "#,
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX workflow_runs_parent_status_idx ON workflow_runs(parent_thread_id, status, updated_at_ms)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "CREATE INDEX workflow_calls_run_started_idx ON workflow_calls(run_id, started_at_ms, call_id)",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        r#"
CREATE TABLE workflow_ledger_schema (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1),
    version INTEGER NOT NULL CHECK(version > 0)
)
        "#,
    )
    .execute(&mut *transaction)
    .await?;
    // The marker is deliberately written last so a partial schema can never be
    // mistaken for a successfully migrated durable format.
    sqlx::query("INSERT INTO workflow_ledger_schema(singleton, version) VALUES (1, ?)")
        .bind(WORKFLOW_LEDGER_SCHEMA_VERSION)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

async fn verify_schema(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), WorkflowLedgerError> {
    sqlx::query(
        r#"
SELECT run_id, workflow_source, workflow_name, workflow_version,
       workflow_source_digest, executable_digest, engine, engine_revision,
       binding_digest, bindings_json, parent_thread_id, request_digest, args_json,
       status, result_json, error, agents_started,
       cancel_requested_at_ms, cancel_reason, created_at_ms, updated_at_ms,
       completed_at_ms
FROM workflow_runs LIMIT 0
        "#,
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r#"
SELECT run_id, call_id, request_digest, operation, request_json, status,
       result_json, error, started_at_ms, completed_at_ms
FROM workflow_calls LIMIT 0
        "#,
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn migrate_v1_to_v2(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), WorkflowLedgerError> {
    sqlx::query(
        "ALTER TABLE workflow_runs ADD COLUMN binding_digest TEXT NOT NULL DEFAULT '09d4abbc65f6dfb96405828f0a754c27d64c49760c9f093efb44381b4211dfb0'",
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r#"ALTER TABLE workflow_runs ADD COLUMN bindings_json TEXT NOT NULL DEFAULT '{"routes":{},"schema":"zuno.workflow-bindings/legacy-unbound-v1","schemaVersion":0}'"#,
    )
    .execute(&mut **transaction)
    .await?;
    let now = migration_now_millis()?;
    sqlx::query(
        r#"
UPDATE workflow_calls
SET status = 'uncertain',
    error = COALESCE(error, 'workflow binding was not recorded by ledger schema v1'),
    completed_at_ms = COALESCE(completed_at_ms, ?)
WHERE status = 'running'
        "#,
    )
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r#"
UPDATE workflow_runs
SET status = 'uncertain',
    error = COALESCE(error, 'workflow binding was not recorded by ledger schema v1'),
    updated_at_ms = ?,
    completed_at_ms = COALESCE(completed_at_ms, ?)
WHERE status IN ('pending_approval', 'queued', 'running')
        "#,
    )
    .bind(now)
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn migration_now_millis() -> Result<i64, WorkflowLedgerError> {
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
