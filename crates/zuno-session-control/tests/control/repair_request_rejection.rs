use super::*;

const NEXT: &str = "msg_after_rejected_recovery";
const RECOVERY_TURN: &str = "turn_rejected_recovery";
const RECOVERY_REQUEST: &str = "request_rejected_recovery";

fn append_gated_input(fixture: &mut RejectionFixture, id: &str, at_ms: i64) {
    let tx = fixture.base.connection.transaction().expect("writer");
    promote_user(&tx, id, at_ms);
    let state = session_execution::read_in(&tx, SESSION).unwrap().unwrap();
    input_receipt::record_execution_gate_in(
        &tx,
        SESSION,
        id,
        &InputExecutionGate {
            reason: InputGateReason::Blocked,
            recovery: InputGateRecovery::InspectSession,
            execution_revision: state.revision,
            cycle_id: format!("input_{id}"),
            request_id: None,
            source_id: None,
        },
        at_ms + 1,
    )
    .unwrap();
    fixture.base.revision = state.revision;
    tx.commit().unwrap();
}

fn repair_latest(
    fixture: &mut RejectionFixture,
    id: &str,
    action: SessionRepairAction,
) -> Result<SessionRepairReport, SessionRepairError> {
    SessionRepairService::new(&mut fixture.base.connection, &fixture.base.runs).repair(
        SessionRepairRequest {
            session_id: SESSION,
            input_id: id,
            action,
            at_ms: 1000,
        },
    )
}

#[test]
fn consecutive_unapplied_gate_inheritance_recovers_latest_input_once() {
    for count in [1, 3] {
        let mut fixture = RejectionFixture::new();
        let mut latest = String::new();
        for index in 0..count {
            latest = format!("msg_gate_bridge_{index}");
            append_gated_input(&mut fixture, &latest, 80 + index * 10);
        }
        let before = snapshot(&fixture.base.connection);
        let inspected = repair_latest(&mut fixture, &latest, SessionRepairAction::Inspect)
            .expect("consecutive gate-only inputs must preserve provable rejection ownership");
        assert_eq!(inspected.disposition, SessionRepairDisposition::Eligible);
        assert_eq!(inspected.evidence.inherited_input_ids.len(), count as usize);
        assert_eq!(inspected.evidence.inherited_input_ids[0], NEXT);
        assert_eq!(snapshot(&fixture.base.connection), before);
        let action = SessionRepairAction::Apply {
            expected_revision: fixture.base.revision,
        };
        let applied =
            repair_latest(&mut fixture, &latest, action).expect("guarded latest recovery");
        assert_eq!(applied.disposition, SessionRepairDisposition::ControlQueued);
        let after = snapshot(&fixture.base.connection);
        let repeated = repair_latest(&mut fixture, &latest, action).expect("idempotent retry");
        assert_eq!(
            repeated.disposition,
            SessionRepairDisposition::AlreadyQueued
        );
        assert_eq!(repeated.control_input_id, applied.control_input_id);
        assert_eq!(snapshot(&fixture.base.connection), after);
        let old = input_receipt::get_in(&fixture.base.connection, SESSION, NEXT)
            .unwrap()
            .unwrap();
        assert!(old.turn_id.is_none() && old.applied_at.is_none());
        assert_eq!(
            old.state,
            zuno_types::admission::InputReceiptState::Recorded
        );
        for (table, rows) in before {
            if !matches!(
                table.as_str(),
                "event"
                    | "event_sequence"
                    | "session_input"
                    | "session_input_receipt"
                    | "session_execution_state"
                    | "session_work_cycle"
            ) {
                assert_eq!(
                    after.iter().find(|(name, _)| name == &table).unwrap().1,
                    rows,
                    "{table}"
                );
            }
        }
    }
}

#[test]
fn inherited_gate_chain_rejects_changed_provenance_and_execution() {
    for mutation in [
        "UPDATE session_input_receipt SET turn_id='unexpected-turn' WHERE input_id='msg_after_rejected_recovery'",
        "UPDATE session_input_receipt SET state='failed' WHERE input_id='msg_after_rejected_recovery'",
        "UPDATE session_input SET prompt='{\"kind\":\"acpPrompt\",\"text\":\"changed\"}' WHERE id='msg_after_rejected_recovery'",
        "UPDATE event SET data=json_set(data,'$.gate.reason','authentication')
         WHERE type='session.input.execution_gate.1' AND json_extract(data,'$.inputId')='msg_after_rejected_recovery'",
        "UPDATE event SET data=json_set(data,'$.executionGate.executionRevision',99)
         WHERE type='session.input.receipt.1' AND json_extract(data,'$.inputId')='msg_after_rejected_recovery'",
        "UPDATE event SET data=json_set(data,'$.previousCycleId','wrong-cycle')
         WHERE type='session.work_cycle.started.1' AND json_extract(data,'$.inputId')='msg_gate_tail'",
        "UPDATE event SET data=json_set(data,'$.protectedGateRetained',json('false'))
         WHERE type='session.work_cycle.started.1' AND json_extract(data,'$.inputId')='msg_gate_tail'",
        "UPDATE event SET type='session.input.promoted.2'
         WHERE type='session.input.promoted.1' AND json_extract(data,'$.inputID')='msg_gate_tail'",
        "UPDATE event SET type='session.turn.started.1'
         WHERE type='session.input.promoted.1' AND json_extract(data,'$.inputID')='msg_gate_tail'",
        "UPDATE session_work_cycle SET data=json_set(data,'$.activeTurnId','unexpected-turn')
         WHERE cycle_id='input_msg_after_rejected_recovery'",
        "DELETE FROM message WHERE id='msg_gate_tail'",
    ] {
        let mut fixture = RejectionFixture::new();
        append_gated_input(&mut fixture, "msg_gate_tail", 80);
        fixture.base.connection.execute_batch(mutation).unwrap();
        let before = snapshot(&fixture.base.connection);
        let action = SessionRepairAction::Apply { expected_revision: fixture.base.revision };
        assert!(
            repair_latest(&mut fixture, "msg_gate_tail", action).is_err(),
            "unsafe mutation was accepted: {mutation}"
        );
        assert_eq!(snapshot(&fixture.base.connection), before);
    }
}

#[test]
fn inherited_gate_chain_is_bounded_and_rejects_late_controls() {
    let mut fixture = RejectionFixture::new();
    for index in 0..17 {
        append_gated_input(
            &mut fixture,
            &format!("msg_long_gate_{index}"),
            80 + index * 10,
        );
    }
    let before = snapshot(&fixture.base.connection);
    assert!(matches!(
        repair_latest(
            &mut fixture,
            "msg_long_gate_16",
            SessionRepairAction::Inspect
        ),
        Err(SessionRepairError::Rejected(
            SessionRepairRejection::EvidenceLimit
        ))
    ));
    assert_eq!(snapshot(&fixture.base.connection), before);

    let mut fixture = RejectionFixture::new();
    append_gated_input(&mut fixture, "msg_gate_tail", 80);
    let tx = fixture.base.connection.transaction().unwrap();
    append(
        &tx,
        "session.interrupted",
        json!({"reason":"explicit_stop"}),
    );
    tx.commit().unwrap();
    let before = snapshot(&fixture.base.connection);
    let action = SessionRepairAction::Apply {
        expected_revision: fixture.base.revision,
    };
    assert!(repair_latest(&mut fixture, "msg_gate_tail", action).is_err());
    assert_eq!(snapshot(&fixture.base.connection), before);
}

struct RejectionFixture {
    base: RepairFixture,
    control: String,
    cycle: String,
}

impl RejectionFixture {
    fn new() -> Self {
        Self::with_goal(true)
    }

    fn with_goal(with_goal: bool) -> Self {
        let mut base = RepairFixture::new();
        if !with_goal {
            base.connection
                .execute("DELETE FROM work_plan WHERE session_id=?1", [SESSION])
                .expect("independent ordinary session");
            base.connection
                .execute("DELETE FROM goal WHERE session_id=?1", [SESSION])
                .expect("no Goal");
        }
        // Use the actual old repair operation, including its frozen proof and
        // typed authorization; never synthesize an "authorized" flag alone.
        let repaired = base.apply().expect("existing guarded legacy repair");
        let control = repaired.control_input_id.expect("control");
        let cycle = repaired.recovery_cycle_id.expect("recovery cycle");
        let tx = base.connection.transaction().expect("writer");
        let admitted = inbox::read_in(&tx, SESSION, &control)
            .expect("control")
            .expect("control");
        inbox::admit_and_promote_in(
            &tx,
            NewSessionInput::new(
                &control,
                SESSION,
                admitted.prompt,
                InputDelivery::Queue,
                admitted.time_created,
            )
            .with_source_key(admitted.source_key.expect("idempotent control"))
            .with_trigger_kind(InputTriggerKind::UserControl)
            .with_cycle_id(Some(cycle.clone())),
        )
        .expect("promote existing control without duplicate admission");
        inbox::mark_consumed_in(&tx, SESSION, &control)
            .expect("consume")
            .expect("consumed");
        let mut bound = session_work_cycle::read_in(&tx, SESSION, &cycle)
            .expect("cycle")
            .expect("cycle");
        bound.active_turn_id = Some(RECOVERY_TURN.to_owned());
        session_work_cycle::save_in(&tx, &bound, 50).expect("bind recovery turn");
        input_receipt::bind_turn_in(
            &tx,
            SESSION,
            &[INPUT.to_owned(), control.clone()],
            RECOVERY_TURN,
            51,
        )
        .expect("bind saved input and control, without marking them applied");
        append(
            &tx,
            "session.turn.started",
            json!({"cycleID":cycle,"turnID":RECOVERY_TURN,"anchorMessageID":INPUT,
                "turnTrigger":"user_control","userControl":"resume_work"}),
        );
        append(
            &tx,
            "session.provider.request",
            json!({"requestID":RECOVERY_REQUEST,"turnID":RECOVERY_TURN,
                "status":"started","inputIDs":[]}),
        );
        append(
            &tx,
            "session.provider.attempt",
            json!({"requestID":RECOVERY_REQUEST,"turnID":RECOVERY_TURN,
                "cycleID":cycle,"turnTrigger":"user_control","status":"started"}),
        );
        let diagnostic = json!({"status":400,"code":"reasoning_replay_context_mismatch",
            "reason":"Private response text must not authorize repair."});
        append(
            &tx,
            "session.provider.attempt",
            json!({"requestID":RECOVERY_REQUEST,"turnID":RECOVERY_TURN,
                "cycleID":cycle,"turnTrigger":"user_control","status":"failed",
                "turnErrorKind":"provider","errorKind":"fatal","retryable":false,
                "providerDiagnostic":diagnostic}),
        );
        append(
            &tx,
            "session.provider.request",
            json!({"requestID":RECOVERY_REQUEST,"turnID":RECOVERY_TURN,"status":"failed",
                "errorKind":"provider","providerDiagnostic":diagnostic}),
        );
        input_receipt::finish_turn_in(
            &tx,
            SESSION,
            RECOVERY_TURN,
            None,
            Some("Private old request failure"),
            52,
        )
        .expect("failed old input is terminal even without appliedAt");
        let state = session_execution::read_in(&tx, SESSION)
            .expect("state")
            .expect("state");
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
            53,
        )
        .expect("released request rejection became persistent blocked");
        append(
            &tx,
            "session.turn.failure_settled",
            json!({"scope":{"cycleId":cycle,"turnId":RECOVERY_TURN,"goalId":null},
                "effectiveGoalId":null,"retryable":false,"ordinaryStopped":false,
                "category":"Private rendered failure is not authority."}),
        );
        promote_user(&tx, NEXT, 60);
        let state = session_execution::read_in(&tx, SESSION)
            .expect("state")
            .expect("state");
        input_receipt::record_execution_gate_in(
            &tx,
            SESSION,
            NEXT,
            &InputExecutionGate {
                reason: InputGateReason::Blocked,
                recovery: InputGateRecovery::InspectSession,
                execution_revision: state.revision,
                cycle_id: format!("input_{NEXT}"),
                request_id: None,
                source_id: None,
            },
            61,
        )
        .expect("new input inherits blocked");
        base.revision = state.revision;
        tx.commit().expect("fixture");
        Self {
            base,
            control,
            cycle,
        }
    }

    fn run(
        &mut self,
        action: SessionRepairAction,
    ) -> Result<SessionRepairReport, SessionRepairError> {
        SessionRepairService::new(&mut self.base.connection, &self.base.runs).repair(
            SessionRepairRequest {
                session_id: SESSION,
                input_id: NEXT,
                action,
                at_ms: 70,
            },
        )
    }

    fn apply(&mut self) -> Result<SessionRepairReport, SessionRepairError> {
        self.run(SessionRepairAction::Apply {
            expected_revision: self.base.revision,
        })
    }

    fn reject(&mut self, expected: SessionRepairRejection) {
        let before = snapshot(&self.base.connection);
        let result = self.apply();
        assert!(
            matches!(result, Err(SessionRepairError::Rejected(found)) if found == expected),
            "{result:?}"
        );
        assert_eq!(snapshot(&self.base.connection), before);
    }

    fn mutate(&self, kind: &str, path: &str, value: Value) {
        self.base
            .connection
            .execute(
                "UPDATE event SET data=json_set(data,?3,json(?4))
             WHERE aggregate_id=?1 AND type=?2 AND seq>
              (SELECT max(seq) FROM event WHERE aggregate_id=?1
               AND type='session.repair.legacy_false_blocked_applied.1')",
                params![SESSION, format!("{kind}.1"), path, value.to_string()],
            )
            .expect("anonymous changed evidence");
    }
}

#[test]
fn request_rejection_repair_inspects_and_recovers_only_the_new_never_applied_input() {
    let mut fixture = RejectionFixture::new();
    let before = snapshot(&fixture.base.connection);
    let report = fixture.run(SessionRepairAction::Inspect).expect(
        "proven request-local replay rejection must not permanently block the next saved input",
    );
    assert_eq!(report.disposition, SessionRepairDisposition::Eligible);
    assert_eq!(report.evidence.faulty_cycle_id, fixture.cycle);
    assert_eq!(report.evidence.faulty_turn_id, RECOVERY_TURN);
    assert_eq!(snapshot(&fixture.base.connection), before);
    let old_rows: Vec<_> = [OLD, INPUT, fixture.control.as_str()]
        .into_iter()
        .map(|id| {
            (
                id.to_owned(),
                inbox::read_in(&fixture.base.connection, SESSION, id).expect("input"),
                input_receipt::get_in(&fixture.base.connection, SESSION, id).expect("receipt"),
            )
        })
        .collect();
    let applied = fixture.apply().expect("explicit, validated apply");
    assert_eq!(applied.disposition, SessionRepairDisposition::ControlQueued);
    assert_eq!(applied.execution_revision, fixture.base.revision + 1);
    for (id, input, receipt) in old_rows {
        assert_eq!(
            inbox::read_in(&fixture.base.connection, SESSION, &id).expect("input"),
            input
        );
        assert_eq!(
            input_receipt::get_in(&fixture.base.connection, SESSION, &id).expect("receipt"),
            receipt
        );
    }
    for (name, rows) in before {
        if matches!(name.as_str(), "goal" | "work_plan" | "message" | "part") {
            assert_eq!(
                snapshot(&fixture.base.connection)
                    .into_iter()
                    .find(|(table, _)| table == &name)
                    .expect("unchanged table")
                    .1,
                rows
            );
        }
    }
    let unchanged = snapshot(&fixture.base.connection);
    let repeated = fixture.apply().expect("idempotent apply");
    assert_eq!(
        repeated.disposition,
        SessionRepairDisposition::AlreadyQueued
    );
    assert_eq!(repeated.control_input_id, applied.control_input_id);
    assert_eq!(snapshot(&fixture.base.connection), unchanged);
    assert!(
        !serde_json::to_string(&repeated)
            .expect("report")
            .contains("Private")
    );
}

#[test]
fn request_rejection_repair_needs_no_goal_and_never_replays_a_failed_recovery_input() {
    let mut fixture = RejectionFixture::with_goal(false);
    fixture.apply().expect("ordinary session without a Goal");
    let before = snapshot(&fixture.base.connection);
    let result = SessionRepairService::new(&mut fixture.base.connection, &fixture.base.runs)
        .repair(SessionRepairRequest {
            session_id: SESSION,
            input_id: INPUT,
            action: SessionRepairAction::Apply {
                expected_revision: fixture.base.revision,
            },
            at_ms: 75,
        });
    assert!(matches!(
        result,
        Err(SessionRepairError::Rejected(
            SessionRepairRejection::IneligibleInput
        ))
    ));
    assert_eq!(snapshot(&fixture.base.connection), before);
    let tx = fixture.base.connection.transaction().expect("writer");
    input_receipt::bind_turn_in(&tx, SESSION, &[NEXT.to_owned()], "actual-new-turn", 76)
        .expect("new input now belongs to one real turn");
    tx.commit().expect("native binding");
    fixture.reject(SessionRepairRejection::IneligibleInput);
}

#[test]
fn request_rejection_repair_requires_the_exact_diagnostic_and_native_failure_scope() {
    for (kind, path, value) in [
        (
            "session.provider.request",
            "$.providerDiagnostic.status",
            json!(401),
        ),
        (
            "session.provider.request",
            "$.providerDiagnostic.status",
            json!(503),
        ),
        (
            "session.provider.request",
            "$.providerDiagnostic.code",
            json!("invalid_api_key"),
        ),
        (
            "session.provider.request",
            "$.providerDiagnostic.code",
            Value::Null,
        ),
        (
            "session.provider.attempt",
            "$.providerDiagnostic.code",
            json!("invalid_config"),
        ),
        ("session.provider.attempt", "$.retryable", json!(true)),
        ("session.provider.attempt", "$.turnID", json!("other-turn")),
        (
            "session.turn.failure_settled",
            "$.effectiveGoalId",
            json!("old-goal"),
        ),
        (
            "session.turn.failure_settled",
            "$.scope.cycleId",
            json!("other-cycle"),
        ),
        (
            "session.turn.failure_settled",
            "$.ordinaryStopped",
            json!(true),
        ),
        ("session.turn.started", "$.turnTrigger", json!("automatic")),
    ] {
        let mut fixture = RejectionFixture::new();
        fixture.mutate(kind, path, value);
        fixture.reject(SessionRepairRejection::Unproven);
    }
}

#[test]
fn request_rejection_repair_keeps_auth_goal_approval_uncertainty_and_lease_gates() {
    let mut fixture = RejectionFixture::new();
    fixture.base.connection.execute(
        "UPDATE session_execution_state SET scheduling=json_set(scheduling,'$.readiness.reason','authentication') WHERE session_id=?1",
        [SESSION],
    ).expect("auth gate");
    fixture.reject(SessionRepairRejection::ProtectedGate);
    let mut fixture = RejectionFixture::new();
    fixture
        .base
        .connection
        .execute(
            "UPDATE goal SET status='paused' WHERE session_id=?1",
            [SESSION],
        )
        .expect("paused Goal");
    fixture.reject(SessionRepairRejection::ProtectedGate);
    let mut fixture = RejectionFixture::new();
    fixture.base.connection.execute(
        "INSERT INTO human_request(id,session_id,kind,state,payload,revision,time_created,time_updated)
         VALUES('permission-replay',?1,'permission','pending','{}',1,65,65)", [SESSION],
    ).expect("approval");
    fixture.reject(SessionRepairRejection::ProtectedGate);
    let mut fixture = RejectionFixture::new();
    fixture
        .base
        .connection
        .execute(
            "INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
         VALUES('uncertain-replay',?1,?2,65,65,?3)",
            params![
                INPUT,
                SESSION,
                json!({"type":"tool","tool":"write","state":{"status":"running"}}).to_string()
            ],
        )
        .expect("uncertain call");
    fixture.reject(SessionRepairRejection::Uncertainty);
    let mut fixture = RejectionFixture::new();
    let lease = fixture.base.runs.begin_turn(SESSION).expect("live turn");
    fixture.reject(SessionRepairRejection::ActiveLease);
    drop(lease);
}

#[test]
fn request_rejection_repair_rejects_late_events_and_changed_original_authorization() {
    let mut fixture = RejectionFixture::new();
    let tx = fixture.base.connection.transaction().expect("writer");
    append(
        &tx,
        "session.execution.paused",
        json!({"reason":"authentication"}),
    );
    tx.commit().expect("late event");
    fixture.reject(SessionRepairRejection::LateEvent);

    let mut fixture = RejectionFixture::new();
    fixture
        .base
        .connection
        .execute(
            "UPDATE event SET data=json_set(data,'$.cycle.goalId','unexpected-goal')
         WHERE aggregate_id=?1 AND type='session.work_cycle.authorized.1'",
            [SESSION],
        )
        .expect("changed authorization");
    fixture.reject(SessionRepairRejection::Unproven);

    let mut fixture = RejectionFixture::new();
    fixture
        .base
        .connection
        .execute(
            "UPDATE event SET data=json_set(data,'$.errorKind','authentication')
         WHERE aggregate_id=?1 AND type='session.provider.request.1'
         AND json_extract(data,'$.requestID')=?2",
            params![SESSION, REQUEST],
        )
        .expect("changed original 503 proof");
    fixture.reject(SessionRepairRejection::Unproven);
}
