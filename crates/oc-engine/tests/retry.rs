use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::ready;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Duration;

use oc_engine::retry::{
    MAX_CONTEXT_LIMIT_RETRIES, MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS,
    MAX_INCOMPLETE_CONTINUATION_ATTEMPTS, ProviderRetryError, ProviderRetryPolicy, RecoveryAttempt,
    RecoveryBudget, RecoveryBudgets, RetryError, retry_provider_with_sleep,
};
use oc_error::ProviderError;
use oc_llm::event::StreamEvent;

fn policy(max_attempts: u32) -> ProviderRetryPolicy {
    ProviderRetryPolicy::new(NonZeroU32::new(max_attempts).expect("non-zero test policy"))
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
