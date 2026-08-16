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
        }
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
        let palette = &self.context.palette;
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
        let palette = &self.context.palette;
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
        let palette = &self.context.palette;
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
        self.columns = self
            .forced
            .unwrap_or_else(|| self.context.diff_columns(width));
        match self.columns {
            DiffColumns::Unified => self.unified(width),
            DiffColumns::Split => self.split(width),
        }
    }

    fn unified(&self, width: u16) -> Vec<Line<'static>> {
        self.lines
            .iter()
            .map(|line| {
                if line.kind == LineKind::Header {
                    return padded(&line.text, width, self.style(LineKind::Header));
                }
                let sign = match line.kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    _ => ' ',
                };
                let number = line
                    .new
                    .or(line.old)
                    .map_or_else(|| String::from("    "), |value| format!("{value:>4}"));
                let body_width = usize::from(width).saturating_sub(6);
                let body = fit(&line.text, body_width);
                Line::from(vec![
                    Span::styled(number, self.number_style(line.kind)),
                    Span::styled(format!("{sign}"), self.sign_style(line.kind)),
                    Span::styled(format!(" {body}"), self.style(line.kind)),
                ])
            })
            .collect()
    }

    fn split(&self, width: u16) -> Vec<Line<'static>> {
        let half = usize::from(width / 2).max(1);
        let cell = half.saturating_sub(5).max(1);
        let mut rows = Vec::new();
        let mut index = 0;
        while index < self.lines.len() {
            let line = &self.lines[index];
            if line.kind == LineKind::Header {
                rows.push(padded(&line.text, width, self.style(LineKind::Header)));
                index += 1;
                continue;
            }
            if line.kind == LineKind::Context {
                rows.push(Line::from(vec![
                    Span::styled(
                        format!("{:>4} ", line.old.unwrap_or_default()),
                        self.number_style(LineKind::Context),
                    ),
                    Span::styled(fit(&line.text, cell), self.style(LineKind::Context)),
                    Span::styled(
                        format!("{:>4} ", line.new.unwrap_or_default()),
                        self.number_style(LineKind::Context),
                    ),
                    Span::styled(fit(&line.text, cell), self.style(LineKind::Context)),
                ]));
                index += 1;
                continue;
            }
            // Pair a removal run with the addition run that follows it, so the two
            // sides line up the way a reviewer reads them.
            let removals = self.lines[index..]
                .iter()
                .take_while(|line| line.kind == LineKind::Removed)
                .collect::<Vec<_>>();
            let after = index + removals.len();
            let additions = self.lines[after..]
                .iter()
                .take_while(|line| line.kind == LineKind::Added)
                .collect::<Vec<_>>();
            for offset in 0..removals.len().max(additions.len()) {
                let left = removals.get(offset);
                let right = additions.get(offset);
                let mut spans = Vec::new();
                match left {
                    Some(line) => {
                        spans.push(Span::styled(
                            format!("{:>4}-", line.old.unwrap_or_default()),
                            self.sign_style(LineKind::Removed),
                        ));
                        spans.push(Span::styled(
                            fit(&line.text, cell),
                            self.style(LineKind::Removed),
                        ));
                    }
                    None => spans.push(Span::styled(
                        " ".repeat(half),
                        self.style(LineKind::Context),
                    )),
                }
                match right {
                    Some(line) => {
                        spans.push(Span::styled(
                            format!("{:>4}+", line.new.unwrap_or_default()),
                            self.sign_style(LineKind::Added),
                        ));
                        spans.push(Span::styled(
                            fit(&line.text, cell),
                            self.style(LineKind::Added),
                        ));
                    }
                    None => spans.push(Span::styled(
                        " ".repeat(half),
                        self.style(LineKind::Context),
                    )),
                }
                rows.push(Line::from(spans));
            }
            index = after + additions.len();
        }
        rows
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
/// A wrapper rather than a `Dialog` impl on [`DiffView`] itself, because the view is also
/// mounted as a plain [`Component`] and the two want different scroll ownership: the
/// component form offsets inside its own `render`, while a dialog is asked for rows by
/// its host and must therefore apply the offset before returning them.
pub struct DiffDialog {
    view: DiffView,
    offset: usize,
    rows: usize,
}

impl DiffDialog {
    /// Open `patch` in a modal diff viewer.
    #[must_use]
    pub fn new(context: ViewContext, patch: &str) -> Self {
        let view = DiffView::new(context, patch);
        let rows = view.parsed().len();
        Self {
            view,
            offset: 0,
            rows,
        }
    }

    fn scroll(&mut self, delta: isize) -> crate::views::dialog::DialogStep {
        let target = isize::try_from(self.offset)
            .unwrap_or(isize::MAX)
            .saturating_add(delta);
        let next = usize::try_from(target.max(0))
            .unwrap_or(0)
            .min(self.rows.saturating_sub(1));
        if next == self.offset {
            return crate::views::dialog::DialogStep::Redraw;
        }
        self.offset = next;
        crate::views::dialog::DialogStep::Redraw
    }
}

impl crate::views::dialog::Dialog for DiffDialog {
    fn id(&self) -> &'static str {
        "diff_open"
    }

    fn title(&self) -> String {
        format!("Diff — {} lines", self.rows)
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let all = self.view.lines(width);
        all.into_iter().skip(self.offset).collect()
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓", "scroll"), ("v", "split/unified"), ("esc", "close")]
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
            "diff_toggle_view" => {
                self.view.set_columns(match self.view.columns() {
                    DiffColumns::Unified => DiffColumns::Split,
                    DiffColumns::Split => DiffColumns::Unified,
                });
                crate::views::dialog::DialogStep::Redraw
            }
            _ => crate::views::dialog::DialogStep::Ignored,
        }
    }
}

/// Truncate or pad `text` to exactly `width` display cells.
fn fit(text: &str, width: usize) -> String {
    let mut out = text.chars().take(width).collect::<String>();
    let len = out.chars().count();
    if len < width {
        out.extend(std::iter::repeat_n(' ', width - len));
    }
    out
}
