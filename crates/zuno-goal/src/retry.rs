//! Persistent retry policy for recoverable goal-turn failures.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Goal;
use zuno_engine::r#loop::TurnRetryReason;

/// Initial delay for the first recoverable goal-turn failure.
pub const DEFAULT_GOAL_RETRY_INITIAL_DELAY: Duration = Duration::from_secs(2);
/// Maximum delay between automatic goal turns.
pub const DEFAULT_GOAL_RETRY_MAX_DELAY: Duration = Duration::from_secs(300);
/// Symmetric jitter applied to locally selected delays.
pub const DEFAULT_GOAL_RETRY_JITTER_PERCENT: u8 = 20;
/// Maximum time a surface waits before rechecking queued user work.
pub const DEFAULT_GOAL_RETRY_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Why a completed turn needs another attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalRetryReason {
    /// The provider explicitly rate limited the request.
    RateLimited,
    /// A transport or upstream server failure is expected to clear.
    ProviderTransient,
    /// The provider stream ended without a complete message.
    ProviderStream,
    /// The bounded same-request retry sequence exhausted its wall-clock budget.
    ProviderRetryDeadline,
    /// SQLite reported another active writer.
    DatabaseBusy,
    /// One agent turn reached its step ceiling while the larger goal remained active.
    StepLimit,
    /// The provider returned no assistant content.
    EmptyAssistantMessage,
    /// A context-limit failure requires compaction before another goal turn.
    ContextLimit,
    /// Compaction completed durably and the provider request may be retried directly.
    ContextCompacted,
}

impl GoalRetryReason {
    /// Stable discriminator persisted in the retry table and shown in status events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::ProviderTransient => "provider_transient",
            Self::ProviderStream => "provider_stream",
            Self::ProviderRetryDeadline => "provider_retry_deadline",
            Self::DatabaseBusy => "database_busy",
            Self::StepLimit => "step_limit",
            Self::EmptyAssistantMessage => "empty_assistant_message",
            Self::ContextLimit => "context_limit",
            Self::ContextCompacted => "context_compacted",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "rate_limited" => Some(Self::RateLimited),
            "provider_transient" => Some(Self::ProviderTransient),
            "provider_stream" => Some(Self::ProviderStream),
            "provider_retry_deadline" => Some(Self::ProviderRetryDeadline),
            "database_busy" => Some(Self::DatabaseBusy),
            "step_limit" => Some(Self::StepLimit),
            "empty_assistant_message" => Some(Self::EmptyAssistantMessage),
            "context_limit" => Some(Self::ContextLimit),
            "context_compacted" => Some(Self::ContextCompacted),
            _ => None,
        }
    }
}

impl From<TurnRetryReason> for GoalRetryReason {
    fn from(reason: TurnRetryReason) -> Self {
        match reason {
            TurnRetryReason::RateLimited => Self::RateLimited,
            TurnRetryReason::ProviderTransient => Self::ProviderTransient,
            TurnRetryReason::ProviderStream => Self::ProviderStream,
            TurnRetryReason::ProviderRetryDeadline => Self::ProviderRetryDeadline,
            TurnRetryReason::DatabaseBusy => Self::DatabaseBusy,
            TurnRetryReason::StepLimit => Self::StepLimit,
            TurnRetryReason::EmptyAssistantMessage => Self::EmptyAssistantMessage,
        }
    }
}

/// One durable retry schedule tied to an exact goal instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRetryState {
    /// Session whose automatic continuation is delayed.
    pub session_id: String,
    /// Goal instance this schedule belongs to.
    pub goal_id: String,
    /// Consecutive recoverable terminal failures for this goal.
    pub attempt: u32,
    /// Typed reason for the retry.
    pub reason: GoalRetryReason,
    /// Actual jittered delay selected for this attempt.
    pub delay_ms: i64,
    /// Unix timestamp at which another automatic turn may start.
    pub retry_at_ms: i64,
    /// Unix timestamp at which this plan was committed.
    pub scheduled_at_ms: i64,
}

/// Invalid retry-policy configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GoalRetryPolicyError {
    /// A zero duration would create a hot loop.
    #[error("goal retry durations must be greater than zero")]
    ZeroDuration,
    /// The cap must not precede the first retry delay.
    #[error("goal retry max delay must be greater than or equal to the initial delay")]
    MaxDelayBeforeInitial,
    /// Jitter is a percentage and cannot exceed one hundred.
    #[error("goal retry jitter percent {actual} is outside 0..=100")]
    JitterPercentOutOfRange { actual: u8 },
}

/// Fully resolved automatic goal retry settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalRetryPolicy {
    initial_delay: Duration,
    max_delay: Duration,
    jitter_percent: u8,
    poll_interval: Duration,
}

impl GoalRetryPolicy {
    /// Validate and construct one resolved retry policy.
    pub fn new(
        initial_delay: Duration,
        max_delay: Duration,
        jitter_percent: u8,
        poll_interval: Duration,
    ) -> Result<Self, GoalRetryPolicyError> {
        if initial_delay.is_zero() || max_delay.is_zero() || poll_interval.is_zero() {
            return Err(GoalRetryPolicyError::ZeroDuration);
        }
        if max_delay < initial_delay {
            return Err(GoalRetryPolicyError::MaxDelayBeforeInitial);
        }
        if jitter_percent > 100 {
            return Err(GoalRetryPolicyError::JitterPercentOutOfRange {
                actual: jitter_percent,
            });
        }
        Ok(Self {
            initial_delay,
            max_delay,
            jitter_percent,
            poll_interval,
        })
    }

    /// Maximum interval before a surface rechecks queued human input.
    #[must_use]
    pub const fn poll_interval(self) -> Duration {
        self.poll_interval
    }

    /// Select the delay for a one-based consecutive failure count.
    #[must_use]
    pub fn delay(self, attempt: u32, retry_after: Option<Duration>, entropy: u64) -> Duration {
        if let Some(retry_after) =
            retry_after.filter(|delay| !delay.is_zero() && *delay <= self.max_delay)
        {
            return retry_after;
        }
        let local = exponential_delay(self.initial_delay, self.max_delay, attempt);
        jittered_delay(local, self.max_delay, self.jitter_percent, entropy)
    }
}

impl Default for GoalRetryPolicy {
    fn default() -> Self {
        Self::new(
            DEFAULT_GOAL_RETRY_INITIAL_DELAY,
            DEFAULT_GOAL_RETRY_MAX_DELAY,
            DEFAULT_GOAL_RETRY_JITTER_PERCENT,
            DEFAULT_GOAL_RETRY_POLL_INTERVAL,
        )
        .expect("default goal retry policy is valid")
    }
}

/// How a terminal turn failure affects an active goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalTerminalFailure {
    /// Persist a new retry schedule and keep the goal active.
    Retry {
        /// Typed failure class.
        reason: GoalRetryReason,
        /// Delay requested by the failed peer, when one was supplied.
        retry_after: Option<Duration>,
    },
    /// Stop automatic execution until a user resumes the goal.
    Pause,
    /// Mark the goal blocked because repeating cannot repair the failure.
    Block,
}

/// Persisted result of applying one terminal failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalFailureDisposition {
    /// A recoverable failure produced a durable retry schedule.
    RetryScheduled(GoalRetryState),
    /// Human intervention is required.
    Paused(Goal),
    /// The failure is permanent for the current goal.
    Blocked(Goal),
    /// No active goal existed by the time the failure was recorded.
    NoActiveGoal,
}

fn exponential_delay(initial: Duration, maximum: Duration, attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(31);
    let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
    initial.saturating_mul(multiplier).min(maximum)
}

fn jittered_delay(base: Duration, maximum: Duration, jitter_percent: u8, entropy: u64) -> Duration {
    if jitter_percent == 0 {
        return base.min(maximum);
    }
    let base_ms = base.as_millis();
    let span = base_ms
        .saturating_mul(u128::from(jitter_percent))
        .saturating_div(100);
    let width = span.saturating_mul(2);
    let position = width
        .saturating_mul(u128::from(entropy))
        .checked_div(u128::from(u64::MAX))
        .unwrap_or_default();
    let jittered_ms = base_ms.saturating_sub(span).saturating_add(position);
    let capped_ms = jittered_ms.min(maximum.as_millis());
    Duration::from_millis(u64::try_from(capped_ms).unwrap_or(u64::MAX))
}

pub(crate) fn now_ms() -> Result<i64, std::time::SystemTimeError> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
}

pub(crate) fn entropy() -> u64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    u64::from_le_bytes(bytes[..8].try_into().expect("UUID has eight leading bytes"))
}
