use super::*;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SubmissionState};
use zuno_session_control::QuestionService;
use zuno_tool::question::QuestionPort;
use zuno_types::execution::{SessionPauseReason, SessionReadiness, SessionScheduling};
use zuno_types::goal_resume::GoalResumeRequest;
use zuno_types::goal_resume::{KEEP_GOAL_PAUSED_CHOICE, RESUME_GOAL_CHOICE};
use zuno_types::question::{QuestionAction, QuestionCommand, QuestionView};

fn paused(fixture: &Fixture, reason: GoalPauseReason) -> GoalResumeRequest {
    fixture
        .goals
        .create_goal(SESSION, "Deliver the approved change", None)
        .expect("goal");
    fixture
        .pool
        .transaction(|tx| {
            let state = zuno_db::session_execution::seed_in(
                tx,
                SESSION,
                CollaborationMode::Work,
                Some(Fixture::identity()),
                10,
            )?;
            zuno_db::session_execution::set_scheduling_in(
                tx,
                SESSION,
                state.revision,
                SessionScheduling {
                    readiness: SessionReadiness::Paused {
                        reason: SessionPauseReason::User,
                    },
                    ..SessionScheduling::default()
                },
                11,
            )?;
            Ok(())
        })
        .expect("state");
    fixture
        .goals
        .pause_with_reason(SESSION, reason)
        .expect("pause");
    let goal = fixture.goals.goal(SESSION).expect("goal").expect("present");
    GoalResumeRequest {
        session_id: SESSION.to_owned(),
        goal_id: goal.goal_id,
        expected_revision: goal.revision,
        input_id: None,
    }
}

#[test]
fn generic_goal_resume_cannot_clear_authentication_or_uncertain_side_effects() {
    for reason in [
        GoalPauseReason::Authentication,
        GoalPauseReason::UncertainSideEffect,
        GoalPauseReason::TurnBudget,
        GoalPauseReason::Permission,
        GoalPauseReason::HumanInput,
    ] {
        let fixture = Fixture::new();
        let request = paused(&fixture, reason);
        let before = fixture.control.state(SESSION).expect("state");
        assert!(
            fixture.control.resume_goal(&request, 20).is_err(),
            "generic Goal consent must not clear {reason}"
        );
        assert_eq!(fixture.control.state(SESSION).expect("state"), before);
        assert_eq!(
            fixture
                .goals
                .goal(SESSION)
                .expect("goal")
                .expect("goal")
                .status,
            GoalStatus::Paused
        );
    }
}

fn input(fixture: &Fixture, id: &str) -> zuno_db::inbox::SessionInput {
    SessionInbox::new(Arc::clone(&fixture.pool))
        .admit(NewSessionInput::new(
            id,
            SESSION,
            serde_json::json!({"text":"What is the current status?"}),
            InputDelivery::Queue,
            15,
        ))
        .expect("input")
}

fn choose(view: &QuestionView, id: &str, choice: &str) -> QuestionCommand {
    QuestionCommand {
        command_id: id.to_owned(),
        expected_revision: view.revision,
        action: QuestionAction::Answer {
            answers: [(view.questions[0].id.clone(), vec![choice.to_owned()])]
                .into_iter()
                .collect(),
        },
    }
}

#[tokio::test]
async fn skipped_resume_and_keep_paused_never_authorize_or_inject_an_answer() {
    let fixture = Fixture::new();
    paused(&fixture, GoalPauseReason::UserInterruption);
    let original = input(&fixture, "new_input");
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let first = service
        .offer_goal_resume(SESSION, Some(&original.id))
        .await
        .expect("offer")
        .expect("choice");
    let duplicate = service
        .offer_goal_resume(SESSION, Some(&original.id))
        .await
        .expect("offer")
        .expect("same choice");
    assert!(duplicate.duplicate);
    assert_eq!(first.question.id, duplicate.question.id);
    let before = fixture.control.state(SESSION).expect("state");
    let deferred = service
        .apply(
            SESSION,
            &first.question.id,
            QuestionCommand {
                command_id: "skip".to_owned(),
                expected_revision: first.question.revision,
                action: QuestionAction::Defer {
                    draft_answers: Default::default(),
                },
            },
        )
        .await
        .expect("defer");
    assert!(deferred.input_id.is_none());
    assert_eq!(fixture.control.state(SESSION).expect("state"), before);
    let kept = service
        .apply(
            SESSION,
            &first.question.id,
            choose(&deferred.question, "keep", KEEP_GOAL_PAUSED_CHOICE),
        )
        .await
        .expect("keep paused");
    assert!(kept.input_id.is_none());
    assert_eq!(
        fixture
            .goals
            .goal(SESSION)
            .expect("goal")
            .expect("goal")
            .status,
        GoalStatus::Paused
    );
    assert!(
        service
            .offer_goal_resume(SESSION, Some(&original.id))
            .await
            .expect("offer")
            .is_none(),
        "the same Goal revision must not nag after an explicit Keep paused"
    );
    let connection = fixture.pool.get().expect("connection");
    let count: i64 = connection
        .query_row("SELECT count(*) FROM session_input", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn resume_binds_pending_input_once_and_rejects_stale_goal_revision() {
    let fixture = Fixture::new();
    paused(&fixture, GoalPauseReason::UserInterruption);
    let original = input(&fixture, "new_input");
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let offer = service
        .offer_goal_resume(SESSION, Some(&original.id))
        .await
        .expect("offer")
        .expect("choice");
    let command = choose(&offer.question, "resume", RESUME_GOAL_CHOICE);
    let resumed = service
        .apply(SESSION, &offer.question.id, command.clone())
        .await
        .expect("resume");
    assert_eq!(resumed.input_id.as_deref(), Some(original.id.as_str()));
    assert_eq!(
        fixture
            .goals
            .goal(SESSION)
            .expect("goal")
            .expect("goal")
            .status,
        GoalStatus::Active
    );
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .expect("state")
            .expect("state")
            .scheduling
            .expect("scheduling")
            .readiness,
        SessionReadiness::Ready
    );
    let replay = service
        .apply(SESSION, &offer.question.id, command)
        .await
        .expect("retry");
    assert!(replay.duplicate);
    assert_eq!(replay.input_id, resumed.input_id);

    fixture
        .goals
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .expect("pause");
    let stale = service
        .offer_goal_resume(SESSION, Some(&original.id))
        .await
        .expect("offer")
        .expect("choice");
    fixture
        .goals
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .expect("advance revision");
    assert!(
        service
            .apply(
                SESSION,
                &stale.question.id,
                choose(&stale.question, "stale", RESUME_GOAL_CHOICE)
            )
            .await
            .is_err()
    );
    assert_eq!(
        service
            .get(SESSION, &stale.question.id)
            .await
            .expect("choice")
            .revision,
        stale.question.revision
    );
}

#[tokio::test]
async fn resume_after_input_was_processed_queues_control_without_replaying_text() {
    let fixture = Fixture::new();
    paused(&fixture, GoalPauseReason::UserInterruption);
    let original = input(&fixture, "already_processed");
    let inbox = SessionInbox::new(Arc::clone(&fixture.pool));
    inbox.promote_id(SESSION, &original.id).expect("promote");
    inbox
        .mark_consumed(SESSION, &original.id)
        .expect("record input");
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let offer = service
        .offer_goal_resume(SESSION, Some(&original.id))
        .await
        .expect("offer")
        .expect("choice");
    let resumed = service
        .apply(
            SESSION,
            &offer.question.id,
            choose(&offer.question, "resume", RESUME_GOAL_CHOICE),
        )
        .await
        .expect("resume");
    let connection = fixture.pool.get().expect("connection");
    let control = zuno_db::inbox::read_in(
        &connection,
        SESSION,
        resumed.input_id.as_deref().expect("control"),
    )
    .expect("read")
    .expect("control");
    assert_ne!(control.id, original.id);
    assert_eq!(control.prompt["kind"], "sessionControl");
    assert!(control.prompt.get("text").is_none());
    assert_eq!(
        zuno_db::inbox::read_in(&connection, SESSION, &original.id)
            .expect("read")
            .expect("input")
            .state,
        SubmissionState::Consumed
    );
}

#[tokio::test]
async fn resume_keeps_exact_external_wait_and_rolls_back_on_inbox_failure() {
    let fixture = Fixture::new();
    let request = paused(&fixture, GoalPauseReason::UserInterruption);
    fixture
        .pool
        .transaction(|tx| {
            let state = zuno_db::session_execution::read_in(tx, SESSION)?.expect("state");
            zuno_db::session_execution::set_waiting_in(
                tx,
                SESSION,
                state.revision,
                zuno_types::execution::SessionWaitReference::External {
                    source_id: "observer".to_owned(),
                    origin_cycle_id: "origin_cycle".to_owned(),
                },
                15,
            )?;
            Ok(())
        })
        .expect("wait");
    let before = fixture.control.state(SESSION).expect("state");
    let resumed = fixture.control.resume_goal(&request, 20).expect("resume");
    assert!(resumed.input.is_none());
    assert_eq!(fixture.control.state(SESSION).expect("state"), before);

    let fixture = Fixture::new();
    paused(&fixture, GoalPauseReason::UserInterruption);
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let offer = service
        .offer_goal_resume(SESSION, None)
        .await
        .expect("offer")
        .expect("choice");
    let before = fixture.control.state(SESSION).expect("state");
    fixture.pool.get().expect("connection").execute_batch(
        "CREATE TRIGGER refuse_resume_input BEFORE INSERT ON session_input BEGIN SELECT RAISE(ABORT,'fixture input failure'); END;"
    ).expect("fault");
    assert!(
        service
            .apply(
                SESSION,
                &offer.question.id,
                choose(&offer.question, "resume", RESUME_GOAL_CHOICE)
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.control.state(SESSION).expect("state"), before);
    assert_eq!(
        fixture
            .goals
            .goal(SESSION)
            .expect("goal")
            .expect("goal")
            .status,
        GoalStatus::Paused
    );
    assert_eq!(
        service
            .get(SESSION, &offer.question.id)
            .await
            .expect("choice"),
        offer.question
    );
}

#[tokio::test]
async fn a_withdrawn_input_cannot_poison_the_next_goal_resume_offer() {
    let fixture = Fixture::new();
    paused(&fixture, GoalPauseReason::UserInterruption);
    let original = input(&fixture, "withdrawn_input");
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let old = service
        .offer_goal_resume(SESSION, Some(&original.id))
        .await
        .expect("offer")
        .expect("choice");
    SessionInbox::new(Arc::clone(&fixture.pool))
        .cancel_pending(SESSION, &original.id, original.revision, 20)
        .expect("withdraw");
    let next_input = input(&fixture, "next_input");
    let next = service
        .offer_goal_resume(SESSION, Some(&next_input.id))
        .await
        .expect("offer")
        .expect("usable choice");
    assert_ne!(
        old.question.id, next.question.id,
        "a cancelled input must not remain the immutable binding of a new offer"
    );
    assert_eq!(
        service
            .get(SESSION, &old.question.id)
            .await
            .expect("old")
            .state,
        zuno_types::question::QuestionState::Cancelled
    );
    let resumed = service
        .apply(
            SESSION,
            &next.question.id,
            choose(&next.question, "resume-next", RESUME_GOAL_CHOICE),
        )
        .await
        .expect("resume");
    assert_eq!(resumed.input_id.as_deref(), Some(next_input.id.as_str()));
    assert_eq!(
        fixture
            .goals
            .goal(SESSION)
            .expect("goal")
            .expect("present")
            .status,
        GoalStatus::Active
    );
}

#[tokio::test]
async fn keep_paused_is_not_superseded_when_its_original_input_is_later_withdrawn() {
    let fixture = Fixture::new();
    paused(&fixture, GoalPauseReason::UserInterruption);
    let original = input(&fixture, "original");
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let offer = service
        .offer_goal_resume(SESSION, Some(&original.id))
        .await
        .expect("offer")
        .expect("choice");
    let kept = service
        .apply(
            SESSION,
            &offer.question.id,
            choose(&offer.question, "keep-paused", KEEP_GOAL_PAUSED_CHOICE),
        )
        .await
        .expect("keep");
    SessionInbox::new(Arc::clone(&fixture.pool))
        .cancel_pending(SESSION, &original.id, original.revision, 20)
        .expect("withdraw");
    let next_input = input(&fixture, "next");
    assert!(
        service
            .offer_goal_resume(SESSION, Some(&next_input.id))
            .await
            .expect("offer")
            .is_none()
    );
    assert_eq!(
        service
            .get(SESSION, &offer.question.id)
            .await
            .expect("decision"),
        kept.question
    );
    assert_eq!(
        fixture
            .goals
            .goal(SESSION)
            .expect("goal")
            .expect("present")
            .status,
        GoalStatus::Paused
    );
}

#[test]
fn explicit_native_resume_can_seed_legacy_identity_atomically_but_cannot_authorize_plan() {
    let fixture = Fixture::new();
    fixture
        .goals
        .create_goal(SESSION, "Legacy Goal", None)
        .unwrap();
    fixture
        .goals
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap();
    let goal = fixture.goals.goal(SESSION).unwrap().unwrap();
    let request = GoalResumeRequest {
        session_id: SESSION.to_owned(),
        goal_id: goal.goal_id,
        expected_revision: goal.revision,
        input_id: None,
    };
    assert!(
        fixture
            .control
            .resume_goal_with_host_identity(
                &request,
                CollaborationMode::Plan,
                Fixture::identity(),
                20,
            )
            .is_err()
    );
    assert!(fixture.control.state(SESSION).unwrap().is_none());
    let resumed = fixture
        .control
        .resume_goal_with_host_identity(&request, CollaborationMode::Work, Fixture::identity(), 21)
        .unwrap();
    assert_eq!(resumed.goal.status, GoalStatus::Active);
    assert_eq!(resumed.state.work_identity, Some(Fixture::identity()));
    assert!(resumed.input.is_some());
}

#[test]
fn explicit_resume_uses_the_reconfigured_trusted_host_identity() {
    let fixture = Fixture::new();
    let request = paused(&fixture, GoalPauseReason::UserInterruption);
    let selected = TurnExecutionIdentity::new("orchestrator", "provider", "model-two");
    let resumed = fixture
        .control
        .resume_goal_with_host_identity(&request, CollaborationMode::Work, selected.clone(), 30)
        .unwrap();
    assert_eq!(resumed.state.work_identity, Some(selected.clone()));
    assert_eq!(resumed.state.continuation.unwrap().identity, selected);
}

#[test]
fn work_selection_preserves_pause_and_does_not_materialize_an_unused_panel() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .control
            .record_work_selection("missing-panel", Fixture::identity(), 10,)
            .unwrap()
            .is_none()
    );
    let request = paused(&fixture, GoalPauseReason::UserInterruption);
    let before = fixture.control.state(SESSION).unwrap().unwrap();
    let selected = TurnExecutionIdentity::new("deep", "provider", "selected-model");
    let changed = fixture
        .control
        .record_work_selection(SESSION, selected.clone(), 21)
        .unwrap()
        .unwrap();
    assert_eq!(changed.scheduling, before.scheduling);
    assert_eq!(changed.mode, before.mode);
    assert_eq!(
        fixture.goals.goal(SESSION).unwrap().unwrap().status,
        GoalStatus::Paused
    );
    let resumed = fixture.control.resume_goal(&request, 22).unwrap();
    assert_eq!(resumed.state.continuation.unwrap().identity, selected);
}
