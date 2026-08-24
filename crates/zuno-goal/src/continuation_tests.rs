use super::*;
use zuno_engine::compaction::select_boundary;
use zuno_llm::event::RequestContentBlock;

struct Fixture {
    store: Arc<GoalStore>,
    runs: SessionRunRegistry,
    continuation: GoalContinuation,
    _spill: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let spill = tempfile::tempdir().expect("create spill directory");
        let store =
            Arc::new(GoalStore::open_memory(spill.path().to_owned()).expect("open goal store"));
        let runs = SessionRunRegistry::new();
        let continuation = GoalContinuation::new(Arc::clone(&store), runs.clone());
        Self {
            store,
            runs,
            continuation,
            _spill: spill,
        }
    }

    fn create(&self, objective: &str) -> Goal {
        self.store
            .create_goal("ses_goal", objective, Some(10_000))
            .expect("create goal")
    }
}

fn text(entry: &TranscriptEntry) -> &str {
    match entry.message.content.as_slice() {
        [RequestContentBlock::Text { text }] => text,
        other => panic!("expected one text block, got {other:?}"),
    }
}

#[test]
fn goal_is_regenerated_from_sql_after_compaction_discards_old_context() {
    let fixture = Fixture::new();
    fixture.create("old objective");
    let old = fixture
        .continuation
        .injection("ses_goal")
        .expect("render injection")
        .expect("active goal injection");
    assert!(old.synthetic);
    assert!(!old.preserve_initial);

    let entries = vec![
        TranscriptEntry::new("system", Message::new(Role::System, "system"), 1),
        old,
        TranscriptEntry::new("user", Message::new(Role::User, "latest"), 1),
        TranscriptEntry::new("assistant", Message::new(Role::Assistant, "answer"), 1),
    ];
    let boundary = select_boundary(&entries, 1, 2).expect("select compaction boundary");
    assert!(
        boundary.retained_from > 1,
        "old goal context must be compacted away"
    );

    fixture
        .store
        .update_objective("ses_goal", "new objective from SQL")
        .expect("update objective");
    let fresh = fixture
        .continuation
        .injection("ses_goal")
        .expect("render fresh injection")
        .expect("active goal injection");
    assert!(text(&fresh).contains("new objective from SQL"));
    assert!(!text(&fresh).contains("old objective"));
    assert!(text(&fresh).contains("Treat completion as unproven"));
    assert!(text(&fresh).contains("Do not substitute a narrower"));
}

#[test]
fn unknown_usage_is_explicit_in_goal_context() {
    let fixture = Fixture::new();
    fixture.create("keep accounting honest");
    let goal = fixture
        .store
        .record_usage("ses_goal", 1_200, 3, false)
        .expect("record unknown usage")
        .expect("goal exists");

    let rendered = render_goal_context(&goal);
    assert!(rendered.contains("Tokens used: unknown (confirmed lower bound: 1200)"));
    assert!(rendered.contains("Tokens remaining: unknown"));
    assert!(!rendered.contains("Tokens remaining: 8800"));
}

#[test]
fn idle_active_goal_prepares_exactly_one_continuation() {
    let fixture = Fixture::new();
    fixture.create("continue once");
    let first = fixture
        .continuation
        .prepare_if_idle("ses_goal", GoalTurnMode::Work, QueuedUserInput::Absent)
        .expect("prepare continuation");
    let ContinuationAttempt::Prepared(prepared) = first else {
        panic!("active idle goal should prepare");
    };
    assert_eq!(fixture.runs.status("ses_goal"), SessionStatus::Busy);
    assert!(text(prepared.entry()).contains("continue once"));

    assert!(matches!(
        fixture
            .continuation
            .prepare_if_idle("ses_goal", GoalTurnMode::Work, QueuedUserInput::Absent)
            .expect("second attempt"),
        ContinuationAttempt::Suppressed(ContinuationSuppression::ConcurrentStart)
    ));
    drop(prepared);
    assert_eq!(fixture.runs.status("ses_goal"), SessionStatus::Idle);
}

#[test]
fn prepared_continuation_is_rejected_after_the_goal_is_replaced() {
    let fixture = Fixture::new();
    let original = fixture.create("original objective");
    let prepared = fixture
        .continuation
        .prepare_if_idle("ses_goal", GoalTurnMode::Work, QueuedUserInput::Absent)
        .expect("prepare original goal");
    let ContinuationAttempt::Prepared(prepared) = prepared else {
        panic!("active goal should prepare");
    };
    assert!(
        fixture
            .continuation
            .is_current(&prepared)
            .expect("validate original goal")
    );

    let replacement = fixture
        .store
        .replace_goal_as_system("ses_goal", "replacement objective", Some(10_000))
        .expect("replace the captured goal");
    assert_ne!(replacement.goal_id, original.goal_id);
    assert!(
        !fixture
            .continuation
            .is_current(&prepared)
            .expect("re-read replacement"),
        "a continuation captured before replacement must not run afterward"
    );
}

#[test]
fn running_plan_and_queued_input_each_suppress_automatic_work() {
    let fixture = Fixture::new();
    fixture.create("guarded continuation");
    let running = fixture.runs.begin_turn("ses_goal").expect("begin turn");
    assert!(matches!(
        fixture
            .continuation
            .prepare_if_idle("ses_goal", GoalTurnMode::Work, QueuedUserInput::Absent)
            .expect("running guard"),
        ContinuationAttempt::Suppressed(ContinuationSuppression::RunningTurn)
    ));
    drop(running);

    assert!(matches!(
        fixture
            .continuation
            .prepare_if_idle("ses_goal", GoalTurnMode::Plan, QueuedUserInput::Absent)
            .expect("plan guard"),
        ContinuationAttempt::Suppressed(ContinuationSuppression::PlanMode)
    ));
    assert!(matches!(
        fixture
            .continuation
            .prepare_if_idle("ses_goal", GoalTurnMode::Work, QueuedUserInput::Present)
            .expect("queued-input guard"),
        ContinuationAttempt::Suppressed(ContinuationSuppression::QueuedUserInput)
    ));
}

#[test]
fn resume_deferral_suppresses_exactly_one_eligible_continuation() {
    let fixture = Fixture::new();
    fixture.create("resume safely");
    assert!(
        fixture
            .continuation
            .defer_once("ses_goal")
            .expect("defer once")
    );
    assert!(matches!(
        fixture
            .continuation
            .prepare_if_idle("ses_goal", GoalTurnMode::Work, QueuedUserInput::Absent)
            .expect("deferred attempt"),
        ContinuationAttempt::Suppressed(ContinuationSuppression::DeferredOnce)
    ));
    assert!(matches!(
        fixture
            .continuation
            .prepare_if_idle("ses_goal", GoalTurnMode::Work, QueuedUserInput::Absent)
            .expect("second attempt"),
        ContinuationAttempt::Prepared(_)
    ));
}

#[test]
fn blocking_requires_three_identical_consecutive_turns_and_progress_resets_it() {
    let fixture = Fixture::new();
    fixture.create("audit blockers");
    let first = fixture
        .continuation
        .record_turn_outcome("ses_goal", GoalTurnOutcome::Blocking("missing credential"))
        .expect("first blocker");
    assert!(matches!(
        first,
        BlockedAudit::Pending(FailureStreak {
            consecutive_turns: 1,
            ..
        })
    ));
    let second = fixture
        .continuation
        .record_turn_outcome("ses_goal", GoalTurnOutcome::Blocking("missing credential"))
        .expect("second blocker");
    assert!(matches!(
        second,
        BlockedAudit::Pending(FailureStreak {
            consecutive_turns: 2,
            ..
        })
    ));
    assert_eq!(
        fixture
            .store
            .goal("ses_goal")
            .expect("read goal")
            .expect("goal")
            .status,
        GoalStatus::Active
    );

    assert_eq!(
        fixture
            .continuation
            .record_turn_outcome("ses_goal", GoalTurnOutcome::Progress)
            .expect("record progress"),
        BlockedAudit::Reset
    );
    for expected in 1..BLOCKED_TURN_THRESHOLD {
        let audit = fixture
            .continuation
            .record_turn_outcome("ses_goal", GoalTurnOutcome::Blocking("missing credential"))
            .expect("record blocker after reset");
        assert!(
            matches!(audit, BlockedAudit::Pending(FailureStreak { consecutive_turns, .. }) if consecutive_turns == expected)
        );
    }
    assert!(matches!(
        fixture
            .continuation
            .record_turn_outcome("ses_goal", GoalTurnOutcome::Blocking("missing credential"))
            .expect("third blocker"),
        BlockedAudit::Blocked(FailureStreak {
            consecutive_turns: 3,
            ..
        })
    ));
    assert_eq!(
        fixture
            .store
            .goal("ses_goal")
            .expect("read goal")
            .expect("goal")
            .status,
        GoalStatus::Blocked
    );
}

#[test]
fn changed_blocker_restarts_the_persistent_count() {
    let fixture = Fixture::new();
    fixture.create("audit blocker identity");
    fixture
        .continuation
        .record_turn_outcome("ses_goal", GoalTurnOutcome::Blocking("network down"))
        .expect("first blocker");
    let changed = fixture
        .continuation
        .record_turn_outcome("ses_goal", GoalTurnOutcome::Blocking("permission denied"))
        .expect("changed blocker");
    assert_eq!(
        changed,
        BlockedAudit::Pending(FailureStreak {
            signal: "permission denied".to_owned(),
            consecutive_turns: 1,
        })
    );
}

#[test]
fn retry_backoff_suppresses_until_due_then_prepares_the_goal() {
    let fixture = Fixture::new();
    fixture.create("keep working after a transient outage");
    fixture
        .store
        .schedule_retry(
            "ses_goal",
            crate::GoalRetryReason::ProviderTransient,
            None,
            crate::GoalRetryPolicy::new(
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(30),
                0,
                std::time::Duration::from_millis(250),
            )
            .expect("valid policy"),
            1_000,
            0,
        )
        .expect("schedule retry")
        .expect("active goal");
    assert!(matches!(
        fixture
            .continuation
            .prepare_if_idle_at(
                "ses_goal",
                GoalTurnMode::Work,
                QueuedUserInput::Absent,
                2_999,
            )
            .expect("backoff before deadline"),
        ContinuationAttempt::Suppressed(ContinuationSuppression::RetryBackoff { remaining })
            if remaining == std::time::Duration::from_millis(1)
    ));
    assert!(matches!(
        fixture
            .continuation
            .prepare_if_idle_at(
                "ses_goal",
                GoalTurnMode::Work,
                QueuedUserInput::Absent,
                3_000,
            )
            .expect("retry at deadline"),
        ContinuationAttempt::Prepared(_)
    ));
}

#[test]
fn a_reopened_continuation_honors_the_persisted_retry_deadline() {
    let database = tempfile::tempdir().expect("create database directory");
    let spill = tempfile::tempdir().expect("create spill directory");
    let path = database.path().join("goal-test.db");
    {
        let store = GoalStore::open_at(&path, spill.path().to_owned()).expect("open goal store");
        store
            .create_goal("ses_restart_retry", "resume after restart", None)
            .expect("create goal");
        store
            .schedule_retry(
                "ses_restart_retry",
                crate::GoalRetryReason::ProviderStream,
                None,
                crate::GoalRetryPolicy::new(
                    std::time::Duration::from_secs(2),
                    std::time::Duration::from_secs(30),
                    0,
                    std::time::Duration::from_millis(250),
                )
                .expect("valid policy"),
                1_000,
                0,
            )
            .expect("schedule retry")
            .expect("active goal");
    }

    let store =
        Arc::new(GoalStore::open_at(&path, spill.path().to_owned()).expect("reopen goal store"));
    let runs = SessionRunRegistry::new();
    let continuation = GoalContinuation::new(Arc::clone(&store), runs.clone());
    assert!(matches!(
        continuation
            .prepare_if_idle_at(
                "ses_restart_retry",
                GoalTurnMode::Work,
                QueuedUserInput::Absent,
                2_999,
            )
            .expect("read persisted backoff"),
        ContinuationAttempt::Suppressed(ContinuationSuppression::RetryBackoff { remaining })
            if remaining == std::time::Duration::from_millis(1)
    ));

    let ContinuationAttempt::Prepared(prepared) = continuation
        .prepare_if_idle_at(
            "ses_restart_retry",
            GoalTurnMode::Work,
            QueuedUserInput::Absent,
            3_000,
        )
        .expect("prepare persisted retry at deadline")
    else {
        panic!("the reopened scheduler must resume the due goal");
    };
    assert_eq!(runs.status("ses_restart_retry"), SessionStatus::Busy);
    assert!(text(prepared.entry()).contains("recovery attempt 1"));
    assert!(text(prepared.entry()).contains("provider_stream"));
}

#[test]
fn retry_context_tells_the_model_to_verify_side_effects_before_repeating_them() {
    let fixture = Fixture::new();
    fixture.create("deploy the release");
    fixture
        .store
        .schedule_retry(
            "ses_goal",
            crate::GoalRetryReason::ProviderStream,
            None,
            crate::GoalRetryPolicy::new(
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(30),
                0,
                std::time::Duration::from_millis(250),
            )
            .expect("valid policy"),
            1_000,
            0,
        )
        .expect("schedule retry")
        .expect("active goal");

    let context = fixture
        .continuation
        .injection("ses_goal")
        .expect("read retry context")
        .expect("active goal")
        .message
        .content;
    let rendered = context
        .iter()
        .filter_map(|block| match block {
            zuno_llm::event::RequestContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(rendered.contains("provider_stream"), "{rendered}");
    assert!(rendered.contains("recovery attempt 1"), "{rendered}");
    assert!(
        rendered.contains("before repeating an action with side effects"),
        "{rendered}"
    );
}

#[test]
fn uncertain_tool_recovery_forbids_replay_until_external_state_is_verified() {
    let fixture = Fixture::new();
    fixture.create("publish without duplicating side effects");
    fixture
        .store
        .schedule_retry(
            "ses_goal",
            crate::GoalRetryReason::ToolUncertain,
            None,
            crate::GoalRetryPolicy::new(
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(30),
                0,
                std::time::Duration::from_millis(250),
            )
            .expect("valid policy"),
            1_000,
            0,
        )
        .expect("schedule retry")
        .expect("active goal");

    let entry = fixture
        .continuation
        .injection("ses_goal")
        .expect("read retry context")
        .expect("active goal");
    let rendered = entry
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            zuno_llm::event::RequestContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();

    assert!(rendered.contains("tool_uncertain"), "{rendered}");
    assert!(
        rendered.contains("Verify authoritative external state"),
        "{rendered}"
    );
}

#[test]
fn failure_streak_survives_restart_and_resume_clears_it() {
    let database = tempfile::tempdir().expect("create database directory");
    let spill = tempfile::tempdir().expect("create spill directory");
    let path = database.path().join("goal-test.db");
    {
        let store = GoalStore::open_at(&path, spill.path().to_owned()).expect("open goal store");
        store
            .create_goal("ses_restart", "persist blocker count", None)
            .expect("create goal");
        for _ in 0..2 {
            store
                .record_failure_signal("ses_restart", Some("network unavailable"))
                .expect("record blocker");
        }
    }

    let store = GoalStore::open_at(&path, spill.path().to_owned()).expect("reopen goal store");
    assert_eq!(
        store
            .failure_streak("ses_restart")
            .expect("read persisted streak"),
        Some(FailureStreak {
            signal: "network unavailable".to_owned(),
            consecutive_turns: 2,
        })
    );
    store
        .set_status_as_system("ses_restart", crate::SystemStatus::Paused)
        .expect("pause goal");
    store
        .set_status_as_system("ses_restart", crate::SystemStatus::Active)
        .expect("resume goal");
    assert_eq!(
        store
            .failure_streak("ses_restart")
            .expect("read cleared streak"),
        None
    );
}

/// Criterion 11's "survives two compactions", which had only ever been driven once.
///
/// One compaction cannot distinguish a goal that is genuinely re-derived from SQL
/// from one that survived because a stale in-memory copy was still reachable: the
/// second boundary is what proves the first regeneration did not consume the source.
/// So the objective and every counter — tokens, wall clock, and the persistent
/// failure streak — are checked after each of two consecutive boundaries, and both
/// boundaries are asserted to have actually discarded the injected goal context
/// rather than retaining it.
#[test]
fn goal_survives_two_consecutive_compactions_with_objective_and_counters_intact() {
    const OBJECTIVE: &str = "port the whole surface, not the easy subset";
    let fixture = Fixture::new();
    fixture.create(OBJECTIVE);
    fixture
        .store
        .record_usage("ses_goal", 1_500, 30, true)
        .expect("record first usage");
    fixture
        .continuation
        .record_turn_outcome("ses_goal", GoalTurnOutcome::Blocking("waiting on review"))
        .expect("record a blocker so a counter has something to lose");

    let mut compacted = Vec::new();
    for round in 1..=2 {
        let injection = fixture
            .continuation
            .injection("ses_goal")
            .expect("render injection")
            .expect("active goal injection");
        assert!(
            text(&injection).contains(OBJECTIVE),
            "round {round}: the injection lost the objective before compaction even ran"
        );
        assert!(
            !injection.preserve_initial,
            "round {round}: goal context that is preserved across compaction would make this \
             test prove nothing — the point is that it IS discarded and re-derived"
        );

        let entries = vec![
            TranscriptEntry::new("system", Message::new(Role::System, "system"), 1),
            injection,
            TranscriptEntry::new("user", Message::new(Role::User, "keep going"), 1),
            TranscriptEntry::new("assistant", Message::new(Role::Assistant, "working"), 1),
        ];
        let boundary = select_boundary(&entries, 1, 2)
            .unwrap_or_else(|| panic!("round {round}: no compaction boundary was selectable"));
        assert!(
            boundary.retained_from > 1,
            "round {round}: the goal injection at index 1 was retained, so this round compacted \
             nothing the goal depended on"
        );
        compacted.push(boundary.retained_from);

        fixture
            .store
            .record_usage("ses_goal", 350, 10, true)
            .unwrap_or_else(|error| panic!("round {round}: record post-compaction usage: {error}"));
    }
    assert_eq!(compacted.len(), 2, "two boundaries must have been selected");

    let goal = fixture
        .store
        .goal("ses_goal")
        .expect("read goal")
        .expect("the goal still exists after two compactions");
    assert_eq!(goal.objective, OBJECTIVE, "the objective did not survive");
    assert_eq!(
        (goal.tokens_used, goal.time_used_seconds),
        (2_200, 50),
        "the usage counters must accumulate across both compactions: 1500+350+350 tokens and \
         30+10+10 seconds"
    );
    assert_eq!(goal.status, GoalStatus::Active);
    assert_eq!(
        fixture
            .store
            .failure_streak("ses_goal")
            .expect("read the persisted streak"),
        Some(FailureStreak {
            signal: "waiting on review".to_owned(),
            consecutive_turns: 1,
        }),
        "the blocked-audit counter is persistent state, so two compactions must not reset it"
    );

    let after = fixture
        .continuation
        .injection("ses_goal")
        .expect("render injection after two compactions")
        .expect("active goal injection");
    assert!(
        text(&after).contains(OBJECTIVE),
        "the third injection is regenerated from SQL after two compactions and must still carry \
         the objective"
    );
}
