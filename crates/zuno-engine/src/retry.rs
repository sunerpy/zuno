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

use zuno_error::ProviderError;
use zuno_llm::event::StreamEvent;
use zuno_llm::sse::MAX_PROVIDER_WAIT;

/// Bounds repeated compaction when a conversation still exceeds the context
/// window after the compactor has already tried to make it fit.
pub const MAX_CONTEXT_LIMIT_RETRIES: u32 = 5;

/// Prevents a provider that repeatedly stops at an output limit from keeping a
/// turn alive forever through incomplete-response continuation prompts.
pub const MAX_INCOMPLETE_CONTINUATION_ATTEMPTS: u32 = 3;

/// Prevents an empty response after tool results from silently ending a turn or
/// retrying forever. One such response in turn 43 ended a 20-hour run half-done.
pub const MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS: u32 = 5;

pub const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(2);

pub const RETRY_MAX_DELAY_WITHOUT_PROVIDER: Duration = Duration::from_secs(30);

/// Maximum provider calls in one recovery sequence.
pub const PROVIDER_RETRY_MAX_ATTEMPTS: u32 = 3;

/// Wall-clock budget for coordinating one provider recovery sequence.
///
/// The budget starts only after the first retryable provider failure. Active
/// provider attempts are governed by their transport and stream-idle policies;
/// this budget limits rollback emission, backoff, and admission of another
/// replay without killing a healthy long-running attempt.
pub const PROVIDER_RETRY_MAX_ELAPSED: Duration = MAX_PROVIDER_WAIT;

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
    max_elapsed: Duration,
}

impl ProviderRetryPolicy {
    #[must_use]
    pub const fn new(max_attempts: NonZeroU32) -> Self {
        Self {
            max_attempts,
            max_elapsed: PROVIDER_RETRY_MAX_ELAPSED,
        }
    }

    #[must_use]
    pub const fn max_attempts(self) -> NonZeroU32 {
        self.max_attempts
    }

    #[must_use]
    pub const fn max_elapsed(self) -> Duration {
        self.max_elapsed
    }

    #[must_use]
    pub fn delay_after(self, failed_attempt: u32, error: &ProviderError) -> Duration {
        error
            .retry_after()
            .unwrap_or_else(|| fallback_delay(failed_attempt))
    }
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
    #[error("provider retry deadline exceeded on attempt {attempt} after {elapsed:?}")]
    DeadlineExceeded { attempt: u32, elapsed: Duration },
    #[error("failed to emit RetryRollback before provider retry")]
    RollbackEmission {
        #[source]
        source: Box<E>,
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
    mut operation: Operation,
    mut emit: Emit,
    mut sleep: Sleep,
    mut wake: Wake,
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
    let max = policy.max_attempts().get();
    let mut recovery_started = None;
    let mut attempt = 1_u32;

    loop {
        let result = operation(attempt).await;
        match result {
            Ok(output) => return Ok(output),
            Err(error) if !error.is_retryable() => {
                return Err(ProviderRetryError::Provider(error));
            }
            Err(error) if attempt >= max => {
                return Err(ProviderRetryError::AttemptsExhausted {
                    attempts: attempt,
                    source: error,
                });
            }
            Err(error) => {
                let started = *recovery_started.get_or_insert_with(tokio::time::Instant::now);
                let deadline = started + policy.max_elapsed();
                let delay = policy.delay_after(attempt, &error);
                let next_attempt = attempt + 1;
                if delay >= deadline.saturating_duration_since(tokio::time::Instant::now()) {
                    return Err(ProviderRetryError::DeadlineExceeded {
                        attempt,
                        elapsed: started.elapsed(),
                    });
                }
                tokio::time::timeout_at(
                    deadline,
                    emit(StreamEvent::RetryRollback {
                        attempt: next_attempt,
                        max,
                    }),
                )
                .await
                .map_err(|_| ProviderRetryError::DeadlineExceeded {
                    attempt,
                    elapsed: started.elapsed(),
                })?
                .map_err(|source| ProviderRetryError::RollbackEmission {
                    source: Box::new(source),
                })?;
                let wait = async {
                    tokio::select! {
                        biased;
                        output = wake() => RetryDelay::Woken(output),
                        () = sleep(delay) => RetryDelay::Elapsed,
                    }
                };
                let delay = tokio::time::timeout_at(deadline, wait).await.map_err(|_| {
                    ProviderRetryError::DeadlineExceeded {
                        attempt,
                        elapsed: started.elapsed(),
                    }
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

fn fallback_delay(failed_attempt: u32) -> Duration {
    let exponent = failed_attempt.saturating_sub(1).min(31);
    let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
    RETRY_INITIAL_DELAY
        .saturating_mul(multiplier)
        .min(RETRY_MAX_DELAY_WITHOUT_PROVIDER)
}
