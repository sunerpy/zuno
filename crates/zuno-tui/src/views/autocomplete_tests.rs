//! Autocomplete tests: the trigger rules, ranking, completion, and the off-screen
//! assertion.

use super::*;
use crate::app::render_offscreen;
use crate::views::slash::{CatalogCommand, SlashRouter};
use crate::views::testkit::{action, rows};
use std::time::{Duration, Instant};

fn source() -> StaticSource {
    StaticSource::new()
        .command("new", "Start a new session")
        .command("session", "Switch sessions")
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
    assert_eq!(detect("/session now", 12), None);
    let mut view = open("/session ");
    assert!(
        !view.is_open(),
        "the command list stayed open past the command word"
    );
    view.refresh("/session", 8);
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
    assert!(
        view.matches()
            .iter()
            .any(|candidate| candidate.display == "/session"),
        "the canonical session command did not surface: {:?}",
        view.matches()
    );
}

#[test]
fn views_autocomplete_production_slash_source_projects_ui_and_catalog_commands() {
    let router = SlashRouter::new([CatalogCommand::new(
        "review",
        Some("Review the current changes".to_owned()),
    )]);
    let mut view =
        AutocompleteView::new(ViewContext::defaults(), Box::new(SlashSource::new(router)));

    view.refresh("/mo", 3);
    assert!(
        view.matches()
            .iter()
            .any(|candidate| candidate.display == "/model")
    );
    view.refresh("/review", 7);
    assert!(
        view.matches()
            .iter()
            .any(|candidate| candidate.display == "/review")
    );
}

#[test]
fn views_autocomplete_production_slash_source_matches_aliases_and_descriptions() {
    let router = SlashRouter::new([CatalogCommand::new(
        "audit",
        Some("Inspect release safety".to_owned()),
    )]);
    let mut view =
        AutocompleteView::new(ViewContext::defaults(), Box::new(SlashSource::new(router)));

    view.refresh("/continue", 9);
    assert_eq!(view.matches()[0].display, "/session");
    view.refresh("/inspect", 8);
    assert_eq!(view.matches()[0].display, "/audit");
}

#[test]
fn views_autocomplete_slash_results_are_capped_at_ten() {
    let router =
        SlashRouter::new((0..15).map(|index| CatalogCommand::new(format!("task-{index}"), None)));
    let mut view =
        AutocompleteView::new(ViewContext::defaults(), Box::new(SlashSource::new(router)));
    view.refresh("/task", 5);
    assert_eq!(view.matches().len(), 10);
    assert_eq!(view.height(), 10);
}

#[test]
fn views_autocomplete_never_offers_unsupported_command_families() {
    let forbidden = [
        "share",
        "unshare",
        "console-org",
        "org",
        "connect",
        "github-app",
        "workspace-list",
        "warp",
        "move-session",
        "stash",
    ];
    let router = SlashRouter::new(
        forbidden
            .iter()
            .map(|name| CatalogCommand::new(*name, Some(format!("forbidden {name}")))),
    );
    let mut view =
        AutocompleteView::new(ViewContext::defaults(), Box::new(SlashSource::new(router)));
    for name in forbidden {
        let input = format!("/{name}");
        view.refresh(&input, input.chars().count());
        let offered = view
            .matches()
            .iter()
            .map(|candidate| candidate.display.as_str())
            .collect::<Vec<_>>();
        assert!(
            !offered.contains(&input.as_str()),
            "`/{name}` leaked into autocomplete: {offered:?}"
        );
    }
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
    assert!(joined.contains("/new"), "a candidate is missing:\n{joined}");
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
        ratatui::style::Color::from(context.palette().primary),
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

/// A prefix carrying `count` continuations, each with `description`.
///
/// Built from real table rows so a `Continuation` here is the same shape the dispatcher
/// produces; inventing a `&'static Definition` is not possible from a test anyway.
fn prefix(count: usize) -> PendingPrefix {
    let keymap = crate::keybind::Keymap::defaults().expect("the shipped table builds");
    let leader = keymap.leader();
    let mut keymap = keymap;
    assert_eq!(
        keymap.resolve(&["session", "app"], leader, Instant::now()),
        crate::keybind::Resolution::Pending,
        "the leader must leave the engine pending or there is nothing to show"
    );
    let mut prefix = PendingPrefix {
        chords: keymap.pending().to_vec(),
        continuations: keymap.continuations(&["session", "app"]),
    };
    prefix.continuations.truncate(count);
    prefix
}

fn which_key(count: usize) -> WhichKeyView {
    which_key_at(count, Instant::now())
}

/// The same, with the instant recorded so an expiry assertion can measure from it.
fn which_key_at(count: usize, now: Instant) -> WhichKeyView {
    let mut view = WhichKeyView::new(ViewContext::defaults());
    view.observe(&prefix(count), now);
    view
}

#[test]
fn views_which_key_renders_a_key_beside_its_description() {
    // Asserted on the shortest description the leader reaches, because a grid cell
    // legitimately truncates a long one — demanding the longest appear in full would be
    // asserting that the layout does not do its job.
    let mut view = which_key(usize::MAX);
    let entry = shortest();
    let rendered = rows(&render_offscreen(&mut view, 100, 8).expect("infallible"));
    let joined = rendered.join("\n");
    assert!(
        joined.contains(&format!("{} {}", entry.keys, entry.definition.description)),
        "the panel did not show `{}` beside `{}`:\n{joined}",
        entry.keys,
        entry.definition.description
    );
}

/// The continuation whose description is shortest, so it survives any cell width.
fn shortest() -> crate::keybind::Continuation {
    let mut all = prefix(usize::MAX).continuations;
    all.sort_by_key(|entry| display_width(entry.definition.description));
    all.into_iter()
        .next()
        .expect("the leader reaches something")
}

#[test]
fn views_which_key_shows_distinct_actions_before_interchangeable_ones() {
    // `continuations` returns table order, and a narrow panel keeps its head. Sorting by
    // spelling instead puts `1`-`9` first, and a 20-column panel then shows four lines of
    // `Switch to session in quick slot N` and hides every action a user pressing the
    // leader is actually looking for. This holds the order that fix depends on.
    let entries = prefix(usize::MAX).continuations;
    let slots = entries
        .iter()
        .filter(|entry| entry.definition.name.starts_with("session_quick_switch"))
        .count();
    assert!(
        slots >= 9,
        "the nine quick-slot rows are what make this hazard real; found {slots}"
    );
    let first_slot = entries
        .iter()
        .position(|entry| entry.definition.name.starts_with("session_quick_switch"))
        .expect("a quick slot is reachable");
    assert!(
        first_slot >= entries.len() - slots,
        "a quick-slot row appears at index {first_slot} of {}, so the interchangeable \
         rows are no longer last and a narrow panel will hide the distinct ones",
        entries.len()
    );

    let mut view = which_key(usize::MAX);
    let rendered = rows(&render_offscreen(&mut view, 20, 10).expect("infallible")).join("\n");
    assert!(
        !rendered.contains("quick slot"),
        "the narrowest panel spent all its rows on interchangeable slots:\n{rendered}"
    );
}

#[test]
fn views_which_key_is_blank_until_a_prefix_arrives() {
    let mut view = WhichKeyView::new(ViewContext::defaults());
    assert!(!view.is_active());
    assert_eq!(view.desired_height(20), 0);
    let rendered = rows(&render_offscreen(&mut view, 40, 3).expect("infallible"));
    assert!(
        rendered.iter().all(|row| row.trim().is_empty()),
        "an idle which-key painted something:\n{}",
        rendered.join("\n")
    );
}

#[test]
fn views_which_key_closes_when_the_prefix_is_abandoned() {
    // The dispatcher reports an inactive prefix on the Action and Unmatched branches.
    // A panel that ignored that would sit over every later keystroke.
    let mut view = which_key(3);
    assert!(view.is_active());
    assert!(view.observe(&PendingPrefix::default(), Instant::now()));
    assert!(!view.is_active());
    assert_eq!(view.desired_height(20), 0);
}

#[test]
fn views_which_key_expires_on_the_leader_timeout() {
    let timeout = ViewContext::defaults().config.leader_timeout;
    let shown = Instant::now();
    let mut view = which_key_at(3, shown);
    assert!(
        !view.prune(shown + timeout - Duration::from_millis(1)),
        "the panel expired before the leader timeout"
    );
    assert!(view.is_active());
    assert!(
        view.prune(shown + timeout),
        "the panel outlived the leader timeout, so it hangs until the next key"
    );
    assert!(!view.is_active());
}

#[test]
fn views_which_key_never_takes_more_than_half_the_frame() {
    // 32 continuations against a 10-row frame. Without the ceiling the panel is the
    // whole screen and the transcript it exists to annotate is gone.
    let view = which_key(32);
    for available in [4u16, 10, 24, 40] {
        let height = view.desired_height(available);
        assert!(
            height <= available / 2 || height == 1,
            "with {available} rows the panel wanted {height}"
        );
        assert!(height >= 1, "the panel vanished at {available} rows");
    }
}

#[test]
fn views_which_key_counts_what_the_grid_could_not_hold() {
    // Truncating silently would teach the user the leader has fewer continuations than
    // it does, which is worse than not showing the panel.
    let mut view = which_key(32);
    let rendered = rows(&render_offscreen(&mut view, 40, 4).expect("infallible"));
    let joined = rendered.join("\n");
    assert!(
        joined.contains("more"),
        "a full grid did not say how many it dropped:\n{joined}"
    );
}

#[test]
fn views_which_key_pads_by_columns_not_characters() {
    // The bug this view shipped with, and why it survived: `chars().count()` plus
    // `{:<12}` both undercount a CJK glyph by one column each, and the one test that
    // rendered it used ASCII only. Asserted on the buffer's own column index, because
    // `rows()` re-reads a wide glyph's continuation cell as a space.
    let mut view = WhichKeyView::new(ViewContext::defaults());
    let mut wide = prefix(2);
    wide.continuations[0].keys = String::from("日");
    view.observe(&wide, Instant::now());

    let buffer = render_offscreen(&mut view, 40, 1).expect("infallible");
    let width = usize::from(buffer.area.width);
    let occupied = (0..width)
        .filter(|column| {
            let symbol = buffer[(u16::try_from(*column).unwrap_or(0), 0)].symbol();
            !symbol.trim().is_empty()
        })
        .count();
    assert!(
        occupied > 0,
        "a wide key produced an empty row, so the padding arithmetic collapsed it"
    );
    let (_, cell) = WhichKeyView::plan_columns(buffer.area.width, 1, 2);
    assert!(
        usize::from(cell) <= width,
        "a cell wider than the frame will wrap: cell {cell}, frame {width}"
    );
}

#[test]
fn views_which_key_survives_the_smallest_frame() {
    let mut view = which_key(32);
    for (width, height) in [(20u16, 10u16), (1, 1), (4, 2), (13, 3)] {
        let buffer = render_offscreen(&mut view, width, height).expect("infallible");
        assert_eq!(buffer.area.width, width);
    }
}

#[test]
#[ignore = "printer, not an assertion: run with --ignored --nocapture to eyeball the rendering"]
fn views_which_key_visual_probe() {
    for (width, height) in [(120u16, 12u16), (80, 10), (40, 6), (20, 10)] {
        println!("\n=========== which-key, {width}x{height} ===========");
        let mut view = which_key(usize::MAX);
        let rows_wanted = view.desired_height(height);
        println!("(panel takes {rows_wanted} of {height} rows)");
        for row in
            rows(&render_offscreen(&mut view, width, rows_wanted.max(1)).expect("infallible"))
        {
            println!("|{}|", row.trim_end());
        }
    }
}
