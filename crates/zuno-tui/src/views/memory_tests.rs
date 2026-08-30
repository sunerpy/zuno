use super::*;
use crate::views::testkit::{action, press};
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use zuno_types::{MemoryScope, MemorySource, WorkStateProjection};

fn candidate(id: &str, status: MemoryCandidateStatus) -> MemoryCandidateProjection {
    MemoryCandidateProjection {
        id: id.to_owned(),
        scope: MemoryScope::Project,
        action: MemoryAction::Add,
        content: Some(format!("entry {id}")),
        old_text: None,
        reason: "verified correction".to_owned(),
        confidence: 9_500,
        source: MemorySource::Reflection,
        source_session_id: Some("ses_source".to_owned()),
        source_message_id: Some("msg_source".to_owned()),
        status,
        error: (status == MemoryCandidateStatus::Uncertain)
            .then(|| "resident state diverged".to_owned()),
        time_created: 1,
        time_updated: 2,
    }
}

fn view(state: WorkStateProjection) -> MemoryView {
    MemoryView::new(ViewContext::defaults(), WorkState::new(state))
}

fn joined(view: &mut MemoryView) -> String {
    view.lines(100)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn memory_view_is_explicit_when_empty_and_reflects_live_projection_updates() {
    let state = WorkState::default();
    let mut view = MemoryView::new(ViewContext::defaults(), state.clone());
    assert!(joined(&mut view).contains(EMPTY));

    state.replace(WorkStateProjection {
        memory_candidates: vec![candidate("pending", MemoryCandidateStatus::Pending)],
        ..WorkStateProjection::default()
    });
    assert!(joined(&mut view).contains("entry pending"));
}

#[test]
fn memory_view_renders_every_durable_candidate_state() {
    let statuses = [
        MemoryCandidateStatus::Pending,
        MemoryCandidateStatus::Applying,
        MemoryCandidateStatus::Undoing,
        MemoryCandidateStatus::Applied,
        MemoryCandidateStatus::Rejected,
        MemoryCandidateStatus::Undone,
        MemoryCandidateStatus::Failed,
        MemoryCandidateStatus::Uncertain,
    ];
    let mut view = view(WorkStateProjection {
        memory_candidates: statuses
            .into_iter()
            .enumerate()
            .map(|(index, status)| candidate(&format!("{index}"), status))
            .collect(),
        ..WorkStateProjection::default()
    });

    let body = joined(&mut view);
    for glyph in ["○", "…", "✓", "×", "↶", "!", "?"] {
        assert!(
            body.contains(glyph),
            "missing state glyph `{glyph}`:\n{body}"
        );
    }
    assert_eq!(
        view.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)),
        DialogStep::Redraw
    );
    let details = joined(&mut view);
    for expected in [
        "status pending",
        "confidence 95%",
        "source reflection",
        "session ses_source",
        "message msg_source",
    ] {
        assert!(
            details.contains(expected),
            "missing `{expected}`:\n{details}"
        );
    }
}

#[test]
fn memory_view_mouse_wheel_moves_the_selection() {
    let mut view = view(WorkStateProjection {
        memory_candidates: vec![
            candidate("first", MemoryCandidateStatus::Pending),
            candidate("second", MemoryCandidateStatus::Pending),
        ],
        ..WorkStateProjection::default()
    });
    assert_eq!(
        view.handle_mouse(
            &MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 4,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            ratatui::layout::Rect::new(0, 0, 80, 20),
        ),
        DialogStep::Redraw
    );
    assert_eq!(view.cursor, 1);
}

#[test]
fn pending_candidate_actions_emit_without_closing_or_mutating_the_list() {
    let projection = WorkStateProjection {
        memory_candidates: vec![candidate("pending", MemoryCandidateStatus::Pending)],
        ..WorkStateProjection::default()
    };
    let mut approve = view(projection.clone());
    assert_eq!(
        approve.handle_action(action("memory_apply"), &press(KeyCode::Char('a'))),
        DialogStep::Emitted(DialogOutcome::MemoryApply {
            id: "pending".to_owned()
        })
    );
    assert_eq!(approve.items().len(), 1);

    let mut reject = view(projection.clone());
    assert_eq!(
        reject.handle_action(action("memory_reject"), &press(KeyCode::Char('r'))),
        DialogStep::Emitted(DialogOutcome::MemoryReject {
            id: "pending".to_owned()
        })
    );

    let mut edit = view(projection);
    assert_eq!(
        edit.handle_action(action("memory_edit"), &press(KeyCode::Char('e'))),
        DialogStep::Emitted(DialogOutcome::MemoryEditRequested {
            id: "pending".to_owned(),
            content: "entry pending".to_owned(),
        })
    );
}

#[test]
fn applied_candidate_can_be_undone_and_saved_entry_requires_double_x() {
    let mut undo = view(WorkStateProjection {
        memory_candidates: vec![candidate("applied", MemoryCandidateStatus::Applied)],
        ..WorkStateProjection::default()
    });
    assert_eq!(
        undo.handle_action(action("memory_undo"), &press(KeyCode::Char('u'))),
        DialogStep::Emitted(DialogOutcome::MemoryUndo {
            id: "applied".to_owned()
        })
    );

    let mut remove = view(WorkStateProjection {
        memory_entries: vec![MemoryEntryProjection {
            scope: MemoryScope::Global,
            content: "prefer concise answers".to_owned(),
        }],
        ..WorkStateProjection::default()
    });
    assert_eq!(
        remove.handle_action(action("memory_remove"), &press(KeyCode::Char('x'))),
        DialogStep::Redraw
    );
    assert!(joined(&mut remove).contains("press x again"));
    assert_eq!(
        remove.handle_action(action("memory_remove"), &press(KeyCode::Char('x'))),
        DialogStep::Emitted(DialogOutcome::MemoryRemove {
            scope: MemoryScope::Global,
            content: "prefer concise answers".to_owned(),
        })
    );
    assert_eq!(remove.items().len(), 1);
}
