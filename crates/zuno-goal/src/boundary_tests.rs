//! Behavioral regressions for the bounded Goal lifecycle change.

use super::*;
use crate::{BlockedAudit, GoalContinuation, GoalTurnOutcome};
use zuno_engine::status::SessionRunRegistry;

#[test]
fn boundary_plain_changed_goal_completes_with_its_finished_plan() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    let connection = pool.get().expect("connection");
    connection
        .execute(
            "INSERT INTO work_plan \
             (id,session_id,goal_id,revision,title,steps,time_created,time_updated) \
             VALUES ('plan-delivered',?1,?2,1,'Delivery',?3,1,1)",
            params![
                SESSION,
                goal.goal_id,
                r#"[{"id":"verify","status":"completed"}]"#
            ],
        )
        .expect("persist finished plan");
    drop(connection);
    store
        .escalate_to_change(SESSION, "write changed source", 1_000)
        .expect("record changed workspace");
    store
        .mark_mutation(SESSION, 1_000)
        .expect("record mutation");
    assert!(goal.success_criteria.is_empty());
    assert!(store.criteria(SESSION).expect("ledger").is_empty());

    let completed = store
        .complete_as_model_checked(SESSION, goal.revision)
        .expect("an undeclared checklist must not be invented after a change")
        .expect("goal");
    assert_eq!(completed.status, GoalStatus::Complete);
    assert_eq!(completed.goal_id, goal.goal_id);
    assert!(store.criteria(SESSION).expect("ledger").is_empty());
}

#[test]
fn boundary_model_cannot_complete_a_paused_goal_through_any_entrypoint() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "old goal", None)
        .unwrap();
    let paused = fixture
        .store
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap()
        .unwrap();
    let history = fixture.store.history(SESSION).unwrap();
    for attempt in [
        fixture
            .store
            .complete_as_model_checked(SESSION, paused.revision),
        fixture
            .store
            .update_status_as_model(SESSION, ModelStatus::Complete),
        fixture.store.update_status_as_model_checked(
            SESSION,
            ModelStatus::Complete,
            paused.revision,
        ),
        fixture
            .store
            .complete_as_model_with_criteria_checked(SESSION, paused.revision, &[], &[]),
    ] {
        assert!(
            attempt.is_err(),
            "independent input grants no authority over an old pause"
        );
        assert_eq!(fixture.goal(SESSION), paused);
    }
    assert_eq!(fixture.store.history(SESSION).unwrap(), history);
    assert_eq!(
        fixture.store.pause_state(SESSION).unwrap().unwrap().reason,
        GoalPauseReason::UserInterruption
    );
}

#[test]
fn boundary_model_cannot_relabel_a_pause_as_blocked_then_complete() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "paused work", None)
        .unwrap();
    let paused = fixture
        .store
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap()
        .unwrap();
    for result in [
        fixture
            .store
            .update_status_as_model(SESSION, ModelStatus::Blocked),
        fixture.store.update_status_as_model_checked(
            SESSION,
            ModelStatus::Blocked,
            paused.revision,
        ),
    ] {
        assert!(
            result.is_err(),
            "a model status writer must not erase a native pause"
        );
        assert_eq!(fixture.goal(SESSION), paused);
    }
    assert!(
        fixture
            .store
            .complete_as_model_checked(SESSION, paused.revision)
            .is_err()
    );
    assert!(fixture.store.pause_state(SESSION).unwrap().is_some());
}

#[test]
fn boundary_native_completion_preserves_old_changed_goal_identity_and_usage() {
    let fixture = Fixture::on_disk();
    fixture
        .store
        .create_goal(SESSION, "delivered goal", Some(9_000))
        .unwrap();
    fixture
        .store
        .record_usage(SESSION, 1_234, 17, true)
        .unwrap();
    fixture
        .store
        .escalate_to_change(SESSION, "write", 1_000)
        .unwrap();
    fixture.store.mark_mutation(SESSION, 1_000).unwrap();
    fixture
        .store
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap();
    let fixture = fixture.restart();
    let paused = fixture.goal(SESSION);
    let history = fixture.store.history(SESSION).unwrap();
    let completed = fixture
        .store
        .complete_checked(SESSION, paused.revision)
        .expect("native user may close a legitimate old goal without resuming")
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Complete);
    assert_eq!(completed.goal_id, paused.goal_id);
    assert_eq!(completed.objective, paused.objective);
    assert_eq!(completed.created_at_ms, paused.created_at_ms);
    assert_eq!(completed.token_budget, paused.token_budget);
    assert_eq!(completed.tokens_used, paused.tokens_used);
    assert_eq!(completed.time_used_seconds, paused.time_used_seconds);
    assert_eq!(completed.usage_known, paused.usage_known);
    let after = fixture.store.history(SESSION).unwrap();
    assert_eq!(&after[..history.len()], history.as_slice());
    assert_eq!(after.len(), history.len() + 1);
    assert!(fixture.store.pause_state(SESSION).unwrap().is_none());
}

#[test]
fn native_user_can_block_a_paused_goal_without_losing_identity_or_usage() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "native user control", Some(9_000))
        .unwrap();
    fixture.store.record_usage(SESSION, 123, 17, true).unwrap();
    let paused = fixture
        .store
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap()
        .unwrap();
    let history = fixture.store.history(SESSION).unwrap();
    let blocked = fixture
        .store
        .block_as_user_checked(SESSION, paused.revision, " waiting for vendor ")
        .expect("explicit native user block must work for a paused Goal")
        .unwrap();
    assert_eq!(blocked.status, GoalStatus::Blocked);
    assert_eq!(
        blocked.blocked_reason.as_deref(),
        Some("waiting for vendor")
    );
    assert_eq!(blocked.goal_id, paused.goal_id);
    assert_eq!(blocked.objective, paused.objective);
    assert_eq!(blocked.token_budget, paused.token_budget);
    assert_eq!(blocked.tokens_used, paused.tokens_used);
    assert_eq!(blocked.time_used_seconds, paused.time_used_seconds);
    assert_eq!(blocked.usage_known, paused.usage_known);
    let after = fixture.store.history(SESSION).unwrap();
    assert_eq!(&after[..history.len()], history.as_slice());
    assert_eq!(after.len(), history.len() + 1);
    assert!(fixture.store.pause_state(SESSION).unwrap().is_none());
}

#[test]
fn native_user_block_preserves_budget_and_cancel_guards_and_checks_revision() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal(SESSION, "budget guard", Some(50))
        .unwrap();
    let limited = fixture
        .store
        .record_usage(SESSION, 50, 7, true)
        .unwrap()
        .unwrap();
    assert_eq!(limited.status, GoalStatus::BudgetLimited);
    assert!(matches!(
        fixture
            .store
            .block_as_user_checked(SESSION, created.revision, "late"),
        Err(GoalError::RevisionConflict { .. })
    ));
    assert_eq!(
        fixture
            .store
            .block_as_user_checked(SESSION, limited.revision, "new reason")
            .unwrap(),
        Some(limited)
    );
    let cancelled = fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Cancelled)
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture
            .store
            .block_as_user_checked(SESSION, cancelled.revision, "new reason")
            .unwrap(),
        Some(cancelled.clone())
    );
    assert!(matches!(
        fixture
            .store
            .block_as_user_checked(SESSION, cancelled.revision, " "),
        Err(GoalError::EmptyBlockReason)
    ));
    assert_eq!(fixture.goal(SESSION), cancelled);
}

#[test]
fn native_user_block_rolls_back_status_history_and_pause_cleanup_together() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "atomic native block", None)
        .unwrap();
    let paused = fixture
        .store
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap()
        .unwrap();
    let history = fixture.store.history(SESSION).unwrap();
    fixture
        .store
        .pool()
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_pause_cleanup BEFORE DELETE ON goal_pause
         BEGIN SELECT RAISE(ABORT, 'injected pause cleanup failure'); END;",
        )
        .unwrap();
    assert!(
        fixture
            .store
            .block_as_user_checked(SESSION, paused.revision, "vendor issue")
            .is_err()
    );
    assert_eq!(fixture.goal(SESSION), paused);
    assert_eq!(fixture.store.history(SESSION).unwrap(), history);
    assert!(fixture.store.pause_state(SESSION).unwrap().is_some());
}

#[test]
fn native_user_block_reactivates_a_historical_declared_checklist_without_waiving_it() {
    let fixture = Fixture::in_memory();
    let created = fixture
        .store
        .create_goal_with_criteria(
            SESSION,
            "historical goal",
            &["verify delivery".to_owned()],
            None,
        )
        .unwrap();
    let waived = fixture
        .store
        .waive_criterion(
            SESSION,
            created.goal.revision,
            "c1",
            "historical decision",
            10,
        )
        .unwrap();
    let complete = fixture
        .store
        .complete_checked(SESSION, waived.goal.revision)
        .unwrap()
        .unwrap();
    // Shape of a terminal Goal from before the criterion ledger existed.
    fixture
        .store
        .pool()
        .get()
        .unwrap()
        .execute("DELETE FROM goal_criterion", [])
        .unwrap();
    let blocked = fixture
        .store
        .block_as_user_checked(SESSION, complete.revision, "recheck delivery")
        .unwrap()
        .unwrap();
    let criteria = fixture.store.criteria(SESSION).unwrap();
    assert_eq!(
        criteria.len(),
        1,
        "native block must retain the existing live-goal backfill contract"
    );
    assert_eq!(criteria[0].statement, "verify delivery");
    assert_eq!(criteria[0].status, GoalCriterionStatus::Open);
    assert!(matches!(
        fixture.store.complete_checked(SESSION, blocked.revision),
        Err(GoalError::EvidenceMissing { .. })
    ));
}

#[test]
fn boundary_inconsistent_declaration_or_ledger_never_downgrades_to_plain_goal() {
    for corrupt in [
        "DELETE FROM goal_criterion",
        "UPDATE goal SET success_criteria = '[]'",
        "UPDATE goal SET success_criteria = '{invalid'",
        "UPDATE goal_criterion SET statement = 'different requirement'",
        "UPDATE goal_criterion SET ordinal = 7",
        "UPDATE goal_criterion SET criterion_id = 'foreign-id'",
    ] {
        let fixture = Fixture::in_memory();
        let created = fixture
            .store
            .create_goal_with_criteria(
                SESSION,
                "explicit acceptance",
                &["prove delivery".to_owned()],
                None,
            )
            .unwrap();
        let settled = fixture
            .store
            .waive_criterion(
                SESSION,
                created.goal.revision,
                "c1",
                "explicitly waived",
                2_000,
            )
            .unwrap();
        let history = fixture.store.history(SESSION).unwrap();
        fixture
            .store
            .pool()
            .get()
            .unwrap()
            .execute(corrupt, [])
            .unwrap();
        let result = fixture
            .store
            .complete_checked(SESSION, settled.goal.revision);
        assert!(
            result.is_err(),
            "corrupt declaration/ledger accepted: {corrupt}"
        );
        assert!(
            !result.unwrap_err().is_model_refusal(),
            "corruption is not model-correctable"
        );
        let state: (String, i64) = fixture
            .store
            .pool()
            .get()
            .unwrap()
            .query_row(
                "SELECT status, revision FROM goal WHERE session_id=?1",
                [SESSION],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, ("active".to_owned(), settled.goal.revision));
        assert_eq!(fixture.store.history(SESSION).unwrap(), history);
    }
}

#[test]
fn boundary_failed_blocked_transition_preserves_observation_and_streak() {
    let fixture = Fixture::on_disk();
    fixture
        .store
        .create_goal(SESSION, "observe blockers", None)
        .unwrap();
    for _ in 0..2 {
        fixture
            .store
            .record_failure_signal(SESSION, Some("external blocker"))
            .unwrap();
    }
    fixture
        .store
        .stage_failure_signal(SESSION, "external blocker")
        .unwrap();
    fixture
        .store
        .pool()
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_blocked BEFORE UPDATE OF status ON goal
         WHEN NEW.status = 'blocked'
         BEGIN SELECT RAISE(ABORT, 'injected blocked write failure'); END;",
        )
        .unwrap();
    let before = fixture.goal(SESSION);
    let continuation = GoalContinuation::new(
        Arc::new(
            GoalStore::from_pool(
                Arc::clone(&fixture.store.pool),
                fixture.spill.path().to_owned(),
            )
            .unwrap(),
        ),
        SessionRunRegistry::new(),
    );
    assert!(
        continuation
            .record_turn_outcome(SESSION, GoalTurnOutcome::Progress)
            .is_err()
    );
    assert_eq!(fixture.goal(SESSION), before);
    assert_eq!(
        fixture
            .store
            .failure_streak(SESSION)
            .unwrap()
            .unwrap()
            .consecutive_turns,
        2,
        "consumption, count and terminal write must roll back together"
    );
    drop(continuation);
    let fixture = fixture.restart();
    fixture
        .store
        .pool()
        .get()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_blocked")
        .unwrap();
    let continuation = GoalContinuation::new(
        Arc::new(
            GoalStore::from_pool(
                Arc::clone(&fixture.store.pool),
                fixture.spill.path().to_owned(),
            )
            .unwrap(),
        ),
        SessionRunRegistry::new(),
    );
    assert!(
        matches!(
            continuation
                .record_turn_outcome(SESSION, GoalTurnOutcome::Progress)
                .unwrap(),
            BlockedAudit::Blocked(FailureStreak {
                consecutive_turns: 3,
                ..
            })
        ),
        "the original staged observation survives rollback and restart"
    );
}

#[test]
fn boundary_native_completion_cannot_clear_an_unresolved_uncertain_call() {
    let (_spill, pool, store, goal) = shared_completion_fixture();
    let connection = pool.get().unwrap();
    connection
        .execute(
            "INSERT INTO message (id,session_id,time_created,time_updated,data)
         VALUES ('msg-uncertain',?1,1,1,'{}')",
            [SESSION],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO part (id,message_id,session_id,time_created,time_updated,data)
         VALUES ('part-uncertain','msg-uncertain',?1,1,1,?2)",
            params![
                SESSION,
                serde_json::json!({
                    "type": "tool",
                    "state": {
                        "outcome": "uncertain",
                        "uncertain": {
                            "callID": "call-deploy",
                            "tool": "shell",
                            "appliedPaths": [],
                            "cause": "lost_outcome",
                            "observedAtMs": goal.created_at_ms + 1
                        }
                    }
                })
                .to_string()
            ],
        )
        .unwrap();
    let paused = store
        .pause_with_reason(SESSION, GoalPauseReason::UncertainSideEffect)
        .unwrap()
        .unwrap();
    let before: String = connection
        .query_row(
            "SELECT data FROM part WHERE id='part-uncertain'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        matches!(
            store.complete_checked(SESSION, paused.revision),
            Err(GoalError::CompletionUncertain { call_ids }) if call_ids == ["call-deploy"]
        ),
        "native completion is not authoritative reconciliation of an uncertain side effect"
    );
    assert_eq!(store.goal(SESSION).unwrap(), Some(paused));
    let after: String = connection
        .query_row(
            "SELECT data FROM part WHERE id='part-uncertain'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, before);
}

#[test]
fn boundary_paused_completion_rolls_back_submitted_criterion_changes() {
    let (fixture, goal) = evidence_fixture();
    let paused = fixture
        .store
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap()
        .unwrap();
    let before = fixture.store.criteria(SESSION).unwrap();
    assert_eq!(goal.goal_id, paused.goal_id);
    assert!(matches!(
        fixture.store.complete_as_model_with_criteria_checked(
            SESSION,
            paused.revision,
            &[CriterionSatisfaction {
                criterion_id: "c1".to_owned(),
                receipt_id: "rec_pass".to_owned()
            }],
            &[CriterionWaiver {
                criterion_id: "c2".to_owned(),
                reason: "outside scope".to_owned()
            }],
        ),
        Err(GoalError::CompletionRequiresUser { .. })
    ));
    assert_eq!(fixture.store.criteria(SESSION).unwrap(), before);
    assert_eq!(fixture.goal(SESSION), paused);
}

#[test]
fn boundary_outer_transaction_owns_observation_consumption_and_count() {
    let fixture = Fixture::in_memory();
    fixture
        .store
        .create_goal(SESSION, "host transaction", None)
        .unwrap();
    fixture
        .store
        .stage_failure_signal(SESSION, "blocked")
        .unwrap();
    {
        let mut connection = fixture.store.pool().get().unwrap();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let audit =
            GoalStore::record_turn_outcome_in(&tx, SESSION, GoalTurnOutcome::Progress, 5_000)
                .unwrap();
        assert!(matches!(
            audit,
            BlockedAudit::Pending(FailureStreak {
                consecutive_turns: 1,
                ..
            })
        ));
        tx.rollback().unwrap();
    }
    assert!(fixture.store.failure_streak(SESSION).unwrap().is_none());
    assert!(
        matches!(
            fixture
                .store
                .record_turn_outcome(SESSION, GoalTurnOutcome::Progress)
                .unwrap(),
            BlockedAudit::Pending(FailureStreak {
                consecutive_turns: 1,
                ..
            })
        ),
        "a host rollback must leave the observation available"
    );
}
