use super::{Fixture, SESSION};
use std::sync::Arc;
use zuno_db::event_log::SessionEventLog;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SubmissionState};
use zuno_db::input_receipt::InputReceiptStore;
use zuno_db::session_execution::SessionExecutionStore;
use zuno_types::admission::{
    InputAdmissionReceipt, InputExecutionGate, InputGateReason, InputGateRecovery,
    InputReceiptState, InputStopReason,
};
use zuno_types::execution::{
    CollaborationMode, InputTriggerKind, SessionExecutionState, SessionPauseReason,
    SessionReadiness, SessionScheduling,
};

const INPUT: &str = "msg_input_gate_a";
const INPUT_CYCLE: &str = "input_cycle";

fn paused_input_before_defer(fixture: &Fixture) -> SessionExecutionState {
    let inbox = SessionInbox::new(Arc::clone(&fixture.pool));
    inbox
        .admit(
            NewSessionInput::new(
                INPUT,
                SESSION,
                serde_json::json!({"kind":"acpPrompt","text":"Apply retained option A."}),
                InputDelivery::Steer,
                10,
            )
            .with_trigger_kind(InputTriggerKind::User)
            .with_cycle_id(Some(INPUT_CYCLE)),
        )
        .expect("admit user A");
    inbox
        .promote_id(SESSION, INPUT)
        .expect("promote user A")
        .expect("promoted input");
    inbox
        .mark_consumed(SESSION, INPUT)
        .expect("record user A")
        .expect("consumed input");
    fixture
        .control
        .record_continuation(
            SESSION,
            INPUT_CYCLE,
            Fixture::identity(),
            CollaborationMode::Work,
            None,
            None,
            Some(INPUT.to_owned()),
            20,
        )
        .expect("record the input cycle and its anchor");
    let state = fixture
        .control
        .state(SESSION)
        .expect("state")
        .expect("state");
    let paused = SessionExecutionStore::new(Arc::clone(&fixture.pool))
        .set_scheduling(
            SESSION,
            state.revision,
            SessionScheduling {
                readiness: SessionReadiness::Paused {
                    reason: SessionPauseReason::User,
                },
                ..SessionScheduling::default()
            },
            21,
        )
        .expect("pause the input cycle");
    let before = receipt(fixture);
    assert_eq!(before.state, InputReceiptState::Recorded);
    assert!(before.execution_gate.is_none());
    assert!(before.turn_id.is_none());
    assert!(before.applied_at.is_none());
    paused
}

fn receipt(fixture: &Fixture) -> InputAdmissionReceipt {
    InputReceiptStore::new(Arc::clone(&fixture.pool))
        .get(SESSION, INPUT)
        .expect("read receipt")
        .expect("input receipt")
}

fn late_defer_preserves_receipt(fixture: &Fixture, at_ms: i64) {
    let before = receipt(fixture);
    // A stale owner may decline to attach a gate. It must not mutate the
    // receipt now owned by the resumed cycle, including after turn binding.
    fixture
        .control
        .defer_input_at_execution_gate(SESSION, INPUT, at_ms)
        .expect("settle the late execution gate observation");
    assert_eq!(receipt(fixture), before);
    // Force the real fallback even if defer observed the gate: an older
    // observation may already have decided to enter this branch.
    let receipts = InputReceiptStore::new(Arc::clone(&fixture.pool));
    assert!(
        !receipts
            .fail_unapplied_input(
                SESSION,
                INPUT,
                INPUT_CYCLE,
                "late fallback from the original input cycle",
                false,
                at_ms,
            )
            .expect("attempt the stale owner's fallback"),
        "the original input cycle must not fail the recovered receipt"
    );
    assert_eq!(receipt(fixture), before);
}

#[test]
fn resume_before_defer_records_gate_and_rebinds_consumed_anchor() {
    let fixture = Fixture::new();
    let paused = paused_input_before_defer(&fixture);
    let inbox = SessionInbox::new(Arc::clone(&fixture.pool));
    let original = inbox.get(SESSION, INPUT).expect("input").expect("input");

    // Deterministically interleave resume after consumption and before the
    // original driver records its gate. No prior defer, sleeps or new API.
    let resumed = fixture
        .control
        .resume_session(SESSION, paused.revision, 30)
        .expect("resume before the original owner settles");
    let retained = receipt(&fixture);
    assert_eq!(retained.state, InputReceiptState::Recorded);
    assert_eq!(
        retained.execution_gate,
        Some(InputExecutionGate {
            reason: InputGateReason::User,
            recovery: InputGateRecovery::ResumeWork,
            execution_revision: paused.revision,
            cycle_id: INPUT_CYCLE.to_owned(),
            request_id: None,
            source_id: None,
        }),
        "resume must freeze the paused gate before changing execution state"
    );
    assert!(retained.turn_id.is_none());
    assert!(retained.applied_at.is_none());
    assert!(retained.completed_at.is_none());
    assert!(retained.error.is_none());
    let recovered = inbox.get(SESSION, INPUT).expect("input").expect("input");
    assert_eq!(recovered.state, SubmissionState::Consumed);
    assert_eq!(recovered.cycle_id, resumed.state.cycle_id);
    assert_ne!(recovered.cycle_id, original.cycle_id);
    assert_eq!(recovered.id, original.id);
    assert_eq!(recovered.prompt, original.prompt);
    assert_eq!(recovered.admitted_sequence, original.admitted_sequence);
    assert_eq!(recovered.promoted_sequence, original.promoted_sequence);
}

#[test]
fn same_revision_resume_preserves_recovery_receipt_and_events() {
    let fixture = Fixture::new();
    let paused = paused_input_before_defer(&fixture);
    let resumed = fixture
        .control
        .resume_session(SESSION, paused.revision, 30)
        .expect("first resume");
    let inbox = SessionInbox::new(Arc::clone(&fixture.pool));
    let log = SessionEventLog::new(Arc::clone(&fixture.pool));
    let retained = receipt(&fixture);
    let recovered = inbox.get(SESSION, INPUT).expect("input");
    let events = log.read_after(SESSION, None).expect("events");

    let repeated = fixture
        .control
        .resume_session(SESSION, paused.revision, 31)
        .expect("repeat the exact resume revision");
    assert_eq!(repeated.state, resumed.state);
    assert_eq!(repeated.input, resumed.input);
    assert_eq!(receipt(&fixture), retained);
    assert_eq!(inbox.get(SESSION, INPUT).expect("input"), recovered);
    assert_eq!(log.read_after(SESSION, None).expect("events"), events);
}

#[test]
fn late_defer_preserves_resumed_bound_and_applied_input() {
    let fixture = Fixture::new();
    let paused = paused_input_before_defer(&fixture);
    fixture
        .control
        .resume_session(SESSION, paused.revision, 30)
        .expect("resume before defer");
    late_defer_preserves_receipt(&fixture, 31);
    assert_eq!(receipt(&fixture).state, InputReceiptState::Recorded);

    let receipts = InputReceiptStore::new(Arc::clone(&fixture.pool));
    let ids = [INPUT.to_owned()];
    receipts
        .bind_turn(SESSION, &ids, "resumed-turn", 40)
        .expect("bind the resumed input");
    late_defer_preserves_receipt(&fixture, 41);
    let bound = receipt(&fixture);
    assert_eq!(bound.state, InputReceiptState::Recorded);
    assert_eq!(bound.turn_id.as_deref(), Some("resumed-turn"));
    assert!(bound.applied_at.is_none());
    assert!(bound.completed_at.is_none());

    receipts
        .mark_applied(SESSION, &ids, "resumed-turn", 50)
        .expect("record provider application");
    late_defer_preserves_receipt(&fixture, 51);
    let applied = receipt(&fixture);
    assert_eq!(applied.state, InputReceiptState::Applied);
    assert_eq!(applied.applied_at, Some(50));
    assert!(applied.execution_gate.is_none());
    assert!(applied.completed_at.is_none());
    assert!(applied.error.is_none());

    receipts
        .finish_turn(
            SESSION,
            "resumed-turn",
            Some(InputStopReason::EndTurn),
            None,
            60,
        )
        .expect("complete the provider turn");
    late_defer_preserves_receipt(&fixture, 61);
    let completed = receipt(&fixture);
    assert_eq!(completed.state, InputReceiptState::Completed);
    assert_eq!(completed.applied_at, Some(50));
    assert_eq!(completed.completed_at, Some(60));
    assert_eq!(completed.stop_reason, Some(InputStopReason::EndTurn));
    assert!(completed.error.is_none());
}
