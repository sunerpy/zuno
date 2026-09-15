use super::*;
use crate::WorkflowEngine;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

fn sqlite_config(temp: &TempDir) -> SqliteConfig {
    SqliteConfig::new_for_testing(
        AbsolutePathBuf::from_absolute_path(temp.path())
            .expect("temporary directory should be absolute"),
    )
}

fn run_request(run_id: &str, args: JsonValue) -> AcceptWorkflowRun {
    AcceptWorkflowRun {
        run_id: WorkflowRunId::new(run_id).expect("valid run id"),
        workflow: WorkflowSourceIdentity {
            source: "/tmp/review.yaml".to_string(),
            name: "review".to_string(),
            version: "1".to_string(),
            digest: "source-sha256".to_string(),
        },
        executable_digest: "executable-sha256".to_string(),
        engine: WorkflowEngine::GraphV1,
        engine_revision: "graph/v1+zuno/v1".to_string(),
        bindings: json!({
            "schemaVersion": 1,
            "schema": "zuno.workflow-bindings/v1",
            "routes": {
                "review": {
                    "digest": "route-binding-sha256",
                    "snapshot": {"backend": "native-codex"}
                }
            }
        }),
        parent_thread_id: "thread-1".to_string(),
        args,
        pending_approval: false,
    }
}

#[tokio::test]
async fn invalid_or_oversized_bindings_fail_before_admission() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    for (run_id, bindings, expected) in [
        ("array", json!([]), "must be a JSON object"),
        (
            "missing-version",
            json!({"schema": "zuno.workflow-bindings/v1", "routes": {}}),
            "schemaVersion must be 1",
        ),
        (
            "missing-schema",
            json!({"schemaVersion": 1, "routes": {}}),
            "schema must be",
        ),
        (
            "missing-routes",
            json!({
                "schemaVersion": 1,
                "schema": "zuno.workflow-bindings/v1"
            }),
            "routes must be a JSON object",
        ),
        (
            "oversized",
            json!({
                "schemaVersion": 1,
                "schema": "zuno.workflow-bindings/v1",
                "routes": {},
                "padding": "x".repeat(300 * 1024),
            }),
            "exceed the 262144-byte limit",
        ),
    ] {
        let mut request = run_request(run_id, json!({}));
        request.bindings = bindings;
        let error = ledger
            .accept_run(request)
            .await
            .expect_err("invalid bindings must not be admitted");
        assert!(error.to_string().contains(expected), "{error:?}");
    }
}

async fn open_test_ledger(temp: &TempDir) -> WorkflowLedger {
    WorkflowLedger::open(&sqlite_config(temp))
        .await
        .expect("ledger should open")
}

#[tokio::test]
async fn accepted_run_is_idempotent_and_survives_restart() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    let created = ledger
        .accept_run(run_request("run-1", json!({"issue": 42})))
        .await
        .expect("run should be accepted");
    assert!(matches!(created, AcceptRunOutcome::Created(_)));
    assert_eq!(created.record().status, WorkflowRunStatus::Queued);
    let existing = ledger
        .accept_run(run_request("run-1", json!({"issue": 42})))
        .await
        .expect("same start should be idempotent");
    assert!(matches!(existing, AcceptRunOutcome::Existing(_)));
    assert_eq!(
        existing.record().request_digest,
        created.record().request_digest
    );
    ledger.close().await;

    let reopened = open_test_ledger(&temp).await;
    let restored = reopened
        .get_run(&WorkflowRunId::new("run-1").expect("valid run id"))
        .await
        .expect("run should be restored");
    assert_eq!(restored.args, json!({"issue": 42}));
    assert_eq!(restored.workflow.name, "review");
    assert_eq!(restored.status, WorkflowRunStatus::Queued);
    assert_eq!(restored.bindings, run_request("unused", json!({})).bindings);
    assert_eq!(restored.binding_digest.len(), 64);
}

#[tokio::test]
async fn run_id_reuse_with_different_args_fails_closed() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    ledger
        .accept_run(run_request("run-1", json!({"issue": 42})))
        .await
        .expect("run should be accepted");

    let error = ledger
        .accept_run(run_request("run-1", json!({"issue": 43})))
        .await
        .expect_err("divergent start must fail");
    assert!(matches!(
        error,
        WorkflowLedgerError::RunReplayDiverged { .. }
    ));
    let restored = ledger
        .get_run(&WorkflowRunId::new("run-1").expect("valid run id"))
        .await
        .expect("original run should remain");
    assert_eq!(restored.args, json!({"issue": 42}));
}

#[tokio::test]
async fn run_id_reuse_with_different_route_binding_fails_closed() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    ledger
        .accept_run(run_request("run-1", json!({})))
        .await
        .expect("run should be accepted");

    let mut changed = run_request("run-1", json!({}));
    changed.bindings["routes"]["review"]["digest"] = json!("changed-binding-sha256");
    let error = ledger
        .accept_run(changed)
        .await
        .expect_err("a changed backend/profile binding must fail replay");
    assert!(matches!(
        error,
        WorkflowLedgerError::RunReplayDiverged { .. }
    ));
}

#[tokio::test]
async fn call_is_dispatched_once_and_terminal_completion_is_idempotent() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    let run_id = WorkflowRunId::new("run-1").expect("valid run id");
    ledger
        .accept_run(run_request("run-1", json!({})))
        .await
        .expect("run should be accepted");
    ledger
        .mark_run_running(&run_id)
        .await
        .expect("run should start");
    let request = json!({"route": "review", "prompt": "inspect"});
    let identity = WorkflowCallIdentity::new("call-1", &request).expect("valid call identity");
    let first = ledger
        .accept_call(AcceptWorkflowCall {
            run_id: run_id.clone(),
            identity: identity.clone(),
            operation: "agent".to_string(),
            request: request.clone(),
        })
        .await
        .expect("call should be accepted");
    assert!(matches!(first, AcceptCallOutcome::Dispatch(_)));
    let retry = ledger
        .accept_call(AcceptWorkflowCall {
            run_id: run_id.clone(),
            identity,
            operation: "agent".to_string(),
            request,
        })
        .await
        .expect("same call should be recognized");
    assert!(matches!(retry, AcceptCallOutcome::Existing(_)));

    let completed = ledger
        .complete_call(
            &run_id,
            "call-1",
            WorkflowCallCompletion::Completed(json!({"verdict": "pass"})),
        )
        .await
        .expect("call should complete");
    assert_eq!(completed.status, WorkflowCallStatus::Completed);
    let duplicate = ledger
        .complete_call(
            &run_id,
            "call-1",
            WorkflowCallCompletion::Completed(json!({"verdict": "pass"})),
        )
        .await
        .expect("same completion should be idempotent");
    assert_eq!(duplicate, completed);

    let error = ledger
        .complete_call(
            &run_id,
            "call-1",
            WorkflowCallCompletion::Completed(json!({"verdict": "fail"})),
        )
        .await
        .expect_err("divergent completion must fail");
    assert!(matches!(
        error,
        WorkflowLedgerError::CallCompletionConflict { .. }
    ));
}

#[tokio::test]
async fn call_id_reuse_with_different_request_fails_closed() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    let run_id = WorkflowRunId::new("run-1").expect("valid run id");
    ledger
        .accept_run(run_request("run-1", json!({})))
        .await
        .expect("run should be accepted");
    ledger
        .mark_run_running(&run_id)
        .await
        .expect("run should start");
    let original = json!({"prompt": "one"});
    ledger
        .accept_call(AcceptWorkflowCall {
            run_id: run_id.clone(),
            identity: WorkflowCallIdentity::new("call-1", &original).expect("valid call identity"),
            operation: "agent".to_string(),
            request: original,
        })
        .await
        .expect("call should be accepted");

    let changed = json!({"prompt": "two"});
    let error = ledger
        .accept_call(AcceptWorkflowCall {
            run_id,
            identity: WorkflowCallIdentity::new("call-1", &changed).expect("valid call identity"),
            operation: "agent".to_string(),
            request: changed,
        })
        .await
        .expect_err("divergent replay must fail");
    assert!(matches!(
        error,
        WorkflowLedgerError::CallReplayDiverged { .. }
    ));
}

#[tokio::test]
async fn restart_marks_running_run_and_call_uncertain_without_replay() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    let run_id = WorkflowRunId::new("run-1").expect("valid run id");
    ledger
        .accept_run(run_request("run-1", json!({})))
        .await
        .expect("run should be accepted");
    ledger
        .mark_run_running(&run_id)
        .await
        .expect("run should start");
    let request = json!({"path": "/tmp/output"});
    ledger
        .accept_call(AcceptWorkflowCall {
            run_id: run_id.clone(),
            identity: WorkflowCallIdentity::new("call-1", &request).expect("valid call identity"),
            operation: "agent".to_string(),
            request,
        })
        .await
        .expect("call should be accepted");
    ledger.close().await;

    let reopened = open_test_ledger(&temp).await;
    let summary = reopened
        .recover_interrupted()
        .await
        .expect("recovery should succeed");
    assert_eq!(
        summary,
        RecoverySummary {
            uncertain_runs: 1,
            uncertain_calls: 1,
        }
    );
    let restored = reopened
        .get_run(&run_id)
        .await
        .expect("run should remain readable");
    assert_eq!(restored.status, WorkflowRunStatus::Uncertain);
    assert_eq!(restored.calls[0].status, WorkflowCallStatus::Uncertain);
    assert!(
        restored.calls[0]
            .error
            .as_deref()
            .is_some_and(|error| { error.contains("outcome was unknown") })
    );

    let retry = reopened
        .recover_interrupted()
        .await
        .expect("recovery should be idempotent");
    assert_eq!(
        retry,
        RecoverySummary {
            uncertain_runs: 0,
            uncertain_calls: 0,
        }
    );
}

#[tokio::test]
async fn uncertain_call_atomically_marks_owning_run_uncertain() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    let run_id = WorkflowRunId::new("run-1").expect("valid run id");
    ledger
        .accept_run(run_request("run-1", json!({})))
        .await
        .expect("run should be accepted");
    ledger
        .mark_run_running(&run_id)
        .await
        .expect("run should start");
    let request = json!({"command": "publish"});
    ledger
        .accept_call(AcceptWorkflowCall {
            run_id: run_id.clone(),
            identity: WorkflowCallIdentity::new("call-1", &request).expect("valid call identity"),
            operation: "agent".to_string(),
            request,
        })
        .await
        .expect("call should be accepted");
    ledger
        .complete_call(
            &run_id,
            "call-1",
            WorkflowCallCompletion::Uncertain("response lost after dispatch".to_string()),
        )
        .await
        .expect("uncertain completion should persist");

    let run = ledger.get_run(&run_id).await.expect("run should load");
    assert_eq!(run.status, WorkflowRunStatus::Uncertain);
    assert_eq!(run.error.as_deref(), Some("response lost after dispatch"));
    assert_eq!(run.calls[0].status, WorkflowCallStatus::Uncertain);
}

#[tokio::test]
async fn completed_run_is_immutable() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    let run_id = WorkflowRunId::new("run-1").expect("valid run id");
    ledger
        .accept_run(run_request("run-1", json!({})))
        .await
        .expect("run should be accepted");
    ledger
        .mark_run_running(&run_id)
        .await
        .expect("run should start");
    ledger
        .complete_run(
            &run_id,
            WorkflowRunCompletion::Completed {
                result: json!({"ok": true}),
                agents_started: 2,
            },
        )
        .await
        .expect("run should complete");

    let duplicate = ledger
        .complete_run(
            &run_id,
            WorkflowRunCompletion::Completed {
                result: json!({"ok": true}),
                agents_started: 2,
            },
        )
        .await
        .expect("same completion should be idempotent");
    assert_eq!(duplicate.status, WorkflowRunStatus::Completed);
    assert_eq!(duplicate.agents_started, 2);
    let error = ledger
        .complete_run(
            &run_id,
            WorkflowRunCompletion::Failed {
                error: "late failure".to_string(),
                agents_started: 2,
            },
        )
        .await
        .expect_err("terminal status must not change");
    assert!(matches!(
        error,
        WorkflowLedgerError::InvalidRunTransition { .. }
    ));
}

#[tokio::test]
async fn future_schema_fails_closed_without_rewriting_marker() {
    let temp = TempDir::new().expect("temporary directory");
    let config = sqlite_config(&temp);
    let ledger = WorkflowLedger::open(&config)
        .await
        .expect("ledger should initialize");
    let path = ledger.path().to_path_buf();
    ledger.close().await;
    let pool = config
        .open_read_write_pool(&path)
        .await
        .expect("database should open");
    sqlx::query("UPDATE workflow_ledger_schema SET version = 999 WHERE singleton = 1")
        .execute(&pool)
        .await
        .expect("marker should update");
    pool.close().await;

    let error = match WorkflowLedger::open(&config).await {
        Ok(_) => panic!("future schema must fail"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        WorkflowLedgerError::FutureSchema {
            found: 999,
            supported: 2
        }
    ));
    let pool = config
        .open_read_write_pool(&path)
        .await
        .expect("database should reopen");
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT version FROM workflow_ledger_schema WHERE singleton = 1",
    )
    .fetch_one(&pool)
    .await
    .expect("marker should remain readable");
    assert_eq!(version, 999);
    pool.close().await;
}

#[tokio::test]
async fn v1_schema_migrates_atomically_and_preserves_durable_rows() {
    let temp = TempDir::new().expect("temporary directory");
    let config = sqlite_config(&temp);
    let path = config.home().join(WORKFLOW_LEDGER_DB_FILENAME);
    let pool = config
        .open_read_write_pool(&path)
        .await
        .expect("database should open");
    sqlx::query(
        r#"
CREATE TABLE workflow_runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    workflow_source TEXT NOT NULL,
    workflow_name TEXT NOT NULL,
    workflow_version TEXT NOT NULL,
    workflow_source_digest TEXT NOT NULL,
    executable_digest TEXT NOT NULL,
    engine TEXT NOT NULL,
    engine_revision TEXT NOT NULL,
    parent_thread_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    args_json TEXT NOT NULL,
    status TEXT NOT NULL,
    result_json TEXT,
    error TEXT,
    agents_started INTEGER NOT NULL DEFAULT 0,
    cancel_requested_at_ms INTEGER,
    cancel_reason TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER
)
        "#,
    )
    .execute(&pool)
    .await
    .expect("v1 runs table");
    sqlx::query(
        r#"
CREATE TABLE workflow_calls (
    run_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    operation TEXT NOT NULL,
    request_json TEXT NOT NULL,
    status TEXT NOT NULL,
    result_json TEXT,
    error TEXT,
    started_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    PRIMARY KEY (run_id, call_id),
    FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id) ON DELETE CASCADE
)
        "#,
    )
    .execute(&pool)
    .await
    .expect("v1 calls table");
    sqlx::query(
        "CREATE TABLE workflow_ledger_schema (singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1), version INTEGER NOT NULL CHECK(version > 0))",
    )
    .execute(&pool)
    .await
    .expect("v1 marker table");
    sqlx::query(
        r#"
INSERT INTO workflow_runs (
    run_id, workflow_source, workflow_name, workflow_version,
    workflow_source_digest, executable_digest, engine, engine_revision,
    parent_thread_id, request_digest, args_json, status, result_json,
    agents_started, created_at_ms, updated_at_ms, completed_at_ms
) VALUES (
    'v1-run', '/tmp/v1.yaml', 'v1-review', '1', 'source-v1',
    'executable-v1', 'graph/v1', 'graph/v1+zuno/v1', 'thread-v1',
    'request-v1', '{"issue":42}', 'completed', '{"ok":true}', 1,
    100, 200, 200
), (
    'v1-queued', '/tmp/v1-queued.yaml', 'v1-queued', '1', 'source-v1-queued',
    'executable-v1-queued', 'graph/v1', 'graph/v1+zuno/v1', 'thread-v1',
    'request-v1-queued', '{}', 'queued', NULL, 0,
    210, 220, NULL
)
        "#,
    )
    .execute(&pool)
    .await
    .expect("v1 run row");
    sqlx::query(
        r#"
INSERT INTO workflow_calls (
    run_id, call_id, request_digest, operation, request_json, status,
    result_json, started_at_ms, completed_at_ms
) VALUES (
    'v1-run', 'call-1', 'call-request-v1', 'agent', '{"prompt":"inspect"}',
    'completed', '{"answer":"ok"}', 110, 190
)
        "#,
    )
    .execute(&pool)
    .await
    .expect("v1 call row");
    sqlx::query(
        r#"
INSERT INTO workflow_calls (
    run_id, call_id, request_digest, operation, request_json, status,
    result_json, started_at_ms, completed_at_ms
) VALUES (
    'v1-queued', 'call-running', 'call-request-running', 'agent',
    '{"prompt":"unknown outcome"}', 'running', NULL, 215, NULL
)
        "#,
    )
    .execute(&pool)
    .await
    .expect("v1 running call row");
    sqlx::query("INSERT INTO workflow_ledger_schema(singleton, version) VALUES (1, 1)")
        .execute(&pool)
        .await
        .expect("v1 marker row");
    pool.close().await;

    let ledger = WorkflowLedger::open(&config)
        .await
        .expect("v1 ledger should migrate");
    let restored = ledger
        .get_run(&WorkflowRunId::new("v1-run").expect("run id"))
        .await
        .expect("migrated run");
    assert_eq!(restored.workflow.name, "v1-review");
    assert_eq!(restored.args, json!({"issue": 42}));
    assert_eq!(restored.result, Some(json!({"ok": true})));
    assert_eq!(restored.calls[0].result, Some(json!({"answer": "ok"})));
    assert_eq!(
        restored.bindings,
        json!({
            "routes": {},
            "schema": "zuno.workflow-bindings/legacy-unbound-v1",
            "schemaVersion": 0
        })
    );
    assert_eq!(
        restored.binding_digest,
        "09d4abbc65f6dfb96405828f0a754c27d64c49760c9f093efb44381b4211dfb0"
    );
    let unresolved = ledger
        .get_run(&WorkflowRunId::new("v1-queued").expect("run id"))
        .await
        .expect("migrated unresolved run");
    assert_eq!(unresolved.status, WorkflowRunStatus::Uncertain);
    assert_eq!(
        unresolved.error.as_deref(),
        Some("workflow binding was not recorded by ledger schema v1")
    );
    assert!(unresolved.completed_at.is_some());
    assert_eq!(unresolved.calls[0].status, WorkflowCallStatus::Uncertain);
    assert_eq!(
        unresolved.calls[0].error.as_deref(),
        Some("workflow binding was not recorded by ledger schema v1")
    );
    ledger.close().await;

    let pool = config
        .open_read_write_pool(&path)
        .await
        .expect("migrated database should reopen");
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT version FROM workflow_ledger_schema WHERE singleton = 1",
    )
    .fetch_one(&pool)
    .await
    .expect("migrated marker");
    assert_eq!(version, 2);
    pool.close().await;
}

#[tokio::test]
async fn preexisting_tables_without_marker_are_rejected() {
    let temp = TempDir::new().expect("temporary directory");
    let config = sqlite_config(&temp);
    let path = config.home().join(WORKFLOW_LEDGER_DB_FILENAME);
    let pool = config
        .open_read_write_pool(&path)
        .await
        .expect("database should open");
    sqlx::query("CREATE TABLE unexplained_state(id INTEGER PRIMARY KEY)")
        .execute(&pool)
        .await
        .expect("table should be created");
    pool.close().await;

    let error = match WorkflowLedger::open(&config).await {
        Ok(_) => panic!("markerless database must fail"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        WorkflowLedgerError::MissingSchemaMarker { table_count: 1 }
    ));
}

#[tokio::test]
async fn cancellation_request_blocks_new_host_calls() {
    let temp = TempDir::new().expect("temporary directory");
    let ledger = open_test_ledger(&temp).await;
    let run_id = WorkflowRunId::new("run-1").expect("valid run id");
    ledger
        .accept_run(run_request("run-1", json!({})))
        .await
        .expect("run should be accepted");
    ledger
        .mark_run_running(&run_id)
        .await
        .expect("run should start");
    let cancelled = ledger
        .request_cancel(&run_id, Some("user requested stop"))
        .await
        .expect("cancellation request should persist");
    assert_eq!(cancelled.status, WorkflowRunStatus::Running);
    assert_eq!(
        cancelled.cancel_reason.as_deref(),
        Some("user requested stop")
    );

    let request = json!({"prompt": "must not dispatch"});
    let error = ledger
        .accept_call(AcceptWorkflowCall {
            run_id,
            identity: WorkflowCallIdentity::new("call-after-cancel", &request)
                .expect("valid call identity"),
            operation: "agent".to_string(),
            request,
        })
        .await
        .expect_err("new calls must not start after cancellation");
    assert!(matches!(
        error,
        WorkflowLedgerError::RunCancellationRequested { .. }
    ));
}
