//! Autocomplete tests: the trigger rules, ranking, completion, and the off-screen
//! assertion.

use super::*;
use crate::app::render_offscreen;
use crate::views::testkit::{action, rows};

fn source() -> StaticSource {
    StaticSource::new()
        .command("session-new", "Start a new session")
        .command("session-list", "Switch sessions")
        .command("share", "Share this session")
        .agent("build", "The default agent")
        .agent("explore", "Read-only investigation")
        .file("src/main.rs")
        .file("src/views/message.rs")
        .file("README.md")
        .directory("src/views")
}

fn view() -> AutocompleteView {
    AutocompleteView::new(ViewContext::defaults(), Box::new(source()))
}

fn open(text: &str) -> AutocompleteView {
    let mut view = view();
    view.refresh(text, text.chars().count());
    view
}

// ---------------------------------------------------------------------------
// Trigger detection
// ---------------------------------------------------------------------------

#[test]
fn views_autocomplete_slash_opens_only_at_the_start_of_the_prompt() {
    let activation = detect("/ses", 4).expect("a slash at column zero triggers");
    assert_eq!(activation.trigger, Trigger::Command);
    assert_eq!(activation.query, "ses");
    assert_eq!(activation.start, 0);

    assert_eq!(
        detect("run /ses", 8),
        None,
        "a slash mid-prompt opened the command list"
    );
}

#[test]
fn views_autocomplete_slash_closes_once_the_query_has_whitespace() {
    // `autocomplete.tsx:684` — a slash command is one word.
    assert_eq!(detect("/share now", 10), None);
    let mut view = open("/share ");
    assert!(
        !view.is_open(),
        "the command list stayed open past the command word"
    );
    view.refresh("/share", 6);
    assert!(view.is_open());
}

#[test]
fn views_autocomplete_at_opens_on_the_nearest_at_with_no_whitespace_after_it() {
    let activation = detect("look at @src/ma", 15).expect("triggers");
    assert_eq!(activation.trigger, Trigger::Reference);
    assert_eq!(activation.query, "src/ma");
    assert_eq!(activation.start, 8);

    assert_eq!(
        detect("hi @ there", 10),
        None,
        "an `@` with a space after it opened the reference list"
    );
}

#[test]
fn views_autocomplete_at_must_start_a_token() {
    // An email address is not a file reference.
    assert_eq!(detect("mail user@host", 14), None);
    assert!(detect("mail @host", 10).is_some());
}

#[test]
fn views_autocomplete_uses_the_nearest_at_when_several_are_present() {
    let activation = detect("@one @two", 9).expect("triggers");
    assert_eq!(activation.start, 5);
    assert_eq!(activation.query, "two");
}

#[test]
fn views_autocomplete_detect_respects_the_cursor_not_the_whole_text() {
    // The cursor is before the second `@`, so the first one is the live trigger.
    let activation = detect("@one @two", 4).expect("triggers");
    assert_eq!(activation.start, 0);
    assert_eq!(activation.query, "one");
}

#[test]
fn views_autocomplete_detect_is_closed_for_plain_prose() {
    assert_eq!(detect("just some text", 14), None);
    assert_eq!(detect("", 0), None);
}

#[test]
fn views_autocomplete_detect_clamps_an_out_of_range_cursor() {
    // A resize or a programmatic set_text can leave the cursor past the end.
    assert!(detect("/x", 999).is_some());
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

#[test]
fn views_autocomplete_score_orders_prefix_above_boundary_above_substring() {
    let prefix = score("session-new", "sess").expect("prefix matches");
    let boundary = score("start-session", "sess").expect("boundary matches");
    let scattered = score("abcdef", "adf").expect("subsequence matches");
    assert!(prefix > boundary, "{prefix} !> {boundary}");
    assert!(boundary > scattered, "{boundary} !> {scattered}");
    assert_eq!(score("abc", "xyz"), None);
}

#[test]
fn views_autocomplete_score_matches_everything_for_an_empty_query() {
    assert_eq!(score("anything", ""), Some(1));
}

#[test]
fn views_autocomplete_score_is_case_insensitive() {
    assert_eq!(score("README.md", "readme"), score("readme.md", "README"));
}

#[test]
fn views_autocomplete_command_matching_requires_a_prefix_or_boundary() {
    // Upstream gives `/` an exact threshold; a scattered match must not surface.
    let view = open("/snn");
    assert!(
        view.matches().is_empty(),
        "a scattered match surfaced for a slash command: {:?}",
        view.matches()
    );
    let view = open("/sess");
    assert_eq!(view.matches().len(), 2);
}

#[test]
fn views_autocomplete_reference_matching_accepts_a_looser_match() {
    let view = open("@msg");
    assert!(
        view.matches()
            .iter()
            .any(|candidate| candidate.display.contains("message.rs")),
        "a scattered file match did not surface: {:?}",
        view.matches()
    );
}

#[test]
fn views_autocomplete_ranking_puts_the_prefix_match_first() {
    let view = open("@src");
    let first = &view.matches()[0].display;
    assert!(
        first.starts_with("src"),
        "the best match is not first: {:?}",
        view.matches()
    );
}

// ---------------------------------------------------------------------------
// Completion
// ---------------------------------------------------------------------------

#[test]
fn views_autocomplete_complete_replaces_the_trigger_and_the_partial_query() {
    let mut view = view();
    let text = "look at @src/ma";
    view.refresh(text, text.chars().count());
    let (completed, cursor) = view.complete(text).expect("a candidate is selected");
    assert_eq!(completed, "look at @src/main.rs ");
    assert_eq!(cursor, completed.chars().count());
}

#[test]
fn views_autocomplete_complete_keeps_the_text_after_the_cursor() {
    let mut view = view();
    // The cursor sits at the end of the reference, not at the end of the line.
    view.refresh("@src/ma and more", 7);
    let (completed, _) = view.complete("@src/ma and more").expect("a candidate");
    assert!(
        completed.ends_with("and more"),
        "text after the cursor was eaten: {completed:?}"
    );
}

#[test]
fn views_autocomplete_complete_is_idempotent() {
    let mut view = view();
    let (once, cursor) = {
        let text = "@src/ma";
        view.refresh(text, text.chars().count());
        view.complete(text).expect("a candidate")
    };
    view.refresh(&once, cursor);
    // The completed text ends in a space, so the trigger has closed: a second
    // completion has nothing to do rather than doubling the insertion.
    assert!(
        !view.is_open() || view.complete(&once).is_none_or(|(again, _)| again == once),
        "completing twice changed the text again"
    );
}

#[test]
fn views_autocomplete_a_directory_completion_re_triggers() {
    let mut view = view();
    let text = "@src/vie";
    view.refresh(text, text.chars().count());
    let directory = view
        .matches()
        .iter()
        .find(|candidate| candidate.kind == CandidateKind::Directory)
        .expect("the directory candidate");
    assert!(
        directory.insert.ends_with('/'),
        "a directory completion does not end in a separator, so it cannot be walked into"
    );
    assert!(
        !directory.insert.ends_with(' '),
        "a directory completion ended the reference with a space"
    );
}

#[test]
fn views_autocomplete_complete_reports_nothing_when_closed() {
    let view = view();
    assert_eq!(view.complete("plain text"), None);
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[test]
fn views_autocomplete_actions_move_hide_and_complete() {
    let mut view = open("@src");
    let first = view.cursor();
    assert_eq!(
        view.handle_action(action("prompt.autocomplete.next")),
        AutocompleteStep::Redraw
    );
    assert_ne!(view.cursor(), first);
    view.handle_action(action("prompt.autocomplete.prev"));
    assert_eq!(view.cursor(), first, "prev did not undo next");

    assert_eq!(
        view.handle_action(action("prompt.autocomplete.select")),
        AutocompleteStep::Complete
    );
    assert_eq!(
        view.handle_action(action("prompt.autocomplete.complete")),
        AutocompleteStep::Complete,
        "tab and enter must both complete"
    );

    assert_eq!(
        view.handle_action(action("prompt.autocomplete.hide")),
        AutocompleteStep::Redraw
    );
    assert!(!view.is_open());
}

#[test]
fn views_autocomplete_cursor_wraps_at_both_ends() {
    let mut view = open("@src");
    let count = view.matches().len();
    assert!(count > 1, "the fixture needs several matches");
    view.handle_action(action("prompt.autocomplete.prev"));
    assert_eq!(
        view.cursor(),
        count - 1,
        "moving up from the top did not wrap"
    );
    view.handle_action(action("prompt.autocomplete.next"));
    assert_eq!(view.cursor(), 0);
}

#[test]
fn views_autocomplete_ignores_actions_while_closed() {
    let mut view = view();
    assert_eq!(
        view.handle_action(action("prompt.autocomplete.next")),
        AutocompleteStep::Ignored
    );
}

#[test]
fn views_autocomplete_unrelated_action_is_ignored() {
    let mut view = open("@src");
    assert_eq!(
        view.handle_action(action("session_new")),
        AutocompleteStep::Ignored
    );
}

// ---------------------------------------------------------------------------
// The off-screen assertion
// ---------------------------------------------------------------------------

#[test]
fn views_autocomplete_renders_offscreen() {
    let mut view = open("/sess");
    let height = view.height();
    assert!(height > 0);
    let rendered = rows(&render_offscreen(&mut view, 44, height).expect("infallible"));
    let joined = rendered.join("\n");
    assert!(
        joined.contains("/session-new"),
        "a candidate is missing:\n{joined}"
    );
    assert!(
        joined.contains("Start a new session"),
        "a candidate description is missing:\n{joined}"
    );
    assert!(
        rendered[0].starts_with(" / "),
        "the command glyph is missing: {rendered:?}"
    );
}

#[test]
fn views_autocomplete_renders_nothing_when_closed() {
    let mut view = view();
    let buffer = render_offscreen(&mut view, 20, 3).expect("infallible");
    assert!(
        (0..3).all(|y| (0..20).all(|x| buffer[(x, y)].symbol() == " ")),
        "a closed popup painted cells"
    );
}

#[test]
fn views_autocomplete_highlights_the_cursor_from_the_palette() {
    let context = ViewContext::defaults();
    let mut view = AutocompleteView::new(context.clone(), Box::new(source()));
    view.refresh("@src", 4);
    let height = view.height();
    let buffer = render_offscreen(&mut view, 44, height).expect("infallible");
    assert_eq!(
        buffer[(0, 0)].bg,
        ratatui::style::Color::from(context.palette.primary),
        "the highlighted row does not carry the palette's selection background"
    );
}

#[test]
fn views_autocomplete_kind_glyphs_are_distinct() {
    let glyphs = [
        CandidateKind::Command,
        CandidateKind::File,
        CandidateKind::Directory,
        CandidateKind::Agent,
        CandidateKind::Reference,
    ]
    .map(CandidateKind::glyph);
    let unique = glyphs.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        glyphs.len(),
        "two candidate kinds share a glyph, so the list cannot be read"
    );
}

#[test]
fn views_autocomplete_agents_are_offered_under_the_at_trigger() {
    let view = open("@expl");
    assert!(
        view.matches()
            .iter()
            .any(|candidate| candidate.kind == CandidateKind::Agent
                && candidate.display == "@explore"),
        "the agent source is not reachable: {:?}",
        view.matches()
    );
}

// ---------------------------------------------------------------------------
// Which-key
// ---------------------------------------------------------------------------

#[test]
fn views_which_key_renders_its_entries_offscreen() {
    let mut view = WhichKeyView::new(ViewContext::defaults());
    view.entries = vec![
        (String::from("q"), String::from("Quit")),
        (String::from("n"), String::from("New session")),
    ];
    let rendered = rows(&render_offscreen(&mut view, 30, 2).expect("infallible"));
    assert!(rendered[0].contains('q') && rendered[0].contains("Quit"));
    assert!(rendered[1].contains("New session"));
}
