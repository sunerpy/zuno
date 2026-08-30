//! The multi-file half of the diff viewer: file tree, hunk index, word-level refinement.
//!
//! # Why this is a separate module from [`crate::views::diff`]
//!
//! `diff.rs` owns the *line* vocabulary — parse a unified patch, classify each line,
//! paint it in one or two columns. That vocabulary is also what the permission prompt
//! renders inline, and it must not grow a notion of "which file am I looking at" to
//! serve a modal that has one. This module owns the *patch* vocabulary instead: split a
//! patch into files, arrange those files as a tree, index the hunks, and refine a
//! changed line down to the words that changed. [`crate::views::diff::DiffDialog`]
//! composes the two.
//!
//! # Width is decided once, by the same fork everything else uses
//!
//! `§7.4` proposes a second threshold for the browser — side-by-side at a patch area of
//! 100 columns, distinct from [`crate::views::SPLIT_DIFF_MIN_WIDTH`]'s 120 — on the
//! grounds that the browser's patch area is narrower than its terminal. That premise is
//! right and the remedy is not: the reason `120` reads wrong for a browser is that it
//! was being compared against the *terminal* width while the patch only gets what the
//! tree leaves behind. Feeding the same [`crate::views::ViewContext::diff_columns`] the
//! patch area's own width fixes exactly that, with one constant instead of two and
//! without a second place that has to remember to honour `diff_style: "stacked"`.
//!
//! A consequence worth stating plainly, because it is not a bug: inside an
//! [`crate::views::dialog::DialogWidth::XLarge`] modal the widest patch area reachable
//! is 114 columns minus the tree, so the *automatic* answer is always
//! [`crate::views::DiffColumns::Unified`]. Split is reachable, but only the way a
//! reviewer asks for it — the `v` toggle, which forces the layout.
//!
//! # Three ceilings, because a patch has no upper bound
//!
//! A patch is produced by whatever tool asked, against whatever file the user had.
//! `apply_patch` over a vendored directory, or a generated file, arrives here as a
//! single string. Following the precedent set for markdown and tree-sitter, each
//! ceiling degrades to plainer rendering rather than erroring:
//!
//! * [`MAX_PATCH_BYTES`] — beyond it the tree and the word refinement are both
//!   abandoned and the patch renders as plain lines. Those two are the only parts whose
//!   cost is superlinear in the patch, and a reviewer facing half a megabyte of diff is
//!   scrolling, not navigating.
//! * [`MAX_PATCH_LINES`] — beyond it the tail is dropped and a notice row says so.
//! * [`MAX_WORD_DIFF_CELLS`] with [`MAX_WORD_DIFF_PAIRS`] — the refinement is the only
//!   quadratic step in the module, so it is bounded twice; see [`refine`].

use crate::views::diff::{DiffLine, LineKind};
use crate::views::{DiffColumns, ViewContext, display_width, truncate};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

#[cfg(test)]
#[path = "diff_browser_tests.rs"]
mod tests;

// ---------------------------------------------------------------------------
// Widths
// ---------------------------------------------------------------------------

/// Columns the file tree occupies when it is drawn.
///
/// `§7.4`: fixed, not a percentage. A proportional tree is the worse choice for the
/// reason a proportional gutter is: the thing in it — a path — has a length that does
/// not scale with the terminal, so a percentage either wastes columns on a wide frame
/// or truncates the same path that fit a moment ago.
pub const FILE_TREE_WIDTH: u16 = 32;

/// The single column between tree and patch.
///
/// One column, drawn as a rule, rather than a blank: two adjacent painted panels with
/// no divider read as one panel whose left third is mysteriously dim.
pub const FILE_TREE_SEPARATOR: u16 = 1;

/// The narrowest patch area worth keeping the tree for.
///
/// Six columns go to the unified gutter (`nnnn` plus sign plus space), leaving 34 for
/// code. Below that the patch — the thing the viewer exists to show — is narrower than
/// the tree beside it, and the tree is the part that can be given up without loss: the
/// file title row above the patch already names the current file, so dropping the tree
/// costs navigation, while dropping patch columns costs the content.
pub const PATCH_MIN_WIDTH: u16 = 40;

/// The total width at or above which the tree can be drawn at all.
pub const FILE_TREE_MIN_TOTAL: u16 = FILE_TREE_WIDTH + FILE_TREE_SEPARATOR + PATCH_MIN_WIDTH;

// ---------------------------------------------------------------------------
// Ceilings
// ---------------------------------------------------------------------------

/// Bytes of patch beyond which the tree and word refinement are abandoned.
///
/// 512 KiB, the same figure the markdown ceiling uses, and for the same reason: it is
/// far above any patch a human reviews and far below the size at which per-line work
/// becomes visible as latency.
pub const MAX_PATCH_BYTES: usize = 512 * 1024;

/// Lines of patch rendered before the tail is dropped with a notice.
pub const MAX_PATCH_LINES: usize = 10_000;

/// The largest token grid one line pair may be refined across.
///
/// The refinement is Myers over token slices, so its worst case is the product of the
/// two token counts. 4 096 admits a 64-token line against another 64-token line, which
/// covers ordinary source and prose; beyond it the pair renders unrefined, which is the
/// pre-existing rendering rather than an error. Bounding *tokens* and not *bytes* is
/// the point: the token counts are known before the diff runs — see [`refine`] — so
/// this is a decision made in O(n), never a diff abandoned halfway.
pub const MAX_WORD_DIFF_CELLS: usize = 4_096;

/// Line pairs refined per patch, after which refinement stops.
///
/// With [`MAX_WORD_DIFF_CELLS`] this bounds the whole patch's refinement at
/// 200 × 4 096 = 819 200 token comparisons, which is the number that matters: it is
/// paid once per rendered frame, and it is two orders of magnitude below the point at
/// which a frame misses its deadline.
pub const MAX_WORD_DIFF_PAIRS: usize = 200;

/// How the available columns are divided between tree and patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WidthPlan {
    /// The tree's columns, or `None` when it is not drawn.
    pub tree: Option<u16>,
    /// The patch's columns. Never zero unless the whole frame is.
    pub patch: u16,
}

/// Divide `total` columns between the tree and the patch.
///
/// The tree is dropped — silently, and recoverably with `b` — whenever it would leave
/// the patch below [`PATCH_MIN_WIDTH`]. This is one policy, not two: the same call
/// answers "is there room" and "how much does each get", so the two halves cannot
/// disagree about where the boundary is.
#[must_use]
pub const fn plan_widths(total: u16, tree_open: bool) -> WidthPlan {
    if !tree_open || total < FILE_TREE_MIN_TOTAL {
        return WidthPlan {
            tree: None,
            patch: total,
        };
    }
    WidthPlan {
        tree: Some(FILE_TREE_WIDTH),
        patch: total - FILE_TREE_WIDTH - FILE_TREE_SEPARATOR,
    }
}

// ---------------------------------------------------------------------------
// Splitting a patch into files
// ---------------------------------------------------------------------------

/// One file's slice of a multi-file patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// The path, as the patch spells it, with any `a/` or `b/` prefix removed.
    pub path: String,
    /// This file's patch text, headers included.
    pub patch: String,
    /// Added lines.
    pub additions: usize,
    /// Removed lines.
    pub deletions: usize,
}

/// Split a unified patch into one entry per file.
///
/// `diff --git` starts a file; so does a bare `--- ` for a patch produced without git
/// headers, which is what [`zuno_tools::diff`] emits. Both forms reach the viewer —
/// `apply_patch` produces the former over several files at once, single-file tools the
/// latter — so recognising only one would collapse a real multi-file patch into a
/// single unnamed entry.
///
/// A patch with no recognisable file header yields one entry whose path is empty. That
/// is deliberate: the alternative is returning nothing and rendering a viewer with no
/// content for a patch that plainly has some.
#[must_use]
pub fn split_files(patch: &str) -> Vec<FilePatch> {
    let mut files: Vec<FilePatch> = Vec::new();
    for raw in patch.lines() {
        let starts_file = raw.starts_with("diff --git ")
            || (raw.starts_with("--- ")
                && files
                    .last()
                    .is_none_or(|file| file.patch.contains("\n+++ ") || file.path.is_empty()));
        if starts_file || files.is_empty() {
            files.push(FilePatch {
                path: header_path(raw).unwrap_or_default(),
                patch: String::new(),
                additions: 0,
                deletions: 0,
            });
        }
        let file = files
            .last_mut()
            .expect("the branch above guarantees an entry");
        if file.path.is_empty()
            && let Some(path) = header_path(raw)
        {
            file.path = path;
        }
        // Counted only for non-header lines, which is why the header test comes first:
        // a `+++ b/x` line begins with `+` and would otherwise be tallied as an
        // addition, inflating every single-file patch by one.
        if !is_file_header(raw) {
            if raw.starts_with('+') {
                file.additions += 1;
            } else if raw.starts_with('-') {
                file.deletions += 1;
            }
        }
        file.patch.push_str(raw);
        file.patch.push('\n');
    }
    files
}

/// Whether `raw` is a file-level header rather than a hunk header or content.
fn is_file_header(raw: &str) -> bool {
    [
        "diff ",
        "index ",
        "--- ",
        "+++ ",
        "new file",
        "deleted file",
    ]
    .iter()
    .any(|prefix| raw.starts_with(prefix))
}

/// `patch` without its file-level headers.
///
/// The browser prints a title row naming the path and its `+N -M`, so keeping the raw
/// `diff --git a/x b/x` beneath it spends a row — up to four, with `index` and the
/// `---`/`+++` pair — restating what the row above just said, and puts the same path on
/// screen twice for every file in the patch. Hunk headers stay: they carry the line
/// numbers and they are what `[`/`]` navigate between.
///
/// Done by filtering the text rather than by teaching [`crate::views::diff::DiffView`] to
/// skip them, because the permission prompt renders the same patches through that view
/// with *no* title row above them, where the headers are the only thing naming the file.
#[must_use]
pub fn body_patch(patch: &str) -> String {
    patch
        .lines()
        .filter(|raw| !is_file_header(raw))
        .fold(String::new(), |mut out, raw| {
            out.push_str(raw);
            out.push('\n');
            out
        })
}

/// The path a header line names, if it names one.
fn header_path(raw: &str) -> Option<String> {
    if let Some(rest) = raw.strip_prefix("diff --git ") {
        // `a/x b/x`: the second half, because a rename's new name is the one to show.
        let mut halves = rest.split_whitespace();
        let first = halves.next()?;
        let second = halves.next().unwrap_or(first);
        return Some(strip_prefix(second));
    }
    if let Some(rest) = raw.strip_prefix("+++ ") {
        let path = strip_prefix(rest.split('\t').next().unwrap_or(rest));
        return (path != "/dev/null").then_some(path);
    }
    if let Some(rest) = raw.strip_prefix("--- ") {
        let path = strip_prefix(rest.split('\t').next().unwrap_or(rest));
        return (path != "/dev/null").then_some(path);
    }
    None
}

/// Drop a `a/` or `b/` patch prefix, keeping a real leading directory of that name.
fn strip_prefix(path: &str) -> String {
    for prefix in ["a/", "b/"] {
        if let Some(rest) = path.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return rest.to_owned();
        }
    }
    path.to_owned()
}

// ---------------------------------------------------------------------------
// The tree
// ---------------------------------------------------------------------------

/// One row of the rendered file tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// The tree-line prefix, already drawn (`│  `, `├─ `, `└─ `).
    pub prefix: String,
    /// The segment shown, a compressed directory chain or a file name.
    pub label: String,
    /// Which file this row selects, or `None` for a directory row.
    pub file: Option<usize>,
}

/// A node while the trie is being built.
#[derive(Default)]
struct Node {
    children: std::collections::BTreeMap<String, Node>,
    file: Option<usize>,
}

/// Arrange `files` as a fully expanded tree.
///
/// Single-child directory chains are compressed onto one row (`a/b/c`), which is the
/// difference between a tree that shows structure and one that shows three rows of
/// nothing for every Rust crate layout. Directories sort before files at each level,
/// then lexicographically — so the shape is stable across renders and across patches
/// that list their files in tool order.
#[must_use]
pub fn tree_rows(files: &[FilePatch]) -> Vec<TreeRow> {
    let mut root = Node::default();
    for (index, file) in files.iter().enumerate() {
        let mut node = &mut root;
        let segments = file.path.split('/').filter(|segment| !segment.is_empty());
        let segments = segments.collect::<Vec<_>>();
        if segments.is_empty() {
            // A patch with no path still needs a selectable row, or the file is
            // reachable by `n`/`p` but invisible in the tree.
            node.children
                .entry(String::from("(unnamed)"))
                .or_default()
                .file = Some(index);
            continue;
        }
        for segment in &segments {
            node = node.children.entry((*segment).to_owned()).or_default();
        }
        node.file = Some(index);
    }
    let mut rows = Vec::new();
    walk(&root, &mut Vec::new(), &mut rows);
    rows
}

/// Emit `node`'s children, `ancestors` recording whether each level continues.
fn walk(node: &Node, ancestors: &mut Vec<bool>, rows: &mut Vec<TreeRow>) {
    let mut entries = node.children.iter().collect::<Vec<_>>();
    // Directories first, then lexicographic. `is_none()` on the file slot is what
    // "directory" means here — a path can be both, and a node carrying a file with
    // children is sorted as a file because that is what its row selects.
    entries.sort_by(|(left_name, left), (right_name, right)| {
        let left_is_dir = left.file.is_none();
        let right_is_dir = right.file.is_none();
        right_is_dir
            .cmp(&left_is_dir)
            .then_with(|| left_name.cmp(right_name))
    });
    let last_index = entries.len().saturating_sub(1);
    for (position, (name, child)) in entries.into_iter().enumerate() {
        let is_last = position == last_index;
        // Compress a single-child directory chain: `a` → `a/b/c`.
        let mut label = name.clone();
        let mut node = child;
        while node.file.is_none() && node.children.len() == 1 {
            let (name, only) = node
                .children
                .iter()
                .next()
                .expect("the loop condition proved exactly one child");
            label.push('/');
            label.push_str(name);
            node = only;
        }
        rows.push(TreeRow {
            prefix: prefix_for(ancestors, is_last),
            label,
            file: node.file,
        });
        if !node.children.is_empty() {
            ancestors.push(!is_last);
            walk(node, ancestors, rows);
            ancestors.pop();
        }
    }
}

/// The tree-line prefix for a row whose ancestors continue as `ancestors` says.
fn prefix_for(ancestors: &[bool], is_last: bool) -> String {
    let mut prefix = String::new();
    for continues in ancestors {
        prefix.push_str(if *continues { "│  " } else { "   " });
    }
    prefix.push_str(if is_last { "└─ " } else { "├─ " });
    prefix
}

// ---------------------------------------------------------------------------
// Hunks
// ---------------------------------------------------------------------------

/// The indices in `lines` that are `@@` hunk headers.
///
/// `§7.4`: scanning for the prefix is enough, no AST. Only `@@` counts and not the
/// whole [`LineKind::Header`] family — `diff --git`, `index`, `+++` and `---` are all
/// headers too, and a `]` that stopped on each of them would take four presses to
/// cross one file's preamble.
#[must_use]
pub fn hunk_indices(lines: &[DiffLine]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.kind == LineKind::Header && line.text.starts_with("@@"))
        .map(|(index, _)| index)
        .collect()
}

// ---------------------------------------------------------------------------
// Word-level refinement
// ---------------------------------------------------------------------------

/// A run of a refined line, flagged with whether it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSpan {
    /// The run's text.
    pub text: String,
    /// Whether this run is part of what changed.
    pub changed: bool,
}

/// Refine a removed/added line pair down to the runs that differ.
///
/// **A capability this project adds.** The reference implementation cannot be shown to
/// have it — `§7.4` records the search — so this is not parity work and is not described
/// as such.
///
/// Returns `None` when the pair is not worth refining, and every caller must treat that
/// as "render the line as it was": either the two lines share nothing (a whole-line
/// rewrite, where marking every word changed says less than saying nothing) or the
/// token grid exceeds [`MAX_WORD_DIFF_CELLS`].
///
/// Tokenising here rather than through `TextDiff::from_words` is what makes the ceiling
/// enforceable *before* any comparison: `from_words` hides its tokenisation inside the
/// diff, so the only bound available through it would be over bytes, which overstates
/// the grid by the average token length and would therefore reject lines that are
/// cheap while admitting some that are not.
#[must_use]
pub fn refine(old: &str, new: &str) -> Option<(Vec<WordSpan>, Vec<WordSpan>)> {
    if old == new || old.is_empty() || new.is_empty() {
        return None;
    }
    let old_tokens = old.split_word_bounds().collect::<Vec<_>>();
    let new_tokens = new.split_word_bounds().collect::<Vec<_>>();
    if old_tokens.len().saturating_mul(new_tokens.len()) > MAX_WORD_DIFF_CELLS {
        return None;
    }
    let diff = similar::TextDiff::from_slices(&old_tokens, &new_tokens);
    let mut removed: Vec<WordSpan> = Vec::new();
    let mut added: Vec<WordSpan> = Vec::new();
    let mut common = 0usize;
    for change in diff.iter_all_changes() {
        let text = change.value();
        match change.tag() {
            similar::ChangeTag::Equal => {
                common += 1;
                push_span(&mut removed, text, false);
                push_span(&mut added, text, false);
            }
            similar::ChangeTag::Delete => push_span(&mut removed, text, true),
            similar::ChangeTag::Insert => push_span(&mut added, text, true),
        }
    }
    // No shared token means the two lines are unrelated: highlighting the whole of both
    // is exactly the unrefined rendering, drawn more expensively and with a background
    // that now implies a distinction it is not making.
    (common > 0).then_some((removed, added))
}

/// Append `text` to `spans`, merging into the tail when the flag matches.
fn push_span(spans: &mut Vec<WordSpan>, text: &str, changed: bool) {
    if let Some(last) = spans.last_mut()
        && last.changed == changed
    {
        last.text.push_str(text);
        return;
    }
    spans.push(WordSpan {
        text: text.to_owned(),
        changed,
    });
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Truncate `spans` to `width` columns and pad the remainder.
///
/// Measured in terminal columns throughout. Truncating by character count is the defect
/// `§10.2` names in the reference implementation: a row of CJK cut to its character
/// count is twice as wide as its cell, and every column to its right is displaced.
#[must_use]
pub fn fitted_spans(
    spans: &[WordSpan],
    width: usize,
    plain: Style,
    highlight: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= width {
            break;
        }
        let room = width - used;
        let text = if display_width(&span.text) > room {
            truncate(&span.text, room)
        } else {
            span.text.clone()
        };
        if text.is_empty() {
            continue;
        }
        used += display_width(&text);
        out.push(Span::styled(
            text,
            if span.changed { highlight } else { plain },
        ));
    }
    if used < width {
        out.push(Span::styled(" ".repeat(width - used), plain));
    }
    out
}

/// One rendered tree row, at `width` columns.
///
/// The two trailing marker columns of `§7.4` carry the review mark and the file's
/// status letter. They are fixed-width and painted last so the labels above them line
/// up regardless of how deep the tree got.
#[must_use]
pub fn tree_line(
    context: &ViewContext,
    row: &TreeRow,
    file: Option<&FilePatch>,
    selected: bool,
    width: u16,
) -> Line<'static> {
    let palette = context.palette();
    let style = if selected {
        // `§11.5`: the selected row everywhere in this TUI is `primary` behind
        // `background`, which is what keeps the diff tree and the pickers agreeing.
        Style::new()
            .fg(palette.background.into())
            .bg(palette.primary.into())
    } else if row.file.is_some() {
        Style::new()
            .fg(palette.text.into())
            .bg(palette.background_panel.into())
    } else {
        Style::new()
            .fg(palette.text_muted.into())
            .bg(palette.background_panel.into())
    };
    let total = usize::from(width);
    let marker = file.map_or(' ', status_letter);
    // Two columns for the marker plus one leading space; below that there is no room to
    // spend on either, so the whole width goes to the label.
    let reserved = if total > 4 { 2 } else { 0 };
    let room = total.saturating_sub(reserved);
    let head = format!("{}{}", row.prefix, row.label);
    let head = if display_width(&head) > room {
        truncate(&head, room)
    } else {
        head
    };
    let pad = room.saturating_sub(display_width(&head));
    let mut spans = vec![Span::styled(format!("{head}{}", " ".repeat(pad)), style)];
    if reserved > 0 {
        spans.push(Span::styled(format!("{marker} "), style));
    }
    Line::from(spans)
}

/// The status letter `§7.4` puts in the tree's right-hand column.
fn status_letter(file: &FilePatch) -> char {
    match (file.additions, file.deletions) {
        (0, 0) => '?',
        (_, 0) => 'A',
        (0, _) => 'D',
        _ => 'M',
    }
}

/// Which layout a patch area `width` columns wide should use.
///
/// A thin wrapper so the browser and its tests name the decision the same way, and so
/// the fact that this is [`ViewContext::diff_columns`] and not a second rule is visible
/// at the call site.
#[must_use]
pub fn columns_for(context: &ViewContext, width: u16) -> DiffColumns {
    context.diff_columns(width)
}

/// A multi-file patch, its tree, and where the reader is in it.
///
/// Owns no scroll *policy* — the offset is set from outside, by the dialog that hosts
/// it — but it does own the mapping from "next hunk" to a row number, because that
/// mapping only exists once the rows have been composed at a known width.
pub struct DiffBrowser {
    context: ViewContext,
    files: Vec<FilePatch>,
    tree: Vec<TreeRow>,
    selected: usize,
    tree_open: bool,
    /// Show only the current file rather than every file in the patch.
    single: bool,
    forced: Option<DiffColumns>,
    columns: DiffColumns,
    /// Row numbers of the `@@` headers in the last composed body.
    hunks: Vec<usize>,
    rows: usize,
    /// [`MAX_PATCH_BYTES`] was exceeded: no tree, no refinement.
    plain: bool,
    /// [`MAX_PATCH_LINES`] was exceeded: the tail was dropped.
    truncated: bool,
}

impl DiffBrowser {
    /// Split `patch` into files and prepare a tree over them.
    #[must_use]
    pub fn new(context: ViewContext, patch: &str) -> Self {
        let plain = patch.len() > MAX_PATCH_BYTES;
        let files = split_files(patch);
        // The tree is the superlinear half, so it is what the byte ceiling gives up.
        let tree = if plain { Vec::new() } else { tree_rows(&files) };
        Self {
            context,
            files,
            tree,
            selected: 0,
            tree_open: !plain,
            single: false,
            forced: None,
            columns: DiffColumns::Unified,
            hunks: Vec::new(),
            rows: 0,
            plain,
            truncated: false,
        }
    }

    #[must_use]
    pub fn files(&self) -> &[FilePatch] {
        &self.files
    }

    #[must_use]
    pub fn tree(&self) -> &[TreeRow] {
        &self.tree
    }

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub const fn is_plain(&self) -> bool {
        self.plain
    }

    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    #[must_use]
    pub const fn columns(&self) -> DiffColumns {
        self.columns
    }

    /// Whether the tree is currently wanted. It may still be dropped for want of room.
    #[must_use]
    pub const fn tree_open(&self) -> bool {
        self.tree_open
    }

    #[must_use]
    pub fn hunks(&self) -> &[usize] {
        &self.hunks
    }

    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    pub const fn toggle_tree(&mut self) {
        self.tree_open = !self.tree_open;
    }

    pub const fn toggle_single(&mut self) {
        self.single = !self.single;
    }

    pub const fn toggle_columns(&mut self) {
        self.forced = Some(match self.columns {
            DiffColumns::Unified => DiffColumns::Split,
            DiffColumns::Split => DiffColumns::Unified,
        });
    }

    /// Select the next file, saturating at the last.
    ///
    /// Saturating and not wrapping: `n` held down at the end of a patch should stop
    /// there rather than silently return the reader to the top, which reads as the
    /// key having done nothing.
    pub fn next_file(&mut self) -> bool {
        let last = self.files.len().saturating_sub(1);
        if self.selected >= last {
            return false;
        }
        self.selected += 1;
        true
    }

    /// Select the previous file, saturating at the first.
    pub fn previous_file(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        self.selected -= 1;
        true
    }

    /// The row to scroll to for the first hunk strictly after `offset`.
    #[must_use]
    pub fn next_hunk(&self, offset: usize) -> Option<usize> {
        self.hunks.iter().copied().find(|row| *row > offset)
    }

    /// The row to scroll to for the last hunk strictly before `offset`.
    #[must_use]
    pub fn previous_hunk(&self, offset: usize) -> Option<usize> {
        self.hunks.iter().copied().rev().find(|row| *row < offset)
    }

    /// Compose the body rows for a patch area `width` columns wide.
    ///
    /// Records the hunk row numbers as a side effect, which is why hunk navigation is a
    /// query against the *last rendered* width rather than against the parse: in a split
    /// layout several parsed lines collapse into one row, so a hunk's row number is not
    /// knowable until the width is.
    fn body(&mut self, width: u16) -> Vec<Line<'static>> {
        self.columns = self
            .forced
            .unwrap_or_else(|| columns_for(&self.context, width));
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut hunks = Vec::new();
        let mut budget = if self.plain { 0 } else { MAX_WORD_DIFF_PAIRS };
        self.truncated = false;
        let indices: Vec<usize> = if self.single {
            vec![self.selected]
        } else {
            (0..self.files.len()).collect()
        };
        for index in indices {
            let Some(file) = self.files.get(index) else {
                continue;
            };
            if rows.len() >= MAX_PATCH_LINES {
                self.truncated = true;
                break;
            }
            rows.push(self.title_row(file, index == self.selected, width));
            let mut view =
                crate::views::diff::DiffView::new(self.context.clone(), &body_patch(&file.patch));
            view.set_columns(self.columns);
            view.set_refine_budget(budget);
            let (body, file_hunks) = view.rows_with_hunks(width);
            budget = view.refine_budget();
            let base = rows.len();
            hunks.extend(file_hunks.iter().map(|row| base + row));
            let room = MAX_PATCH_LINES.saturating_sub(rows.len());
            if body.len() > room {
                self.truncated = true;
                rows.extend(body.into_iter().take(room));
                break;
            }
            rows.extend(body);
        }
        if self.truncated {
            rows.push(crate::views::padded(
                &format!("  … patch truncated at {MAX_PATCH_LINES} rows"),
                width,
                self.context.muted(),
            ));
        }
        self.hunks = hunks;
        self.rows = rows.len();
        rows
    }

    /// The `path … +N -M` row above one file's patch.
    fn title_row(&self, file: &FilePatch, current: bool, width: u16) -> Line<'static> {
        let palette = self.context.palette();
        let style = if current {
            Style::new()
                .fg(palette.text.into())
                .bg(palette.background_element.into())
        } else {
            self.context.muted()
        };
        let total = usize::from(width);
        let counts = format!("+{} -{}", file.additions, file.deletions);
        let counts_width = display_width(&counts);
        let path = if file.path.is_empty() {
            String::from("(unnamed)")
        } else {
            file.path.clone()
        };
        // Below the width the counts alone need, the path wins: it is the only part
        // that says which file this is.
        if total <= counts_width + 2 {
            return crate::views::padded(&path, width, style);
        }
        let room = total - counts_width - 2;
        let shown = if display_width(&path) > room {
            truncate(&path, room)
        } else {
            path
        };
        let pad = room.saturating_sub(display_width(&shown));
        Line::from(vec![
            Span::styled(format!(" {shown}{}", " ".repeat(pad)), style),
            Span::styled(
                format!("{counts} "),
                if file.deletions > file.additions {
                    Style::new()
                        .fg(palette.diff_removed.into())
                        .bg(palette.background_element.into())
                } else {
                    Style::new()
                        .fg(palette.diff_added.into())
                        .bg(palette.background_element.into())
                },
            ),
        ])
    }

    /// The rows to draw at `width`, tree and patch side by side.
    ///
    /// Windowing is the caller's, so this returns every row; the offset is applied by
    /// whoever owns the viewport. See [`crate::views::diff::DiffDialog`].
    pub fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let plan = plan_widths(width, self.tree_open && !self.plain);
        let body = self.body(plan.patch);
        let Some(tree_width) = plan.tree else {
            return body;
        };
        let rule = Span::styled(
            String::from(ratatui::symbols::line::VERTICAL),
            Style::new()
                .fg(self.context.palette().border_subtle.into())
                .bg(self.context.palette().background_panel.into()),
        );
        let blank = TreeRow {
            prefix: String::new(),
            label: String::new(),
            file: None,
        };
        (0..body.len().max(self.tree.len()))
            .map(|row| {
                let entry = self.tree.get(row).unwrap_or(&blank);
                let selected = entry
                    .file
                    .is_some_and(|index| index == self.selected && self.tree.get(row).is_some());
                let file = entry.file.and_then(|index| self.files.get(index));
                let mut spans = tree_line(&self.context, entry, file, selected, tree_width).spans;
                spans.push(rule.clone());
                match body.get(row) {
                    Some(line) => spans.extend(line.spans.iter().cloned()),
                    None => spans.push(Span::styled(
                        " ".repeat(usize::from(plan.patch)),
                        self.context.surface(),
                    )),
                }
                Line::from(spans)
            })
            .collect()
    }
}
