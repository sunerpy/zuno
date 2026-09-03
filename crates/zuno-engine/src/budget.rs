//! Per-request budget accounting for a running turn.
//!
//! Usage reconciled only after a turn ends cannot stop that turn. A forty-step
//! turn can spend a whole session's token budget before anything reads a counter,
//! so a budget that is only checked at turn boundaries is not a budget.
//!
//! A [`TurnBudgetPolicy`] is consulted around every provider request while the
//! turn is still running, and answers with a [`BudgetDecision`] the loop must
//! honour: keep going, compact first, or stop. The engine owns when the policy is
//! asked; the policy owns the limits and where they are stored.

use async_trait::async_trait;
use std::num::NonZeroU32;
use std::time::Duration;
use zuno_error::DbError;

/// Token counts attributable to provider requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderRequestUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    /// Whether the provider reported counts at all.
    ///
    /// `false` means the numbers are floors, not measurements: a policy must not
    /// treat an unreported request as a free one.
    pub accounted: bool,
}

impl ProviderRequestUsage {
    /// Every token this usage accounts for.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_write_input_tokens)
    }

    /// Accumulate another request's usage, keeping `accounted` pessimistic.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_add(other.cache_write_input_tokens),
            accounted: self.accounted && other.accounted,
        }
    }
}

/// What a turn has consumed at the moment the policy is consulted.
#[derive(Debug, Clone)]
pub struct TurnUsageSnapshot<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    /// The assistant step about to run, or that just finished.
    pub step: u32,
    /// Everything accumulated since this turn started.
    pub turn_usage: ProviderRequestUsage,
    /// The request that just completed; zero before the first response.
    pub last_request: ProviderRequestUsage,
    /// Prompt size estimated for the request this snapshot is about.
    ///
    /// The request about to be sent in [`TurnBudgetPolicy::before_request`], and the
    /// one just answered in [`TurnBudgetPolicy::after_response`].
    pub estimated_prompt_tokens: u64,
    /// The model's context window, when the catalog reports one.
    pub context_limit: Option<u64>,
    /// Wall-clock seconds since the turn started.
    pub elapsed_seconds: u64,
    /// Tool calls this turn has dispatched so far.
    ///
    /// Counts calls the loop actually ran, not calls the model issued: a call left
    /// behind by a stop or skipped for an urgent input did no work, and a ceiling
    /// on this number is meant to bound work done. It is a dimension of its own
    /// rather than something inferred from `step` because a turn can loop cheaply
    /// and endlessly on tool calls without ever nearing a token ceiling.
    pub tool_calls_dispatched: u32,
}

/// Why a turn must stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetStopKind {
    /// The token allowance is spent.
    TokenBudget,
    /// The time allowance is spent.
    TimeBudget,
    /// The tool-call allowance is spent.
    ToolCallBudget,
    /// Usage cannot be measured, so the allowance cannot be honoured.
    UsageUnknown,
}

impl BudgetStopKind {
    /// A stable machine-readable code for events and host mapping.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::TokenBudget => "token_budget",
            Self::TimeBudget => "time_budget",
            Self::ToolCallBudget => "tool_call_budget",
            Self::UsageUnknown => "usage_unknown",
        }
    }
}

/// A budget stop, with the reason a human needs to decide what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetStop {
    pub kind: BudgetStopKind,
    pub detail: String,
}

/// What the loop must do with the request it was about to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetDecision {
    /// Proceed unchanged.
    Continue,
    /// Compact the transcript before continuing, and say why in the turn's events.
    Compact { reason: String },
    /// Stop the turn without issuing another request.
    Stop(BudgetStop),
}

impl BudgetDecision {
    /// Stop the turn because the token allowance is spent.
    #[must_use]
    pub fn stop_tokens(detail: impl Into<String>) -> Self {
        Self::Stop(BudgetStop {
            kind: BudgetStopKind::TokenBudget,
            detail: detail.into(),
        })
    }

    /// Stop the turn because the time allowance is spent.
    #[must_use]
    pub fn stop_time(detail: impl Into<String>) -> Self {
        Self::Stop(BudgetStop {
            kind: BudgetStopKind::TimeBudget,
            detail: detail.into(),
        })
    }

    /// Stop the turn because usage cannot be measured.
    #[must_use]
    pub fn stop_usage_unknown(detail: impl Into<String>) -> Self {
        Self::Stop(BudgetStop {
            kind: BudgetStopKind::UsageUnknown,
            detail: detail.into(),
        })
    }

    /// Stop the turn because the tool-call allowance is spent.
    #[must_use]
    pub fn stop_tool_calls(detail: impl Into<String>) -> Self {
        Self::Stop(BudgetStop {
            kind: BudgetStopKind::ToolCallBudget,
            detail: detail.into(),
        })
    }
}

/// Why a [`TurnBudgetPolicy`] could not answer.
///
/// Typed so the loop decides recovery from the variant and never from a rendered
/// message. A policy that reads its allowance from durable storage reports the
/// store's own failure, and the loop classifies it exactly as it classifies any
/// other [`DbError`]: a writer that merely holds the lock is a retry with a
/// persisted backoff, everything else is permanent. Anything that is not a storage
/// failure is [`Self::Permanent`], and repeating the consultation will not change it.
#[derive(Debug, thiserror::Error)]
pub enum BudgetPolicyError {
    /// The store behind the allowance failed while it was read or charged.
    #[error(transparent)]
    Database(#[from] DbError),
    /// The policy cannot decide, and no retry can make it able to.
    #[error("{0}")]
    Permanent(String),
}

/// The limits a turn runs under, consulted while the turn is still running.
///
/// Both hooks default to [`BudgetDecision::Continue`], so a host that installs no
/// policy keeps the previous behaviour exactly.
#[async_trait]
pub trait TurnBudgetPolicy: Send + Sync {
    /// Called before each provider request, after the prompt is estimated.
    ///
    /// # Errors
    ///
    /// [`BudgetPolicyError::Permanent`] when the policy cannot decide, which the loop
    /// treats as a turn failure rather than as permission to continue, and
    /// [`BudgetPolicyError::Database`] when the store behind the allowance failed, which
    /// the loop classifies as it classifies any other database failure.
    async fn before_request(
        &self,
        _snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        Ok(BudgetDecision::Continue)
    }

    /// Called after each provider response is accounted for.
    ///
    /// # Errors
    ///
    /// As for [`Self::before_request`], when the policy cannot record or evaluate the
    /// response.
    async fn after_response(
        &self,
        _snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        Ok(BudgetDecision::Continue)
    }
}

/// A policy that imposes no limits.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopBudgetPolicy;

impl TurnBudgetPolicy for NoopBudgetPolicy {}

/// What a host allows a turn when nothing more specific was set.
///
/// Carried by the harness profile and handed to a [`TurnBudgetPolicy`] when it is
/// built, so the ceiling a run stops on is a decision the host made once and wrote
/// down, not a constant buried in whichever store the policy reads. Every field is
/// an `Option` so that "no ceiling of this kind" is a visible choice in the type: a
/// zero here would have to mean either unlimited, which is the runaway this exists
/// to prevent, or a turn that may do nothing, and a reader could not tell which.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnAllowance {
    /// The token budget a goal runs under when nobody put a number on it.
    ///
    /// An explicit goal budget always wins over this. `None` leaves such a goal
    /// without a token ceiling, which is the historical behaviour and the choice a
    /// host makes when it genuinely wants unlimited autonomy. It is not a turn
    /// ceiling: the policy charges it against the durable goal, where an allowance
    /// can outlive the turn and actually bind.
    pub default_token_budget: Option<u64>,
    /// The most tool calls one turn may dispatch.
    ///
    /// Non-zero because a turn allowed no tool calls would stop on its first
    /// request before the model could answer; `None` is how a host says there is
    /// no ceiling.
    pub max_tool_calls: Option<NonZeroU32>,
    /// The longest one turn may run, measured from the turn's start.
    ///
    /// Honoured at the snapshot's one-second resolution and only where the policy
    /// is consulted: the ceiling is never reported reached before the turn has
    /// actually run this long, and a provider request already in flight when the
    /// clock passes it completes before the turn stops.
    pub max_duration: Option<Duration>,
}

impl TurnAllowance {
    /// No ceilings of any kind: what a turn ran under before allowances existed.
    pub const UNLIMITED: Self = Self {
        default_token_budget: None,
        max_tool_calls: None,
        max_duration: None,
    };

    /// The turn ceiling this snapshot has reached, if any.
    ///
    /// A pure function of the allowance and the snapshot, so a policy can call it
    /// from either hook and a test can exercise it without a loop. The tool-call
    /// ceiling is checked first because its count is exact, whereas elapsed time is
    /// rounded down to whole seconds; when both are reached the stop names the
    /// measurement that cannot be argued with. The token default is deliberately
    /// not consulted here: it belongs to the goal, not the turn.
    #[must_use]
    pub fn ceiling_reached(&self, snapshot: &TurnUsageSnapshot<'_>) -> Option<BudgetStop> {
        if let Some(max) = self.max_tool_calls
            && snapshot.tool_calls_dispatched >= max.get()
        {
            return Some(BudgetStop {
                kind: BudgetStopKind::ToolCallBudget,
                detail: format!(
                    "the turn's tool-call ceiling of {max} is reached: {} tool calls dispatched",
                    snapshot.tool_calls_dispatched
                ),
            });
        }
        if let Some(max) = self.max_duration
            && Duration::from_secs(snapshot.elapsed_seconds) >= max
        {
            return Some(BudgetStop {
                kind: BudgetStopKind::TimeBudget,
                detail: format!(
                    "the turn's wall-time ceiling of {} seconds is reached: {} seconds elapsed",
                    max.as_secs_f64(),
                    snapshot.elapsed_seconds
                ),
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, accounted: bool) -> ProviderRequestUsage {
        ProviderRequestUsage {
            input_tokens: input,
            output_tokens: output,
            accounted,
            ..ProviderRequestUsage::default()
        }
    }

    #[test]
    fn accumulated_usage_stays_unaccounted_once_any_request_is_unaccounted() {
        let combined = usage(10, 5, true).saturating_add(usage(3, 2, false));
        assert_eq!(combined.total(), 20);
        assert!(!combined.accounted);
    }

    #[test]
    fn totals_saturate_instead_of_wrapping() {
        let huge = ProviderRequestUsage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            cache_read_input_tokens: u64::MAX,
            cache_write_input_tokens: u64::MAX,
            accounted: true,
        };
        assert_eq!(huge.total(), u64::MAX);
        assert_eq!(huge.saturating_add(huge).input_tokens, u64::MAX);
    }

    #[tokio::test]
    async fn the_default_policy_never_interferes() {
        let policy = NoopBudgetPolicy;
        let snapshot = TurnUsageSnapshot {
            session_id: "session-1",
            turn_id: "turn-1",
            step: 3,
            turn_usage: usage(100, 50, true),
            last_request: usage(10, 5, true),
            estimated_prompt_tokens: 1_000,
            context_limit: Some(200_000),
            elapsed_seconds: 12,
            tool_calls_dispatched: 2,
        };

        assert_eq!(
            policy.before_request(&snapshot).await.expect("decide"),
            BudgetDecision::Continue
        );
        assert_eq!(
            policy.after_response(&snapshot).await.expect("decide"),
            BudgetDecision::Continue
        );
    }

    #[test]
    fn stop_constructors_carry_a_stable_code_and_the_human_detail() {
        let BudgetDecision::Stop(stop) = BudgetDecision::stop_tokens("budget spent") else {
            panic!("expected a stop");
        };
        assert_eq!(stop.kind.code(), "token_budget");
        assert_eq!(stop.detail, "budget spent");
        assert_eq!(
            match BudgetDecision::stop_time("out of time") {
                BudgetDecision::Stop(stop) => stop.kind.code(),
                _ => panic!("expected a stop"),
            },
            "time_budget"
        );
        assert_eq!(
            match BudgetDecision::stop_usage_unknown("provider reported nothing") {
                BudgetDecision::Stop(stop) => stop.kind.code(),
                _ => panic!("expected a stop"),
            },
            "usage_unknown"
        );
        assert_eq!(
            match BudgetDecision::stop_tool_calls("too many tool calls") {
                BudgetDecision::Stop(stop) => stop.kind.code(),
                _ => panic!("expected a stop"),
            },
            "tool_call_budget"
        );
    }

    fn snapshot(tool_calls_dispatched: u32, elapsed_seconds: u64) -> TurnUsageSnapshot<'static> {
        TurnUsageSnapshot {
            session_id: "session-1",
            turn_id: "turn-1",
            step: 4,
            turn_usage: usage(100, 50, true),
            last_request: usage(10, 5, true),
            estimated_prompt_tokens: 1_000,
            context_limit: Some(200_000),
            elapsed_seconds,
            tool_calls_dispatched,
        }
    }

    #[test]
    fn an_unlimited_allowance_reaches_no_ceiling_however_long_a_turn_runs() {
        assert_eq!(TurnAllowance::default(), TurnAllowance::UNLIMITED);
        assert_eq!(
            TurnAllowance::UNLIMITED.ceiling_reached(&snapshot(u32::MAX, u64::MAX)),
            None
        );
    }

    #[test]
    fn a_tool_call_ceiling_is_reached_by_the_count_and_names_both_numbers() {
        let allowance = TurnAllowance {
            max_tool_calls: NonZeroU32::new(3),
            ..TurnAllowance::UNLIMITED
        };

        assert_eq!(allowance.ceiling_reached(&snapshot(2, 0)), None);
        let stop = allowance
            .ceiling_reached(&snapshot(3, 0))
            .expect("the third call reaches the ceiling");
        assert_eq!(stop.kind, BudgetStopKind::ToolCallBudget);
        assert_eq!(
            stop.detail,
            "the turn's tool-call ceiling of 3 is reached: 3 tool calls dispatched"
        );
        let over = allowance
            .ceiling_reached(&snapshot(5, 0))
            .expect("past the ceiling is still reached");
        assert!(
            over.detail.contains("5 tool calls dispatched"),
            "the stop must report the observed count, not the ceiling: {over:?}"
        );
    }

    #[test]
    fn a_wall_time_ceiling_is_never_reached_before_the_turn_has_run_that_long() {
        let allowance = TurnAllowance {
            max_duration: Some(Duration::from_millis(1_500)),
            ..TurnAllowance::UNLIMITED
        };

        // A reading of one second means the turn has run at least one second and
        // less than two, which may or may not be past 1.5; a reading of two certainly
        // is. The ceiling is reported only once it is certain.
        assert_eq!(allowance.ceiling_reached(&snapshot(0, 1)), None);
        let stop = allowance
            .ceiling_reached(&snapshot(0, 2))
            .expect("two whole seconds exceed the ceiling");
        assert_eq!(stop.kind, BudgetStopKind::TimeBudget);
        assert_eq!(
            stop.detail,
            "the turn's wall-time ceiling of 1.5 seconds is reached: 2 seconds elapsed"
        );
    }

    #[test]
    fn the_exact_tool_call_count_is_reported_ahead_of_the_rounded_clock() {
        let allowance = TurnAllowance {
            default_token_budget: Some(1),
            max_tool_calls: NonZeroU32::new(1),
            max_duration: Some(Duration::from_secs(10)),
        };

        let stop = allowance
            .ceiling_reached(&snapshot(1, 10))
            .expect("both ceilings are reached");

        assert_eq!(stop.kind, BudgetStopKind::ToolCallBudget);
    }

    #[test]
    fn the_token_default_is_a_goal_matter_and_not_a_turn_ceiling() {
        let allowance = TurnAllowance {
            default_token_budget: Some(1),
            ..TurnAllowance::UNLIMITED
        };

        // The snapshot has spent far more than one token; the ceiling check does not
        // care, because charging the default against a turn total would reset every
        // turn and never bind.
        assert_eq!(allowance.ceiling_reached(&snapshot(0, 0)), None);
    }
}
