//! Cross-module guards: every view paints from the palette and names no key, plus
//! the composition assertions that only make sense above a single view.
//!
//! Each directory scan asserts a **floor** on the number of files it inspected. Todo
//! 2's `no_anyhow_in_libraries` earned that rule the hard way: with a stale
//! `CARGO_MANIFEST_DIR` it scanned zero files and, without a floor, would have passed
//! vacuously.

use super::*;
use crate::app::{AppEvent, Component, EventResult, render_offscreen};
use crate::config::{DiffStyle, ResolvedTuiConfig};
use crate::keybind::{ActionComponent, Definition, KeyDispatcher, Keymap};
use crate::theme::{Mode, ThemeRegistry};
use crate::views::dialog::{DialogHost, DialogOutcome, ObservedBase};
use crate::views::editor::{EditorSignal, InputEditor};
use crate::views::message::TranscriptView;
use crate::views::permission::PermissionPrompt;
use crate::views::testkit::{press, rows};
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent};
use ratatui::layout::Rect;
use std::path::{Path, PathBuf};

/// Files in this directory that are implementation rather than test.
///
/// Test modules are excluded from both scans because a test's whole job is to name a
/// literal colour and a literal key and assert the shipping code produced them.
fn source_files() -> Vec<PathBuf> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/views");
    let mut files = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_tests.rs"))
        })
        .collect::<Vec<_>>();
    files.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/views.rs"));
    files.sort();
    files
}

// ---------------------------------------------------------------------------
// Discipline: no view names a colour
// ---------------------------------------------------------------------------

#[test]
fn views_no_view_hardcodes_a_colour() {
    let files = source_files();
    assert!(
        files.len() >= 12,
        "the colour scan found only {} view sources; it is looking in the wrong place and \
         would pass by inspecting nothing",
        files.len()
    );

    // Every way a colour can be written without going through the palette. `Rgba`
    // is included because todo 75's constructor would bypass the palette just as
    // effectively as a ratatui variant.
    let forbidden = [
        "Color::Rgb",
        "Color::Red",
        "Color::Green",
        "Color::Blue",
        "Color::Yellow",
        "Color::Magenta",
        "Color::Cyan",
        "Color::White",
        "Color::Black",
        "Color::Gray",
        "Color::DarkGray",
        "Color::LightRed",
        "Color::LightGreen",
        "Color::LightBlue",
        "Color::LightYellow",
        "Color::LightMagenta",
        "Color::LightCyan",
        "Color::Indexed",
        "Rgba::opaque",
        "Rgba::from_hex",
    ];
    let mut scanned = 0;
    let mut offences = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        scanned += 1;
        for (number, line) in source.lines().enumerate() {
            // A doc comment may legitimately mention a colour name while explaining
            // why the palette exists.
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for needle in forbidden {
                if code.contains(needle) {
                    offences.push(format!(
                        "{}:{}: {needle} — paint from `ViewContext`'s palette instead",
                        path.display(),
                        number + 1
                    ));
                }
            }
        }
    }
    assert_eq!(scanned, files.len());
    assert!(
        offences.is_empty(),
        "a view named a colour instead of reading the resolved palette:\n{}",
        offences.join("\n")
    );
}

#[test]
fn views_every_view_source_reaches_the_palette_through_the_context() {
    // The complement of the scan above: proving nobody wrote a literal is only half
    // the property, because a view that paints nothing also has no literals.
    let files = source_files();
    let mut without = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("readable");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        // `scroll.rs` computes offsets and `external.rs` talks to other processes;
        // neither paints, so neither should have to pretend to.
        if matches!(name, "scroll.rs" | "external.rs") {
            continue;
        }
        if !source.contains("ViewContext") && !source.contains("context.") {
            without.push(path.display().to_string());
        }
    }
    assert!(
        without.is_empty(),
        "these view sources never reach the palette, so they render unthemed:\n{}",
        without.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Discipline: no view names a key
// ---------------------------------------------------------------------------

#[test]
fn views_no_view_matches_on_a_raw_key_spelling() {
    let files = source_files();
    assert!(
        files.len() >= 12,
        "the keybind scan found only {} view sources",
        files.len()
    );

    // A view is allowed to read a typed character — that is how a filter box works —
    // but it may not branch on a named key or a modifier chord, because those are
    // the things a user rebinds.
    let forbidden = [
        "KeyCode::Up",
        "KeyCode::Down",
        "KeyCode::Left",
        "KeyCode::Right",
        "KeyCode::Enter",
        "KeyCode::Tab",
        "KeyCode::Esc",
        "KeyCode::Backspace",
        "KeyCode::Delete",
        "KeyCode::Home",
        "KeyCode::End",
        "KeyCode::PageUp",
        "KeyCode::PageDown",
        "KeyModifiers::ALT",
        "KeyModifiers::SHIFT",
    ];
    let mut offences = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("readable");
        for (number, line) in source.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for needle in forbidden {
                if code.contains(needle) {
                    offences.push(format!(
                        "{}:{}: {needle} — act on a resolved `Definition` instead",
                        path.display(),
                        number + 1
                    ));
                }
            }
        }
    }
    assert!(
        offences.is_empty(),
        "a view branched on a raw key, so rebinding it would not work:\n{}",
        offences.join("\n")
    );
}

#[test]
fn views_every_action_a_view_names_exists_in_the_shipped_table() {
    // A typo in an action name is otherwise a silent dead branch: the view compiles,
    // the key resolves to a real action, and nothing happens.
    //
    // Only match arms *inside* an action handler are inspected. A tool name and an
    // action name are both snake_case strings in a match arm, so a whole-file scan
    // would report `"bash" =>` in the tool-icon table as an unknown action.
    let files = source_files();
    let mut checked = 0;
    let mut unknown = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("readable");
        let mut depth = 0i32;
        let mut inside = false;
        for (number, line) in source.lines().enumerate() {
            let code = line.trim_start();
            if !inside && code.contains("fn handle_action") {
                inside = true;
                depth = 0;
            }
            if inside {
                depth += i32::try_from(code.matches('{').count()).unwrap_or(0);
                depth -= i32::try_from(code.matches('}').count()).unwrap_or(0);
            }
            if inside && !code.starts_with("//") && code.contains("=>") {
                let head = code.split("=>").next().unwrap_or_default();
                for candidate in head.split('|') {
                    let Some(name) = candidate
                        .trim()
                        .strip_prefix('"')
                        .and_then(|rest| rest.strip_suffix('"'))
                    else {
                        continue;
                    };
                    if !name
                        .chars()
                        .all(|character| character.is_ascii_lowercase() || "_.".contains(character))
                    {
                        continue;
                    }
                    checked += 1;
                    if crate::keybind::definition(name).is_none() {
                        unknown.push(format!("{}:{}: `{name}`", path.display(), number + 1));
                    }
                }
            }
            if inside && depth <= 0 && code.contains('}') {
                inside = false;
            }
        }
    }
    assert!(
        checked >= 40,
        "the action-name scan checked only {checked} names, so it is not finding the \
         match arms it exists to check"
    );
    assert!(
        unknown.is_empty(),
        "these views act on action names the shipped binding table does not have:\n{}",
        unknown.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Composition: the pieces work together
// ---------------------------------------------------------------------------

/// A root that wires the transcript, the editor, and the dialog host together the
/// way a session screen does.
struct SessionRoot {
    transcript: TranscriptView,
    editor: InputEditor,
    signals: Vec<EditorSignal>,
}

impl Component for SessionRoot {
    fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let [transcript, editor] = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Min(1),
            ratatui::layout::Constraint::Length(2),
        ])
        .areas(area);
        self.transcript.render(frame, transcript);
        self.editor.render(frame, editor);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        self.transcript.handle_event(event)
    }
}

impl ActionComponent for SessionRoot {
    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> EventResult {
        let signal = self.editor.handle_action(action);
        let changed = signal != EditorSignal::None;
        if let EditorSignal::Submit(text) = &signal {
            self.transcript
                .transcript_mut()
                .push(crate::views::message::Message::user(text.clone()));
        }
        self.signals.push(signal);
        if changed {
            EventResult::REDRAW
        } else {
            EventResult::IGNORED
        }
    }
}

fn session_root() -> SessionRoot {
    let context = ViewContext::defaults();
    SessionRoot {
        transcript: TranscriptView::new(context.clone()),
        editor: InputEditor::new(context),
        signals: Vec::new(),
    }
}

#[test]
fn views_a_session_screen_renders_its_transcript_and_prompt_together() {
    let mut root = session_root();
    root.transcript
        .transcript_mut()
        .push(crate::views::message::Message::user("earlier prompt"));
    root.editor.set_text("what I am typing");
    let rendered = rows(&render_offscreen(&mut root, 40, 8).expect("infallible"));
    let joined = rendered.join("\n");
    assert!(
        joined.contains("earlier prompt"),
        "the transcript region is empty:\n{joined}"
    );
    assert!(
        rendered[6].contains("what I am typing"),
        "the prompt did not render in its own region: {rendered:?}"
    );
}

#[test]
fn views_a_key_travels_from_the_dispatcher_to_the_editor_as_an_action() {
    // The end-to-end proof of the "actions, not keys" discipline: the dispatcher
    // resolves a key against the shipped table and the editor acts on the result,
    // with no key spelling anywhere in between.
    let keymap = Keymap::defaults().expect("the table builds");
    let root = session_root();
    let mut dispatcher = KeyDispatcher::new(
        keymap,
        vec![String::from("input"), String::from("prompt")],
        Box::new(root),
    );
    let spelling = crate::keybind::definition("input_clear")
        .expect("in the table")
        .keys;
    let chord =
        crate::keybind::Chord::parse(spelling.split(',').next().expect("at least one spelling"))
            .expect("a valid spelling");
    let event = chord_to_event(&chord);
    let result = dispatcher.handle_event(&AppEvent::Terminal(crate::app::TerminalEvent::Input(
        CrosstermEvent::Key(event),
    )));
    assert!(
        result.handled,
        "the dispatcher did not resolve `input_clear`'s own default spelling"
    );
}

/// Turn a chord back into the key event that would produce it.
fn chord_to_event(chord: &crate::keybind::Chord) -> KeyEvent {
    let rendered = chord.to_string();
    let mut modifiers = crossterm::event::KeyModifiers::NONE;
    if rendered.contains("ctrl+") {
        modifiers |= crossterm::event::KeyModifiers::CONTROL;
    }
    if rendered.contains("alt+") {
        modifiers |= crossterm::event::KeyModifiers::ALT;
    }
    let last = rendered.rsplit('+').next().unwrap_or_default().to_owned();
    let code = match last.as_str() {
        "return" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        other => KeyCode::Char(other.chars().next().unwrap_or('?')),
    };
    KeyEvent {
        code,
        modifiers,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

#[test]
fn views_a_permission_prompt_over_a_live_session_resolves_without_disturbing_it() {
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(context.clone(), Box::new(ObservedBase::new(session_root())));
    host.handle_event(&AppEvent::Engine(
        zuno_engine::r#loop::TurnEvent::AssistantMessageCreated {
            step: 1,
            message_id: String::from("m"),
        },
    ));
    host.open(Box::new(PermissionPrompt::new(
        context,
        zuno_permission::PermissionRequest {
            id: String::from("r"),
            session_id: String::from("s"),
            permission: String::from("bash"),
            patterns: Vec::new(),
            metadata: serde_json::Map::new(),
            always: Vec::new(),
            tool: None,
        },
        &serde_json::json!({"command": "make"}),
    )));

    let before = rows(&render_offscreen(&mut host, 50, 14).expect("infallible")).join("\n");
    assert!(before.contains("Permission required"));

    host.handle_action(
        crate::views::testkit::action("dialog.select.submit"),
        &press(KeyCode::Enter),
    );
    let outcomes = host.drain_outcomes();
    assert!(matches!(
        outcomes.first(),
        Some(("permission", DialogOutcome::Permission(_)))
    ));

    let after = rows(&render_offscreen(&mut host, 50, 14).expect("infallible")).join("\n");
    assert!(
        !after.contains("Permission required"),
        "the resolved prompt is still drawn:\n{after}"
    );
    assert!(
        after.contains("Assistant"),
        "the session behind the prompt was lost:\n{after}"
    );
}

// ---------------------------------------------------------------------------
// The context itself
// ---------------------------------------------------------------------------

#[test]
fn views_context_styles_are_all_distinct_where_it_matters() {
    let context = ViewContext::defaults();
    let pairs: [(&str, ratatui::style::Style); 6] = [
        ("text", context.text()),
        ("muted", context.muted()),
        ("accent", context.accent()),
        ("warning", context.warning()),
        ("error", context.error()),
        ("success", context.success()),
    ];
    for (index, (left_name, left)) in pairs.iter().enumerate() {
        for (right_name, right) in pairs.iter().skip(index + 1) {
            assert_ne!(
                left.fg, right.fg,
                "`{left_name}` and `{right_name}` are the same colour, so the two states \
                 are indistinguishable"
            );
        }
    }
}

#[test]
fn views_context_selected_style_is_readable_against_its_own_background() {
    let context = ViewContext::defaults();
    let selected = context.selected();
    assert_ne!(
        selected.fg, selected.bg,
        "the selection paints its text in its own background colour"
    );
}

#[test]
fn views_context_defaults_use_the_built_in_default_theme() {
    let registry = ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, Mode::Dark);
    assert_eq!(ViewContext::defaults().palette, resolved.palette);
}

#[test]
fn views_context_carries_the_configuration_through() {
    let registry = ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, Mode::Light);
    let context = ViewContext::new(
        &resolved,
        ResolvedTuiConfig {
            diff_style: Some(DiffStyle::Stacked),
            scroll_speed: Some(2.5),
            ..ResolvedTuiConfig::default()
        },
    );
    assert_eq!(context.config.scroll_speed, Some(2.5));
    assert_eq!(context.diff_columns(300), DiffColumns::Unified);
}

#[test]
fn views_padded_fills_and_truncates_to_the_width() {
    let style = ViewContext::defaults().text();
    let short = padded("ab", 5, style);
    assert_eq!(short.spans[0].content.chars().count(), 5);
    let long = padded("abcdefgh", 3, style);
    assert_eq!(long.spans[0].content.as_ref(), "abc");
    assert_eq!(padded("", 0, style).spans[0].content.as_ref(), "");
}

#[test]
fn views_padded_counts_terminal_columns_not_bytes_and_not_characters() {
    // This assertion used to demand four *characters* for `日本`, which caught
    // byte-counting and let character-counting through. A CJK glyph occupies two cells, so
    // a row measured in characters overflows its frame by one column per wide glyph —
    // measured as a skill description running past the right edge and wrapping the frame.
    // Asserting columns catches both mistakes, and keeps the original intent.
    let style = ViewContext::defaults().text();
    let line = padded("日本", 4, style);
    assert_eq!(
        display_width(line.spans[0].content.as_ref()),
        4,
        "padding did not measure terminal columns"
    );
    assert_eq!(
        line.spans[0].content.as_ref(),
        "日本",
        "a row that already fills its width was padded further"
    );

    // And the truncating direction: three columns cannot hold two wide glyphs, and half a
    // cell is not something a terminal can draw.
    let narrow = padded("日本", 3, style);
    assert_eq!(display_width(narrow.spans[0].content.as_ref()), 3);
    assert_eq!(narrow.spans[0].content.as_ref(), "日 ");
}

#[test]
fn views_display_width_and_truncate_agree_about_wide_glyphs() {
    assert_eq!(display_width("abc"), 3);
    assert_eq!(display_width("日本語"), 6);
    assert_eq!(display_width(""), 0);
    assert_eq!(truncate("日本語", 4), "日本");
    assert_eq!(truncate("日本語", 5), "日本");
    assert_eq!(truncate("日本語", 6), "日本語");
    assert_eq!(truncate("abc", 2), "ab");
    assert_eq!(truncate("abc", 0), "");
    for width in 0..10 {
        assert!(
            display_width(&truncate("a日b本c", width)) <= width,
            "truncating to {width} produced a wider string"
        );
    }
}

#[test]
fn views_every_padded_surface_stays_inside_its_frame_with_wide_glyphs() {
    // The property the two helpers exist for, asserted through the surfaces that render
    // user-supplied names: a project whose skills and agents are named in Chinese must not
    // push the frame apart.
    let context = ViewContext::defaults();
    let long = "飞书多维表格操作：建表、字段、记录、视图、统计、公式".repeat(4);
    for width in [40_u16, 80, 200] {
        for text in [long.as_str(), "ascii only", "", "日"] {
            let line = padded(text, width, context.text());
            let used: usize = line
                .spans
                .iter()
                .map(|span| display_width(&span.content))
                .sum();
            assert_eq!(
                used,
                usize::from(width),
                "padded({text:?}, {width}) measured {used} columns"
            );
        }
    }
}

#[test]
fn views_fill_paints_every_cell_in_the_area() {
    struct Filler(ViewContext);
    impl Component for Filler {
        fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
            fill(frame.buffer_mut(), area, self.0.element());
        }
        fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
            EventResult::IGNORED
        }
    }
    let context = ViewContext::defaults();
    let mut filler = Filler(context.clone());
    let buffer = render_offscreen(&mut filler, 4, 3).expect("infallible");
    let expected = ratatui::style::Color::from(context.palette.background_element);
    for y in 0..3 {
        for x in 0..4 {
            assert_eq!(
                buffer[(x, y)].bg,
                expected,
                "cell ({x},{y}) was left unpainted"
            );
        }
    }
    assert!(!is_reset(expected));
    assert!(is_reset(ratatui::style::Color::Reset));
}

#[test]
fn views_hint_renders_a_key_and_its_label() {
    let context = ViewContext::defaults();
    let spans = hint("enter", "confirm", &context);
    let text = spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert_eq!(text, "enter confirm  ");
    assert_ne!(
        spans[0].style.fg, spans[2].style.fg,
        "the key and its label are the same colour, so the hint has no structure"
    );
}
