//! Adopted Work Plan steps are execution facts, not assistant-text predictions.
use super::*;
use zuno_types::execution::{SessionPauseReason, SessionReadiness, SessionScheduling};

async fn owned_plan(
    behavior: ScriptedTurnBehavior,
) -> (
    tempfile::TempDir,
    TurnHost,
    Arc<ScriptedTurnDriver>,
    zuno_tools::WorkStateStore,
) {
    let (directory, mut host, driver, work) = scripted_reconciliation_host("build", behavior).await;
    let plan = seed_scripted_plan(&work, &host.session_id, false)
        .plan
        .unwrap();
    let (message, parts) = host
        .prepare_turn_user_message(
            "Implement the authorized Plan.",
            Some("input-owned-plan"),
            None,
        )
        .unwrap();
    host.persist_user_input(&message, &parts).unwrap();
    let scope = zuno_db::session_work_cycle::current_in(&host.connection, &host.session_id)
        .unwrap()
        .unwrap();
    host.database
        .transaction(|tx| {
            assert!(zuno_db::session_work_cycle::adopt_in(
                tx,
                &host.session_id,
                &scope.cycle_id,
                Some(&plan.id),
                std::iter::empty(),
                zuno_db::message::now_millis(),
            )?);
            Ok(())
        })
        .unwrap();
    (directory, host, driver, work)
}

async fn execute(host: &mut TurnHost) {
    let guard = host.runs.begin_turn(host.session_id.clone()).unwrap();
    let (sender, receiver) = zuno_engine::r#loop::event_channel();
    let (outcome, _events) = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::join!(
            host.execute_turn_unaccounted(
                DynamicContext::default(),
                DynamicContextRefreshInstruction::Fixed(
                    "Perform current authorized work.".to_owned()
                ),
                TurnStart::UserMessage,
                None,
                &guard,
                sender,
            ),
            collect_turn_events(receiver)
        )
    })
    .await
    .expect("bounded recovery");
    assert!(matches!(
        outcome.unwrap(),
        Some(TurnOutcome::Completed { .. })
    ));
}

#[tokio::test]
async fn adopted_in_progress_plan_is_executable_without_duplicate_todos() {
    let (_directory, mut host, _driver, work) =
        owned_plan(ScriptedTurnBehavior::PreserveWork).await;
    assert!(work.items(&host.session_id).unwrap().is_empty());
    let (facts, authorized, _) = host.plan_reconciliation_state().unwrap();
    assert!(authorized && facts.plan_exists && !facts.plan_terminal);
    assert!(
        facts.executable_work,
        "the current adopted in_progress step is actionable"
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn adopted_plan_continues_to_settlement_without_another_user_input() {
    let (_directory, mut host, driver, work) =
        owned_plan(ScriptedTurnBehavior::SettleWorkOnSecondTurn).await;
    execute(&mut host).await;
    assert_eq!(
        driver.calls(),
        2,
        "do not pause after announcing the next owned step"
    );
    assert!(
        work.plan(&host.session_id)
            .unwrap()
            .unwrap()
            .steps
            .iter()
            .all(|step| step.status.is_terminal())
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn unchanged_adopted_plan_still_stops_at_the_no_progress_limit() {
    let (_directory, mut host, driver, _) = owned_plan(ScriptedTurnBehavior::PreserveWork).await;
    execute(&mut host).await;
    assert_eq!(driver.calls(), 3);
    let state = host
        .session_control
        .state(&host.session_id)
        .unwrap()
        .unwrap();
    assert!(matches!(
        state.scheduling.unwrap().readiness,
        SessionReadiness::Paused {
            reason: SessionPauseReason::NoProgress
        }
    ));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_recorded_wait_still_prevents_adopted_plan_recovery() {
    let (_directory, mut host, driver, _) = owned_plan(ScriptedTurnBehavior::PreserveWork).await;
    let state = host
        .session_control
        .state(&host.session_id)
        .unwrap()
        .unwrap();
    zuno_db::session_execution::SessionExecutionStore::new(host.database.clone())
        .set_scheduling(
            &host.session_id,
            state.revision,
            SessionScheduling {
                readiness: SessionReadiness::WaitingHuman {
                    request_id: "required-approval".to_owned(),
                },
                ..state.scheduling.unwrap_or_default()
            },
            zuno_db::message::now_millis(),
        )
        .unwrap();
    let guard = host.runs.begin_turn(host.session_id.clone()).unwrap();
    let (sender, receiver) = zuno_engine::r#loop::event_channel();
    let (outcome, _) = tokio::join!(
        host.execute_turn_unaccounted(
            DynamicContext::default(),
            DynamicContextRefreshInstruction::Fixed("Do not waive the wait.".to_owned()),
            TurnStart::UserMessage,
            None,
            &guard,
            sender
        ),
        collect_turn_events(receiver)
    );
    assert!(outcome.unwrap().is_none());
    assert_eq!(driver.calls(), 0);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn unadopted_and_stopped_plans_do_not_supply_execution_evidence() {
    for stopped in [false, true] {
        let (_directory, mut host, _, _) = owned_plan(ScriptedTurnBehavior::PreserveWork).await;
        let mut scope = zuno_db::session_work_cycle::current_in(&host.connection, &host.session_id)
            .unwrap()
            .unwrap();
        if stopped {
            scope.stopped = Some(zuno_db::session_work_cycle::CycleStop {
                turn_id: None,
                input_id: None,
                user_cancelled: true,
                at_ms: zuno_db::message::now_millis(),
            });
        } else {
            scope.plan_id = None;
        }
        host.database
            .transaction(|tx| {
                zuno_db::session_work_cycle::save_in(tx, &scope, zuno_db::message::now_millis())
            })
            .unwrap();
        assert!(!host.plan_reconciliation_state().unwrap().0.executable_work);
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn linked_todo_ownership_blockers_and_dependencies_override_coarse_plan_status() {
    for (status, owner, dependencies) in [
        (zuno_tools::WorkItemStatus::Blocked, Some("build"), vec![]),
        (
            zuno_tools::WorkItemStatus::InProgress,
            Some("another-agent"),
            vec![],
        ),
        (
            zuno_tools::WorkItemStatus::Pending,
            Some("build"),
            vec!["unresolved".to_owned()],
        ),
    ] {
        let (_directory, mut host, _, work) = owned_plan(ScriptedTurnBehavior::PreserveWork).await;
        let plan = work.plan(&host.session_id).unwrap().unwrap();
        let make_item =
            |id: &str, status, deps, owner: Option<&str>| zuno_tools::WorkItemChange::Add {
                id: Some(id.to_owned()),
                goal_id: None,
                plan_step_id: Some(plan.steps[0].id.clone()),
                parent_id: None,
                subject: id.to_owned(),
                description: "Typed dependency evidence".to_owned(),
                active_form: None,
                status,
                priority: zuno_tools::WorkItemPriority::Medium,
                dependencies: deps,
                owner: owner.map(str::to_owned),
            };
        let mut changes = vec![];
        if !dependencies.is_empty() {
            changes.push(make_item(
                "unresolved",
                zuno_tools::WorkItemStatus::Blocked,
                vec![],
                None,
            ));
        }
        changes.push(make_item("current", status, dependencies, owner));
        work.update_items(&host.session_id, changes).unwrap();
        let (facts, _, _) = host.plan_reconciliation_state().unwrap();
        assert!(facts.active_todo);
        assert!(
            !facts.executable_work,
            "Plan must not erase {status:?}/{owner:?}"
        );
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn later_pending_decision_does_not_block_the_current_step_by_array_order() {
    let (_directory, mut host, _, work) = owned_plan(ScriptedTurnBehavior::PreserveWork).await;
    let plan = work.plan(&host.session_id).unwrap().unwrap();
    work.update_plan(
        &host.session_id,
        zuno_tools::PlanUpdateParams {
            expected_revision: Some(plan.revision),
            goal_id: None,
            title: plan.title,
            steps: vec![
                zuno_tools::PlanStep {
                    id: "later".to_owned(),
                    title: "Later independent phase needs user decision".to_owned(),
                    status: zuno_tools::PlanStepStatus::Pending,
                },
                plan.steps[0].clone(),
            ],
        },
    )
    .unwrap();
    assert!(host.plan_reconciliation_state().unwrap().0.executable_work);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn plan_mode_and_stale_authorization_do_not_supply_execution_evidence() {
    for planning in [false, true] {
        let (_directory, mut host, _, work) = owned_plan(ScriptedTurnBehavior::PreserveWork).await;
        let plan = work.plan(&host.session_id).unwrap().unwrap();
        let store = zuno_db::session_execution::SessionExecutionStore::new(host.database.clone());
        let mut state = store.get(&host.session_id).unwrap().unwrap();
        if planning {
            state.mode = CollaborationMode::Plan;
        } else {
            state.authorized_plan_id = Some(plan.id);
            state.authorized_plan_revision = Some(plan.revision + 1);
        }
        store.update(state.revision, state).unwrap();
        let (facts, authorized, _) = host.plan_reconciliation_state().unwrap();
        assert!(!authorized && !facts.executable_work);
        host.shutdown().await.unwrap();
    }
}
