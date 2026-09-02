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
//!
//! # What the host's allowance adds
//!
//! A goal nobody put a number on used to mean unlimited: the policy left it alone,
//! and the only thing that stopped an autonomous run was a human noticing. The
//! host now hands the policy a [`TurnAllowance`], and its `default_token_budget`
//! stands in for a goal's missing `token_budget` — charged against the same
//! durable counters, with the same reserve — so `None` on the goal means "the
//! host's default" and not "infinite". An explicit goal budget always wins, and a
//! host that genuinely wants unlimited says so with [`TurnAllowance::UNLIMITED`].
//! The allowance's tool-call and wall-time ceilings bound the turn itself, goal or
//! no goal, because a turn that loops cheaply on tool calls is a runaway no token
//! ceiling would ever notice.

use crate::store::{Goal, GoalStore};
use async_trait::async_trait;
use std::sync::Arc;
use zuno_engine::budget::{BudgetDecision, TurnAllowance, TurnBudgetPolicy, TurnUsageSnapshot};

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
/// The host's [`TurnAllowance`] supplies the budget for a goal that names none
/// and the per-turn ceilings that stop a turn no token count would.
///
/// A store failure is returned as `Err`, which the engine treats as a turn
/// failure. That is deliberate: a policy that cannot read the budget does not
/// know whether the turn may continue, and answering
/// [`BudgetDecision::Continue`] on a database error would turn every outage into
/// an unlimited run.
#[derive(Debug, Clone)]
pub struct GoalBudgetPolicy {
    store: Arc<GoalStore>,
    allowance: TurnAllowance,
}

impl GoalBudgetPolicy {
    /// Bind the policy to a shared goal store, under no host allowance.
    ///
    /// This is [`TurnAllowance::UNLIMITED`]: a goal without a token budget runs
    /// without a ceiling and no turn ceiling applies, exactly as before allowances
    /// existed. A host that wants a default hands one over with
    /// [`GoalBudgetPolicy::with_allowance`].
    #[must_use]
    pub fn new(store: Arc<GoalStore>) -> Self {
        Self {
            store,
            allowance: TurnAllowance::UNLIMITED,
        }
    }

    /// Run under the host's allowance.
    ///
    /// The allowance is a value, not a lookup: what a turn was held to must not
    /// change under it because a profile was re-activated halfway through.
    #[must_use]
    pub fn with_allowance(mut self, allowance: TurnAllowance) -> Self {
        self.allowance = allowance;
        self
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

    /// Let a reached turn ceiling override anything but a stop.
    ///
    /// A stop the goal already produced stands: it is the durable fact a human will
    /// find in the goal afterwards, and both outcomes are stops. A ceiling beats a
    /// compaction request or a continue, because either would spend a provider
    /// request the ceiling no longer allows. The ceilings apply to every turn this
    /// policy governs, goal or not — they bound the turn, and a turn without a goal
    /// can loop just as well as one with.
    fn under_ceilings(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
        goal_decision: BudgetDecision,
    ) -> BudgetDecision {
        match goal_decision {
            stop @ BudgetDecision::Stop(_) => stop,
            other => self
                .allowance
                .ceiling_reached(snapshot)
                .map_or(other, BudgetDecision::Stop),
        }
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
        let decision = decide(goal.as_ref(), true, self.allowance.default_token_budget);
        Ok(self.under_ceilings(snapshot, decision))
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
        let decision = decide(
            recorded.goal.as_ref(),
            measured,
            self.allowance.default_token_budget,
        );
        Ok(self.under_ceilings(snapshot, decision))
    }
}

/// Which number a goal is being held to, because the remedy differs.
///
/// A user who set a budget and spent it raises it; a user who never set one is
/// told to set one. The stop kind is the same — the turn stopped on its token
/// allowance either way — but a message that could not tell the two apart would
/// send one of them to the wrong fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetSource {
    /// The goal's own `token_budget`.
    Goal,
    /// The host's `default_token_budget`, applied because the goal has none.
    HostDefault,
}

impl BudgetSource {
    /// The budget as a noun phrase that fits every message it appears in.
    fn describe(self, budget: i64) -> String {
        match self {
            Self::Goal => format!("the goal's token budget of {budget}"),
            Self::HostDefault => format!(
                "the host's default allowance of {budget} tokens (applied because the goal has \
                 no token budget of its own)"
            ),
        }
    }

    /// What a human does about a spent budget of this kind.
    const fn remedy(self) -> &'static str {
        match self {
            Self::Goal => "raise the goal's token budget to continue",
            Self::HostDefault => "set a token budget on the goal to continue",
        }
    }
}

/// The token budget a goal is held to, and where it came from.
///
/// An explicit goal budget always wins: a user who set a number gets that number,
/// whatever the host's default says. The default saturates into the goal's `i64`
/// counter space because a host default past `i64::MAX` is not a budget anyone
/// will reach, and wrapping it would turn "practically unlimited" into a stop.
fn effective_budget(goal: &Goal, default_token_budget: Option<u64>) -> Option<(i64, BudgetSource)> {
    if let Some(budget) = goal.token_budget {
        return Some((budget, BudgetSource::Goal));
    }
    default_token_budget.map(|budget| {
        (
            i64::try_from(budget).unwrap_or(i64::MAX),
            BudgetSource::HostDefault,
        )
    })
}

/// The whole goal decision, as a pure function of the goal, whether the last
/// response was measured, and the host's default budget.
///
/// Ordered so the cheapest certainty wins. A session with no goal has nothing to
/// charge and is left alone; the host's default is not applied to it because
/// there is no durable counter to apply it against, and a default enforced from an
/// in-memory turn total would reset every turn and never bind. A goal with no
/// budget of its own runs under the host's default, and under nothing when the
/// host set none. A spent allowance stops the turn before anything else is
/// considered, because it is already spent whatever else is true. Unmeasured
/// usage stops next, since a budget that cannot be counted cannot be honoured, and
/// continuing on unreported numbers is how a budget silently becomes advisory.
fn decide(
    goal: Option<&Goal>,
    measured: bool,
    default_token_budget: Option<u64>,
) -> BudgetDecision {
    let Some(goal) = goal else {
        return BudgetDecision::Continue;
    };
    let Some((budget, source)) = effective_budget(goal, default_token_budget) else {
        return BudgetDecision::Continue;
    };
    let named = source.describe(budget);
    if goal.tokens_used >= budget {
        return BudgetDecision::stop_tokens(format!(
            "{named} is spent: {} tokens used; {}",
            goal.tokens_used,
            source.remedy()
        ));
    }
    if !goal.usage_known || !measured {
        return BudgetDecision::stop_usage_unknown(format!(
            "{named} cannot be honoured because the provider did not report usage, so the {} \
             tokens recorded so far are a floor and not a measurement",
            goal.tokens_used
        ));
    }
    let remaining = budget.saturating_sub(goal.tokens_used).max(0);
    if remaining <= budget / SOFT_RESERVE_DIVISOR {
        return BudgetDecision::Compact {
            reason: format!(
                "only {remaining} of {named} is left, which is the reserve kept for winding \
                 down; compact before spending it"
            ),
        };
    }
    BudgetDecision::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use std::time::Duration;
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
            tool_calls_dispatched: 0,
        }
    }

    /// A policy under a host default and no turn ceilings.
    fn under_default(store: &Arc<GoalStore>, default_token_budget: u64) -> GoalBudgetPolicy {
        GoalBudgetPolicy::new(Arc::clone(store)).with_allowance(TurnAllowance {
            default_token_budget: Some(default_token_budget),
            ..TurnAllowance::UNLIMITED
        })
    }

    /// A policy under turn ceilings and no host default.
    fn under_turn_ceilings(
        store: &Arc<GoalStore>,
        max_tool_calls: Option<u32>,
        max_duration: Option<Duration>,
    ) -> GoalBudgetPolicy {
        GoalBudgetPolicy::new(Arc::clone(store)).with_allowance(TurnAllowance {
            default_token_budget: None,
            max_tool_calls: max_tool_calls.and_then(NonZeroU32::new),
            max_duration,
        })
    }

    /// The detail of a stop of the expected kind, or a panic naming what came back.
    fn expect_stop(decision: BudgetDecision, kind: BudgetStopKind) -> String {
        let BudgetDecision::Stop(stop) = decision else {
            panic!("expected a {kind:?} stop, got {decision:?}");
        };
        assert_eq!(stop.kind, kind, "wrong stop kind: {stop:?}");
        stop.detail
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
    async fn a_goal_without_a_token_budget_is_left_alone_when_the_host_sets_no_default() {
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

    #[tokio::test]
    async fn a_goal_without_a_token_budget_runs_under_the_host_default() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "answer a question", None)
            .expect("create goal");
        let policy = under_default(&fixture.store, 500);

        let roomy = policy
            .after_response(&snapshot("turn-1", 0, 100, true))
            .await
            .expect("decide");
        assert_eq!(roomy, BudgetDecision::Continue);

        let decision = policy
            .after_response(&snapshot("turn-1", 1, 400, true))
            .await
            .expect("decide");
        let detail = expect_stop(decision, BudgetStopKind::TokenBudget);
        assert!(
            detail.contains("default allowance of 500"),
            "the stop names the default: {detail}"
        );
        assert!(
            detail.contains("500 tokens used"),
            "the stop names the spend: {detail}"
        );
    }

    #[tokio::test]
    async fn a_default_imposed_stop_tells_the_user_to_set_a_budget_not_raise_one() {
        let defaulted = fixture();
        defaulted
            .store
            .create_goal("ses_budget", "land the port", None)
            .expect("create goal");
        let by_default = expect_stop(
            under_default(&defaulted.store, 500)
                .after_response(&snapshot("turn-1", 0, 500, true))
                .await
                .expect("decide"),
            BudgetStopKind::TokenBudget,
        );

        let explicit = fixture();
        explicit
            .store
            .create_goal("ses_budget", "land the port", Some(500))
            .expect("create goal");
        let by_goal = expect_stop(
            GoalBudgetPolicy::new(Arc::clone(&explicit.store))
                .after_response(&snapshot("turn-1", 0, 500, true))
                .await
                .expect("decide"),
            BudgetStopKind::TokenBudget,
        );

        assert_ne!(by_default, by_goal);
        assert!(
            by_default.contains("set a token budget"),
            "a default-imposed stop asks for a budget to be set: {by_default}"
        );
        assert!(
            !by_default.contains("the goal's token budget"),
            "a default-imposed stop must not claim the goal had a budget: {by_default}"
        );
        assert!(
            by_goal.contains("raise the goal's token budget"),
            "a spent goal budget asks to be raised: {by_goal}"
        );
        assert!(
            !by_goal.contains("default"),
            "a spent goal budget must not mention a default: {by_goal}"
        );
    }

    #[tokio::test]
    async fn an_explicit_goal_budget_wins_over_the_host_default_in_both_directions() {
        let generous_goal = fixture();
        generous_goal
            .store
            .create_goal("ses_budget", "land the port", Some(10_000))
            .expect("create goal");
        let decision = under_default(&generous_goal.store, 100)
            .after_response(&snapshot("turn-1", 0, 500, true))
            .await
            .expect("decide");
        assert_eq!(
            decision,
            BudgetDecision::Continue,
            "a smaller host default narrowed an explicit budget"
        );

        let tight_goal = fixture();
        tight_goal
            .store
            .create_goal("ses_budget", "land the port", Some(100))
            .expect("create goal");
        let decision = under_default(&tight_goal.store, 10_000)
            .after_response(&snapshot("turn-1", 0, 100, true))
            .await
            .expect("decide");
        let detail = expect_stop(decision, BudgetStopKind::TokenBudget);
        assert!(
            detail.contains("the goal's token budget of 100"),
            "a larger host default widened an explicit budget: {detail}"
        );
    }

    #[tokio::test]
    async fn the_host_default_leaves_a_session_without_a_goal_alone() {
        let fixture = fixture();
        let policy = under_default(&fixture.store, 10);
        let snapshot = snapshot("turn-1", 0, 1_000, true);

        assert_eq!(
            policy.before_request(&snapshot).await.expect("decide"),
            BudgetDecision::Continue
        );
        assert_eq!(
            policy.after_response(&snapshot).await.expect("decide"),
            BudgetDecision::Continue,
            "there is no durable counter to hold a goalless session to"
        );
    }

    #[tokio::test]
    async fn the_host_default_is_charged_and_keyed_exactly_as_an_explicit_budget() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", None)
            .expect("create goal");
        let policy = under_default(&fixture.store, 1_000);
        let snapshot = snapshot("turn-1", 3, 200, true);

        policy.after_response(&snapshot).await.expect("first pass");
        policy.after_response(&snapshot).await.expect("replay");

        let goal = fixture
            .store
            .goal("ses_budget")
            .expect("read goal")
            .expect("goal exists");
        assert_eq!(
            goal.tokens_used, 200,
            "a replayed request was charged twice under the default"
        );
        assert_eq!(
            goal.token_budget, None,
            "the default is the host's to change and must not be written into the goal"
        );
    }

    #[tokio::test]
    async fn a_host_default_down_to_its_reserve_asks_for_compaction() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", None)
            .expect("create goal");

        let decision = under_default(&fixture.store, 1_000)
            .after_response(&snapshot("turn-1", 0, 950, true))
            .await
            .expect("decide");

        let BudgetDecision::Compact { reason } = decision else {
            panic!("the reserve asks for compaction under a default too, got {decision:?}");
        };
        assert!(
            reason.contains("only 50 of") && reason.contains("default allowance"),
            "the reason names what is left and whose number it is: {reason}"
        );
    }

    #[tokio::test]
    async fn an_unreported_response_under_the_host_default_stops_the_turn() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", None)
            .expect("create goal");

        let decision = under_default(&fixture.store, 1_000)
            .after_response(&snapshot("turn-1", 0, 0, false))
            .await
            .expect("decide");

        let detail = expect_stop(decision, BudgetStopKind::UsageUnknown);
        assert!(
            detail.contains("default allowance"),
            "the stop names the allowance that could not be honoured: {detail}"
        );
    }

    #[tokio::test]
    async fn a_tool_call_ceiling_stops_the_next_request_and_names_the_count() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000_000))
            .expect("create goal");
        let policy = under_turn_ceilings(&fixture.store, Some(3), None);
        let mut snapshot = snapshot("turn-1", 2, 0, true);

        snapshot.tool_calls_dispatched = 2;
        assert_eq!(
            policy.before_request(&snapshot).await.expect("decide"),
            BudgetDecision::Continue
        );

        snapshot.tool_calls_dispatched = 3;
        let detail = expect_stop(
            policy.before_request(&snapshot).await.expect("decide"),
            BudgetStopKind::ToolCallBudget,
        );
        assert!(
            detail.contains("ceiling of 3") && detail.contains("3 tool calls dispatched"),
            "the stop names the ceiling and the observed count: {detail}"
        );
    }

    #[tokio::test]
    async fn a_wall_time_ceiling_stops_the_turn_and_names_the_elapsed_time() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000_000))
            .expect("create goal");
        let policy = under_turn_ceilings(&fixture.store, None, Some(Duration::from_secs(60)));
        let mut snapshot = snapshot("turn-1", 0, 10, true);

        snapshot.elapsed_seconds = 59;
        assert_eq!(
            policy.after_response(&snapshot).await.expect("decide"),
            BudgetDecision::Continue
        );

        snapshot.step = 1;
        snapshot.elapsed_seconds = 61;
        let detail = expect_stop(
            policy.after_response(&snapshot).await.expect("decide"),
            BudgetStopKind::TimeBudget,
        );
        assert!(
            detail.contains("60 seconds") && detail.contains("61 seconds elapsed"),
            "the stop names the ceiling and the observed time: {detail}"
        );
    }

    #[tokio::test]
    async fn a_turn_ceiling_applies_even_when_the_session_has_no_goal() {
        let fixture = fixture();
        let policy = under_turn_ceilings(&fixture.store, Some(1), None);
        let mut snapshot = snapshot("turn-1", 1, 0, true);
        snapshot.tool_calls_dispatched = 1;

        expect_stop(
            policy.before_request(&snapshot).await.expect("decide"),
            BudgetStopKind::ToolCallBudget,
        );
    }

    #[tokio::test]
    async fn a_ceiling_stop_still_charges_the_response_that_reached_it() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        let policy = under_turn_ceilings(&fixture.store, None, Some(Duration::from_secs(1)));
        let mut snapshot = snapshot("turn-1", 0, 200, true);
        snapshot.elapsed_seconds = 5;

        expect_stop(
            policy.after_response(&snapshot).await.expect("decide"),
            BudgetStopKind::TimeBudget,
        );

        let goal = fixture
            .store
            .goal("ses_budget")
            .expect("read goal")
            .expect("goal exists");
        assert_eq!(
            goal.tokens_used, 200,
            "the response that reached the ceiling was still paid for"
        );
    }

    #[tokio::test]
    async fn a_spent_goal_budget_is_reported_ahead_of_a_turn_ceiling() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(100))
            .expect("create goal");
        let policy = under_turn_ceilings(&fixture.store, Some(1), None);
        let mut snapshot = snapshot("turn-1", 0, 100, true);
        snapshot.tool_calls_dispatched = 1;

        expect_stop(
            policy.after_response(&snapshot).await.expect("decide"),
            BudgetStopKind::TokenBudget,
        );
    }

    #[tokio::test]
    async fn a_turn_ceiling_is_reported_ahead_of_a_compaction_request() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        let policy = under_turn_ceilings(&fixture.store, Some(1), None);
        let mut snapshot = snapshot("turn-1", 0, 950, true);
        snapshot.tool_calls_dispatched = 1;

        expect_stop(
            policy.after_response(&snapshot).await.expect("decide"),
            BudgetStopKind::ToolCallBudget,
        );
    }
}
