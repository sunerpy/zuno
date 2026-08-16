//! Input editor tests: motion, deletion, selection, undo, history, and the
//! off-screen assertion.

use super::*;
use crate::app::render_offscreen;
use crate::views::testkit::{action, rows};

fn editor() -> InputEditor {
    InputEditor::new(ViewContext::defaults())
}

fn typing(text: &str) -> InputEditor {
    let mut editor = editor();
    for character in text.chars() {
        editor.insert_char(character);
    }
    editor
}

fn act(editor: &mut InputEditor, names: &[&'static str]) {
    for name in names {
        editor.handle_action(action(name));
    }
}

// ---------------------------------------------------------------------------
// The off-screen assertion
// ---------------------------------------------------------------------------

#[test]
fn views_input_editor_renders_offscreen_with_its_cursor() {
    let mut editor = typing("hello");
    let buffer = render_offscreen(&mut editor, 12, 2).expect("infallible");
    let rendered = rows(&buffer);
    assert_eq!(
        rendered[0], "hello▏",
        "the buffer text or the cursor glyph is missing: {rendered:?}"
    );
}

#[test]
fn views_input_editor_renders_multiple_lines_offscreen() {
    let mut editor = editor();
    editor.insert_text("first\nsecond\nthird");
    assert_eq!(editor.height(), 3);
    let rendered = rows(&render_offscreen(&mut editor, 12, 4).expect("infallible"));
    assert_eq!(&rendered[..3], ["first", "second", "third▏"]);
}

#[test]
fn views_input_editor_scrolls_to_keep_the_cursor_visible() {
    let mut editor = editor();
    editor.insert_text("a\nb\nc\nd\ne");
    // The area holds two rows and the cursor is on the fifth line, so the first
    // rendered row has to be the fourth.
    let rendered = rows(&render_offscreen(&mut editor, 8, 2).expect("infallible"));
    assert_eq!(rendered, vec![String::from("d"), String::from("e▏")]);
}

#[test]
fn views_input_editor_paints_from_the_palette() {
    let context = ViewContext::defaults();
    let mut editor = InputEditor::new(context.clone());
    editor.insert_char('x');
    let buffer = render_offscreen(&mut editor, 6, 1).expect("infallible");
    assert_eq!(
        buffer[(0, 0)].fg,
        ratatui::style::Color::from(context.palette.text)
    );
    assert_eq!(
        buffer[(1, 0)].fg,
        ratatui::style::Color::from(context.palette.border_active),
        "the cursor glyph did not use the palette's active border colour"
    );
}

#[test]
fn views_input_editor_renders_a_selection_from_the_palette() {
    let context = ViewContext::defaults();
    let mut editor = InputEditor::new(context.clone());
    editor.insert_text("abcd");
    act(&mut editor, &["input_line_home"]);
    act(&mut editor, &["input_select_right", "input_select_right"]);
    let buffer = render_offscreen(&mut editor, 8, 1).expect("infallible");
    assert_eq!(
        buffer[(0, 0)].bg,
        ratatui::style::Color::from(context.palette.primary),
        "the selected cell does not carry the selection background"
    );
}

// ---------------------------------------------------------------------------
// Insertion and multi-line
// ---------------------------------------------------------------------------

#[test]
fn views_input_editor_starts_empty() {
    let editor = editor();
    assert!(editor.is_empty());
    assert_eq!(editor.text(), "");
    assert_eq!(editor.cursor(), Position { line: 0, column: 0 });
}

#[test]
fn views_input_newline_splits_the_line_at_the_cursor() {
    let mut editor = typing("abcd");
    act(&mut editor, &["input_move_left", "input_move_left"]);
    act(&mut editor, &["input_newline"]);
    assert_eq!(editor.text(), "ab\ncd");
    assert_eq!(editor.cursor(), Position { line: 1, column: 0 });
}

#[test]
fn views_input_editor_handles_multibyte_text_without_splitting_a_character() {
    let mut editor = typing("日本語");
    assert_eq!(editor.cursor().column, 3);
    act(&mut editor, &["input_move_left"]);
    assert_eq!(editor.cursor().column, 2);
    act(&mut editor, &["input_backspace"]);
    assert_eq!(
        editor.text(),
        "日語",
        "a multi-byte character was cut in half"
    );
}

#[test]
fn views_input_paste_keeps_the_line_structure() {
    let mut editor = editor();
    editor.insert_text("#!/bin/sh\nset -e\necho ok");
    assert_eq!(editor.height(), 3);
    assert_eq!(editor.text(), "#!/bin/sh\nset -e\necho ok");
}

// ---------------------------------------------------------------------------
// Motion
// ---------------------------------------------------------------------------

#[test]
fn views_input_motion_walks_lines_and_clamps_at_the_ends() {
    let mut editor = editor();
    editor.insert_text("ab\ncdef");
    act(&mut editor, &["input_buffer_home"]);
    assert_eq!(editor.cursor(), Position { line: 0, column: 0 });
    act(&mut editor, &["input_move_left"]);
    assert_eq!(
        editor.cursor(),
        Position { line: 0, column: 0 },
        "moving left off the start of the buffer moved somewhere"
    );
    act(&mut editor, &["input_move_down"]);
    assert_eq!(editor.cursor(), Position { line: 1, column: 0 });
    act(&mut editor, &["input_line_end"]);
    assert_eq!(editor.cursor(), Position { line: 1, column: 4 });
    act(&mut editor, &["input_move_right"]);
    assert_eq!(editor.cursor(), Position { line: 1, column: 4 });
}

#[test]
fn views_input_moving_right_off_a_line_end_wraps_to_the_next_line() {
    let mut editor = editor();
    editor.insert_text("ab\ncd");
    act(&mut editor, &["input_buffer_home", "input_line_end"]);
    act(&mut editor, &["input_move_right"]);
    assert_eq!(editor.cursor(), Position { line: 1, column: 0 });
}

#[test]
fn views_input_moving_up_clamps_the_column_to_the_shorter_line() {
    let mut editor = editor();
    editor.insert_text("ab\nlonger");
    assert_eq!(editor.cursor(), Position { line: 1, column: 6 });
    act(&mut editor, &["input_move_up"]);
    assert_eq!(editor.cursor(), Position { line: 0, column: 2 });
}

#[test]
fn views_input_word_motion_skips_punctuation_then_the_word() {
    let mut editor = typing("foo  bar-baz");
    act(&mut editor, &["input_line_home"]);
    act(&mut editor, &["input_word_forward"]);
    assert_eq!(editor.cursor().column, 3, "the first word was not crossed");
    act(&mut editor, &["input_word_forward"]);
    assert_eq!(editor.cursor().column, 8);
    act(&mut editor, &["input_word_backward"]);
    assert_eq!(editor.cursor().column, 5);
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

#[test]
fn views_input_selection_extends_with_select_actions_and_drops_on_a_plain_move() {
    let mut editor = typing("abcdef");
    act(&mut editor, &["input_line_home"]);
    act(&mut editor, &["input_select_right", "input_select_right"]);
    assert_eq!(editor.selection(), Some(String::from("ab")));
    act(&mut editor, &["input_move_right"]);
    assert_eq!(
        editor.selection(),
        None,
        "a plain move kept the selection, so the next keystroke would delete it"
    );
}

#[test]
fn views_input_selection_spans_lines() {
    let mut editor = editor();
    editor.insert_text("one\ntwo\nthree");
    act(&mut editor, &["input_buffer_home"]);
    act(
        &mut editor,
        &[
            "input_select_down",
            "input_select_down",
            "input_select_right",
        ],
    );
    assert_eq!(editor.selection(), Some(String::from("one\ntwo\nt")));
}

#[test]
fn views_input_select_all_covers_the_whole_buffer() {
    let mut editor = editor();
    editor.insert_text("a\nb");
    act(&mut editor, &["input_select_all"]);
    assert_eq!(editor.selection(), Some(String::from("a\nb")));
    assert_eq!(editor.anchor(), Some(Position { line: 0, column: 0 }));
}

#[test]
fn views_input_typing_over_a_selection_replaces_it() {
    let mut editor = typing("abcd");
    act(&mut editor, &["input_select_all"]);
    editor.insert_char('x');
    assert_eq!(editor.text(), "x");
}

#[test]
fn views_input_backspace_over_a_selection_deletes_the_whole_range() {
    let mut editor = editor();
    editor.insert_text("keep\ndrop");
    act(&mut editor, &["input_select_line_home"]);
    act(&mut editor, &["input_backspace"]);
    assert_eq!(editor.text(), "keep\n");
}

// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------

#[test]
fn views_input_backspace_at_a_line_start_joins_the_previous_line() {
    let mut editor = editor();
    editor.insert_text("ab\ncd");
    act(&mut editor, &["input_line_home"]);
    act(&mut editor, &["input_backspace"]);
    assert_eq!(editor.text(), "abcd");
    assert_eq!(editor.cursor(), Position { line: 0, column: 2 });
}

#[test]
fn views_input_delete_at_a_line_end_pulls_the_next_line_up() {
    let mut editor = editor();
    editor.insert_text("ab\ncd");
    act(&mut editor, &["input_buffer_home", "input_line_end"]);
    act(&mut editor, &["input_delete"]);
    assert_eq!(editor.text(), "abcd");
}

#[test]
fn views_input_delete_line_and_the_kill_actions() {
    let mut editor = editor();
    editor.insert_text("one\ntwo\nthree");
    act(&mut editor, &["input_delete_line"]);
    assert_eq!(editor.text(), "one\ntwo");

    act(&mut editor, &["input_line_end", "input_move_left"]);
    act(&mut editor, &["input_delete_to_line_end"]);
    assert_eq!(editor.text(), "one\ntw");
    act(&mut editor, &["input_delete_to_line_start"]);
    assert_eq!(editor.text(), "one\n");
}

#[test]
fn views_input_delete_line_on_the_last_line_leaves_an_empty_buffer() {
    let mut editor = typing("only");
    act(&mut editor, &["input_delete_line"]);
    assert!(editor.is_empty());
    assert_eq!(editor.height(), 1, "the buffer must always have one line");
}

#[test]
fn views_input_delete_word_backward_and_forward() {
    let mut editor = typing("alpha beta");
    act(&mut editor, &["input_delete_word_backward"]);
    assert_eq!(editor.text(), "alpha ");
    act(&mut editor, &["input_line_home"]);
    act(&mut editor, &["input_delete_word_forward"]);
    assert_eq!(editor.text(), " ");
}

#[test]
fn views_input_clear_empties_the_buffer() {
    let mut editor = typing("something");
    act(&mut editor, &["input_clear"]);
    assert!(editor.is_empty());
}

// ---------------------------------------------------------------------------
// Undo and redo
// ---------------------------------------------------------------------------

#[test]
fn views_input_undo_restores_the_previous_state_and_redo_reapplies_it() {
    let mut editor = typing("ab");
    act(&mut editor, &["input_undo"]);
    assert_eq!(editor.text(), "a");
    act(&mut editor, &["input_undo"]);
    assert_eq!(editor.text(), "");
    act(&mut editor, &["input_redo", "input_redo"]);
    assert_eq!(editor.text(), "ab");
}

#[test]
fn views_input_undo_on_an_empty_stack_reports_nothing_changed() {
    let mut editor = editor();
    assert_eq!(
        editor.handle_action(action("input_undo")),
        EditorSignal::None
    );
    assert_eq!(
        editor.handle_action(action("input_redo")),
        EditorSignal::None
    );
}

#[test]
fn views_input_a_new_edit_discards_the_redo_stack() {
    let mut editor = typing("ab");
    act(&mut editor, &["input_undo"]);
    editor.insert_char('z');
    assert_eq!(
        editor.handle_action(action("input_redo")),
        EditorSignal::None
    );
    assert_eq!(editor.text(), "az");
}

#[test]
fn views_input_undo_recovers_a_deleted_line() {
    // The failure `input_delete_line` exists to be recoverable from.
    let mut editor = editor();
    editor.insert_text("keep me\nand me");
    act(&mut editor, &["input_delete_line"]);
    act(&mut editor, &["input_undo"]);
    assert_eq!(editor.text(), "keep me\nand me");
}

// ---------------------------------------------------------------------------
// Submission and history
// ---------------------------------------------------------------------------

#[test]
fn views_input_submit_reports_the_text_and_clears_the_buffer() {
    let mut editor = typing("ship it");
    assert_eq!(
        editor.handle_action(action("input_submit")),
        EditorSignal::Submit(String::from("ship it"))
    );
    assert!(editor.is_empty());
    assert_eq!(editor.history(), [String::from("ship it")]);
}

#[test]
fn views_input_submit_refuses_a_blank_prompt() {
    let mut editor = typing("   \n  ");
    assert_eq!(
        editor.handle_action(action("input_submit")),
        EditorSignal::None,
        "a whitespace-only prompt was submitted"
    );
    assert!(editor.history().is_empty());
}

#[test]
fn views_input_history_walks_back_and_forward() {
    let mut editor = editor();
    for prompt in ["first", "second", "third"] {
        editor.set_text(prompt);
        editor.handle_action(action("input_submit"));
    }
    act(&mut editor, &["history_previous"]);
    assert_eq!(editor.text(), "third");
    act(&mut editor, &["history_previous"]);
    assert_eq!(editor.text(), "second");
    act(&mut editor, &["history_next"]);
    assert_eq!(editor.text(), "third");
}

#[test]
fn views_input_history_stashes_a_half_written_prompt() {
    let mut editor = editor();
    editor.set_text("submitted");
    editor.handle_action(action("input_submit"));
    editor.set_text("half written");
    act(&mut editor, &["history_previous"]);
    assert_eq!(editor.text(), "submitted");
    act(&mut editor, &["history_next"]);
    assert_eq!(
        editor.text(),
        "half written",
        "stepping out of history lost the prompt that was being typed"
    );
}

#[test]
fn views_input_history_clamps_at_the_oldest_entry() {
    let mut editor = editor();
    editor.set_text("only");
    editor.handle_action(action("input_submit"));
    act(&mut editor, &["history_previous", "history_previous"]);
    assert_eq!(editor.text(), "only");
}

#[test]
fn views_input_history_on_an_empty_history_does_nothing() {
    let mut editor = typing("draft");
    assert_eq!(
        editor.handle_action(action("history_previous")),
        EditorSignal::None
    );
    assert_eq!(editor.text(), "draft");
}

#[test]
fn views_input_history_skips_an_immediate_duplicate() {
    let mut editor = editor();
    for _ in 0..3 {
        editor.set_text("same");
        editor.handle_action(action("input_submit"));
    }
    assert_eq!(
        editor.history().len(),
        1,
        "three identical prompts produced three history entries"
    );
}

#[test]
fn views_input_history_is_capped() {
    let mut editor = editor();
    for index in 0..HISTORY_LIMIT + 10 {
        editor.set_text(&format!("prompt {index}"));
        editor.handle_action(action("input_submit"));
    }
    assert_eq!(editor.history().len(), HISTORY_LIMIT);
    assert_eq!(
        editor.history()[0],
        format!("prompt {}", 10),
        "the cap dropped the newest entries instead of the oldest"
    );
}

// ---------------------------------------------------------------------------
// The external surfaces
// ---------------------------------------------------------------------------

#[test]
fn views_input_editor_open_and_paste_are_reported_not_performed() {
    let mut editor = typing("draft");
    assert_eq!(
        editor.handle_action(action("editor_open")),
        EditorSignal::OpenExternalEditor,
        "the editor performed the spawn itself instead of asking its host"
    );
    assert_eq!(
        editor.handle_action(action("input_paste")),
        EditorSignal::Paste
    );
}

#[test]
fn views_input_copy_prefers_the_selection_and_falls_back_to_everything() {
    let mut editor = typing("abcdef");
    assert_eq!(
        editor.handle_action(action("messages_copy")),
        EditorSignal::Copy(String::from("abcdef"))
    );
    act(&mut editor, &["input_line_home"]);
    act(&mut editor, &["input_select_right", "input_select_right"]);
    assert_eq!(
        editor.handle_action(action("messages_copy")),
        EditorSignal::Copy(String::from("ab"))
    );
}

#[test]
fn views_input_unknown_action_is_ignored() {
    let mut editor = typing("x");
    assert_eq!(
        editor.handle_action(action("session_new")),
        EditorSignal::None
    );
    assert_eq!(editor.text(), "x");
}

// ---------------------------------------------------------------------------
// Paste normalisation
// ---------------------------------------------------------------------------

#[test]
fn views_input_normalize_prompt_content_matches_the_oracle_rule() {
    assert_eq!(normalize_prompt_content("one line\n"), "one line");
    assert_eq!(normalize_prompt_content("one line\r\n"), "one line");
    assert_eq!(
        normalize_prompt_content("two\nlines\n"),
        "two\nlines\n",
        "a multi-line paste lost its trailing newline"
    );
    assert_eq!(normalize_prompt_content("no newline"), "no newline");
    assert_eq!(normalize_prompt_content(""), "");
}

#[test]
fn views_prompt_gutter_renders_its_marker_from_the_palette() {
    let context = ViewContext::defaults();
    let mut gutter = PromptGutter::new(context.clone(), String::from(">"));
    let buffer = render_offscreen(&mut gutter, 2, 1).expect("infallible");
    assert_eq!(buffer[(0, 0)].symbol(), ">");
    assert_eq!(
        buffer[(0, 0)].fg,
        ratatui::style::Color::from(context.palette.border_active)
    );
}

#[test]
fn views_input_editor_ignores_application_events() {
    // The editor's input arrives as actions. Claiming an engine event here would
    // stop the transcript from seeing it.
    let mut editor = editor();
    let result = editor.handle_event(&crate::app::AppEvent::Engine(
        zuno_engine::r#loop::TurnEvent::TurnStarted {
            session_id: String::from("s"),
        },
    ));
    assert!(!result.handled);
}
