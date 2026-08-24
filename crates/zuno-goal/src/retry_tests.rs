use std::time::Duration;

use crate::{
    GoalFailureDisposition, GoalRetryPolicy, GoalRetryPolicyError, GoalRetryReason, GoalStatus,
    GoalStore, GoalTerminalFailure, ModelStatus, SystemStatus,
};

const SESSION: &str = "ses_retry";

fn policy() -> GoalRetryPolicy {
    GoalRetryPolicy::new(
        Duration::from_secs(2),
        Duration::from_secs(30),
        0,
        Duration::from_millis(250),
    )
    .expect("valid retry policy")
}

#[test]
fn retry_policy_is_exponential_and_caps_without_overflow() {
    let policy = policy();
    let delays = (1..=7)
        .map(|attempt| policy.delay(attempt, None, 0))
        .collect::<Vec<_>>();

    assert_eq!(
        delays,
        [
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ]
    );
    assert_eq!(
        policy.delay(u32::MAX, None, u64::MAX),
        Duration::from_secs(30)
    );
}

#[test]
fn retry_policy_applies_symmetric_jitter_after_selecting_a_delay() {
    let policy = GoalRetryPolicy::new(
        Duration::from_secs(10),
        Duration::from_secs(60),
        20,
        Duration::from_millis(250),
    )
    .expect("valid retry policy");

    assert_eq!(policy.delay(1, None, 0), Duration::from_secs(8));
    assert_eq!(policy.delay(1, None, u64::MAX), Duration::from_secs(12));
}

#[test]
fn provider_retry_after_never_schedules_an_earlier_retry() {
    let policy = GoalRetryPolicy::new(
        Duration::from_secs(2),
        Duration::from_secs(30),
        20,
        Duration::from_millis(250),
    )
    .expect("valid retry policy");

    assert_eq!(
        policy.delay(1, Some(Duration::from_secs(7)), 0),
        Duration::from_secs(7),
        "negative jitter must not retry before the peer's minimum"
    );
    assert_eq!(
        policy.delay(1, Some(Duration::from_secs(7)), u64::MAX),
        Duration::from_secs(7),
        "positive jitter must not alter an explicit peer delay"
    );
    assert_eq!(
        policy.delay(1, Some(Duration::from_secs(60)), 0),
        Duration::from_secs(30),
        "a peer delay above the local ceiling is capped, not discarded in favor of an earlier retry"
    );
    assert_eq!(
        policy.delay(1, Some(Duration::ZERO), 0),
        Duration::from_millis(1_600)
    );
}

#[test]
fn maximum_jitter_never_creates_a_zero_delay() {
    let policy = GoalRetryPolicy::new(
        Duration::from_secs(2),
        Duration::from_secs(30),
        100,
        Duration::from_millis(250),
    )
    .expect("valid retry policy");

    assert!(
        !policy.delay(1, None, 0).is_zero(),
        "a persisted retry must never become an immediate hot loop"
    );
}

#[test]
fn retry_policy_rejects_an_inverted_window_or_invalid_jitter() {
    assert_eq!(
        GoalRetryPolicy::new(
            Duration::from_secs(2),
            Duration::from_secs(1),
            0,
            Duration::from_millis(250),
        )
        .expect_err("max below initial is invalid"),
        GoalRetryPolicyError::MaxDelayBeforeInitial
    );
    assert_eq!(
        GoalRetryPolicy::new(
            Duration::from_secs(1),
            Duration::from_secs(2),
            101,
            Duration::from_millis(250),
        )
        .expect_err("jitter above one hundred percent is invalid"),
        GoalRetryPolicyError::JitterPercentOutOfRange { actual: 101 }
    );
}

#[test]
fn tool_retry_reasons_have_stable_persisted_discriminators() {
    for (reason, persisted) in [
        (GoalRetryReason::ToolTransient, "tool_transient"),
        (GoalRetryReason::ToolUncertain, "tool_uncertain"),
    ] {
        assert_eq!(reason.as_str(), persisted);
        assert_eq!(GoalRetryReason::parse(persisted), Some(reason));
    }
}

#[test]
fn retry_schedule_survives_reopen_and_increments_for_the_same_goal() {
    let database = tempfile::tempdir().expect("create database directory");
    let spill = tempfile::tempdir().expect("create spill directory");
    let path = database.path().join("goal-test.db");
    let first_goal_id;
    {
        let store = GoalStore::open_at(&path, spill.path().to_owned()).expect("open goal store");
        first_goal_id = store
            .create_goal(SESSION, "finish despite a provider outage", None)
            .expect("create goal")
            .goal_id;
        let first = store
            .schedule_retry(
                SESSION,
                GoalRetryReason::ProviderTransient,
                None,
                policy(),
                1_000,
                0,
            )
            .expect("schedule first retry")
            .expect("active goal");
        assert_eq!(first.goal_id, first_goal_id);
        assert_eq!(first.attempt, 1);
        assert_eq!(first.delay_ms, 2_000);
        assert_eq!(first.retry_at_ms, 3_000);
    }

    let store = GoalStore::open_at(&path, spill.path().to_owned()).expect("reopen goal store");
    let persisted = store
        .retry_state(SESSION)
        .expect("read retry state")
        .expect("retry state survived restart");
    assert_eq!(persisted.goal_id, first_goal_id);
    assert_eq!(persisted.attempt, 1);
    assert_eq!(persisted.reason, GoalRetryReason::ProviderTransient);

    let second = store
        .schedule_retry(
            SESSION,
            GoalRetryReason::RateLimited,
            None,
            policy(),
            4_000,
            0,
        )
        .expect("schedule second retry")
        .expect("active goal");
    assert_eq!(second.attempt, 2);
    assert_eq!(second.delay_ms, 4_000);
    assert_eq!(second.retry_at_ms, 8_000);
    assert_eq!(second.reason, GoalRetryReason::RateLimited);
}

#[test]
fn durable_context_compaction_phase_survives_restart_without_incrementing_backoff() {
    let database = tempfile::tempdir().expect("create database directory");
    let spill = tempfile::tempdir().expect("create spill directory");
    let path = database.path().join("goal-test.db");
    {
        let store = GoalStore::open_at(&path, spill.path().to_owned()).expect("open goal store");
        store
            .create_goal(SESSION, "finish after compaction", None)
            .expect("create goal");
        store
            .schedule_retry(
                SESSION,
                GoalRetryReason::ContextLimit,
                None,
                policy(),
                1_000,
                0,
            )
            .expect("schedule context retry")
            .expect("active goal");
        assert!(
            store
                .mark_retry_context_compacted(SESSION)
                .expect("mark compaction")
        );
    }

    let store = GoalStore::open_at(&path, spill.path().to_owned()).expect("reopen goal store");
    let retry = store
        .retry_state(SESSION)
        .expect("read retry")
        .expect("retry survives");
    assert_eq!(retry.reason, GoalRetryReason::ContextCompacted);
    assert_eq!(retry.attempt, 1);
    assert_eq!(retry.retry_at_ms, 3_000);
}

#[test]
fn progress_completion_pause_and_replacement_invalidate_stale_retry_state() {
    let spill = tempfile::tempdir().expect("create spill directory");
    let store = GoalStore::open_memory(spill.path().to_owned()).expect("open goal store");
    let first = store
        .create_goal(SESSION, "first objective", None)
        .expect("create goal");

    for clear in ["progress", "complete", "pause", "resume", "replacement"] {
        store
            .schedule_retry(
                SESSION,
                GoalRetryReason::ProviderTransient,
                None,
                policy(),
                1_000,
                0,
            )
            .unwrap_or_else(|error| panic!("{clear}: schedule retry: {error}"))
            .unwrap_or_else(|| panic!("{clear}: active goal disappeared"));
        match clear {
            "progress" => {
                store
                    .record_failure_signal(SESSION, None)
                    .expect("record progress");
            }
            "complete" => {
                store
                    .update_status_as_model(SESSION, ModelStatus::Complete)
                    .expect("complete goal");
                store
                    .replace_goal_as_system(SESSION, "first objective", None)
                    .expect("restore active goal");
            }
            "pause" => {
                store
                    .set_status_as_system(SESSION, SystemStatus::Paused)
                    .expect("pause goal");
                store
                    .set_status_as_system(SESSION, SystemStatus::Active)
                    .expect("restore active goal");
            }
            "resume" => {
                store
                    .set_status_as_system(SESSION, SystemStatus::Paused)
                    .expect("pause goal");
                store
                    .set_status_as_system(SESSION, SystemStatus::Active)
                    .expect("resume goal");
            }
            "replacement" => {
                store
                    .replace_goal_as_system(SESSION, "replacement objective", None)
                    .expect("replace goal");
            }
            _ => unreachable!(),
        }
        assert_eq!(
            store.retry_state(SESSION).expect("read retry state"),
            None,
            "{clear} left a stale retry"
        );
    }

    let current = store
        .goal(SESSION)
        .expect("read goal")
        .expect("goal exists");
    assert_eq!(current.status, GoalStatus::Active);
    assert_ne!(current.goal_id, first.goal_id);
}

#[test]
fn terminal_failure_disposition_never_retries_auth_or_permanent_failures() {
    let spill = tempfile::tempdir().expect("create spill directory");
    let store = std::sync::Arc::new(
        GoalStore::open_memory(spill.path().to_owned()).expect("open goal store"),
    );
    store
        .create_goal(SESSION, "finish safely", None)
        .expect("create goal");
    let continuation = crate::GoalContinuation::new(
        std::sync::Arc::clone(&store),
        zuno_engine::status::SessionRunRegistry::new(),
    )
    .with_retry_policy(policy());

    let retry = continuation
        .record_terminal_failure_at(
            SESSION,
            GoalTerminalFailure::Retry {
                reason: GoalRetryReason::ProviderTransient,
                retry_after: None,
            },
            1_000,
            0,
        )
        .expect("schedule recoverable failure");
    assert!(matches!(
        retry,
        GoalFailureDisposition::RetryScheduled(ref state) if state.attempt == 1
    ));
    assert_eq!(
        store
            .goal(SESSION)
            .expect("read goal")
            .expect("goal")
            .status,
        GoalStatus::Active
    );

    let paused = continuation
        .record_terminal_failure_at(SESSION, GoalTerminalFailure::Pause, 2_000, 0)
        .expect("pause after authentication failure");
    assert!(matches!(paused, GoalFailureDisposition::Paused(_)));
    assert_eq!(
        store
            .goal(SESSION)
            .expect("read goal")
            .expect("goal")
            .status,
        GoalStatus::Paused
    );
    assert_eq!(store.retry_state(SESSION).expect("read retry"), None);

    store
        .set_status_as_system(SESSION, SystemStatus::Active)
        .expect("resume goal");
    let blocked = continuation
        .record_terminal_failure_at(SESSION, GoalTerminalFailure::Block, 3_000, 0)
        .expect("block permanent failure");
    assert!(matches!(blocked, GoalFailureDisposition::Blocked(_)));
    assert_eq!(
        store
            .goal(SESSION)
            .expect("read goal")
            .expect("goal")
            .status,
        GoalStatus::Blocked
    );
    assert_eq!(store.retry_state(SESSION).expect("read retry"), None);
}
