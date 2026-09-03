use std::time::Duration;

use crate::{
    GoalBlockReason, GoalFailureDisposition, GoalPauseReason, GoalRetryPolicy,
    GoalRetryPolicyError, GoalRetryReason, GoalStatus, GoalStore, GoalTerminalFailure, ModelStatus,
    SystemStatus,
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
fn transient_tool_retry_reason_has_a_stable_persisted_discriminator() {
    assert_eq!(GoalRetryReason::ToolTransient.as_str(), "tool_transient");
    assert_eq!(
        GoalRetryReason::parse("tool_transient"),
        Some(GoalRetryReason::ToolTransient)
    );
    assert_eq!(GoalRetryReason::parse("tool_uncertain"), None);
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
        .record_terminal_failure_at(
            SESSION,
            GoalTerminalFailure::Pause(GoalPauseReason::Authentication),
            2_000,
            0,
        )
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

    let late_failure = continuation
        .record_terminal_failure_at(
            SESSION,
            GoalTerminalFailure::Block(GoalBlockReason::ProviderFatal { status: Some(400) }),
            2_500,
            0,
        )
        .expect("ignore a late permanent failure after pause");
    assert_eq!(late_failure, GoalFailureDisposition::NoActiveGoal);
    assert_eq!(
        store
            .goal(SESSION)
            .expect("read paused goal")
            .expect("goal")
            .status,
        GoalStatus::Paused
    );

    store
        .set_status_as_system(SESSION, SystemStatus::Active)
        .expect("resume goal");
    let blocked = continuation
        .record_terminal_failure_at(
            SESSION,
            GoalTerminalFailure::Block(GoalBlockReason::ProviderFatal { status: Some(400) }),
            3_000,
            0,
        )
        .expect("block permanent failure");
    assert!(matches!(blocked, GoalFailureDisposition::Blocked(_)));
    let blocked = store.goal(SESSION).expect("read goal").expect("goal");
    assert_eq!(blocked.status, GoalStatus::Blocked);
    assert_eq!(
        blocked.blocked_reason.as_deref(),
        Some("provider_fatal(status=400): the provider returned a non-recoverable failure")
    );
    let history = store.history(SESSION).expect("read goal history");
    assert_eq!(
        history
            .last()
            .and_then(|entry| entry.goal.blocked_reason.as_deref()),
        blocked.blocked_reason.as_deref()
    );
    assert_eq!(store.retry_state(SESSION).expect("read retry"), None);
}

#[test]
fn a_busy_database_schedules_a_persisted_retry_and_any_other_database_failure_blocks() {
    let spill = tempfile::tempdir().expect("create spill directory");
    let store = std::sync::Arc::new(
        GoalStore::open_memory(spill.path().to_owned()).expect("open goal store"),
    );
    store
        .create_goal(SESSION, "outlast a contended database", None)
        .expect("create goal");
    let continuation = crate::GoalContinuation::new(
        std::sync::Arc::clone(&store),
        zuno_engine::status::SessionRunRegistry::new(),
    )
    .with_retry_policy(policy());

    let busy = GoalTerminalFailure::from_db_error(&zuno_error::DbError::Busy {
        retry_after: Some(Duration::from_secs(7)),
    });
    assert_eq!(
        busy,
        GoalTerminalFailure::Retry {
            reason: GoalRetryReason::DatabaseBusy,
            retry_after: Some(Duration::from_secs(7)),
        }
    );
    let GoalFailureDisposition::RetryScheduled(retry) = continuation
        .record_terminal_failure_at(SESSION, busy, 1_000, 0)
        .expect("record the busy database")
    else {
        panic!("another writer holding the lock is a retry, not a block");
    };
    assert_eq!(retry.reason, GoalRetryReason::DatabaseBusy);
    assert_eq!(
        retry.retry_at_ms, 8_000,
        "the store's own delay becomes the deadline"
    );
    assert_eq!(
        store.retry_state(SESSION).expect("read retry state"),
        Some(retry),
        "the deadline is durable, so a restart reconstructs it from SQLite"
    );
    assert_eq!(
        store
            .goal(SESSION)
            .expect("read goal")
            .expect("goal")
            .status,
        GoalStatus::Active
    );

    let permanent = [
        zuno_error::DbError::Query {
            source: Box::new(std::io::Error::other("disk I/O error")),
        },
        zuno_error::DbError::NotFound {
            table: "goal".to_owned(),
            id: SESSION.to_owned(),
        },
        zuno_error::DbError::Conflict {
            table: "goal".to_owned(),
            id: SESSION.to_owned(),
            detail: "revision moved".to_owned(),
        },
        zuno_error::DbError::Decode {
            table: "goal".to_owned(),
            source: serde_json::from_str::<u8>("{}").unwrap_err(),
        },
        zuno_error::DbError::Open {
            path: std::path::PathBuf::from("zuno.db"),
            source: Box::new(std::io::Error::other("permission denied")),
        },
        zuno_error::DbError::Schema {
            format: 7,
            source: Box::new(std::io::Error::other("no such column")),
        },
        zuno_error::DbError::SchemaMismatch {
            expected: 7,
            observed: Some(9),
        },
    ];
    for error in &permanent {
        assert_eq!(
            GoalTerminalFailure::from_db_error(error),
            GoalTerminalFailure::Block(GoalBlockReason::DatabasePermanent),
            "{error}"
        );
    }
    let blocked = continuation
        .record_terminal_failure_at(
            SESSION,
            GoalTerminalFailure::from_db_error(&permanent[0]),
            2_000,
            0,
        )
        .expect("record the permanent failure");
    assert!(
        matches!(blocked, GoalFailureDisposition::Blocked(ref goal) if goal.status == GoalStatus::Blocked),
        "a statement failure no retry repairs blocks the goal: {blocked:?}"
    );
    assert!(
        store
            .retry_state(SESSION)
            .expect("read retry state")
            .is_none(),
        "a blocked goal schedules nothing"
    );
}

#[test]
fn a_peer_delay_beyond_the_turn_deadline_becomes_the_goal_delay_clamped_to_the_ceiling() {
    let policy = policy();
    assert_eq!(
        policy.delay(1, Some(Duration::from_secs(20)), 0),
        Duration::from_secs(20),
        "a peer delay under the ceiling is used as given, never the two-second local start"
    );
    assert_eq!(
        policy.delay(1, Some(Duration::from_secs(400)), 0),
        Duration::from_secs(30),
        "a peer delay above the ceiling is clamped to it, never replaced by an earlier local delay"
    );

    let spill = tempfile::tempdir().expect("create spill directory");
    let store = std::sync::Arc::new(
        GoalStore::open_memory(spill.path().to_owned()).expect("open goal store"),
    );
    store
        .create_goal(SESSION, "wait as long as the provider asks", None)
        .expect("create goal");
    let continuation = crate::GoalContinuation::new(
        std::sync::Arc::clone(&store),
        zuno_engine::status::SessionRunRegistry::new(),
    )
    .with_retry_policy(policy);
    let GoalFailureDisposition::RetryScheduled(retry) = continuation
        .record_terminal_failure_at(
            SESSION,
            GoalTerminalFailure::Retry {
                reason: GoalRetryReason::RateLimited,
                retry_after: Some(Duration::from_secs(400)),
            },
            1_000,
            0,
        )
        .expect("schedule the rate limit")
    else {
        panic!("a rate limit is a retry");
    };
    assert_eq!(retry.delay_ms, 30_000);
    assert_eq!(retry.retry_at_ms, 31_000);
}
