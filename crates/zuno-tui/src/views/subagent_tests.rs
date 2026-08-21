//! What a host needs to trust about the delegated-task view.

use super::*;
use crate::views::message::{Message, Role};
use crate::views::testkit::{action, press};
use crossterm::event::KeyCode;

/// A completed `task` call as the transcript records one.
///
/// The output is the exact envelope `zuno_tools::task::render` emits for a foreground
/// delegation, so a change to that shape breaks this rather than passing on a shape the
/// tool no longer produces.
fn delegated(call_id: &str, agent: &str, description: &str, session: &str) -> MessagePart {
    MessagePart::Tool {
        call_id: call_id.to_owned(),
        name: TASK_TOOL.to_owned(),
        arguments: format!(
            r#"{{"description":"{description}","prompt":"do it","subagent_type":"{agent}"}}"#
        ),
        title: Some(format!("task {agent}")),
        status: ToolStatus::Completed,
        output: Some(format!(
            "<task id=\"{session}\" state=\"completed\">\n<task_result>\nthe answer\n</task_result>\n</task>"
        )),
        diff: None,
    }
}

fn message_with(parts: Vec<MessagePart>) -> Message {
    Message {
        role: Role::Assistant,
        id: Some(String::from("msg_1")),
        parts,
    }
}

fn view(tasks: Vec<Delegation>) -> SubagentView {
    SubagentView::new(ViewContext::defaults(), tasks)
}

#[test]
fn a_task_call_in_the_transcript_becomes_a_delegation_row() {
    let messages = vec![message_with(vec![delegated(
        "call_1",
        "explore",
        "survey the auth code",
        "ses_child_1",
    )])];

    let found = delegations(&messages);

    assert_eq!(found.len(), 1, "{found:#?}");
    let task = &found[0];
    assert_eq!(task.call_id, "call_1");
    assert_eq!(task.agent.as_deref(), Some("explore"));
    assert_eq!(task.objective.as_deref(), Some("survey the auth code"));
    assert_eq!(task.status, ToolStatus::Completed);
    assert_eq!(
        task.session_id.as_deref(),
        Some("ses_child_1"),
        "the child session id has to be recovered from the envelope, or the view names \
         no session to open"
    );
    assert_eq!(task.state.as_deref(), Some("completed"));
}

#[test]
fn other_tools_are_not_delegations() {
    let messages = vec![message_with(vec![
        MessagePart::Tool {
            call_id: String::from("call_read"),
            name: String::from("read"),
            arguments: String::from(r#"{"filePath":"/tmp/x"}"#),
            title: None,
            status: ToolStatus::Completed,
            output: Some(String::from("contents")),
            diff: None,
        },
        delegated("call_task", "worker", "do the thing", "ses_child_2"),
    ])];

    let found = delegations(&messages);

    assert_eq!(
        found.len(),
        1,
        "only the task call is a delegation: {found:#?}"
    );
    assert_eq!(found[0].call_id, "call_task");
}

/// The view lists every delegation and left/right move between them, wrapping.
///
/// This is the assertion behind 所有功能都要完整可用: the keys the binding table
/// advertises as "next/previous child session" have to actually move the cursor, in both
/// directions, including off each end.
#[test]
fn left_and_right_move_between_tasks_and_wrap_at_both_ends() {
    let messages = vec![message_with(vec![
        delegated("call_1", "explore", "first task", "ses_a"),
        delegated("call_2", "worker", "second task", "ses_b"),
        delegated("call_3", "librarian", "third task", "ses_c"),
    ])];
    let mut view = view(delegations(&messages));
    assert_eq!(view.len(), 3);
    assert_eq!(view.cursor(), 0);

    view.handle_action(action("session_child_cycle"), &press(KeyCode::Right));
    assert_eq!(view.cursor(), 1, "right moves to the next task");
    assert_eq!(
        view.selected().and_then(|t| t.agent.clone()).as_deref(),
        Some("worker")
    );

    view.handle_action(action("session_child_cycle"), &press(KeyCode::Right));
    assert_eq!(view.cursor(), 2);

    view.handle_action(action("session_child_cycle"), &press(KeyCode::Right));
    assert_eq!(
        view.cursor(),
        0,
        "right off the last task wraps to the first"
    );

    view.handle_action(action("session_child_cycle_reverse"), &press(KeyCode::Left));
    assert_eq!(
        view.cursor(),
        2,
        "left off the first task wraps to the last, or the key is dead at that edge"
    );
}

#[test]
fn the_selected_task_is_the_one_rendered_in_detail() {
    let messages = vec![message_with(vec![
        delegated("call_1", "explore", "first task", "ses_a"),
        delegated("call_2", "worker", "second task", "ses_b"),
    ])];
    let mut view = view(delegations(&messages));
    view.handle_action(action("session_child_cycle"), &press(KeyCode::Right));

    let body = view.lines(60);
    let joined = body
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        joined.contains("ses_b"),
        "the detail body must describe the selected task:\n{joined}"
    );
    assert!(
        joined.contains(CHILD_TRANSCRIPT_NOTE),
        "the view has to say where the child's own messages are:\n{joined}"
    );
}

/// A session that delegated nothing says so, rather than showing an empty panel.
#[test]
fn a_session_with_no_delegations_says_so() {
    let mut view = view(Vec::new());
    assert!(view.is_empty());

    let body = view.lines(60);
    let joined = body
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(joined.contains(EMPTY), "{joined}");
}

/// Escape-equivalents close the view, so it is never a surface without an exit.
#[test]
fn the_view_can_be_left() {
    let messages = vec![message_with(vec![delegated(
        "call_1", "explore", "task", "ses_a",
    )])];
    let mut view = view(delegations(&messages));

    let step = view.handle_action(action("session_parent"), &press(KeyCode::Up));

    assert_eq!(step, DialogStep::Resolved(DialogOutcome::Cancelled));
}

/// A refused background delegation still gets a row, and says why it has no session.
#[test]
fn a_failed_delegation_is_listed_with_no_session_rather_than_dropped() {
    let messages = vec![message_with(vec![MessagePart::Tool {
        call_id: String::from("call_bg"),
        name: TASK_TOOL.to_owned(),
        arguments: String::from(
            r#"{"description":"run it in the background","prompt":"go","background":true}"#,
        ),
        title: None,
        status: ToolStatus::Error,
        output: Some(String::from(
            "background delegation is not available in this build",
        )),
        diff: None,
    }])];
    let found = delegations(&messages);

    assert_eq!(found.len(), 1, "a failed delegation is still a delegation");
    assert!(
        found[0].background,
        "the background request has to be visible"
    );
    assert_eq!(found[0].session_id, None);

    let mut view = view(found);
    let joined = view
        .lines(70)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("did not run"),
        "a row with no session must say why:\n{joined}"
    );
}

/// Every width this task must survive, including the one `u16::clamp` panics on.
///
/// `lines` is where a width computation lives, and a `clamp` whose minimum exceeded its
/// maximum is how a 20-column frame panics. `saturating_sub` is what this view uses
/// instead; this asserts the outcome rather than the choice.
#[test]
fn the_view_renders_without_panicking_at_every_required_width() {
    let messages = vec![message_with(vec![
        delegated(
            "call_1",
            "explore",
            "a description long enough to need truncating at every width",
            "ses_a",
        ),
        delegated("call_2", "worker", "second", "ses_b"),
    ])];

    for width in [80, 120, 20, 1, 0] {
        let mut populated = view(delegations(&messages));
        let _lines = populated.lines(width);
        let _title = populated.title();
        let _hints = populated.hints();

        let mut empty = view(Vec::new());
        let _empty_lines = empty.lines(width);
        let _empty_title = empty.title();
    }
}
