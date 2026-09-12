use super::*;
use serde_json::json;
use zuno_db::message::MessageStore;
use zuno_types::execution::{SessionPauseReason, SessionReadiness, SessionScheduling};

fn paused_work(fixture: &Fixture, reason: SessionPauseReason) -> i64 {
    let store = zuno_db::session_execution::SessionExecutionStore::new(fixture.pool.clone());
    let state = store
        .seed(
            SESSION,
            CollaborationMode::Work,
            Some(Fixture::identity()),
            2,
        )
        .expect("seed Work");
    store
        .set_scheduling(
            SESSION,
            state.revision,
            SessionScheduling {
                readiness: SessionReadiness::Paused { reason },
                ..SessionScheduling::default()
            },
            3,
        )
        .expect("pause")
        .revision
}

#[test]
fn session_resume_does_not_claim_uninspected_side_effects_were_reconciled() {
    let fixture = Fixture::new();
    let revision = paused_work(&fixture, SessionPauseReason::User);
    let part = json!({
        "id":"uncertain-part", "sessionID":SESSION, "messageID":"uncertain-message",
        "type":"tool", "callID":"uncertain-call", "tool":"shell",
        "state":{
            "status":"error", "outcome":"uncertain",
            "uncertain":{
                "tool":"shell", "callID":"uncertain-call", "appliedPaths":["artifact.txt"],
                "cause":"lost_outcome", "observedAtMs":4
            },
            "input":{"command":"publish-artifact"},
            "error":"the external result was not observed",
            "time":{"start":4,"end":4}
        }
    });
    {
        let connection = fixture.pool.get().expect("connection");
        connection
            .execute(
                "INSERT INTO message(id,session_id,time_created,time_updated,data)
                 VALUES('uncertain-message',?1,4,4,'{\"role\":\"assistant\"}')",
                [SESSION],
            )
            .expect("message");
        connection
            .execute(
                "INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
                 VALUES('uncertain-part','uncertain-message',?1,4,4,?2)",
                rusqlite::params![SESSION, part.to_string()],
            )
            .expect("uncertain part");
        assert_eq!(
            MessageStore::new(&connection)
                .pending_uncertain_tool_calls(SESSION, 0)
                .expect("valid uncertainty fixture")
                .len(),
            1
        );
    }
    let before = fixture.control.state(SESSION).expect("before");
    let result = fixture.control.resume_session(SESSION, revision, 10);
    let connection = fixture.pool.get().expect("connection");
    assert_eq!(
        MessageStore::new(&connection)
            .pending_uncertain_tool_calls(SESSION, 0)
            .expect("pending after resume")
            .len(),
        1,
        "resume is not an authoritative inspection receipt"
    );
    assert!(matches!(
        result,
        Err(SessionControlError::ResumeRejected { .. })
    ));
    assert_eq!(fixture.control.state(SESSION).expect("after"), before);
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM session_input WHERE session_id=?1",
                [SESSION],
                |row| { row.get::<_, i64>(0) }
            )
            .expect("no admitted control"),
        0
    );
}

#[test]
fn session_resume_keeps_authentication_and_turn_budget_gates() {
    for reason in [
        SessionPauseReason::Authentication,
        SessionPauseReason::TurnBudget,
    ] {
        let fixture = Fixture::new();
        let revision = paused_work(&fixture, reason);
        let before = fixture.control.state(SESSION).expect("before");
        assert!(
            matches!(
                fixture.control.resume_session(SESSION, revision, 10),
                Err(SessionControlError::ResumeRejected { .. })
            ),
            "ordinary resume must not remove {reason:?}"
        );
        assert_eq!(fixture.control.state(SESSION).expect("after"), before);
    }
}

fn activate(fixture: &Fixture, id: &str, at: i64) -> zuno_db::session_work_cycle::SessionWorkCycle {
    activate_with_trigger(fixture, id, at, InputTriggerKind::User)
}

fn activate_with_trigger(
    fixture: &Fixture,
    id: &str,
    at: i64,
    trigger: InputTriggerKind,
) -> zuno_db::session_work_cycle::SessionWorkCycle {
    fixture.pool.try_transaction(|tx| {
        tx.execute(
            "INSERT INTO message(id,session_id,time_created,time_updated,data) VALUES(?1,?2,?3,?3,'{\"role\":\"user\"}')",
            rusqlite::params![id, SESSION, at],
        ).map_err(zuno_db::map_error)?;
        zuno_db::inbox::admit_and_promote_in(tx,
            zuno_db::inbox::NewSessionInput::new(
                id, SESSION, json!({"kind":"acpPrompt","text":"independent user request"}),
                zuno_db::inbox::InputDelivery::Queue, at,
            ).with_trigger_kind(trigger),
        )?;
        let cycle = SessionControlService::activate_user_input_in(
            tx, SESSION, id, id, CollaborationMode::Work, Fixture::identity(), at,
        )?;
        zuno_db::inbox::mark_consumed_in(tx, SESSION, id)?;
        Ok::<_, SessionControlError>(cycle)
    }).expect("activate user input")
}

#[test]
fn stopped_cycle_does_not_pause_new_user_input_or_accept_old_completion() {
    let fixture = Fixture::new();
    let first = activate(&fixture, "first-input", 10);
    fixture
        .control
        .begin_engine_turn(SESSION, &first.cycle_id, "turn-one")
        .unwrap();
    fixture
        .control
        .stop_turn(SESSION, &first.cycle_id, "turn-one", true, 20)
        .expect("stop");
    assert_eq!(
        fixture.control.state(SESSION).unwrap().unwrap().phase,
        SessionExecutionPhase::Completed
    );
    let second = activate(&fixture, "second-input", 30);
    assert_ne!(first.cycle_id, second.cycle_id);
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(state.scheduling.unwrap().readiness, SessionReadiness::Ready);
    let connection = fixture.pool.get().expect("connection");
    assert!(
        zuno_db::session_work_cycle::is_stopped_in(&connection, SESSION, &first.cycle_id).unwrap()
    );
    drop(connection);
    let inbox = zuno_db::inbox::SessionInbox::new(fixture.pool.clone());
    let report = inbox
        .admit(
            zuno_db::inbox::NewSessionInput::new(
                "late-result",
                SESSION,
                json!({"kind":"backgroundExecutionReport","executionID":"process-one"}),
                zuno_db::inbox::InputDelivery::Queue,
                40,
            )
            .with_trigger_kind(InputTriggerKind::Automatic)
            .with_cycle_id(Some(first.cycle_id.clone())),
        )
        .unwrap();
    let connection = fixture.pool.get().unwrap();
    assert_eq!(
        zuno_db::session_wake::admission_in(&connection, &report).unwrap(),
        zuno_types::execution::WakeAdmission::Reject
    );
    drop(connection);
    let before = fixture.control.state(SESSION).unwrap();
    fixture
        .control
        .stop_turn(SESSION, &first.cycle_id, "turn-one", true, 50)
        .unwrap();
    assert_eq!(
        fixture.control.state(SESSION).unwrap(),
        before,
        "late stop cannot hit T2"
    );
}

#[test]
fn new_request_keeps_goal_pause_identity_and_does_not_adopt_old_plan() {
    let fixture = Fixture::new();
    let goal = fixture
        .goals
        .create_goal(SESSION, "old objective", Some(1000))
        .unwrap();
    fixture.plan();
    fixture
        .goals
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap();
    let before = fixture.goals.goal(SESSION).unwrap().unwrap();
    let cycle = activate(&fixture, "new-independent-request", 30);
    assert!(cycle.goal_id.is_none());
    assert!(cycle.plan_id.is_none());
    assert_eq!(fixture.goals.goal(SESSION).unwrap().unwrap(), before);
    assert_eq!(before.goal_id, goal.goal_id);
}

#[test]
fn unknown_legacy_pause_and_protected_gates_survive_real_new_input() {
    for reason in [
        SessionPauseReason::User,
        SessionPauseReason::Authentication,
        SessionPauseReason::TurnBudget,
        SessionPauseReason::Blocked,
    ] {
        let fixture = Fixture::new();
        paused_work(&fixture, reason);
        activate(&fixture, "new-input", 10);
        assert_eq!(
            fixture
                .control
                .state(SESSION)
                .unwrap()
                .unwrap()
                .scheduling
                .unwrap()
                .readiness,
            SessionReadiness::Paused { reason }
        );
        assert!(
            zuno_engine::plan_driver::PlanReconciliationDriver::new(fixture.pool.clone())
                .begin_with_wake(
                    SESSION,
                    "untrusted-new-cycle",
                    &zuno_types::execution::SessionWakeSignal::UserMessage,
                    11
                )
                .unwrap()
                .is_none(),
            "protected gate {reason:?} must deny full model execution"
        );
    }
}

#[test]
fn resumed_goal_is_not_stopped_by_replayed_old_turn_stop() {
    let fixture = Fixture::new();
    fixture
        .goals
        .create_goal(SESSION, "long task", None)
        .unwrap();
    let original = activate(&fixture, "old-goal-input", 10);
    fixture
        .control
        .begin_engine_turn(SESSION, &original.cycle_id, "turn-old")
        .unwrap();
    fixture
        .control
        .stop_turn(SESSION, &original.cycle_id, "turn-old", true, 20)
        .unwrap();
    fixture
        .goals
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap();
    let goal = fixture.goals.goal(SESSION).unwrap().unwrap();
    let resumed = fixture
        .control
        .resume_goal(
            &zuno_types::goal_resume::GoalResumeRequest {
                session_id: SESSION.to_owned(),
                goal_id: goal.goal_id,
                expected_revision: goal.revision,
                input_id: None,
            },
            30,
        )
        .unwrap();
    let current_cycle = resumed.state.cycle_id.clone().unwrap();
    fixture
        .control
        .begin_engine_turn(SESSION, &current_cycle, "turn-new")
        .unwrap();
    let before = fixture.control.state(SESSION).unwrap();
    fixture
        .control
        .stop_turn(SESSION, &original.cycle_id, "turn-old", true, 40)
        .unwrap();
    assert_eq!(
        fixture.control.state(SESSION).unwrap(),
        before,
        "an old cancellation cannot close explicitly resumed execution"
    );
}

#[test]
fn old_engine_turn_cannot_stop_a_new_turn_on_the_same_logical_cycle() {
    let fixture = Fixture::new();
    let cycle = activate(&fixture, "logical-input", 10);
    fixture
        .control
        .begin_engine_turn(SESSION, &cycle.cycle_id, "T1")
        .unwrap();
    fixture
        .control
        .begin_engine_turn(SESSION, &cycle.cycle_id, "T2")
        .unwrap();
    let before = fixture.control.state(SESSION).unwrap();
    fixture
        .control
        .stop_turn(SESSION, &cycle.cycle_id, "T1", true, 20)
        .unwrap();
    assert_eq!(fixture.control.state(SESSION).unwrap(), before);
    fixture
        .control
        .stop_turn(SESSION, &cycle.cycle_id, "T2", true, 30)
        .unwrap();
    assert_eq!(
        fixture.control.state(SESSION).unwrap().unwrap().phase,
        SessionExecutionPhase::Completed
    );
}

#[test]
fn explicit_goal_command_binds_new_goal_after_an_ordinary_input() {
    let fixture = Fixture::new();
    let previous = activate(&fixture, "ordinary-input", 10);
    let goal = fixture
        .goals
        .create_goal(SESSION, "new Goal command", None)
        .unwrap();
    fixture.control.resume_goal_execution(SESSION, 20).unwrap();
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    let cycle = state.cycle_id.unwrap();
    let identity = fixture
        .control
        .begin_engine_turn(SESSION, &cycle, "first-goal-turn")
        .unwrap()
        .expect("new active Goal must have native cycle and turn ownership");
    assert_eq!(identity.goal_id, goal.goal_id);
    assert_ne!(
        cycle, previous.cycle_id,
        "a new Goal is not the previous ordinary input"
    );
}

#[test]
fn proven_legacy_turn_interruption_is_reclassified_only_on_real_user_input() {
    let fixture = Fixture::new();
    // Include the native lifecycle evidence; an assistant AbortError and a
    // matching last turn alone do not prove what caused a later session pause.
    legacy_pause_after_turns(&fixture, 0);
    assert_exact_latest_abort_matches(&fixture);
    let before = fixture.control.state(SESSION).unwrap();
    for wake in [
        zuno_types::execution::SessionWakeSignal::Automatic,
        zuno_types::execution::SessionWakeSignal::Callback,
        zuno_types::execution::SessionWakeSignal::Recovery,
    ] {
        assert!(
            zuno_engine::plan_driver::PlanReconciliationDriver::new(fixture.pool.clone())
                .begin_with_wake(SESSION, "not-user-authority", &wake, 90)
                .unwrap()
                .is_none()
        );
        assert_eq!(fixture.control.state(SESSION).unwrap(), before);
    }
    assert_eq!(
        fixture.control.state(SESSION).unwrap().unwrap().phase,
        SessionExecutionPhase::Paused
    );
    activate(&fixture, "new-after-legacy-stop", 100);
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(state.scheduling.unwrap().readiness, SessionReadiness::Ready);
    assert_eq!(
        state.continuation.unwrap().context_epoch,
        7,
        "a new request must not reset compaction accounting"
    );
}

fn assert_exact_latest_abort_matches(fixture: &Fixture) {
    let connection = fixture.pool.get().unwrap();
    let (name, reason, parent, turn): (String, String, String, String) = connection
        .query_row(
            "SELECT json_extract(data,'$.error.name'),json_extract(data,'$.error.data.reason'),
                    json_extract(data,'$.parentID'),json_extract(data,'$.turnID')
             FROM message WHERE session_id=?1 AND json_extract(data,'$.role')='assistant'
             ORDER BY time_created DESC,id DESC LIMIT 1",
            [SESSION],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    let latest_turn: String = connection
        .query_row(
            "SELECT json_extract(data,'$.turnID') FROM event WHERE aggregate_id=?1
             AND type='session.turn.started.1' ORDER BY seq DESC LIMIT 1",
            [SESSION],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(name, "AbortError");
    assert_eq!(reason, "user_cancel");
    assert_eq!(turn, latest_turn);
    assert_eq!(
        state.continuation.unwrap().anchor_message_id.as_deref(),
        Some(parent.as_str())
    );
}

#[test]
fn legacy_pause_provenance_latest_checkpoint_without_pause_origin_stays_gated() {
    let fixture = Fixture::new();
    // Preserve the original minimal fixture as a safety counterexample. It
    // proves an exact latest cancellation, but not the origin of paused/user:
    // the pause was written at t=3, the AbortError at t=4, with no cycle start or
    // cancellation receipt linking them. An independent later pause can leave
    // exactly the same last assistant/turn/anchor. Native /resume, not guessed
    // cancellation provenance, is the authorization path for this old state.
    paused_work(&fixture, SessionPauseReason::User);
    fixture.pool.transaction(|tx| {
        let mut state = zuno_db::session_execution::read_in(tx, SESSION)?.unwrap();
        state.cycle_id = Some("old-cycle".to_owned());
        state.continuation = Some(zuno_types::execution::ContinuationToken {
            cycle_id: "old-cycle".to_owned(), identity: Fixture::identity(), mode: CollaborationMode::Work,
            plan_id: None, plan_revision: None, context_epoch: 7, anchor_message_id: Some("old-user".to_owned()),
        });
        zuno_db::session_execution::update_in(tx, state.revision, state)?;
        tx.execute(
            "INSERT INTO message(id,session_id,time_created,time_updated,data) VALUES('old-assistant',?1,4,4,?2)",
            rusqlite::params![SESSION,json!({"role":"assistant","parentID":"old-user","turnID":"old-turn",
                "error":{"name":"AbortError","data":{"reason":"user_cancel","source":"tui"}}}).to_string()],
        ).map_err(zuno_db::map_error)?;
        zuno_db::event_log::append_in(tx, SESSION,
            zuno_db::event_log::NewSessionEvent::new("session.turn.started",
                json!({"turnID":"old-turn","anchorMessageID":"old-user"}).as_object().unwrap().clone())?)?;
        Ok(())
    }).unwrap();
    assert_exact_latest_abort_matches(&fixture);
    activate_with_trigger(
        &fixture,
        "new-after-unproven-stop",
        100,
        InputTriggerKind::Legacy,
    );
    assert_legacy_user_gate(&fixture);
    let event = zuno_db::event_log::SessionEventLog::new(fixture.pool.clone())
        .latest_of_type(SESSION, "session.work_cycle.started")
        .unwrap()
        .unwrap();
    assert_eq!(event.properties["legacyInterruption"], false);
    assert_eq!(event.properties["protectedGateRetained"], true);
}

#[test]
fn fresh_user_cycle_resets_old_no_progress_but_not_its_evidence() {
    let fixture = Fixture::new();
    let old = activate(&fixture, "old-input", 10);
    paused_work(&fixture, SessionPauseReason::NoProgress);
    let next = activate(&fixture, "fresh-input", 20);
    assert_ne!(old.cycle_id, next.cycle_id);
    let connection = fixture.pool.get().unwrap();
    let previous = zuno_db::session_work_cycle::read_in(&connection, SESSION, &old.cycle_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        previous.scheduling.unwrap().readiness,
        SessionReadiness::Paused {
            reason: SessionPauseReason::NoProgress
        }
    );
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .unwrap()
            .unwrap()
            .scheduling
            .unwrap()
            .unchanged_progress_count,
        0
    );
}

#[test]
fn explicit_goal_resume_recovers_original_report_cycle_after_independent_query() {
    let fixture = Fixture::new();
    fixture
        .goals
        .create_goal(SESSION, "original objective", None)
        .unwrap();
    let original = activate(&fixture, "goal-input", 10);
    fixture
        .control
        .begin_engine_turn(SESSION, &original.cycle_id, "goal-turn")
        .unwrap();
    fixture
        .control
        .stop_turn(SESSION, &original.cycle_id, "goal-turn", true, 20)
        .unwrap();
    fixture
        .goals
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap();
    let query = activate(&fixture, "independent-query", 30);
    assert!(query.goal_id.is_none());
    let paused = fixture.goals.goal(SESSION).unwrap().unwrap();
    let resumed = fixture
        .control
        .resume_goal(
            &zuno_types::goal_resume::GoalResumeRequest {
                session_id: SESSION.to_owned(),
                goal_id: paused.goal_id,
                expected_revision: paused.revision,
                input_id: Some("independent-query".to_owned()),
            },
            40,
        )
        .unwrap();
    assert_eq!(resumed.goal.status, GoalStatus::Active);
    let report = zuno_db::inbox::SessionInbox::new(fixture.pool.clone())
        .admit(
            zuno_db::inbox::NewSessionInput::new(
                "goal-result",
                SESSION,
                json!({"kind":"backgroundExecutionReport","executionID":"goal-process"}),
                zuno_db::inbox::InputDelivery::Queue,
                50,
            )
            .with_trigger_kind(InputTriggerKind::Automatic)
            .with_cycle_id(Some(original.cycle_id)),
        )
        .unwrap();
    assert_eq!(
        zuno_db::session_wake::admission_in(&fixture.pool.get().unwrap(), &report).unwrap(),
        zuno_types::execution::WakeAdmission::Admit
    );
    assert_ne!(
        resumed.input.unwrap().id,
        "independent-query",
        "processed query is not replayed"
    );
}

#[test]
fn stopped_external_wait_resumes_with_a_new_cycle_and_exact_original_completion() {
    for crash_before_goal_pause in [false, true] {
        let fixture = Fixture::new();
        fixture
            .goals
            .create_goal(SESSION, "await original observer", None)
            .unwrap();
        let cycle = activate(&fixture, "observer-input", 10);
        fixture
            .control
            .begin_engine_turn(SESSION, &cycle.cycle_id, "observer-turn")
            .unwrap();
        let wait = zuno_types::execution::SessionWaitReference::External {
            source_id: "observer".to_owned(),
            origin_cycle_id: cycle.cycle_id.clone(),
        };
        fixture
            .pool
            .transaction(|tx| {
                let state = zuno_db::session_execution::read_in(tx, SESSION)?.unwrap();
                zuno_db::session_execution::set_waiting_in(
                    tx,
                    SESSION,
                    state.revision,
                    wait.clone(),
                    20,
                )?;
                Ok(())
            })
            .unwrap();
        fixture
            .control
            .stop_turn(SESSION, &cycle.cycle_id, "observer-turn", true, 21)
            .unwrap();
        if !crash_before_goal_pause {
            fixture
                .goals
                .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
                .unwrap();
        }
        let goal = fixture.goals.goal(SESSION).unwrap().unwrap();
        let resumed = fixture
            .control
            .resume_goal(
                &zuno_types::goal_resume::GoalResumeRequest {
                    session_id: SESSION.to_owned(),
                    goal_id: goal.goal_id,
                    expected_revision: goal.revision,
                    input_id: None,
                },
                30,
            )
            .unwrap();
        assert_eq!(resumed.goal.status, GoalStatus::Active);
        assert!(
            resumed.input.is_none(),
            "resume preserves the wait rather than invoking a model"
        );
        assert_ne!(
            resumed.state.cycle_id.as_deref(),
            Some(cycle.cycle_id.as_str())
        );
        assert_eq!(
            resumed.state.scheduling.unwrap().readiness,
            SessionReadiness::from(wait)
        );
        let inbox = zuno_db::inbox::SessionInbox::new(fixture.pool.clone());
        let report = inbox
            .admit(
                zuno_db::inbox::NewSessionInput::new(
                    "observer-result",
                    SESSION,
                    json!({"kind":"backgroundExecutionReport","executionID":"observer"}),
                    zuno_db::inbox::InputDelivery::Queue,
                    40,
                )
                .with_trigger_kind(InputTriggerKind::Automatic)
                .with_cycle_id(Some(cycle.cycle_id.clone())),
            )
            .unwrap();
        let connection = fixture.pool.get().unwrap();
        assert_eq!(
            zuno_db::session_wake::admission_in(&connection, &report).unwrap(),
            zuno_types::execution::WakeAdmission::Resume
        );
        assert!(
            zuno_db::session_work_cycle::is_stopped_in(&connection, SESSION, &cycle.cycle_id)
                .unwrap()
        );
    }
}

#[test]
fn stale_applied_or_completed_input_cannot_rebind_another_running_input_to_a_goal() {
    for completed_old in [false, true] {
        let fixture = Fixture::new();
        fixture
            .goals
            .create_goal(SESSION, "paused objective", None)
            .unwrap();
        fixture
            .goals
            .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
            .unwrap();
        let a = activate(&fixture, "input-a", 10);
        fixture
            .control
            .begin_engine_turn(SESSION, &a.cycle_id, "turn-a")
            .unwrap();
        let receipts = zuno_db::input_receipt::InputReceiptStore::new(fixture.pool.clone());
        receipts
            .mark_applied(SESSION, &["input-a".to_owned()], "turn-a", 11)
            .unwrap();
        if completed_old {
            receipts
                .finish_turn(
                    SESSION,
                    "turn-a",
                    Some(zuno_types::admission::InputStopReason::EndTurn),
                    None,
                    12,
                )
                .unwrap();
        }
        let b = activate(&fixture, "input-b", 20);
        fixture
            .control
            .begin_engine_turn(SESSION, &b.cycle_id, "turn-b")
            .unwrap();
        receipts
            .mark_applied(SESSION, &["input-b".to_owned()], "turn-b", 21)
            .unwrap();
        let before = fixture.control.state(SESSION).unwrap();
        let goal = fixture.goals.goal(SESSION).unwrap().unwrap();
        assert!(
            fixture
                .control
                .resume_goal(
                    &zuno_types::goal_resume::GoalResumeRequest {
                        session_id: SESSION.to_owned(),
                        goal_id: goal.goal_id.clone(),
                        expected_revision: goal.revision,
                        input_id: Some("input-a".to_owned()),
                    },
                    30
                )
                .is_err()
        );
        assert_eq!(fixture.control.state(SESSION).unwrap(), before);
        assert_eq!(fixture.goals.goal(SESSION).unwrap().unwrap(), goal);
        assert_eq!(
            zuno_db::session_work_cycle::current_in(&fixture.pool.get().unwrap(), SESSION)
                .unwrap()
                .unwrap()
                .goal_id,
            None
        );
    }
}

// A format-14 database as left by the v31 host, not a reconstruction from text.
// In particular, successful user queries did not rewrite the paused driver row.
const LEGACY_CYCLE: &str = "legacy-driver";
const LEGACY_INPUT: &str = "legacy-user";
const LEGACY_TURN: &str = "legacy-turn";

fn legacy_phase(fixture: &Fixture, cycle: &str, phase: &str, reason: Option<&str>) {
    let mut properties = json!({
        "cycleId":cycle, "phase":phase, "reconciliationAttempt":0
    });
    if let Some(reason) = reason {
        properties["reason"] = json!(reason);
        properties["pauseReason"] = json!(reason);
    }
    zuno_db::event_log::SessionEventLog::new(fixture.pool.clone())
        .append(
            SESSION,
            zuno_db::event_log::NewSessionEvent::new(
                "session.driver.phase",
                properties.as_object().unwrap().clone(),
            )
            .unwrap(),
        )
        .unwrap();
}

fn pad_legacy_events_to(
    tx: &zuno_db::Transaction<'_>,
    next_sequence: i64,
) -> Result<(), zuno_error::DbError> {
    let next: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq),-1)+1 FROM event WHERE aggregate_id=?1",
            [SESSION],
            |row| row.get(0),
        )
        .map_err(zuno_db::map_error)?;
    assert!(
        next <= next_sequence,
        "fixture passed requested sequence {next_sequence}: {next}"
    );
    for _ in next..next_sequence {
        zuno_db::event_log::append_in(
            tx,
            SESSION,
            zuno_db::event_log::NewSessionEvent::new("fixture.anonymous", Default::default())?,
        )?;
    }
    Ok(())
}

struct LegacyTurnPositions {
    started: i64,
    finished: i64,
    executing: Option<i64>,
    paused: Option<i64>,
}

fn legacy_turn(
    fixture: &Fixture,
    input: &str,
    turn: &str,
    at: i64,
    cancelled: bool,
    positions: LegacyTurnPositions,
) {
    // Match released defaults: trigger_kind=legacy and cycle_id=NULL.
    // The native turnTrigger=user event is the execution-source evidence.
    fixture
        .pool
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO message(id,session_id,time_created,time_updated,data)
             VALUES(?1,?2,?3,?3,?4)",
                rusqlite::params![
                    input,
                    SESSION,
                    at,
                    json!({"role":"user","id":input}).to_string()
                ],
            )
            .map_err(zuno_db::map_error)?;
            zuno_db::inbox::admit_and_promote_in(
                tx,
                zuno_db::inbox::NewSessionInput::new(
                    input,
                    SESSION,
                    json!({"kind":"acpPrompt","text":"anonymous request"}),
                    zuno_db::inbox::InputDelivery::Queue,
                    at,
                ),
            )?;
            zuno_db::inbox::mark_consumed_in(tx, SESSION, input)?;
            if let Some(executing) = positions.executing {
                pad_legacy_events_to(tx, executing)?;
                zuno_db::event_log::append_in(tx, SESSION,
                    zuno_db::event_log::NewSessionEvent::new("session.driver.phase",
                        json!({"cycleId":LEGACY_CYCLE,"phase":"executing","reconciliationAttempt":0})
                            .as_object().unwrap().clone())?)?;
            }
            Ok(())
        })
        .unwrap();
    // Exact v31 checkpoint write (not the current service): v32 additionally
    // authorizes a SessionWorkCycle here, which would turn this historical
    // fixture into a *new native authorization* and correctly disqualify repair.
    fixture
        .pool
        .transaction(|tx| {
            let mut state = zuno_db::session_execution::read_in(tx, SESSION)?.unwrap();
            state.cycle_id = Some(LEGACY_CYCLE.to_owned());
            let token = state.continuation.as_mut().unwrap();
            token.anchor_message_id = Some(input.to_owned());
            if state
                .scheduling
                .as_ref()
                .is_none_or(|s| s.readiness == SessionReadiness::Ready)
            {
                state.phase = SessionExecutionPhase::Running;
            }
            state.time_updated = at;
            zuno_db::session_execution::update_in(tx, state.revision, state)?;
            Ok(())
        })
        .unwrap();
    fixture
        .pool
        .transaction(|tx| {
            pad_legacy_events_to(tx, positions.started)?;
            zuno_db::event_log::append_in(
                tx,
                SESSION,
                zuno_db::event_log::NewSessionEvent::new(
                    "session.turn.started",
                    json!({"turnID":turn,"anchorMessageID":input,"turnTrigger":"user"})
                        .as_object()
                        .unwrap()
                        .clone(),
                )?,
            )?;
            zuno_db::input_receipt::mark_applied_in(
                tx,
                SESSION,
                &[input.to_owned()],
                turn,
                at + 1,
            )?;
            let assistant_id = format!("{input}-assistant");
            let mut assistant = json!({
                "role":"assistant","id":assistant_id,"parentID":input,"turnID":turn,
                "time":{"created":at+1,"completed":at+2}
            });
            if cancelled {
                assistant["error"] = json!({
                    "name":"AbortError", "data":{"reason":"user_cancel","source":"acp"}
                });
            } else {
                assistant["finish"] = json!("stop");
            }
            tx.execute(
                "INSERT INTO message(id,session_id,time_created,time_updated,data)
             VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![assistant_id, SESSION, at + 1, at + 2, assistant.to_string()],
            )
            .map_err(zuno_db::map_error)?;
            if let Some(paused) = positions.paused {
                pad_legacy_events_to(tx, paused)?;
                zuno_db::event_log::append_in(
                    tx,
                    SESSION,
                    zuno_db::event_log::NewSessionEvent::new(
                        "session.driver.phase",
                        json!({"cycleId":LEGACY_CYCLE,"phase":"paused","reason":"user",
                               "pauseReason":"user","reconciliationAttempt":0})
                        .as_object()
                        .unwrap()
                        .clone(),
                    )?,
                )?;
            }
            pad_legacy_events_to(tx, positions.finished)?;
            zuno_db::input_receipt::finish_turn_in(
                tx,
                SESSION,
                turn,
                Some(if cancelled {
                    zuno_types::admission::InputStopReason::Cancelled
                } else {
                    zuno_types::admission::InputStopReason::EndTurn
                }),
                None,
                at + 3,
            )?;
            Ok(())
        })
        .unwrap();
}

fn legacy_pause_after_turns(fixture: &Fixture, successful_followups: usize) {
    zuno_db::session_execution::SessionExecutionStore::new(fixture.pool.clone())
        .seed(
            SESSION,
            CollaborationMode::Work,
            Some(Fixture::identity()),
            2,
        )
        .unwrap();
    fixture
        .pool
        .transaction(|tx| {
            let mut state = zuno_db::session_execution::read_in(tx, SESSION)?.unwrap();
            state.cycle_id = Some(LEGACY_CYCLE.to_owned());
            state.continuation = Some(zuno_types::execution::ContinuationToken {
                cycle_id: LEGACY_CYCLE.to_owned(),
                identity: Fixture::identity(),
                mode: CollaborationMode::Work,
                plan_id: None,
                plan_revision: None,
                context_epoch: 7,
                anchor_message_id: None,
            });
            zuno_db::session_execution::update_in(tx, state.revision, state)?;
            Ok(())
        })
        .unwrap();
    legacy_turn(
        fixture,
        LEGACY_INPUT,
        LEGACY_TURN,
        10,
        true,
        LegacyTurnPositions {
            started: 68,
            finished: 90,
            executing: Some(65),
            paused: None,
        },
    );
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    zuno_db::session_execution::SessionExecutionStore::new(fixture.pool.clone())
        .set_scheduling(
            SESSION,
            state.revision,
            SessionScheduling {
                readiness: SessionReadiness::Paused {
                    reason: SessionPauseReason::User,
                },
                ..Default::default()
            },
            14,
        )
        .unwrap();
    if successful_followups == 0 {
        legacy_phase(fixture, LEGACY_CYCLE, "paused", Some("user"));
    }
    for index in 0..successful_followups {
        legacy_turn(
            fixture,
            &format!("followup-{index}"),
            &format!("followup-turn-{index}"),
            20 + i64::try_from(index).unwrap() * 10,
            false,
            if index == 0 {
                // The first pause projection was recorded during B, *after*
                // B had already started and before its completion receipt.
                LegacyTurnPositions {
                    started: 96,
                    finished: 115,
                    executing: None,
                    paused: Some(114),
                }
            } else {
                LegacyTurnPositions {
                    started: 123 + (i64::try_from(index).unwrap() - 1) * 50,
                    finished: 165 + (i64::try_from(index).unwrap() - 1) * 50,
                    executing: None,
                    paused: None,
                }
            },
        );
    }
}

// Exact v32 retained-gate artifact: a consumed input, failed/unapplied receipt,
// and a fresh cycle whose start event records the inherited pause. This bypasses
// activate_user_input_in intentionally so the fixture still represents v32 after
// the new implementation fixes that function.
fn legacy_failed_bridge(fixture: &Fixture, input: &str, at: i64) {
    fixture
        .pool
        .transaction(|tx| {
            let mut state = zuno_db::session_execution::read_in(tx, SESSION)?.unwrap();
            let previous_cycle_id = state.cycle_id.clone();
            let previous_scheduling = state.scheduling.clone();
            if let Some(mut previous) = zuno_db::session_work_cycle::current_in(tx, SESSION)? {
                previous.scheduling = previous_scheduling.clone();
                zuno_db::session_work_cycle::save_in(tx, &previous, at)?;
            }
            tx.execute(
                "INSERT INTO message(id,session_id,time_created,time_updated,data)
             VALUES(?1,?2,?3,?3,?4)",
                rusqlite::params![
                    input,
                    SESSION,
                    at,
                    json!({"role":"user","id":input}).to_string()
                ],
            )
            .map_err(zuno_db::map_error)?;
            let cycle_id = format!("input_{input}");
            zuno_db::inbox::admit_and_promote_in(
                tx,
                zuno_db::inbox::NewSessionInput::new(
                    input,
                    SESSION,
                    json!({"kind":"acpPrompt","text":"anonymous followup"}),
                    zuno_db::inbox::InputDelivery::Steer,
                    at,
                )
                .with_cycle_id(Some(cycle_id.clone())),
            )?;
            let cycle = zuno_db::session_work_cycle::SessionWorkCycle {
                session_id: SESSION.to_owned(),
                cycle_id: cycle_id.clone(),
                anchor_message_id: Some(input.to_owned()),
                goal_id: None,
                plan_id: None,
                todo_ids: Default::default(),
                active_turn_id: None,
                resumed_goal_cycles: Default::default(),
                stopped: None,
                scheduling: None,
            };
            state.cycle_id = Some(cycle_id.clone());
            let token = state.continuation.as_mut().unwrap();
            token.cycle_id = cycle_id;
            token.anchor_message_id = Some(input.to_owned());
            state.time_updated = at;
            zuno_db::session_execution::update_in(tx, state.revision, state)?;
            zuno_db::session_work_cycle::save_in(tx, &cycle, at)?;
            zuno_db::event_log::append_in(
                tx,
                SESSION,
                zuno_db::event_log::NewSessionEvent::new(
                    "session.work_cycle.started",
                    json!({"cycle":cycle,"inputId":input,"time":at,
                    "previousCycleId":previous_cycle_id,"previousScheduling":previous_scheduling,
                    "legacyInterruption":false,"protectedGateRetained":true})
                    .as_object()
                    .unwrap()
                    .clone(),
                )?,
            )?;
            zuno_db::inbox::mark_consumed_in(tx, SESSION, input)?;
            Ok(())
        })
        .unwrap();
    zuno_db::input_receipt::InputReceiptStore::new(fixture.pool.clone())
        .fail_input(
            SESSION,
            input,
            "native processing stopped before this input was applied; its durable record was retained",
            false,
            at + 1,
        )
        .unwrap();
}

fn assert_legacy_user_gate(fixture: &Fixture) {
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(
        state.scheduling.unwrap().readiness,
        SessionReadiness::Paused {
            reason: SessionPauseReason::User
        }
    );
    assert!(
        zuno_engine::plan_driver::PlanReconciliationDriver::new(fixture.pool.clone())
            .begin_with_wake(
                SESSION,
                "must-not-execute",
                &zuno_types::execution::SessionWakeSignal::UserMessage,
                300,
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn legacy_pause_provenance_matches_released_acp_event_sequence() {
    let fixture = Fixture::new();
    legacy_pause_after_turns(&fixture, 2);
    let events = zuno_db::event_log::SessionEventLog::new(fixture.pool.clone())
        .read_after(SESSION, None)
        .unwrap();
    let selected: Vec<_> = events
        .iter()
        .filter(|event| {
            event.event_type == "session.driver.phase"
                || event.event_type == "session.turn.started"
                || (event.event_type == "session.input.receipt"
                    && matches!(
                        event.properties["state"].as_str(),
                        Some("cancelled" | "completed")
                    ))
        })
        .map(|event| (event.sequence, event.event_type.as_str()))
        .collect();
    assert_eq!(
        selected,
        [
            (65, "session.driver.phase"),
            (68, "session.turn.started"),
            (90, "session.input.receipt"),
            (96, "session.turn.started"),
            (114, "session.driver.phase"),
            (115, "session.input.receipt"),
            (123, "session.turn.started"),
            (165, "session.input.receipt"),
        ]
    );
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(
        state.continuation.unwrap().anchor_message_id.as_deref(),
        Some("followup-1")
    );
    assert_eq!(
        state.time_updated, 30,
        "v31 updated the continuation at C's prelude"
    );
    assert!(
        events
            .iter()
            .all(|event| !event.event_type.starts_with("session.work_cycle."))
    );
    assert!(
        zuno_db::session_work_cycle::current_in(&fixture.pool.get().unwrap(), SESSION)
            .unwrap()
            .is_none()
    );
    for input_id in [LEGACY_INPUT, "followup-0", "followup-1"] {
        let input = zuno_db::inbox::read_in(&fixture.pool.get().unwrap(), SESSION, input_id)
            .unwrap()
            .unwrap();
        assert_eq!(input.trigger_kind, InputTriggerKind::Legacy);
        assert!(input.cycle_id.is_none());
    }
    activate_with_trigger(
        &fixture,
        "new-legacy-acp-input",
        100,
        InputTriggerKind::Legacy,
    );
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .unwrap()
            .unwrap()
            .scheduling
            .unwrap()
            .readiness,
        SessionReadiness::Ready,
        "the direct released sequence has no lossy bridge"
    );
    let event = zuno_db::event_log::SessionEventLog::new(fixture.pool.clone())
        .latest_of_type(SESSION, "session.work_cycle.started")
        .unwrap()
        .unwrap();
    let proof = &event.properties["legacyInterruptionProof"];
    assert_eq!(proof["executingSequence"], 65);
    assert_eq!(proof["turnStartedSequence"], 68);
    assert_eq!(proof["cancellationSequence"], 90);
    assert_eq!(proof["pausedSequence"], 114);
}

#[test]
fn legacy_pause_provenance_survives_successful_followup_turns() {
    let fixture = Fixture::new();
    legacy_pause_after_turns(&fixture, 2);
    let before = fixture.control.state(SESSION).unwrap();
    for wake in [
        zuno_types::execution::SessionWakeSignal::Automatic,
        zuno_types::execution::SessionWakeSignal::Callback,
        zuno_types::execution::SessionWakeSignal::Recovery,
    ] {
        assert!(
            zuno_engine::plan_driver::PlanReconciliationDriver::new(fixture.pool.clone())
                .begin_with_wake(SESSION, "not-a-user-request", &wake, 80)
                .unwrap()
                .is_none()
        );
        assert_eq!(fixture.control.state(SESSION).unwrap(), before);
    }
    let cycle = activate(&fixture, "real-new-request", 100);
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(
        state.scheduling.unwrap().readiness,
        SessionReadiness::Ready,
        "later successful replies do not erase the exact original interruption provenance"
    );
    assert_ne!(cycle.cycle_id, LEGACY_CYCLE);
    assert!(cycle.plan_id.is_none());
    assert_eq!(state.continuation.unwrap().context_epoch, 7);
}

#[test]
fn legacy_pause_provenance_one_retained_gate_failure_requires_explicit_resume() {
    assert_legacy_failed_bridges_require_explicit_resume(1);
}

#[test]
fn legacy_pause_provenance_multiple_retained_gate_failures_require_explicit_resume() {
    assert_legacy_failed_bridges_require_explicit_resume(3);
}

fn assert_legacy_failed_bridges_require_explicit_resume(failures: usize) {
    let fixture = Fixture::new();
    legacy_pause_after_turns(&fixture, 2);
    let receipts = zuno_db::input_receipt::InputReceiptStore::new(fixture.pool.clone());
    let mut old_receipts = Vec::new();
    for index in 0..failures {
        let input = format!("retained-{index}");
        legacy_failed_bridge(&fixture, &input, 50 + i64::try_from(index).unwrap() * 10);
        let receipt = receipts.get(SESSION, &input).unwrap().unwrap();
        assert_eq!(
            receipt.state,
            zuno_types::admission::InputReceiptState::Failed
        );
        assert!(receipt.turn_id.is_none());
        assert!(receipt.applied_at.is_none());
        old_receipts.push(receipt);
    }
    let paused_before = fixture.control.state(SESSION).unwrap().unwrap();
    let gated_cycle = activate_with_trigger(
        &fixture,
        "new-after-failures",
        100,
        InputTriggerKind::Legacy,
    );
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(state.scheduling, paused_before.scheduling);
    assert_legacy_user_gate(&fixture);
    let log = zuno_db::event_log::SessionEventLog::new(fixture.pool.clone());
    let event = log
        .latest_of_type(SESSION, "session.work_cycle.started")
        .unwrap()
        .unwrap();
    assert_eq!(event.properties["legacyInterruption"], false);
    assert_eq!(event.properties["protectedGateRetained"], true);
    assert!(event.properties["legacyInterruptionProof"].is_null());
    assert!(
        fixture
            .control
            .defer_input_at_execution_gate(SESSION, "new-after-failures", 101)
            .unwrap()
    );
    let gated_receipt = receipts
        .get(SESSION, "new-after-failures")
        .unwrap()
        .unwrap();
    let wire_receipt = serde_json::to_value(&gated_receipt).unwrap();
    assert_eq!(wire_receipt["state"], "recorded");
    assert_eq!(wire_receipt["executionGate"]["reason"], "user");
    assert_eq!(wire_receipt["executionGate"]["recovery"], "resume_work");
    assert!(gated_receipt.turn_id.is_none() && gated_receipt.applied_at.is_none());

    let retained_input =
        zuno_db::inbox::read_in(&fixture.pool.get().unwrap(), SESSION, "new-after-failures")
            .unwrap()
            .unwrap();
    let history_before = legacy_message_rows(&fixture);
    let inputs_before: i64 = fixture
        .pool
        .get()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM session_input WHERE session_id=?1",
            [SESSION],
            |row| row.get(0),
        )
        .unwrap();
    let authorized_before = log
        .read_of_type_after(SESSION, "session.work_cycle.authorized", None)
        .unwrap()
        .len();
    let resumed = fixture
        .control
        .resume_session(SESSION, state.revision, 110)
        .unwrap();
    assert_eq!(
        resumed.state.scheduling.as_ref().unwrap().readiness,
        SessionReadiness::Ready
    );
    assert_ne!(
        resumed.state.cycle_id.as_deref(),
        Some(gated_cycle.cycle_id.as_str())
    );
    assert_eq!(resumed.input.trigger_kind, InputTriggerKind::UserControl);
    assert_eq!(resumed.input.prompt["control"], "resume_work");
    assert_eq!(resumed.input.cycle_id, resumed.state.cycle_id);
    assert_ne!(resumed.input.id, retained_input.id);
    let input_after =
        zuno_db::inbox::read_in(&fixture.pool.get().unwrap(), SESSION, &retained_input.id)
            .unwrap()
            .unwrap();
    assert_eq!(input_after.state, zuno_db::inbox::SubmissionState::Consumed);
    assert_eq!(input_after.cycle_id, resumed.state.cycle_id);
    assert_eq!(input_after.prompt, retained_input.prompt);
    assert_eq!(
        input_after.admitted_sequence,
        retained_input.admitted_sequence
    );
    assert_eq!(
        legacy_message_rows(&fixture),
        history_before,
        "resume must not append the retained text again"
    );
    assert_eq!(
        fixture
            .pool
            .get()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM session_input WHERE session_id=?1",
                [SESSION],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        inputs_before + 1,
        "only the new native resume control is admitted"
    );
    assert_eq!(
        log.read_of_type_after(SESSION, "session.work_cycle.authorized", None)
            .unwrap()
            .len(),
        authorized_before + 1
    );

    let events_after = log.read_after(SESSION, None).unwrap();
    let repeated = fixture
        .control
        .resume_session(SESSION, state.revision, 111)
        .unwrap();
    assert_eq!(repeated.state, resumed.state);
    assert_eq!(repeated.input, resumed.input);
    assert_eq!(log.read_after(SESSION, None).unwrap(), events_after);
    assert_eq!(legacy_message_rows(&fixture), history_before);
    assert_eq!(
        zuno_db::inbox::pending_in(&fixture.pool.get().unwrap(), SESSION).unwrap(),
        vec![resumed.input]
    );
    for old in old_receipts {
        assert_eq!(receipts.get(SESSION, &old.input_id).unwrap().unwrap(), old);
        let input = zuno_db::inbox::read_in(&fixture.pool.get().unwrap(), SESSION, &old.input_id)
            .unwrap()
            .unwrap();
        assert_eq!(input.state, zuno_db::inbox::SubmissionState::Consumed);
        assert_ne!(
            input.cycle_id, resumed.state.cycle_id,
            "failed legacy input is not rebound or replayed"
        );
    }
    let current = zuno_db::session_work_cycle::current_in(&fixture.pool.get().unwrap(), SESSION)
        .unwrap()
        .unwrap();
    assert!(
        current.resumed_goal_cycles.is_empty(),
        "ordinary resume does not authorize old callback delivery"
    );
}

fn legacy_message_rows(fixture: &Fixture) -> Vec<(String, String)> {
    let connection = fixture.pool.get().unwrap();
    let mut statement = connection
        .prepare("SELECT id,data FROM message WHERE session_id=?1 ORDER BY id")
        .unwrap();
    statement
        .query_map([SESSION], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn legacy_pause_provenance_keeps_pending_authority_and_unlogged_pause() {
    for barrier in ["pending_human", "goal", "unlogged_pause", "question_event"] {
        let fixture = Fixture::new();
        legacy_pause_after_turns(&fixture, 2);
        legacy_failed_bridge(&fixture, "retained-before-authority", 50);
        match barrier {
            "pending_human" => {
                zuno_db::human_request::HumanRequestStore::new(fixture.pool.clone())
                    .create(zuno_db::human_request::NewHumanRequest {
                        id: "pending-permission".to_owned(),
                        session_id: SESSION.to_owned(),
                        goal_id: None,
                        kind: zuno_db::human_request::HumanRequestKind::Permission,
                        payload: json!({"operation":"independent protected action"}),
                        message_id: None,
                        call_id: None,
                        time_created: 80,
                    })
                    .unwrap();
            }
            "goal" => {
                fixture
                    .goals
                    .create_goal(SESSION, "separate objective", None)
                    .unwrap();
                fixture
                    .goals
                    .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
                    .unwrap();
            }
            "unlogged_pause" => {
                let state = fixture.control.state(SESSION).unwrap().unwrap();
                zuno_db::session_execution::SessionExecutionStore::new(fixture.pool.clone())
                    .set_scheduling(SESSION, state.revision, state.scheduling.unwrap(), 90)
                    .unwrap();
            }
            "question_event" => {
                zuno_db::event_log::SessionEventLog::new(fixture.pool.clone())
                    .append(SESSION, zuno_db::event_log::NewSessionEvent::new(
                        "question.authorization",
                        json!({"question":{"id":"restored-question","state":"cancelled","revision":2}})
                            .as_object().unwrap().clone(),
                    ).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }
        activate(&fixture, "request-after-authority-change", 100);
        assert_legacy_user_gate(&fixture);
    }
}

#[test]
fn legacy_pause_provenance_cannot_be_invoked_by_automatic_user_shaped_input() {
    let fixture = Fixture::new();
    legacy_pause_after_turns(&fixture, 2);
    let before = fixture.control.state(SESSION).unwrap();
    let result = fixture.pool.try_transaction(|tx| {
        zuno_db::inbox::admit_and_promote_in(
            tx,
            zuno_db::inbox::NewSessionInput::new(
                "automatic-input",
                SESSION,
                json!({"kind":"acpPrompt","text":"not a real user request"}),
                zuno_db::inbox::InputDelivery::Queue,
                100,
            )
            .with_trigger_kind(InputTriggerKind::User),
        )?;
        // Directly exercise the boundary's own source fence. Normally inbox
        // promotion already rejects this invalid Automatic producer below.
        tx.execute(
            "UPDATE session_input SET trigger_kind='automatic'
             WHERE session_id=?1 AND id='automatic-input'",
            [SESSION],
        )
        .map_err(zuno_db::map_error)?;
        SessionControlService::activate_user_input_in(
            tx,
            SESSION,
            "automatic-input",
            "automatic-input",
            CollaborationMode::Work,
            Fixture::identity(),
            100,
        )
    });
    assert!(
        matches!(result, Err(SessionControlError::CorruptState { .. })),
        "boundary must reject the non-user producer: {result:?}"
    );
    assert_eq!(fixture.control.state(SESSION).unwrap(), before);
}

#[test]
fn legacy_pause_provenance_automatic_user_shaped_input_is_not_promotable() {
    let fixture = Fixture::new();
    legacy_pause_after_turns(&fixture, 2);
    let before = fixture.control.state(SESSION).unwrap();
    let result = fixture.pool.transaction(|tx| {
        zuno_db::inbox::admit_and_promote_in(
            tx,
            zuno_db::inbox::NewSessionInput::new(
                "automatic-input",
                SESSION,
                json!({"kind":"acpPrompt","text":"not a real user request"}),
                zuno_db::inbox::InputDelivery::Queue,
                100,
            )
            .with_trigger_kind(InputTriggerKind::Automatic),
        )
    });
    assert!(
        result.is_err(),
        "inbox must reject before native activation: {result:?}"
    );
    assert_eq!(fixture.control.state(SESSION).unwrap(), before);
}

#[tokio::test]
async fn legacy_pause_provenance_cancelled_required_question_survives_retained_bridge() {
    use zuno_tool::question::QuestionPort as _;
    use zuno_types::question::{
        QuestionAction, QuestionCommand, QuestionMode, QuestionOption, QuestionOrigin,
        QuestionPurpose, QuestionRequest, QuestionSpec,
    };
    let fixture = Fixture::new();
    legacy_pause_after_turns(&fixture, 2);
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    let old_scheduling = state.scheduling.unwrap();
    // A later legitimate interaction gets its own readiness, then QuestionService
    // creates the wait and Cancel establishes a *different* paused/user cause.
    zuno_db::session_execution::SessionExecutionStore::new(fixture.pool.clone())
        .set_scheduling(SESSION, state.revision, SessionScheduling::default(), 40)
        .unwrap();
    let questions = zuno_session_control::QuestionService::new(fixture.pool.clone());
    let opened = questions
        .open(QuestionSpec {
            origin: QuestionOrigin {
                session_id: SESSION.to_owned(),
                message_id: Some("followup-1".to_owned()),
                call_id: Some("later-required-question".to_owned()),
                turn_id: Some("followup-turn-1".to_owned()),
                goal_id: None,
            },
            mode: QuestionMode::Deferred,
            purpose: QuestionPurpose::RequiredInput,
            questions: vec![QuestionRequest::closed(
                "Approve the separate action?",
                "Approval",
                vec![
                    QuestionOption::new("approve", "Approve"),
                    QuestionOption::new("keep", "Keep paused"),
                ],
            )],
            expected_goal_revision: None,
            plan: None,
        })
        .await
        .unwrap();
    questions
        .apply(
            SESSION,
            &opened.question.id,
            QuestionCommand {
                command_id: "cancel-later-question".to_owned(),
                expected_revision: opened.question.revision,
                action: QuestionAction::Cancel,
            },
        )
        .await
        .unwrap();
    let humans = zuno_db::human_request::HumanRequestStore::new(fixture.pool.clone());
    assert!(humans.pending(Some(SESSION)).unwrap().is_empty());
    assert_eq!(
        humans.get(&opened.question.id).unwrap().unwrap().state,
        zuno_db::human_request::HumanRequestState::Cancelled
    );
    assert_eq!(
        fixture.control.state(SESSION).unwrap().unwrap().scheduling,
        Some(old_scheduling.clone())
    );
    let bridge_at = zuno_db::message::now_millis() + 10;
    legacy_failed_bridge(&fixture, "retained-after-human-cancel", bridge_at);
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .unwrap()
            .unwrap()
            .time_updated,
        bridge_at
    );
    assert_eq!(
        fixture.control.state(SESSION).unwrap().unwrap().scheduling,
        Some(old_scheduling)
    );
    activate_with_trigger(
        &fixture,
        "new-after-human-cancel",
        bridge_at + 10,
        InputTriggerKind::Legacy,
    );
    assert_legacy_user_gate(&fixture);
}

#[test]
fn legacy_pause_provenance_requires_complete_original_turn_evidence() {
    for missing in [
        "driver_start",
        "turn_start",
        "cancel_receipt",
        "cancel_source",
        "turn_trigger",
    ] {
        let fixture = Fixture::new();
        legacy_pause_after_turns(&fixture, 2);
        let connection = fixture.pool.get().unwrap();
        match missing {
            "driver_start" => connection.execute(
                "DELETE FROM event WHERE aggregate_id=?1 AND type='session.driver.phase.1'
                 AND json_extract(data,'$.phase')='executing'",
                [SESSION],
            ),
            "turn_start" => connection.execute(
                "DELETE FROM event WHERE aggregate_id=?1 AND type='session.turn.started.1'
                 AND json_extract(data,'$.turnID')=?2",
                [SESSION, LEGACY_TURN],
            ),
            "cancel_receipt" => connection.execute(
                "DELETE FROM event WHERE aggregate_id=?1 AND type='session.input.receipt.1'
                 AND json_extract(data,'$.turnId')=?2 AND json_extract(data,'$.state')='cancelled'",
                [SESSION, LEGACY_TURN],
            ),
            "cancel_source" => connection.execute(
                "UPDATE message SET data=json_remove(data,'$.error.data.source')
                 WHERE session_id=?1 AND id='legacy-user-assistant'",
                [SESSION],
            ),
            "turn_trigger" => connection.execute(
                "UPDATE event SET data=json_remove(data,'$.turnTrigger')
                 WHERE aggregate_id=?1 AND type='session.turn.started.1'
                   AND json_extract(data,'$.turnID')=?2",
                [SESSION, LEGACY_TURN],
            ),
            _ => unreachable!(),
        }
        .unwrap();
        drop(connection);
        activate(&fixture, "unproven-new-request", 100);
        assert_legacy_user_gate(&fixture);
    }
}

#[test]
fn legacy_pause_provenance_does_not_treat_automatic_turns_as_user_followups() {
    for turn in [LEGACY_TURN, "followup-turn-0", "followup-turn-1"] {
        for alter_input in [false, true] {
            let fixture = Fixture::new();
            legacy_pause_after_turns(&fixture, 2);
            let connection = fixture.pool.get().unwrap();
            if alter_input {
                connection
                    .execute(
                        "UPDATE session_input SET trigger_kind='automatic' WHERE session_id=?1
                     AND id IN (SELECT input_id FROM session_input_receipt WHERE turn_id=?2)",
                        [SESSION, turn],
                    )
                    .unwrap();
            } else {
                connection
                    .execute(
                        "UPDATE event SET data=json_set(data,'$.turnTrigger','automatic')
                     WHERE aggregate_id=?1 AND type='session.turn.started.1'
                       AND json_extract(data,'$.turnID')=?2",
                        [SESSION, turn],
                    )
                    .unwrap();
            }
            drop(connection);
            activate_with_trigger(
                &fixture,
                "new-input-after-automatic",
                100,
                InputTriggerKind::Legacy,
            );
            assert_legacy_user_gate(&fixture);
        }
    }
}

#[test]
fn legacy_pause_provenance_does_not_erase_a_later_explicit_pause() {
    for (followups, failed_bridges) in [(0, 0), (0, 2), (2, 0), (2, 2)] {
        let fixture = Fixture::new();
        legacy_pause_after_turns(&fixture, followups);
        if followups == 0 {
            assert_exact_latest_abort_matches(&fixture);
        }
        for index in 0..failed_bridges {
            legacy_failed_bridge(
                &fixture,
                &format!("retained-{index}"),
                50 + i64::from(index) * 10,
            );
        }
        let state = fixture.control.state(SESSION).unwrap().unwrap();
        zuno_db::session_execution::SessionExecutionStore::new(fixture.pool.clone())
            .set_scheduling(SESSION, state.revision, state.scheduling.unwrap(), 90)
            .unwrap();
        // A distinct pause is not the original interrupted turn's pause,
        // even when a client reuses the same cycle and the reason is still User.
        legacy_phase(&fixture, &state.cycle_id.unwrap(), "paused", Some("user"));
        activate(&fixture, "request-after-explicit-pause", 100);
        assert_legacy_user_gate(&fixture);
    }
}

#[test]
fn legacy_pause_provenance_unknown_pause_before_one_bridge_stays_gated() {
    assert_unknown_pause_before_retained_bridges_stays_gated(1);
}

#[test]
fn legacy_pause_provenance_unknown_pause_before_multiple_bridges_stays_gated() {
    assert_unknown_pause_before_retained_bridges_stays_gated(3);
}

fn assert_unknown_pause_before_retained_bridges_stays_gated(bridges: usize) {
    let fixture = Fixture::new();
    legacy_pause_after_turns(&fixture, 2);
    let log = zuno_db::event_log::SessionEventLog::new(fixture.pool.clone());
    let events_before = log.read_after(SESSION, None).unwrap();
    let driver_before = log.latest_of_type(SESSION, "session.driver.phase").unwrap();
    assert_eq!(driver_before.as_ref().unwrap().sequence, 114);
    let before = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(before.time_updated, 30);
    let scheduling = before.scheduling.clone().unwrap();

    // After the exact A/B/C sequence, an independent native state writer sets
    // the *same* Paused(User) value. No new driver event identifies that cause.
    let unknown_pause =
        zuno_db::session_execution::SessionExecutionStore::new(fixture.pool.clone())
            .set_scheduling(SESSION, before.revision, scheduling.clone(), 40)
            .unwrap();
    assert_eq!(unknown_pause.time_updated, 40);
    assert_eq!(unknown_pause.scheduling, Some(scheduling.clone()));
    assert_eq!(log.read_after(SESSION, None).unwrap(), events_before);
    assert_eq!(
        fixture
            .pool
            .get()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM human_request WHERE session_id=?1",
                [SESSION],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0,
        "this counterexample has no human request or permission barrier"
    );

    let receipts = zuno_db::input_receipt::InputReceiptStore::new(fixture.pool.clone());
    let mut old_receipts = Vec::new();
    for index in 0..bridges {
        let input_id = format!("unknown-pause-bridge-{index}");
        let at = 50 + i64::try_from(index).unwrap() * 10;
        legacy_failed_bridge(&fixture, &input_id, at);
        let state = fixture.control.state(SESSION).unwrap().unwrap();
        assert_eq!(state.time_updated, at, "v32 overwrote the pause timestamp");
        assert_eq!(state.scheduling, Some(scheduling.clone()));
        assert_eq!(
            log.latest_of_type(SESSION, "session.driver.phase").unwrap(),
            driver_before
        );
        let receipt = receipts.get(SESSION, &input_id).unwrap().unwrap();
        assert_eq!(
            receipt.state,
            zuno_types::admission::InputReceiptState::Failed
        );
        assert!(receipt.turn_id.is_none() && receipt.applied_at.is_none());
        old_receipts.push(receipt);
    }

    activate_with_trigger(
        &fixture,
        "new-after-unknown-pause-bridges",
        100,
        InputTriggerKind::Legacy,
    );
    let after = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(
        after.scheduling,
        Some(scheduling),
        "a retained v32 bridge cannot prove whether its identical paused/user \
         snapshot came from A's cancellation or the later unknown pause"
    );
    assert_legacy_user_gate(&fixture);
    for receipt in old_receipts {
        assert_eq!(
            receipts.get(SESSION, &receipt.input_id).unwrap(),
            Some(receipt)
        );
    }
    let started = log
        .latest_of_type(SESSION, "session.work_cycle.started")
        .unwrap()
        .unwrap();
    assert_eq!(started.properties["protectedGateRetained"], true);
    assert_eq!(started.properties["legacyInterruption"], false);
    assert!(
        started
            .properties
            .get("legacyInterruptionProof")
            .is_none_or(serde_json::Value::is_null)
    );
}

#[test]
fn legacy_pause_provenance_keeps_later_safety_gates() {
    for reason in [
        SessionPauseReason::Authentication,
        SessionPauseReason::TurnBudget,
        SessionPauseReason::Blocked,
        SessionPauseReason::UncertainSideEffect,
    ] {
        let fixture = Fixture::new();
        legacy_pause_after_turns(&fixture, 2);
        legacy_failed_bridge(&fixture, "retained-before-safety", 50);
        let state = fixture.control.state(SESSION).unwrap().unwrap();
        zuno_db::session_execution::SessionExecutionStore::new(fixture.pool.clone())
            .set_scheduling(
                SESSION,
                state.revision,
                SessionScheduling {
                    readiness: SessionReadiness::Paused { reason },
                    ..Default::default()
                },
                90,
            )
            .unwrap();
        activate(&fixture, "request-after-safety-gate", 100);
        assert_eq!(
            fixture
                .control
                .state(SESSION)
                .unwrap()
                .unwrap()
                .scheduling
                .unwrap()
                .readiness,
            SessionReadiness::Paused { reason }
        );
    }
}

#[test]
fn legacy_pause_provenance_rejects_missing_or_changed_bridge_evidence() {
    for changed in [
        "missing_link",
        "applied_input",
        "active_turn",
        "changed_scheduling",
    ] {
        let fixture = Fixture::new();
        legacy_pause_after_turns(&fixture, 2);
        legacy_failed_bridge(&fixture, "retained-once", 50);
        let connection = fixture.pool.get().unwrap();
        match changed {
            "missing_link" => connection.execute(
                "DELETE FROM event WHERE aggregate_id=?1 AND type='session.work_cycle.started.1'",
                [SESSION],
            ),
            "applied_input" => connection.execute(
                "UPDATE session_input_receipt SET applied_at=70,turn_id='later-turn'
                 WHERE input_id='retained-once'",
                [],
            ),
            "active_turn" => connection.execute(
                "UPDATE session_work_cycle SET data=json_set(data,'$.activeTurnId','later-turn')
                 WHERE session_id=?1 AND cycle_id='input_retained-once'",
                [SESSION],
            ),
            "changed_scheduling" => connection.execute(
                "UPDATE session_execution_state SET scheduling=json_set(
                     scheduling,'$.progressFingerprint','later-independent-pause')
                 WHERE session_id=?1",
                [SESSION],
            ),
            _ => unreachable!(),
        }
        .unwrap();
        drop(connection);
        activate(&fixture, "request-after-broken-bridge", 100);
        assert_legacy_user_gate(&fixture);
    }
}

#[test]
fn legacy_pause_provenance_never_reauthorizes_old_callback_cycles() {
    let fixture = Fixture::new();
    legacy_pause_after_turns(&fixture, 2);
    legacy_failed_bridge(&fixture, "retained-once", 50);
    let gated_cycle = activate(&fixture, "new-live-input", 100);
    assert_legacy_user_gate(&fixture);
    let paused = fixture.control.state(SESSION).unwrap().unwrap();
    let inbox = zuno_db::inbox::SessionInbox::new(fixture.pool.clone());
    let mut reports = Vec::new();
    for (index, origin) in [
        LEGACY_CYCLE,
        "input_retained-once",
        gated_cycle.cycle_id.as_str(),
    ]
    .into_iter()
    .enumerate()
    {
        let report = inbox.admit(
            zuno_db::inbox::NewSessionInput::new(
                format!("late-report-{index}"), SESSION,
                json!({"kind":"backgroundExecutionReport","executionID":format!("old-job-{index}")}),
                zuno_db::inbox::InputDelivery::Queue, 110,
            ).with_trigger_kind(InputTriggerKind::Automatic).with_cycle_id(Some(origin.to_owned())),
        ).unwrap();
        assert_eq!(
            zuno_db::session_wake::admission_in(&fixture.pool.get().unwrap(), &report).unwrap(),
            zuno_types::execution::WakeAdmission::Reject
        );
        assert_eq!(fixture.control.state(SESSION).unwrap().unwrap(), paused);
        reports.push(report);
    }
    let resumed = fixture
        .control
        .resume_session(SESSION, paused.revision, 120)
        .unwrap();
    assert_eq!(
        resumed.state.scheduling.as_ref().unwrap().readiness,
        SessionReadiness::Ready
    );
    for report in reports {
        assert_eq!(
            zuno_db::session_wake::admission_in(&fixture.pool.get().unwrap(), &report).unwrap(),
            zuno_types::execution::WakeAdmission::Reject,
            "explicit ordinary resume must not transfer delivery authority for {}",
            report.id,
        );
        assert_eq!(
            fixture.control.state(SESSION).unwrap().unwrap(),
            resumed.state
        );
        assert_eq!(
            inbox.get(SESSION, &report.id).unwrap().unwrap().state,
            zuno_db::inbox::SubmissionState::Queued,
            "retained evidence was not consumed by a model"
        );
    }
    let current = zuno_db::session_work_cycle::current_in(&fixture.pool.get().unwrap(), SESSION)
        .unwrap()
        .unwrap();
    assert_eq!(
        Some(current.cycle_id.as_str()),
        resumed.state.cycle_id.as_deref()
    );
    assert!(current.resumed_goal_cycles.is_empty());
}
