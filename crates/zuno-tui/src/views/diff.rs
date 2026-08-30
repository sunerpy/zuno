//! Unified-diff rendering, in one or two columns depending on `diff_style`.
//!
//! # The `diff_style` fork is the whole contract
//!
//! `packages/tui/src/routes/session/permission.tsx:38-42`:
//!
//! ```text
//! const diffStyle = config.diff_style
//! if (diffStyle === "stacked") return "unified"
//! return dimensions().width > 120 ? "split" : "unified"
//! ```
//!
//! So `stacked` is an override that always wins, and `auto` — the absent value —
//! is width-driven. That fork lives in [`crate::views::ViewContext::diff_columns`]
//! so the permission prompt and a standalone diff viewer cannot disagree about it.
//!
//! # Colours come in eleven flavours, and that is not decoration
//!
//! The palette carries `diffAdded`, `diffRemoved`, `diffContext`,
//! `diffHunkHeader`, `diffHighlightAdded`, `diffHighlightRemoved`, three
//! background variants and three line-number variants
//! (`packages/tui/src/theme/index.ts`). A diff that reused `success`/`error` would
//! be unreadable in the several shipped themes that deliberately pick low-contrast
//! diff backgrounds distinct from their status colours, so every one of those keys
//! is used here.
//!
//! # Parsing is tolerant on purpose
//!
//! A patch arrives as a string in a permission request's metadata, produced by
//! whatever tool asked. A line this parser does not recognise is rendered as
//! context rather than dropped: showing a user slightly mis-classified text is
//! recoverable, hiding a line of a patch they are about to approve is not.

use crate::app::{AppEvent, Component, EventResult};
use crate::keybind::Definition;
use crate::views::diff_browser::{
    DiffBrowser, MAX_WORD_DIFF_PAIRS, WordSpan, fitted_spans, hunk_indices, refine,
};
use crate::views::{DiffColumns, ViewContext, fill, padded};
use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

#[cfg(test)]
#[path = "diff_tests.rs"]
mod tests;

/// What one line of a patch is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// `@@ … @@`, or a `diff --git`/`+++`/`---` header.
    Header,
    /// A line present in both sides.
    Context,
    /// A line only in the new file.
    Added,
    /// A line only in the old file.
    Removed,
}

/// One parsed patch line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// Its classification.
    pub kind: LineKind,
    /// Its text, with the leading marker removed.
    pub text: String,
    /// Line number in the old file, when it has one.
    pub old: Option<usize>,
    /// Line number in the new file, when it has one.
    pub new: Option<usize>,
}

/// Parse a unified diff.
///
/// Hunk headers reset the line counters, so the numbers shown are the file's own
/// rather than an offset into the patch.
#[must_use]
pub fn parse(patch: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut old = 0usize;
    let mut new = 0usize;
    for raw in patch.lines() {
        if raw.starts_with("@@") {
            let (start_old, start_new) = hunk_starts(raw);
            old = start_old;
            new = start_new;
            lines.push(DiffLine {
                kind: LineKind::Header,
                text: raw.to_owned(),
                old: None,
                new: None,
            });
            continue;
        }
        if raw.starts_with("diff ")
            || raw.starts_with("index ")
            || raw.starts_with("--- ")
            || raw.starts_with("+++ ")
            || raw.starts_with("new file")
            || raw.starts_with("deleted file")
        {
            lines.push(DiffLine {
                kind: LineKind::Header,
                text: raw.to_owned(),
                old: None,
                new: None,
            });
            continue;
        }
        let (kind, text) = match raw.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &raw[1..]),
            Some(b'-') => (LineKind::Removed, &raw[1..]),
            Some(b' ') => (LineKind::Context, &raw[1..]),
            // An unmarked line inside a patch body: treat it as context. See the
            // module docs on why dropping it is the worse choice.
            _ => (LineKind::Context, raw),
        };
        let (line_old, line_new) = match kind {
            LineKind::Added => {
                new += 1;
                (None, Some(new))
            }
            LineKind::Removed => {
                old += 1;
                (Some(old), None)
            }
            _ => {
                old += 1;
                new += 1;
                (Some(old), Some(new))
            }
        };
        lines.push(DiffLine {
            kind,
            text: text.to_owned(),
            old: line_old,
            new: line_new,
        });
    }
    lines
}

fn hunk_starts(header: &str) -> (usize, usize) {
    let mut old = 0;
    let mut new = 0;
    for token in header.split_whitespace() {
        let number = |text: &str| {
            text.split(',')
                .next()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1)
        };
        if let Some(rest) = token.strip_prefix('-') {
            old = number(rest).saturating_sub(1);
        } else if let Some(rest) = token.strip_prefix('+') {
            new = number(rest).saturating_sub(1);
        }
    }
    (old, new)
}

/// A rendered diff.
pub struct DiffView {
    context: ViewContext,
    lines: Vec<DiffLine>,
    /// An explicit column override, for a caller that already decided.
    forced: Option<DiffColumns>,
    /// First rendered row.
    offset: usize,
    columns: DiffColumns,
    /// Line pairs this view may still refine to words.
    ///
    /// A budget rather than a flag because it is shared across the files of one patch:
    /// a browser hands each file what the previous files left, so the ceiling bounds the
    /// whole frame instead of bounding each file and multiplying by the file count.
    refine_budget: usize,
}

impl DiffView {
    /// Parse `patch` into a view.
    #[must_use]
    pub fn new(context: ViewContext, patch: &str) -> Self {
        Self {
            context,
            lines: parse(patch),
            forced: None,
            offset: 0,
            columns: DiffColumns::Unified,
            refine_budget: MAX_WORD_DIFF_PAIRS,
        }
    }

    pub const fn set_refine_budget(&mut self, pairs: usize) {
        self.refine_budget = pairs;
    }

    #[must_use]
    pub const fn refine_budget(&self) -> usize {
        self.refine_budget
    }

    /// Force a layout, ignoring width and configuration.
    #[must_use]
    pub const fn with_columns(mut self, columns: DiffColumns) -> Self {
        self.forced = Some(columns);
        self
    }

    /// The parsed lines.
    #[must_use]
    pub fn parsed(&self) -> &[DiffLine] {
        &self.lines
    }

    /// The layout the last call to [`Self::lines`] used.
    #[must_use]
    pub const fn columns(&self) -> DiffColumns {
        self.columns
    }

    /// Scroll to `offset`.
    pub const fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
    }

    /// Force a layout after construction, for a caller that toggles it.
    pub const fn set_columns(&mut self, columns: DiffColumns) {
        self.forced = Some(columns);
    }

    fn style(&self, kind: LineKind) -> Style {
        let palette = self.context.palette();
        match kind {
            LineKind::Header => Style::new()
                .fg(palette.diff_hunk_header.into())
                .bg(palette.background_panel.into()),
            LineKind::Context => Style::new()
                .fg(palette.diff_context.into())
                .bg(palette.diff_context_bg.into()),
            LineKind::Added => Style::new()
                .fg(palette.diff_added.into())
                .bg(palette.diff_added_bg.into()),
            LineKind::Removed => Style::new()
                .fg(palette.diff_removed.into())
                .bg(palette.diff_removed_bg.into()),
        }
    }

    fn number_style(&self, kind: LineKind) -> Style {
        let palette = self.context.palette();
        let background = match kind {
            LineKind::Added => palette.diff_added_line_number_bg,
            LineKind::Removed => palette.diff_removed_line_number_bg,
            _ => palette.diff_context_bg,
        };
        Style::new()
            .fg(palette.diff_line_number.into())
            .bg(background.into())
    }

    fn sign_style(&self, kind: LineKind) -> Style {
        let palette = self.context.palette();
        match kind {
            LineKind::Added => Style::new()
                .fg(palette.diff_highlight_added.into())
                .bg(palette.diff_added_bg.into()),
            LineKind::Removed => Style::new()
                .fg(palette.diff_highlight_removed.into())
                .bg(palette.diff_removed_bg.into()),
            _ => self.style(kind),
        }
    }

    /// The rows this diff renders at `width`, honouring `diff_style`.
    pub fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        self.rows_with_hunks(width).0
    }

    /// The rows, paired with the row numbers of the `@@` headers among them.
    ///
    /// Two returns rather than a second pass because in a split layout a hunk's row
    /// number is not derivable from the parse: several parsed lines collapse into one
    /// row, so only the code that emitted the rows knows where the headers landed.
    pub fn rows_with_hunks(&mut self, width: u16) -> (Vec<Line<'static>>, Vec<usize>) {
        self.columns = self
            .forced
            .unwrap_or_else(|| self.context.diff_columns(width));
        match self.columns {
            DiffColumns::Unified => self.unified(width),
            DiffColumns::Split => self.split(width),
        }
    }

    /// The removal run at `index`, the addition run following it, and the index past both.
    ///
    /// Cloned rather than borrowed so the caller can then take `&mut self` to spend the
    /// refinement budget. Either the removals or the additions is non-empty whenever
    /// `index` names a changed line, which is what guarantees the walk advances.
    fn runs(&self, index: usize) -> (Vec<DiffLine>, Vec<DiffLine>, usize) {
        let removals = self.lines[index..]
            .iter()
            .take_while(|line| line.kind == LineKind::Removed)
            .cloned()
            .collect::<Vec<_>>();
        let after = index + removals.len();
        let additions = self.lines[after..]
            .iter()
            .take_while(|line| line.kind == LineKind::Added)
            .cloned()
            .collect::<Vec<_>>();
        let end = after + additions.len();
        (removals, additions, end)
    }

    fn refine_runs(
        &mut self,
        removals: &[DiffLine],
        additions: &[DiffLine],
    ) -> Vec<Option<(Vec<WordSpan>, Vec<WordSpan>)>> {
        (0..removals.len().min(additions.len()))
            .map(|offset| {
                if self.refine_budget == 0 {
                    return None;
                }
                let pair = refine(&removals[offset].text, &additions[offset].text);
                if pair.is_some() {
                    self.refine_budget -= 1;
                }
                pair
            })
            .collect()
    }

    fn text_spans(
        &self,
        line: &DiffLine,
        spans: Option<&[WordSpan]>,
        width: usize,
    ) -> Vec<Span<'static>> {
        let base = self.style(line.kind);
        match spans {
            // The changed runs take `diffHighlightAdded`/`diffHighlightRemoved`, which is
            // what those two palette keys are for and what makes the refinement legible
            // in the themes that pick a deliberately low-contrast diff background.
            Some(spans) => fitted_spans(spans, width, base, self.sign_style(line.kind)),
            None => fitted_spans(
                &[WordSpan {
                    text: line.text.clone(),
                    changed: false,
                }],
                width,
                base,
                base,
            ),
        }
    }

    fn unified(&mut self, width: u16) -> (Vec<Line<'static>>, Vec<usize>) {
        // Rows are one-to-one with parsed lines in this layout — a run is emitted as all
        // its removals then all its additions, which is the order they were parsed in —
        // so the parse's own hunk positions are the row positions.
        let hunks = hunk_indices(&self.lines);
        let body_width = usize::from(width).saturating_sub(6);
        let mut rows = Vec::new();
        let mut index = 0;
        while index < self.lines.len() {
            let line = self.lines[index].clone();
            if line.kind == LineKind::Header {
                rows.push(padded(&line.text, width, self.style(LineKind::Header)));
                index += 1;
                continue;
            }
            if line.kind == LineKind::Context {
                rows.push(self.unified_row(&line, None, body_width));
                index += 1;
                continue;
            }
            let (removals, additions, end) = self.runs(index);
            let refined = self.refine_runs(&removals, &additions);
            for (offset, line) in removals.iter().enumerate() {
                let spans = refined
                    .get(offset)
                    .and_then(|pair| pair.as_ref().map(|(old, _)| old.as_slice()));
                rows.push(self.unified_row(line, spans, body_width));
            }
            for (offset, line) in additions.iter().enumerate() {
                let spans = refined
                    .get(offset)
                    .and_then(|pair| pair.as_ref().map(|(_, new)| new.as_slice()));
                rows.push(self.unified_row(line, spans, body_width));
            }
            index = end;
        }
        (rows, hunks)
    }

    fn unified_row(
        &self,
        line: &DiffLine,
        spans: Option<&[WordSpan]>,
        body_width: usize,
    ) -> Line<'static> {
        let sign = match line.kind {
            LineKind::Added => '+',
            LineKind::Removed => '-',
            _ => ' ',
        };
        let number = line
            .new
            .or(line.old)
            .map_or_else(|| String::from("    "), |value| format!("{value:>4}"));
        let mut out = vec![
            Span::styled(number, self.number_style(line.kind)),
            Span::styled(sign.to_string(), self.sign_style(line.kind)),
            Span::styled(String::from(" "), self.style(line.kind)),
        ];
        out.extend(self.text_spans(line, spans, body_width));
        Line::from(out)
    }

    fn split(&mut self, width: u16) -> (Vec<Line<'static>>, Vec<usize>) {
        let half = usize::from(width / 2).max(1);
        let cell = half.saturating_sub(5).max(1);
        let mut rows = Vec::new();
        let mut hunks = Vec::new();
        let mut index = 0;
        while index < self.lines.len() {
            let line = self.lines[index].clone();
            if line.kind == LineKind::Header {
                if line.text.starts_with("@@") {
                    hunks.push(rows.len());
                }
                rows.push(padded(&line.text, width, self.style(LineKind::Header)));
                index += 1;
                continue;
            }
            if line.kind == LineKind::Context {
                let mut spans = vec![Span::styled(
                    format!("{:>4} ", line.old.unwrap_or_default()),
                    self.number_style(LineKind::Context),
                )];
                spans.extend(self.text_spans(&line, None, cell));
                spans.push(Span::styled(
                    format!("{:>4} ", line.new.unwrap_or_default()),
                    self.number_style(LineKind::Context),
                ));
                spans.extend(self.text_spans(&line, None, cell));
                rows.push(Line::from(spans));
                index += 1;
                continue;
            }
            // Pair a removal run with the addition run that follows it, so the two
            // sides line up the way a reviewer reads them.
            let (removals, additions, end) = self.runs(index);
            let refined = self.refine_runs(&removals, &additions);
            for offset in 0..removals.len().max(additions.len()) {
                let mut spans = Vec::new();
                match removals.get(offset) {
                    Some(line) => {
                        spans.push(Span::styled(
                            format!("{:>4}-", line.old.unwrap_or_default()),
                            self.sign_style(LineKind::Removed),
                        ));
                        let refined = refined
                            .get(offset)
                            .and_then(|pair| pair.as_ref().map(|(old, _)| old.as_slice()));
                        spans.extend(self.text_spans(line, refined, cell));
                    }
                    None => spans.push(Span::styled(
                        " ".repeat(half),
                        self.style(LineKind::Context),
                    )),
                }
                match additions.get(offset) {
                    Some(line) => {
                        spans.push(Span::styled(
                            format!("{:>4}+", line.new.unwrap_or_default()),
                            self.sign_style(LineKind::Added),
                        ));
                        let refined = refined
                            .get(offset)
                            .and_then(|pair| pair.as_ref().map(|(_, new)| new.as_slice()));
                        spans.extend(self.text_spans(line, refined, cell));
                    }
                    None => spans.push(Span::styled(
                        " ".repeat(half),
                        self.style(LineKind::Context),
                    )),
                }
                rows.push(Line::from(spans));
            }
            index = end;
        }
        (rows, hunks)
    }
}

impl Component for DiffView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        fill(frame.buffer_mut(), area, self.context.surface());
        let lines = self.lines(area.width);
        let visible = lines
            .into_iter()
            .skip(self.offset)
            .take(usize::from(area.height))
            .collect::<Vec<_>>();
        Paragraph::new(visible)
            .style(self.context.surface())
            .render(area, frame.buffer_mut());
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

/// How many rows a diff dialog scrolls for a page key.
///
/// A fixed step rather than the frame's height: [`crate::views::dialog::Dialog::lines`]
/// is handed a width but not a height, so the dialog cannot know its own viewport.
const PAGE_ROWS: isize = 20;

/// The diff viewer as a modal, which is how `diff_open` reaches it.
///
/// A wrapper rather than a `Dialog` impl on [`DiffBrowser`] itself, because the line
/// renderer is also mounted as a plain [`Component`] and the two want different scroll
/// ownership: the component form offsets inside its own `render`, while a dialog is asked
/// for rows by its host and must therefore apply the offset before returning them.
///
/// # Why the browser lives here and not behind a second action
///
/// `diff_open` is the only production route to a diff viewer
/// (`views/session.rs::diff_view`), so the file tree had to arrive *inside* this dialog
/// or arrive unreachable. Adding a `diff_browser_open` action instead would have left
/// `DiffDialog` as the thing users actually reach and the tree as a surface with zero
/// production constructors — the failure mode this project has now recorded four times.
pub struct DiffDialog {
    browser: DiffBrowser,
    offset: usize,
    rows: usize,
}

impl DiffDialog {
    /// Open `patch` in a modal diff viewer.
    #[must_use]
    pub fn new(context: ViewContext, patch: &str) -> Self {
        Self {
            browser: DiffBrowser::new(context, patch),
            offset: 0,
            rows: 0,
        }
    }

    fn scroll(&mut self, delta: isize) -> crate::views::dialog::DialogStep {
        let target = isize::try_from(self.offset)
            .unwrap_or(isize::MAX)
            .saturating_add(delta);
        self.jump(usize::try_from(target.max(0)).unwrap_or(0))
    }

    fn jump(&mut self, row: usize) -> crate::views::dialog::DialogStep {
        self.offset = row.min(self.rows.saturating_sub(1));
        crate::views::dialog::DialogStep::Redraw
    }
}

impl crate::views::dialog::Dialog for DiffDialog {
    fn id(&self) -> &'static str {
        "diff_open"
    }

    fn title(&self) -> String {
        let files = self.browser.files();
        let additions: usize = files.iter().map(|file| file.additions).sum();
        let deletions: usize = files.iter().map(|file| file.deletions).sum();
        let current = files
            .get(self.browser.selected())
            .map_or("", |file| file.path.as_str());
        if files.len() > 1 {
            return format!(
                "Diff — {} files  +{additions} -{deletions}  ·  {current}",
                files.len()
            );
        }
        format!("Diff — {current}  +{additions} -{deletions}")
    }

    /// `§11.4` puts the inline diff at `XLarge`, and a browser needs at least as much:
    /// the tree takes [`crate::views::diff_browser::FILE_TREE_WIDTH`] columns off the
    /// front, so a `Large` tier would leave the patch below
    /// [`crate::views::diff_browser::PATCH_MIN_WIDTH`] on any terminal narrower than
    /// about 120 and drop the tree it exists to show.
    fn width(&self) -> crate::views::dialog::DialogWidth {
        crate::views::dialog::DialogWidth::XLarge
    }

    /// The whole frame, always.
    ///
    /// The default sizes a dialog to the rows its body produced, and this body shrinks as
    /// the offset advances — so scrolling toward the end of a patch made the modal shrink
    /// and slide down the frame under the reader, moving every row they were reading.
    /// Measured while writing the hunk-navigation test: after `]` reached the last hunk,
    /// the title had migrated from the first row of the frame to the third. Empty rows
    /// below the last line of a patch are the smaller cost by a wide margin.
    fn desired_height(&self, _content_rows: u16, available: u16) -> u16 {
        available
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let all = self.browser.lines(width);
        self.rows = all.len();
        // Clamped after the row count is known: a `]` pressed on the last hunk of a
        // patch that has since been re-rendered narrower would otherwise hold an offset
        // past the end and show an empty viewer.
        self.offset = self.offset.min(self.rows.saturating_sub(1));
        all.into_iter().skip(self.offset).collect()
    }

    /// Five hints, and `↑↓ scroll` is deliberately not among them.
    ///
    /// The host renders the footer through a `Paragraph`, which drops the *tail* — so the
    /// hint list is ordered by what may be lost, and `esc close` must never be it. Six
    /// hints measured 62 columns and the `XLarge` tier collapses to 54 on a 60-column
    /// terminal, which silently truncated the only hint saying how to leave a modal that
    /// still owns the keyboard. Arrow-key scrolling is the one affordance a reader tries
    /// without being told, so it is what pays for `esc` surviving.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("[]", "hunk"),
            ("pn", "file"),
            ("b", "tree"),
            ("v", "split"),
            ("esc", "close"),
        ]
    }

    fn handle_action(
        &mut self,
        action: &'static Definition,
        _event: &KeyEvent,
    ) -> crate::views::dialog::DialogStep {
        match action.name {
            // `session_interrupt` as well as `diff_close`, because `escape` resolves to
            // the former in the scope chain a session screen carries — the same reason
            // the pickers accept it.
            "diff_close" | "session_interrupt" => crate::views::dialog::DialogStep::Resolved(
                crate::views::dialog::DialogOutcome::Cancelled,
            ),
            // Both vocabularies: `dialog.select.*` is what the arrow keys resolve to
            // while a modal is focused, and `messages_*` is what the same keys resolve to
            // from the session screen's own chain. Accepting one only would leave the
            // viewer scrollable by half the keys that reach it.
            "dialog.select.prev" | "messages_line_up" => self.scroll(-1),
            "dialog.select.next" | "messages_line_down" => self.scroll(1),
            "dialog.select.page_up" | "messages_page_up" => self.scroll(-PAGE_ROWS),
            "dialog.select.page_down" | "messages_page_down" => self.scroll(PAGE_ROWS),
            "dialog.select.home" | "messages_first" => self.scroll(isize::MIN),
            "dialog.select.end" | "messages_last" => self.scroll(isize::MAX),
            // These six are the `diff` scope's bare letters. Claiming them here cannot
            // stop them being typed into the prompt: a dialog is only offered an action
            // while it is on `DialogHost`'s stack, and with the stack empty the same
            // action reaches `SessionScreen`, which has no arm for any of them and
            // therefore lets the key fall through to the editor. That fall-through is
            // the entire reason `session::scopes()` can afford to list `diff` at all, so
            // the arm that must never be added is there, not here.
            "diff_next_hunk" => match self.browser.next_hunk(self.offset) {
                Some(row) => self.jump(row),
                None => crate::views::dialog::DialogStep::Redraw,
            },
            "diff_previous_hunk" => match self.browser.previous_hunk(self.offset) {
                Some(row) => self.jump(row),
                None => crate::views::dialog::DialogStep::Redraw,
            },
            // Moving files rewinds the viewport, because the selected file's patch is
            // what the reader just asked to see and it is above the current offset.
            "diff_next_file" => {
                self.browser.next_file();
                self.jump(0)
            }
            "diff_previous_file" => {
                self.browser.previous_file();
                self.jump(0)
            }
            "diff_toggle_file_tree" => {
                self.browser.toggle_tree();
                crate::views::dialog::DialogStep::Redraw
            }
            "diff_single_patch" => {
                self.browser.toggle_single();
                self.jump(0)
            }
            "diff_toggle_view" => {
                self.browser.toggle_columns();
                self.jump(0)
            }
            _ => crate::views::dialog::DialogStep::Ignored,
        }
    }
}
