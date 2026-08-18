//! The base layer's contracts: `Enter`, `Esc`, and what each form resolves with.
//!
//! Every assertion goes through [`DialogHost`], never straight at the dialog. Two
//! reasons, and the second is a recorded failure of this project's own: the host is
//! what forwards resolved actions and pops the stack, so a dialog tested in isolation
//! can pass while being unreachable — and a frame assertion once passed *vacuously*
//! because a dialog covered the row under test. Asserting on the rendered frame with
//! the real host is what makes both visible.

use super::*;
use crate::app::{AppEvent, Component, TerminalEvent, render_offscreen};
use crate::keybind::ActionComponent;
use crate::views::dialog::{DialogHost, ObservedBase};
use crate::views::message::TranscriptView;
use crate::views::testkit::{action, press, rows};
use crossterm::event::KeyCode;

/// A host over a transcript, the shape a session actually uses.
fn host() -> (DialogHost, ViewContext) {
    let context = ViewContext::defaults();
    let base = ObservedBase::new(TranscriptView::new(context.clone()));
    (DialogHost::new(context.clone(), Box::new(base)), context)
}

/// Press whatever the table binds `name` to, through the host.
fn send(host: &mut DialogHost, name: &'static str) {
    host.handle_action(action(name), &press(KeyCode::Null));
}

/// The frame the host draws at `width` × `height`, one string per row.
fn frame(host: &mut DialogHost, width: u16, height: u16) -> Vec<String> {
    rows(&render_offscreen(host, width, height).expect("infallible"))
}

// ---------------------------------------------------------------------------
// ConfirmDialog
// ---------------------------------------------------------------------------

fn confirm(context: ViewContext) -> ConfirmDialog {
    ConfirmDialog::new(
        context,
        "confirm.test",
        "Delete it",
        "This cannot be undone.",
    )
}

#[test]
fn views_confirm_focuses_confirm_and_enter_resolves_with_the_confirm_value() {
    let (mut host, context) = host();
    host.open(Box::new(confirm(context)));

    let before = frame(&mut host, 70, 14).join("\n");
    assert!(
        before.contains("Delete it") && before.contains("This cannot be undone."),
        "the confirmation did not render its question:\n{before}"
    );

    send(&mut host, "dialog.select.submit");
    assert_eq!(
        host.drain_outcomes(),
        vec![(
            "confirm.test",
            DialogOutcome::Selected {
                dialog: "confirm.test",
                value: String::from(CONFIRM_VALUE),
            }
        )],
        "enter on the default focus did not confirm"
    );
    assert!(!host.is_open(), "the confirmation stayed on the stack");
    assert!(
        !frame(&mut host, 70, 14).join("\n").contains("Delete it"),
        "the resolved confirmation is still drawn"
    );
}

#[test]
fn views_confirm_enter_after_switching_focus_cancels_instead() {
    // The half that a "does Enter work" test cannot see: Enter has to mean the *focused*
    // button, so an implementation that always confirms passes the test above.
    let (mut host, context) = host();
    host.open(Box::new(confirm(context)));
    send(&mut host, "dialog.select.next");
    send(&mut host, "dialog.select.submit");
    assert_eq!(
        host.drain_outcomes(),
        vec![("confirm.test", DialogOutcome::Cancelled)],
        "enter on Cancel confirmed anyway"
    );
}

#[test]
fn views_confirm_switching_focus_twice_returns_to_confirm() {
    let (mut host, context) = host();
    host.open(Box::new(confirm(context)));
    send(&mut host, "dialog.select.next");
    send(&mut host, "dialog.select.prev");
    send(&mut host, "dialog.select.submit");
    assert!(matches!(
        host.drain_outcomes().first(),
        Some(("confirm.test", DialogOutcome::Selected { .. }))
    ));
}

#[test]
fn views_confirm_escape_cancels_without_confirming() {
    let (mut host, context) = host();
    host.open(Box::new(confirm(context)));
    // `session_interrupt` is what the table binds escape to. Naming the action rather
    // than the key is the discipline this whole module keeps.
    send(&mut host, "session_interrupt");
    assert_eq!(
        host.drain_outcomes(),
        vec![("confirm.test", DialogOutcome::Cancelled)]
    );
    assert!(!host.is_open());
}

#[test]
fn views_confirm_marks_the_focused_button_with_the_selection_style() {
    // A confirmation whose focus is invisible is a confirmation the user answers by
    // guessing, and the two buttons render as the same text either way — so the style is
    // the only thing that carries it.
    let context = ViewContext::defaults();
    let mut dialog = confirm(context.clone());
    let lines = dialog.lines(58);
    let buttons = lines.last().expect("a button row");
    let selected = context.selected();
    let confirm_span = buttons
        .spans
        .iter()
        .find(|span| span.content.contains("Confirm"))
        .expect("a Confirm button");
    let cancel_span = buttons
        .spans
        .iter()
        .find(|span| span.content.contains("Cancel"))
        .expect("a Cancel button");
    assert_eq!(
        confirm_span.style.bg, selected.bg,
        "the default focus is not painted as selected"
    );
    assert_ne!(
        cancel_span.style.bg, selected.bg,
        "both buttons are painted as focused, so focus is unreadable"
    );
}

#[test]
fn views_confirm_custom_labels_replace_both_buttons() {
    let context = ViewContext::defaults();
    let mut dialog = confirm(context).with_labels("Restore", "Keep");
    let rendered = dialog
        .lines(58)
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
        .collect::<String>();
    assert!(rendered.contains("Restore") && rendered.contains("Keep"));
    assert!(!rendered.contains("Confirm"));
}

// ---------------------------------------------------------------------------
// AlertDialog
// ---------------------------------------------------------------------------

fn alert(context: ViewContext) -> AlertDialog {
    AlertDialog::new(
        context,
        "alert.test",
        "It failed",
        "spawn failed: no such file\n\nNothing was changed.",
    )
}

#[test]
fn views_alert_closes_on_enter() {
    let (mut host, context) = host();
    host.open(Box::new(alert(context)));
    let before = frame(&mut host, 70, 14).join("\n");
    assert!(
        before.contains("spawn failed: no such file"),
        "the alert did not render its message:\n{before}"
    );
    send(&mut host, "dialog.select.submit");
    assert_eq!(
        host.drain_outcomes(),
        vec![("alert.test", DialogOutcome::Cancelled)]
    );
    assert!(!host.is_open());
}

#[test]
fn views_alert_closes_on_escape_too() {
    let (mut host, context) = host();
    host.open(Box::new(alert(context)));
    send(&mut host, "session_interrupt");
    assert_eq!(
        host.drain_outcomes(),
        vec![("alert.test", DialogOutcome::Cancelled)]
    );
    assert!(!host.is_open(), "escape did not dismiss the alert");
}

#[test]
fn views_alert_keeps_a_blank_line_from_the_message_body() {
    // The reason an alert exists rather than a toast: it can carry a diagnostic with
    // structure. A wrapper that dropped the empty line would run the two sentences
    // together, which is what made the transcript version hard to read.
    let context = ViewContext::defaults();
    let mut dialog = alert(context);
    let rendered = dialog
        .lines(58)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>();
    let first = rendered
        .iter()
        .position(|row| row.contains("spawn failed"))
        .expect("the first sentence");
    let second = rendered
        .iter()
        .position(|row| row.contains("Nothing was changed"))
        .expect("the second sentence");
    assert!(
        rendered[first + 1..second].iter().any(String::is_empty),
        "the blank line between the two sentences was dropped: {rendered:?}"
    );
}

// ---------------------------------------------------------------------------
// PromptDialog
// ---------------------------------------------------------------------------

#[test]
fn views_prompt_enter_submits_the_typed_text() {
    let (mut host, context) = host();
    host.open(Box::new(PromptDialog::new(
        context,
        "prompt.test",
        "Rename",
        "before",
    )));
    // Typed characters arrive as ordinary key events, not as actions — the seam a filter
    // box uses. Going through `handle_event` is what proves the host routes them to the
    // dialog and not to the prompt behind it.
    for character in "-after".chars() {
        host.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            crossterm::event::Event::Key(press(KeyCode::Char(character))),
        )));
    }
    let shown = frame(&mut host, 70, 14).join("\n");
    assert!(
        shown.contains("before-after"),
        "the prompt did not show what was typed:\n{shown}"
    );

    send(&mut host, "dialog.prompt.submit");
    assert_eq!(
        host.drain_outcomes(),
        vec![(
            "prompt.test",
            DialogOutcome::Submitted {
                dialog: "prompt.test",
                text: String::from("before-after"),
            }
        )]
    );
    assert!(!host.is_open());
}

#[test]
fn views_prompt_escape_cancels_and_discards_the_text() {
    let (mut host, context) = host();
    host.open(Box::new(PromptDialog::new(
        context,
        "prompt.test",
        "Rename",
        "typed",
    )));
    send(&mut host, "session_interrupt");
    assert_eq!(
        host.drain_outcomes(),
        vec![("prompt.test", DialogOutcome::Cancelled)],
        "escape submitted the text instead of discarding it"
    );
}

#[test]
fn views_prompt_backspace_removes_a_character_and_stops_at_empty() {
    let context = ViewContext::defaults();
    let mut dialog = PromptDialog::new(context, "prompt.test", "Rename", "ab");
    assert_eq!(
        dialog.handle_action(action("input_backspace"), &press(KeyCode::Null)),
        DialogStep::Redraw
    );
    assert_eq!(dialog.text(), "a");
    assert_eq!(
        dialog.handle_action(action("input_backspace"), &press(KeyCode::Null)),
        DialogStep::Redraw
    );
    assert_eq!(dialog.text(), "");
    assert_eq!(
        dialog.handle_action(action("input_backspace"), &press(KeyCode::Null)),
        DialogStep::Ignored,
        "backspace on an empty prompt claimed to have changed something"
    );
}

#[test]
fn views_prompt_submits_empty_text_rather_than_refusing() {
    // Clearing a value is a legitimate answer, and this dialog does not know what the
    // field means. See the module docs.
    let (mut host, context) = host();
    host.open(Box::new(PromptDialog::new(
        context,
        "prompt.test",
        "Rename",
        "",
    )));
    send(&mut host, "dialog.prompt.submit");
    assert_eq!(
        host.drain_outcomes(),
        vec![(
            "prompt.test",
            DialogOutcome::Submitted {
                dialog: "prompt.test",
                text: String::new(),
            }
        )]
    );
}

#[test]
fn views_prompt_keeps_the_tail_of_a_long_value_in_view() {
    // A text area that scrolled the cursor instead of the window hides what the user is
    // typing the moment they pass the third row — measured against `PROMPT_ROWS`.
    let context = ViewContext::defaults();
    let long = "x".repeat(400);
    let mut dialog = PromptDialog::new(context, "prompt.test", "Rename", long);
    let lines = dialog.lines(40);
    assert_eq!(lines.len(), PROMPT_ROWS);
    let last = lines
        .last()
        .expect("a row")
        .spans
        .iter()
        .map(|span| span.content.to_string())
        .collect::<String>();
    assert!(
        last.contains('▏'),
        "the cursor scrolled off the bottom of the box: {last:?}"
    );
}

// ---------------------------------------------------------------------------
// Wide glyphs, in every form
// ---------------------------------------------------------------------------

#[test]
fn views_basics_wrap_measures_terminal_columns_not_characters() {
    // `chars().count()` would fit six CJK glyphs into a six-column row, and the frame
    // would be twelve columns wide with everything under it shifted. The markdown layer
    // has three tests that catch exactly this; the dialog layer needs its own because it
    // wraps with its own helper.
    // From two columns up, `wrap` itself guarantees the bound. At one column a
    // double-width glyph has no correct answer and gets its own over-wide row; see the
    // function's docs, and the rendered assertion below for why nothing overflows.
    for width in 2..12_u16 {
        for row in wrap("日本語のテキストです", width) {
            assert!(
                display_width(&row) <= usize::from(width),
                "wrapping to {width} produced a {}-column row {row:?}",
                display_width(&row)
            );
        }
    }
    // The property that actually reaches a terminal, asserted at every width a dialog can
    // be squeezed to including the degenerate one.
    let context = ViewContext::defaults();
    for width in 1..12_u16 {
        let mut dialog = AlertDialog::new(context.clone(), "a", "t", "日本語のテキストです");
        for line in dialog.lines(width) {
            let used: usize = line
                .spans
                .iter()
                .map(|span| display_width(&span.content))
                .sum();
            assert_eq!(
                used,
                usize::from(width),
                "a {width}-column alert body rendered {used} columns"
            );
        }
    }
    assert_eq!(wrap("日本", 4), vec![String::from("日本")]);
    assert_eq!(
        wrap("日本", 3),
        vec![String::from("日"), String::from("本")]
    );
    assert_eq!(wrap("", 10), vec![String::new()]);
    assert!(wrap("anything", 0).is_empty());
}

#[test]
fn views_basics_every_form_stays_inside_a_medium_dialog_with_wide_glyphs() {
    let context = ViewContext::defaults();
    let cjk = "飞书多维表格操作：建表、字段、记录、视图、统计、公式".repeat(3);
    let width = DialogWidth::Medium.columns() - 2;
    let mut forms: Vec<Box<dyn Dialog>> = vec![
        Box::new(ConfirmDialog::new(
            context.clone(),
            "c",
            cjk.clone(),
            cjk.clone(),
        )),
        Box::new(AlertDialog::new(
            context.clone(),
            "a",
            cjk.clone(),
            cjk.clone(),
        )),
        Box::new(PromptDialog::new(context, "p", cjk.clone(), cjk)),
    ];
    for form in &mut forms {
        for line in form.lines(width) {
            let used: usize = line
                .spans
                .iter()
                .map(|span| display_width(&span.content))
                .sum();
            assert_eq!(
                used,
                usize::from(width),
                "`{}` produced a {used}-column row in a {width}-column body",
                form.id()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The visual probe
// ---------------------------------------------------------------------------

/// Print each form at 120 and 60 columns.
///
/// `#[ignore]`d and run by hand: it asserts nothing, because what it exists for is the
/// class of defect no assertion caught — dropped link URLs, a collapse hint with no key,
/// a truncation that kept the wrong half. Run with
/// `cargo test -p zuno-tui views_basics_visual_probe -- --ignored --nocapture`.
#[test]
#[ignore = "prints frames for a human to read; asserts nothing"]
fn views_basics_visual_probe() {
    let context = ViewContext::defaults();
    for (width, height) in [(120_u16, 16_u16), (60, 16), (40, 12), (20, 10)] {
        for name in ["confirm", "alert", "prompt"] {
            let (mut host, _) = host();
            let dialog: Box<dyn Dialog> = match name {
                "confirm" => Box::new(
                    ConfirmDialog::new(
                        context.clone(),
                        "confirm.undo",
                        "Undo the last turn",
                        "The worktree is restored to the boundary before the last completed \
                         turn. Anything edited since is discarded and cannot be recovered.",
                    )
                    .with_labels("Restore", "Keep"),
                ),
                "alert" => Box::new(AlertDialog::new(
                    context.clone(),
                    "alert.editor",
                    "External editor failed",
                    "spawn `vi` failed: No such file or directory (os error 2)\n\nThe prompt \
                     is unchanged.",
                )),
                _ => Box::new(PromptDialog::new(
                    context.clone(),
                    "prompt.editor.fallback",
                    "Edit prompt (no $EDITOR available)",
                    "explain why the width tiers are fixed columns",
                )),
            };
            host.open(dialog);
            println!("\n=== {name} at {width}x{height} ===");
            for row in frame(&mut host, width, height) {
                println!("|{row}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// What a frame too short to hold the body must not lose
// ---------------------------------------------------------------------------

#[test]
fn views_confirm_keeps_its_buttons_visible_on_a_frame_too_short_for_the_question() {
    // Found by reading the 20×10 frame, not by an assertion: the wrapped question filled
    // the height and pushed `Restore`/`Keep` off the bottom, so a destructive prompt was
    // asking a question with no visible answer and no way to see which button `Enter`
    // would press. Nothing panicked and every width assertion passed.
    let (mut host, context) = host();
    host.open(Box::new(
        ConfirmDialog::new(
            context,
            "confirm.undo",
            "Undo the last turn",
            "The worktree is restored to the boundary before the last completed turn. \
             Anything edited since is discarded and cannot be recovered.",
        )
        .with_labels("Restore", "Keep"),
    ));
    for (width, height) in [(20_u16, 10_u16), (40, 8), (60, 6), (30, 5)] {
        let rendered = frame(&mut host, width, height);
        assert!(
            rendered.iter().any(|row| row.contains("Restore")),
            "the confirm button is off the bottom at {width}x{height}: {rendered:?}"
        );
    }
}

#[test]
fn views_alert_keeps_its_dismiss_button_visible_on_a_short_frame() {
    let (mut host, context) = host();
    host.open(Box::new(AlertDialog::new(
        context,
        "alert.editor",
        "External editor failed",
        "spawn failed: No such file or directory\n\n".to_owned() + &"detail ".repeat(60),
    )));
    for (width, height) in [(20_u16, 10_u16), (40, 8), (60, 6)] {
        let rendered = frame(&mut host, width, height);
        assert!(
            rendered.iter().any(|row| row.contains("Dismiss")),
            "the dismiss button is off the bottom at {width}x{height}: {rendered:?}"
        );
    }
}

#[test]
fn views_prompt_keeps_the_cursor_visible_on_a_short_frame() {
    let (mut host, context) = host();
    host.open(Box::new(PromptDialog::new(
        context,
        "prompt.editor.fallback",
        "Edit prompt",
        "x".repeat(300),
    )));
    for (width, height) in [(20_u16, 10_u16), (40, 8), (60, 5)] {
        let rendered = frame(&mut host, width, height);
        assert!(
            rendered.iter().any(|row| row.contains('▏')),
            "the cursor is off the bottom at {width}x{height}: {rendered:?}"
        );
    }
}

#[test]
fn views_basics_wrap_breaks_between_words_rather_than_inside_one() {
    // Also found by eye: a hard character wrap produced `discar` / `ded` and `os err` /
    // `or 2`. Every row was the right length, so no width assertion had anything to say.
    let rows = wrap(
        "The worktree is restored to the boundary before the last completed turn.",
        30,
    );
    for row in &rows {
        assert!(
            display_width(row) <= 30,
            "word wrapping produced an over-wide row {row:?}"
        );
        assert!(
            !row.starts_with(' ') && !row.ends_with(' '),
            "a wrapped row kept the whitespace it broke on: {row:?}"
        );
    }
    assert_eq!(
        rows.join(" "),
        "The worktree is restored to the boundary before the last completed turn.",
        "word wrapping lost or duplicated text: {rows:?}"
    );

    // A single word longer than the row still has to break somewhere, and the hard break
    // is what keeps that from looping forever.
    let long = wrap(&"w".repeat(70), 20);
    assert_eq!(long.len(), 4);
    assert!(long.iter().all(|row| display_width(row) <= 20));
}
