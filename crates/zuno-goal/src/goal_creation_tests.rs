use super::*;
use crate::goal_turn_test_support::{fence, row_count, select_cycle, with_snapshot};
use crate::{GoalStatus, GoalTurnDisposition, GoalTurnIdentity, GoalTurnOutcome, SystemStatus};
use zuno_db::session_work_cycle::{CycleStop, SessionWorkCycle};
use zuno_tool::{AllowAll, NeverInterrupted};

const SESSION: &str = "ses_scoped_creation";
const CYCLE: &str = "input-cycle";
const TURN: &str = "actual-engine-turn";

struct Fixture {
    store: Arc<GoalStore>,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            GoalStore::open_at(&dir.path().join("state.db"), dir.path().join("spill")).unwrap(),
        );
        select_cycle(&store, SESSION, CYCLE, None);
        fence(&store, SESSION, TURN);
        Self { store, dir }
    }

    fn restart(self) -> Self {
        let Self { store, dir } = self;
        drop(store);
        let store = Arc::new(
            GoalStore::open_at(&dir.path().join("state.db"), dir.path().join("spill")).unwrap(),
        );
        Self { store, dir }
    }

    fn bare_context(&self) -> ToolContext {
        ToolContext::new(
            SESSION,
            "assistant",
            "call",
            "build",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    fn context(&self) -> ToolContext {
        with_snapshot(
            self.bare_context(),
            &GoalTurnIdentity::new("not-yet-created", CYCLE, TURN).unwrap(),
        )
    }

    fn scope(&self) -> SessionWorkCycle {
        zuno_db::session_work_cycle::current_in(&self.store.pool().get().unwrap(), SESSION)
            .unwrap()
            .unwrap()
    }

    fn edit_scope(&self, edit: impl FnOnce(&mut SessionWorkCycle)) {
        self.store
            .pool()
            .try_transaction(|tx| -> Result<(), GoalError> {
                let mut scope = zuno_db::session_work_cycle::current_in(tx, SESSION)?.unwrap();
                edit(&mut scope);
                zuno_db::session_work_cycle::save_in(tx, &scope, 2)?;
                Ok(())
            })
            .unwrap();
    }

    async fn create(&self, context: ToolContext) -> Result<ToolOutput, ToolError> {
        erase(CreateGoalTool::new(Arc::clone(&self.store))).execute(serde_json::json!({
            "objective":"deliver current requested Goal","success_criteria":["verify delivery"]
        }), context).await
    }

    async fn complete(&self, context: ToolContext, revision: i64) -> Result<ToolOutput, ToolError> {
        erase(UpdateGoalTool::new(Arc::clone(&self.store))).execute(serde_json::json!({
            "expected_revision":revision,"status":"complete",
            "waive_criteria":[{"criterionId":"c1","reason":"fixture models authority, not business evidence"}]
        }), context).await
    }

    fn assert_no_creation(&self, before: &SessionWorkCycle) {
        assert!(self.store.goal(SESSION).unwrap().is_none());
        assert_eq!(self.scope(), *before);
        assert_eq!(row_count(&self.store, "goal_criterion", SESSION), 0);
        assert_eq!(row_count(&self.store, "goal_history", SESSION), 0);
        assert!(self.store.current_goal_turn(SESSION).unwrap().is_none());
    }
}

#[tokio::test]
async fn scoped_creation_can_stage_blocked_in_the_same_actual_turn() {
    let fixture = Fixture::new();
    let created = goal_from_metadata(&fixture.create(fixture.context()).await.unwrap())
        .unwrap()
        .unwrap();
    let id = GoalTurnIdentity::new(&created.goal_id, CYCLE, TURN).unwrap();
    assert_eq!(
        fixture.scope().goal_id.as_deref(),
        Some(created.goal_id.as_str())
    );
    assert_eq!(
        fixture.store.current_goal_turn(SESSION).unwrap(),
        Some(id.clone())
    );
    let result = erase(UpdateGoalTool::new(Arc::clone(&fixture.store))).execute(serde_json::json!({
        "expected_revision":created.revision,"status":"blocked","blocking_condition":"external blocker"
    }), fixture.context()).await.unwrap();
    assert_eq!(
        result.metadata["blockedObservation"]["disposition"],
        "staged"
    );
    assert_eq!(
        fixture
            .store
            .settle_goal_turn(SESSION, &id, GoalTurnOutcome::Progress)
            .unwrap()
            .audit
            .completed_turn_streak,
        1
    );
}

#[tokio::test]
async fn scoped_creation_can_complete_in_the_same_actual_turn() {
    let fixture = Fixture::new();
    let created = goal_from_metadata(&fixture.create(fixture.context()).await.unwrap())
        .unwrap()
        .unwrap();
    let id = GoalTurnIdentity::new(&created.goal_id, CYCLE, TURN).unwrap();
    assert_eq!(
        fixture.store.current_goal_turn(SESSION).unwrap(),
        Some(id.clone())
    );
    let completed = fixture
        .complete(fixture.context(), created.revision)
        .await
        .unwrap();
    assert_eq!(
        goal_from_metadata(&completed).unwrap().unwrap().status,
        GoalStatus::Complete
    );
    assert_eq!(
        fixture
            .store
            .settle_goal_turn(SESSION, &id, GoalTurnOutcome::Progress)
            .unwrap()
            .audit
            .disposition,
        GoalTurnDisposition::Inactive
    );
}

#[tokio::test]
async fn scoped_creation_rejects_stale_or_untrusted_snapshot_without_writes() {
    for case in [
        "new-turn",
        "new-cycle",
        "stopped",
        "no-fence",
        "no-snapshot",
        "other-session",
    ] {
        let fixture = Fixture::new();
        let mut context = fixture.context();
        match case {
            "new-turn" => {
                fixture.edit_scope(|scope| scope.active_turn_id = Some("new-turn".to_owned()))
            }
            "new-cycle" => {
                select_cycle(&fixture.store, SESSION, "new-cycle", None);
                fence(&fixture.store, SESSION, "new-turn");
                fixture.edit_scope(|scope| {
                    scope.resumed_goal_cycles.insert(CYCLE.to_owned());
                });
            }
            "stopped" => fixture.edit_scope(|scope| {
                scope.stopped = Some(CycleStop {
                    turn_id: Some(TURN.to_owned()),
                    input_id: None,
                    user_cancelled: true,
                    at_ms: 2,
                })
            }),
            "no-fence" => fixture.edit_scope(|scope| scope.active_turn_id = None),
            "no-snapshot" => context = fixture.bare_context(),
            "other-session" => {
                let mut snapshot = context.orchestration_snapshot().unwrap().as_ref().clone();
                snapshot.owner.session_id = "other-session".to_owned();
                context = context.with_orchestration_snapshot(Arc::new(snapshot));
            }
            _ => unreachable!(),
        }
        let before = fixture.scope();
        assert!(fixture.create(context).await.is_err(), "accepted {case}");
        fixture.assert_no_creation(&before);
    }
}

#[tokio::test]
async fn scoped_creation_rolls_back_goal_scope_cursor_and_history_on_failure() {
    for (table, trigger_condition) in [
        ("goal_cycle_failure", ""),
        ("event", "WHEN NEW.type='session.goal.created_and_bound.1'"),
    ] {
        let fixture = Fixture::new();
        let before = fixture.scope();
        fixture
            .store
            .pool()
            .get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER fail_creation BEFORE INSERT ON {table} {trigger_condition}
             BEGIN SELECT RAISE(ABORT, 'injected atomic creation failure'); END;"
            ))
            .unwrap();
        assert!(
            fixture.create(fixture.context()).await.is_err(),
            "failed to exercise {table}"
        );
        fixture.assert_no_creation(&before);
        let fixture = fixture.restart();
        fixture.assert_no_creation(&before);
        fixture
            .store
            .pool()
            .get()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_creation")
            .unwrap();
        let created = goal_from_metadata(&fixture.create(fixture.context()).await.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            fixture
                .store
                .current_goal_turn(SESSION)
                .unwrap()
                .unwrap()
                .goal_id,
            created.goal_id
        );
    }
}

#[tokio::test]
async fn scoped_creation_never_replaces_or_resumes_an_unfinished_old_goal() {
    for status in [
        GoalStatus::Active,
        GoalStatus::Paused,
        GoalStatus::Blocked,
        GoalStatus::BudgetLimited,
        GoalStatus::UsageLimited,
    ] {
        let fixture = Fixture::new();
        fixture
            .store
            .create_goal(SESSION, "old native Goal", Some(9000))
            .unwrap();
        fixture.store.record_usage(SESSION, 123, 7, true).unwrap();
        match status {
            GoalStatus::Active => {}
            GoalStatus::Blocked => {
                let goal = fixture.store.goal(SESSION).unwrap().unwrap();
                fixture
                    .store
                    .block_as_user_checked(SESSION, goal.revision, "old block")
                    .unwrap();
            }
            status => {
                fixture
                    .store
                    .set_status_as_system(SESSION, SystemStatus::from_status(status).unwrap())
                    .unwrap();
            }
        }
        let before = fixture.store.goal(SESSION).unwrap();
        let history = fixture.store.history(SESSION).unwrap();
        let scope = fixture.scope();
        assert!(
            fixture.create(fixture.context()).await.is_err(),
            "replaced {status}"
        );
        assert_eq!(fixture.store.goal(SESSION).unwrap(), before);
        assert_eq!(fixture.store.history(SESSION).unwrap(), history);
        assert_eq!(fixture.scope(), scope);
        assert!(fixture.store.current_goal_turn(SESSION).unwrap().is_none());
    }
}

#[tokio::test]
async fn scoped_creation_complete_tool_cannot_close_unowned_blocked_goal() {
    let fixture = Fixture::new();
    let old = fixture
        .store
        .create_goal_as_model(
            SESSION,
            "old goal",
            &["verify delivery".to_owned()],
            Some(9000),
        )
        .unwrap();
    let blocked = fixture
        .store
        .block_as_user_checked(SESSION, old.goal.revision, "old blocker")
        .unwrap()
        .unwrap();
    let scope = fixture.scope();
    assert!(scope.goal_id.is_none());
    let criteria = fixture.store.criteria(SESSION).unwrap();
    assert!(
        fixture
            .complete(fixture.context(), blocked.revision)
            .await
            .is_err(),
        "JSON expected_revision cannot grant authority over an unowned old Goal"
    );
    assert_eq!(fixture.store.goal(SESSION).unwrap(), Some(blocked));
    assert_eq!(fixture.store.criteria(SESSION).unwrap(), criteria);
    assert_eq!(fixture.scope(), scope);
}

#[tokio::test]
async fn scoped_creation_complete_tool_checks_fence_and_scope_before_criterion_updates() {
    for case in ["new-turn", "new-cycle", "stopped", "no-snapshot"] {
        let fixture = Fixture::new();
        let old = fixture
            .store
            .create_goal_as_model(SESSION, "old", &["verify delivery".to_owned()], None)
            .unwrap();
        fixture.edit_scope(|scope| scope.goal_id = Some(old.goal.goal_id.clone()));
        let id = GoalTurnIdentity::new(&old.goal.goal_id, CYCLE, TURN).unwrap();
        fixture
            .store
            .bind_goal_turn_checked(SESSION, &id, None)
            .unwrap();
        let mut context = fixture.context();
        match case {
            "new-turn" => {
                fixture.edit_scope(|scope| scope.active_turn_id = Some("new-turn".to_owned()))
            }
            "new-cycle" => {
                select_cycle(&fixture.store, SESSION, "new-cycle", None);
                fence(&fixture.store, SESSION, "new-turn");
            }
            "stopped" => fixture.edit_scope(|scope| {
                scope.stopped = Some(CycleStop {
                    turn_id: Some(TURN.to_owned()),
                    input_id: None,
                    user_cancelled: true,
                    at_ms: 2,
                })
            }),
            "no-snapshot" => context = fixture.bare_context(),
            _ => unreachable!(),
        }
        let scope = fixture.scope();
        assert!(
            fixture.complete(context, old.goal.revision).await.is_err(),
            "accepted {case}"
        );
        assert_eq!(fixture.store.goal(SESSION).unwrap(), Some(old.goal));
        assert_eq!(fixture.store.criteria(SESSION).unwrap(), old.criteria);
        assert_eq!(fixture.scope(), scope);
    }
}

#[tokio::test]
async fn scoped_creation_cannot_reuse_an_already_settled_actual_turn() {
    let fixture = Fixture::new();
    let created = goal_from_metadata(&fixture.create(fixture.context()).await.unwrap())
        .unwrap()
        .unwrap();
    fixture
        .complete(fixture.context(), created.revision)
        .await
        .unwrap();
    let identity = fixture.store.current_goal_turn(SESSION).unwrap().unwrap();
    fixture
        .store
        .settle_goal_turn(SESSION, &identity, GoalTurnOutcome::Progress)
        .unwrap();
    let goal = fixture.store.goal(SESSION).unwrap();
    let history = fixture.store.history(SESSION).unwrap();
    let scope = fixture.scope();
    assert!(
        fixture.create(fixture.context()).await.is_err(),
        "an old completed turn cannot replace its completed Goal using the unchanged last-start fence"
    );
    assert_eq!(fixture.store.goal(SESSION).unwrap(), goal);
    assert_eq!(fixture.store.history(SESSION).unwrap(), history);
    assert_eq!(fixture.scope(), scope);
    assert_eq!(
        fixture.store.current_goal_turn(SESSION).unwrap(),
        Some(identity)
    );
}

#[tokio::test]
async fn scoped_creation_concurrent_proposals_leave_one_owned_goal() {
    let fixture = Fixture::new();
    let (first, second) = tokio::join!(
        fixture.create(fixture.context()),
        fixture.create(fixture.context()),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let goal = fixture.store.goal(SESSION).unwrap().unwrap();
    assert_eq!(
        fixture.scope().goal_id.as_deref(),
        Some(goal.goal_id.as_str())
    );
    assert_eq!(
        fixture
            .store
            .current_goal_turn(SESSION)
            .unwrap()
            .unwrap()
            .goal_id,
        goal.goal_id
    );
    assert_eq!(row_count(&fixture.store, "goal_history", SESSION), 1);
    assert_eq!(row_count(&fixture.store, "goal_criterion", SESSION), 1);
}
