use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::ready;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Duration;

use zuno_engine::r#loop::{TurnError, TurnRecovery, TurnRetryReason};
use zuno_engine::retry::{
    MAX_CONTEXT_LIMIT_RETRIES, MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS,
    MAX_INCOMPLETE_CONTINUATION_ATTEMPTS, PROVIDER_RETRY_MAX_ATTEMPTS, PROVIDER_RETRY_MAX_ELAPSED,
    ProviderAttemptObservation, ProviderRetryError, ProviderRetryPolicy, ProviderRetryPolicyError,
    RETRY_INITIAL_DELAY, RETRY_MAX_DELAY_WITHOUT_PROVIDER, RecoveryAttempt, RecoveryBudget,
    RecoveryBudgets, RetryError, retry_provider, retry_provider_with_sleep,
    retry_provider_with_wake_observed,
};
use zuno_error::{ProviderError, ProviderProtocolFailure, ProviderStreamFailure};
use zuno_llm::event::StreamEvent;

fn policy(max_attempts: u32) -> ProviderRetryPolicy {
    ProviderRetryPolicy::with_timing(
        NonZeroU32::new(max_attempts).expect("non-zero test policy"),
        PROVIDER_RETRY_MAX_ELAPSED,
        RETRY_INITIAL_DELAY,
        RETRY_MAX_DELAY_WITHOUT_PROVIDER,
        0,
    )
    .expect("deterministic provider retry policy")
}

#[test]
fn every_terminal_turn_error_has_an_explicit_goal_recovery_decision() {
    let cases = [
        (
            TurnError::StepLimit {
                agent: "build".to_owned(),
                max_steps: 100,
            },
            TurnRecovery::Retry {
                reason: TurnRetryReason::StepLimit,
                after: None,
            },
        ),
        (
            TurnError::StreamEndedWithoutMessageEnd { step: 1 },
            TurnRecovery::Retry {
                reason: TurnRetryReason::ProviderStream,
                after: None,
            },
        ),
        (
            TurnError::EmptyAssistantMessage {
                provider_id: "test".to_owned(),
                step: 1,
            },
            TurnRecovery::Retry {
                reason: TurnRetryReason::EmptyAssistantMessage,
                after: None,
            },
        ),
        (
            TurnError::Database(zuno_error::DbError::Busy {
                retry_after: Some(Duration::from_millis(50)),
            }),
            TurnRecovery::Retry {
                reason: TurnRetryReason::DatabaseBusy,
                after: Some(Duration::from_millis(50)),
            },
        ),
        (
            TurnError::Provider(ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            }),
            TurnRecovery::Retry {
                reason: TurnRetryReason::RateLimited,
                after: Some(Duration::from_secs(7)),
            },
        ),
        (
            TurnError::Provider(ProviderError::Transient {
                status: Some(503),
                source: None,
            }),
            TurnRecovery::Retry {
                reason: TurnRetryReason::ProviderTransient,
                after: None,
            },
        ),
        (
            TurnError::Provider(ProviderError::Stream {
                code: ProviderStreamFailure::UpstreamStreamError,
                source: None,
            }),
            TurnRecovery::Retry {
                reason: TurnRetryReason::ProviderTransient,
                after: None,
            },
        ),
        (
            TurnError::Provider(ProviderError::Protocol {
                code: ProviderProtocolFailure::UpstreamProtocolError,
                source: None,
            }),
            TurnRecovery::Fail,
        ),
        (
            TurnError::ProviderRetryDeadlineExceeded {
                attempt: 3,
                elapsed: Duration::from_secs(180),
            },
            TurnRecovery::Retry {
                reason: TurnRetryReason::ProviderRetryDeadline,
                after: None,
            },
        ),
        (
            TurnError::Provider(ProviderError::ContextLimit {
                limit_tokens: Some(200_000),
                used_tokens: Some(210_000),
            }),
            TurnRecovery::Compact,
        ),
        (
            TurnError::Provider(ProviderError::Auth {
                provider: "test".to_owned(),
                source: None,
            }),
            TurnRecovery::Pause,
        ),
        (TurnError::EventConsumerClosed, TurnRecovery::Pause),
        (
            TurnError::StagnantToolLoop {
                count: 3,
                tool: "plan_get".to_owned(),
            },
            TurnRecovery::Pause,
        ),
        (
            TurnError::Provider(ProviderError::Fatal {
                status: Some(400),
                source: None,
            }),
            TurnRecovery::Fail,
        ),
        (
            TurnError::Provider(ProviderError::Refused {
                provider: "test".to_owned(),
                provider_text: None,
            }),
            TurnRecovery::Fail,
        ),
        (
            TurnError::AgentNotFound {
                agent: "missing".to_owned(),
            },
            TurnRecovery::Fail,
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.recovery(), expected, "{error}");
    }
}

#[tokio::test(start_paused = true)]
async fn first_provider_attempt_is_not_part_of_the_retry_recovery_budget() {
    let started = tokio::time::Instant::now();
    let result = retry_provider(
        policy(PROVIDER_RETRY_MAX_ATTEMPTS),
        |_| async {
            tokio::time::sleep(Duration::from_secs(181)).await;
            Ok::<_, ProviderError>("completed")
        },
        |_| ready(Ok::<(), std::io::Error>(())),
    )
    .await;

    assert_eq!(
        result.expect("a healthy first attempt must not be mistaken for retry recovery"),
        "completed"
    );
    assert_eq!(started.elapsed(), Duration::from_secs(181));
}

#[tokio::test(start_paused = true)]
async fn recovery_deadline_interrupts_an_active_replay() {
    let attempts = Rc::new(Cell::new(0_u32));
    let seen = Rc::clone(&attempts);
    let started = tokio::time::Instant::now();
    let result = retry_provider(
        policy(PROVIDER_RETRY_MAX_ATTEMPTS),
        move |attempt| {
            seen.set(attempt);
            async move {
                if attempt == 1 {
                    return Err(ProviderError::Transient {
                        status: None,
                        source: None,
                    });
                }
                tokio::time::sleep(Duration::from_secs(181)).await;
                Ok::<_, ProviderError>("recovered")
            }
        },
        |_| ready(Ok::<(), std::io::Error>(())),
    )
    .await;

    assert!(matches!(
        result,
        Err(ProviderRetryError::DeadlineExceeded {
            attempt: 2,
            elapsed
        }) if elapsed == Duration::from_secs(180)
    ));
    assert_eq!(attempts.get(), 2);
    assert_eq!(started.elapsed(), Duration::from_secs(180));
}

#[tokio::test(start_paused = true)]
async fn recovery_deadline_prevents_starting_another_replay() {
    let attempts = Rc::new(Cell::new(0_u32));
    let seen = Rc::clone(&attempts);
    let started = tokio::time::Instant::now();
    let result = retry_provider(
        policy(PROVIDER_RETRY_MAX_ATTEMPTS),
        move |attempt| {
            seen.set(attempt);
            async move {
                if attempt > 1 {
                    tokio::time::sleep(Duration::from_secs(181)).await;
                }
                Err::<(), _>(ProviderError::Transient {
                    status: None,
                    source: None,
                })
            }
        },
        |_| ready(Ok::<(), std::io::Error>(())),
    )
    .await;

    assert!(matches!(
        result,
        Err(ProviderRetryError::DeadlineExceeded {
            attempt: 2,
            elapsed
        }) if elapsed == Duration::from_secs(180)
    ));
    assert_eq!(
        attempts.get(),
        2,
        "the expired budget must reject attempt 3"
    );
    assert_eq!(started.elapsed(), Duration::from_secs(180));
}

fn assert_budget<F>(budget: RecoveryBudget, limit: u32, mut record: F)
where
    F: FnMut() -> Result<RecoveryAttempt, RetryError>,
{
    for expected_attempt in 1..=limit {
        assert_eq!(
            record().expect("attempt within the documented budget"),
            RecoveryAttempt {
                attempt: expected_attempt,
                max: limit,
            }
        );
    }

    assert_eq!(
        record().expect_err("the next attempt must exhaust the budget"),
        RetryError::BudgetExhausted {
            budget,
            attempts: limit,
        }
    );
}

#[test]
fn retry_context_limit_budget_allows_exactly_five_compaction_retries() {
    let mut budgets = RecoveryBudgets::default();

    assert_budget(
        RecoveryBudget::ContextLimit,
        MAX_CONTEXT_LIMIT_RETRIES,
        || budgets.record_context_limit_retry(),
    );
}

#[test]
fn retry_incomplete_continuation_budget_allows_exactly_three_attempts() {
    let mut budgets = RecoveryBudgets::default();

    assert_budget(
        RecoveryBudget::IncompleteContinuation,
        MAX_INCOMPLETE_CONTINUATION_ATTEMPTS,
        || budgets.record_incomplete_continuation(),
    );
}

#[test]
fn retry_empty_post_tool_budget_allows_exactly_five_attempts_then_names_the_budget() {
    let mut budgets = RecoveryBudgets::default();

    assert_budget(
        RecoveryBudget::EmptyPostToolContinuation,
        MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS,
        || budgets.record_empty_post_tool_continuation(),
    );

    let error = budgets
        .record_empty_post_tool_continuation()
        .expect_err("repeated empty responses must remain terminal");
    assert_eq!(
        error.to_string(),
        "empty-post-tool-continuation attempts budget exhausted after 5 attempts"
    );
    eprintln!(
        "FAILURE_QA empty_responses={} error={error}",
        MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS + 1
    );
}

#[derive(Debug, PartialEq, Eq)]
enum Trace {
    Operation(u32),
    Rollback { attempt: u32, max: u32 },
    Sleep(Duration),
}

#[tokio::test]
async fn retry_rollback_is_emitted_before_every_replayed_provider_attempt() {
    let script = Rc::new(RefCell::new(VecDeque::from([
        Err(ProviderError::Transient {
            status: Some(502),
            source: None,
        }),
        Err(ProviderError::Transient {
            status: Some(503),
            source: None,
        }),
        Ok("complete"),
    ])));
    let trace = Rc::new(RefCell::new(Vec::new()));

    let result = retry_provider_with_sleep(
        policy(3),
        {
            let script = Rc::clone(&script);
            let trace = Rc::clone(&trace);
            move |attempt| {
                trace.borrow_mut().push(Trace::Operation(attempt));
                ready(
                    script
                        .borrow_mut()
                        .pop_front()
                        .expect("one scripted result per attempt"),
                )
            }
        },
        {
            let trace = Rc::clone(&trace);
            move |event| {
                let StreamEvent::RetryRollback { attempt, max } = event else {
                    panic!("retry executor emitted a non-rollback event");
                };
                trace.borrow_mut().push(Trace::Rollback { attempt, max });
                ready(Ok::<(), std::io::Error>(()))
            }
        },
        {
            let trace = Rc::clone(&trace);
            move |delay| {
                trace.borrow_mut().push(Trace::Sleep(delay));
                ready(())
            }
        },
    )
    .await
    .expect("third provider attempt succeeds");

    assert_eq!(result, "complete");
    assert_eq!(
        *trace.borrow(),
        [
            Trace::Operation(1),
            Trace::Rollback { attempt: 2, max: 3 },
            Trace::Sleep(Duration::from_secs(2)),
            Trace::Operation(2),
            Trace::Rollback { attempt: 3, max: 3 },
            Trace::Sleep(Duration::from_secs(4)),
            Trace::Operation(3),
        ]
    );
}

#[tokio::test]
async fn retry_transient_503_then_completes_happy_qa() {
    let attempts = Rc::new(Cell::new(0_u32));
    let rollbacks = Rc::new(RefCell::new(Vec::new()));

    let result = retry_provider_with_sleep(
        policy(2),
        {
            let attempts = Rc::clone(&attempts);
            move |_| {
                let current = attempts.get() + 1;
                attempts.set(current);
                ready(if current == 1 {
                    Err(ProviderError::Transient {
                        status: Some(503),
                        source: None,
                    })
                } else {
                    Ok("turn completed")
                })
            }
        },
        {
            let rollbacks = Rc::clone(&rollbacks);
            move |event| {
                rollbacks.borrow_mut().push(event);
                ready(Ok::<(), std::io::Error>(()))
            }
        },
        |_| ready(()),
    )
    .await
    .expect("transient 503 is retried");

    assert_eq!(result, "turn completed");
    assert_eq!(attempts.get(), 2);
    assert_eq!(
        *rollbacks.borrow(),
        [StreamEvent::RetryRollback { attempt: 2, max: 2 }]
    );
    eprintln!(
        "HAPPY_QA transient_status=503 attempts={} rollback={:?} result={result}",
        attempts.get(),
        rollbacks.borrow()
    );
}

#[tokio::test]
async fn retry_rate_limit_uses_the_carried_retry_after_duration() {
    let script = Rc::new(RefCell::new(VecDeque::from([
        Err(ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        }),
        Ok("complete"),
    ])));
    let sleeps = Rc::new(RefCell::new(Vec::new()));

    let result = retry_provider_with_sleep(
        policy(2),
        {
            let script = Rc::clone(&script);
            move |_| {
                ready(
                    script
                        .borrow_mut()
                        .pop_front()
                        .expect("one scripted result per attempt"),
                )
            }
        },
        |_| ready(Ok::<(), std::io::Error>(())),
        {
            let sleeps = Rc::clone(&sleeps);
            move |delay| {
                sleeps.borrow_mut().push(delay);
                ready(())
            }
        },
    )
    .await
    .expect("rate limited request succeeds after retry");

    assert_eq!(result, "complete");
    assert_eq!(*sleeps.borrow(), [Duration::from_secs(30)]);
}

#[tokio::test]
async fn durable_backoff_observation_precedes_every_wait_or_wake() {
    let trace = Rc::new(RefCell::new(Vec::new()));
    let result = retry_provider_with_wake_observed(
        policy(2),
        {
            let trace = Rc::clone(&trace);
            move |_| {
                trace.borrow_mut().push("provider");
                ready(Err::<&'static str, _>(ProviderError::Transient {
                    status: Some(503),
                    source: None,
                }))
            }
        },
        {
            let trace = Rc::clone(&trace);
            move |_| {
                trace.borrow_mut().push("rollback");
                ready(Ok::<(), std::io::Error>(()))
            }
        },
        {
            let trace = Rc::clone(&trace);
            move || {
                trace.borrow_mut().push("wake");
                ready("interrupted")
            }
        },
        {
            let trace = Rc::clone(&trace);
            move |observation| {
                trace.borrow_mut().push(match observation {
                    ProviderAttemptObservation::Started { .. } => "started",
                    ProviderAttemptObservation::Finished { .. } => "finished",
                    ProviderAttemptObservation::DeadlineExceeded { .. } => "deadline",
                    ProviderAttemptObservation::BackoffScheduled { .. } => "backoff",
                });
                Ok::<(), std::convert::Infallible>(())
            }
        },
    )
    .await
    .expect("wake ends retry recovery");

    assert_eq!(result, "interrupted");
    assert_eq!(
        *trace.borrow(),
        [
            "started", "provider", "finished", "rollback", "backoff", "wake"
        ],
        "the durable deadline hook must run after rollback and before any wait can be interrupted"
    );
}

#[test]
fn retry_local_delay_is_positive_capped_and_jittered() {
    let policy = ProviderRetryPolicy::new(NonZeroU32::new(3).expect("non-zero"));
    let transient = ProviderError::Transient {
        status: Some(503),
        source: None,
    };
    assert_eq!(
        policy.delay_after(1, &transient, 0),
        Duration::from_millis(1_600)
    );
    assert_eq!(
        policy.delay_after(1, &transient, u64::MAX),
        Duration::from_millis(2_400)
    );
    assert_eq!(
        policy.delay_after(5, &transient, u64::MAX),
        RETRY_MAX_DELAY_WITHOUT_PROVIDER,
        "positive jitter must remain capped"
    );

    let peer = ProviderError::RateLimited {
        retry_after: Some(Duration::from_secs(7)),
    };
    assert_eq!(
        policy.delay_after(1, &peer, 0),
        Duration::from_secs(7),
        "negative jitter must not shorten Retry-After"
    );
    assert_eq!(
        policy.delay_after(1, &peer, u64::MAX),
        Duration::from_secs(7),
        "positive jitter must not alter Retry-After"
    );
}

#[test]
fn retry_policy_rejects_invalid_timing() {
    let attempts = NonZeroU32::new(3).expect("non-zero");
    assert_eq!(
        ProviderRetryPolicy::with_timing(
            attempts,
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(2),
            0,
        )
        .expect_err("zero elapsed budget is invalid"),
        ProviderRetryPolicyError::ZeroDuration
    );
    assert_eq!(
        ProviderRetryPolicy::with_timing(
            attempts,
            Duration::from_secs(3),
            Duration::from_secs(2),
            Duration::from_secs(1),
            0,
        )
        .expect_err("max below initial is invalid"),
        ProviderRetryPolicyError::MaxDelayBeforeInitial
    );
    assert_eq!(
        ProviderRetryPolicy::with_timing(
            attempts,
            Duration::from_secs(3),
            Duration::from_secs(1),
            Duration::from_secs(2),
            101,
        )
        .expect_err("jitter above one hundred is invalid"),
        ProviderRetryPolicyError::JitterPercentOutOfRange { actual: 101 }
    );
}

#[tokio::test]
async fn retry_without_provider_delay_uses_oracle_backoff_capped_at_thirty_seconds() {
    let script = Rc::new(RefCell::new(VecDeque::from([
        Err(ProviderError::Transient {
            status: Some(503),
            source: None,
        }),
        Err(ProviderError::Transient {
            status: Some(503),
            source: None,
        }),
        Err(ProviderError::Transient {
            status: Some(503),
            source: None,
        }),
        Err(ProviderError::Transient {
            status: Some(503),
            source: None,
        }),
        Err(ProviderError::Transient {
            status: Some(503),
            source: None,
        }),
        Ok("complete"),
    ])));
    let sleeps = Rc::new(RefCell::new(Vec::new()));

    retry_provider_with_sleep(
        policy(6),
        {
            let script = Rc::clone(&script);
            move |_| {
                ready(
                    script
                        .borrow_mut()
                        .pop_front()
                        .expect("one scripted result per attempt"),
                )
            }
        },
        |_| ready(Ok::<(), std::io::Error>(())),
        {
            let sleeps = Rc::clone(&sleeps);
            move |delay| {
                sleeps.borrow_mut().push(delay);
                ready(())
            }
        },
    )
    .await
    .expect("sixth attempt succeeds");

    assert_eq!(
        *sleeps.borrow(),
        [
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(30),
        ]
    );
}

#[tokio::test]
async fn retry_never_replays_non_retryable_provider_errors() {
    let errors = [
        ProviderError::ContextLimit {
            limit_tokens: Some(200_000),
            used_tokens: Some(210_000),
        },
        ProviderError::Auth {
            provider: "test".to_owned(),
            source: None,
        },
        ProviderError::Refused {
            provider: "test".to_owned(),
            provider_text: Some("refused".to_owned()),
        },
        ProviderError::Protocol {
            code: ProviderProtocolFailure::InvalidUpstreamReasoning,
            source: None,
        },
        ProviderError::Fatal {
            status: Some(400),
            source: None,
        },
    ];

    for error in errors {
        let attempts = Rc::new(Cell::new(0_u32));
        let rollback_count = Rc::new(Cell::new(0_u32));
        let sleep_count = Rc::new(Cell::new(0_u32));
        let error = Rc::new(RefCell::new(Some(error)));

        let result = retry_provider_with_sleep(
            policy(3),
            {
                let attempts = Rc::clone(&attempts);
                let error = Rc::clone(&error);
                move |_| {
                    attempts.set(attempts.get() + 1);
                    ready(Err::<(), _>(
                        error.borrow_mut().take().expect("one provider error"),
                    ))
                }
            },
            {
                let rollback_count = Rc::clone(&rollback_count);
                move |_| {
                    rollback_count.set(rollback_count.get() + 1);
                    ready(Ok::<(), std::io::Error>(()))
                }
            },
            {
                let sleep_count = Rc::clone(&sleep_count);
                move |_| {
                    sleep_count.set(sleep_count.get() + 1);
                    ready(())
                }
            },
        )
        .await;

        let ProviderRetryError::Provider(source) = result.expect_err("error is terminal") else {
            panic!("non-retryable error was not returned directly");
        };
        assert!(!source.is_retryable());
        assert_eq!(attempts.get(), 1);
        assert_eq!(rollback_count.get(), 0);
        assert_eq!(sleep_count.get(), 0);
    }
}

#[tokio::test]
async fn retry_provider_attempt_limit_returns_a_typed_error() {
    let attempts = Rc::new(Cell::new(0_u32));
    let rollback_count = Rc::new(Cell::new(0_u32));

    let result = retry_provider_with_sleep(
        policy(3),
        {
            let attempts = Rc::clone(&attempts);
            move |_| {
                attempts.set(attempts.get() + 1);
                ready(Err::<(), _>(ProviderError::Transient {
                    status: Some(503),
                    source: None,
                }))
            }
        },
        {
            let rollback_count = Rc::clone(&rollback_count);
            move |_| {
                rollback_count.set(rollback_count.get() + 1);
                ready(Ok::<(), std::io::Error>(()))
            }
        },
        |_| ready(()),
    )
    .await;

    let ProviderRetryError::AttemptsExhausted { attempts, source } =
        result.expect_err("provider retry limit must end the operation")
    else {
        panic!("retryable failures must exhaust with the typed attempts error");
    };
    assert_eq!(attempts, 3);
    assert!(source.is_retryable());
    assert_eq!(rollback_count.get(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_retry_after_beyond_the_deadline_is_surfaced_not_replaced_by_a_local_delay() {
    let attempts = Rc::new(Cell::new(0_u32));
    let seen = Rc::clone(&attempts);
    let started = tokio::time::Instant::now();
    let peer_delay = PROVIDER_RETRY_MAX_ELAPSED + Duration::from_secs(220);
    let result = retry_provider(
        policy(PROVIDER_RETRY_MAX_ATTEMPTS),
        move |attempt| {
            seen.set(attempt);
            async move {
                Err::<(), _>(ProviderError::RateLimited {
                    retry_after: Some(peer_delay),
                })
            }
        },
        |_| ready(Ok::<(), std::io::Error>(())),
    )
    .await;

    assert!(
        matches!(
            result,
            Err(ProviderRetryError::RetryAfterBeyondDeadline {
                attempt: 1,
                retry_after,
                source: ProviderError::RateLimited { retry_after: Some(named) },
                ..
            }) if retry_after == peer_delay && named == peer_delay
        ),
        "the peer's delay and its typed error must survive the refused replay: {result:?}"
    );
    assert_eq!(
        attempts.get(),
        1,
        "no replay may start against a deadline the peer's delay already exceeds"
    );
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "recovery neither sleeps the local delay nor waits for the deadline"
    );
}

#[tokio::test(start_paused = true)]
async fn a_local_delay_beyond_the_deadline_still_reports_the_deadline() {
    let result = retry_provider(
        ProviderRetryPolicy::with_timing(
            NonZeroU32::new(PROVIDER_RETRY_MAX_ATTEMPTS).expect("non-zero"),
            Duration::from_secs(1),
            RETRY_INITIAL_DELAY,
            RETRY_MAX_DELAY_WITHOUT_PROVIDER,
            0,
        )
        .expect("one-second recovery budget"),
        |_| async {
            Err::<(), _>(ProviderError::Transient {
                status: Some(503),
                source: None,
            })
        },
        |_| ready(Ok::<(), std::io::Error>(())),
    )
    .await;

    assert!(
        matches!(
            result,
            Err(ProviderRetryError::DeadlineExceeded { attempt: 1, .. })
        ),
        "without a peer delay there is nothing to surface but the deadline: {result:?}"
    );
}
