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
//!
//! # Why compaction is asked for once, and only after a response
//!
//! The decision is a function of the durable row, and the engine consults it
//! before every request as well as after every response. A compaction asked for
//! before a request would come back identical once the host had compacted and
//! re-run the turn — the row it was decided from has not moved — so the turn would
//! compact forever without issuing a single request. Compaction is a response to
//! an observed cost: only the response whose charge takes the goal into its
//! reserve asks for it, exactly once, and the turn then continues on the smaller
//! transcript until the allowance is spent.
//!
//! # Why a database failure pauses the turn instead of blocking the goal
//!
//! The engine turns a policy `Err` into a hook failure, and the host records a
//! hook failure as a permanent block. A `SQLITE_BUSY` that outlasted the pool's
//! busy timeout is rare but real under concurrent sessions, and it says nothing
//! about the goal, so it must not end the goal for good. A failure to read or
//! charge the goal is therefore a stop: the turn still ends without spending
//! anything more and never continues unmeasured, but the goal pauses and can be
//! resumed once the database is readable. Only durable state this build cannot
//! read at all — a value the schema promises but cannot be decoded, a format this
//! build does not know, a status outside the closed set — stays an `Err`, because
//! no retry makes such state readable.

use crate::error::GoalError;
use crate::store::{Goal, GoalStore};
use async_trait::async_trait;
use std::sync::Arc;
use zuno_engine::budget::{BudgetDecision, TurnAllowance, TurnBudgetPolicy, TurnUsageSnapshot};
use zuno_error::DbError;

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
/// measured, compact once when a response takes the goal into its reserve,
/// otherwise continue. The host's [`TurnAllowance`] supplies the budget for a
/// goal that names none and the per-turn ceilings that stop a turn no token count
/// would.
///
/// A failure to read or charge the goal never becomes
/// [`BudgetDecision::Continue`]: a policy that cannot see the budget does not
/// know whether the turn may go on, and continuing on a database error would turn
/// every outage into an unlimited run. It becomes a stop, so the goal pauses and
/// resumes once the database is readable; only durable state this build cannot
/// read at all is returned as `Err`, which the engine treats as a turn failure.
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
    /// to stop. Never asks for compaction, because nothing has been observed that
    /// a compaction could be a response to; see `Consulted::BeforeRequest`.
    ///
    /// # Errors
    ///
    /// A message only when the stored goal is in a state this build cannot read,
    /// or the read never finished. A database that merely cannot be reached is a
    /// stop, not an error; see `store_failure`.
    async fn before_request(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, String> {
        let store = Arc::clone(&self.store);
        let session_id = snapshot.session_id.to_owned();
        let goal = match tokio::task::spawn_blocking(move || store.goal(&session_id))
            .await
            .map_err(|error| format!("reading the goal did not finish: {error}"))?
        {
            Ok(goal) => goal,
            Err(error) => return store_failure("reading the goal", error),
        };
        let decision = decide(
            goal.as_ref(),
            Consulted::BeforeRequest,
            self.allowance.default_token_budget,
        );
        Ok(self.under_ceilings(snapshot, decision))
    }

    /// Record the response, then decide from what the goal now says.
    ///
    /// # Errors
    ///
    /// A message only when the stored goal is in a state this build cannot read,
    /// the host clock is unusable, or the write never finished. A database that
    /// merely cannot be reached is a stop, not an error; see `store_failure`.
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
        let recorded = match tokio::task::spawn_blocking(move || {
            store.record_request_usage(&session_id, &request_id, tokens, at_ms)
        })
        .await
        .map_err(|error| {
            format!("recording the response against the goal did not finish: {error}")
        })? {
            Ok(recorded) => recorded,
            Err(error) => {
                return store_failure(
                    &format!("recording the response's {tokens} tokens against the goal"),
                    error,
                );
            }
        };
        let decision = decide(
            recorded.goal.as_ref(),
            Consulted::AfterResponse {
                charged: recorded.accounted.then_some(tokens),
                measured,
            },
            self.allowance.default_token_budget,
        );
        Ok(self.under_ceilings(snapshot, decision))
    }
}

/// What the policy had just observed when it was consulted.
///
/// Compaction is a response to an observed cost, so only a consultation that just
/// charged something may ask for it. A would-be compaction before a request would
/// be decided again from the very same row after the host compacted and re-ran
/// the turn, and the turn would compact forever without issuing a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Consulted {
    /// Before a request. Nothing was charged, so nothing can have crossed into the
    /// reserve and only a stop can be decided. The snapshot's last request is not
    /// a measurement of anything yet either — before the first response it is zero
    /// and unaccounted, and treating that as unmeasured usage would stop every
    /// budgeted turn on its first step — so only the goal's own `usage_known`
    /// applies here.
    BeforeRequest,
    /// After a response.
    AfterResponse {
        /// The tokens the ledger accounted for the first time on this call, or
        /// `None` when it had already seen the request: a replay did not move the
        /// row and therefore cannot have crossed anything.
        charged: Option<i64>,
        /// Whether the provider reported the response at all.
        measured: bool,
    },
}

/// Turn a failure in the policy's own path into what the engine should do.
///
/// `Err` becomes a hook failure, which the host records as a permanent block, so
/// it is reserved for state no retry can make readable: a stored value the schema
/// promises but this build cannot decode, a database in a format this build does
/// not know, or a goal row whose status, kind or reason is outside the closed set.
/// Every other database failure — the write lock held by another session, a
/// statement that failed for any other reason, I/O included, a file that would not
/// open — is the environment and not the goal, and becomes a stop instead: still
/// fail-closed, because the turn ends without spending anything more and never
/// continues unmeasured, but the goal pauses and can be resumed once the database
/// is readable, rather than being blocked for good over a `SQLITE_BUSY` that
/// happened to outlast the pool's busy timeout. That timeout is the bounded wait;
/// retrying here again would only delay the pause.
fn store_failure(doing: &str, error: GoalError) -> Result<BudgetDecision, String> {
    match error {
        GoalError::Db(DbError::Decode { .. } | DbError::SchemaMismatch { .. })
        | GoalError::UnknownStatus { .. }
        | GoalError::UnknownRetryReason { .. }
        | GoalError::UnknownPauseReason { .. }
        | GoalError::UnknownCriterionStatus { .. }
        | GoalError::UnknownGoalKind { .. } => Err(format!(
            "{doing} found durable goal state this build cannot read: {error}"
        )),
        GoalError::Db(error) => Ok(BudgetDecision::stop_usage_unknown(format!(
            "the goal's budget cannot be honoured because {doing} failed ({error}); the turn \
             stops rather than continue unmeasured, and can resume once the database is \
             readable"
        ))),
        other => Err(format!("{doing} failed: {other}")),
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

/// The whole goal decision, as a pure function of the goal, what this consultation
/// observed, and the host's default budget.
///
/// Ordered so the cheapest certainty wins. A session with no goal has nothing to
/// charge and is left alone; the host's default is not applied to it because
/// there is no durable counter to apply it against, and a default enforced from an
/// in-memory turn total would reset every turn and never bind. A goal with no
/// budget of its own runs under the host's default, and under nothing when the
/// host set none. A spent allowance stops the turn before anything else is
/// considered, because it is already spent whatever else is true. Unmeasured usage
/// stops next when the budget is the goal's own, since a budget that cannot be counted
/// cannot be honoured, and continuing on unreported numbers is how a budget silently
/// becomes advisory. Under the host's default it does not stop, for the reason given at
/// that branch.
///
/// Compaction is asked for only by the consultation whose charge took the goal
/// into its reserve. Inside the reserve both hooks answer `Continue`: a request is
/// issued and charged, and the allowance runs out through a token stop rather than
/// through a compaction that an unchanged row would ask for again on every
/// consultation.
fn decide(
    goal: Option<&Goal>,
    consulted: Consulted,
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
    let measured = match consulted {
        Consulted::BeforeRequest => true,
        Consulted::AfterResponse { measured, .. } => measured,
    };
    if !goal.usage_known || !measured {
        // Only a ceiling somebody asked for is worth stopping a run over. A user who
        // set a budget gets it honoured or gets told it cannot be: continuing on
        // unreported numbers is how a budget silently becomes advisory. The host's
        // default is not that promise. Stopping on it would end every run on a
        // provider that does not report usage — an endpoint's choice, not a runaway —
        // and it would do so on a limit the user never set, with no remedy but to set
        // one. The default still binds on whatever was counted, because a floor that
        // crosses it stops above, and a host that wants a bound no provider can
        // withhold has the tool-call and wall-time ceilings.
        if source == BudgetSource::HostDefault {
            return BudgetDecision::Continue;
        }
        return BudgetDecision::stop_usage_unknown(format!(
            "{named} cannot be honoured because the provider did not report usage, so the {} \
             tokens recorded so far are a floor and not a measurement",
            goal.tokens_used
        ));
    }
    let remaining = budget.saturating_sub(goal.tokens_used).max(0);
    let reserve = budget / SOFT_RESERVE_DIVISOR;
    if remaining <= reserve
        && let Consulted::AfterResponse {
            charged: Some(charged),
            ..
        } = consulted
        && remaining.saturating_add(charged) > reserve
    {
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

    /// Make every statement against the goal table fail, standing in for a database
    /// that cannot be read right now. Renamed rather than dropped: nothing is
    /// deleted, and the failure is exactly "the row could not be read".
    fn make_goal_table_unreadable(store: &GoalStore) {
        let connection = store.pool().get().expect("check out a connection");
        connection
            .execute_batch("ALTER TABLE goal RENAME TO goal_unreachable")
            .expect("hide the goal table");
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
    async fn an_unreported_response_under_the_host_default_keeps_going() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", None)
            .expect("create goal");

        let decision = under_default(&fixture.store, 1_000)
            .after_response(&snapshot("turn-1", 0, 0, false))
            .await
            .expect("decide");

        assert_eq!(
            decision,
            BudgetDecision::Continue,
            "a provider that reports no usage is an endpoint's choice, not a runaway, and the \
             user never asked for the default it would be stopped on"
        );
    }

    #[tokio::test]
    async fn the_host_default_still_stops_on_what_was_counted_under_a_floor() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", None)
            .expect("create goal");
        fixture
            .store
            .record_usage("ses_budget", 1_000, 0, false)
            .expect("record unaccounted usage");

        let decision = under_default(&fixture.store, 1_000)
            .before_request(&snapshot("turn-1", 1, 0, true))
            .await
            .expect("decide");

        let detail = expect_stop(decision, BudgetStopKind::TokenBudget);
        assert!(
            detail.contains("default allowance") && detail.contains("1000"),
            "an unmeasured goal is still stopped by the tokens that were counted: {detail}"
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

    #[tokio::test]
    async fn a_goal_inside_the_reserve_makes_progress_instead_of_compacting_forever() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        fixture
            .store
            .record_usage("ses_budget", 950, 0, true)
            .expect("spend the goal down into its reserve");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));
        let request = snapshot("turn-2", 1, 0, true);

        let first = policy.before_request(&request).await.expect("decide");
        let second = policy.before_request(&request).await.expect("decide");

        assert_eq!(
            (first, second),
            (BudgetDecision::Continue, BudgetDecision::Continue),
            "an unchanged row inside the reserve asked for compaction before a request; the \
             host would compact, re-run, and be told the same thing forever"
        );
        let charged = policy
            .after_response(&snapshot("turn-2", 1, 20, true))
            .await
            .expect("decide");
        assert_eq!(
            charged,
            BudgetDecision::Continue,
            "a response that stayed inside the reserve crossed nothing and compacts nothing"
        );
        expect_stop(
            policy
                .after_response(&snapshot("turn-2", 2, 30, true))
                .await
                .expect("decide"),
            BudgetStopKind::TokenBudget,
        );
    }

    #[tokio::test]
    async fn compaction_is_asked_for_once_when_a_response_crosses_into_the_reserve() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        assert_eq!(
            policy
                .after_response(&snapshot("turn-1", 0, 850, true))
                .await
                .expect("decide"),
            BudgetDecision::Continue,
            "150 left is still outside a reserve of 100"
        );
        let crossing = policy
            .after_response(&snapshot("turn-1", 1, 100, true))
            .await
            .expect("decide");
        let BudgetDecision::Compact { reason } = crossing else {
            panic!("the response that took the goal into its reserve compacts, got {crossing:?}");
        };
        assert!(reason.contains("only 50 of"), "{reason}");

        // The host compacts and re-runs the turn under a fresh turn id.
        assert_eq!(
            policy
                .before_request(&snapshot("turn-2", 1, 0, true))
                .await
                .expect("decide"),
            BudgetDecision::Continue,
            "the compacted turn must be allowed to issue a request"
        );
        assert_eq!(
            policy
                .after_response(&snapshot("turn-2", 1, 30, true))
                .await
                .expect("decide"),
            BudgetDecision::Continue,
            "the reserve is spent on requests, not on a second compaction"
        );
        expect_stop(
            policy
                .after_response(&snapshot("turn-2", 2, 20, true))
                .await
                .expect("decide"),
            BudgetStopKind::TokenBudget,
        );
    }

    #[tokio::test]
    async fn a_replayed_crossing_response_does_not_ask_for_compaction_again() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));
        let crossing = snapshot("turn-1", 0, 950, true);

        let first = policy.after_response(&crossing).await.expect("first pass");
        let replay = policy.after_response(&crossing).await.expect("replay");

        assert!(
            matches!(first, BudgetDecision::Compact { .. }),
            "the first pass crossed into the reserve: {first:?}"
        );
        assert_eq!(
            replay,
            BudgetDecision::Continue,
            "the ledger had already seen turn-1:0, so the row did not move and nothing crossed"
        );
        let goal = fixture
            .store
            .goal("ses_budget")
            .expect("read goal")
            .expect("goal exists");
        assert_eq!(goal.tokens_used, 950);
    }

    #[tokio::test]
    async fn a_database_failure_pauses_the_turn_instead_of_blocking_the_goal() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        make_goal_table_unreadable(&fixture.store);
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        let before = policy
            .before_request(&snapshot("turn-1", 0, 0, true))
            .await
            .expect("an unreadable database is a decision the goal can recover from, not a hook failure");
        let detail = expect_stop(before, BudgetStopKind::UsageUnknown);
        assert!(
            detail.contains("reading the goal failed"),
            "the stop names what failed: {detail}"
        );

        let after = policy
            .after_response(&snapshot("turn-1", 0, 200, true))
            .await
            .expect("an unrecordable response is a decision the goal can recover from, not a hook failure");
        let detail = expect_stop(after, BudgetStopKind::UsageUnknown);
        assert!(
            detail.contains("200 tokens"),
            "the stop names the spend that went unrecorded: {detail}"
        );
    }

    #[tokio::test]
    async fn a_goal_status_this_build_cannot_read_still_fails_the_hook() {
        let fixture = fixture();
        fixture
            .store
            .create_goal("ses_budget", "land the port", Some(1_000))
            .expect("create goal");
        {
            // The revision moves with the status, as it does for every real write, so the
            // history trigger records a new row instead of colliding with the last one.
            let connection = fixture.store.pool().get().expect("check out a connection");
            connection
                .execute_batch(
                    "PRAGMA ignore_check_constraints = ON; \
                     UPDATE goal SET status = 'from_the_future', revision = revision + 1 \
                     WHERE session_id = 'ses_budget'; \
                     PRAGMA ignore_check_constraints = OFF;",
                )
                .expect("write a status outside the closed set");
        }
        let policy = GoalBudgetPolicy::new(Arc::clone(&fixture.store));

        let error = policy
            .before_request(&snapshot("turn-1", 0, 0, true))
            .await
            .expect_err("a status no retry can make readable is not something a resume can fix");

        assert!(
            error.contains("from_the_future"),
            "the failure names the unreadable value: {error}"
        );
    }
}
