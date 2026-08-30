//! Tests for continuation and the job board.
//!
//! Each refusal gets a fixture that isolates *its* condition: the five ways a
//! `task_id` can be unusable overlap heavily in setup, and a shared fixture that
//! happens to trip several of them at once would report per-condition coverage it does
//! not have.

use std::sync::Arc;

use super::*;
use crate::builtin::delegable;

const PARENT: &str = "ses_root";
const OTHER_PARENT: &str = "ses_other_root";

struct Fixture {
    board: JobBoard,
    sessions: Arc<RecordingSessions>,
    runs: Arc<StatedRunState>,
}

fn fixture() -> Fixture {
    let sessions = Arc::new(RecordingSessions::new());
    let runs = Arc::new(StatedRunState::idle());
    let board = JobBoard::new(sessions.clone(), runs.clone());
    Fixture {
        board,
        sessions,
        runs,
    }
}

fn fresh(agent: &str, prompt: &str) -> Dispatch {
    Dispatch {
        parent_session_id: PARENT.to_owned(),
        agent: agent.to_owned(),
        task_id: None,
        prompt: prompt.to_owned(),
        objective: format!("{agent}: {prompt}"),
        background: true,
    }
}

fn continuing(agent: &str, prompt: &str, task_id: &str) -> Dispatch {
    Dispatch {
        task_id: Some(task_id.to_owned()),
        ..fresh(agent, prompt)
    }
}

fn opens(ops: &[SessionOp]) -> usize {
    ops.iter()
        .filter(|op| matches!(op, SessionOp::Open { .. }))
        .count()
}

fn state_of(board: &JobBoard, alias: &str) -> JobState {
    board
        .jobs(PARENT)
        .into_iter()
        .find(|job| job.alias == alias)
        .map(|job| job.state)
        .unwrap_or_else(|| panic!("no lane aliased `{alias}`"))
}

// ---------------------------------------------------------------------------
// Prose is not a continuation. The headline property.
// ---------------------------------------------------------------------------

#[test]
fn claiming_continuation_in_prose_without_a_task_id_starts_a_second_session() {
    let harness = fixture();
    let first = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    assert!(harness.board.settle(&first.job_id, Outcome::Completed));
    assert!(harness.board.reconcile(&first.job_id));

    // The prompt says it is continuing; `task_id` is absent. That is the failure.
    let second = harness
        .board
        .dispatch(&Dispatch {
            prompt: "Continuing the previous explorer session, now map the writer".to_owned(),
            ..fresh("explorer", "ignored")
        })
        .expect("a second dispatch");

    assert_ne!(
        second.session_id, first.session_id,
        "prose claimed reuse, so a reused session would mean the claim had force"
    );
    assert!(!second.continued);
    assert_eq!(first.message_count, 1);
    assert_eq!(
        second.message_count, 1,
        "the second session starts from zero, whatever the prompt said"
    );
    assert_eq!(harness.sessions.messages(&second.session_id).len(), 1);
}

#[test]
fn a_task_id_continues_the_same_session_and_its_message_count_grows() {
    let harness = fixture();
    let first = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    assert!(harness.board.settle(&first.job_id, Outcome::Completed));
    assert!(harness.board.reconcile(&first.job_id));

    let second = harness
        .board
        .dispatch(&continuing("explorer", "now map the writer", &first.alias))
        .expect("a continuation");

    assert_eq!(second.session_id, first.session_id);
    assert!(second.continued);
    assert_eq!(
        second.message_count,
        first.message_count + 1,
        "a continuation grows the conversation instead of restarting it"
    );
    assert_eq!(second.messages_before, first.message_count);
    assert!(second.message_count > second.messages_before);
}

#[test]
fn a_fresh_lane_reports_that_nothing_preceded_it() {
    let harness = fixture();
    let first = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    assert_eq!(first.messages_before, 0);
    assert_eq!(first.message_count, 1);
}

#[test]
fn continuing_a_lane_whose_session_was_deleted_is_refused_and_drops_the_lane() {
    let harness = fixture();
    let lane = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    harness.board.settle(&lane.job_id, Outcome::Completed);
    harness.board.reconcile(&lane.job_id);
    harness.sessions.delete(&lane.session_id);

    let refusal = harness
        .board
        .dispatch(&continuing("explorer", "map the writer", &lane.alias))
        .expect_err("a session that is gone cannot be continued");

    let message = refusal.to_string();
    assert!(message.contains(&lane.alias), "{message}");
    assert!(message.contains(&lane.session_id), "{message}");
    assert!(message.contains("no longer exists"), "{message}");
    assert!(matches!(refusal, ContinuationError::VanishedSession { .. }));
    assert!(
        harness.sessions.messages(&lane.session_id).is_empty(),
        "the refused dispatch appended to a session that does not exist"
    );
    assert!(
        harness.board.render(PARENT).is_none(),
        "the board still advertises a handle the store cannot honour"
    );
}

#[test]
fn a_vanished_lanes_alias_is_never_reissued_to_a_later_lane() {
    let harness = fixture();
    let lane = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    harness.board.settle(&lane.job_id, Outcome::Completed);
    harness.board.reconcile(&lane.job_id);
    harness.sessions.delete(&lane.session_id);
    harness
        .board
        .dispatch(&continuing("explorer", "map the writer", &lane.alias))
        .expect_err("a session that is gone cannot be continued");

    let replacement = harness
        .board
        .dispatch(&fresh("explorer", "map the writer"))
        .expect("a fresh dispatch");
    assert_ne!(
        replacement.alias, lane.alias,
        "a handle the model may still hold must not come to mean a different lane"
    );
}

#[test]
fn a_continuation_appends_to_the_session_and_never_reopens_or_replays_it() {
    let harness = fixture();
    let first = harness
        .board
        .dispatch(&fresh("explorer", "first"))
        .expect("a fresh dispatch");
    harness.board.settle(&first.job_id, Outcome::Completed);
    harness.board.reconcile(&first.job_id);
    harness
        .board
        .dispatch(&continuing("explorer", "second", &first.session_id))
        .expect("a continuation");

    let ops = harness.sessions.ops();
    assert_eq!(
        opens(&ops),
        1,
        "the continuation opened a second session: {ops:?}"
    );
    assert_eq!(
        ops,
        vec![
            SessionOp::Open {
                parent: PARENT.to_owned(),
                agent: "explorer".to_owned(),
            },
            SessionOp::Append {
                session: first.session_id.clone(),
                prompt: "first".to_owned(),
            },
            SessionOp::Count {
                session: first.session_id.clone(),
            },
            SessionOp::Append {
                session: first.session_id.clone(),
                prompt: "second".to_owned(),
            },
        ],
        "a continuation is one append against the same session; a replay would show \
         `first` being written twice"
    );
    assert_eq!(
        harness.sessions.messages(&first.session_id),
        vec!["first".to_owned(), "second".to_owned()],
        "a rebuild would re-insert `first`; a reset would drop it"
    );
}

#[test]
fn the_board_states_that_prose_cannot_substitute_for_a_task_id() {
    let harness = fixture();
    harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");

    let board = harness.board.render(PARENT).expect("a board");
    assert!(board.contains(PROSE_IS_NOT_ENOUGH), "{board}");
    assert!(board.contains("`task_id`"), "{board}");
}

// ---------------------------------------------------------------------------
// Two id spaces.
// ---------------------------------------------------------------------------

#[test]
fn a_continuation_keeps_the_session_id_and_takes_a_fresh_job_id() {
    let harness = fixture();
    let first = harness
        .board
        .dispatch(&fresh("worker", "rename the field"))
        .expect("a fresh dispatch");
    harness.board.settle(&first.job_id, Outcome::Completed);
    harness.board.reconcile(&first.job_id);

    let second = harness
        .board
        .dispatch(&continuing("worker", "and its callers", &first.alias))
        .expect("a continuation");

    assert_eq!(second.session_id, first.session_id);
    assert_ne!(
        second.job_id, first.job_id,
        "one lane takes many dispatches, so a dispatch needs its own handle"
    );
    assert_eq!(second.alias, first.alias);
}

#[test]
fn no_job_id_is_ever_a_session_id() {
    let harness = fixture();
    let mut jobs = Vec::new();
    let mut sessions = Vec::new();
    for turn in 0..4 {
        let dispatched = harness
            .board
            .dispatch(&fresh("librarian", &format!("lookup {turn}")))
            .expect("a fresh dispatch");
        assert!(dispatched.job_id.starts_with(JOB_ID_PREFIX));
        assert!(!sessions.contains(&dispatched.job_id));
        assert!(!jobs.contains(&dispatched.job_id));
        jobs.push(dispatched.job_id);
        sessions.push(dispatched.session_id);
    }
    assert_eq!(jobs.len(), 4);
}

#[test]
fn a_stale_completion_cannot_settle_the_dispatch_that_replaced_it() {
    let harness = fixture();
    let first = harness
        .board
        .dispatch(&fresh("worker", "first"))
        .expect("a fresh dispatch");
    harness.board.settle(&first.job_id, Outcome::Completed);
    harness.board.reconcile(&first.job_id);
    let second = harness
        .board
        .dispatch(&continuing("worker", "second", &first.alias))
        .expect("a continuation");

    assert!(
        !harness.board.settle(&first.job_id, Outcome::Failed),
        "the superseded job id must no longer name a lane"
    );
    assert_eq!(state_of(&harness.board, &first.alias), JobState::Active);
    assert!(harness.board.settle(&second.job_id, Outcome::Completed));
}

// ---------------------------------------------------------------------------
// The Active rule. One fixture per condition.
// ---------------------------------------------------------------------------

#[test]
fn re_dispatching_to_an_active_job_is_refused_naming_the_job() {
    let harness = fixture();
    let running = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");

    let refusal = harness
        .board
        .dispatch(&continuing(
            "explorer",
            "also map the writer",
            &running.alias,
        ))
        .expect_err("an active lane is not addressable");

    let message = refusal.to_string();
    for named in [
        running.alias.as_str(),
        running.session_id.as_str(),
        running.job_id.as_str(),
        "explorer",
        "active",
    ] {
        assert!(
            message.contains(named),
            "{named} is missing from: {message}"
        );
    }
    assert!(message.contains("not a queue"), "{message}");
    assert!(message.contains("Reusable"), "{message}");
    assert!(matches!(refusal, ContinuationError::ActiveLane { .. }));
    assert_eq!(
        opens(&harness.sessions.ops()),
        1,
        "a refused dispatch must not have opened a session"
    );
}

#[test]
fn a_lane_whose_session_holds_a_live_turn_is_active_even_though_its_record_settled() {
    let harness = fixture();
    let lane = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    harness.board.settle(&lane.job_id, Outcome::Completed);
    harness.board.reconcile(&lane.job_id);
    assert_eq!(state_of(&harness.board, &lane.alias), JobState::Reconciled);

    harness.runs.mark_busy(&lane.session_id);

    assert_eq!(
        state_of(&harness.board, &lane.alias),
        JobState::Active,
        "the run registry is the authority on whether a turn is live"
    );
    let refusal = harness
        .board
        .dispatch(&continuing("explorer", "more", &lane.alias))
        .expect_err("a busy session is not addressable");
    assert!(matches!(refusal, ContinuationError::ActiveLane { .. }));
}

#[test]
fn re_dispatching_to_a_completed_but_unreconciled_job_is_refused() {
    let harness = fixture();
    let lane = harness
        .board
        .dispatch(&fresh("advisor", "review the migration"))
        .expect("a fresh dispatch");
    harness.board.settle(&lane.job_id, Outcome::Completed);

    assert_eq!(
        state_of(&harness.board, &lane.alias),
        JobState::Unreconciled
    );
    let refusal = harness
        .board
        .dispatch(&continuing("advisor", "and the rollback", &lane.alias))
        .expect_err("an unread result is not addressable");
    let message = refusal.to_string();
    assert!(message.contains("unreconciled"), "{message}");
    assert!(matches!(refusal, ContinuationError::ActiveLane { .. }));
}

#[test]
fn an_unknown_task_id_is_refused_naming_the_addressable_jobs() {
    let harness = fixture();
    let reusable = harness
        .board
        .dispatch(&fresh("librarian", "find the default"))
        .expect("a fresh dispatch");
    harness.board.settle(&reusable.job_id, Outcome::Completed);
    harness.board.reconcile(&reusable.job_id);

    let refusal = harness
        .board
        .dispatch(&continuing("librarian", "find another", "lib-9"))
        .expect_err("an unknown handle is refused");

    let message = refusal.to_string();
    assert!(message.contains("lib-9"), "{message}");
    assert!(message.contains(&reusable.alias), "{message}");
    assert!(message.contains("omit `task_id`"), "{message}");
    assert!(matches!(refusal, ContinuationError::UnknownTaskId { .. }));
}

#[test]
fn an_unknown_task_id_says_so_plainly_when_nothing_is_addressable_yet() {
    let harness = fixture();
    let refusal = harness
        .board
        .dispatch(&continuing("librarian", "find the default", "lib-1"))
        .expect_err("an unknown handle is refused");
    assert!(refusal.to_string().contains("none yet"), "{refusal}");
    assert_eq!(
        opens(&harness.sessions.ops()),
        0,
        "a refusal must not leave an orphan session behind"
    );
}

#[test]
fn a_failed_job_is_refused_as_not_reusable_rather_than_as_active() {
    let harness = fixture();
    let lane = harness
        .board
        .dispatch(&fresh("worker", "rename the field"))
        .expect("a fresh dispatch");
    harness.board.settle(&lane.job_id, Outcome::Failed);
    harness.board.reconcile(&lane.job_id);

    assert_eq!(state_of(&harness.board, &lane.alias), JobState::Failed);
    let refusal = harness
        .board
        .dispatch(&continuing("worker", "try again", &lane.alias))
        .expect_err("a failed lane is not reusable");
    let message = refusal.to_string();
    assert!(message.contains("failed"), "{message}");
    assert!(message.contains("is not reusable"), "{message}");
    assert!(matches!(refusal, ContinuationError::NotReusable { .. }));
}

#[test]
fn a_cancelled_job_is_refused_as_not_reusable() {
    let harness = fixture();
    let lane = harness
        .board
        .dispatch(&fresh("worker", "rename the field"))
        .expect("a fresh dispatch");
    harness.board.settle(&lane.job_id, Outcome::Cancelled);
    harness.board.reconcile(&lane.job_id);

    assert_eq!(state_of(&harness.board, &lane.alias), JobState::Cancelled);
    let refusal = harness
        .board
        .dispatch(&continuing("worker", "try again", &lane.alias))
        .expect_err("a cancelled lane is not reusable");
    assert!(refusal.to_string().contains("cancelled"), "{refusal}");
    assert!(matches!(refusal, ContinuationError::NotReusable { .. }));
}

#[test]
fn a_task_id_from_another_parents_board_is_refused() {
    let harness = fixture();
    let theirs = harness
        .board
        .dispatch(&Dispatch {
            parent_session_id: OTHER_PARENT.to_owned(),
            ..fresh("explorer", "map their loader")
        })
        .expect("a fresh dispatch");
    harness.board.settle(&theirs.job_id, Outcome::Completed);
    harness.board.reconcile(&theirs.job_id);

    let refusal = harness
        .board
        .dispatch(&continuing("explorer", "map ours", &theirs.session_id))
        .expect_err("another parent's lane is not ours to continue");

    let message = refusal.to_string();
    assert!(message.contains(OTHER_PARENT), "{message}");
    assert!(message.contains(PARENT), "{message}");
    assert!(matches!(refusal, ContinuationError::ForeignParent { .. }));
}

#[test]
fn a_task_id_naming_another_agents_session_is_refused() {
    let harness = fixture();
    let explorer = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    harness.board.settle(&explorer.job_id, Outcome::Completed);
    harness.board.reconcile(&explorer.job_id);

    let refusal = harness
        .board
        .dispatch(&continuing("worker", "now rename it", &explorer.alias))
        .expect_err("a session belongs to its agent");

    let message = refusal.to_string();
    assert!(message.contains("explorer"), "{message}");
    assert!(message.contains("worker"), "{message}");
    assert!(matches!(refusal, ContinuationError::AgentMismatch { .. }));
}

#[test]
fn only_a_reconciled_job_is_addressable() {
    for state in JobState::ALL {
        assert_eq!(
            state.addressable(),
            state == JobState::Reconciled,
            "{state} disagrees with the one-reusable-state rule"
        );
    }
}

#[test]
fn an_empty_task_id_starts_a_fresh_lane_rather_than_being_refused() {
    let harness = fixture();
    for empty in ["", "   "] {
        let dispatched = harness
            .board
            .dispatch(&Dispatch {
                task_id: Some(empty.to_owned()),
                ..fresh("explorer", "map the loader")
            })
            .expect("an empty handle is not a continuation request");
        assert!(!dispatched.continued);
    }
}

// ---------------------------------------------------------------------------
// The board.
// ---------------------------------------------------------------------------

/// Drive one lane per state, so a single render exercises all five.
fn every_state() -> Fixture {
    let harness = fixture();
    harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("active");

    let unreconciled = harness
        .board
        .dispatch(&fresh("worker", "rename the field"))
        .expect("unreconciled");
    harness
        .board
        .settle(&unreconciled.job_id, Outcome::Completed);

    let reconciled = harness
        .board
        .dispatch(&fresh("librarian", "find the default"))
        .expect("reconciled");
    harness.board.settle(&reconciled.job_id, Outcome::Completed);
    harness.board.reconcile(&reconciled.job_id);

    let failed = harness
        .board
        .dispatch(&fresh("advisor", "review the migration"))
        .expect("failed");
    harness.board.settle(&failed.job_id, Outcome::Failed);

    let cancelled = harness
        .board
        .dispatch(&fresh("looker", "read the screenshot"))
        .expect("cancelled");
    harness.board.settle(&cancelled.job_id, Outcome::Cancelled);

    harness
}

#[test]
fn the_board_renders_every_state() {
    let harness = every_state();
    let board = harness.board.render(PARENT).expect("a board");

    let rendered: Vec<JobState> = harness
        .board
        .jobs(PARENT)
        .into_iter()
        .map(|job| job.state)
        .collect();
    for state in JobState::ALL {
        assert!(rendered.contains(&state), "{state} is not on the board");
        assert!(
            board.contains(&format!("/ {state}")),
            "{state} does not render: {board}"
        );
    }
}

#[test]
fn the_board_sorts_every_job_into_active_reusable_or_closed() {
    let harness = every_state();
    let board = harness.board.render(PARENT).expect("a board");

    let mut offsets = Vec::new();
    for section in Section::ALL {
        let offset = board
            .find(section.heading())
            .unwrap_or_else(|| panic!("{} is missing: {board}", section.heading()));
        offsets.push(offset);
    }
    assert!(
        offsets.windows(2).all(|pair| pair[0] < pair[1]),
        "sections render out of order: {board}"
    );

    for job in harness.board.jobs(PARENT) {
        let heading = job.state.section().heading();
        let section_start = board.find(heading).expect("a heading");
        let row = board
            .find(&format!("- {} /", job.alias))
            .expect("a row for every lane");
        assert!(
            row > section_start,
            "{} renders above its own section: {board}",
            job.alias
        );
    }
}

#[test]
fn an_empty_board_renders_nothing_at_all() {
    let harness = fixture();
    assert!(harness.board.render(PARENT).is_none());
    harness
        .board
        .dispatch(&Dispatch {
            parent_session_id: OTHER_PARENT.to_owned(),
            ..fresh("explorer", "map their loader")
        })
        .expect("a fresh dispatch");
    assert!(
        harness.board.render(PARENT).is_none(),
        "another parent's lane must not conjure a board here"
    );
}

#[test]
fn a_board_with_no_reusable_job_still_renders_the_reusable_section_as_empty() {
    let harness = fixture();
    harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    let board = harness.board.render(PARENT).expect("a board");

    let reusable = board
        .find(Section::Reusable.heading())
        .expect("the reusable heading");
    assert!(
        board[reusable..].starts_with("#### Reusable\n- none"),
        "an absent section reads as an unanswered question: {board}"
    );
}

#[test]
fn the_board_shows_only_this_parents_jobs() {
    let harness = fixture();
    let ours = harness
        .board
        .dispatch(&fresh("explorer", "map ours"))
        .expect("a fresh dispatch");
    let theirs = harness
        .board
        .dispatch(&Dispatch {
            parent_session_id: OTHER_PARENT.to_owned(),
            ..fresh("explorer", "map theirs")
        })
        .expect("a fresh dispatch");

    let board = harness.board.render(PARENT).expect("a board");
    assert!(board.contains(&ours.session_id), "{board}");
    assert!(!board.contains(&theirs.session_id), "{board}");
}

#[test]
fn the_board_renders_the_alias_session_agent_and_state_of_each_job() {
    let harness = fixture();
    let lane = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    let board = harness.board.render(PARENT).expect("a board");

    assert!(
        board.contains(&format!(
            "- {} / {} / explorer / active",
            lane.alias, lane.session_id
        )),
        "{board}"
    );
    assert!(
        board.contains(&format!("  Job: {}", lane.job_id)),
        "{board}"
    );
    assert!(
        board.contains("  Objective: explorer: map the loader"),
        "{board}"
    );
}

#[test]
fn a_long_objective_is_collapsed_to_one_bounded_line() {
    let harness = fixture();
    harness
        .board
        .dispatch(&Dispatch {
            objective: format!("map\n the\t{}", "loader ".repeat(40)),
            ..fresh("explorer", "map the loader")
        })
        .expect("a fresh dispatch");

    let board = harness.board.render(PARENT).expect("a board");
    let objective = board
        .lines()
        .find(|line| line.starts_with("  Objective:"))
        .expect("an objective line");
    assert!(objective.ends_with("..."), "{objective}");
    assert!(objective.chars().count() <= 140, "{objective}");
    assert_eq!(
        board.lines().filter(|line| line.contains("loader")).count(),
        1,
        "a multi-line objective broke the row structure: {board}"
    );
}

// ---------------------------------------------------------------------------
// Ids are stable across turns.
// ---------------------------------------------------------------------------

#[test]
fn the_board_is_byte_identical_across_repeated_renders() {
    let harness = every_state();
    let first = harness.board.render(PARENT).expect("a board");
    let second = harness.board.render(PARENT).expect("a board");
    let third = harness.board.render(PARENT).expect("a board");
    assert_eq!(first, second);
    assert_eq!(second, third);
}

#[test]
fn an_alias_survives_a_lane_moving_from_active_to_reusable() {
    let harness = fixture();
    let lane = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    let while_active = harness.board.render(PARENT).expect("a board");
    assert!(while_active.contains(&format!("- {} /", lane.alias)));

    harness.board.settle(&lane.job_id, Outcome::Completed);
    harness.board.reconcile(&lane.job_id);
    let once_reusable = harness.board.render(PARENT).expect("a board");

    assert!(
        once_reusable.contains(&format!(
            "- {} / {} / explorer",
            lane.alias, lane.session_id
        )),
        "the handle the model read last turn must still resolve: {once_reusable}"
    );
    assert_eq!(harness.board.addressable(PARENT), vec![lane.alias]);
}

#[test]
fn a_continuation_does_not_renumber_the_lane() {
    let harness = fixture();
    let first = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    for turn in 0..3 {
        harness
            .board
            .settle(&harness.board.jobs(PARENT)[0].job_id, Outcome::Completed);
        harness
            .board
            .reconcile(&harness.board.jobs(PARENT)[0].job_id);
        let continued = harness
            .board
            .dispatch(&continuing(
                "explorer",
                &format!("turn {turn}"),
                &first.alias,
            ))
            .expect("a continuation");
        assert_eq!(continued.alias, first.alias);
        assert_eq!(continued.session_id, first.session_id);
    }
    assert_eq!(harness.board.jobs(PARENT).len(), 1);
}

#[test]
fn a_lane_keeps_its_number_when_a_later_lane_joins_the_board() {
    let harness = fixture();
    let first = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    let second = harness
        .board
        .dispatch(&fresh("explorer", "map the writer"))
        .expect("a fresh dispatch");

    assert_eq!(first.alias, "exp-1");
    assert_eq!(second.alias, "exp-2");
    let aliases: Vec<String> = harness
        .board
        .jobs(PARENT)
        .into_iter()
        .map(|job| job.alias)
        .collect();
    assert_eq!(aliases, vec!["exp-1".to_owned(), "exp-2".to_owned()]);
}

#[test]
fn alias_numbering_is_per_parent_so_two_boards_do_not_share_a_counter() {
    let harness = fixture();
    let ours = harness
        .board
        .dispatch(&fresh("explorer", "map ours"))
        .expect("a fresh dispatch");
    let theirs = harness
        .board
        .dispatch(&Dispatch {
            parent_session_id: OTHER_PARENT.to_owned(),
            ..fresh("explorer", "map theirs")
        })
        .expect("a fresh dispatch");
    assert_eq!(ours.alias, "exp-1");
    assert_eq!(theirs.alias, "exp-1");
    assert_ne!(ours.session_id, theirs.session_id);
}

#[test]
fn every_delegable_agent_gets_a_distinct_alias_prefix() {
    let mut prefixes = Vec::new();
    for agent in delegable(true) {
        let prefix = alias_prefix(agent.name);
        assert_eq!(
            prefix.chars().count(),
            ALIAS_PREFIX_LENGTH,
            "{}",
            agent.name
        );
        assert!(
            !prefixes.contains(&prefix),
            "`{}` collides with an earlier agent on the alias prefix `{prefix}`; the \
             roster needs an explicit prefix once that happens",
            agent.name
        );
        prefixes.push(prefix);
    }
    assert!(prefixes.len() >= 4, "the roster shrank to {prefixes:?}");
}

#[test]
fn an_agent_with_no_usable_name_still_gets_a_typeable_alias() {
    assert_eq!(alias_prefix(""), FALLBACK_ALIAS_PREFIX);
    assert_eq!(alias_prefix("--"), FALLBACK_ALIAS_PREFIX);
    assert_eq!(alias_prefix("Explorer"), "exp");
}

// ---------------------------------------------------------------------------
// Bookkeeping edges.
// ---------------------------------------------------------------------------

#[test]
fn a_running_job_cannot_be_reconciled_before_it_settles() {
    let harness = fixture();
    let lane = harness
        .board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect("a fresh dispatch");
    assert!(
        !harness.board.reconcile(&lane.job_id),
        "reconciling a running lane would make it addressable while it works"
    );
    assert_eq!(state_of(&harness.board, &lane.alias), JobState::Active);
}

#[test]
fn settling_or_reconciling_an_unknown_job_id_reports_false() {
    let harness = fixture();
    assert!(!harness.board.settle("bg_999999", Outcome::Completed));
    assert!(!harness.board.reconcile("bg_999999"));
}

#[test]
fn a_child_session_store_failure_is_reported_rather_than_swallowed() {
    let board = JobBoard::new(
        Arc::new(RecordingSessions::failing("the database is locked")),
        Arc::new(NoLiveTurns),
    );
    let refusal = board
        .dispatch(&fresh("explorer", "map the loader"))
        .expect_err("a store failure is not a successful dispatch");
    assert!(matches!(refusal, ContinuationError::Store(_)));
    assert!(refusal.to_string().contains("the database is locked"));
    assert!(
        board.render(PARENT).is_none(),
        "a failed dispatch must not appear on the board"
    );
}

#[test]
fn no_live_turns_reports_every_session_idle() {
    assert!(!NoLiveTurns.is_running("ses_anything"));
}

/// The whole rendered board, pinned.
///
/// A per-field assertion cannot catch a row moving between sections, a heading
/// disappearing, or a state word changing spelling — and the model reads the shape, not
/// the fields. Pinning it makes any of those a deliberate edit.
#[test]
fn the_board_renders_in_one_pinned_shape() {
    let harness = every_state();
    let expected = format!(
        "{BOARD_HEADING}\n\
         {PROSE_IS_NOT_ENOUGH}\n\
         {ACTIVE_IS_NOT_ADDRESSABLE}\n\
         {REUSABLE_RULE}\n\
         \n\
         #### Active\n\
         - exp-1 / ses_child_0001 / explorer / active\n\
         \x20 Job: bg_000001\n\
         \x20 Objective: explorer: map the loader\n\
         - wor-1 / ses_child_0002 / worker / unreconciled\n\
         \x20 Job: bg_000002\n\
         \x20 Objective: worker: rename the field\n\
         \n\
         #### Reusable\n\
         - lib-1 / ses_child_0003 / librarian / reconciled\n\
         \x20 Job: bg_000003\n\
         \x20 Objective: librarian: find the default\n\
         \n\
         #### Closed\n\
         - adv-1 / ses_child_0004 / advisor / failed\n\
         \x20 Job: bg_000004\n\
         \x20 Objective: advisor: review the migration\n\
         - loo-1 / ses_child_0005 / looker / cancelled\n\
         \x20 Job: bg_000005\n\
         \x20 Objective: looker: read the screenshot"
    );
    assert_eq!(harness.board.render(PARENT).expect("a board"), expected);
}
