//! The permission prompt: three resolutions, two escalations, and the off-screen
//! assertion.

use super::*;
use crate::app::render_offscreen;
use crate::views::dialog::{DialogHost, DialogPlacement, ObservedBase};
use crate::views::message::TranscriptView;
use crate::views::testkit::{action, press as key, rows};
use crossterm::event::{
    KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use serde_json::json;
use zuno_permission::ToolCall;

fn request(permission: &str) -> PermissionRequest {
    PermissionRequest {
        id: String::from("req_1"),
        session_id: String::from("ses_1"),
        permission: permission.to_owned(),
        patterns: vec![String::from("src/**")],
        metadata: serde_json::Map::new(),
        always: vec![String::from("src/**")],
        tool: None,
    }
}

fn prompt(permission: &str, input: serde_json::Value) -> PermissionPrompt {
    PermissionPrompt::new(ViewContext::defaults(), request(permission), &input)
}

/// Drive one action and require a resolution.
fn resolve(prompt: &mut PermissionPrompt, actions: &[&'static str]) -> DialogOutcome {
    let mut last = None;
    for name in actions {
        last = Some(prompt.handle_action(action(name), &key(KeyCode::Enter)));
    }
    match last {
        Some(DialogStep::Resolved(outcome)) => outcome,
        other => panic!("expected a resolution, got {other:?}"),
    }
}

fn decision(outcome: DialogOutcome) -> PermissionDecision {
    match outcome {
        DialogOutcome::Permission(decision) => decision,
        other => panic!("expected a permission decision, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The three resolutions the plan names
// ---------------------------------------------------------------------------

#[test]
fn views_permission_prompt_resolves_to_once() {
    let mut prompt = prompt("shell", json!({"command": "ls -la"}));
    assert_eq!(prompt.highlighted(), ReplyKind::Once);
    let decision = decision(resolve(&mut prompt, &["dialog.select.submit"]));
    assert_eq!(
        decision,
        PermissionDecision {
            request_id: String::from("req_1"),
            session_id: String::from("ses_1"),
            reply: ReplyKind::Once,
            message: None,
        }
    );
}

#[test]
fn views_permission_prompt_uses_the_composer_unless_explicitly_expanded() {
    let mut prompt = prompt("shell", json!({"command": "git status"}));
    assert_eq!(prompt.placement(), DialogPlacement::Composer);
    assert_eq!(prompt.focused_scopes(), ["dialog.permission"]);

    prompt.handle_action(
        action("permission.prompt.fullscreen"),
        &key(KeyCode::Char('f')),
    );
    assert_eq!(prompt.placement(), DialogPlacement::Overlay);
}

#[test]
fn views_permission_prompt_can_be_decided_with_the_mouse() {
    let mut prompt = prompt("shell", json!({"command": "ls -la"}));
    let body = Rect::new(8, 4, 60, 12);
    let outcome = prompt.handle_mouse(
        &MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 10,
            row: 9,
            modifiers: KeyModifiers::NONE,
        },
        body,
    );
    assert_eq!(
        decision(match outcome {
            DialogStep::Resolved(outcome) => outcome,
            other => panic!("mouse did not resolve permission: {other:?}"),
        })
        .reply,
        ReplyKind::Once
    );
}

#[test]
fn views_permission_prompt_resolves_to_always_after_confirming() {
    let mut prompt = prompt("edit", json!({}));
    // `always` escalates instead of resolving: the confirmation is the point.
    prompt.handle_action(action("dialog.select.next"), &key(KeyCode::Down));
    assert_eq!(prompt.highlighted(), ReplyKind::Always);
    let step = prompt.handle_action(action("dialog.select.submit"), &key(KeyCode::Enter));
    assert_eq!(step, DialogStep::Redraw);
    assert_eq!(prompt.stage(), Stage::ConfirmAlways);

    let decision = decision(resolve(&mut prompt, &["dialog.select.submit"]));
    assert_eq!(decision.reply, ReplyKind::Always);
}

#[test]
fn views_permission_manual_prompt_omits_always() {
    let mut request = request("shell");
    request.always.clear();
    let input = json!({"command": "git push"});
    let joined = render(
        PermissionPrompt::new(ViewContext::defaults(), request.clone(), &input),
        80,
        14,
    )
    .join("\n");
    assert!(joined.contains("Allow once"));
    assert!(joined.contains("Reject"));
    assert!(!joined.contains("Allow always"));

    let mut prompt = PermissionPrompt::new(ViewContext::defaults(), request, &input);
    prompt.handle_action(action("dialog.select.next"), &key(KeyCode::Down));
    assert_eq!(prompt.highlighted(), ReplyKind::Reject);
}

#[test]
fn views_permission_prompt_resolves_to_reject() {
    let mut prompt = prompt("webfetch", json!({"url": "https://example.com"}));
    prompt.handle_action(action("dialog.select.end"), &key(KeyCode::End));
    assert_eq!(prompt.highlighted(), ReplyKind::Reject);
    let decision = decision(resolve(&mut prompt, &["dialog.select.submit"]));
    assert_eq!(decision.reply, ReplyKind::Reject);
    assert_eq!(decision.message, None);
}

#[test]
fn views_permission_escape_rejects_rather_than_allowing() {
    // The highlighted option is `once`, and escape must not take it: a prompt
    // dismissed by accident cannot have granted anything.
    let mut prompt = prompt("shell", json!({}));
    assert_eq!(prompt.highlighted(), ReplyKind::Once);
    let decision = decision(resolve(&mut prompt, &["app_exit"]));
    assert_eq!(decision.reply, ReplyKind::Reject);
}

#[test]
fn views_permission_cancelling_the_always_confirmation_decides_nothing() {
    let mut prompt = prompt("edit", json!({}));
    prompt.handle_action(action("dialog.select.next"), &key(KeyCode::Down));
    prompt.handle_action(action("dialog.select.submit"), &key(KeyCode::Enter));
    assert_eq!(prompt.stage(), Stage::ConfirmAlways);
    let step = prompt.handle_action(action("app_exit"), &key(KeyCode::Esc));
    assert_eq!(
        step,
        DialogStep::Redraw,
        "cancelling the escalation resolved the prompt instead of returning to it"
    );
    assert_eq!(prompt.stage(), Stage::Choose);
}

#[test]
fn views_permission_reject_message_is_only_offered_in_a_child_session() {
    let mut top_level = prompt("shell", json!({}));
    top_level.handle_action(action("dialog.select.end"), &key(KeyCode::End));
    assert!(matches!(
        top_level.handle_action(action("dialog.select.submit"), &key(KeyCode::Enter)),
        DialogStep::Resolved(_)
    ));

    let mut child = prompt("shell", json!({})).with_reject_message(true);
    child.handle_action(action("dialog.select.end"), &key(KeyCode::End));
    let step = child.handle_action(action("dialog.select.submit"), &key(KeyCode::Enter));
    assert_eq!(step, DialogStep::Redraw);
    assert_eq!(child.stage(), Stage::RejectMessage);
}

#[test]
fn views_permission_reject_message_is_typed_and_carried() {
    let mut prompt = prompt("shell", json!({})).with_reject_message(true);
    prompt.handle_action(action("dialog.select.end"), &key(KeyCode::End));
    prompt.handle_action(action("dialog.select.submit"), &key(KeyCode::Enter));
    for character in "use read".chars() {
        prompt.handle_action(action("messages_next"), &key(KeyCode::Char(character)));
    }
    prompt.handle_action(action("input_backspace"), &key(KeyCode::Backspace));
    let decision = decision(resolve(&mut prompt, &["dialog.prompt.submit"]));
    assert_eq!(decision.reply, ReplyKind::Reject);
    assert_eq!(decision.message, Some(String::from("use rea")));
}

#[test]
fn views_permission_empty_reject_message_is_none_not_an_empty_string() {
    let mut prompt = prompt("shell", json!({})).with_reject_message(true);
    prompt.handle_action(action("dialog.select.end"), &key(KeyCode::End));
    prompt.handle_action(action("dialog.select.submit"), &key(KeyCode::Enter));
    let decision = decision(resolve(&mut prompt, &["dialog.prompt.submit"]));
    assert_eq!(decision.message, None);
}

#[test]
fn views_permission_decision_converts_to_the_engine_reply() {
    let reply = PermissionDecision {
        request_id: String::from("req_9"),
        session_id: String::from("ses_1"),
        reply: ReplyKind::Always,
        message: None,
    }
    .into_reply();
    assert_eq!(reply.request_id, "req_9");
    assert_eq!(reply.reply, ReplyKind::Always);
}

#[test]
fn views_permission_prompt_shows_trusted_source_ids_without_rendering_unrelated_metadata() {
    let mut inner = request("mystery_tool");
    inner.session_id = String::from("ses_child_42");
    inner.tool = Some(ToolCall {
        message_id: String::from("msg_child_7"),
        call_id: String::from("call_child_9"),
    });
    inner.metadata.insert(
        String::from("credential"),
        json!("secret-that-must-not-render"),
    );

    let joined = render(
        PermissionPrompt::new(ViewContext::defaults(), inner, &json!({})),
        80,
        16,
    )
    .join("\n");

    assert!(joined.contains("Source session: ses_child_42"), "{joined}");
    assert!(joined.contains("Tool call: call_child_9"), "{joined}");
    assert!(joined.contains("Message: msg_child_7"), "{joined}");
    assert!(!joined.contains("secret-that-must-not-render"), "{joined}");
}

// ---------------------------------------------------------------------------
// The off-screen assertion
// ---------------------------------------------------------------------------

/// Render a prompt through its host, which is how it is drawn in production.
fn render(prompt: PermissionPrompt, width: u16, height: u16) -> Vec<String> {
    let base = ObservedBase::new(TranscriptView::new(ViewContext::defaults()));
    let mut host = DialogHost::new(ViewContext::defaults(), Box::new(base));
    host.open(Box::new(prompt));
    let buffer =
        render_offscreen(&mut host, width, height).expect("the offscreen backend is infallible");
    rows(&buffer)
}

#[test]
fn views_permission_prompt_renders_offscreen() {
    let rows = render(prompt("shell", json!({"command": "rm -rf build"})), 60, 20);
    let joined = rows.join("\n");
    assert!(
        joined.contains("Permission required"),
        "the prompt has no headline:\n{joined}"
    );
    assert!(
        joined.contains("Shell command"),
        "the per-permission title is missing:\n{joined}"
    );
    assert!(
        joined.contains("$ rm -rf build"),
        "the command being approved is not shown, so the prompt is undecidable:\n{joined}"
    );
    for label in ["Allow once", "Allow always", "Reject"] {
        assert!(
            joined.contains(label),
            "the {label:?} option is missing:\n{joined}"
        );
    }
    assert!(
        joined.contains("select") && joined.contains("confirm"),
        "the footer hints are missing:\n{joined}"
    );
}

#[test]
fn views_permission_prompt_highlights_the_cursor_from_the_palette() {
    let context = ViewContext::defaults();
    let base = ObservedBase::new(TranscriptView::new(context.clone()));
    let mut host = DialogHost::new(context.clone(), Box::new(base));
    host.open(Box::new(prompt("shell", json!({}))));
    let buffer = render_offscreen(&mut host, 60, 20).expect("infallible");
    let expected = ratatui::style::Color::from(context.palette().primary);
    let highlighted = (0..buffer.area.height)
        .any(|y| (0..buffer.area.width).any(|x| buffer[(x, y)].bg == expected));
    assert!(
        highlighted,
        "no cell carries the palette's selection background, so the cursor is invisible"
    );
}

#[test]
fn views_permission_always_stage_lists_the_patterns_offscreen() {
    let mut prompt = prompt("edit", json!({}));
    prompt.handle_action(action("dialog.select.next"), &key(KeyCode::Down));
    prompt.handle_action(action("dialog.select.submit"), &key(KeyCode::Enter));
    let joined = render(prompt, 60, 14).join("\n");
    assert!(joined.contains("Always allow"), "{joined}");
    assert!(
        joined.contains("- src/**"),
        "the patterns about to be installed are not shown:\n{joined}"
    );
    assert!(
        joined.contains("Confirm") && joined.contains("Cancel"),
        "{joined}"
    );
}

#[test]
fn views_permission_always_stage_says_so_for_a_blanket_grant() {
    let mut inner = request("shell");
    inner.always = vec![String::from("*")];
    let mut prompt = PermissionPrompt::new(ViewContext::defaults(), inner, &json!({}));
    prompt.handle_action(action("dialog.select.next"), &key(KeyCode::Down));
    prompt.handle_action(action("dialog.select.submit"), &key(KeyCode::Enter));
    let joined = render(prompt, 70, 12).join("\n");
    assert!(
        joined.contains("This will allow shell until Zuno is restarted"),
        "a `*` grant rendered as a pattern list instead of as a blanket grant:\n{joined}"
    );
}

#[test]
fn views_permission_edit_renders_the_diff_it_is_approving() {
    let mut inner = request("edit");
    inner.patterns = vec![String::from("src/main.rs")];
    let prompt = PermissionPrompt::new(
        ViewContext::defaults(),
        inner,
        &json!({
            "filePath": "src/main.rs",
            "oldString": "let a = 1;",
            "newString": "let a = 2;"
        }),
    );
    let joined = render(prompt, 70, 20).join("\n");
    assert!(joined.contains("Edit src/main.rs"), "{joined}");
    assert!(
        joined.contains("let a = 1;") && joined.contains("let a = 2;"),
        "the diff did not render, so the user is approving an unseen change:\n{joined}"
    );
}

#[test]
fn views_permission_fullscreen_toggle_changes_the_requested_height() {
    let mut prompt = prompt("shell", json!({}));
    assert!(!prompt.is_expanded());
    assert_eq!(
        prompt.desired_height(30, 40),
        15,
        "a collapsed prompt is capped at fifteen rows"
    );
    prompt.handle_action(
        action("permission.prompt.fullscreen"),
        &key(KeyCode::Char('f')),
    );
    assert!(prompt.is_expanded());
    // Expanding lifts the cap; the frame is the ceiling rather than the height. This
    // assertion used to demand the whole 40 rows for 30 rows of content, and that is
    // exactly the behaviour that painted thirty-eight blank rows around a three-line
    // diff on a 200×50 terminal.
    assert_eq!(
        prompt.desired_height(30, 40),
        32,
        "an expanded prompt should fit its content, not stretch to the frame"
    );
    assert_eq!(
        prompt.desired_height(80, 40),
        40,
        "content longer than the frame must still be allowed all of it"
    );
}

#[test]
fn views_permission_fullscreen_renders_the_edit_subject_and_diff() {
    let mut inner = request("edit");
    inner.patterns = vec![String::from("src/fullscreen.rs")];
    let mut prompt = PermissionPrompt::new(
        ViewContext::defaults(),
        inner,
        &json!({
            "filePath": "src/fullscreen.rs",
            "oldString": "before_fullscreen",
            "newString": "after_fullscreen"
        }),
    );
    prompt.handle_action(
        action("permission.prompt.fullscreen"),
        &key(KeyCode::Char('f')),
    );

    let joined = render(prompt, 80, 30).join("\n");
    assert!(
        joined.contains("Edit src/fullscreen.rs"),
        "fullscreen dropped the edit subject:\n{joined}"
    );
    assert!(
        joined.contains("before_fullscreen") && joined.contains("after_fullscreen"),
        "fullscreen dropped the diff:\n{joined}"
    );
    assert!(
        joined.contains("ctrl+f minimize"),
        "fullscreen did not render its active footer:\n{joined}"
    );
}

// ---------------------------------------------------------------------------
// Per-permission descriptions
// ---------------------------------------------------------------------------

#[test]
fn views_permission_describe_covers_every_oracle_branch() {
    let cases: Vec<(&str, serde_json::Value, &str, &str)> = vec![
        (
            "edit",
            json!({
                "filePath": "src/edit.rs",
                "oldString": "old",
                "newString": "new"
            }),
            "→",
            "Edit src/edit.rs",
        ),
        ("read", json!({"filePath": "a.rs"}), "→", "Read a.rs"),
        (
            "glob",
            json!({"pattern": "**/*.rs"}),
            "✱",
            "Glob \"**/*.rs\"",
        ),
        ("grep", json!({"pattern": "TODO"}), "✱", "Grep \"TODO\""),
        ("list", json!({"path": "src"}), "→", "List src"),
        ("shell", json!({"command": "ls"}), "#", "Shell command"),
        (
            "task",
            json!({"subagent_type": "explore", "description": "find it"}),
            "#",
            "Explore Task",
        ),
        (
            "webfetch",
            json!({"url": "https://x"}),
            "%",
            "WebFetch https://x",
        ),
        (
            "web_search",
            json!({"queries": ["rust", "cargo"]}),
            "◈",
            "Web search \"rust, cargo\"",
        ),
        (
            "doom_loop",
            json!({}),
            "⟳",
            "Continue after repeated failures",
        ),
        ("mystery_tool", json!({}), "⚙", "Call tool mystery_tool"),
    ];
    for (permission, input, icon, title) in cases {
        let inner = request(permission);
        let subject = describe(&inner, &input);
        assert_eq!(subject.icon, icon, "wrong icon for {permission}");
        assert_eq!(subject.title, title, "wrong title for {permission}");
        assert!(
            !subject.title.trim().is_empty(),
            "{permission} produced an empty subject"
        );
        let joined = render(
            PermissionPrompt::new(ViewContext::defaults(), inner, &input),
            90,
            20,
        )
        .join("\n");
        assert!(
            joined.contains(title),
            "{permission} did not render its non-empty subject:\n{joined}"
        );
    }
}

#[test]
fn views_permission_external_directory_renders_a_non_empty_subject() {
    let mut inner = request("external_directory");
    inner.patterns = vec![String::from("/tmp/work/**")];
    let expected = "Access external directory /tmp/work";
    let subject = describe(&inner, &json!({}));
    assert_eq!(subject.title, expected);
    let joined = render(
        PermissionPrompt::new(ViewContext::defaults(), inner, &json!({})),
        90,
        20,
    )
    .join("\n");
    assert!(
        joined.contains(expected),
        "external_directory did not render its subject:\n{joined}"
    );
}

#[test]
fn views_permission_footer_advertises_horizontal_selection_and_keeps_vertical_aliases() {
    let mut prompt = prompt("shell", json!({"command": "true"}));
    assert!(
        prompt.hints().contains(&("←→", "select")),
        "the horizontal choice row must advertise Left/Right"
    );

    assert_eq!(prompt.highlighted(), ReplyKind::Once);
    prompt.handle_action(action("dialog.permission.next"), &key(KeyCode::Right));
    assert_eq!(prompt.highlighted(), ReplyKind::Always);
    prompt.handle_action(action("dialog.permission.prev"), &key(KeyCode::Left));
    assert_eq!(prompt.highlighted(), ReplyKind::Once);

    prompt.handle_action(action("dialog.select.next"), &key(KeyCode::Down));
    assert_eq!(prompt.highlighted(), ReplyKind::Always);
    prompt.handle_action(action("dialog.select.prev"), &key(KeyCode::Up));
    assert_eq!(prompt.highlighted(), ReplyKind::Once);
}

#[test]
fn views_permission_external_directory_reduces_a_wildcard_to_its_parent() {
    let mut inner = request("external_directory");
    inner.patterns = vec![String::from("/tmp/work/**")];
    let subject = describe(&inner, &json!({}));
    assert_eq!(
        subject.title, "Access external directory /tmp/work",
        "a glob was shown to the user instead of the directory it covers"
    );
    assert_eq!(subject.detail, vec![String::from("- /tmp/work/**")]);
}

#[test]
fn views_permission_external_directory_prefers_the_metadata_parent() {
    let mut inner = request("external_directory");
    inner
        .metadata
        .insert(String::from("parentDir"), json!("/opt/data"));
    inner.patterns = vec![String::from("/tmp/other/**")];
    assert_eq!(
        describe(&inner, &json!({})).title,
        "Access external directory /opt/data"
    );
}

#[test]
fn views_permission_describe_tolerates_missing_arguments() {
    // A permission ask can arrive before the arguments finish streaming.
    let subject = describe(&request("read"), &json!({}));
    assert_eq!(subject.title, "Read ");
    assert!(subject.detail.is_empty());
}

#[test]
fn views_permission_typed_character_rejects_a_control_chord() {
    let plain = KeyEvent {
        code: KeyCode::Char('a'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    let control = KeyEvent {
        modifiers: KeyModifiers::CONTROL,
        ..plain
    };
    assert_eq!(typed_character(&plain), Some('a'));
    assert_eq!(
        typed_character(&control),
        None,
        "ctrl+a was treated as typed text, so a chord would insert a letter"
    );
}

#[test]
fn permission_expanded_prompt_does_not_inflate_a_short_body_to_the_whole_frame() {
    // Measured at 200×50: pressing the fullscreen key on a three-line diff produced
    // thirty-eight blank rows inside the prompt's border, which reads as a broken frame.
    // Expanding must lift the cap, not stretch the content.
    let mut prompt = PermissionPrompt::new(
        ViewContext::defaults(),
        request("edit"),
        &serde_json::json!({
            "filePath": "/work/demo.txt",
            "oldString": "second line\n",
            "newString": "second line, amended\nan inserted line\n"
        }),
    );
    let rows = u16::try_from(prompt.lines(200).len()).expect("a row count");
    let collapsed = prompt.desired_height(rows, 50);
    assert!(
        collapsed <= COLLAPSED_MAX_ROWS,
        "a collapsed prompt must stay capped: {collapsed}"
    );

    prompt.handle_action(
        crate::views::testkit::action("permission.prompt.fullscreen"),
        &crate::views::testkit::press(crossterm::event::KeyCode::Char('f')),
    );
    assert!(prompt.is_expanded());
    let expanded = prompt.desired_height(rows, 50);
    assert!(
        expanded < 50,
        "an expanded short prompt claimed the whole frame: {expanded}"
    );
    assert!(
        expanded >= collapsed,
        "expanding made the prompt smaller: {expanded} < {collapsed}"
    );
}

#[test]
fn permission_expanded_prompt_uses_the_frame_when_the_diff_is_genuinely_long() {
    // The other half: expanding exists so a long diff stops being truncated at fifteen
    // rows. A version that always fitted the content would cap nothing.
    let mut prompt = PermissionPrompt::new(
        ViewContext::defaults(),
        request("edit"),
        &serde_json::json!({
            "filePath": "/work/demo.txt",
            "oldString": "old\n".repeat(60),
            "newString": "new\n".repeat(60)
        }),
    );
    let rows = u16::try_from(prompt.lines(200).len()).expect("a row count");
    assert!(
        rows > COLLAPSED_MAX_ROWS,
        "the fixture is not long enough: {rows}"
    );
    assert_eq!(
        prompt.desired_height(rows, 50),
        COLLAPSED_MAX_ROWS,
        "a long collapsed diff must be capped"
    );
    prompt.handle_action(
        crate::views::testkit::action("permission.prompt.fullscreen"),
        &crate::views::testkit::press(crossterm::event::KeyCode::Char('f')),
    );
    assert_eq!(
        prompt.desired_height(rows, 50),
        50,
        "an expanded long diff must be allowed the whole frame"
    );
}
