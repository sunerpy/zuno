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
    fixture.pool.try_transaction(|tx| {
        tx.execute(
            "INSERT INTO message(id,session_id,time_created,time_updated,data) VALUES(?1,?2,?3,?3,'{\"role\":\"user\"}')",
            rusqlite::params![id, SESSION, at],
        ).map_err(zuno_db::map_error)?;
        zuno_db::inbox::admit_and_promote_in(tx,
            zuno_db::inbox::NewSessionInput::new(
                id, SESSION, json!({"kind":"tuiPrompt","text":"independent user request"}),
                zuno_db::inbox::InputDelivery::Queue, at,
            ).with_trigger_kind(InputTriggerKind::User),
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
    assert_eq!(
        fixture.control.state(SESSION).unwrap().unwrap().phase,
        SessionExecutionPhase::Paused
    );
    activate(&fixture, "new-after-legacy-stop", 10);
    let state = fixture.control.state(SESSION).unwrap().unwrap();
    assert_eq!(state.scheduling.unwrap().readiness, SessionReadiness::Ready);
    assert_eq!(
        state.continuation.unwrap().context_epoch,
        7,
        "a new request must not reset compaction accounting"
    );
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
