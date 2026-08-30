//! Input editor tests: motion, deletion, selection, undo, history, and the
//! off-screen assertion.

use super::*;
use crate::app::{AppEvent, TerminalEvent, render_offscreen};
use crate::views::testkit::{action, rows};
use crossterm::event::{
    Event as CrosstermEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

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

fn render_at(editor: &mut InputEditor, width: u16, height: u16, area: Rect) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| editor.render(frame, area))
        .expect("infallible");
    terminal.backend().buffer().clone()
}

const fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
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
        "the buffer text or caret glyph is missing: {rendered:?}"
    );
    assert!(
        buffer[(5, 0)].modifier.contains(Modifier::REVERSED),
        "the end-of-line caret is not a visible inverse cell"
    );
}

#[test]
fn views_input_editor_renders_multiple_lines_offscreen() {
    let mut editor = editor();
    editor.insert_text("first\nsecond\nthird");
    assert_eq!(editor.height(), 3);
    let buffer = render_offscreen(&mut editor, 12, 4).expect("infallible");
    let rendered = rows(&buffer);
    assert_eq!(&rendered[..3], ["first", "second", "third▏"]);
    assert!(
        buffer[(5, 2)].modifier.contains(Modifier::REVERSED),
        "the caret is not visible on the active line"
    );
    assert!(
        !buffer[(5, 0)].modifier.contains(Modifier::REVERSED),
        "an inactive line was painted as if it owned the caret"
    );
}

#[test]
fn views_input_editor_scrolls_to_keep_the_cursor_visible() {
    let mut editor = editor();
    editor.insert_text("a\nb\nc\nd\ne");
    // The area holds two rows and the cursor is on the fifth line, so the first
    // rendered row has to be the fourth.
    let buffer = render_offscreen(&mut editor, 8, 2).expect("infallible");
    let rendered = rows(&buffer);
    assert_eq!(rendered, vec![String::from("d"), String::from("e▏")]);
    assert!(buffer[(1, 1)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn views_input_editor_paints_from_the_palette() {
    let context = ViewContext::defaults();
    let mut editor = InputEditor::new(context.clone());
    editor.insert_char('x');
    let buffer = render_offscreen(&mut editor, 6, 1).expect("infallible");
    assert_eq!(
        buffer[(0, 0)].fg,
        ratatui::style::Color::from(context.palette().text)
    );
    assert_eq!(
        buffer[(1, 0)].fg,
        ratatui::style::Color::from(context.palette().text),
        "the caret did not derive its foreground from the theme text role"
    );
    assert_eq!(
        buffer[(1, 0)].bg,
        ratatui::style::Color::from(context.palette().background_element),
        "the caret did not remain seated on the editor's theme surface"
    );
    assert!(
        buffer[(1, 0)].modifier.contains(Modifier::REVERSED),
        "the theme-derived caret is not visibly distinct from adjacent text"
    );
}

#[test]
fn views_input_editor_shows_a_theme_derived_caret_in_an_empty_prompt() {
    let context = ViewContext::defaults();
    let mut editor = InputEditor::new(context.clone());
    let buffer = render_offscreen(&mut editor, 6, 1).expect("infallible");

    assert_eq!(buffer[(0, 0)].symbol(), "▏");
    assert_eq!(
        buffer[(0, 0)].fg,
        ratatui::style::Color::from(context.palette().text)
    );
    assert_eq!(
        buffer[(0, 0)].bg,
        ratatui::style::Color::from(context.palette().background_element)
    );
    assert!(
        buffer[(0, 0)].modifier.contains(Modifier::REVERSED),
        "an empty prompt has no visible caret"
    );
}

#[test]
fn views_input_editor_shows_the_caret_at_the_requested_text_position() {
    let mut editor = typing("abcd");
    act(&mut editor, &["input_move_left", "input_move_left"]);
    let buffer = render_offscreen(&mut editor, 8, 1).expect("infallible");

    assert_eq!(rows(&buffer)[0], "ab▏cd");
    assert_eq!(buffer[(2, 0)].symbol(), "▏");
    assert!(buffer[(2, 0)].modifier.contains(Modifier::REVERSED));
    assert!(!buffer[(1, 0)].modifier.contains(Modifier::REVERSED));
    assert!(!buffer[(3, 0)].modifier.contains(Modifier::REVERSED));
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
        ratatui::style::Color::from(context.palette().primary),
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

#[test]
fn views_input_completion_preserves_its_absolute_cursor() {
    let mut editor = typing("old");
    editor.apply_completion("/models and more", 8);
    assert_eq!(editor.text(), "/models and more");
    assert_eq!(editor.cursor(), Position { line: 0, column: 8 });

    editor.apply_completion("first\n/review tail", 14);
    assert_eq!(editor.cursor(), Position { line: 1, column: 8 });
}

#[test]
fn views_input_completion_clamps_a_cursor_past_the_buffer_end() {
    let mut editor = editor();
    editor.apply_completion("short", usize::MAX);
    assert_eq!(editor.cursor(), Position { line: 0, column: 5 });
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
fn views_input_mouse_drag_selects_from_the_last_rendered_editor_area() {
    let mut editor = typing("abcdef");
    let area = Rect::new(4, 2, 10, 2);
    let _buffer = render_at(&mut editor, 20, 8, area);

    assert_eq!(
        editor.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 5, 2)),
        EditorSignal::Changed
    );
    assert_eq!(
        editor.handle_mouse(&mouse(MouseEventKind::Drag(MouseButton::Left), 8, 2)),
        EditorSignal::Changed
    );
    assert_eq!(
        editor.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), 8, 2)),
        EditorSignal::Changed
    );

    assert_eq!(editor.anchor(), Some(Position { line: 0, column: 1 }));
    assert_eq!(editor.cursor(), Position { line: 0, column: 4 });
    assert_eq!(editor.selection(), Some(String::from("bcd")));
    assert_eq!(
        editor.handle_action(action("messages_copy")),
        EditorSignal::Copy(String::from("bcd")),
        "the pointer selection did not reuse the existing clipboard signal"
    );
}

#[test]
fn views_input_mouse_selection_maps_visible_rows_through_the_scroll_offset() {
    let mut editor = editor();
    editor.insert_text("zero\none\ntwo\nthree");
    let area = Rect::new(3, 4, 8, 2);
    let _buffer = render_at(&mut editor, 16, 8, area);

    editor.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 4, 4));
    editor.handle_mouse(&mouse(MouseEventKind::Drag(MouseButton::Left), 6, 5));
    editor.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), 6, 5));

    assert_eq!(editor.anchor(), Some(Position { line: 2, column: 1 }));
    assert_eq!(editor.cursor(), Position { line: 3, column: 3 });
    assert_eq!(editor.selection(), Some(String::from("wo\nthr")));
}

#[test]
fn views_input_mouse_drag_clamps_outside_the_rendered_area_after_capture() {
    let mut editor = editor();
    editor.insert_text("abcd\nef");
    act(&mut editor, &["input_buffer_home"]);
    let area = Rect::new(2, 1, 6, 2);
    let _buffer = render_at(&mut editor, 12, 6, area);

    editor.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 5, 1));
    editor.handle_mouse(&mouse(MouseEventKind::Drag(MouseButton::Left), 40, 20));
    editor.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), 40, 20));

    assert_eq!(editor.anchor(), Some(Position { line: 0, column: 2 }));
    assert_eq!(editor.cursor(), Position { line: 1, column: 2 });
    assert_eq!(editor.selection(), Some(String::from("cd\nef")));
}

#[test]
fn views_input_mouse_click_moves_the_caret_without_leaving_an_empty_selection() {
    let mut editor = typing("abcdef");
    let area = Rect::new(2, 1, 8, 1);
    let _buffer = render_at(&mut editor, 12, 4, area);

    editor.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 5, 1));
    editor.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), 5, 1));

    assert_eq!(editor.cursor(), Position { line: 0, column: 3 });
    assert_eq!(editor.anchor(), None);
    assert_eq!(editor.selection(), None);
}

#[test]
fn views_input_mouse_selection_requires_a_rendered_area_and_a_left_press() {
    let mut editor = typing("abcdef");
    assert_eq!(
        editor.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0)),
        EditorSignal::None,
        "a pointer coordinate was interpreted without a rendered editor area"
    );

    let _buffer = render_at(&mut editor, 12, 4, Rect::new(2, 1, 8, 1));
    assert_eq!(
        editor.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Right), 4, 1)),
        EditorSignal::None
    );
    assert_eq!(editor.cursor(), Position { line: 0, column: 6 });
}

#[test]
fn views_input_mouse_coordinates_skip_the_rendered_caret_cell() {
    let mut editor = typing("abcd");
    act(&mut editor, &["input_move_left", "input_move_left"]);
    let _buffer = render_offscreen(&mut editor, 8, 1).expect("infallible");

    editor.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 4, 0));
    editor.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), 4, 0));

    assert_eq!(
        editor.cursor(),
        Position { line: 0, column: 3 },
        "the caret's own display cell shifted every text coordinate after it"
    );
}

#[test]
fn views_input_mouse_coordinates_follow_terminal_columns_for_wide_glyphs() {
    let mut editor = typing("你ab");
    let _buffer = render_offscreen(&mut editor, 8, 1).expect("infallible");

    editor.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
    editor.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), 2, 0));

    assert_eq!(
        editor.cursor(),
        Position { line: 0, column: 1 },
        "the first narrow glyph after a two-cell glyph was mapped as a character column"
    );
}

#[test]
fn views_input_editor_component_handles_captured_mouse_selection_events() {
    let mut editor = typing("abcdef");
    let _buffer = render_offscreen(&mut editor, 8, 1).expect("infallible");
    let event = |kind| {
        AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Mouse(mouse(
            kind, 1, 0,
        ))))
    };

    assert!(
        editor
            .handle_event(&event(MouseEventKind::Down(MouseButton::Left)))
            .handled
    );
    let drag = AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        4,
        0,
    ))));
    assert!(editor.handle_event(&drag).handled);
    let release = AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        4,
        0,
    ))));
    assert!(editor.handle_event(&release).handled);
    assert_eq!(editor.selection(), Some(String::from("bcd")));
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
fn views_input_history_actions_move_within_a_multi_line_buffer_before_walking_history() {
    let mut editor = editor();
    editor.load_history(vec![String::from("remembered")]);
    editor.set_text("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight");

    assert_eq!(editor.cursor(), Position { line: 7, column: 5 });
    act(&mut editor, &["history_previous"]);
    assert_eq!(
        editor.cursor(),
        Position { line: 6, column: 5 },
        "Up from the last line walked history instead of keeping the pasted block editable"
    );
    assert_eq!(
        editor.text(),
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight"
    );

    act(&mut editor, &["input_buffer_home", "history_next"]);
    assert_eq!(
        editor.cursor(),
        Position { line: 1, column: 0 },
        "Down from the first line walked history instead of moving into the buffer"
    );
}

#[test]
fn views_input_history_actions_walk_only_beyond_the_vertical_edges() {
    let mut editor = editor();
    editor.load_history(vec![String::from("older"), String::from("newest")]);
    editor.set_text("draft one\ndraft two\ndraft three");
    act(&mut editor, &["input_buffer_home", "history_previous"]);
    assert_eq!(
        editor.text(),
        "newest",
        "Up on the first line did not recall the newest history entry"
    );

    act(&mut editor, &["history_next"]);
    assert_eq!(
        editor.text(),
        "draft one\ndraft two\ndraft three",
        "Down past the newest history entry did not restore the in-progress draft"
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
// Persisted history
// ---------------------------------------------------------------------------

/// A history file holding `entries`, written the way the host writes it.
fn history_file(directory: &std::path::Path, entries: &[&str]) -> std::path::PathBuf {
    let path = directory.join(PROMPT_HISTORY_FILE);
    let mut text = String::new();
    for entry in entries {
        text.push_str(&PromptHistory::encode(entry).expect("a short entry encodes"));
    }
    std::fs::write(&path, text).expect("write the history file");
    path
}

#[test]
fn views_input_history_round_trips_through_a_real_file() {
    // A real file, not an in-memory fixture: the whole failure this feature exists to
    // fix is that nothing was written down, and a fake writer would pass whether or not
    // `encode` and `load` agree about the format.
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join(PROMPT_HISTORY_FILE);

    let mut writing = editor();
    let (sender, mut records) = tokio::sync::mpsc::channel(8);
    writing.record_history_to(sender);
    for prompt in ["first", "second"] {
        writing.set_text(prompt);
        writing.handle_action(action("input_submit"));
    }
    let mut text = String::new();
    while let Ok(entry) = records.try_recv() {
        text.push_str(&PromptHistory::encode(&entry).expect("a short entry encodes"));
    }
    std::fs::write(&path, text).expect("write the history file");

    let loaded = PromptHistory::load(&path);
    assert_eq!(loaded.notice(), None, "a clean file produced a diagnostic");
    let mut restarted = editor();
    restarted.load_history(loaded.into_entries());
    act(&mut restarted, &["history_previous"]);
    assert_eq!(
        restarted.text(),
        "second",
        "a restarted editor could not recall the newest persisted prompt"
    );
    act(&mut restarted, &["history_previous"]);
    assert_eq!(restarted.text(), "first");
}

#[test]
fn views_input_history_round_trips_a_multi_line_prompt() {
    // The reason the format is JSON per line rather than one prompt per line: a
    // multi-line prompt written raw would come back as several entries, and a prompt
    // containing a `}` or a quote would come back corrupt.
    let directory = tempfile::tempdir().expect("temp dir");
    let prompt = "fix this:\n```sh\nset -e \"quoted\"\n```\n{\"json\": true}";
    let path = history_file(directory.path(), &[prompt]);

    let loaded = PromptHistory::load(&path);
    assert_eq!(loaded.entries(), [String::from(prompt)]);
    assert_eq!(
        std::fs::read_to_string(&path)
            .expect("readable")
            .lines()
            .count(),
        1,
        "a multi-line prompt was written as more than one line, so it cannot round-trip"
    );
}

#[test]
fn views_input_history_from_a_missing_file_is_empty_and_silent() {
    // Every first run takes this path, so a notice here would greet every new user
    // with a warning about a file they were never supposed to have.
    let directory = tempfile::tempdir().expect("temp dir");
    let loaded = PromptHistory::load(&directory.path().join(PROMPT_HISTORY_FILE));
    assert!(loaded.entries().is_empty());
    assert_eq!(loaded.notice(), None);
}

#[test]
fn views_input_history_from_a_corrupt_file_is_empty_and_says_so() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join(PROMPT_HISTORY_FILE);
    std::fs::write(&path, "not json\n{\"input\": \n\u{0}\u{1}garbage\n").expect("write");

    let loaded = PromptHistory::load(&path);
    assert!(
        loaded.entries().is_empty(),
        "unparseable lines were loaded as prompts: {:?}",
        loaded.entries()
    );
    let notice = loaded.notice().expect("a corrupt file must be reported");
    assert!(
        notice.contains("skipped 3"),
        "the notice does not say how much was lost: {notice}"
    );
}

#[test]
fn views_input_history_keeps_the_lines_before_a_truncated_one() {
    // The reason for JSONL over one JSON document: a process killed mid-append
    // truncates its final line only, and every earlier prompt still loads. A single
    // document would be unparseable in its entirety.
    let directory = tempfile::tempdir().expect("temp dir");
    let path = history_file(directory.path(), &["kept", "also kept"]);
    let mut text = std::fs::read_to_string(&path).expect("readable");
    text.push_str("{\"input\": \"cut off in the mi");
    std::fs::write(&path, text).expect("write");

    let loaded = PromptHistory::load(&path);
    assert_eq!(
        loaded.entries(),
        [String::from("kept"), String::from("also kept")]
    );
    assert!(
        loaded.notice().is_some(),
        "the truncated line went unreported"
    );
}

#[test]
fn views_input_history_is_capped_on_load_as_well_as_on_record() {
    // The record-time cap alone does not bound this: a file grown by an older build,
    // or edited by hand, arrives already over the limit.
    let directory = tempfile::tempdir().expect("temp dir");
    let entries: Vec<String> = (0..HISTORY_LIMIT + 10)
        .map(|index| format!("prompt {index}"))
        .collect();
    let borrowed: Vec<&str> = entries.iter().map(String::as_str).collect();
    let path = history_file(directory.path(), &borrowed);

    let loaded = PromptHistory::load(&path);
    assert_eq!(loaded.entries().len(), HISTORY_LIMIT);
    assert_eq!(
        loaded.entries()[0],
        "prompt 10",
        "the load cap dropped the newest entries instead of the oldest"
    );

    let mut editor = editor();
    editor.load_history(entries);
    assert_eq!(
        editor.history().len(),
        HISTORY_LIMIT,
        "an over-long list installed into the editor was not capped"
    );
}

#[test]
fn views_input_history_does_not_persist_an_over_long_prompt() {
    // Bounded because a paste is one keystroke now: without this, the startup read the
    // whole feature depends on becomes the slowest thing the TUI does.
    assert!(PromptHistory::encode("short").is_some());
    let huge = "x".repeat(HISTORY_ENTRY_LIMIT + 1);
    assert!(
        PromptHistory::encode(&huge).is_none(),
        "an entry over the limit was encoded for the file anyway"
    );
}

#[test]
fn views_input_history_records_only_what_it_remembered() {
    // The file and the in-memory list have to agree, so a de-duplicated repeat must not
    // reach the sink either — otherwise the next run's history differs from the one the
    // user was walking a moment ago.
    let mut editor = editor();
    let (sender, mut records) = tokio::sync::mpsc::channel(8);
    editor.record_history_to(sender);
    for _ in 0..3 {
        editor.set_text("same");
        editor.handle_action(action("input_submit"));
    }
    assert_eq!(records.try_recv(), Ok(String::from("same")));
    assert!(
        records.try_recv().is_err(),
        "a repeat the editor refused to remember was still written down"
    );
}

// ---------------------------------------------------------------------------
// Paste
// ---------------------------------------------------------------------------

#[test]
fn views_input_paste_inserts_every_line_and_submits_nothing() {
    let mut editor = editor();
    let pasted = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight";
    assert_eq!(editor.insert_paste(pasted), EditorSignal::Changed);
    assert_eq!(
        editor.height(),
        8,
        "an eight-line paste did not become eight lines: {:?}",
        editor.text()
    );
    assert_eq!(editor.text(), pasted);
    assert_eq!(editor.submission_text(), pasted);
}

#[test]
fn views_input_paste_below_the_threshold_is_shown_in_full() {
    let mut editor = editor();
    editor.insert_paste("a\nb\nc");
    assert_eq!(editor.text(), "a\nb\nc", "a short paste was summarised");
}

#[test]
fn views_input_a_large_paste_is_summarised_but_sent_in_full() {
    let mut editor = editor();
    let lines: Vec<String> = (0..PASTE_SUMMARY_LINES + 5)
        .map(|index| format!("line {index}"))
        .collect();
    let pasted = lines.join("\n");
    editor.insert_paste(&pasted);

    assert_eq!(
        editor.height(),
        1,
        "a summarised paste still occupied the prompt: {:?}",
        editor.text()
    );
    assert!(
        editor.text().contains(&format!("~{} lines", lines.len())),
        "the summary does not state the line count: {:?}",
        editor.text()
    );
    assert_eq!(
        editor.submission_text(),
        pasted,
        "the placeholder, not the paste, would have been sent"
    );
    assert_eq!(
        editor.handle_action(action("input_submit")),
        EditorSignal::Submit(pasted.clone()),
        "submitting sent the summary instead of the text"
    );
    assert_eq!(
        editor.history(),
        [pasted],
        "history recorded the summary rather than what was sent"
    );
}

#[test]
fn views_input_a_very_long_single_line_paste_is_summarised_too() {
    // The character limit exists for this shape: one line, far wider than the prompt,
    // which the line count alone would let through.
    let mut editor = editor();
    let pasted = "x".repeat(PASTE_SUMMARY_CHARS + 1);
    editor.insert_paste(&pasted);
    assert!(
        editor.text().starts_with("[Pasted #1"),
        "a very long single line was inserted whole: {} chars",
        editor.text().chars().count()
    );
    assert_eq!(editor.submission_text(), pasted);
}

#[test]
fn views_input_two_large_pastes_expand_to_their_own_text() {
    // The placeholder's ordinal is what makes this work. Without it both pastes would
    // carry the same placeholder and one would expand with the other's text — sending
    // the model content the user never pasted there.
    let mut editor = editor();
    let first = (0..PASTE_SUMMARY_LINES + 1)
        .map(|_| String::from("first"))
        .collect::<Vec<_>>()
        .join("\n");
    let second = (0..PASTE_SUMMARY_LINES + 1)
        .map(|_| String::from("second"))
        .collect::<Vec<_>>()
        .join("\n");
    editor.insert_paste(&first);
    editor.insert_char(' ');
    editor.insert_paste(&second);

    let submitted = editor.submission_text();
    assert!(
        submitted.starts_with(&first) && submitted.ends_with(&second),
        "the two pastes did not expand to their own text: {submitted:?}"
    );
}

#[test]
fn views_input_a_deleted_placeholder_sends_nothing_of_its_paste() {
    let mut editor = editor();
    let pasted = (0..PASTE_SUMMARY_LINES + 1)
        .map(|_| String::from("body"))
        .collect::<Vec<_>>()
        .join("\n");
    editor.insert_paste(&pasted);
    act(&mut editor, &["input_clear"]);
    editor.insert_text("never mind");
    assert_eq!(
        editor.submission_text(),
        "never mind",
        "a placeholder the user deleted still expanded"
    );
}

#[test]
fn views_input_a_paste_starting_with_a_slash_is_escaped_as_literal_text() {
    // A pasted path is not a command. Unescaped, `/etc/hosts` resolves to `unknown
    // command /etc` and the paste is discarded — the buffer has already been cleared by
    // then. `//` is the slash router's own literal escape; see `views/slash.rs`.
    let mut editor = editor();
    editor.insert_paste("/etc/hosts");
    assert_eq!(editor.text(), "//etc/hosts");
    assert_eq!(editor.submission_text(), "//etc/hosts");
}

#[test]
fn views_input_a_slash_pasted_mid_prompt_is_left_alone() {
    // Only a leading slash can be read as a command, so escaping anywhere else would
    // corrupt the text for no reason.
    let mut editor = editor();
    editor.insert_text("look at ");
    editor.insert_paste("/etc/hosts");
    assert_eq!(editor.text(), "look at /etc/hosts");
}

#[test]
fn views_input_an_empty_paste_changes_nothing() {
    let mut editor = typing("draft");
    assert_eq!(editor.insert_paste(""), EditorSignal::None);
    assert_eq!(editor.insert_paste("\n"), EditorSignal::None);
    assert_eq!(editor.text(), "draft");
}

#[test]
fn views_input_setting_the_text_drops_a_held_paste() {
    // `$EDITOR` returns through `set_text`, so a retained placeholder would either
    // expand a string the editor happened to contain or be submitted literally.
    let mut editor = editor();
    let pasted = (0..PASTE_SUMMARY_LINES + 1)
        .map(|_| String::from("body"))
        .collect::<Vec<_>>()
        .join("\n");
    editor.insert_paste(&pasted);
    editor.set_text("edited elsewhere");
    assert_eq!(editor.submission_text(), "edited elsewhere");
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
        ratatui::style::Color::from(context.palette().border_active)
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
