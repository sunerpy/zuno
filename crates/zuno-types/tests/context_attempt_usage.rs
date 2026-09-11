use zuno_types::context_usage::{
    ContextRequestIdentity, ContextTokenAccounting, ContextUsageCounters, ContextUsageTracker,
};

fn request(sequence: u64, attempt: u32) -> ContextRequestIdentity {
    serde_json::from_value(serde_json::json!({
        "requestId": format!("request-{sequence}"),
        "requestSequence": sequence,
        "attempt": attempt,
        "contextEpoch": 0,
        "providerId": "synthetic",
        "modelId": "synthetic",
        "source": "main",
        "turnId": "synthetic-turn",
        "timeStarted": sequence,
    }))
    .unwrap()
}

#[test]
fn failed_partial_attempt_keeps_observed_consumption_while_context_rolls_back() {
    let mut tracker = ContextUsageTracker::new("ses_attempts");
    let first = request(1, 1);
    tracker.start_request(first.clone(), Some(80), Some(0), Some(200_000), 1);
    tracker.observe_usage(
        &first,
        ContextUsageCounters {
            input_tokens: Some(100),
            output_tokens: Some(10),
            accounting: ContextTokenAccounting::CacheInsideInput,
            ..ContextUsageCounters::default()
        },
        2,
    );
    tracker.commit_request(&first, 3);
    let second = request(2, 1);
    tracker.start_request(second.clone(), Some(90), Some(7), Some(200_000), 4);
    tracker.observe_usage(
        &second,
        ContextUsageCounters {
            input_tokens: Some(149_501),
            accounting: ContextTokenAccounting::CacheInsideInput,
            ..ContextUsageCounters::default()
        },
        5,
    );
    assert!(tracker.rollback_request(&second, 2, 6));
    assert_eq!(tracker.snapshot().used_tokens, Some(117));
    assert_eq!(tracker.snapshot().cumulative_usage.total(), 149_611);
    assert!(!tracker.snapshot().cumulative_known);
    assert!(!tracker.rollback_request(&second, 2, 7));
    assert_eq!(tracker.snapshot().cumulative_usage.total(), 149_611);
    let retry = request(2, 2);
    tracker.observe_usage(
        &retry,
        ContextUsageCounters {
            input_tokens: Some(150_001),
            output_tokens: Some(9),
            accounting: ContextTokenAccounting::CacheInsideInput,
            ..ContextUsageCounters::default()
        },
        8,
    );
    tracker.commit_request(&retry, 9);
    assert_eq!(tracker.snapshot().used_tokens, Some(150_010));
    assert_eq!(tracker.snapshot().cumulative_usage.total(), 299_621);
    assert!(!tracker.snapshot().cumulative_known);
    tracker.validate().unwrap();
}
