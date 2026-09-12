//! Typed provider retry and bounded turn recovery.
//!
//! Provider retry decisions use [`ProviderError`] data directly. Turn-level
//! recovery counters are separate because context compaction and continuation
//! prompts change the next request rather than replaying the failed request.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::time::Duration;

use zuno_config::schema::provider::{
    DEFAULT_PROVIDER_RETRY_INITIAL_DELAY_MS, DEFAULT_PROVIDER_RETRY_JITTER_PERCENT,
    DEFAULT_PROVIDER_RETRY_MAX_ATTEMPTS, DEFAULT_PROVIDER_RETRY_MAX_DELAY_MS,
    DEFAULT_PROVIDER_RETRY_RECOVERY_WINDOW_MS, ProviderRetryConfig,
};
use zuno_error::{ProviderDiagnostic, ProviderError};
use zuno_llm::event::StreamEvent;

/// Bounds repeated compaction when a conversation still exceeds the context
/// window after the compactor has already tried to make it fit.
pub const MAX_CONTEXT_LIMIT_RETRIES: u32 = 5;

/// Prevents a provider that repeatedly stops at an output limit from keeping a
/// turn alive forever through incomplete-response continuation prompts.
pub const MAX_INCOMPLETE_CONTINUATION_ATTEMPTS: u32 = 3;

/// Prevents an empty response after tool results from silently ending a turn or
/// retrying forever. One such response in turn 43 ended a 20-hour run half-done.
pub const MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS: u32 = 5;

pub const RETRY_INITIAL_DELAY: Duration =
    Duration::from_millis(DEFAULT_PROVIDER_RETRY_INITIAL_DELAY_MS);

pub const RETRY_MAX_DELAY_WITHOUT_PROVIDER: Duration =
    Duration::from_millis(DEFAULT_PROVIDER_RETRY_MAX_DELAY_MS);

/// Symmetric jitter applied only to locally selected retry delays.
pub const PROVIDER_RETRY_JITTER_PERCENT: u8 = DEFAULT_PROVIDER_RETRY_JITTER_PERCENT;

/// Maximum provider calls in one recovery sequence.
pub const PROVIDER_RETRY_MAX_ATTEMPTS: u32 = DEFAULT_PROVIDER_RETRY_MAX_ATTEMPTS;

/// Wall-clock budget for coordinating replacement provider attempts.
///
/// The initial request remains governed by transport and stream-idle policies.
/// This budget begins only after that request returns its first retryable failure,
/// then covers rollback, backoff, and every replacement attempt.
pub const PROVIDER_RETRY_RECOVERY_WINDOW: Duration =
    Duration::from_millis(DEFAULT_PROVIDER_RETRY_RECOVERY_WINDOW_MS);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryBudget {
    ContextLimit,
    IncompleteContinuation,
    EmptyPostToolContinuation,
}

impl RecoveryBudget {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ContextLimit => "context-limit retries",
            Self::IncompleteContinuation => "incomplete-continuation attempts",
            Self::EmptyPostToolContinuation => "empty-post-tool-continuation attempts",
        }
    }

    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        match self {
            Self::ContextLimit => MAX_CONTEXT_LIMIT_RETRIES,
            Self::IncompleteContinuation => MAX_INCOMPLETE_CONTINUATION_ATTEMPTS,
            Self::EmptyPostToolContinuation => MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS,
        }
    }
}

impl fmt::Display for RecoveryBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryAttempt {
    pub attempt: u32,
    pub max: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RetryError {
    #[error("{budget} budget exhausted after {attempts} attempts")]
    BudgetExhausted {
        budget: RecoveryBudget,
        attempts: u32,
    },
}

#[derive(Debug, Default)]
pub struct RecoveryBudgets {
    context_limit_retries: u32,
    incomplete_continuation_attempts: u32,
    empty_post_tool_continuation_attempts: u32,
}

impl RecoveryBudgets {
    pub fn record_context_limit_retry(&mut self) -> Result<RecoveryAttempt, RetryError> {
        Self::record(
            &mut self.context_limit_retries,
            RecoveryBudget::ContextLimit,
        )
    }

    pub fn reset_context_limit_retries(&mut self) {
        self.context_limit_retries = 0;
    }

    pub fn record_incomplete_continuation(&mut self) -> Result<RecoveryAttempt, RetryError> {
        Self::record(
            &mut self.incomplete_continuation_attempts,
            RecoveryBudget::IncompleteContinuation,
        )
    }

    pub fn record_empty_post_tool_continuation(&mut self) -> Result<RecoveryAttempt, RetryError> {
        Self::record(
            &mut self.empty_post_tool_continuation_attempts,
            RecoveryBudget::EmptyPostToolContinuation,
        )
    }

    #[must_use]
    pub const fn attempts(&self, budget: RecoveryBudget) -> u32 {
        match budget {
            RecoveryBudget::ContextLimit => self.context_limit_retries,
            RecoveryBudget::IncompleteContinuation => self.incomplete_continuation_attempts,
            RecoveryBudget::EmptyPostToolContinuation => self.empty_post_tool_continuation_attempts,
        }
    }

    fn record(attempts: &mut u32, budget: RecoveryBudget) -> Result<RecoveryAttempt, RetryError> {
        let max = budget.max_attempts();
        if *attempts >= max {
            return Err(RetryError::BudgetExhausted {
                budget,
                attempts: *attempts,
            });
        }

        *attempts += 1;
        Ok(RecoveryAttempt {
            attempt: *attempts,
            max,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRetryPolicy {
    max_attempts: NonZeroU32,
    recovery_window: Duration,
    initial_delay: Duration,
    max_delay: Duration,
    jitter_percent: u8,
}

impl ProviderRetryPolicy {
    #[must_use]
    pub const fn new(max_attempts: NonZeroU32) -> Self {
        Self {
            max_attempts,
            recovery_window: PROVIDER_RETRY_RECOVERY_WINDOW,
            initial_delay: RETRY_INITIAL_DELAY,
            max_delay: RETRY_MAX_DELAY_WITHOUT_PROVIDER,
            jitter_percent: PROVIDER_RETRY_JITTER_PERCENT,
        }
    }

    /// Construct a validated policy with explicit timing for composition roots
    /// and deterministic tests.
    pub fn with_timing(
        max_attempts: NonZeroU32,
        recovery_window: Duration,
        initial_delay: Duration,
        max_delay: Duration,
        jitter_percent: u8,
    ) -> Result<Self, ProviderRetryPolicyError> {
        if recovery_window.is_zero() || initial_delay.is_zero() || max_delay.is_zero() {
            return Err(ProviderRetryPolicyError::ZeroDuration);
        }
        if max_delay < initial_delay {
            return Err(ProviderRetryPolicyError::MaxDelayBeforeInitial);
        }
        if jitter_percent > 100 {
            return Err(ProviderRetryPolicyError::JitterPercentOutOfRange {
                actual: jitter_percent,
            });
        }
        Ok(Self {
            max_attempts,
            recovery_window,
            initial_delay,
            max_delay,
            jitter_percent,
        })
    }

    /// Resolve a provider's optional config over the native defaults.
    pub fn from_config(
        config: Option<&ProviderRetryConfig>,
    ) -> Result<Self, ProviderRetryPolicyError> {
        let Some(config) = config else {
            return Ok(Self::new(
                NonZeroU32::new(PROVIDER_RETRY_MAX_ATTEMPTS)
                    .expect("provider retry maximum is non-zero"),
            ));
        };
        Self::with_timing(
            config.resolved_max_attempts(),
            Duration::from_millis(config.resolved_recovery_window_ms().get()),
            Duration::from_millis(config.resolved_initial_delay_ms().get()),
            Duration::from_millis(config.resolved_max_delay_ms().get()),
            config.resolved_jitter_percent(),
        )
    }

    #[must_use]
    pub const fn max_attempts(self) -> NonZeroU32 {
        self.max_attempts
    }

    #[must_use]
    pub const fn recovery_window(self) -> Duration {
        self.recovery_window
    }

    #[must_use]
    pub fn delay_after(self, failed_attempt: u32, error: &ProviderError, entropy: u64) -> Duration {
        error
            .retry_after()
            .filter(|delay| !delay.is_zero())
            .map_or_else(
                || {
                    let base =
                        exponential_delay(self.initial_delay, self.max_delay, failed_attempt);
                    jittered_delay(base, self.max_delay, self.jitter_percent, entropy)
                },
                |delay| {
                    delay
                        .min(self.recovery_window)
                        .max(Duration::from_millis(1))
                },
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProviderRetryPolicyError {
    #[error("provider retry durations must be greater than zero")]
    ZeroDuration,
    #[error("provider retry max delay must be greater than or equal to the initial delay")]
    MaxDelayBeforeInitial,
    #[error("provider retry jitter percent {actual} is outside 0..=100")]
    JitterPercentOutOfRange { actual: u8 },
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderRetryError<E>
where
    E: Error + 'static,
{
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("provider retry attempts exhausted after {attempts} attempts")]
    AttemptsExhausted {
        attempts: u32,
        #[source]
        source: ProviderError,
    },
    #[error(
        "provider retry deadline exceeded on attempt {attempt} after {recovery_elapsed:?} \
         recovery ({total_elapsed:?} total; last provider failure: {last_failure})"
    )]
    DeadlineExceeded {
        attempt: u32,
        recovery_elapsed: Duration,
        total_elapsed: Duration,
        last_failure: Box<ProviderDiagnostic>,
    },
    /// The peer asked for a longer delay than the recovery deadline had left.
    ///
    /// Recovery stops instead of sleeping past its deadline — and instead of
    /// replacing the peer's delay with a shorter local one. `source` is the typed
    /// error that named `retry_after`, so the turn owner can schedule its own retry
    /// with the peer's delay, clamped to the configured ceiling, rather than fall back
    /// to a local backoff the peer has already said is too soon.
    #[error(
        "provider asked to retry after {retry_after:?} on attempt {attempt}, beyond the recovery deadline after {elapsed:?}"
    )]
    RetryAfterBeyondDeadline {
        attempt: u32,
        elapsed: Duration,
        retry_after: Duration,
        #[source]
        source: ProviderError,
    },
    #[error("failed to emit RetryRollback before provider retry")]
    RollbackEmission {
        #[source]
        source: Box<E>,
    },
}

/// A durable observer's view of one provider attempt.
#[derive(Debug)]
pub enum ProviderAttemptObservation<'a, T> {
    /// The attempt is about to call the provider.
    Started { attempt: u32, max: u32 },
    /// The attempt returned, before any rollback, delay, or replay is admitted.
    Finished {
        attempt: u32,
        max: u32,
        result: &'a Result<T, ProviderError>,
    },
    /// A replacement attempt was cancelled by the absolute recovery deadline.
    DeadlineExceeded {
        attempt: u32,
        max: u32,
        recovery_elapsed: Duration,
        total_elapsed: Duration,
        last_failure: &'a ProviderDiagnostic,
    },
    /// Rollback was emitted and this exact deadline must commit before sleeping.
    BackoffScheduled {
        failed_attempt: u32,
        next_attempt: u32,
        max: u32,
        delay: Duration,
        error: &'a ProviderError,
    },
}

/// A provider retry failed either in recovery itself or while durably observing
/// an attempt boundary.
#[derive(Debug, thiserror::Error)]
pub enum ProviderRetryObservedError<E, O>
where
    E: Error + 'static,
    O: Error + 'static,
{
    #[error(transparent)]
    Retry(#[from] ProviderRetryError<E>),
    #[error("provider attempt observation failed")]
    Observation {
        #[source]
        source: O,
    },
}

/// Retry a provider operation with real Tokio sleeps between attempts.
///
/// `emit` receives [`StreamEvent::RetryRollback`] before every sleep and replay.
/// Its asynchronous result lets the turn loop preserve lossless backpressure.
pub async fn retry_provider<T, Operation, OperationFuture, Emit, EmitFuture, EmitError>(
    policy: ProviderRetryPolicy,
    operation: Operation,
    emit: Emit,
) -> Result<T, ProviderRetryError<EmitError>>
where
    Operation: FnMut(u32) -> OperationFuture,
    OperationFuture: Future<Output = Result<T, ProviderError>>,
    Emit: FnMut(StreamEvent) -> EmitFuture,
    EmitFuture: Future<Output = Result<(), EmitError>>,
    EmitError: Error + 'static,
{
    retry_provider_with_sleep(policy, operation, emit, tokio::time::sleep).await
}

/// Retry a provider operation while allowing user control to interrupt backoff.
///
/// `wake` is polled only between attempts. The provider operation remains
/// responsible for observing the same control while its stream is active.
pub async fn retry_provider_with_wake<
    T,
    Operation,
    OperationFuture,
    Emit,
    EmitFuture,
    EmitError,
    Wake,
    WakeFuture,
>(
    policy: ProviderRetryPolicy,
    operation: Operation,
    emit: Emit,
    wake: Wake,
) -> Result<T, ProviderRetryError<EmitError>>
where
    Operation: FnMut(u32) -> OperationFuture,
    OperationFuture: Future<Output = Result<T, ProviderError>>,
    Emit: FnMut(StreamEvent) -> EmitFuture,
    EmitFuture: Future<Output = Result<(), EmitError>>,
    EmitError: Error + 'static,
    Wake: FnMut() -> WakeFuture,
    WakeFuture: Future<Output = T>,
{
    retry_provider_with_sleep_and_wake(policy, operation, emit, tokio::time::sleep, wake).await
}

/// Retry a provider operation while synchronously observing every attempt boundary.
///
/// `observe` runs before the provider call and immediately after it returns. A
/// failed observation aborts recovery before rollback or another attempt, so a
/// caller can require durable attempt records before replay is scheduled.
pub async fn retry_provider_with_wake_observed<
    T,
    Operation,
    OperationFuture,
    Emit,
    EmitFuture,
    EmitError,
    Wake,
    WakeFuture,
    Observe,
    ObserveError,
>(
    policy: ProviderRetryPolicy,
    operation: Operation,
    emit: Emit,
    wake: Wake,
    observe: Observe,
) -> Result<T, ProviderRetryObservedError<EmitError, ObserveError>>
where
    Operation: FnMut(u32) -> OperationFuture,
    OperationFuture: Future<Output = Result<T, ProviderError>>,
    Emit: FnMut(StreamEvent) -> EmitFuture,
    EmitFuture: Future<Output = Result<(), EmitError>>,
    EmitError: Error + 'static,
    Wake: FnMut() -> WakeFuture,
    WakeFuture: Future<Output = T>,
    Observe: for<'a> FnMut(ProviderAttemptObservation<'a, T>) -> Result<(), ObserveError>,
    ObserveError: Error + 'static,
{
    retry_provider_with_sleep_and_wake_observed(
        policy,
        operation,
        emit,
        tokio::time::sleep,
        wake,
        observe,
    )
    .await
}

/// Retry a provider operation with an injected sleeper for deterministic tests.
///
/// The callback order is operation failure, rollback emission, sleep, then the
/// next operation. A rollback emission failure aborts before any replay starts.
pub async fn retry_provider_with_sleep<
    T,
    Operation,
    OperationFuture,
    Emit,
    EmitFuture,
    EmitError,
    Sleep,
    SleepFuture,
>(
    policy: ProviderRetryPolicy,
    operation: Operation,
    emit: Emit,
    sleep: Sleep,
) -> Result<T, ProviderRetryError<EmitError>>
where
    Operation: FnMut(u32) -> OperationFuture,
    OperationFuture: Future<Output = Result<T, ProviderError>>,
    Emit: FnMut(StreamEvent) -> EmitFuture,
    EmitFuture: Future<Output = Result<(), EmitError>>,
    EmitError: Error + 'static,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
{
    retry_provider_with_sleep_and_wake(policy, operation, emit, sleep, std::future::pending::<T>)
        .await
}

async fn retry_provider_with_sleep_and_wake<
    T,
    Operation,
    OperationFuture,
    Emit,
    EmitFuture,
    EmitError,
    Sleep,
    SleepFuture,
    Wake,
    WakeFuture,
>(
    policy: ProviderRetryPolicy,
    operation: Operation,
    emit: Emit,
    sleep: Sleep,
    wake: Wake,
) -> Result<T, ProviderRetryError<EmitError>>
where
    Operation: FnMut(u32) -> OperationFuture,
    OperationFuture: Future<Output = Result<T, ProviderError>>,
    Emit: FnMut(StreamEvent) -> EmitFuture,
    EmitFuture: Future<Output = Result<(), EmitError>>,
    EmitError: Error + 'static,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
    Wake: FnMut() -> WakeFuture,
    WakeFuture: Future<Output = T>,
{
    let result = retry_provider_with_sleep_and_wake_observed(
        policy,
        operation,
        emit,
        sleep,
        wake,
        |_: ProviderAttemptObservation<'_, T>| Ok::<(), std::convert::Infallible>(()),
    )
    .await;
    match result {
        Ok(output) => Ok(output),
        Err(ProviderRetryObservedError::Retry(error)) => Err(error),
        Err(ProviderRetryObservedError::Observation { source }) => match source {},
    }
}

async fn retry_provider_with_sleep_and_wake_observed<
    T,
    Operation,
    OperationFuture,
    Emit,
    EmitFuture,
    EmitError,
    Sleep,
    SleepFuture,
    Wake,
    WakeFuture,
    Observe,
    ObserveError,
>(
    policy: ProviderRetryPolicy,
    mut operation: Operation,
    mut emit: Emit,
    mut sleep: Sleep,
    mut wake: Wake,
    mut observe: Observe,
) -> Result<T, ProviderRetryObservedError<EmitError, ObserveError>>
where
    Operation: FnMut(u32) -> OperationFuture,
    OperationFuture: Future<Output = Result<T, ProviderError>>,
    Emit: FnMut(StreamEvent) -> EmitFuture,
    EmitFuture: Future<Output = Result<(), EmitError>>,
    EmitError: Error + 'static,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: Future<Output = ()>,
    Wake: FnMut() -> WakeFuture,
    WakeFuture: Future<Output = T>,
    Observe: for<'a> FnMut(ProviderAttemptObservation<'a, T>) -> Result<(), ObserveError>,
    ObserveError: Error + 'static,
{
    let max = policy.max_attempts().get();
    let total_started = tokio::time::Instant::now();
    let mut recovery_started: Option<tokio::time::Instant> = None;
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut last_failure: Option<ProviderDiagnostic> = None;
    let mut attempt = 1_u32;

    loop {
        observe(ProviderAttemptObservation::Started { attempt, max })
            .map_err(|source| ProviderRetryObservedError::Observation { source })?;
        let result = match deadline {
            None => operation(attempt).await,
            Some(deadline) => match tokio::time::timeout_at(deadline, operation(attempt)).await {
                Ok(result) => result,
                Err(_) => {
                    let last_failure = last_failure
                        .clone()
                        .expect("a recovery deadline follows a provider failure");
                    let recovery_elapsed = recovery_started
                        .expect("a replacement attempt has a recovery start")
                        .elapsed();
                    let total_elapsed = total_started.elapsed();
                    observe(ProviderAttemptObservation::DeadlineExceeded {
                        attempt,
                        max,
                        recovery_elapsed,
                        total_elapsed,
                        last_failure: &last_failure,
                    })
                    .map_err(|source| ProviderRetryObservedError::Observation { source })?;
                    return Err(ProviderRetryError::DeadlineExceeded {
                        attempt,
                        recovery_elapsed,
                        total_elapsed,
                        last_failure: Box::new(last_failure),
                    }
                    .into());
                }
            },
        };
        observe(ProviderAttemptObservation::Finished {
            attempt,
            max,
            result: &result,
        })
        .map_err(|source| ProviderRetryObservedError::Observation { source })?;
        match result {
            Ok(output) => return Ok(output),
            Err(error) if !error.is_retryable() => {
                return Err(ProviderRetryError::Provider(error).into());
            }
            Err(error) if attempt >= max => {
                return Err(ProviderRetryError::AttemptsExhausted {
                    attempts: attempt,
                    source: error,
                }
                .into());
            }
            Err(error) => {
                let diagnostic = error.diagnostic_snapshot();
                last_failure = Some(diagnostic.clone());
                let recovery_started =
                    *recovery_started.get_or_insert_with(tokio::time::Instant::now);
                let deadline = *deadline.get_or_insert(recovery_started + policy.recovery_window());
                let delay = policy.delay_after(attempt, &error, retry_entropy());
                let next_attempt = attempt + 1;
                if delay >= deadline.saturating_duration_since(tokio::time::Instant::now()) {
                    let recovery_elapsed = recovery_started.elapsed();
                    return Err(match error.retry_after().filter(|after| !after.is_zero()) {
                        Some(retry_after) => ProviderRetryError::RetryAfterBeyondDeadline {
                            attempt,
                            elapsed: recovery_elapsed,
                            retry_after,
                            source: error,
                        },
                        None => ProviderRetryError::DeadlineExceeded {
                            attempt,
                            recovery_elapsed,
                            total_elapsed: total_started.elapsed(),
                            last_failure: Box::new(diagnostic),
                        },
                    }
                    .into());
                }
                tokio::time::timeout_at(
                    deadline,
                    emit(StreamEvent::RetryRollback {
                        attempt: next_attempt,
                        max,
                    }),
                )
                .await
                .map_err(|_| {
                    ProviderRetryObservedError::Retry(ProviderRetryError::DeadlineExceeded {
                        attempt,
                        recovery_elapsed: recovery_started.elapsed(),
                        total_elapsed: total_started.elapsed(),
                        last_failure: Box::new(diagnostic.clone()),
                    })
                })?
                .map_err(|source| {
                    ProviderRetryObservedError::Retry(ProviderRetryError::RollbackEmission {
                        source: Box::new(source),
                    })
                })?;
                observe(ProviderAttemptObservation::BackoffScheduled {
                    failed_attempt: attempt,
                    next_attempt,
                    max,
                    delay,
                    error: &error,
                })
                .map_err(|source| ProviderRetryObservedError::Observation { source })?;
                let wait = async {
                    tokio::select! {
                        biased;
                        output = wake() => RetryDelay::Woken(output),
                        () = sleep(delay) => RetryDelay::Elapsed,
                    }
                };
                let delay = tokio::time::timeout_at(deadline, wait).await.map_err(|_| {
                    ProviderRetryObservedError::Retry(ProviderRetryError::DeadlineExceeded {
                        attempt,
                        recovery_elapsed: recovery_started.elapsed(),
                        total_elapsed: total_started.elapsed(),
                        last_failure: Box::new(diagnostic.clone()),
                    })
                })?;
                if let RetryDelay::Woken(output) = delay {
                    return Ok(output);
                }
                attempt = next_attempt;
            }
        }
    }
}

enum RetryDelay<T> {
    Elapsed,
    Woken(T),
}

fn exponential_delay(initial: Duration, maximum: Duration, failed_attempt: u32) -> Duration {
    let exponent = failed_attempt.saturating_sub(1).min(31);
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
    let capped_ms = jittered_ms.max(1).min(maximum.as_millis());
    Duration::from_millis(u64::try_from(capped_ms).unwrap_or(u64::MAX))
}

fn retry_entropy() -> u64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    u64::from_le_bytes(bytes[..8].try_into().expect("UUID has eight leading bytes"))
}
