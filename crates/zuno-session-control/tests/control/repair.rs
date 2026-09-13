use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};
use zuno_db::event_log::{NewSessionEvent, append_in};
use zuno_db::inbox::{self, InputDelivery, NewSessionInput, SubmissionState};
use zuno_db::{input_receipt, session_execution, session_work_cycle};
use zuno_engine::status::SessionRunRegistry;
use zuno_session_control::SessionControlService;
use zuno_session_control::repair::{
    SessionRepairAction, SessionRepairDisposition, SessionRepairError, SessionRepairRejection,
    SessionRepairReport, SessionRepairRequest, SessionRepairService,
};
use zuno_types::admission::{InputExecutionGate, InputGateReason, InputGateRecovery};
use zuno_types::execution::{
    CollaborationMode, InputTriggerKind, SessionPauseReason, SessionReadiness, SessionScheduling,
    TurnExecutionIdentity,
};

const SESSION: &str = "ses_repair_fixture";
const OLD: &str = "msg_failed_fixture";
const INPUT: &str = "msg_saved_fixture";
const TURN: &str = "turn_failed_fixture";
const REQUEST: &str = "request_failed_fixture";
const FORMAT_14: &str = concat!(
    include_str!("../../../zuno-db/tests/fixtures/format-7.sql"),
    include_str!("../../../zuno-db/tests/fixtures/format-8.sql"),
    include_str!("../../../zuno-db/tests/fixtures/format-9.sql"),
    include_str!("../../../zuno-db/tests/fixtures/format-10.sql"),
    include_str!("../../../zuno-db/tests/fixtures/format-11.sql"),
    include_str!("../../../zuno-db/tests/fixtures/format-12.sql"),
    include_str!("../../../zuno-db/tests/fixtures/format-13.sql"),
    include_str!("../../../zuno-db/tests/fixtures/format-14.sql")
);
const LEGACY_GOAL: &str = include_str!("../../../zuno-db/tests/fixtures/legacy-goal-format14.sql");

struct RepairFixture {
    connection: Connection,
    runs: SessionRunRegistry,
    revision: i64,
}

impl RepairFixture {
    fn new() -> Self {
        Self::with_connection(Connection::open_in_memory().expect("connection"))
    }

    fn with_connection(mut connection: Connection) -> Self {
        // Frozen released DDL and representative Session/Message/Memory/Goal
        // rows. Setup alone migrates; the service must preserve format 15.
        connection
            .execute_batch(FORMAT_14)
            .expect("published format 14");
        connection
            .execute_batch(LEGACY_GOAL)
            .expect("published optional Goal tables");
        zuno_db::migration::apply(&mut connection).expect("fixture upgrade");
        connection.execute_batch(
            "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
             VALUES('repair-project','/anonymous',1,1,'[]');
             INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
             VALUES('ses_repair_fixture','repair-project','repair','/anonymous','Anonymous','fixture',1,1);
             INSERT INTO goal(session_id,goal_id,revision,objective,success_criteria,status,
               token_budget,tokens_used,usage_known,time_used_seconds,created_at_ms,updated_at_ms)
             VALUES('ses_repair_fixture','completed-fixture-goal',7,'Old work','[]','complete',10000,731,1,42,1,10);
             INSERT INTO work_plan(session_id,id,goal_id,revision,title,steps,time_created,time_updated)
             VALUES('ses_repair_fixture','old-fixture-plan','completed-fixture-goal',3,'Historical work',
               '[{\"id\":\"old-step\",\"title\":\"Old step\",\"status\":\"pending\"}]',1,10);"
        ).expect("anonymous session and completed Goal");
        let tx = connection.transaction().expect("fixture writer");
        promote_user(&tx, OLD, 20);
        let old_cycle_id = format!("input_{OLD}");
        let mut old_cycle = session_work_cycle::read_in(&tx, SESSION, &old_cycle_id)
            .expect("cycle")
            .expect("cycle");
        old_cycle.active_turn_id = Some(TURN.to_owned());
        session_work_cycle::save_in(&tx, &old_cycle, 21).expect("bind actual old turn");
        append(
            &tx,
            "session.turn.started",
            json!({
                "turnID":TURN,"turnTrigger":"user","anchorMessageID":OLD,
            }),
        );
        append(
            &tx,
            "session.provider.request",
            json!({
                "requestID":REQUEST,"turnID":TURN,"status":"started","inputIDs":[OLD],
            }),
        );
        append(
            &tx,
            "session.context.usage",
            json!({"snapshot":{"known":false}}),
        );
        input_receipt::mark_applied_in(&tx, SESSION, &[OLD.to_owned()], TURN, 22)
            .expect("the failed old input really reached the provider");
        append(
            &tx,
            "session.provider.attempt",
            json!({
                "requestID":REQUEST,"turnID":TURN,"turnTrigger":"user","status":"failed",
                "turnErrorKind":"provider","errorKind":"transient","retryable":true,
            }),
        );
        append(
            &tx,
            "session.provider.attempt",
            json!({
                "requestID":REQUEST,"turnID":TURN,"turnTrigger":"user","status":"failed",
                "turnErrorKind":"provider_retry_deadline","retryable":true,
            }),
        );
        append(
            &tx,
            "session.provider.request",
            json!({
                "requestID":REQUEST,"turnID":TURN,"status":"failed","errorKind":"provider_retry_deadline",
                "message":"Private fixture error text is not repair authority.",
            }),
        );
        append(
            &tx,
            "session.context.usage",
            json!({"snapshot":{"known":false}}),
        );
        input_receipt::finish_turn_in(&tx, SESSION, TURN, None, Some("Anonymous old failure"), 23)
            .expect("settle applied old input");
        let state = session_execution::read_in(&tx, SESSION)
            .expect("state")
            .expect("state");
        // Reproduce the released misclassification using its typed durable
        // shape. The corrected live failure classifier must never create it.
        session_execution::set_scheduling_in(
            &tx,
            SESSION,
            state.revision,
            SessionScheduling {
                readiness: SessionReadiness::Paused {
                    reason: SessionPauseReason::Blocked,
                },
                ..SessionScheduling::default()
            },
            24,
        )
        .expect("legacy false block");
        promote_user(&tx, INPUT, 30);
        let state = session_execution::read_in(&tx, SESSION)
            .expect("state")
            .expect("state");
        input_receipt::record_execution_gate_in(
            &tx,
            SESSION,
            INPUT,
            &InputExecutionGate {
                reason: InputGateReason::Blocked,
                recovery: InputGateRecovery::InspectSession,
                execution_revision: state.revision,
                cycle_id: format!("input_{INPUT}"),
                request_id: None,
                source_id: None,
            },
            31,
        )
        .expect("native gate receipt");
        let revision = state.revision;
        tx.commit().expect("fixture");
        Self {
            connection,
            runs: SessionRunRegistry::new(),
            revision,
        }
    }

    fn run(
        &mut self,
        action: SessionRepairAction,
    ) -> Result<SessionRepairReport, SessionRepairError> {
        SessionRepairService::new(&mut self.connection, &self.runs).repair(SessionRepairRequest {
            session_id: SESSION,
            input_id: INPUT,
            action,
            at_ms: 40,
        })
    }

    fn apply(&mut self) -> Result<SessionRepairReport, SessionRepairError> {
        self.run(SessionRepairAction::Apply {
            expected_revision: self.revision,
        })
    }

    fn mutate_event(&self, kind: &str, path: &str, value: Value) {
        self.connection
            .execute(
                "UPDATE event SET data=json_set(data,?3,json(?4))
             WHERE aggregate_id=?1 AND type=?2",
                params![SESSION, format!("{kind}.1"), path, value.to_string()],
            )
            .expect("anonymous changed evidence");
    }

    fn reject_unchanged(&mut self, reason: SessionRepairRejection) {
        let before = snapshot(&self.connection);
        let result = self.apply();
        assert!(
            matches!(result, Err(SessionRepairError::Rejected(found)) if found == reason),
            "{result:?}"
        );
        assert_eq!(
            snapshot(&self.connection),
            before,
            "rejection must roll back every row and event"
        );
    }
}

fn append(tx: &Transaction<'_>, kind: &str, data: Value) {
    append_in(
        tx,
        SESSION,
        NewSessionEvent::new(kind, data.as_object().expect("object").clone()).expect("event"),
    )
    .expect("append");
}

fn promote_user(tx: &Transaction<'_>, id: &str, at_ms: i64) {
    inbox::admit_and_promote_in(
        tx,
        NewSessionInput::new(
            id,
            SESSION,
            json!({"kind":"acpPrompt","text":"Private anonymous input retained exactly."}),
            InputDelivery::Steer,
            at_ms,
        )
        .with_trigger_kind(InputTriggerKind::Legacy),
    )
    .expect("admit and promote real user input");
    tx.execute(
        "INSERT INTO message(id,session_id,time_created,time_updated,data) VALUES(?1,?2,?3,?3,?4)",
        params![
            id,
            SESSION,
            at_ms,
            json!({"id":id,"sessionID":SESSION,"role":"user"}).to_string()
        ],
    )
    .expect("saved user message");
    SessionControlService::activate_user_input_in(
        tx,
        SESSION,
        id,
        id,
        CollaborationMode::Work,
        TurnExecutionIdentity::new("build", "fixture", "fixture"),
        at_ms,
    )
    .expect("real user cycle");
    inbox::mark_consumed_in(tx, SESSION, id)
        .expect("consume")
        .expect("consumed");
}

/// Complete fixture snapshot, including schema, sequences, Goal, Plan and Memory.
fn snapshot(connection: &Connection) -> Vec<(String, Vec<Vec<String>>)> {
    let mut names = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("tables");
    let mut tables = names
        .query_map([], |row| row.get::<_, String>(0))
        .expect("names")
        .collect::<Result<Vec<_>, _>>()
        .expect("names");
    tables.push("sqlite_master".to_owned());
    tables
        .into_iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{table}\""))
                .expect("fixture table");
            let columns = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|i| row.get_ref(i).map(|value| format!("{value:?}")))
                        .collect::<Result<Vec<_>, _>>()
                })
                .expect("rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("rows");
            rows.sort();
            (table, rows)
        })
        .collect()
}

#[test]
fn native_repair_defaults_to_read_only_inspection_of_published_evidence() {
    let mut fixture = RepairFixture::new();
    let before = snapshot(&fixture.connection);
    fixture
        .connection
        .pragma_update(None, "query_only", true)
        .expect("read only");
    let report = SessionRepairService::new(&mut fixture.connection, &fixture.runs)
        .repair(SessionRepairRequest::new(SESSION, INPUT))
        .expect("inspect");
    assert_eq!(report.disposition, SessionRepairDisposition::Eligible);
    assert_eq!(report.expected_revision, fixture.revision);
    assert!(report.control_input_id.is_none());
    assert_eq!(snapshot(&fixture.connection), before);
    let text = serde_json::to_string(&report).expect("report");
    assert!(!text.contains("Private"));
    assert!(!text.contains("Anonymous old failure"));
}

#[test]
fn repair_rebinds_only_saved_input_and_admits_one_independent_native_control() {
    let mut fixture = RepairFixture::new();
    let before = snapshot(&fixture.connection);
    let old_input = inbox::read_in(&fixture.connection, SESSION, OLD).expect("old input");
    let old_receipt =
        input_receipt::get_in(&fixture.connection, SESSION, OLD).expect("old receipt");
    let input = inbox::read_in(&fixture.connection, SESSION, INPUT)
        .expect("input")
        .expect("input");
    let receipt = input_receipt::get_in(&fixture.connection, SESSION, INPUT).expect("receipt");
    let report = fixture.apply().expect("guarded apply");
    assert_eq!(report.disposition, SessionRepairDisposition::ControlQueued);
    assert_eq!(report.execution_revision, fixture.revision + 1);
    let retained = inbox::read_in(&fixture.connection, SESSION, INPUT)
        .expect("input")
        .expect("input");
    assert_eq!(retained.state, SubmissionState::Consumed);
    assert_eq!(retained.prompt, input.prompt);
    assert_eq!(retained.id, input.id);
    assert_eq!(retained.admitted_sequence, input.admitted_sequence);
    assert_eq!(retained.promoted_sequence, input.promoted_sequence);
    assert_eq!(retained.revision, input.revision + 1);
    assert_eq!(retained.cycle_id, report.recovery_cycle_id);
    assert_ne!(retained.cycle_id, input.cycle_id);
    assert_eq!(
        input_receipt::get_in(&fixture.connection, SESSION, INPUT).expect("receipt"),
        receipt
    );
    assert_eq!(
        inbox::read_in(&fixture.connection, SESSION, OLD).expect("old input"),
        old_input
    );
    assert_eq!(
        input_receipt::get_in(&fixture.connection, SESSION, OLD).expect("old receipt"),
        old_receipt
    );
    let control = inbox::read_in(
        &fixture.connection,
        SESSION,
        report.control_input_id.as_deref().expect("control"),
    )
    .expect("control")
    .expect("control");
    assert_eq!(control.state, SubmissionState::Queued);
    assert_eq!(control.trigger_kind, InputTriggerKind::UserControl);
    assert_eq!(control.prompt["control"], "resume_work");
    assert_eq!(control.prompt["continuation"]["anchorMessageId"], INPUT);
    let cycle = session_work_cycle::current_in(&fixture.connection, SESSION)
        .expect("cycle")
        .expect("cycle");
    assert!(cycle.goal_id.is_none());
    assert!(cycle.plan_id.is_none());
    assert!(cycle.resumed_goal_cycles.is_empty());
    for origin in [format!("input_{OLD}"), format!("input_{INPUT}")] {
        assert!(
            session_work_cycle::completion_cycle_in(&fixture.connection, SESSION, &origin)
                .expect("late report authority")
                .is_none()
        );
    }
    let after = snapshot(&fixture.connection);
    for (name, values) in &before {
        if !matches!(
            name.as_str(),
            "event"
                | "event_sequence"
                | "session_input"
                | "session_input_receipt"
                | "session_execution_state"
                | "session_work_cycle"
        ) {
            assert_eq!(
                after
                    .iter()
                    .find(|(table, _)| table == name)
                    .expect("table")
                    .1,
                *values,
                "{name}"
            );
        }
    }
}

#[test]
fn exact_apply_retry_is_idempotent_without_requeue_or_second_control() {
    let mut fixture = RepairFixture::new();
    let first = fixture.apply().expect("first");
    let before = snapshot(&fixture.connection);
    let repeated = fixture.apply().expect("duplicate exact revision");
    assert_eq!(
        repeated.disposition,
        SessionRepairDisposition::AlreadyQueued
    );
    assert_eq!(first.control_input_id, repeated.control_input_id);
    assert_eq!(first.recovery_cycle_id, repeated.recovery_cycle_id);
    assert_eq!(snapshot(&fixture.connection), before);
    let inspected = fixture
        .run(SessionRepairAction::Inspect)
        .expect("inspect committed repair");
    assert_eq!(
        inspected.disposition,
        SessionRepairDisposition::AlreadyQueued
    );
    assert_eq!(snapshot(&fixture.connection), before);
    fixture.revision += 1;
    fixture.reject_unchanged(SessionRepairRejection::Changed);
}

#[test]
fn repair_rejects_wrong_execution_revision_without_mutation() {
    let mut fixture = RepairFixture::new();
    fixture.revision += 1;
    fixture.reject_unchanged(SessionRepairRejection::Changed);
}

#[test]
fn repair_rechecks_input_revision_cycle_and_protected_provenance() {
    let mut fixture = RepairFixture::new();
    fixture
        .connection
        .execute(
            "UPDATE session_input SET revision=revision+1 WHERE id=?1",
            [INPUT],
        )
        .expect("changed input");
    fixture.reject_unchanged(SessionRepairRejection::Changed);

    let mut fixture = RepairFixture::new();
    fixture
        .connection
        .execute(
            "UPDATE session_input SET cycle_id='different-cycle' WHERE id=?1",
            [INPUT],
        )
        .expect("changed cycle");
    fixture.reject_unchanged(SessionRepairRejection::Changed);

    for (path, value) in [
        ("$.protectedGateRetained", json!(false)),
        ("$.cycle.goalId", json!("old-goal")),
        ("$.cycle.planId", json!("old-plan")),
        ("$.previousCycleId", json!("unknown-cycle")),
    ] {
        let mut fixture = RepairFixture::new();
        fixture.connection.execute(
            "UPDATE event SET data=json_set(data,?2,json(?3))
             WHERE aggregate_id=?1 AND type='session.work_cycle.started.1' AND json_extract(data,'$.inputId')=?4",
            params![SESSION, path, value.to_string(), INPUT],
        ).expect("changed provenance");
        fixture.reject_unchanged(SessionRepairRejection::Unproven);
    }
}

#[test]
fn no_rendered_error_can_substitute_for_structured_provider_deadline_proof() {
    for value in [
        json!("authentication"),
        Value::Null,
        json!("provider"),
        json!("turn_budget"),
    ] {
        let mut fixture = RepairFixture::new();
        fixture.mutate_event(
            "session.provider.request",
            "$.message",
            json!("provider_retry_deadline"),
        );
        fixture.mutate_event("session.provider.request", "$.errorKind", value);
        fixture.reject_unchanged(SessionRepairRejection::Unproven);
    }
}

#[test]
fn missing_explicit_goal_independence_and_ambiguous_cycle_events_fail_closed() {
    let mut fixture = RepairFixture::new();
    fixture
        .connection
        .execute(
            "UPDATE event SET data=json_remove(data,'$.cycle.goalId')
         WHERE aggregate_id=?1 AND type='session.work_cycle.started.1'",
            [SESSION],
        )
        .expect("missing provenance");
    fixture.reject_unchanged(SessionRepairRejection::Unproven);

    let mut fixture = RepairFixture::new();
    let raw: String = fixture
        .connection
        .query_row(
            "SELECT data FROM event WHERE aggregate_id=?1 AND type='session.work_cycle.started.1'
         AND json_extract(data,'$.inputId')=?2",
            params![SESSION, INPUT],
            |row| row.get(0),
        )
        .expect("cycle event");
    let tx = fixture.connection.transaction().expect("writer");
    append(
        &tx,
        "session.work_cycle.started",
        serde_json::from_str(&raw).expect("event"),
    );
    tx.commit().expect("duplicate event");
    fixture.reject_unchanged(SessionRepairRejection::Unproven);
}

#[test]
fn repair_never_accepts_applied_failed_or_bound_saved_inputs() {
    let mut fixture = RepairFixture::new();
    let before = snapshot(&fixture.connection);
    let error = SessionRepairService::new(&mut fixture.connection, &fixture.runs)
        .repair(SessionRepairRequest {
            session_id: SESSION,
            input_id: OLD,
            action: SessionRepairAction::Apply {
                expected_revision: fixture.revision,
            },
            at_ms: 40,
        })
        .expect_err("applied failed old input");
    assert!(matches!(
        error,
        SessionRepairError::Rejected(SessionRepairRejection::IneligibleInput)
    ));
    assert_eq!(snapshot(&fixture.connection), before);
    for mutation in [
        "UPDATE session_input_receipt SET turn_id='bound-turn' WHERE input_id=?1",
        "UPDATE session_input_receipt SET state='applied',turn_id='bound-turn',applied_at=35 WHERE input_id=?1",
        "UPDATE session_input_receipt SET state='failed',completed_at=35,error='unapplied failure' WHERE input_id=?1",
    ] {
        let mut fixture = RepairFixture::new();
        fixture
            .connection
            .execute(mutation, [INPUT])
            .expect("receipt change");
        fixture.reject_unchanged(SessionRepairRejection::IneligibleInput);
    }
}

#[test]
fn authentication_budget_user_and_plan_gates_require_their_own_controls() {
    for reason in [
        "authentication",
        "turn_budget",
        "user",
        "uncertain_side_effect",
    ] {
        let mut fixture = RepairFixture::new();
        fixture.connection.execute(
            "UPDATE session_execution_state SET scheduling=json_set(scheduling,'$.readiness.reason',?2)
             WHERE session_id=?1", params![SESSION, reason],
        ).expect("real native gate");
        fixture.reject_unchanged(SessionRepairRejection::ProtectedGate);
    }
    let mut fixture = RepairFixture::new();
    fixture
        .connection
        .execute(
        "UPDATE session_execution_state SET mode='plan',phase='planning',
         scheduling='{\"readiness\":{\"kind\":\"ready\"},\"unchangedProgressCount\":0}' WHERE session_id=?1",
            [SESSION],
        )
        .expect("Plan mode");
    fixture.reject_unchanged(SessionRepairRejection::ProtectedGate);
    let mut fixture = RepairFixture::new();
    fixture
        .connection
        .execute(
            "UPDATE work_plan SET goal_id=NULL WHERE session_id=?1",
            [SESSION],
        )
        .expect("independent Plan requires authorization");
    fixture.reject_unchanged(SessionRepairRejection::ProtectedGate);
}

#[test]
fn real_goal_or_permission_gates_cannot_be_cleared_by_legacy_repair() {
    for status in [
        "active",
        "paused",
        "blocked",
        "budget_limited",
        "usage_limited",
    ] {
        let mut fixture = RepairFixture::new();
        fixture
            .connection
            .execute(
                "UPDATE goal SET status=?2 WHERE session_id=?1",
                params![SESSION, status],
            )
            .expect("real Goal gate");
        fixture.reject_unchanged(SessionRepairRejection::ProtectedGate);
    }
    let mut fixture = RepairFixture::new();
    fixture.connection.execute(
        "INSERT INTO human_request(id,session_id,kind,state,payload,revision,time_created,time_updated)
         VALUES('permission-fixture',?1,'permission','pending','{}',1,35,35)", [SESSION],
    ).expect("approval");
    fixture.reject_unchanged(SessionRepairRejection::ProtectedGate);
}

#[test]
fn uncertainty_is_rejected_without_reconciliation() {
    let mut fixture = RepairFixture::new();
    fixture.connection.execute(
        "INSERT INTO part(id,message_id,session_id,time_created,time_updated,data) VALUES('uncertain-part',?1,?2,35,35,?3)",
        params![OLD, SESSION, json!({"type":"tool","tool":"write","state":{
            "status":"error","outcome":"uncertain",
            "uncertain":{"callID":"fixture-call","tool":"write","observedAtMs":35}
        }}).to_string()],
    ).expect("uncertain result");
    fixture.reject_unchanged(SessionRepairRejection::Uncertainty);
}

#[test]
fn active_turn_and_recovery_leases_reject_without_changes() {
    let mut fixture = RepairFixture::new();
    let guard = fixture.runs.begin_turn(SESSION).expect("active lease");
    fixture.reject_unchanged(SessionRepairRejection::ActiveLease);
    drop(guard);
    let guard = fixture
        .runs
        .begin_recovery(SESSION)
        .expect("other recovery writer lease");
    fixture.reject_unchanged(SessionRepairRejection::ActiveLease);
    drop(guard);
    fixture.apply().expect("released lease");
}

#[test]
fn late_events_cannot_reuse_a_stale_dry_run_or_committed_repair() {
    for after_apply in [false, true] {
        let mut fixture = RepairFixture::new();
        fixture.run(SessionRepairAction::Inspect).expect("dry run");
        if after_apply {
            fixture.apply().expect("apply");
        }
        let tx = fixture.connection.transaction().expect("writer");
        append(
            &tx,
            "session.execution.paused",
            json!({"reason":"authentication","cycleId":format!("input_{OLD}")}),
        );
        tx.commit().expect("late native event");
        fixture.reject_unchanged(SessionRepairRejection::LateEvent);
    }
}

#[test]
fn evidence_limits_fail_closed_without_extending_the_classifier() {
    let mut fixture = RepairFixture::new();
    let tx = fixture.connection.transaction().expect("writer");
    for _ in 0..513 {
        append(
            &tx,
            "learning.consolidation.outcome",
            json!({"status":"completed"}),
        );
    }
    tx.commit().expect("bounded history fixture");
    fixture.reject_unchanged(SessionRepairRejection::EvidenceLimit);
}

#[test]
fn a_new_control_or_changed_recovery_receipt_cannot_be_repaired_again() {
    let mut fixture = RepairFixture::new();
    fixture.apply().expect("apply");
    let tx = fixture.connection.transaction().expect("writer");
    input_receipt::mark_applied_in(&tx, SESSION, &[INPUT.to_owned()], "new-turn", 50)
        .expect("real provider application");
    tx.commit().expect("applied");
    fixture.reject_unchanged(SessionRepairRejection::IneligibleInput);
}

#[test]
fn an_apply_retry_rechecks_the_original_input_and_provider_proof() {
    let mut fixture = RepairFixture::new();
    fixture.apply().expect("queued control");
    fixture.connection.execute(
        "UPDATE session_input SET prompt=json_set(prompt,'$.text','different user input') WHERE id=?1",
        [INPUT],
    ).expect("input changed without trusting its revision");
    fixture.reject_unchanged(SessionRepairRejection::Changed);

    let mut fixture = RepairFixture::new();
    fixture.apply().expect("queued control");
    fixture.mutate_event(
        "session.provider.request",
        "$.errorKind",
        json!("authentication"),
    );
    fixture.reject_unchanged(SessionRepairRejection::Unproven);
}

#[test]
fn consumed_control_and_bound_input_cannot_admit_a_second_control() {
    for bind_input in [false, true] {
        let mut fixture = RepairFixture::new();
        let report = fixture.apply().expect("queued control");
        if bind_input {
            let tx = fixture.connection.transaction().expect("writer");
            input_receipt::bind_turn_in(&tx, SESSION, &[INPUT.to_owned()], "recovered-turn", 50)
                .expect("bind recovered input");
            tx.commit().expect("binding");
            fixture.reject_unchanged(SessionRepairRejection::IneligibleInput);
        } else {
            fixture
                .connection
                .execute(
                    "UPDATE session_input SET state='consumed',revision=revision+1 WHERE id=?1",
                    [report.control_input_id.expect("control")],
                )
                .expect("control advanced");
            fixture.reject_unchanged(SessionRepairRejection::Changed);
        }
    }
}

#[test]
fn event_owner_and_a_real_sqlite_writer_reject_without_stealing_ownership() {
    let mut fixture = RepairFixture::new();
    fixture
        .connection
        .execute(
            "UPDATE event_sequence SET owner_id='native-owner' WHERE aggregate_id=?1",
            [SESSION],
        )
        .expect("event owner");
    fixture.reject_unchanged(SessionRepairRejection::ActiveLease);

    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("repair.db");
    let mut fixture = RepairFixture::with_connection(Connection::open(&path).expect("fixture"));
    fixture
        .connection
        .busy_timeout(std::time::Duration::ZERO)
        .expect("bounded lock check");
    let writer = Connection::open(&path).expect("other writer");
    writer
        .execute_batch("BEGIN IMMEDIATE")
        .expect("writer lease");
    fixture.reject_unchanged(SessionRepairRejection::WriterLease);
    writer.execute_batch("ROLLBACK").expect("release writer");
    fixture.apply().expect("apply after writer released");
}

#[test]
fn a_failure_after_rebinding_rolls_back_control_cycle_input_and_execution_together() {
    let mut fixture = RepairFixture::new();
    fixture
        .connection
        .execute_batch(
            "CREATE TRIGGER reject_repair_audit BEFORE INSERT ON event
         WHEN NEW.type='session.repair.legacy_false_blocked_applied.1'
         BEGIN SELECT RAISE(ABORT,'anonymous audit failure'); END;",
        )
        .expect("fixture failure injection");
    let before = snapshot(&fixture.connection);
    assert!(matches!(
        fixture.apply(),
        Err(SessionRepairError::Database(_))
    ));
    assert_eq!(snapshot(&fixture.connection), before);
}

#[test]
fn inspection_refuses_an_older_published_format_without_migrating_it() {
    let mut connection = Connection::open_in_memory().expect("connection");
    connection
        .execute_batch(FORMAT_14)
        .expect("published format 14");
    connection
        .execute_batch(LEGACY_GOAL)
        .expect("published Goal rows");
    let before = snapshot(&connection);
    let runs = SessionRunRegistry::new();
    let error = SessionRepairService::new(&mut connection, &runs)
        .repair(SessionRepairRequest::new(SESSION, INPUT))
        .expect_err("repair is not migration");
    assert!(matches!(
        error,
        SessionRepairError::Rejected(SessionRepairRejection::UnsupportedFormat)
    ));
    assert_eq!(snapshot(&connection), before);
}
