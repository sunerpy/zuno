use super::*;
use crate::goal_turn_test_support::{fence, row_count, select_cycle};
use crate::{GoalPauseReason, SystemStatus};
use std::num::NonZeroU32;
use std::time::Duration;
use zuno_engine::budget::{BudgetStopKind, ProviderRequestUsage};

const SESSION: &str = "ses_budget_scope";
const TURN: &str = "real-turn";

struct Fixture {
    store: Arc<GoalStore>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(budget: Option<i64>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(GoalStore::open_memory(dir.path().to_owned()).unwrap());
        store.create_goal(SESSION, "original Goal", budget).unwrap();
        Self { store, _dir: dir }
    }

    fn scope(&self, goal_id: Option<&str>, cycle: &str, turn: &str) {
        select_cycle(&self.store, SESSION, cycle, goal_id);
        fence(&self.store, SESSION, turn);
    }

    fn goal(&self) -> Goal {
        self.store.goal(SESSION).unwrap().unwrap()
    }

    fn policy(&self) -> GoalBudgetPolicy {
        GoalBudgetPolicy::new(Arc::clone(&self.store))
    }
}

fn snapshot(turn: &str, step: u32, tokens: u64) -> TurnUsageSnapshot<'_> {
    TurnUsageSnapshot {
        session_id: SESSION,
        turn_id: turn,
        step,
        turn_usage: ProviderRequestUsage::default(),
        last_request: ProviderRequestUsage {
            input_tokens: tokens,
            accounted: true,
            ..Default::default()
        },
        estimated_prompt_tokens: 0,
        context_limit: None,
        elapsed_seconds: 0,
        tool_calls_dispatched: 0,
    }
}

#[tokio::test]
async fn scoped_budget_independent_input_does_not_charge_old_paused_goal() {
    let fixture = Fixture::new(Some(1000));
    fixture.store.record_usage(SESSION, 123, 7, true).unwrap();
    fixture
        .store
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap();
    fixture.scope(None, "independent", TURN);
    let before = fixture.goal();
    let policy = fixture.policy();
    assert_eq!(
        policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap(),
        BudgetDecision::Continue
    );
    assert_eq!(
        policy
            .after_response(&snapshot(TURN, 1, 800))
            .await
            .unwrap(),
        BudgetDecision::Continue
    );
    assert_eq!(
        fixture.goal(),
        before,
        "unowned request must not change old usage/status/budget"
    );
    assert_eq!(row_count(&fixture.store, "goal_request_usage", SESSION), 0);
}

#[tokio::test]
async fn scoped_budget_independent_input_is_not_stopped_by_old_budget_limited_goal() {
    let fixture = Fixture::new(Some(100));
    fixture.store.record_usage(SESSION, 100, 7, true).unwrap();
    fixture.scope(None, "independent", TURN);
    let before = fixture.goal();
    assert_eq!(before.status, GoalStatus::BudgetLimited);
    let policy = fixture.policy();
    assert_eq!(
        policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap(),
        BudgetDecision::Continue
    );
    assert_eq!(
        policy
            .after_response(&snapshot(TURN, 1, 900))
            .await
            .unwrap(),
        BudgetDecision::Continue
    );
    assert_eq!(fixture.goal(), before);
}

#[tokio::test]
async fn scoped_budget_owned_request_is_charged_once_and_obeys_its_ceiling() {
    let fixture = Fixture::new(Some(100));
    fixture.scope(Some(&fixture.goal().goal_id), "owned", TURN);
    let policy = fixture.policy();
    assert_eq!(
        policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap(),
        BudgetDecision::Continue
    );
    assert!(
        matches!(policy.after_response(&snapshot(TURN, 1, 100)).await.unwrap(),
        BudgetDecision::Stop(stop) if stop.kind == BudgetStopKind::TokenBudget)
    );
    let charged = fixture.goal();
    assert_eq!(charged.tokens_used, 100);
    assert_eq!(charged.status, GoalStatus::BudgetLimited);
    assert!(
        matches!(policy.clone().after_response(&snapshot(TURN, 1, 100)).await.unwrap(),
        BudgetDecision::Stop(stop) if stop.kind == BudgetStopKind::TokenBudget)
    );
    assert_eq!(fixture.goal(), charged);
    assert_eq!(row_count(&fixture.store, "goal_request_usage", SESSION), 1);
}

#[tokio::test]
async fn scoped_budget_independent_inflight_request_is_not_reassigned_after_goal_resume() {
    let fixture = Fixture::new(Some(1000));
    let paused = fixture
        .store
        .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
        .unwrap()
        .unwrap();
    fixture.scope(None, "input", TURN);
    let policy = fixture.policy();
    policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap();
    fixture
        .store
        .pool()
        .try_transaction(|tx| GoalStore::resume_explicit_in(tx, SESSION, paused.revision, 10))
        .unwrap();
    fixture.scope(Some(&paused.goal_id), "input", TURN);
    let resumed = fixture.goal();
    policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap();
    assert_eq!(
        policy
            .after_response(&snapshot(TURN, 1, 250))
            .await
            .unwrap(),
        BudgetDecision::Continue
    );
    assert_eq!(
        fixture.goal(),
        resumed,
        "resume must not retroactively charge an independent request"
    );
    policy.before_request(&snapshot(TURN, 2, 0)).await.unwrap();
    policy
        .after_response(&snapshot(TURN, 2, 100))
        .await
        .unwrap();
    assert_eq!(fixture.goal().tokens_used, resumed.tokens_used + 100);
}

#[tokio::test]
async fn scoped_budget_repeated_before_keeps_the_original_owned_goal_ceiling() {
    let fixture = Fixture::new(Some(100));
    let original = fixture.goal();
    fixture.scope(Some(&original.goal_id), "owned", TURN);
    let policy = fixture.policy();
    policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap();
    fixture.store.record_usage(SESSION, 100, 0, true).unwrap();
    fixture.scope(None, "independent-next", "next-turn");
    assert!(
        matches!(policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap(),
        BudgetDecision::Stop(stop) if stop.kind == BudgetStopKind::TokenBudget),
        "a repeated admission check must not discard the already-pinned Goal budget"
    );
}

#[tokio::test]
async fn scoped_budget_unadmitted_response_cannot_infer_an_owned_goal() {
    let fixture = Fixture::new(Some(100));
    let before = fixture.goal();
    fixture.scope(Some(&before.goal_id), "owned", TURN);
    assert!(matches!(
        fixture
            .policy()
            .after_response(&snapshot(TURN, 1, 50))
            .await,
        Err(BudgetPolicyError::Permanent(_))
    ));
    assert_eq!(fixture.goal(), before);
    assert_eq!(row_count(&fixture.store, "goal_request_usage", SESSION), 0);
}

#[tokio::test]
async fn scoped_budget_wrong_or_stopped_turn_never_acquires_an_owner() {
    for stopped in [true, false] {
        let fixture = Fixture::new(Some(100));
        let original = fixture.goal();
        fixture.scope(
            Some(&original.goal_id),
            "owned",
            if stopped { TURN } else { "other-turn" },
        );
        if stopped {
            fixture
                .store
                .pool()
                .try_transaction(|tx| -> Result<(), GoalError> {
                    let mut scope = zuno_db::session_work_cycle::current_in(tx, SESSION)?.unwrap();
                    scope.stopped = Some(zuno_db::session_work_cycle::CycleStop {
                        turn_id: Some(TURN.to_owned()),
                        input_id: None,
                        user_cancelled: true,
                        at_ms: 10,
                    });
                    zuno_db::session_work_cycle::save_in(tx, &scope, 10)?;
                    Ok(())
                })
                .unwrap();
        }
        let policy = fixture.policy();
        policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap();
        assert_eq!(
            policy
                .after_response(&snapshot(TURN, 1, 200))
                .await
                .unwrap(),
            BudgetDecision::Continue
        );
        assert_eq!(fixture.goal(), original);
    }
}

#[tokio::test]
async fn scoped_budget_charge_and_request_receipt_rollback_together() {
    let fixture = Fixture::new(Some(1000));
    let before = fixture.goal();
    fixture.scope(Some(&before.goal_id), "owned", TURN);
    let policy = fixture.policy();
    policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap();
    fixture
        .store
        .pool()
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_budget_charge BEFORE UPDATE OF tokens_used ON goal
         BEGIN SELECT RAISE(ABORT, 'injected charge failure'); END;",
        )
        .unwrap();
    assert!(
        matches!(policy.after_response(&snapshot(TURN, 1, 100)).await.unwrap(),
        BudgetDecision::Stop(stop) if stop.kind == BudgetStopKind::UsageUnknown)
    );
    assert_eq!(fixture.goal(), before);
    assert_eq!(row_count(&fixture.store, "goal_request_usage", SESSION), 0);
    fixture
        .store
        .pool()
        .get()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_budget_charge")
        .unwrap();
    assert_eq!(
        policy
            .after_response(&snapshot(TURN, 1, 100))
            .await
            .unwrap(),
        BudgetDecision::Continue
    );
    assert_eq!(fixture.goal().tokens_used, 100);
    assert_eq!(row_count(&fixture.store, "goal_request_usage", SESSION), 1);
}

#[tokio::test]
async fn scoped_budget_replaced_goal_never_receives_original_request_charge() {
    let fixture = Fixture::new(Some(1000));
    let old = fixture.goal();
    fixture.scope(Some(&old.goal_id), "owned", TURN);
    let policy = fixture.policy();
    policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap();
    fixture
        .store
        .replace_goal_as_system(SESSION, "new Goal", Some(9000))
        .unwrap();
    let replacement = fixture.goal();
    assert_ne!(replacement.goal_id, old.goal_id);
    // Even a native rebind under the same actual turn cannot change the owner of
    // a request that was already sent.
    fixture.scope(Some(&replacement.goal_id), "owned", TURN);
    assert_eq!(
        policy
            .after_response(&snapshot(TURN, 1, 700))
            .await
            .unwrap(),
        BudgetDecision::Continue
    );
    assert_eq!(fixture.goal(), replacement);
    assert_eq!(row_count(&fixture.store, "goal_request_usage", SESSION), 0);
    policy.before_request(&snapshot(TURN, 2, 0)).await.unwrap();
    policy
        .after_response(&snapshot(TURN, 2, 100))
        .await
        .unwrap();
    let next = fixture.goal();
    policy
        .after_response(&snapshot(TURN, 1, 700))
        .await
        .unwrap();
    assert_eq!(
        fixture.goal(),
        next,
        "old response redelivery remains owned by the old Goal"
    );
}

#[tokio::test]
async fn scoped_budget_final_response_keeps_original_owner_after_pause_or_completion() {
    for pause in [true, false] {
        let fixture = Fixture::new(Some(1000));
        let original = fixture.goal();
        fixture.scope(Some(&original.goal_id), "owned", TURN);
        let policy = fixture.policy();
        policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap();
        let closed = if pause {
            fixture
                .store
                .pause_with_reason(SESSION, GoalPauseReason::UserInterruption)
                .unwrap()
                .unwrap()
        } else {
            fixture
                .store
                .complete_checked(SESSION, original.revision)
                .unwrap()
                .unwrap()
        };
        fixture.scope(None, "independent-next", "new-turn");
        assert_eq!(
            policy
                .clone()
                .after_response(&snapshot(TURN, 1, 350))
                .await
                .unwrap(),
            BudgetDecision::Continue
        );
        let after = fixture.goal();
        assert_eq!(after.goal_id, closed.goal_id);
        assert_eq!(after.status, closed.status);
        assert_eq!(after.token_budget, closed.token_budget);
        assert_eq!(after.revision, closed.revision);
        assert_eq!(after.tokens_used, closed.tokens_used + 350);
    }
}

#[tokio::test]
async fn scoped_budget_independent_input_retains_host_turn_ceilings() {
    for time in [false, true] {
        let fixture = Fixture::new(Some(10));
        fixture
            .store
            .set_status_as_system(SESSION, SystemStatus::Paused)
            .unwrap();
        fixture.scope(None, "independent", TURN);
        let before = fixture.goal();
        let policy = fixture.policy().with_allowance(TurnAllowance {
            default_token_budget: Some(5),
            max_tool_calls: (!time).then(|| NonZeroU32::new(1).unwrap()),
            max_duration: time.then(|| Duration::from_secs(1)),
        });
        policy.before_request(&snapshot(TURN, 1, 0)).await.unwrap();
        let mut response = snapshot(TURN, 1, 500);
        response.tool_calls_dispatched = 2;
        response.elapsed_seconds = 2;
        let expected = if time {
            BudgetStopKind::TimeBudget
        } else {
            BudgetStopKind::ToolCallBudget
        };
        assert!(matches!(policy.after_response(&response).await.unwrap(),
            BudgetDecision::Stop(stop) if stop.kind == expected));
        assert_eq!(fixture.goal(), before);
    }
}
