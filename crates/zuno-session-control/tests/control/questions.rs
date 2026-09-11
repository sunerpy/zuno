use super::*;
use zuno_session_control::QuestionService;
use zuno_tool::question::{QuestionError, QuestionPort};
use zuno_types::execution::{
    SessionPauseReason, SessionReadiness, SessionWakeSignal, WakeAdmission,
};
use zuno_types::question::{
    PlanAuthorizationState, PlanQuestionDecision, QuestionAction, QuestionCommand, QuestionMode,
    QuestionOption, QuestionOrigin, QuestionPurpose, QuestionRequest, QuestionSpec, QuestionState,
    QuestionView,
};

fn spec(purpose: QuestionPurpose) -> QuestionSpec {
    QuestionSpec {
        origin: QuestionOrigin {
            session_id: SESSION.to_owned(),
            message_id: Some("message_plan".to_owned()),
            call_id: Some("call_question".to_owned()),
            turn_id: Some("turn_plan".to_owned()),
            goal_id: None,
        },
        mode: QuestionMode::Deferred,
        purpose,
        questions: vec![QuestionRequest::closed(
            "May the announcement be sent?",
            "Announcement",
            vec![
                QuestionOption::new("yes", "Send"),
                QuestionOption::new("no", "Keep draft"),
            ],
        )],
        expected_goal_revision: None,
        plan: None,
    }
}

fn command(view: &QuestionView, id: &str, action: QuestionAction) -> QuestionCommand {
    QuestionCommand {
        command_id: id.to_owned(),
        expected_revision: view.revision,
        action,
    }
}

fn answer(view: &QuestionView) -> QuestionAction {
    QuestionAction::Answer {
        answers: [(view.questions[0].id.clone(), vec!["yes".to_owned()])]
            .into_iter()
            .collect(),
    }
}

fn approve() -> QuestionAction {
    QuestionAction::PlanDecision {
        decision: PlanQuestionDecision::Approve,
        risk_reason: None,
    }
}

fn count(fixture: &Fixture, table: &str) -> i64 {
    fixture
        .pool
        .get()
        .expect("connection")
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count")
}

#[tokio::test]
async fn ordinary_required_input_waits_without_goal_and_defer_does_not_resume() {
    let fixture = Fixture::new();
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let opened = service
        .open(spec(QuestionPurpose::RequiredInput))
        .await
        .expect("open");
    let state = fixture
        .control
        .state(SESSION)
        .expect("state")
        .expect("seeded");
    assert_eq!(state.phase, SessionExecutionPhase::Waiting);
    assert_eq!(
        state.wake_admission(&SessionWakeSignal::Callback),
        WakeAdmission::Reject
    );
    assert!(fixture.goals.goal(SESSION).expect("goal").is_none());
    let deferred = service
        .apply(
            SESSION,
            &opened.question.id,
            command(
                &opened.question,
                "defer",
                QuestionAction::Defer {
                    draft_answers: Default::default(),
                },
            ),
        )
        .await
        .expect("defer");
    assert_eq!(deferred.question.state, QuestionState::Pending);
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .expect("state")
            .expect("state")
            .phase,
        SessionExecutionPhase::Waiting
    );
    let answered = service
        .apply(
            SESSION,
            &opened.question.id,
            command(&deferred.question, "answer", answer(&deferred.question)),
        )
        .await
        .expect("answer");
    assert_eq!(answered.question.state, QuestionState::Answered);
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .expect("state")
            .expect("state")
            .scheduling
            .expect("schedule")
            .readiness,
        SessionReadiness::Ready
    );
    let duplicate = service
        .apply(
            SESSION,
            &opened.question.id,
            command(&deferred.question, "answer", answer(&deferred.question)),
        )
        .await
        .expect("retry");
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.input_id, answered.input_id);
}

#[tokio::test]
async fn cancellation_leaves_ordinary_work_paused() {
    let fixture = Fixture::new();
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let opened = service
        .open(spec(QuestionPurpose::RequiredInput))
        .await
        .expect("open");
    service
        .apply(
            SESSION,
            &opened.question.id,
            command(&opened.question, "cancel", QuestionAction::Cancel),
        )
        .await
        .expect("cancel");
    let state = fixture
        .control
        .state(SESSION)
        .expect("state")
        .expect("state");
    assert_eq!(
        state.scheduling.expect("schedule").readiness,
        SessionReadiness::Paused {
            reason: SessionPauseReason::User
        }
    );
}

#[tokio::test]
async fn deferred_clarification_does_not_pause_or_resume_a_goal() {
    let fixture = Fixture::new();
    fixture
        .goals
        .create_goal(SESSION, "Deliver", None)
        .expect("goal");
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let opened = service
        .open(spec(QuestionPurpose::Clarification))
        .await
        .expect("open");
    assert_eq!(
        fixture
            .goals
            .goal(SESSION)
            .expect("goal")
            .expect("goal")
            .status,
        GoalStatus::Active
    );
    fixture
        .goals
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .expect("pause");
    service
        .apply(
            SESSION,
            &opened.question.id,
            command(&opened.question, "answer", answer(&opened.question)),
        )
        .await
        .expect("answer");
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

#[tokio::test]
async fn goal_required_input_commits_both_waits_and_only_matching_answer_resumes() {
    let fixture = Fixture::new();
    let goal = fixture
        .goals
        .create_goal(SESSION, "Deliver", None)
        .expect("goal");
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let mut request = spec(QuestionPurpose::RequiredInput);
    request.origin.goal_id = Some(goal.goal_id);
    request.expected_goal_revision = Some(goal.revision);
    let opened = service.open(request).await.expect("open");
    assert_eq!(
        fixture
            .goals
            .goal(SESSION)
            .expect("goal")
            .expect("goal")
            .status,
        GoalStatus::Paused
    );
    let state = fixture
        .control
        .state(SESSION)
        .expect("state")
        .expect("state");
    assert_eq!(
        state.wake_admission(&SessionWakeSignal::UserAnswer {
            request_id: "unrelated".to_owned()
        }),
        WakeAdmission::Reject
    );
    service
        .apply(
            SESSION,
            &opened.question.id,
            command(&opened.question, "answer", answer(&opened.question)),
        )
        .await
        .expect("answer");
    assert_eq!(
        fixture
            .goals
            .goal(SESSION)
            .expect("goal")
            .expect("goal")
            .status,
        GoalStatus::Active
    );
}

#[tokio::test]
async fn stale_goal_revision_rolls_back_question_event_and_wait() {
    let fixture = Fixture::new();
    let goal = fixture
        .goals
        .create_goal(SESSION, "Deliver", None)
        .expect("goal");
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let mut request = spec(QuestionPurpose::RequiredInput);
    request.origin.goal_id = Some(goal.goal_id);
    request.expected_goal_revision = Some(goal.revision + 1);
    let before = count(&fixture, "event");
    assert!(service.open(request).await.is_err());
    assert_eq!(count(&fixture, "human_request"), 0);
    assert_eq!(count(&fixture, "event"), before);
    assert!(fixture.control.state(SESSION).expect("state").is_none());
}

#[tokio::test]
async fn early_plan_approval_waits_for_successful_exact_source_turn() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    fixture.enter_plan();
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let opened = service
        .open(spec(QuestionPurpose::PlanAuthorization))
        .await
        .expect("open");
    assert_eq!(
        opened.question.plan.as_ref().expect("binding").plan_id,
        plan.id
    );
    let approved = service
        .apply(
            SESSION,
            &opened.question.id,
            command(&opened.question, "approve", approve()),
        )
        .await
        .expect("approve");
    assert_eq!(
        approved.question.authorization,
        Some(PlanAuthorizationState::WaitingForHandoff)
    );
    assert!(approved.input_id.is_none());
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .expect("state")
            .expect("state")
            .mode,
        CollaborationMode::Plan
    );
    service
        .complete_plan_turn(SESSION, "different_turn")
        .await
        .expect("unrelated handoff");
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .expect("state")
            .expect("state")
            .mode,
        CollaborationMode::Plan
    );
    service
        .complete_plan_turn(SESSION, "turn_plan")
        .await
        .expect("handoff");
    let final_view = service
        .get(SESSION, &opened.question.id)
        .await
        .expect("question");
    assert_eq!(
        final_view.authorization,
        Some(PlanAuthorizationState::Applied)
    );
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .expect("state")
            .expect("state")
            .mode,
        CollaborationMode::Work
    );
    assert_eq!(count(&fixture, "session_input"), 1);
    let retried = service
        .apply(
            SESSION,
            &opened.question.id,
            command(&opened.question, "approve", approve()),
        )
        .await
        .expect("retry after handoff");
    assert!(retried.duplicate);
    assert_eq!(
        retried.question.authorization,
        Some(PlanAuthorizationState::Applied)
    );
    assert!(retried.input_id.is_some());
    service
        .complete_plan_turn(SESSION, "turn_plan")
        .await
        .expect("idempotent handoff replay");
    assert_eq!(count(&fixture, "session_input"), 1);
}

#[tokio::test]
async fn empty_answers_cannot_authorize_and_repeated_plan_publish_reuses_request() {
    let fixture = Fixture::new();
    fixture.plan();
    fixture.enter_plan();
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let opened = service
        .open(spec(QuestionPurpose::PlanAuthorization))
        .await
        .expect("open");
    let mut repeated = spec(QuestionPurpose::PlanAuthorization);
    repeated.origin.call_id = Some("second_call".to_owned());
    let repeated = service.open(repeated).await.expect("deduplicate");
    assert_eq!(repeated.question.id, opened.question.id);
    assert!(repeated.duplicate);
    assert!(matches!(
        service
            .apply(
                SESSION,
                &opened.question.id,
                command(
                    &opened.question,
                    "empty",
                    QuestionAction::Answer {
                        answers: Default::default()
                    }
                )
            )
            .await,
        Err(QuestionError::Invalid(_))
    ));
    assert_eq!(count(&fixture, "session_input"), 0);
}

#[tokio::test]
async fn interrupted_plan_turn_invalidates_unapplied_consent() {
    let fixture = Fixture::new();
    fixture.plan();
    fixture.enter_plan();
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let opened = service
        .open(spec(QuestionPurpose::PlanAuthorization))
        .await
        .expect("open");
    service
        .apply(
            SESSION,
            &opened.question.id,
            command(&opened.question, "approve", approve()),
        )
        .await
        .expect("approve");
    service
        .interrupt_plan_turn(SESSION, "turn_plan")
        .await
        .expect("interrupt");
    let view = service
        .get(SESSION, &opened.question.id)
        .await
        .expect("get");
    assert_eq!(
        view.authorization,
        Some(PlanAuthorizationState::Invalidated)
    );
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .expect("state")
            .expect("state")
            .mode,
        CollaborationMode::Plan
    );
    assert_eq!(count(&fixture, "session_input"), 0);
    assert_eq!(
        fixture
            .control
            .state(SESSION)
            .expect("state")
            .expect("state")
            .phase,
        SessionExecutionPhase::Paused,
    );
}

#[tokio::test]
async fn plan_changes_make_approval_stale_without_committing_response() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    fixture.enter_plan();
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let opened = service
        .open(spec(QuestionPurpose::PlanAuthorization))
        .await
        .expect("open");
    fixture
        .work
        .update_plan(
            SESSION,
            PlanUpdateParams {
                expected_revision: Some(plan.revision),
                goal_id: None,
                title: "Changed".to_owned(),
                steps: plan.steps,
            },
        )
        .expect("change");
    assert!(matches!(
        service
            .apply(
                SESSION,
                &opened.question.id,
                command(&opened.question, "approve", approve())
            )
            .await,
        Err(QuestionError::Rejected {
            code: "stale_plan",
            ..
        })
    ));
    assert_eq!(
        service
            .get(SESSION, &opened.question.id)
            .await
            .expect("get")
            .revision,
        opened.question.revision
    );
}

#[test]
fn recovery_preserves_pause_and_progress_instead_of_reopening_work() {
    let fixture = Fixture::new();
    fixture
        .control
        .record_continuation(
            SESSION,
            "cycle",
            Fixture::identity(),
            CollaborationMode::Work,
            None,
            None,
            None,
            10,
        )
        .expect("seed");
    fixture
        .pool
        .transaction(|tx| {
            let state = zuno_db::session_execution::read_in(tx, SESSION)?.expect("state");
            zuno_db::session_execution::set_paused_in(
                tx,
                SESSION,
                state.revision,
                SessionPauseReason::NoProgress,
                20,
            )
            .map(|_| ())
        })
        .expect("pause");
    fixture
        .control
        .record_recovery(
            SESSION,
            "cycle",
            Fixture::identity(),
            CollaborationMode::Work,
            None,
            None,
            None,
            30,
        )
        .expect("recover identity only");
    let state = fixture
        .control
        .state(SESSION)
        .expect("state")
        .expect("state");
    assert_eq!(state.phase, SessionExecutionPhase::Paused);
    assert_eq!(
        state.wake_admission(&SessionWakeSignal::Recovery),
        WakeAdmission::Reject
    );
}

#[test]
fn explicit_resume_without_goal_or_plan_queues_an_idempotent_work_control() {
    let fixture = Fixture::new();
    fixture
        .control
        .record_continuation(
            SESSION,
            "cycle",
            Fixture::identity(),
            CollaborationMode::Work,
            None,
            None,
            None,
            10,
        )
        .expect("seed");
    let paused = fixture
        .pool
        .transaction(|tx| {
            let state = zuno_db::session_execution::read_in(tx, SESSION)?.expect("state");
            zuno_db::session_execution::set_paused_in(
                tx,
                SESSION,
                state.revision,
                SessionPauseReason::NoExecutableWork,
                20,
            )
        })
        .expect("pause");
    let resumed = fixture
        .control
        .resume_session(SESSION, paused.revision, 30)
        .expect("resume");
    assert_eq!(resumed.state.mode, CollaborationMode::Work);
    assert_eq!(resumed.state.phase, SessionExecutionPhase::Authorized);
    assert_eq!(resumed.input.prompt["control"], "resume_work");
    assert_eq!(resumed.input.trigger_kind, InputTriggerKind::UserControl);
    assert!(resumed.state.authorized_plan_id.is_none());
    let duplicate = fixture
        .control
        .resume_session(SESSION, paused.revision, 40)
        .expect("retry");
    assert_eq!(duplicate.input.id, resumed.input.id);
    assert_eq!(count(&fixture, "session_input"), 1);
}

#[tokio::test]
async fn explicit_resume_cannot_waive_required_input_or_authorize_a_plan() {
    let fixture = Fixture::new();
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let opened = service
        .open(spec(QuestionPurpose::RequiredInput))
        .await
        .expect("open");
    let waiting = fixture
        .control
        .state(SESSION)
        .expect("state")
        .expect("state");
    assert!(matches!(
        fixture
            .control
            .resume_session(SESSION, waiting.revision, 40),
        Err(SessionControlError::ResumeRejected { .. }),
    ));
    assert_eq!(
        service
            .get(SESSION, &opened.question.id)
            .await
            .expect("question")
            .state,
        QuestionState::Pending
    );
    assert_eq!(count(&fixture, "session_input"), 0);
    fixture.enter_plan();
    let planning = fixture
        .control
        .state(SESSION)
        .expect("state")
        .expect("state");
    assert!(matches!(
        fixture
            .control
            .resume_session(SESSION, planning.revision, 50),
        Err(SessionControlError::ResumeRejected { .. }),
    ));
    assert_eq!(count(&fixture, "session_input"), 0);
}

fn bind_plan_cycle(fixture: &Fixture) {
    let plan = fixture.plan();
    fixture.enter_plan();
    fixture
        .control
        .record_continuation(
            SESSION,
            "logical-plan",
            Fixture::identity(),
            CollaborationMode::Plan,
            Some(plan.id),
            Some(plan.revision),
            None,
            20,
        )
        .expect("bind logical source cycle");
}

#[tokio::test]
async fn context_recovery_hands_off_approved_plan_with_a_new_engine_turn_id() {
    let fixture = Fixture::new();
    bind_plan_cycle(&fixture);
    let service = QuestionService::new(Arc::clone(&fixture.pool));
    let opened = service
        .open(spec(QuestionPurpose::PlanAuthorization))
        .await
        .expect("open");
    assert_eq!(
        opened
            .question
            .plan
            .as_ref()
            .expect("binding")
            .source_cycle_id
            .as_deref(),
        Some("logical-plan")
    );
    service
        .apply(
            SESSION,
            &opened.question.id,
            command(&opened.question, "approve", approve()),
        )
        .await
        .expect("early approval");
    service
        .complete_plan_turn(SESSION, "engine-turn-after-compaction")
        .await
        .expect("same-cycle handoff");
    assert_eq!(
        service
            .get(SESSION, &opened.question.id)
            .await
            .expect("view")
            .authorization,
        Some(PlanAuthorizationState::Applied)
    );
    assert_eq!(count(&fixture, "session_input"), 1);
}

#[tokio::test]
async fn interrupted_recovery_invalidates_unfinished_source_but_not_an_earlier_handoff() {
    for completed_source in [false, true] {
        let fixture = Fixture::new();
        bind_plan_cycle(&fixture);
        let service = QuestionService::new(Arc::clone(&fixture.pool));
        let opened = service
            .open(spec(QuestionPurpose::PlanAuthorization))
            .await
            .expect("open");
        if completed_source {
            service
                .complete_plan_turn(SESSION, "turn_plan")
                .await
                .expect("source succeeded");
        }
        service
            .interrupt_plan_turn(SESSION, "different-engine-turn")
            .await
            .expect("interrupt");
        let view = service
            .get(SESSION, &opened.question.id)
            .await
            .expect("view");
        if completed_source {
            assert_eq!(view.state, QuestionState::Pending);
            assert_eq!(view.authorization, None);
        } else {
            assert_eq!(view.state, QuestionState::Cancelled);
            assert_eq!(
                view.authorization,
                Some(PlanAuthorizationState::Invalidated)
            );
        }
        assert_eq!(count(&fixture, "session_input"), 0);
    }
}
