//! File tree, hunk navigation, word-level refinement, and the ceilings on all three.
//!
//! Frame assertions go through [`crate::views::dialog::DialogHost`] rather than calling
//! [`DiffBrowser::lines`] directly, because that is the only route production takes:
//! `diff_open` → `SessionScreen::diff_view` → `DiffDialog` → the host, which clamps the
//! width to a tier and windows the body. A test that composed rows itself would pass
//! while the tier left the tree no room.

use super::*;
use crate::app::render_offscreen;
use crate::config::{DiffStyle, ResolvedTuiConfig};
use crate::keybind::ActionComponent;
use crate::theme::{Mode, ThemeRegistry};
use crate::views::dialog::{DialogHost, ObservedBase};
use crate::views::diff::DiffDialog;
use crate::views::message::TranscriptView;
use crate::views::testkit::{action, press, rows};
use crossterm::event::KeyCode;

/// Three files, so the tree has a directory to compress and two siblings to order.
const MULTI: &str = concat!(
    "diff --git a/src/lib.rs b/src/lib.rs\n",
    "@@ -1,3 +1,3 @@\n",
    " pub mod app;\n",
    "-pub mod diff;\n",
    "+pub mod diff_browser;\n",
    " pub mod theme;\n",
    "diff --git a/src/views/diff.rs b/src/views/diff.rs\n",
    "@@ -10,4 +10,4 @@ impl DiffView {\n",
    " fn style(&self) {\n",
    "-    let width = text.chars().count();\n",
    "+    let width = display_width(text);\n",
    " }\n",
    "@@ -40,2 +40,3 @@ impl DiffView {\n",
    " done();\n",
    "+extra();\n",
    "diff --git a/README.md b/README.md\n",
    "@@ -1,2 +1,2 @@\n",
    "-# Old title\n",
    "+# New title\n",
    " body\n",
);

fn context() -> ViewContext {
    let registry = ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, Mode::Dark);
    ViewContext::new(
        &resolved,
        ResolvedTuiConfig {
            diff_style: Some(DiffStyle::Auto),
            ..ResolvedTuiConfig::default()
        },
    )
}

/// The browser mounted the way `diff_open` mounts it.
fn hosted(patch: &str) -> DialogHost {
    let context = context();
    let base = ObservedBase::new(TranscriptView::new(context.clone()));
    let mut host = DialogHost::new(context.clone(), Box::new(base));
    host.open(Box::new(DiffDialog::new(context, patch)));
    host
}

/// Send one resolved action, as the dispatcher would.
fn send(host: &mut DialogHost, name: &'static str, key: char) {
    host.handle_action(action(name), &press(KeyCode::Char(key)));
}

/// Terminal columns from the dialog's left border to the tree/patch divider, per row.
///
/// Read off the buffer's own coordinates rather than from a [`rows`] string, and that is
/// the load-bearing part. `rows` yields one entry per *cell*, so ratatui's continuation
/// cell for a wide glyph reads back as a space — `display_width` over that string counts
/// a CJK glyph as three columns (two for the glyph, one for the injected space) and
/// reported this tree as 37 columns wide when it is 32. A buffer index *is* a column, so
/// there is nothing left to miscount.
///
/// Two glyph complications remain. The dialog draws its own left rule with `│`, so the
/// first bar on a row is the border, not the divider; and the tree's line art uses `│`
/// too, so occurrences cannot be counted — but no patch body in this file contains one,
/// so the last bar is the divider.
fn tree_column_widths(buffer: &ratatui::buffer::Buffer) -> std::collections::BTreeSet<u16> {
    (0..buffer.area.height)
        .filter_map(|y| {
            let bars = (0..buffer.area.width)
                .filter(|x| buffer[(*x, y)].symbol() == "│")
                .collect::<Vec<_>>();
            let first = *bars.first()?;
            let last = *bars.last()?;
            (first != last).then_some(last - first)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Splitting a patch into files
// ---------------------------------------------------------------------------

#[test]
fn views_diff_browser_splits_a_multi_file_patch_into_its_files() {
    let files = split_files(MULTI);
    assert_eq!(
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/lib.rs", "src/views/diff.rs", "README.md"],
        "the `a/`/`b/` prefixes should be gone and the order should be the patch's"
    );
}

#[test]
fn views_diff_browser_counts_changes_without_counting_the_file_headers() {
    // `+++ b/x` begins with `+`. A tally that ran before the header test would report
    // one extra addition for every file in the patch, which is exactly the kind of
    // off-by-one that looks plausible in a title bar and is never questioned.
    let files = split_files(concat!(
        "diff --git a/x b/x\n",
        "--- a/x\n",
        "+++ b/x\n",
        "@@ -1,2 +1,2 @@\n",
        "-old\n",
        "+new\n",
    ));
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].additions, 1,
        "`+++ b/x` was counted as an addition"
    );
    assert_eq!(files[0].deletions, 1, "`--- a/x` was counted as a deletion");
}

#[test]
fn views_diff_browser_splits_a_patch_that_has_no_git_headers() {
    // What `zuno_tools::diff::unified_diff` emits: `--- `/`+++ ` and no `diff --git`.
    let files = split_files(concat!(
        "--- one.rs\n",
        "+++ one.rs\n",
        "@@ -1 +1 @@\n",
        "-a\n",
        "+b\n",
        "--- two.rs\n",
        "+++ two.rs\n",
        "@@ -1 +1 @@\n",
        "-c\n",
        "+d\n",
    ));
    assert_eq!(
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["one.rs", "two.rs"]
    );
}

#[test]
fn views_diff_browser_keeps_a_headerless_patch_as_one_unnamed_file() {
    // Rendering nothing for a patch that plainly has content is the worse failure.
    let files = split_files("@@ -1 +1 @@\n-a\n+b\n");
    assert_eq!(files.len(), 1);
    assert!(files[0].path.is_empty());
    assert_eq!(files[0].additions, 1);
}

// ---------------------------------------------------------------------------
// The tree
// ---------------------------------------------------------------------------

#[test]
fn views_diff_browser_tree_compresses_a_single_child_directory_chain() {
    let files = split_files(concat!(
        "diff --git a/crates/zuno-tui/src/views/diff.rs b/crates/zuno-tui/src/views/diff.rs\n",
        "@@ -1 +1 @@\n",
        "-a\n",
        "+b\n",
    ));
    let tree = tree_rows(&files);
    let labels = tree
        .iter()
        .map(|row| row.label.as_str())
        .collect::<Vec<_>>();
    // A chain with exactly one file at the end collapses all the way to one row.
    // Uncompressed this is five rows to say what one says.
    assert_eq!(
        labels,
        vec!["crates/zuno-tui/src/views/diff.rs"],
        "the chain was not compressed: {tree:?}"
    );
    assert_eq!(
        tree[0].file,
        Some(0),
        "the compressed row must still select"
    );
}

#[test]
fn views_diff_browser_tree_stops_compressing_at_a_branch() {
    // The counterpart to the case above: compression must not swallow a directory that
    // two files share, or the tree stops showing the structure it exists to show.
    let files = split_files(concat!(
        "diff --git a/src/views/diff.rs b/src/views/diff.rs\n",
        "@@ -1 +1 @@\n",
        "-a\n",
        "+b\n",
        "diff --git a/src/views/tool.rs b/src/views/tool.rs\n",
        "@@ -1 +1 @@\n",
        "-c\n",
        "+d\n",
    ));
    let tree = tree_rows(&files);
    let labels = tree
        .iter()
        .map(|row| row.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(labels, vec!["src/views", "diff.rs", "tool.rs"]);
    assert_eq!(tree[0].file, None, "the shared directory is not a file");
}

#[test]
fn views_diff_browser_tree_sorts_directories_before_files() {
    let tree = tree_rows(&split_files(MULTI));
    let labels = tree
        .iter()
        .map(|row| row.label.as_str())
        .collect::<Vec<_>>();
    // `src` is a directory and sorts ahead of `README.md` despite `R` < `s`.
    assert_eq!(labels[0], "src");
    assert_eq!(
        labels.last().copied(),
        Some("README.md"),
        "the file did not sort after the directory: {labels:?}"
    );
}

#[test]
fn views_diff_browser_tree_draws_both_a_continuation_and_a_corner() {
    let tree = tree_rows(&split_files(MULTI));
    let files = tree
        .iter()
        .filter(|row| row.file.is_some())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 3);
    assert!(
        tree.iter().any(|row| row.prefix.ends_with("└─ ")),
        "no row closed its level: {tree:?}"
    );
    assert!(
        tree.iter().any(|row| row.prefix.ends_with("├─ ")),
        "no row continued its level: {tree:?}"
    );
}

#[test]
fn views_diff_browser_tree_gives_an_unnamed_patch_a_selectable_row() {
    let tree = tree_rows(&split_files("@@ -1 +1 @@\n-a\n+b\n"));
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].file, Some(0), "the only file is not selectable");
}

// ---------------------------------------------------------------------------
// Width allocation
// ---------------------------------------------------------------------------

#[test]
fn views_diff_browser_width_plan_gives_the_tree_a_fixed_column_count() {
    let plan = plan_widths(114, true);
    assert_eq!(plan.tree, Some(FILE_TREE_WIDTH));
    assert_eq!(plan.patch, 114 - FILE_TREE_WIDTH - FILE_TREE_SEPARATOR);
}

#[test]
fn views_diff_browser_width_plan_drops_the_tree_at_its_own_threshold() {
    // Exactly at the threshold the tree survives and the patch gets its floor; one
    // column below, the tree goes rather than the patch shrinking under it.
    let at = plan_widths(FILE_TREE_MIN_TOTAL, true);
    assert_eq!(at.tree, Some(FILE_TREE_WIDTH));
    assert_eq!(at.patch, PATCH_MIN_WIDTH);

    let below = plan_widths(FILE_TREE_MIN_TOTAL - 1, true);
    assert_eq!(below.tree, None);
    assert_eq!(
        below.patch,
        FILE_TREE_MIN_TOTAL - 1,
        "dropping the tree must hand every column to the patch"
    );
}

#[test]
fn views_diff_browser_width_plan_honours_a_closed_tree_at_any_width() {
    assert_eq!(plan_widths(200, false).tree, None);
}

#[test]
fn views_diff_browser_layout_comes_from_the_one_diff_columns_fork() {
    // `§7.4` proposed a second threshold at 100 for the browser. The module docs record
    // why that is not what shipped; this is the assertion that keeps it one policy —
    // `stacked` must still win here, which a private second threshold would not honour.
    let registry = ThemeRegistry::new();
    let resolved = registry.resolve(crate::theme::DEFAULT_THEME, Mode::Dark);
    let stacked = ViewContext::new(
        &resolved,
        ResolvedTuiConfig {
            diff_style: Some(DiffStyle::Stacked),
            ..ResolvedTuiConfig::default()
        },
    );
    assert_eq!(columns_for(&stacked, 200), DiffColumns::Unified);
    assert_eq!(
        columns_for(&context(), crate::views::SPLIT_DIFF_MIN_WIDTH + 1),
        DiffColumns::Split
    );
}

// ---------------------------------------------------------------------------
// The rendered frame
// ---------------------------------------------------------------------------

#[test]
fn views_diff_browser_renders_the_tree_beside_the_patch() {
    let mut host = hosted(MULTI);
    let buffer = render_offscreen(&mut host, 200, 24).expect("infallible");
    let joined = rows(&buffer).join("\n");
    assert!(
        joined.contains("diff.rs"),
        "no tree row names a file:\n{joined}"
    );
    assert!(
        joined.contains("pub mod diff_browser;"),
        "the patch body is missing:\n{joined}"
    );
    // The divider must sit at one column on every row, or the two panels are not panels.
    let widths = tree_column_widths(&buffer);
    assert_eq!(
        widths.len(),
        1,
        "the divider is not at a single column: {widths:?}\n{joined}"
    );
    assert_eq!(
        widths.iter().next().copied(),
        Some(FILE_TREE_WIDTH + FILE_TREE_SEPARATOR),
        "the tree is not {FILE_TREE_WIDTH} columns wide"
    );
}

#[test]
fn views_diff_browser_marks_the_selected_file_with_the_shared_selection_colours() {
    // `§11.5`: `primary` behind `background`, the same pair every picker uses.
    let context = context();
    let mut host = hosted(MULTI);
    let buffer = render_offscreen(&mut host, 200, 24).expect("infallible");
    let selected = ratatui::style::Color::from(context.palette().primary);
    assert!(
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .any(|(x, y)| buffer[(x, y)].bg == selected),
        "no cell carries the selection background"
    );
}

#[test]
fn views_diff_browser_next_file_moves_the_selection_to_the_next_file() {
    let mut host = hosted(MULTI);
    let before = rows(&render_offscreen(&mut host, 200, 24).expect("infallible"));
    send(&mut host, "diff_next_file", 'n');
    let after = rows(&render_offscreen(&mut host, 200, 24).expect("infallible"));
    assert_ne!(
        before[0], after[0],
        "the title bar did not follow the selection:\n{before:?}\n{after:?}"
    );
    assert!(
        after[0].contains("src/views/diff.rs"),
        "`n` did not select the second file: {:?}",
        after[0]
    );
}

#[test]
fn views_diff_browser_file_navigation_saturates_at_both_ends() {
    let mut host = hosted(MULTI);
    render_offscreen(&mut host, 200, 24).expect("infallible");
    for _ in 0..6 {
        send(&mut host, "diff_next_file", 'n');
    }
    let last = rows(&render_offscreen(&mut host, 200, 24).expect("infallible"));
    assert!(
        last[0].contains("README.md"),
        "`n` past the end wrapped or overran: {:?}",
        last[0]
    );
    for _ in 0..6 {
        send(&mut host, "diff_previous_file", 'p');
    }
    let first = rows(&render_offscreen(&mut host, 200, 24).expect("infallible"));
    assert!(
        first[0].contains("src/lib.rs"),
        "`p` past the start wrapped or overran: {:?}",
        first[0]
    );
}

#[test]
fn views_diff_browser_toggling_the_tree_removes_it() {
    let mut host = hosted(MULTI);
    let before = rows(&render_offscreen(&mut host, 200, 24).expect("infallible"));
    assert!(before.iter().any(|row| row.contains("├─ ")));
    send(&mut host, "diff_toggle_file_tree", 'b');
    let after = rows(&render_offscreen(&mut host, 200, 24).expect("infallible"));
    assert!(
        !after.iter().any(|row| row.contains("├─ ")),
        "`b` left the tree drawn:\n{after:?}"
    );
}

#[test]
fn views_diff_browser_split_is_reachable_only_by_asking_for_it() {
    // The documented consequence of reusing one threshold: inside an `XLarge` modal the
    // widest patch area is 114 columns minus the tree, so `diff_columns` never chooses
    // split on its own. `v` is therefore the *only* route to it, and without this test
    // the split layout would be a rendering nobody can reach.
    let mut host = hosted(MULTI);
    let before = rows(&render_offscreen(&mut host, 200, 24).expect("infallible")).join("\n");
    assert!(
        before.contains("   2- pub mod diff;"),
        "the unified gutter is not what rendered by default:\n{before}"
    );
    send(&mut host, "diff_toggle_view", 'v');
    let after = rows(&render_offscreen(&mut host, 200, 24).expect("infallible"));
    let paired = after
        .iter()
        .find(|row| row.contains("pub mod diff;") && row.contains("pub mod diff_browser;"))
        .unwrap_or_else(|| panic!("`v` did not put both sides on one row: {after:?}"));
    let old_at = paired.find("pub mod diff;").expect("the old side");
    let new_at = paired.find("pub mod diff_browser;").expect("the new side");
    assert!(
        old_at < new_at,
        "the new side rendered on the left: {paired:?}"
    );
}

#[test]
fn views_diff_browser_single_patch_shows_only_the_selected_file() {
    let mut host = hosted(MULTI);
    let all = rows(&render_offscreen(&mut host, 200, 40).expect("infallible")).join("\n");
    assert!(all.contains("# New title"), "the third file is missing");
    send(&mut host, "diff_single_patch", 's');
    let one = rows(&render_offscreen(&mut host, 200, 40).expect("infallible")).join("\n");
    assert!(
        !one.contains("# New title"),
        "`s` still shows every file:\n{one}"
    );
    assert!(
        one.contains("pub mod diff_browser;"),
        "`s` dropped the selected file too:\n{one}"
    );
}

// ---------------------------------------------------------------------------
// Hunk navigation
// ---------------------------------------------------------------------------

#[test]
fn views_diff_browser_hunk_indices_find_only_hunk_headers() {
    // Not the whole `Header` family: `diff --git`, `index`, `---` and `+++` are headers
    // too, and a `]` that stopped on each would take four presses to cross a preamble.
    let lines = crate::views::diff::parse(concat!(
        "diff --git a/x b/x\n",
        "index 1..2 100644\n",
        "--- a/x\n",
        "+++ b/x\n",
        "@@ -1 +1 @@\n",
        "+a\n",
        "@@ -9 +9 @@\n",
        "+b\n",
    ));
    assert_eq!(hunk_indices(&lines), vec![4, 6]);
}

#[test]
fn views_diff_browser_bracket_keys_move_forward_and_back_between_hunks() {
    let mut host = hosted(MULTI);
    let first = rows(&render_offscreen(&mut host, 200, 8).expect("infallible"));

    send(&mut host, "diff_next_hunk", ']');
    let second = rows(&render_offscreen(&mut host, 200, 8).expect("infallible"));
    assert_ne!(
        first[1], second[1],
        "`]` did not scroll:\n{first:?}\n{second:?}"
    );
    assert!(
        second[1].contains("@@"),
        "`]` did not land on a hunk header: {:?}",
        second[1]
    );

    send(&mut host, "diff_next_hunk", ']');
    let third = rows(&render_offscreen(&mut host, 200, 8).expect("infallible"));
    assert!(third[1].contains("@@"), "the second `]` left the headers");
    assert_ne!(second[1], third[1], "the second `]` did not move");

    send(&mut host, "diff_previous_hunk", '[');
    let back = rows(&render_offscreen(&mut host, 200, 8).expect("infallible"));
    assert_eq!(
        back[1], second[1],
        "`[` did not return to the previous hunk:\n{third:?}\n{back:?}"
    );
}

#[test]
fn views_diff_browser_hunk_navigation_stops_at_the_ends_without_wrapping() {
    let mut host = hosted(MULTI);
    render_offscreen(&mut host, 200, 8).expect("infallible");
    for _ in 0..12 {
        send(&mut host, "diff_next_hunk", ']');
    }
    let last = rows(&render_offscreen(&mut host, 200, 8).expect("infallible"));
    assert!(
        last[1].contains("@@ -1,2 +1,2 @@"),
        "`]` past the last hunk wrapped to the top: {:?}",
        last[1]
    );
    for _ in 0..12 {
        send(&mut host, "diff_previous_hunk", '[');
    }
    let first = rows(&render_offscreen(&mut host, 200, 8).expect("infallible"));
    assert!(
        first[1].contains("@@ -1,3 +1,3 @@"),
        "`[` past the first hunk wrapped to the bottom: {:?}",
        first[1]
    );
}

#[test]
fn views_diff_browser_hunk_rows_are_measured_in_the_split_layout_too() {
    // In a split layout several parsed lines collapse onto one row, so a hunk's row
    // number is not its parse index. Deriving it from the parse would drift by one row
    // per pair — invisible on a patch with one hunk, wrong on every real patch.
    let mut view = crate::views::diff::DiffView::new(context(), MULTI);
    view.set_columns(DiffColumns::Split);
    let (split_rows, split_hunks) = view.rows_with_hunks(120);
    assert_eq!(split_hunks.len(), 4, "one hunk per `@@` in the fixture");
    for row in &split_hunks {
        let text = split_rows[*row]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            text.starts_with("@@"),
            "split row {row} is not a hunk header: {text:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Word-level refinement — a capability this project adds
// ---------------------------------------------------------------------------

#[test]
fn views_diff_browser_word_diff_marks_only_the_run_that_changed() {
    let (removed, added) = refine(
        "let width = text.chars().count();",
        "let width = display_width(text);",
    )
    .expect("the two lines share their opening");
    let unchanged = removed
        .iter()
        .filter(|span| !span.changed)
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert!(
        unchanged.starts_with("let width = "),
        "the shared prefix was marked as changed: {removed:?}"
    );
    assert!(
        removed.iter().any(|span| span.changed),
        "nothing was marked changed: {removed:?}"
    );
    assert!(
        added.iter().any(|span| span.changed),
        "nothing was marked changed on the new side: {added:?}"
    );
    // Reassembly must be lossless, or refinement is a way to silently drop code.
    assert_eq!(
        removed
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>(),
        "let width = text.chars().count();"
    );
    assert_eq!(
        added
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>(),
        "let width = display_width(text);"
    );
}

#[test]
fn views_diff_browser_word_diff_declines_an_unrelated_pair() {
    // Marking every word of both sides is the unrefined rendering drawn expensively,
    // with a highlight that now implies a distinction it is not making.
    assert!(refine("alpha", "!!!").is_none());
}

#[test]
fn views_diff_browser_word_diff_declines_an_identical_or_empty_pair() {
    assert!(refine("same", "same").is_none());
    assert!(refine("", "new").is_none());
    assert!(refine("old", "").is_none());
}

#[test]
fn views_diff_browser_word_diff_declines_a_grid_over_the_cell_ceiling() {
    // The ceiling is on the token grid and is checked before any comparison runs, so a
    // line that is long only because its tokens are long is still refined.
    let wide = std::iter::repeat_n("x ", 200).collect::<String>();
    let also_wide = format!("{wide}tail");
    assert!(
        refine(&wide, &also_wide).is_none(),
        "a grid this size is over the {MAX_WORD_DIFF_CELLS}-cell ceiling"
    );

    let long_tokens = format!("{} a", "y".repeat(400));
    let long_tokens_changed = format!("{} b", "y".repeat(400));
    assert!(
        refine(&long_tokens, &long_tokens_changed).is_some(),
        "a long line with few tokens is cheap and must still refine"
    );
}

#[test]
fn views_diff_browser_refinement_budget_is_observable_and_spent() {
    let patch = "@@ -1,2 +1,2 @@\n-let a = one;\n+let a = two;\n";
    let mut spending = crate::views::diff::DiffView::new(context(), patch);
    spending.set_columns(DiffColumns::Unified);
    spending.set_refine_budget(1);
    spending.lines(80);
    assert_eq!(
        spending.refine_budget(),
        0,
        "the pair did not spend the budget"
    );

    let mut exhausted = crate::views::diff::DiffView::new(context(), patch);
    exhausted.set_columns(DiffColumns::Unified);
    exhausted.set_refine_budget(0);
    let (without, _) = exhausted.rows_with_hunks(80);

    let mut refined = crate::views::diff::DiffView::new(context(), patch);
    refined.set_columns(DiffColumns::Unified);
    refined.set_refine_budget(MAX_WORD_DIFF_PAIRS);
    let (with, _) = refined.rows_with_hunks(80);

    assert!(
        with[1].spans.len() > without[1].spans.len(),
        "refinement produced no extra spans, so the budget is unobservable: {} vs {}",
        with[1].spans.len(),
        without[1].spans.len()
    );
}

#[test]
fn views_diff_browser_refined_run_is_painted_with_the_diff_highlight_keys() {
    let context = context();
    let mut host = hosted(concat!(
        "diff --git a/x.rs b/x.rs\n",
        "@@ -1,2 +1,2 @@\n",
        "-let width = text.chars().count();\n",
        "+let width = display_width(text);\n",
    ));
    let buffer = render_offscreen(&mut host, 200, 12).expect("infallible");
    let foregrounds = (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
        .map(|(x, y)| buffer[(x, y)].fg)
        .collect::<Vec<_>>();
    for (key, colour) in [
        ("diffHighlightAdded", context.palette().diff_highlight_added),
        (
            "diffHighlightRemoved",
            context.palette().diff_highlight_removed,
        ),
    ] {
        assert!(
            foregrounds.contains(&ratatui::style::Color::from(colour)),
            "no cell carries `{key}`, so the refinement is invisible"
        );
    }
    // The shared prefix must NOT be highlighted, or every cell is and the refinement
    // says nothing.
    for (key, colour) in [
        ("diffAdded", context.palette().diff_added),
        ("diffRemoved", context.palette().diff_removed),
    ] {
        assert!(
            foregrounds.contains(&ratatui::style::Color::from(colour)),
            "no cell carries `{key}`, so the whole changed line was highlighted and \
             nothing distinguishes the change"
        );
    }
}

// ---------------------------------------------------------------------------
// Ceilings
// ---------------------------------------------------------------------------

#[test]
fn views_diff_browser_byte_ceiling_abandons_the_tree_and_refinement() {
    // The two superlinear parts go; the patch still renders, which is the whole point of
    // a ceiling that degrades rather than errors.
    let big = format!(
        "diff --git a/x b/x\n@@ -1 +1 @@\n{}",
        std::iter::repeat_n("+padding padding padding\n", MAX_PATCH_BYTES / 20).collect::<String>()
    );
    assert!(big.len() > MAX_PATCH_BYTES);
    let browser = DiffBrowser::new(context(), &big);
    assert!(browser.is_plain());
    assert!(
        browser.tree().is_empty(),
        "the tree was built for an over-sized patch"
    );
    assert!(
        !browser.tree_open(),
        "the tree is still wanted, so it would be drawn empty beside the patch"
    );

    let small = DiffBrowser::new(context(), MULTI);
    assert!(!small.is_plain());
    assert!(!small.tree().is_empty());
    assert!(small.tree_open());
}

#[test]
fn views_diff_browser_line_ceiling_truncates_with_a_notice() {
    let long = format!(
        "diff --git a/x b/x\n@@ -1 +1 @@\n{}",
        std::iter::repeat_n("+line\n", MAX_PATCH_LINES + 500).collect::<String>()
    );
    let mut browser = DiffBrowser::new(context(), &long);
    let composed = browser.lines(80);
    assert!(browser.is_truncated());
    assert!(
        composed.len() <= MAX_PATCH_LINES + 1,
        "the ceiling did not bound the rows: {}",
        composed.len()
    );
    let last = composed
        .last()
        .expect("a truncated patch still has rows")
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(
        last.contains("patch truncated"),
        "the tail was dropped in silence: {last:?}"
    );

    let mut short = DiffBrowser::new(context(), MULTI);
    short.lines(80);
    assert!(!short.is_truncated());
}

// ---------------------------------------------------------------------------
// Wide glyphs
// ---------------------------------------------------------------------------

#[test]
fn views_diff_browser_pads_a_cjk_tree_label_by_columns_not_characters() {
    let files = split_files("diff --git a/文档/说明文件.md b/文档/说明文件.md\n@@ -1 +1 @@\n+x\n");
    let tree = tree_rows(&files);
    assert!(!tree.is_empty());
    for row in &tree {
        let line = tree_line(&context(), row, files.first(), false, FILE_TREE_WIDTH);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(
            display_width(&text),
            usize::from(FILE_TREE_WIDTH),
            "a CJK label was padded by character count, so the row is {} columns wide \
             instead of {FILE_TREE_WIDTH}: {text:?}",
            display_width(&text)
        );
    }
}

#[test]
fn views_diff_browser_keeps_a_cjk_patch_body_inside_its_cell() {
    // The defect `§10.2` names: a split cell filled by character count is twice as wide
    // as its cell for CJK and displaces the entire opposite side of the diff.
    let mut view = crate::views::diff::DiffView::new(
        context(),
        "@@ -1,2 +1,2 @@\n-旧的中文内容在这里\n+新的中文内容在这里\n",
    );
    view.set_columns(DiffColumns::Split);
    let (split, _) = view.rows_with_hunks(60);
    for row in &split {
        let text = row
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            display_width(&text) <= 60,
            "a CJK split row is {} columns wide in a 60-column frame: {text:?}",
            display_width(&text)
        );
    }
}

#[test]
fn views_diff_browser_cjk_tree_keeps_the_divider_at_one_column() {
    let mut host = hosted(concat!(
        "diff --git a/文档/说明.md b/文档/说明.md\n",
        "@@ -1 +1 @@\n",
        "-旧\n",
        "+新\n",
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        "@@ -1 +1 @@\n",
        "-a\n",
        "+b\n",
    ));
    let buffer = render_offscreen(&mut host, 200, 16).expect("infallible");
    let widths = tree_column_widths(&buffer);
    assert_eq!(
        widths.len(),
        1,
        "a CJK tree row displaced the divider: {widths:?}\n{:?}",
        rows(&buffer)
    );
    assert_eq!(
        widths.iter().next().copied(),
        Some(FILE_TREE_WIDTH + FILE_TREE_SEPARATOR)
    );
}

// ---------------------------------------------------------------------------
// Widths and the degenerate frame
// ---------------------------------------------------------------------------

#[test]
fn views_diff_browser_renders_at_every_required_width() {
    for width in [200u16, 120, 80, 60, 40] {
        let mut host = hosted(MULTI);
        let rendered = rows(&render_offscreen(&mut host, width, 20).expect("infallible"));
        for row in &rendered {
            assert!(
                display_width(row) <= usize::from(width),
                "a row overflowed a {width}-column frame: {row:?}"
            );
        }
        assert!(
            rendered.iter().any(|row| row.contains("Diff")),
            "the title vanished at {width} columns:\n{rendered:?}"
        );
    }
}

#[test]
fn views_diff_browser_survives_a_degenerate_frame() {
    // 20×10 is `§11.6`'s acceptance case, and it is reachable during a resize.
    let mut host = hosted(MULTI);
    assert!(render_offscreen(&mut host, 20, 10).is_ok());
    for width in 1..8u16 {
        let mut narrow = hosted(MULTI);
        assert!(
            render_offscreen(&mut narrow, width, 4).is_ok(),
            "width {width} panicked"
        );
    }
}

#[test]
fn views_diff_browser_handles_navigation_before_its_first_render() {
    // A key can arrive before a frame is drawn, and hunk rows are only known once the
    // rows have been composed. Every arm has to tolerate an empty index.
    let mut host = hosted(MULTI);
    for name in [
        "diff_next_hunk",
        "diff_previous_hunk",
        "diff_next_file",
        "diff_previous_file",
        "diff_toggle_file_tree",
        "diff_single_patch",
        "diff_toggle_view",
    ] {
        send(&mut host, name, 'x');
    }
    assert!(render_offscreen(&mut host, 120, 12).is_ok());
}

#[test]
fn views_diff_browser_does_not_print_the_path_twice_per_file() {
    // The title row names the path; the raw `diff --git` beneath it said the same thing
    // again, and with `index`/`---`/`+++` that is up to four wasted rows per file.
    let mut host = hosted(MULTI);
    let rendered = rows(&render_offscreen(&mut host, 200, 30).expect("infallible"));
    let joined = rendered.join("\n");
    assert!(
        !joined.contains("diff --git"),
        "the raw git header is still drawn beside a title row that names the file:\n{joined}"
    );
    assert_eq!(
        rendered
            .iter()
            .filter(|row| row.contains("src/lib.rs"))
            .count(),
        2,
        "`src/lib.rs` should appear once in the tree and once as a title: {rendered:?}"
    );
    // Hunk headers must survive — they carry the line numbers and they are what `[`/`]`
    // navigate between.
    assert!(joined.contains("@@ -1,3 +1,3 @@"));
}

#[test]
fn views_diff_browser_body_patch_keeps_hunks_and_content() {
    let stripped = body_patch(concat!(
        "diff --git a/x b/x\n",
        "index 1..2 100644\n",
        "--- a/x\n",
        "+++ b/x\n",
        "new file mode 100644\n",
        "@@ -1,2 +1,2 @@\n",
        "-old\n",
        "+new\n",
    ));
    assert_eq!(stripped, "@@ -1,2 +1,2 @@\n-old\n+new\n");
}

#[test]
fn views_diff_browser_keeps_the_close_hint_at_the_narrowest_tier() {
    // The host's footer is a `Paragraph`, which drops the tail. A hint list long enough
    // to overflow loses its last entry — and that entry was `esc close`, the only thing
    // on screen saying how to leave a modal that still owns the keyboard.
    for width in [200u16, 120, 80, 60] {
        let mut host = hosted(MULTI);
        let rendered = rows(&render_offscreen(&mut host, width, 20).expect("infallible"));
        let footer = rendered
            .last()
            .unwrap_or_else(|| panic!("a {width}-column frame has rows"));
        assert!(
            footer.contains("esc"),
            "the close hint was truncated away at {width} columns: {footer:?}"
        );
    }
}

#[test]
#[ignore = "printer, not an assertion: run with --ignored --nocapture to eyeball the rendering"]
fn views_diff_browser_visual_probe() {
    for width in [120u16, 60] {
        println!("\n=========== multi-file patch, {width} columns ===========");
        let mut host = hosted(MULTI);
        for row in rows(&render_offscreen(&mut host, width, 26).expect("infallible")) {
            println!("|{}|", row.trim_end());
        }
    }
    // The refinement is a colour distinction, so a text dump cannot show it. Bracketing
    // the changed runs is how it becomes eyeballable at all.
    println!("\n=========== word-level refinement, run segmentation ===========");
    for (old, new) in [
        (
            "let used = owned.chars().count();",
            "let used = display_width(&owned);",
        ),
        ("pub mod diff;", "pub mod diff_browser;"),
        ("旧的中文标题在这里", "新的中文标题在这里"),
        (
            "    let width = text.chars().count();",
            "    let width = display_width(text);",
        ),
    ] {
        match refine(old, new) {
            Some((removed, added)) => {
                for (sign, spans) in [('-', &removed), ('+', &added)] {
                    let marked = spans
                        .iter()
                        .map(|span| {
                            if span.changed {
                                format!("⟦{}⟧", span.text)
                            } else {
                                span.text.clone()
                            }
                        })
                        .collect::<String>();
                    println!("{sign} {marked}");
                }
            }
            None => println!("(declined) - {old}\n(declined) + {new}"),
        }
        println!();
    }

    println!("\n=========== word-level refinement, 120 columns ===========");
    let mut host = hosted(concat!(
        "diff --git a/src/views.rs b/src/views.rs\n",
        "@@ -12,4 +12,4 @@ pub fn padded() {\n",
        " let mut owned = truncate(text, width);\n",
        "-let used = owned.chars().count();\n",
        "+let used = display_width(&owned);\n",
        " owned.extend(std::iter::repeat_n(' ', width - used));\n",
    ));
    for row in rows(&render_offscreen(&mut host, 120, 12).expect("infallible")) {
        println!("|{}|", row.trim_end());
    }
    println!("\n=========== CJK tree, 120 columns ===========");
    let mut cjk = hosted(concat!(
        "diff --git a/文档/说明文件.md b/文档/说明文件.md\n",
        "@@ -1,2 +1,2 @@\n",
        "-旧的中文标题在这里\n",
        "+新的中文标题在这里\n",
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        "@@ -1 +1 @@\n",
        "-pub mod diff;\n",
        "+pub mod diff_browser;\n",
    ));
    for row in rows(&render_offscreen(&mut cjk, 120, 14).expect("infallible")) {
        println!("|{}|", row.trim_end());
    }
}
