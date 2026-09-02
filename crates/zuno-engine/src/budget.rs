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
    /// Prompt size estimated for the next request, when one is assembled.
    pub estimated_prompt_tokens: u64,
    /// The model's context window, when the catalog reports one.
    pub context_limit: Option<u64>,
    /// Wall-clock seconds since the turn started.
    pub elapsed_seconds: u64,
}

/// Why a turn must stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetStopKind {
    /// The token allowance is spent.
    TokenBudget,
    /// The time allowance is spent.
    TimeBudget,
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
    /// A message when the policy cannot decide, which the loop treats as a turn
    /// failure rather than as permission to continue.
    async fn before_request(
        &self,
        _snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, String> {
        Ok(BudgetDecision::Continue)
    }

    /// Called after each provider response is accounted for.
    ///
    /// # Errors
    ///
    /// A message when the policy cannot record or evaluate the response.
    async fn after_response(
        &self,
        _snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, String> {
        Ok(BudgetDecision::Continue)
    }
}

/// A policy that imposes no limits.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopBudgetPolicy;

impl TurnBudgetPolicy for NoopBudgetPolicy {}

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
    }
}
