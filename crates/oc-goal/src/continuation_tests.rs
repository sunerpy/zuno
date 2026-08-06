use super::*;
use oc_engine::compaction::select_boundary;
use oc_llm::event::RequestContentBlock;

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
fn terminal_turn_error_blocks_active_goal_immediately() {
    let fixture = Fixture::new();
    fixture.create("do not loop on provider failure");
    let stopped = fixture
        .continuation
        .on_terminal_turn_error("ses_goal")
        .expect("stop goal")
        .expect("goal exists");
    assert_eq!(stopped.status, GoalStatus::Blocked);
    assert!(matches!(
        fixture
            .continuation
            .prepare_if_idle("ses_goal", GoalTurnMode::Work, QueuedUserInput::Absent)
            .expect("idle after error"),
        ContinuationAttempt::Suppressed(ContinuationSuppression::NoActiveGoal)
    ));
}

#[test]
fn failure_streak_survives_restart_and_resume_clears_it() {
    let database = tempfile::tempdir().expect("create database directory");
    let spill = tempfile::tempdir().expect("create spill directory");
    let path = database.path().join(crate::GOAL_DB_FILE);
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
