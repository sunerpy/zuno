use super::*;
use serde_json::json;
use zuno_db::inbox::{InputDelivery, NewSessionInput};
use zuno_types::execution::{SessionPauseReason, SessionReadiness, SessionScheduling};

fn prepare(reason: SessionPauseReason) -> Fixture {
    let fixture = Fixture::new();
    fixture
        .goals
        .create_goal(SESSION, "Remote operation", None)
        .unwrap();
    fixture
        .goals
        .pause_with_reason(SESSION, GoalPauseReason::UncertainSideEffect)
        .unwrap();
    fixture
        .pool
        .transaction(|tx| {
            let state = zuno_db::session_execution::seed_in(
                tx,
                SESSION,
                CollaborationMode::Work,
                Some(Fixture::identity()),
                1,
            )?;
            zuno_db::session_execution::set_scheduling_in(
                tx,
                SESSION,
                state.revision,
                SessionScheduling {
                    readiness: SessionReadiness::Paused { reason },
                    ..Default::default()
                },
                2,
            )?;
            Ok(())
        })
        .unwrap();
    fixture
        .pool
        .try_transaction(|tx| {
            zuno_db::inbox::admit_and_promote_in(
                tx,
                NewSessionInput::new(
                    "discussion-input",
                    SESSION,
                    json!({"kind":"acpPrompt","text":"Explain only"}),
                    InputDelivery::Steer,
                    3,
                )
                .with_trigger_kind(InputTriggerKind::User),
            )?;
            tx.execute(
                "INSERT INTO message(id,session_id,time_created,time_updated,data)
            VALUES('discussion-input',?1,3,3,'{\"role\":\"user\"}')",
                [SESSION],
            )
            .map_err(zuno_db::map_error)?;
            SessionControlService::activate_user_input_in(
                tx,
                SESSION,
                "discussion-input",
                "discussion-input",
                CollaborationMode::Work,
                Fixture::identity(),
                4,
            )?;
            zuno_db::inbox::mark_consumed_in(tx, SESSION, "discussion-input")?;
            Ok::<_, SessionControlError>(())
        })
        .unwrap();
    fixture
}

#[test]
fn discussion_claim_binds_exact_input_once_and_keeps_all_safety_state() {
    let fixture = prepare(SessionPauseReason::Blocked);
    let state = fixture.control.state(SESSION).unwrap();
    let goal = fixture.goals.goal(SESSION).unwrap();
    let pause = fixture.goals.pause_state(SESSION).unwrap();
    let candidate = fixture
        .control
        .pending_discussion(SESSION)
        .unwrap()
        .unwrap();
    assert_eq!(candidate.input_id, "discussion-input");
    assert!(
        fixture
            .control
            .claim_discussion(&candidate, "discussion-turn", 6)
            .unwrap()
    );
    assert!(
        !fixture
            .control
            .claim_discussion(&candidate, "duplicate-turn", 7)
            .unwrap()
    );
    assert!(
        fixture
            .control
            .pending_discussion(SESSION)
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.control.state(SESSION).unwrap(), state);
    assert_eq!(fixture.goals.goal(SESSION).unwrap(), goal);
    assert_eq!(fixture.goals.pause_state(SESSION).unwrap(), pause);
    let connection = fixture.pool.get().unwrap();
    let receipt = zuno_db::input_receipt::get_in(&connection, SESSION, "discussion-input")
        .unwrap()
        .unwrap();
    assert_eq!(receipt.turn_id.as_deref(), Some("discussion-turn"));
    assert!(receipt.applied_at.is_none());
}

#[test]
fn discussion_does_not_waive_authentication_budget_or_approval() {
    for reason in [
        SessionPauseReason::Authentication,
        SessionPauseReason::TurnBudget,
        SessionPauseReason::User,
        SessionPauseReason::NoProgress,
    ] {
        let fixture = prepare(reason);
        assert!(
            fixture
                .control
                .pending_discussion(SESSION)
                .unwrap()
                .is_none(),
            "{reason:?}"
        );
    }
    for mutation in [
        "INSERT INTO human_request(id,session_id,kind,state,payload,revision,time_created,time_updated)
         VALUES('approval','ses_control','permission','pending','{}',1,5,5)",
        "UPDATE goal SET token_budget=1,tokens_used=1 WHERE session_id='ses_control'",
        "UPDATE goal SET status='active' WHERE session_id='ses_control'",
        "UPDATE session_input SET trigger_kind='automatic' WHERE id='discussion-input'",
        "UPDATE session_input_receipt SET state='failed' WHERE input_id='discussion-input'",
        "UPDATE session_work_cycle SET data=json_set(data,'$.goalId','old-goal')
         WHERE session_id='ses_control'",
    ] {
        let fixture = prepare(SessionPauseReason::Blocked);
        fixture.pool.get().unwrap().execute_batch(mutation).unwrap();
        assert!(fixture.control.pending_discussion(SESSION).unwrap().is_none(), "{mutation}");
    }
}

#[test]
fn discussion_claim_rechecks_revision_and_never_clears_actual_uncertainty() {
    let fixture = prepare(SessionPauseReason::UncertainSideEffect);
    let candidate = fixture
        .control
        .pending_discussion(SESSION)
        .unwrap()
        .unwrap();
    fixture
        .pool
        .transaction(|tx| {
            let state = zuno_db::session_execution::read_in(tx, SESSION)?.unwrap();
            zuno_db::session_execution::set_paused_in(
                tx,
                SESSION,
                state.revision,
                SessionPauseReason::Authentication,
                5,
            )
            .map(|_| ())
        })
        .unwrap();
    let before = fixture.control.state(SESSION).unwrap();
    assert!(
        !fixture
            .control
            .claim_discussion(&candidate, "stale", 6)
            .unwrap()
    );
    assert_eq!(fixture.control.state(SESSION).unwrap(), before);
}
