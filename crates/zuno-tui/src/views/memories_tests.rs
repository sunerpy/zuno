use super::*;
use crate::views::testkit::{action, press};
use crossterm::event::KeyCode;
use zuno_types::{SessionMemoryPolicyProjection, WorkStateProjection};

fn view(policy: SessionMemoryPolicyProjection) -> MemoryPolicyView {
    MemoryPolicyView::new(
        ViewContext::defaults(),
        WorkState::new(WorkStateProjection {
            memory_policy: policy,
            ..WorkStateProjection::default()
        }),
    )
}

fn joined(view: &mut MemoryPolicyView) -> String {
    view.lines(100)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn policy_view_renders_both_independent_controls_and_audit_state() {
    let mut view = view(SessionMemoryPolicyProjection {
        use_memories: true,
        generation: SessionMemoryGeneration::Disabled,
        reason: Some("user choice".to_owned()),
        source: Some("tui".to_owned()),
        time: Some(10),
        revision: 3,
    });

    let body = joined(&mut view);
    assert!(body.contains("[x] Use resident Memory"));
    assert!(body.contains("[ ] Generate learning"));
    assert!(body.contains("revision 3 · source tui"));
    assert!(body.contains("reason user choice"));
}

#[test]
fn policy_view_emits_typed_toggle_outcomes() {
    let mut view = view(SessionMemoryPolicyProjection::default());
    assert_eq!(
        view.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)),
        DialogStep::Emitted(DialogOutcome::MemoryUseSet { enabled: false })
    );
    view.handle_action(action("dialog.select.next"), &press(KeyCode::Right));
    assert_eq!(
        view.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)),
        DialogStep::Emitted(DialogOutcome::MemoryGenerationSet { enabled: false })
    );
}

#[test]
fn excluded_generation_is_visible_but_cannot_be_reenabled() {
    let mut view = view(SessionMemoryPolicyProjection {
        generation: SessionMemoryGeneration::Excluded,
        reason: Some("external context".to_owned()),
        ..SessionMemoryPolicyProjection::default()
    });
    view.handle_action(action("dialog.select.next"), &press(KeyCode::Right));

    assert_eq!(
        view.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter)),
        DialogStep::Redraw
    );
    assert!(joined(&mut view).contains("[!] Learning generation excluded"));
}
