use super::*;
use crate::goal_turn_test_support::{bind, row_count, select_cycle};

const SESSION: &str = "ses_scoped";
struct Fixture {
    store: GoalStore,
    directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = GoalStore::open_at(
            &directory.path().join("state.db"),
            directory.path().join("spill"),
        )
        .unwrap();
        store
            .create_goal(SESSION, "deliver scoped work", Some(9_000))
            .unwrap();
        Self { store, directory }
    }
    fn restart(self) -> Self {
        let Self { store, directory } = self;
        drop(store);
        let store = GoalStore::open_at(
            &directory.path().join("state.db"),
            directory.path().join("spill"),
        )
        .unwrap();
        Self { store, directory }
    }
    fn bind(&self, cycle: &str, turn: &str) -> GoalTurnIdentity {
        bind(&self.store, SESSION, cycle, turn)
    }
    fn stage(
        &self,
        id: &GoalTurnIdentity,
        signal: &str,
    ) -> Result<GoalTurnObservationReceipt, GoalError> {
        self.store.stage_goal_turn_observation(
            SESSION,
            GoalTurnObservationUpdate {
                identity: id,
                expected_revision: self.store.goal(SESSION)?.unwrap().revision,
                signal,
                satisfy: &[],
                waive: &[],
            },
        )
    }
    fn settle(&self, id: &GoalTurnIdentity) -> Result<GoalTurnSettlement, GoalError> {
        self.store
            .settle_goal_turn(SESSION, id, GoalTurnOutcome::Progress)
    }
}

#[test]
fn scoped_three_real_turns_in_one_cycle_block_once_and_survive_restart() {
    let mut fixture = Fixture::new();
    for turn in 1..=3 {
        let id = fixture.bind("one-logical-request", &format!("real-{turn}"));
        for _ in 0..3 {
            assert_eq!(
                fixture
                    .stage(&id, "external blocker")
                    .unwrap()
                    .completed_turn_streak,
                turn - 1
            );
        }
        fixture = fixture.restart();
        let settled = fixture.settle(&id).unwrap();
        assert_eq!(settled.audit.completed_turn_streak, turn);
        assert!(!settled.replayed);
        let replay = fixture.settle(&id).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.audit, settled.audit);
        assert_eq!(
            row_count(&fixture.store, "goal_turn_observation", SESSION),
            0
        );
    }
    assert_eq!(
        fixture.store.goal(SESSION).unwrap().unwrap().status,
        GoalStatus::Blocked
    );
    assert_eq!(row_count(&fixture.store, "goal_turn_audit", SESSION), 3);
    assert_eq!(fixture.store.history(SESSION).unwrap().len(), 2);
}

#[test]
fn scoped_late_old_turn_cannot_stage_settle_or_replace_the_new_cursor() {
    let fixture = Fixture::new();
    let old = fixture.bind("cycle", "old");
    fixture.stage(&old, "old blocker").unwrap();
    let new = fixture.bind("cycle", "new");
    fixture.stage(&new, "new blocker").unwrap();
    assert!(matches!(
        fixture.stage(&old, "late"),
        Err(GoalError::GoalTurnConflict { .. })
    ));
    assert!(matches!(
        fixture.settle(&old),
        Err(GoalError::GoalTurnConflict { .. })
    ));
    assert!(
        fixture
            .store
            .bind_goal_turn_checked(SESSION, &old, Some(&old))
            .is_err()
    );
    let settled = fixture.settle(&new).unwrap();
    assert_eq!(settled.audit.signal.as_deref(), Some("new blocker"));
    assert_eq!(settled.audit.completed_turn_streak, 1);
    assert_eq!(
        row_count(&fixture.store, "goal_turn_observation", SESSION),
        1,
        "late delivery must leave both old evidence and newer observation ownership intact"
    );
}

#[test]
fn scoped_old_receipt_replay_after_goal_replacement_returns_history_without_mutation() {
    let fixture = Fixture::new();
    let old = fixture.bind("cycle-old", "old");
    fixture.stage(&old, "blocker").unwrap();
    let original = fixture.settle(&old).unwrap();
    fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Cancelled)
        .unwrap();
    fixture
        .store
        .create_goal(SESSION, "new goal", None)
        .unwrap();
    let new = fixture.bind("cycle-new", "new");
    fixture.stage(&new, "new pending").unwrap();
    let goal = fixture.store.goal(SESSION).unwrap();
    let replay = fixture.settle(&old).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.audit, original.audit);
    assert_eq!(replay.current_goal, goal);
    assert_eq!(fixture.store.current_goal_turn(SESSION).unwrap(), Some(new));
    assert_eq!(
        row_count(&fixture.store, "goal_turn_observation", SESSION),
        1
    );
}

#[test]
fn scoped_unsettled_old_goal_never_counts_against_replacement_even_with_same_revision() {
    let fixture = Fixture::new();
    let old = fixture.bind("cycle", "old");
    fixture.stage(&old, "old").unwrap();
    fixture
        .store
        .set_status_as_system(SESSION, SystemStatus::Cancelled)
        .unwrap();
    fixture
        .store
        .create_goal(SESSION, "replacement", None)
        .unwrap();
    let new = fixture.bind("new-cycle", "new");
    assert!(fixture.stage(&old, "late").is_err());
    assert!(fixture.settle(&old).is_err());
    assert_eq!(
        fixture.settle(&new).unwrap().audit.disposition,
        GoalTurnDisposition::Reset
    );
}

#[test]
fn scoped_missing_signal_and_changed_signal_reset_the_completed_turn_streak() {
    let fixture = Fixture::new();
    for (turn, signal, expected) in [
        ("one", Some("same"), 1),
        ("two", None, 0),
        ("three", Some("same"), 1),
        ("four", Some("different"), 1),
    ] {
        let id = fixture.bind("cycle", turn);
        if let Some(signal) = signal {
            fixture.stage(&id, signal).unwrap();
        }
        assert_eq!(
            fixture.settle(&id).unwrap().audit.completed_turn_streak,
            expected
        );
    }
    let id = fixture.bind("new-cycle", "five");
    fixture.stage(&id, "different").unwrap();
    assert_eq!(fixture.settle(&id).unwrap().audit.completed_turn_streak, 1);
}

#[test]
fn scoped_pause_resume_invalidates_old_turn_and_starts_fresh_audit() {
    let fixture = Fixture::new();
    let first = fixture.bind("cycle", "first");
    fixture.stage(&first, "same").unwrap();
    fixture.settle(&first).unwrap();
    let old = fixture.bind("cycle", "old");
    fixture.stage(&old, "same").unwrap();
    fixture
        .store
        .pause_with_reason(SESSION, crate::GoalPauseReason::UserInterruption)
        .unwrap();
    let paused = fixture.store.goal(SESSION).unwrap().unwrap();
    fixture
        .store
        .pool()
        .try_transaction(|tx| GoalStore::resume_explicit_in(tx, SESSION, paused.revision, 50))
        .unwrap();
    assert!(fixture.settle(&old).is_err());
    let resumed = fixture.bind("cycle", "resumed");
    fixture.stage(&resumed, "same").unwrap();
    assert_eq!(
        fixture
            .settle(&resumed)
            .unwrap()
            .audit
            .completed_turn_streak,
        1
    );
}

#[test]
fn scoped_transferred_report_origin_does_not_reauthorize_an_old_engine_turn() {
    let fixture = Fixture::new();
    let old = fixture.bind("old-cycle", "old-turn");
    fixture.stage(&old, "old observation").unwrap();
    fixture
        .store
        .pause_with_reason(SESSION, crate::GoalPauseReason::UserInterruption)
        .unwrap();
    let paused = fixture.store.goal(SESSION).unwrap().unwrap();
    fixture
        .store
        .pool()
        .try_transaction(|tx| GoalStore::resume_explicit_in(tx, SESSION, paused.revision, 50))
        .unwrap();
    let current = fixture.bind("new-cycle", "new-turn");
    fixture
        .store
        .pool()
        .try_transaction(|tx| -> Result<(), GoalError> {
            let mut cycle = zuno_db::session_work_cycle::current_in(tx, SESSION)?.unwrap();
            cycle.resumed_goal_cycles.insert(old.cycle_id.clone());
            zuno_db::session_work_cycle::save_in(tx, &cycle, 51)?;
            assert_eq!(
                zuno_db::session_work_cycle::completion_cycle_in(tx, SESSION, &old.cycle_id)?,
                Some(current.cycle_id.clone())
            );
            Ok(())
        })
        .unwrap();
    assert!(fixture.stage(&old, "late").is_err());
    assert!(
        fixture
            .settle(&old)
            .unwrap_err()
            .is_expected_turn_invalidation()
    );
    fixture.stage(&current, "current observation").unwrap();
    assert_eq!(
        fixture
            .settle(&current)
            .unwrap()
            .audit
            .completed_turn_streak,
        1
    );
}

#[test]
fn scoped_model_completion_preserves_cursor_for_inactive_turn_settlement() {
    let fixture = Fixture::new();
    let id = fixture.bind("cycle", "real");
    fixture.stage(&id, "previously observed").unwrap();
    let goal = fixture.store.goal(SESSION).unwrap().unwrap();
    let complete = fixture
        .store
        .complete_as_model_with_criteria_checked(SESSION, goal.revision, &[], &[])
        .unwrap()
        .unwrap();
    assert_eq!(complete.status, GoalStatus::Complete);
    assert_eq!(
        fixture.store.current_goal_turn(SESSION).unwrap(),
        Some(id.clone())
    );
    let result = fixture.settle(&id).unwrap();
    assert_eq!(result.audit.disposition, GoalTurnDisposition::Inactive);
    assert_eq!(result.audit.status, GoalStatus::Complete);
    assert_eq!(result.audit.completed_turn_streak, 0);
    assert_eq!(result.current_goal, Some(complete));
    assert!(fixture.settle(&id).unwrap().replayed);
}

#[test]
fn scoped_native_pause_rejects_the_old_unsettled_turn_before_resume() {
    let fixture = Fixture::new();
    let id = fixture.bind("cycle", "real");
    fixture.stage(&id, "observed").unwrap();
    let paused = fixture
        .store
        .pause_with_reason(SESSION, crate::GoalPauseReason::UserInterruption)
        .unwrap()
        .unwrap();
    let history = fixture.store.history(SESSION).unwrap();
    let error = fixture
        .settle(&id)
        .expect_err("native pause must invalidate the captured turn");
    assert!(matches!(
        error,
        GoalError::GoalTurnConflict {
            reason: GoalTurnConflictReason::BindingInvalidated,
            ..
        }
    ));
    assert!(error.is_expected_turn_invalidation());
    assert_eq!(fixture.store.goal(SESSION).unwrap(), Some(paused));
    assert_eq!(fixture.store.history(SESSION).unwrap(), history);
    assert_eq!(row_count(&fixture.store, "goal_turn_audit", SESSION), 0);
    assert_eq!(
        row_count(&fixture.store, "goal_turn_observation", SESSION),
        1
    );
}

#[test]
fn scoped_expected_invalidation_classification_never_hides_database_or_corruption() {
    use GoalTurnConflictReason as Reason;
    for reason in [
        Reason::CycleChanged,
        Reason::CycleStopped,
        Reason::GoalUnowned,
        Reason::GoalReplaced,
        Reason::BindingInvalidated,
        Reason::TurnSuperseded,
    ] {
        assert!(turn_conflict(SESSION, reason).is_expected_turn_invalidation());
    }
    for error in [
        GoalError::Db(DbError::Busy { retry_after: None }),
        GoalError::GoalTurnAuditCorrupt {
            session_id: SESSION.to_owned(),
        },
        GoalError::GoalTurnContextMissing,
        GoalError::InvalidGoalTurnIdentity { field: "turn_id" },
        turn_conflict(SESSION, Reason::CycleUnavailable),
        turn_conflict(SESSION, Reason::AttemptSessionMismatch),
        turn_conflict(SESSION, Reason::NativeBindingChanged),
        turn_conflict(SESSION, Reason::GoalStateConflict),
    ] {
        assert!(!error.is_expected_turn_invalidation(), "{error:?}");
    }
}

#[test]
fn scoped_native_user_block_invalidates_the_captured_turn_without_model_authority() {
    let fixture = Fixture::new();
    let id = fixture.bind("cycle", "real");
    fixture.stage(&id, "model observation").unwrap();
    let goal = fixture.store.goal(SESSION).unwrap().unwrap();
    let blocked = fixture
        .store
        .block_as_user_checked(SESSION, goal.revision, "native user reason")
        .unwrap()
        .unwrap();
    assert_eq!(blocked.status, GoalStatus::Blocked);
    assert_eq!(
        blocked.blocked_reason.as_deref(),
        Some("native user reason")
    );
    assert!(
        fixture
            .settle(&id)
            .unwrap_err()
            .is_expected_turn_invalidation()
    );
    assert_eq!(fixture.store.goal(SESSION).unwrap(), Some(blocked));
    assert_eq!(row_count(&fixture.store, "goal_turn_audit", SESSION), 0);
}

#[test]
fn scoped_cycle_without_goal_or_stopped_cycle_cannot_grant_goal_authority() {
    let fixture = Fixture::new();
    let id = fixture.bind("cycle", "one");
    for stopped in [false, true] {
        select_cycle(
            &fixture.store,
            SESSION,
            "cycle",
            stopped.then_some(id.goal_id.as_str()),
        );
        if stopped {
            fixture
                .store
                .pool()
                .try_transaction(|tx| -> Result<(), GoalError> {
                    let mut cycle = zuno_db::session_work_cycle::current_in(tx, SESSION)?.unwrap();
                    cycle.stopped = Some(zuno_db::session_work_cycle::CycleStop {
                        turn_id: Some(id.turn_id.clone()),
                        input_id: None,
                        user_cancelled: true,
                        at_ms: 2,
                    });
                    zuno_db::session_work_cycle::save_in(tx, &cycle, 2)?;
                    Ok(())
                })
                .unwrap();
        }
        assert!(fixture.stage(&id, "must not count").is_err());
        assert!(fixture.settle(&id).is_err());
        assert!(
            fixture
                .store
                .bind_goal_turn_checked(SESSION, &id, Some(&id))
                .is_err()
        );
    }
    assert_eq!(row_count(&fixture.store, "goal_turn_audit", SESSION), 0);
}

#[test]
fn scoped_receipt_failure_rolls_back_consumption_count_status_and_history() {
    let fixture = Fixture::new();
    for turn in ["one", "two"] {
        let id = fixture.bind("cycle", turn);
        fixture.stage(&id, "same").unwrap();
        fixture.settle(&id).unwrap();
    }
    let id = fixture.bind("cycle", "three");
    fixture.stage(&id, "same").unwrap();
    let before = fixture.store.goal(SESSION).unwrap();
    let history = fixture.store.history(SESSION).unwrap();
    fixture
        .store
        .pool()
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_audit BEFORE INSERT ON goal_turn_audit
         BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END;",
        )
        .unwrap();
    assert!(fixture.settle(&id).is_err());
    assert_eq!(fixture.store.goal(SESSION).unwrap(), before);
    assert_eq!(fixture.store.history(SESSION).unwrap(), history);
    assert_eq!(
        row_count(&fixture.store, "goal_turn_observation", SESSION),
        1
    );
    assert_eq!(fixture.stage(&id, "same").unwrap().completed_turn_streak, 2);
    let fixture = fixture.restart();
    fixture
        .store
        .pool()
        .get()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_audit")
        .unwrap();
    assert_eq!(
        fixture.settle(&id).unwrap().audit.disposition,
        GoalTurnDisposition::Blocked
    );
}

#[test]
fn scoped_outer_transaction_rollback_preserves_observation() {
    let fixture = Fixture::new();
    let id = fixture.bind("cycle", "one");
    fixture.stage(&id, "same").unwrap();
    let mut connection = fixture.store.pool().get().unwrap();
    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    GoalStore::settle_goal_turn_in(&tx, SESSION, &id, GoalTurnOutcome::Progress, 10).unwrap();
    tx.rollback().unwrap();
    drop(connection);
    assert_eq!(row_count(&fixture.store, "goal_turn_audit", SESSION), 0);
    assert_eq!(fixture.settle(&id).unwrap().audit.completed_turn_streak, 1);
}

#[test]
fn scoped_corrupt_receipt_fails_closed_instead_of_recounting() {
    let fixture = Fixture::new();
    let id = fixture.bind("cycle", "one");
    fixture.stage(&id, "same").unwrap();
    fixture.settle(&id).unwrap();
    fixture
        .store
        .pool()
        .get()
        .unwrap()
        .execute("UPDATE goal_turn_audit SET audit='{}'", [])
        .unwrap();
    assert!(matches!(
        fixture.settle(&id),
        Err(GoalError::GoalTurnAuditCorrupt { .. })
    ));
    assert_eq!(
        cycle_failure_in(&fixture.store.pool().get().unwrap(), SESSION)
            .unwrap()
            .unwrap()
            .consecutive_turns,
        1
    );
}

#[test]
fn scoped_observations_cannot_reopen_a_settled_turn() {
    let fixture = Fixture::new();
    let id = fixture.bind("cycle", "one");
    fixture.settle(&id).unwrap();
    assert!(matches!(
        fixture.stage(&id, "late"),
        Err(GoalError::GoalTurnAlreadySettled { .. })
    ));
    assert!(matches!(
        fixture
            .store
            .bind_goal_turn_checked(SESSION, &id, Some(&id)),
        Err(GoalError::GoalTurnAlreadySettled { .. })
    ));
    assert_eq!(
        row_count(&fixture.store, "goal_turn_observation", SESSION),
        0
    );
}

#[test]
fn scoped_rebinding_current_unfinished_turn_keeps_prior_completed_streak() {
    let fixture = Fixture::new();
    let first = fixture.bind("cycle", "first");
    fixture.stage(&first, "same").unwrap();
    fixture.settle(&first).unwrap();
    let second = fixture.bind("cycle", "second");
    fixture.stage(&second, "same").unwrap();
    fixture
        .store
        .bind_goal_turn_checked(SESSION, &second, Some(&second))
        .unwrap();
    assert_eq!(
        fixture
            .stage(&second, "same")
            .unwrap()
            .completed_turn_streak,
        1
    );
    assert_eq!(
        fixture.settle(&second).unwrap().audit.completed_turn_streak,
        2
    );
}

#[test]
fn scoped_replay_ignores_changed_outcome_and_keeps_newer_same_cycle_observation() {
    let fixture = Fixture::new();
    let first = fixture.bind("cycle", "first");
    fixture.stage(&first, "same").unwrap();
    let original = fixture.settle(&first).unwrap();
    let second = fixture.bind("cycle", "second");
    fixture.stage(&second, "newer").unwrap();
    let replay = fixture
        .store
        .settle_goal_turn(SESSION, &first, GoalTurnOutcome::Blocking("changed"))
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.audit, original.audit);
    assert_eq!(
        row_count(&fixture.store, "goal_turn_observation", SESSION),
        1
    );
    assert_eq!(
        fixture.settle(&second).unwrap().audit.signal.as_deref(),
        Some("newer")
    );
}

#[test]
fn scoped_concurrent_settlements_count_the_same_real_turn_once() {
    let fixture = Fixture::new();
    let id = fixture.bind("cycle", "one");
    fixture.stage(&id, "same").unwrap();
    let store = Arc::new(fixture.store);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let joins = (0..2)
        .map(|_| {
            let (store, barrier, id) = (Arc::clone(&store), Arc::clone(&barrier), id.clone());
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .settle_goal_turn(SESSION, &id, GoalTurnOutcome::Progress)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let results = joins
        .into_iter()
        .map(|join| join.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| !result.replayed).count(), 1);
    assert_eq!(results[0].audit, results[1].audit);
    assert_eq!(results[0].audit.completed_turn_streak, 1);
}

#[test]
fn scoped_schema_matches_the_parent_owned_format15_ddl() {
    let parent = include_str!("../../zuno-db/src/schema/goal_turn.sql");
    let ours = GOAL_TURN_SCHEMA.replace("IF NOT EXISTS ", "");
    assert_eq!(
        ours.split_whitespace().collect::<Vec<_>>(),
        parent.split_whitespace().collect::<Vec<_>>()
    );
}
