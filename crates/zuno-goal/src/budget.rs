//! The goal's token budget, enforced while a turn is still running.
//!
//! A turn that is only reconciled at its boundary cannot be stopped by its own
//! budget: a forty-step turn can spend a session's entire allowance before
//! anything reads a counter. [`zuno_engine::budget::TurnBudgetPolicy`] exists so
//! the loop can ask around every provider request instead, and this is the
//! implementation that answers from the durable goal — the same row the model
//! reads, the same counters the projection shows, so what stops a turn is exactly
//! what a human sees afterwards.
//!
//! # Why the decision comes from the goal row and not from the snapshot
//!
//! [`TurnUsageSnapshot`] describes one turn. The budget belongs to the goal, which
//! outlives the turn, so a decision taken from the snapshot alone would reset every
//! time a turn ended and a budget would never actually bind. Each response is
//! therefore recorded against the goal first, and the decision is read back from
//! the state that write produced.

use crate::store::{Goal, GoalStore};
use async_trait::async_trait;
use std::sync::Arc;
use zuno_engine::budget::{BudgetDecision, TurnBudgetPolicy, TurnUsageSnapshot};

/// The reserve is the budget divided by this, so one tenth of the allowance.
///
/// Compaction is not free: it costs a provider request of its own and it has to
/// leave room for the summary plus the next real request. Asking for it at the
/// exact moment the budget runs out would mean asking when there is nothing left
/// to pay with, so the last tenth is held back to buy an orderly wind-down rather
/// than a hard stop mid-thought. Deliberately a constant and not a configuration
/// key: this is the shape of the mechanism, not a preference, and a host that
/// wants a different trade-off writes a different policy.
pub const SOFT_RESERVE_DIVISOR: i64 = 10;

/// A [`TurnBudgetPolicy`] backed by the durable goal.
///
/// Records each response's tokens against the goal, then decides from the
/// resulting row: stop when the allowance is spent, stop when usage cannot be
/// measured, compact when the reserve is all that is left, otherwise continue.
///
/// A store failure is returned as `Err`, which the engine treats as a turn
/// failure. That is deliberate: a policy that cannot read the budget does not
/// know whether the turn may continue, and answering
/// [`BudgetDecision::Continue`] on a database error would turn every outage into
/// an unlimited run.
#[derive(Debug, Clone)]
pub struct GoalBudgetPolicy {
    store: Arc<GoalStore>,
}

impl GoalBudgetPolicy {
    /// Bind the policy to a shared goal store.
    #[must_use]
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self { store }
    }

    /// The idempotency key for one provider request inside a turn.
    ///
    /// `turn_id:step` identifies the request without needing an id the engine does
    /// not mint. A resumed or retried turn replays the same pair, and the ledger
    /// recognises it instead of charging twice — double-charging would end a turn
    /// that still had allowance left, which is indistinguishable from a budget
    /// that cannot be trusted.
    fn request_id(snapshot: &TurnUsageSnapshot<'_>) -> String {
        format!("{}:{}", snapshot.turn_id, snapshot.step)
    }
}

#[async_trait]
impl TurnBudgetPolicy for GoalBudgetPolicy {
    /// Decide before a request without charging for it.
    ///
    /// Reads only: nothing has been spent yet, and recording here would charge for
    /// a request that may never be issued — including the one this call is about
    /// to stop.
    ///
    /// # Errors
    ///
    /// A message when the goal cannot be read, which the engine treats as a turn
    /// failure rather than as permission to continue.
    async fn before_request(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, String> {
        let store = Arc::clone(&self.store);
        let session_id = snapshot.session_id.to_owned();
        let goal = tokio::task::spawn_blocking(move || store.goal(&session_id))
            .await
            .map_err(|error| format!("goal budget lookup did not finish: {error}"))?
            .map_err(|error| format!("goal budget lookup failed: {error}"))?;
        // `true` because the snapshot's last request is not a measurement of
        // anything yet: before the first response it is zero and unaccounted, and
        // treating that as unmeasured usage would stop every budgeted turn on its
        // first step. The goal's own `usage_known` flag still applies.
        Ok(decide(goal.as_ref(), true))
    }

    /// Record the response, then decide from what the goal now says.
    ///
    /// # Errors
    ///
    /// A message when the usage cannot be recorded or the resulting goal cannot be
    /// read, which the engine treats as a turn failure.
    async fn after_response(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, String> {
        let store = Arc::clone(&self.store);
        let session_id = snapshot.session_id.to_owned();
        let request_id = Self::request_id(snapshot);
        let tokens = i64::try_from(snapshot.last_request.total()).unwrap_or(i64::MAX);
        let measured = snapshot.last_request.accounted;
        let at_ms = crate::store::now_ms()
            .map_err(|error| format!("goal budget clock is unusable: {error}"))?;
        let recorded = tokio::task::spawn_blocking(move || {
            store.record_request_usage(&session_id, &request_id, tokens, at_ms)
        })
        .await
        .map_err(|error| format!("goal budget accounting did not finish: {error}"))?
        .map_err(|error| format!("goal budget accounting failed: {error}"))?;
        Ok(decide(recorded.goal.as_ref(), measured))
    }
}

/// The whole decision, as a pure function of the goal and whether the last
/// response was measured.
///
/// Ordered so the cheapest certainty wins. A session with no goal, or a goal with
/// no budget, has nothing to enforce and is left alone; a host that installs this
/// policy does not thereby impose a limit nobody set. A spent allowance stops the
/// turn before anything else is considered, because it is already spent whatever
/// else is true. Unmeasured usage stops next, since a budget that cannot be
/// counted cannot be honoured, and continuing on unreported numbers is how a
/// budget silently becomes advisory.
fn decide(goal: Option<&Goal>, measured: bool) -> BudgetDecision {
    let Some(goal) = goal else {
        return BudgetDecision::Continue;
    };
    let Some(budget) = goal.token_budget else {
        return BudgetDecision::Continue;
    };
    if goal.is_over_budget() {
        return BudgetDecision::stop_tokens(format!(
            "the goal's token budget of {budget} is spent: {} tokens used",
            goal.tokens_used
        ));
    }
    if !goal.usage_known || !measured {
        return BudgetDecision::stop_usage_unknown(format!(
            "the goal has a token budget of {budget} but the provider did not report usage, \
             so the {} tokens recorded so far are a floor and not a measurement",
            goal.tokens_used
        ));
    }
    let remaining = budget.saturating_sub(goal.tokens_used).max(0);
    if remaining <= budget / SOFT_RESERVE_DIVISOR {
        return BudgetDecision::Compact {
            reason: format!(
                "only {remaining} of the goal's {budget} token budget is left, which is the \
                 reserve kept for winding down; compact before spending it"
            ),
        };
    }
    BudgetDecision::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_engine::budget::{BudgetStopKind, ProviderRequestUsage};

    struct Fixture {
        store: Arc<GoalStore>,
        _spill: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let spill = tempfile::tempdir().expect("temp dir");
        let store = GoalStore::open_memory(spill.path().to_path_buf()).expect("open store");
        Fixture {
            store: Arc::new(store),
            _spill: spill,
        }
    }

    fn snapshot<'a>(
        turn_id: &'a str,
        step: u32,
        tokens: u64,
        accounted: bool,
    ) -> TurnUsageSnapshot<'a> {
        TurnUsageSnapshot {
            session_id: "ses_budget",
            turn_id,
            step,
            turn_usage: ProviderRequestUsage::default(),
            last_request: ProviderRequestUsage {
                input_tokens: tokens,
                accounted,
                ..ProviderRequestUsage::default()
            },
            estimated_prompt_tokens: 0,
            context_limit: None,
            elapsed_seconds: 0,
        }
    }

    #[tokio::test]
    async fn a_session_without_a_goal_is_left_alone() {
        let fixture = fixture();
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));
        let snapshot = snapshot("turn-1", 0, 10, true);

        assert_eq!(
            policy.before_request(&snapshot).await.expect("decide"),
            BudgetDecision::Continue
        );
        assert_eq!(
            policy.after_response(&snapshot).await.expect("decide"),
            BudgetDecision::Continue
        );
    }

    #[tokio::test]
    async fn a_goal_without_a_token_budget_is_left_alone() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "answer a question", None)
            .expect("create goal");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        let decision = policy
            .after_response(&snapshot("turn-1", 0, 10_000, true))
            .await
            .expect("decide");

        assert_eq!(decision, BudgetDecision::Continue);
    }

    #[tokio::test]
    async fn a_budget_with_room_to_spare_continues() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        let decision = policy
            .after_response(&snapshot("turn-1", 0, 100, true))
            .await
            .expect("decide");

        assert_eq!(decision, BudgetDecision::Continue);
    }

    #[tokio::test]
    async fn a_spent_budget_stops_the_turn_and_names_the_allowance() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(500))
            .expect("create goal");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        let decision = policy
            .after_response(&snapshot("turn-1", 0, 500, true))
            .await
            .expect("decide");

        let BudgetDecision::Stop(stop) = decision else {
            panic!("a spent budget stops the turn, got {decision:?}");
        };
        assert_eq!(stop.kind, BudgetStopKind::TokenBudget);
        assert!(
            stop.detail.contains("500"),
            "detail names the budget: {stop:?}"
        );
    }

    #[tokio::test]
    async fn a_response_the_provider_did_not_account_for_stops_the_turn() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        let decision = policy
            .after_response(&snapshot("turn-1", 0, 0, false))
            .await
            .expect("decide");

        let BudgetDecision::Stop(stop) = decision else {
            panic!("unmeasured usage stops the turn, got {decision:?}");
        };
        assert_eq!(stop.kind, BudgetStopKind::UsageUnknown);
    }

    #[tokio::test]
    async fn a_goal_whose_recorded_usage_is_a_floor_stops_the_next_request() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        fixture
            .store
            .record_usage("ses_budget", 10, 0, false)
            .expect("record unaccounted usage");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        let decision = policy
            .before_request(&snapshot("turn-1", 1, 0, true))
            .await
            .expect("decide");

        let BudgetDecision::Stop(stop) = decision else {
            panic!("an unmeasurable budget stops the turn, got {decision:?}");
        };
        assert_eq!(stop.kind, BudgetStopKind::UsageUnknown);
    }

    #[tokio::test]
    async fn a_budget_down_to_its_reserve_asks_for_compaction() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        let decision = policy
            .after_response(&snapshot("turn-1", 0, 950, true))
            .await
            .expect("decide");

        let BudgetDecision::Compact { reason } = decision else {
            panic!("the reserve asks for compaction, got {decision:?}");
        };
        assert!(
            reason.contains("50"),
            "the reason names what is left: {reason}"
        );
    }

    #[tokio::test]
    async fn a_replayed_turn_and_step_is_charged_once() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));
        let snapshot = snapshot("turn-1", 3, 200, true);

        policy.after_response(&snapshot).await.expect("first pass");
        policy.after_response(&snapshot).await.expect("replay");

        let goal = fixture
            .store
            .goal("ses_budget")
            .expect("read goal")
            .expect("goal exists");
        assert_eq!(goal.tokens_used, 200);
    }

    #[tokio::test]
    async fn deciding_before_a_request_charges_nothing() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        policy
            .before_request(&snapshot("turn-1", 0, 900, true))
            .await
            .expect("decide");

        let goal = fixture
            .store
            .goal("ses_budget")
            .expect("read goal")
            .expect("goal exists");
        assert_eq!(goal.tokens_used, 0);
    }
}
