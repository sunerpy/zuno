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

#[test]
fn views_autocomplete_mouse_selects_a_visible_slash_command_row() {
    let mut view = open("/");
    let area = Rect::new(10, 5, 50, view.height());
    assert!(
        view.select_at(12, 6, area),
        "a click on the second visible command was ignored"
    );
    assert_eq!(view.cursor(), 1);
    assert_eq!(
        view.selected().map(|candidate| candidate.insert.as_str()),
        Some("/session ")
    );
    assert!(
        !view.select_at(12, area.bottom().saturating_sub(1), area),
        "the hint footer was treated as a selectable command"
    );
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
    // Eleven, not ten: `height` reports the rows the *popup* needs, and one of them is its
    // hint row. The cap being asserted is on the candidates, so it is asserted on the
    // candidates above; restating it here as a height would only re-derive the same number
    // through the row that is not a candidate.
    assert_eq!(
        view.height(),
        11,
        "the popup no longer asks for its ten candidates plus a hint row"
    );
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
    // This asserted `rendered[0].starts_with(" / ")`, which is the defect written down as the
    // expectation: the ` / ` was the kind's marker, and the `/session` that followed brought
    // its own. The assertion that should have caught the doubled sigil is what froze it, so
    // the claim is now about the command being legible rather than about a glyph before it.
    let first = &rendered[0];
    assert!(
        first.trim_start().starts_with('/'),
        "the command row does not lead with the command: {first:?}"
    );
    assert_eq!(
        first.matches('/').count(),
        1,
        "the command row repeats the slash: {first:?}"
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

/// The markers that exist are distinct, and the kinds without one say why.
///
/// This asserted all five `glyph()`s were distinct. The property it protected — a reader can
/// tell one kind from another — is kept; what changed is the *carrier*. `Command` and `Agent`
/// have no marker of their own because their `display` opens with `/` or `@` already, and
/// emitting a marker for them printed the sigil twice (` / /mcp`). So the claim is split: the
/// markers that are drawn must not collide, and a kind without one must be a kind whose
/// display supplies the sigil instead. Asserting only the first half would let a future
/// `File` marker be dropped to `None` and lose its identity silently.
#[test]
fn views_autocomplete_every_kind_stays_distinguishable() {
    let kinds = [
        CandidateKind::Command,
        CandidateKind::File,
        CandidateKind::Directory,
        CandidateKind::Agent,
        CandidateKind::Reference,
    ];
    let markers: Vec<&str> = kinds.iter().filter_map(|kind| kind.marker()).collect();
    let unique = markers.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        markers.len(),
        "two candidate kinds share a marker, so the list cannot be read: {markers:?}"
    );
    for kind in kinds {
        if kind.marker().is_some() {
            continue;
        }
        // A markerless kind must be one whose rendered display carries the sigil. Checked
        // against a real candidate from the production sources rather than against a list
        // written here, so a kind that lost its marker without its display gaining a sigil
        // fails instead of being excused by this test's own table.
        // The probe carries a letter as well as the sigil: a bare `/` scores every command at
        // the empty-query floor of 1, which the `Command` branch then discards for being
        // under its prefix threshold, so a sigil-only query returns nothing to inspect.
        let (sigil, probe) = match kind {
            CandidateKind::Command => ('/', "/sess"),
            CandidateKind::Agent => ('@', "@expl"),
            other => panic!("{other:?} has no marker and no sigil, so its rows are unlabelled"),
        };
        let view = open(probe);
        let found = view
            .matches()
            .iter()
            .find(|candidate| candidate.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} candidate to check the sigil against"));
        assert!(
            found.display.starts_with(sigil),
            "{kind:?} draws no marker, so its display must open with {sigil:?}: {:?}",
            found.display
        );
    }
}

/// A slash candidate's row carries exactly one `/`, the one in its own name.
///
/// The measured defect: `/mcp` rendered as ` / /mcp` — the kind's marker plus a display that
/// already began with `/`. Counted over the row rather than compared to a fixed string so the
/// assertion survives a change to the padding, and asserted on the row `lines()` actually
/// produces rather than on `display` alone, because the duplication was introduced by the row
/// composer and not by the candidate.
#[test]
fn views_autocomplete_a_command_row_carries_one_slash() {
    // The reported command, so the row under assertion is the one that was captured.
    let mut view = AutocompleteView::new(
        ViewContext::defaults(),
        Box::new(StaticSource::new().command("mcp", "List MCP servers")),
    );
    view.refresh("/mcp", 4);
    let rows = view.lines(60);
    let row = rows
        .first()
        .expect("a `/mcp` query matches at least the mcp command");
    let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
    assert_eq!(
        text.matches('/').count(),
        1,
        "the candidate row repeats the slash: {text:?}"
    );
    assert!(
        text.contains("/mcp"),
        "the row lost the command it is offering: {text:?}"
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

#[test]
fn views_which_key_is_a_compact_centered_framed_overlay() {
    let mut view = which_key(usize::MAX);
    let buffer = render_offscreen(&mut view, 100, 24).expect("infallible");
    let rendered = rows(&buffer).join("\n");
    assert!(
        rendered.contains("Next key"),
        "the leader help has no title, so it is visually indistinguishable from transcript          content:\n{rendered}"
    );

    let corner = (0..buffer.area.height).find_map(|row| {
        (0..buffer.area.width)
            .find(|column| buffer[(*column, row)].symbol() == "╭")
            .map(|column| (column, row))
    });
    let (left, top) =
        corner.unwrap_or_else(|| panic!("the leader help has no visible frame:\n{rendered}"));
    assert!(
        left > 0 && top > 0,
        "the leader help is pinned to the terminal edge instead of floating: {left},{top}"
    );
    assert!(
        view.desired_height(24) < 12,
        "a wide terminal still receives a half-screen-tall key map instead of packing columns"
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
fn views_which_key_hides_numeric_quick_slots_without_disabling_them() {
    let entries = prefix(usize::MAX).continuations;
    assert!(
        entries
            .iter()
            .all(|entry| !entry.definition.name.starts_with("session_quick_switch")),
        "the leader overlay repeated the same quick-slot action once per digit: {entries:#?}"
    );

    let mut keymap = crate::keybind::Keymap::defaults().expect("the shipped table builds");
    let now = Instant::now();
    assert_eq!(
        keymap.resolve(&["session"], keymap.leader(), now),
        crate::keybind::Resolution::Pending
    );
    assert!(matches!(
        keymap.resolve(
            &["session"],
            crate::keybind::Chord::parse("1").expect("digit chord"),
            now
        ),
        crate::keybind::Resolution::Action { definition, .. }
            if definition.name == "session_quick_switch_1"
    ));
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
fn views_which_key_keeps_an_action_before_overflow_on_a_short_frame() {
    let mut view = which_key(32);
    let rendered = rows(&render_offscreen(&mut view, 40, 6).expect("infallible")).join("\n");
    assert!(
        rendered.contains("Export"),
        "the frame spent its only content row on the overflow count and exposed no action:\n{rendered}"
    );
    assert!(
        rendered.contains("more"),
        "overflow became silent:\n{rendered}"
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
        for row in rows(&render_offscreen(&mut view, width, height).expect("infallible")) {
            println!("|{}|", row.trim_end());
        }
    }
}

// ---------------------------------------------------------------------------
// Where the popup floats
// ---------------------------------------------------------------------------

/// The popup is centred in the region it floats over, at every supported width.
#[test]
fn views_autocomplete_floats_centred_at_every_supported_width() {
    for width in [200u16, 120, 80, 60, 40] {
        let view = open("/sess");
        let main = Rect::new(0, 0, width, 24);
        let frame = view
            .overlay_frame(main)
            .expect("an open popup wants a frame in a 24-row region");

        // Centred, to within the odd-remainder column and row a centre cannot split. Asserted
        // as a symmetry between the two margins rather than against a computed x/y, which would
        // just restate the implementation's own arithmetic.
        let left = frame.x - main.x;
        let right = main.width - frame.width - left;
        assert!(
            left.abs_diff(right) <= 1,
            "at {width} columns the popup is not horizontally centred: {left} left, {right} right"
        );
        let above = frame.y - main.y;
        let below = main.height - frame.height - above;
        assert!(
            above.abs_diff(below) <= 1,
            "at {width} columns the popup is not vertically centred: {above} above, {below} below"
        );
        assert!(
            frame.y > main.y,
            "at {width} columns the popup still starts at the region's top edge"
        );
        assert!(
            frame.y + frame.height < main.y + main.height,
            "at {width} columns the popup still reaches the region's bottom edge, which is the \
             reported placement"
        );
        assert!(
            frame.width <= main.width && frame.height <= main.height,
            "at {width} columns the popup does not fit the region it floats over: {frame:?}"
        );
        // Symmetry alone is satisfied by a full-width band — margins of zero on both sides — and
        // a band spanning the terminal is the placement being replaced, not a centred popup. So
        // the width has to be content-derived wherever there is room for it to be.
        if width > OVERLAY_MIN_COLS {
            assert!(
                frame.width < main.width,
                "at {width} columns the popup spans the whole region instead of taking the \
                 columns its candidates need: {frame:?}"
            );
        }
    }
}

/// The popup keeps its own hint row along its bottom edge.
#[test]
fn views_autocomplete_hints_sit_along_the_bottom_of_the_popup() {
    // Rendered at the width the popup itself asks for, not at an arbitrary one: sizing is part
    // of the claim, so a test that handed it 60 columns would hide a popup that asks for 30.
    let mut view = open("/sess");
    let height = view.height();
    let width = view
        .overlay_frame(Rect::new(0, 0, 120, 24))
        .expect("an open popup wants a frame")
        .width;
    let rendered = rows(&render_offscreen(&mut view, width, height).expect("infallible"));
    let last = rendered.last().expect("the popup drew rows");
    assert!(
        last.contains("tab") && last.contains("complete") && last.contains("esc"),
        "the popup's last row is not its hint row: {rendered:?}"
    );
    // The *last* pair spelled in full, which the first assertion does not cover: sized from its
    // candidates alone the popup came out narrower than its own hints and this row rendered as
    // `esc dis` on a real 120-column frame. A half-spelled key still reads as a key.
    assert!(
        last.contains("dismiss"),
        "the popup clipped its own hint row: {last:?}"
    );
    // And the candidates are all still there, so the hint row was added rather than taken out of
    // the list — the degradation this could have introduced.
    let joined = rendered.join("\n");
    for expected in ["/session", "/new"] {
        assert!(
            joined.contains(expected),
            "the hint row cost the list a candidate ({expected}):\n{joined}"
        );
    }
}

/// A popup narrower than its floor takes the region instead of overflowing it.
#[test]
fn views_autocomplete_degrades_rather_than_overflowing_a_narrow_region() {
    for width in [30u16, 20, 12, 4, 1] {
        let view = open("/sess");
        let Some(frame) = view.overlay_frame(Rect::new(0, 0, width, 12)) else {
            continue;
        };
        assert!(
            frame.width <= width,
            "at {width} columns the popup asked for {} and would be clipped",
            frame.width
        );
    }
    // A region with no rows yields no frame rather than a zero-height one, which would be an
    // invisible layer the renderer still walked.
    let view = open("/sess");
    assert_eq!(view.overlay_frame(Rect::new(0, 0, 40, 0)), None);
}

/// A candidate whose text is CJK is measured in columns, not characters.
#[test]
fn views_autocomplete_measures_a_cjk_candidate_in_terminal_columns() {
    let mut view = AutocompleteView::new(
        ViewContext::defaults(),
        // Sixteen characters of description, thirty-two columns of it.
        Box::new(StaticSource::new().command("session", "切换会话并保留上下文与历史记录")),
    );
    view.refresh("/sess", 5);
    let frame = view
        .overlay_frame(Rect::new(0, 0, 120, 24))
        .expect("an open popup wants a frame");

    let display = crate::views::display_width("/session");
    let description = crate::views::display_width("切换会话并保留上下文与历史记录");
    assert!(
        description > "切换会话并保留上下文与历史记录".chars().count(),
        "the fixture is not wide-character text, so this test proves nothing"
    );
    assert!(
        usize::from(frame.width) >= display + description,
        "the popup was sized by characters rather than columns, so the row will be clipped: \
         {} columns for {} of content",
        frame.width,
        display + description
    );

    // And nothing is dropped when it is drawn: the row is one cell per column, so the text is
    // reassembled from the frame with ratatui's wide-character padding removed.
    let popup_height = view.height();
    let rendered = rows(&render_offscreen(&mut view, frame.width, popup_height).expect("ok"));
    let joined: String = rendered.join("").chars().filter(|c| *c != ' ').collect();
    assert!(
        joined.contains("切换会话并保留上下文与历史记录"),
        "a wide-character description lost characters on the way to the frame: {rendered:?}"
    );
}
