//! Diff rendering tests, including the `diff_style` fork.

use super::*;
use crate::app::render_offscreen;
use crate::config::{DiffStyle, ResolvedTuiConfig};
use crate::theme::{Mode, ThemeRegistry};
use crate::views::testkit::rows;

/// A patch with one changed line, its git header, and context either side.
///
/// Written as a concatenation rather than a `\`-continued literal because the
/// continuation form silently absorbs the source file's own indentation into the
/// diff body, and a diff's leading whitespace is significant.
const PATCH: &str = concat!(
    "diff --git a/src/main.rs b/src/main.rs\n",
    "@@ -3,4 +3,4 @@ fn main() {\n",
    " let config = load();\n",
    "-println!(\"old\");\n",
    "+println!(\"new\");\n",
    " done();\n",
);

fn context_with(diff_style: Option<DiffStyle>) -> ViewContext {
    let registry = ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, Mode::Dark);
    ViewContext::new(
        &resolved,
        ResolvedTuiConfig {
            diff_style,
            ..ResolvedTuiConfig::default()
        },
    )
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[test]
fn views_diff_parse_classifies_every_line_and_numbers_them_from_the_hunk() {
    let parsed = parse(PATCH);
    let kinds = parsed.iter().map(|line| line.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            LineKind::Header,
            LineKind::Header,
            LineKind::Context,
            LineKind::Removed,
            LineKind::Added,
            LineKind::Context,
        ]
    );
    // The hunk header says the old file resumes at line 3, so the context line
    // before the change is line 3, not line 1.
    assert_eq!(parsed[2].old, Some(3));
    assert_eq!(parsed[2].new, Some(3));
    assert_eq!(parsed[3].old, Some(4));
    assert_eq!(parsed[3].new, None, "a removal has no new-file line number");
    assert_eq!(parsed[4].old, None);
    assert_eq!(parsed[4].new, Some(4));
}

#[test]
fn views_diff_parse_strips_the_marker_from_the_text() {
    let parsed = parse("+added\n-removed\n context\n");
    assert_eq!(parsed[0].text, "added");
    assert_eq!(parsed[1].text, "removed");
    assert_eq!(parsed[2].text, "context");
}

#[test]
fn views_diff_parse_keeps_an_unmarked_line_as_context() {
    // Dropping it would hide a line of a patch the user is about to approve.
    let parsed = parse("@@ -1 +1 @@\nunmarked\n");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[1].kind, LineKind::Context);
    assert_eq!(parsed[1].text, "unmarked");
}

#[test]
fn views_diff_parse_recognises_the_git_header_family() {
    let parsed = parse(
        "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\nnew file mode 100644\n@@ -0,0 +1 @@\n+x\n",
    );
    assert_eq!(
        parsed
            .iter()
            .filter(|line| line.kind == LineKind::Header)
            .count(),
        6
    );
}

#[test]
fn views_diff_parse_handles_an_empty_patch() {
    assert!(parse("").is_empty());
}

// ---------------------------------------------------------------------------
// The `diff_style` fork
// ---------------------------------------------------------------------------

#[test]
fn views_diff_style_stacked_is_always_one_column() {
    let context = context_with(Some(DiffStyle::Stacked));
    assert_eq!(context.diff_columns(200), DiffColumns::Unified);
    assert_eq!(context.diff_columns(40), DiffColumns::Unified);
}

#[test]
fn views_diff_style_auto_splits_only_above_the_oracle_threshold() {
    for style in [Some(DiffStyle::Auto), None] {
        let context = context_with(style);
        assert_eq!(
            context.diff_columns(crate::views::SPLIT_DIFF_MIN_WIDTH),
            DiffColumns::Unified,
            "the threshold is exclusive, matching `width > 120`"
        );
        assert_eq!(
            context.diff_columns(crate::views::SPLIT_DIFF_MIN_WIDTH + 1),
            DiffColumns::Split
        );
    }
}

#[test]
fn views_diff_view_uses_the_configured_layout() {
    let mut stacked = DiffView::new(context_with(Some(DiffStyle::Stacked)), PATCH);
    stacked.lines(200);
    assert_eq!(stacked.columns(), DiffColumns::Unified);

    let mut auto = DiffView::new(context_with(Some(DiffStyle::Auto)), PATCH);
    auto.lines(200);
    assert_eq!(auto.columns(), DiffColumns::Split);
}

// ---------------------------------------------------------------------------
// The off-screen assertions, one per layout
// ---------------------------------------------------------------------------

#[test]
fn views_diff_renders_unified_offscreen() {
    let mut view = DiffView::new(context_with(Some(DiffStyle::Stacked)), PATCH);
    let rendered = rows(&render_offscreen(&mut view, 46, 6).expect("infallible"));
    let joined = rendered.join("\n");
    assert!(
        joined.contains("@@ -3,4 +3,4 @@"),
        "the hunk header is missing:\n{joined}"
    );
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("4- println!(\"old\")")),
        "a removal did not render with its line number and sign: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("4+ println!(\"new\")")),
        "an addition did not render with its line number and sign: {rendered:?}"
    );
}

#[test]
fn views_diff_renders_split_offscreen_with_both_sides() {
    const WIDTH: u16 = 130;
    let mut view = DiffView::new(context_with(Some(DiffStyle::Auto)), PATCH);
    let rendered = rows(&render_offscreen(&mut view, WIDTH, 6).expect("infallible"));
    let paired = rendered
        .iter()
        .find(|row| row.contains("old") && row.contains("new"))
        .unwrap_or_else(|| panic!("no row carries both sides of the change: {rendered:?}"));
    let old_at = paired.find("old").expect("the old side");
    let new_at = paired.find("new").expect("the new side");
    assert!(
        old_at < new_at,
        "the split layout put the new side on the left: {paired:?}"
    );
    assert!(
        new_at >= usize::from(WIDTH / 2),
        "the new side did not start in the right-hand column: {paired:?}"
    );
}

#[test]
fn views_diff_split_pads_an_unpaired_addition() {
    // Three additions against one removal: the two extra rows have to render with an
    // empty left column rather than being dropped or shifted.
    let patch = "@@ -1,1 +1,3 @@\n-one\n+a\n+b\n+c\n";
    let mut view =
        DiffView::new(context_with(Some(DiffStyle::Auto)), patch).with_columns(DiffColumns::Split);
    let rendered = rows(&render_offscreen(&mut view, 40, 5).expect("infallible"));
    let body = rendered
        .iter()
        .filter(|row| !row.contains("@@") && !row.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        body.len(),
        3,
        "an unpaired addition was dropped: {rendered:?}"
    );
    for letter in ["a", "b", "c"] {
        assert!(
            body.iter().any(|row| row.contains(letter)),
            "addition {letter:?} is missing: {rendered:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

#[test]
fn views_diff_uses_the_dedicated_diff_palette_keys_not_the_status_colours() {
    let context = context_with(Some(DiffStyle::Stacked));
    let mut view = DiffView::new(context.clone(), PATCH);
    let buffer = render_offscreen(&mut view, 46, 6).expect("infallible");
    let backgrounds = (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
        .map(|(x, y)| buffer[(x, y)].bg)
        .collect::<Vec<_>>();
    assert!(
        backgrounds.contains(&ratatui::style::Color::from(
            context.palette().diff_added_bg
        )),
        "no cell uses `diffAddedBg`"
    );
    assert!(
        backgrounds.contains(&ratatui::style::Color::from(
            context.palette().diff_removed_bg
        )),
        "no cell uses `diffRemovedBg`"
    );
    assert!(
        backgrounds.contains(&ratatui::style::Color::from(
            context.palette().diff_added_line_number_bg
        )),
        "no cell uses `diffAddedLineNumberBg`, so the gutter is not distinguished"
    );
}

#[test]
fn views_diff_hunk_header_uses_its_own_colour() {
    let context = context_with(Some(DiffStyle::Stacked));
    let mut view = DiffView::new(context.clone(), "@@ -1 +1 @@\n x\n");
    let buffer = render_offscreen(&mut view, 20, 2).expect("infallible");
    assert_eq!(
        buffer[(0, 0)].fg,
        ratatui::style::Color::from(context.palette().diff_hunk_header)
    );
}

#[test]
fn views_diff_scroll_offset_skips_leading_rows() {
    let mut view = DiffView::new(context_with(Some(DiffStyle::Stacked)), PATCH);
    view.set_offset(2);
    let rendered = rows(&render_offscreen(&mut view, 46, 4).expect("infallible"));
    assert!(
        !rendered.join("\n").contains("diff --git"),
        "a scrolled diff still shows its first row: {rendered:?}"
    );
}

#[test]
fn views_diff_narrow_width_does_not_panic() {
    // A one-column frame is reachable during a resize, and arithmetic that assumed
    // room for the gutter would panic there.
    for width in 1..8 {
        let mut view = DiffView::new(context_with(Some(DiffStyle::Auto)), PATCH)
            .with_columns(DiffColumns::Split);
        assert!(
            render_offscreen(&mut view, width, 4).is_ok(),
            "width {width}"
        );
        let mut unified = DiffView::new(context_with(Some(DiffStyle::Stacked)), PATCH);
        assert!(
            render_offscreen(&mut unified, width, 4).is_ok(),
            "width {width}"
        );
    }
}

#[test]
fn views_diff_parsed_is_exposed_for_a_caller_that_wants_the_lines() {
    let view = DiffView::new(context_with(None), PATCH);
    assert_eq!(view.parsed().len(), 6);
}
