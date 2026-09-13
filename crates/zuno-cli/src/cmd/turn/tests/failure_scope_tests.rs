use super::*;
use zuno_db::inbox::{InputDelivery, NewSessionInput};
use zuno_types::execution::{InputTriggerKind, SessionReadiness};

fn activate_failure_input(host: &TurnHost, id: &str) -> String {
    host.database
        .try_transaction(|tx| {
            tx.execute(
                "INSERT INTO message(id,session_id,time_created,time_updated,data)
                 VALUES(?1,?2,10,10,'{\"role\":\"user\"}')",
                rusqlite::params![id, host.session_id],
            )
            .map_err(zuno_db::map_error)?;
            zuno_db::inbox::admit_and_promote_in(
                tx,
                NewSessionInput::new(
                    id,
                    &host.session_id,
                    json!({"kind":"acpPrompt","text":"inspect the current status"}),
                    InputDelivery::Queue,
                    10,
                )
                .with_trigger_kind(InputTriggerKind::User),
            )?;
            let cycle = zuno_session_control::SessionControlService::activate_user_input_in(
                tx,
                &host.session_id,
                id,
                id,
                CollaborationMode::Work,
                host.current_turn_identity(),
                10,
            )?;
            zuno_db::inbox::mark_consumed_in(tx, &host.session_id, id)?;
            Ok::<_, zuno_session_control::SessionControlError>(cycle.cycle_id)
        })
        .unwrap()
}

fn exhausted_retry() -> TurnFailure {
    TurnFailure::Engine(TurnError::ProviderRetryDeadlineExceeded {
        attempt: 3,
        recovery_elapsed: std::time::Duration::from_secs(180),
        total_elapsed: std::time::Duration::from_secs(339),
        last_failure: Box::new(
            ProviderError::Transient {
                status: Some(503),
                source: None,
            }
            .diagnostic_snapshot(),
        ),
    })
}

#[tokio::test]
async fn ordinary_retry_exhaustion_stops_only_its_cycle_and_new_input_runs() {
    let (_dir, mut host, _driver, _work) =
        scripted_reconciliation_host("build", ScriptedTurnBehavior::PreserveWork).await;
    let first = activate_failure_input(&host, "failed-input");
    host.session_control
        .begin_engine_turn(&host.session_id, &first, "failed-turn")
        .unwrap();
    host.failure_scope = host
        .session_control
        .capture_failure_scope(&host.session_id)
        .unwrap();
    let before = goal_usage(&host.connection, &host.session_id).unwrap();
    let (events, _receiver) = zuno_engine::r#loop::event_channel();
    assert!(
        host.handle_turn_failure(before, Instant::now(), exhausted_retry(), &events)
            .await
            .is_err()
    );
    let state = host
        .session_control
        .state(&host.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        state.scheduling.unwrap().readiness,
        SessionReadiness::Completed,
        "a retryable request failure ends the cycle, not the conversation"
    );
    assert!(
        zuno_db::session_work_cycle::is_stopped_in(&host.connection, &host.session_id, &first)
            .unwrap(),
        "late callbacks must not revive the failed cycle"
    );
    activate_failure_input(&host, "next-input");
    assert_eq!(
        host.session_control
            .state(&host.session_id)
            .unwrap()
            .unwrap()
            .scheduling
            .unwrap()
            .readiness,
        SessionReadiness::Ready
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn independent_failure_does_not_modify_a_paused_goal() {
    let (_dir, mut host, _driver, _work) =
        scripted_reconciliation_host("build", ScriptedTurnBehavior::PreserveWork).await;
    host.goal_store
        .create_goal(&host.session_id, "separate goal", None)
        .unwrap();
    host.goal_store
        .pause_with_reason(
            &host.session_id,
            zuno_goal::GoalPauseReason::UserInterruption,
        )
        .unwrap();
    let goal = host.goal_store.goal(&host.session_id).unwrap();
    let pause = host.goal_store.pause_state(&host.session_id).unwrap();
    let cycle = activate_failure_input(&host, "independent-input");
    host.session_control
        .begin_engine_turn(&host.session_id, &cycle, "independent-turn")
        .unwrap();
    host.failure_scope = host
        .session_control
        .capture_failure_scope(&host.session_id)
        .unwrap();
    let usage = goal_usage(&host.connection, &host.session_id).unwrap();
    let (events, _receiver) = zuno_engine::r#loop::event_channel();
    let _ = host
        .handle_turn_failure(usage, Instant::now(), exhausted_retry(), &events)
        .await;
    assert_eq!(host.goal_store.goal(&host.session_id).unwrap(), goal);
    assert_eq!(
        host.goal_store.pause_state(&host.session_id).unwrap(),
        pause
    );
    assert_eq!(
        host.session_control
            .state(&host.session_id)
            .unwrap()
            .unwrap()
            .scheduling
            .unwrap()
            .readiness,
        SessionReadiness::Completed
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_goal_is_unchanged_by_an_independent_failed_query() {
    let (_dir, mut host, _driver, _work) =
        scripted_reconciliation_host("build", ScriptedTurnBehavior::PreserveWork).await;
    host.goal_store
        .create_goal(&host.session_id, "already delivered", None)
        .unwrap();
    host.connection
        .execute(
            "UPDATE goal SET status='complete',revision=7 WHERE session_id=?1",
            [&host.session_id],
        )
        .unwrap();
    let original = host.goal_store.goal(&host.session_id).unwrap();
    let cycle = activate_failure_input(&host, "query-after-completion");
    host.session_control
        .begin_engine_turn(&host.session_id, &cycle, "query-turn")
        .unwrap();
    host.failure_scope = host
        .session_control
        .capture_failure_scope(&host.session_id)
        .unwrap();
    let usage = goal_usage(&host.connection, &host.session_id).unwrap();
    let (events, _receiver) = zuno_engine::r#loop::event_channel();
    assert!(
        host.handle_turn_failure(usage, Instant::now(), exhausted_retry(), &events)
            .await
            .is_err()
    );
    assert_eq!(host.goal_store.goal(&host.session_id).unwrap(), original);
    assert!(
        host.goal_store
            .retry_state(&host.session_id)
            .unwrap()
            .is_none()
    );
    assert!(
        zuno_db::session_work_cycle::is_stopped_in(&host.connection, &host.session_id, &cycle)
            .unwrap()
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_owned_active_goal_retains_persistent_retry() {
    let (_dir, mut host, _driver, _work) =
        scripted_reconciliation_host("build", ScriptedTurnBehavior::PreserveWork).await;
    let goal = host
        .goal_store
        .create_goal(&host.session_id, "active objective", None)
        .unwrap();
    let cycle = activate_failure_input(&host, "goal-owned-input");
    host.session_control
        .begin_engine_turn(&host.session_id, &cycle, "goal-owned-turn")
        .unwrap();
    host.failure_scope = host
        .session_control
        .capture_failure_scope(&host.session_id)
        .unwrap();
    let usage = goal_usage(&host.connection, &host.session_id).unwrap();
    let (events, _receiver) = zuno_engine::r#loop::event_channel();
    assert!(
        host.handle_turn_failure(usage, Instant::now(), exhausted_retry(), &events)
            .await
            .unwrap()
    );
    let retry = host
        .goal_store
        .retry_state(&host.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(retry.goal_id, goal.goal_id);
    assert!(retry.delay_ms > 0);
    assert_eq!(
        host.goal_store
            .goal(&host.session_id)
            .unwrap()
            .unwrap()
            .status,
        GoalStatus::Active
    );
    assert!(
        !zuno_db::session_work_cycle::is_stopped_in(&host.connection, &host.session_id, &cycle)
            .unwrap()
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_failure_cannot_stop_a_newer_turn_in_the_same_cycle() {
    let (_dir, mut host, _driver, _work) =
        scripted_reconciliation_host("build", ScriptedTurnBehavior::PreserveWork).await;
    let cycle = activate_failure_input(&host, "same-cycle-input");
    host.session_control
        .begin_engine_turn(&host.session_id, &cycle, "older-turn")
        .unwrap();
    let older = host
        .session_control
        .capture_failure_scope(&host.session_id)
        .unwrap()
        .unwrap();
    host.session_control
        .begin_engine_turn(&host.session_id, &cycle, "newer-turn")
        .unwrap();
    let before = host.session_control.state(&host.session_id).unwrap();
    let disposition = host
        .session_control
        .settle_turn_failure(
            &host.session_id,
            &older,
            exhausted_retry().goal_failure(),
            host.goal_continuation.retry_policy(),
            100,
            5,
        )
        .unwrap();
    assert!(matches!(
        disposition,
        zuno_session_control::SessionFailureDisposition::Stale
    ));
    assert_eq!(
        host.session_control.state(&host.session_id).unwrap(),
        before
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_goal_bound_during_the_same_turn_owns_its_later_failure() {
    let (_dir, mut host, _driver, _work) =
        scripted_reconciliation_host("build", ScriptedTurnBehavior::PreserveWork).await;
    let cycle = activate_failure_input(&host, "propose-input");
    host.session_control
        .begin_engine_turn(&host.session_id, &cycle, "propose-turn")
        .unwrap();
    let captured = host
        .session_control
        .capture_failure_scope(&host.session_id)
        .unwrap()
        .unwrap();
    assert!(captured.goal_id.is_none());
    let goal = host
        .goal_store
        .create_goal(&host.session_id, "newly proposed objective", None)
        .unwrap();
    let identity = zuno_goal::GoalTurnIdentity::new(&goal.goal_id, &cycle, "propose-turn").unwrap();
    // Native creation's already-tested transaction binds both coordinates and
    // records this audit. An unrelated create_goal alone supplies neither.
    host.database
        .try_transaction(|tx| {
            let mut scope = zuno_db::session_work_cycle::current_in(tx, &host.session_id)?.unwrap();
            scope.goal_id = Some(goal.goal_id.clone());
            zuno_db::session_work_cycle::save_in(tx, &scope, 20)?;
            GoalStore::bind_goal_turn_in(tx, &host.session_id, &identity, None)?;
            zuno_db::event_log::append_in(
                tx,
                &host.session_id,
                zuno_db::event_log::NewSessionEvent::new(
                    "session.goal.created_and_bound",
                    json!({"identity":identity,"goalRevision":goal.revision,"time":20})
                        .as_object()
                        .unwrap()
                        .clone(),
                )?,
            )?;
            Ok::<_, zuno_goal::GoalError>(())
        })
        .unwrap();
    let disposition = host
        .session_control
        .settle_turn_failure(
            &host.session_id,
            &captured,
            exhausted_retry().goal_failure(),
            host.goal_continuation.retry_policy(),
            30,
            5,
        )
        .unwrap();
    assert!(
        matches!(
            disposition,
            zuno_session_control::SessionFailureDisposition::Goal(
                GoalFailureDisposition::RetryScheduled(_)
            )
        ),
        "native same-turn Goal creation must not lose failure ownership"
    );
    assert_eq!(
        host.goal_store
            .retry_state(&host.session_id)
            .unwrap()
            .unwrap()
            .goal_id,
        goal.goal_id
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_execution_phase_without_scheduling_remains_protected() {
    for phase in [
        zuno_types::execution::SessionExecutionPhase::Paused,
        zuno_types::execution::SessionExecutionPhase::Blocked,
        zuno_types::execution::SessionExecutionPhase::Waiting,
    ] {
        let (_dir, mut host, _, _) =
            scripted_reconciliation_host("build", ScriptedTurnBehavior::PreserveWork).await;
        let cycle = activate_failure_input(&host, "legacy-protected-input");
        host.session_control
            .begin_engine_turn(&host.session_id, &cycle, "legacy-turn")
            .unwrap();
        let captured = host
            .session_control
            .capture_failure_scope(&host.session_id)
            .unwrap()
            .unwrap();
        host.database
            .transaction(|tx| {
                let mut state = zuno_db::session_execution::read_in(tx, &host.session_id)?.unwrap();
                state.phase = phase;
                state.scheduling = None;
                zuno_db::session_execution::update_in(tx, state.revision, state).map(|_| ())
            })
            .unwrap();
        let before = host.session_control.state(&host.session_id).unwrap();
        let disposition = host
            .session_control
            .settle_turn_failure(
                &host.session_id,
                &captured,
                exhausted_retry().goal_failure(),
                host.goal_continuation.retry_policy(),
                30,
                5,
            )
            .unwrap();
        assert!(matches!(
            disposition,
            zuno_session_control::SessionFailureDisposition::GateRetained
        ));
        assert_eq!(
            host.session_control.state(&host.session_id).unwrap(),
            before
        );
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_stale_failure_does_not_invalidate_current_cycle_plan_approval() {
    use zuno_tool::question::QuestionPort as _;
    use zuno_types::question::{
        QuestionMode, QuestionOrigin, QuestionPurpose, QuestionSpec, QuestionState,
    };
    let (_dir, mut host, _, work) =
        scripted_reconciliation_host("plan", ScriptedTurnBehavior::PreserveWork).await;
    let old = activate_failure_input(&host, "old-plan-input");
    host.session_control
        .begin_engine_turn(&host.session_id, &old, "old-plan-turn")
        .unwrap();
    host.failure_scope = host
        .session_control
        .capture_failure_scope(&host.session_id)
        .unwrap();
    let usage = goal_usage(&host.connection, &host.session_id).unwrap();
    seed_scripted_plan(&work, &host.session_id, false);
    host.session_control
        .enter_plan(zuno_session_control::EnterPlanRequest {
            session_id: &host.session_id,
            work_identity: host.execution_identity_for("build"),
            at_ms: 30,
        })
        .unwrap();
    let next = activate_failure_input(&host, "new-plan-input");
    host.session_control
        .begin_engine_turn(&host.session_id, &next, "new-plan-turn")
        .unwrap();
    let approval = host
        .questions
        .open(QuestionSpec {
            origin: QuestionOrigin {
                session_id: host.session_id.clone(),
                message_id: Some("new-plan-input".to_owned()),
                call_id: Some("plan-exit-new".to_owned()),
                turn_id: Some("new-plan-turn".to_owned()),
                goal_id: None,
            },
            mode: QuestionMode::Deferred,
            purpose: QuestionPurpose::PlanAuthorization,
            questions: Vec::new(),
            expected_goal_revision: None,
            plan: None,
        })
        .await
        .unwrap()
        .question;
    let before = host.session_control.state(&host.session_id).unwrap();
    let (events, _receiver) = zuno_engine::r#loop::event_channel();
    assert!(
        host.handle_turn_failure(usage, Instant::now(), exhausted_retry(), &events)
            .await
            .is_err()
    );
    let retained = host
        .questions
        .get(&host.session_id, &approval.id)
        .await
        .unwrap();
    assert_eq!(retained.state, QuestionState::Pending);
    assert_eq!(retained, approval);
    assert_eq!(
        host.session_control.state(&host.session_id).unwrap(),
        before
    );
    host.shutdown().await.unwrap();
}
