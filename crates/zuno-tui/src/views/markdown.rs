//! Assistant prose, rendered as formatted rows instead of its own source.
//!
//! A model's reply is CommonMark. Before this module the transcript wrapped it as
//! plain text, so `**bold**`, `# Heading` and a triple-backtick fence reached the
//! screen as the literal punctuation the model typed. Plan §7.1 is this module; §11.2
//! records it as "提升可读性最大的一项" among the four message-visual gaps.
//!
//! # Rows of spans, never ANSI
//!
//! [`render`] returns `Vec<Vec<Span<'static>>>`: one inner vector per screen row,
//! already broken to the width it was asked for. It does **not** produce an ANSI
//! string. Plan §7.1 names that explicitly as the root cause of the reference
//! implementation's misaligned tables — ratatui measures a `Span`'s content, so an
//! escape sequence embedded in the text counts as visible columns that are not there,
//! and every width decision downstream is wrong by the length of the escape.
//!
//! Rows rather than a `Paragraph` with `Wrap` for the same reason
//! [`super::message::wrap`] exists: the transcript has to *count* the rows a message
//! will occupy before it draws them, because the scroll offset and the scrollbar are
//! both derived from that count. A widget that wraps internally knows the number and
//! will not say.
//!
//! # Why the signature takes a `&Palette` and not a [`super::ViewContext`]
//!
//! Colours still come from the resolved theme — the caller hands over
//! `ViewContext::palette()`, and this module names no colour, which the
//! `views_tests` palette scan enforces over this file like every other. What it does
//! not take is the context itself, because a `ViewContext` carries a lock and a
//! configuration this renderer has no use for, and because taking one would make the
//! function's output depend on shared interior-mutable state — exactly the property
//! the next section says must not exist.
//!
//! # Purity, so a cache can be added above it
//!
//! [`render`] is a pure function of `(source, width, palette)`. It reads no global, it
//! holds no cache, and it mutates nothing that outlives the call — [`Builder`] is
//! constructed per call and dropped with it. That is a requirement rather than a
//! preference: `.omo/plans/memory-perf-optimization.md` §3.3 R2-R5 add a
//! prepared-frame cache and a 2048-entry per-message line cache *above* this
//! function, and a memoised function that consults hidden state cannot be memoised
//! correctly. Nothing here needs `&mut self` on a value the caller keeps.
//!
//! # Columns, not characters; clusters, not characters
//!
//! Every width is [`display_width`]. Plan §10.2 lists `chars().count()` table widths
//! among the reference-implementation defects this project does not replicate, and the
//! repository has already measured the failure once: a CJK model name pushed a border
//! past the frame. Over-long words break at **grapheme cluster** boundaries, because a
//! ZWJ emoji or a combining mark cut between its pieces is not something a terminal
//! can draw.
//!
//! # Content survives, formatting is best-effort
//!
//! `pulldown-cmark` is a CommonMark parser: it has no error case, so an unterminated
//! fence, a stray `*`, or a table with ragged columns each produce *some* event stream
//! rather than a failure. This module keeps that property end to end — every arm emits
//! its text somewhere, including the HTML arms and the ragged-table arm. A user's words
//! matter more than the shape they are drawn in.
//!
//! # Syntax highlighting stays inside the code-row seam
//!
//! Plan §7.2 / P2-2 is isolated in [`Builder::code_rows`]: it asks the bounded
//! tree-sitter adapter for styled spans, then applies the same hard-break path used by
//! the plain fallback. The frame, label and width arithmetic remain outside the seam.

use super::{display_width, highlight};
use crate::theme::Palette;
use pulldown_cmark::{
    BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;

/// One rendered row: the spans that fill it, left to right.
pub type Row = Vec<Span<'static>>;

/// The narrowest content column this module will lay text into.
///
/// Below this a row holds a glyph and nothing else, so indentation is dropped rather
/// than allowed to consume the whole frame — at 20 columns behind a two-column gutter
/// a fourth-level list would otherwise leave no room for the list's own text.
const MIN_CONTENT_WIDTH: usize = 4;

/// Columns of indent per list nesting level (plan §7.1: "每层缩进 2 空格").
const LIST_INDENT: usize = 2;

/// The bullet drawn for an unordered list item (plan §7.1).
const BULLET: &str = "• ";

/// The rule drawn for a thematic break, repeated to the available width.
const RULE_GLYPH: &str = "─";

/// The narrowest a table column is squeezed to before its text is truncated.
///
/// Three columns hold an ellipsis and one character. Shrinking further produces a
/// column of pure punctuation, which carries less than the truncation it is trying
/// to avoid.
const MIN_TABLE_COLUMN: usize = 3;

/// Render `source` as rows no wider than `width` terminal columns.
///
/// Every colour is read from `palette`; this module names none. The mapping follows
/// plan §7.1's element table and §11.5's semantic assignment.
#[must_use]
pub fn render(source: &str, width: u16, palette: &Palette) -> Vec<Row> {
    let mut builder = Builder::new(width, palette);
    let mut options = Options::empty();
    // Five extensions, each because models emit the syntax unprompted and the
    // alternative is worse than rendering it. Without `TABLES` a pipe table's rows
    // become one paragraph and collapse into a single reflowed line; without
    // `TASKLISTS` a checklist renders its `[ ]` as literal text inside the bullet;
    // without `STRIKETHROUGH` a `~~word~~` keeps its tildes; without `GFM` a
    // `> [!NOTE]` alert keeps its bracket syntax as prose. Footnotes are left off:
    // their syntax degrades to readable literal text, and the definition block would
    // need a placement decision this task does not own.
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    options.insert(Options::ENABLE_GFM);
    for event in Parser::new_ext(source, options) {
        builder.event(event);
    }
    builder.finish()
}

/// The columns `spans` occupy, measured the way the terminal will draw them.
#[must_use]
pub fn row_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|span| display_width(&span.content)).sum()
}

/// The longest prefix of `spans` that fits in `width` columns.
///
/// The safety net every caller gets for free: whatever a block emitter computed, the
/// row handed to ratatui cannot be wider than the frame. Splitting stops at cluster
/// boundaries, so the cut never lands inside a wide glyph or an emoji sequence.
///
/// # The one documented exception
///
/// Content that is not empty never truncates to *nothing*. One column cannot hold a
/// two-column glyph, and a strict clamp at that width deleted the glyph outright —
/// measured, as `render("日本語", 1, …)` returning no rows at all. A column of overflow
/// is an artefact the terminal absorbs; a deleted sentence is not recoverable. This is
/// the same trade [`super::message::wrap`] documents for the same situation, so the two
/// wrappers agree.
#[must_use]
pub fn truncate_row(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut used = 0;
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut overflow: Option<Span<'static>> = None;
    for span in spans {
        let cost = display_width(&span.content);
        if used + cost <= width {
            used += cost;
            out.push(span);
            continue;
        }
        let head = truncate_clusters(&span.content, width - used);
        if head.is_empty() {
            if out.is_empty() && !span.content.is_empty() {
                let (first, _) = split_first_cluster(&span.content);
                overflow = Some(Span::styled(first, span.style));
            }
        } else {
            out.push(Span::styled(head, span.style));
        }
        break;
    }
    if let Some(span) = overflow.filter(|_| out.is_empty()) {
        out.push(span);
    }
    out
}

/// The longest prefix of `text` fitting in `width` columns, cut between clusters.
///
/// [`super::truncate`] stops before a wide *character*, which is right for a CJK glyph and
/// wrong for `👩‍💻`: that is one cluster of four scalar values, and a cut inside it
/// leaves a terminal drawing a woman and a computer where the author wrote a
/// programmer. Measuring per cluster costs one extra iterator and removes the class.
#[must_use]
fn truncate_clusters(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for cluster in text.graphemes(true) {
        let cost = display_width(cluster);
        if used + cost > width {
            break;
        }
        out.push_str(cluster);
        used += cost;
    }
    out
}

/// The first cluster of `text`, and the rest.
///
/// Used only when the available width cannot hold even one cluster. Emitting the
/// cluster anyway costs a column of overflow the terminal absorbs; consuming nothing
/// would spin forever, which is the trade [`super::message::wrap`] already documented
/// for the same situation.
fn split_first_cluster(text: &str) -> (String, &str) {
    match text.graphemes(true).next() {
        Some(first) => (first.to_owned(), &text[first.len()..]),
        None => (String::new(), text),
    }
}

fn break_code_row(mut pending: Row, width: usize) -> Vec<Row> {
    if pending.is_empty() {
        return vec![Vec::new()];
    }
    let mut rows = Vec::new();
    while !pending.is_empty() {
        let (head, tail) = split_code_row(pending, width);
        rows.push(head);
        pending = tail;
    }
    rows
}

fn split_code_row(spans: Row, width: usize) -> (Row, Row) {
    let mut head = Vec::new();
    let mut tail = Vec::new();
    let mut used = 0;
    let mut spans = spans.into_iter();
    while let Some(span) = spans.next() {
        let style = span.style;
        let content = span.content.into_owned();
        let available = width.saturating_sub(used);
        let fitting = truncate_clusters(&content, available);
        if fitting.len() == content.len() {
            used += display_width(&content);
            head.push(Span::styled(content, style));
            continue;
        }
        if fitting.is_empty() {
            if head.is_empty() {
                let (first, rest) = split_first_cluster(&content);
                head.push(Span::styled(first, style));
                if !rest.is_empty() {
                    tail.push(Span::styled(rest.to_owned(), style));
                }
            } else {
                tail.push(Span::styled(content, style));
            }
        } else {
            let taken = fitting.len();
            head.push(Span::styled(fitting, style));
            tail.push(Span::styled(content[taken..].to_owned(), style));
        }
        tail.extend(spans);
        break;
    }
    (head, tail)
}

// ---------------------------------------------------------------------------
// Inline tokens
// ---------------------------------------------------------------------------

/// One unit of inline content, before it is assigned to a row.
///
/// A space is its **own** token rather than part of a word, because a styled run can
/// end mid-word: `**bo**ld` arrives as a bold `bo` and a plain `ld` with nothing
/// between them, and a wrapper that re-joined words with spaces would render `bo ld`.
/// Making the separator explicit is what keeps adjacent styles adjacent.
#[derive(Debug, Clone)]
enum Token {
    Word { text: String, style: Style },
    Space,
    HardBreak,
}

/// A styled run of code text inside a fence, before it is framed.
#[derive(Debug)]
struct Fence {
    language: Option<String>,
    text: String,
}

/// One cell of a table, as tokens so its emphasis survives the grid.
type Cell = Vec<Token>;

/// A table under construction.
#[derive(Debug, Default)]
struct Table {
    head: Vec<Cell>,
    body: Vec<Vec<Cell>>,
    in_head: bool,
}

/// One level of list nesting.
#[derive(Debug)]
struct ListLevel {
    /// The next ordinal for an ordered list, or `None` for a bullet list.
    ///
    /// Carried forward from the source's own start number rather than restarted at
    /// one, because a model that wrote `3.` meant to continue a list.
    next_ordinal: Option<u64>,
    /// The marker owed to the item currently open, taken by its first emitted row.
    ///
    /// An option rather than a flag because a loose list item holds several blocks and
    /// only the first of them wears the bullet; the rest align under it.
    pending_marker: Option<String>,
}

// ---------------------------------------------------------------------------
// The builder
// ---------------------------------------------------------------------------

/// Per-call state: the event stream folded into rows.
///
/// Deliberately not reachable from outside this module and deliberately not reusable
/// across calls — see this module's `# Purity` note.
struct Builder<'palette> {
    width: usize,
    palette: &'palette Palette,
    rows: Vec<Row>,
    /// Inline tokens accumulated for the block currently open.
    inline: Vec<Token>,
    /// The style stack: emphasis, strong, code, link and strikethrough compose.
    styles: Vec<Style>,
    lists: Vec<ListLevel>,
    quote_depth: usize,
    heading: Option<HeadingLevel>,
    fence: Option<Fence>,
    table: Option<Table>,
    /// Set while inline events belong to a table cell rather than to a block.
    cell: Option<Cell>,
    /// Link and image destinations awaiting their closing tag.
    ///
    /// A stack because a link's label may itself contain an image, so the two
    /// destinations are open at once and the inner one closes first.
    destinations: Vec<String>,
}

impl<'palette> Builder<'palette> {
    fn new(width: u16, palette: &'palette Palette) -> Self {
        Self {
            width: usize::from(width),
            palette,
            rows: Vec::new(),
            inline: Vec::new(),
            styles: Vec::new(),
            lists: Vec::new(),
            quote_depth: 0,
            heading: None,
            fence: None,
            table: None,
            cell: None,
            destinations: Vec::new(),
        }
    }

    /// Close a link or an image: `]`, then `(dest)` when there is one.
    ///
    /// An autolink has label and destination equal, and repeating it reads as a stutter,
    /// so the destination is suppressed in exactly that case.
    fn close_destination(&mut self, color: crate::theme::Rgba) {
        self.styles.pop();
        let style = self.tinted(color);
        let destination = self.destinations.pop().unwrap_or_default();
        let label_was_the_url = matches!(
            self.inline.last(),
            Some(Token::Word { text, .. }) if *text == destination
        );
        self.push_atom(String::from("]"), style);
        if !destination.is_empty() && !label_was_the_url {
            self.push_atom(format!("({destination})"), style);
        }
    }

    // -- styles -------------------------------------------------------------

    /// Body text on the panel background.
    ///
    /// Every style in this module starts here so that no row leaves a cell at
    /// `Color::Reset`, which shows through as a stripe on a terminal whose own
    /// background differs from the theme's (`views::fill` documents the same trap).
    fn base(&self) -> Style {
        Style::new()
            .fg(self.palette.markdown_text.into())
            .bg(self.palette.background_panel.into())
    }

    fn tinted(&self, color: crate::theme::Rgba) -> Style {
        self.base().fg(color.into())
    }

    /// The style inline text should carry right now.
    fn inline_style(&self) -> Style {
        let mut style = self.styles.last().copied().unwrap_or_else(|| self.base());
        if self.heading.is_some() {
            style = style.fg(self.palette.markdown_heading.into());
        }
        if self.quote_depth > 0 && self.styles.is_empty() {
            style = self
                .tinted(self.palette.markdown_block_quote)
                .add_modifier(Modifier::ITALIC);
        }
        style
    }

    fn push_style(&mut self, mutate: impl FnOnce(Style) -> Style) {
        let current = self.inline_style();
        self.styles.push(mutate(current));
    }

    // -- inline accumulation ------------------------------------------------

    /// Split `text` into word and space tokens at the current style.
    ///
    /// A newline inside `text` is treated as a space: that is a CommonMark soft break,
    /// and the whole point of reflowing a paragraph is that the author's line endings
    /// are not the reader's.
    fn push_text(&mut self, text: &str) {
        let style = self.inline_style();
        for (index, word) in text.split([' ', '\n', '\t']).enumerate() {
            if index > 0 {
                self.inline.push(Token::Space);
            }
            if !word.is_empty() {
                self.inline.push(Token::Word {
                    text: word.to_owned(),
                    style,
                });
            }
        }
    }

    /// Push `text` as one unbreakable word, whatever whitespace it contains.
    ///
    /// Inline code keeps its spaces: `` `a b` `` is one identifier as far as the reader
    /// is concerned, and letting the wrapper break it would put half a symbol on each
    /// of two rows.
    fn push_atom(&mut self, text: String, style: Style) {
        self.inline.push(Token::Word { text, style });
    }

    /// Move the accumulated inline tokens somewhere, leaving the accumulator empty.
    fn take_inline(&mut self) -> Vec<Token> {
        std::mem::take(&mut self.inline)
    }

    // -- prefixes -----------------------------------------------------------

    /// The prefix every row of the current block carries, before its content.
    ///
    /// `first` distinguishes the row that wears a list marker from the rows that align
    /// under it. The hanging indent is what makes a wrapped list item readable as one
    /// item rather than as an item followed by a paragraph.
    ///
    /// # Why the decoration yields to the text
    ///
    /// Quote bars and list indent are both **capped** so that
    /// [`MIN_CONTENT_WIDTH`] columns always survive for content. Without the cap a
    /// fifth-level list inside a quote inside a 20-column frame produces a prefix wider
    /// than the frame, and [`Self::push_row`]'s clamp then throws the user's sentence
    /// away to make room for indentation describing where the sentence was. Deep
    /// nesting in a narrow frame renders flat, which loses structure; the alternative
    /// loses words.
    fn prefix(&self, first: bool) -> Row {
        let marker = self
            .lists
            .last()
            .and_then(|level| level.pending_marker.as_deref());
        let budget = self
            .width
            .saturating_sub(MIN_CONTENT_WIDTH)
            .saturating_sub(display_width(marker.unwrap_or("")));
        let bars = self.quote_depth.min(budget / 2);
        let indent = if self.lists.is_empty() {
            0
        } else {
            (LIST_INDENT * (self.lists.len() - 1)).min(budget.saturating_sub(2 * bars))
        };

        let mut spans = Vec::new();
        // Plan §7.1: a quote is prefixed `│ ` and the whole block is muted italic. One
        // bar per level of nesting, so a quoted quote is visibly deeper.
        for _ in 0..bars {
            spans.push(Span::styled(
                String::from("│ "),
                self.tinted(self.palette.markdown_block_quote),
            ));
        }
        if self.lists.is_empty() {
            return spans;
        }
        if indent > 0 {
            spans.push(Span::styled(" ".repeat(indent), self.base()));
        }
        // The marker yields for the same reason the indent does, one step further along:
        // when it would consume the whole row there is nothing left to mark. At width 1 a
        // two-column `• ` filled the row and `push_row` then clamped the item's text away
        // entirely — six nested items rendered as six bullets and no words.
        let marker = marker
            .filter(|marker| display_width(marker) < self.width.saturating_sub(indent + 2 * bars));
        match (first, marker) {
            (true, Some(marker)) => {
                let ordered = self
                    .lists
                    .last()
                    .is_some_and(|level| level.next_ordinal.is_some());
                let color = if ordered {
                    self.palette.markdown_list_enumeration
                } else {
                    self.palette.markdown_list_item
                };
                spans.push(Span::styled(
                    marker.to_owned(),
                    self.tinted(color).add_modifier(Modifier::BOLD),
                ));
            }
            (_, Some(marker)) => {
                spans.push(Span::styled(" ".repeat(display_width(marker)), self.base()));
            }
            (_, None) => {}
        }
        spans
    }

    /// Consume the current item's marker, so the next row aligns instead of repeating it.
    fn consume_marker(&mut self) {
        if let Some(level) = self.lists.last_mut() {
            level.pending_marker = None;
        }
    }

    /// The width available for content, given a prefix of `prefix_width` columns.
    ///
    /// Indentation is dropped rather than honoured when honouring it would leave no
    /// usable column. A deeply nested list in a 20-column frame is the case: the
    /// indent is decoration and the text is the message.
    fn content_width(&self, prefix_width: usize) -> usize {
        self.width
            .saturating_sub(prefix_width)
            .max(MIN_CONTENT_WIDTH.min(self.width.max(1)))
    }

    // -- emitting -----------------------------------------------------------

    /// Lay `tokens` out under the current prefix and append the resulting rows.
    fn emit_inline(&mut self, tokens: Vec<Token>) {
        if tokens.is_empty() {
            return;
        }
        let first_prefix = self.prefix(true);
        let rest_prefix = self.prefix(false);
        let content = self.content_width(row_width(&first_prefix).max(row_width(&rest_prefix)));
        let laid_out = lay_out(&tokens, content);
        for (index, mut row) in laid_out.into_iter().enumerate() {
            let mut line = if index == 0 {
                first_prefix.clone()
            } else {
                rest_prefix.clone()
            };
            line.append(&mut row);
            self.push_row(line);
        }
        self.consume_marker();
    }

    /// Append one row, clamped to the frame.
    ///
    /// The clamp is the last line of defence described on [`truncate_row`]: a block
    /// emitter that miscounted produces a short row here rather than a row ratatui
    /// clips after the layout above already counted it — which is the exact shape of
    /// the bug §11.5 records against `chars().count()`.
    fn push_row(&mut self, row: Row) {
        self.rows.push(truncate_row(row, self.width));
    }

    /// A blank row, used as the gap after a block.
    ///
    /// Not emitted at the top of the output and never doubled, so a source with blank
    /// lines between every element does not render as a column of gaps.
    fn blank(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.rows.last().is_some_and(|row| row_width(row) == 0) {
            return;
        }
        self.rows.push(Vec::new());
    }

    // -- blocks -------------------------------------------------------------

    fn open_heading(&mut self, level: HeadingLevel) {
        self.blank();
        self.heading = Some(level);
    }

    /// Plan §7.1: the `#` run is not printed; H1 is bold and underlined, H2 bold,
    /// H3 and below carry the heading colour alone.
    ///
    /// Dropping the `#` costs nothing and buys columns — a `### ` prefix spends four
    /// of a 40-column frame on punctuation — and the weight gradient says the same
    /// thing the hashes did.
    fn close_heading(&mut self) {
        let level = self.heading.take();
        let tokens = self.take_inline();
        let modifier = match level {
            Some(HeadingLevel::H1) => Modifier::BOLD | Modifier::UNDERLINED,
            Some(HeadingLevel::H2) => Modifier::BOLD,
            _ => Modifier::empty(),
        };
        let tokens = tokens
            .into_iter()
            .map(|token| match token {
                Token::Word { text, style } => Token::Word {
                    text,
                    style: style
                        .fg(self.palette.markdown_heading.into())
                        .add_modifier(modifier),
                },
                other => other,
            })
            .collect();
        self.emit_inline(tokens);
        self.blank();
    }

    fn open_list(&mut self, start: Option<u64>) {
        self.lists.push(ListLevel {
            next_ordinal: start,
            pending_marker: None,
        });
    }

    fn close_list(&mut self) {
        self.lists.pop();
        if self.lists.is_empty() {
            self.blank();
        }
    }

    fn open_item(&mut self) {
        let Some(level) = self.lists.last_mut() else {
            return;
        };
        level.pending_marker = Some(match level.next_ordinal.as_mut() {
            Some(ordinal) => {
                let marker = format!("{ordinal}. ");
                *ordinal = ordinal.saturating_add(1);
                marker
            }
            None => String::from(BULLET),
        });
    }

    /// Plan §7.1: a task item shows `[x] ` or `[ ] ` instead of a bullet.
    ///
    /// The marker replaces the bullet rather than following it, because `• [x] done`
    /// spends four columns saying "this is a list" twice.
    fn task_marker(&mut self, checked: bool) {
        if let Some(level) = self.lists.last_mut() {
            level.pending_marker = Some(String::from(if checked { "[x] " } else { "[ ] " }));
        }
    }

    fn close_item(&mut self) {
        self.flush_inline();
        // A marker that no row claimed belongs to an empty item. Drawing it on a row of
        // its own keeps the ordinal sequence intact: a list whose third entry is blank
        // still numbers its fourth entry four.
        if self
            .lists
            .last()
            .is_some_and(|level| level.pending_marker.is_some())
        {
            let prefix = self.prefix(true);
            self.push_row(prefix);
            self.consume_marker();
        }
    }

    /// Plan §7.1: a thematic break is `─` laid to the available width.
    fn rule(&mut self) {
        self.blank();
        let prefix = self.prefix(true);
        let content = self.content_width(row_width(&prefix));
        let mut row = prefix;
        row.push(Span::styled(
            RULE_GLYPH.repeat(content),
            self.tinted(self.palette.markdown_horizontal_rule),
        ));
        self.push_row(row);
        self.consume_marker();
        self.blank();
    }

    // -- code fences --------------------------------------------------------

    /// Plan §7.1: a fence is framed `╭─ <lang>` above and `╰─` below.
    ///
    /// The frame is sized to the code rather than to the terminal. A full-width rule
    /// on a 200-column terminal draws two hundred columns of furniture around four
    /// columns of code, which is the opposite of the hierarchy the frame exists to
    /// express; a frame that hugs its content reads as one object.
    ///
    /// # The P2-2 seam
    ///
    /// [`Self::code_rows`] is the only function that turns code text into spans. Syntax
    /// highlighting replaces its body and nothing around it: the frame, the label, the
    /// hard-break behaviour and the width math all stay.
    fn close_fence(&mut self) {
        let Some(fence) = self.fence.take() else {
            return;
        };
        let prefix = self.prefix(true);
        let content = self.content_width(row_width(&prefix));
        let frame_style = self.tinted(self.palette.markdown_code_block);

        // A fence whose closing marker never arrived still holds the model's code; the
        // parser reports it running to end of input, and trailing newline noise from
        // that is not content worth a blank row.
        let source = fence.text.trim_end_matches('\n');
        let lines: Vec<&str> = source.split('\n').collect();

        // The frame yields too, on the same rule as the indent, the marker and the table
        // grid: `│ ` plus one column of code is the floor, and below it the frame consumed
        // the whole row and `push_row` clamped every line of code away — measured as a
        // fence rendering `╭││││││╰` with none of its contents. Unframed code in a
        // two-column terminal is still the code.
        if content < MIN_TABLE_COLUMN {
            for chunk in self.code_rows(fence.language.as_deref(), source, content.max(1)) {
                let mut row = self.prefix(false);
                row.extend(chunk);
                self.push_row(row);
            }
            self.consume_marker();
            self.blank();
            return;
        }

        let widest = lines
            .iter()
            .map(|line| display_width(line))
            .max()
            .unwrap_or(0);
        let label = fence.language.as_deref().unwrap_or("");
        // `│ ` opens each body row and one column of right margin closes the frame.
        let needed = widest + 3;
        let labelled = display_width(label) + 5;
        let frame = needed.max(labelled).min(content).max(MIN_TABLE_COLUMN);
        let body = frame.saturating_sub(2).max(1);

        let mut top = prefix.clone();
        let opener = if label.is_empty() {
            format!("╭{}", RULE_GLYPH.repeat(frame.saturating_sub(1)))
        } else {
            let head = format!("╭{RULE_GLYPH} {label} ");
            let rest = frame.saturating_sub(display_width(&head));
            format!("{head}{}", RULE_GLYPH.repeat(rest))
        };
        top.push(Span::styled(opener, frame_style));
        self.push_row(top);
        self.consume_marker();

        for chunk in self.code_rows(fence.language.as_deref(), source, body) {
            let mut row = self.prefix(false);
            row.push(Span::styled(String::from("│ "), frame_style));
            row.extend(chunk);
            self.push_row(row);
        }

        let mut bottom = self.prefix(false);
        bottom.push(Span::styled(
            format!("╰{}", RULE_GLYPH.repeat(frame.saturating_sub(1))),
            frame_style,
        ));
        self.push_row(bottom);
        self.blank();
    }

    /// One code block, as the rows it needs at `width` columns.
    ///
    /// This remains the only code-to-span seam. The bounded capture walk either returns
    /// rows mapped through §7.2's capture→token table or yields the exact P2-1 plain
    /// style; both then take the same cluster-aware truncation path.
    ///
    /// Code is broken, never reflowed: indentation is meaning in most languages, and a
    /// word-wrapped `if` is a different program to read. The break lands on cluster
    /// boundaries so a wide glyph inside a string literal stays whole.
    fn code_rows(&self, language: Option<&str>, source: &str, width: usize) -> Vec<Row> {
        let rows = highlight::spans(language, source, self.palette).unwrap_or_else(|| {
            let style = self.tinted(self.palette.markdown_code_block);
            source
                .split('\n')
                .map(|line| {
                    if line.is_empty() {
                        Vec::new()
                    } else {
                        vec![Span::styled(line.to_owned(), style)]
                    }
                })
                .collect()
        });
        rows.into_iter()
            .flat_map(|row| break_code_row(row, width))
            .collect()
    }

    // -- tables -------------------------------------------------------------

    /// Plan §7.1: a table is drawn as a grid whose widths come from `unicode-width`.
    ///
    /// Ragged input is the normal case, not the exception: a model that writes a
    /// five-column header and a four-cell row is common. A short row is padded and a
    /// long one widens the grid, so no cell is dropped either way.
    fn close_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let prefix = self.prefix(true);
        let available = self.content_width(row_width(&prefix));
        let columns = table
            .body
            .iter()
            .map(Vec::len)
            .chain(std::iter::once(table.head.len()))
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return;
        }

        // A grid needs `MIN_TABLE_COLUMN` per column plus ` │ ` between each. Below that
        // there is no arrangement of columns that fits, and drawing one anyway means
        // every row is clamped and most cells are deleted — a five-column table in a
        // 20-column frame loses four of them. Falling back to wrapped `a │ b` prose keeps
        // the data and loses only the alignment, which is the cheaper half.
        let floor = columns * MIN_TABLE_COLUMN + 3 * columns.saturating_sub(1);
        if available < floor {
            for row in std::iter::once(&table.head).chain(table.body.iter()) {
                let mut tokens = Vec::new();
                for (index, cell) in row.iter().enumerate() {
                    if index > 0 {
                        tokens.push(Token::Space);
                        tokens.push(Token::Word {
                            text: String::from("│"),
                            style: self.tinted(self.palette.border_subtle),
                        });
                        tokens.push(Token::Space);
                    }
                    tokens.extend(cell.iter().cloned());
                }
                self.emit_inline(tokens);
            }
            self.blank();
            return;
        }

        let widths = self.table_widths(&table, columns, available);
        let separator_style = self.tinted(self.palette.border_active);

        let mut first = true;
        if !table.head.is_empty() {
            let row = self.grid_row(&table.head, &widths, true);
            self.emit_grid_row(row, &mut first);
            let mut rule = self.prefix(false);
            let mut drawn = Vec::new();
            for (index, column) in widths.iter().enumerate() {
                if index > 0 {
                    drawn.push(String::from(RULE_GLYPH));
                    drawn.push(String::from("┼"));
                    drawn.push(String::from(RULE_GLYPH));
                }
                drawn.push(RULE_GLYPH.repeat(*column));
            }
            rule.push(Span::styled(drawn.concat(), separator_style));
            self.push_row(rule);
        }
        for body in &table.body {
            let row = self.grid_row(body, &widths, false);
            self.emit_grid_row(row, &mut first);
        }
        self.blank();
    }

    /// Emit one grid row, taking the list marker on the first of them.
    fn emit_grid_row(&mut self, mut cells: Row, first: &mut bool) {
        let mut row = self.prefix(*first);
        if *first {
            *first = false;
        }
        row.append(&mut cells);
        self.push_row(row);
        self.consume_marker();
    }

    /// Column widths that fit `available`, shrinking the widest first.
    ///
    /// Shrinking the widest rather than scaling everything proportionally keeps a
    /// narrow column readable: a two-character `ok` column scaled by 0.6 becomes one
    /// character and says nothing, while the prose column beside it can lose ten and
    /// still be read.
    fn table_widths(&self, table: &Table, columns: usize, available: usize) -> Vec<usize> {
        let mut widths = vec![0_usize; columns];
        let rows = std::iter::once(&table.head).chain(table.body.iter());
        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                let width = row_width(&flatten(cell));
                if let Some(slot) = widths.get_mut(index) {
                    *slot = (*slot).max(width);
                }
            }
        }
        let gaps = 3 * columns.saturating_sub(1);
        let budget = available.saturating_sub(gaps);
        let mut total: usize = widths.iter().sum();
        while total > budget {
            let Some(widest) = widths
                .iter_mut()
                .filter(|width| **width > MIN_TABLE_COLUMN)
                .max_by_key(|width| **width)
            else {
                break;
            };
            *widest -= 1;
            total -= 1;
        }
        widths
    }

    /// One row of cells padded to `widths` and joined with ` │ `.
    fn grid_row(&self, cells: &[Cell], widths: &[usize], head: bool) -> Row {
        let separator_style = if head {
            Style::new()
                .fg(self.palette.border_active.into())
                .bg(self.palette.background_element.into())
        } else {
            self.tinted(self.palette.border_subtle)
        };
        let head_style = Style::new()
            .fg(self.palette.markdown_heading.into())
            .bg(self.palette.background_element.into())
            .add_modifier(Modifier::BOLD);
        let mut out = Vec::new();
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                out.push(Span::styled(String::from(" │ "), separator_style));
            }
            // A missing cell is a ragged row, which is padding rather than an error.
            let spans = cells
                .get(index)
                .map(|cell| flatten(cell))
                .unwrap_or_default();
            let spans = spans
                .into_iter()
                .map(|span| {
                    if head {
                        Span::styled(
                            span.content,
                            span.style
                                .fg(self.palette.markdown_heading.into())
                                .bg(self.palette.background_element.into())
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        span
                    }
                })
                .collect();
            let mut spans = truncate_row(spans, *width);
            let used = row_width(&spans);
            if used < *width {
                spans.push(Span::styled(
                    " ".repeat(width - used),
                    if head { head_style } else { self.base() },
                ));
            }
            out.extend(spans);
        }
        out
    }

    // -- the event fold -----------------------------------------------------

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some(fence) = self.fence.as_mut() {
                    fence.text.push_str(&text);
                } else {
                    self.push_text(&text);
                }
            }
            // Plan §7.1: inline code keeps its backticks. They are the shortest
            // possible signal that a word is a symbol rather than prose, and colour
            // alone is lost on a monochrome terminal or a copied screenshot.
            Event::Code(code) => {
                let style = self.tinted(self.palette.markdown_code);
                self.push_atom(format!("`{code}`"), style);
            }
            // Every remaining textual arm exists so nothing is silently dropped: raw
            // HTML a model emitted, a math span the parser did not expand, a footnote
            // marker without the extension. Rendering the source verbatim is worse
            // typography and better behaviour than losing the sentence.
            Event::Html(text) | Event::InlineHtml(text) => {
                let style = self.tinted(self.palette.text_muted);
                for line in text.trim_end_matches('\n').split('\n') {
                    self.push_atom(line.to_owned(), style);
                    self.inline.push(Token::HardBreak);
                }
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let style = self.tinted(self.palette.markdown_code);
                self.push_atom(format!("${text}$"), style);
            }
            Event::FootnoteReference(label) => {
                let style = self.tinted(self.palette.markdown_link);
                self.push_atom(format!("[^{label}]"), style);
            }
            Event::SoftBreak => self.inline.push(Token::Space),
            Event::HardBreak => self.inline.push(Token::HardBreak),
            Event::Rule => {
                self.flush_inline();
                self.rule();
            }
            Event::TaskListMarker(checked) => self.task_marker(checked),
        }
    }

    /// Emit whatever inline content is pending, because a new block is starting.
    ///
    /// Every block-opening tag calls this, and a nested list is why. A tight list nests
    /// as `Item · Text · List · Item · Text`, with no paragraph wrapper: the inner
    /// `List` arrives while the outer item's own text is still in the accumulator. Without
    /// this flush all three levels' text merged into one string and rendered at the
    /// deepest indent — measured as `- one / - two / - three` becoming
    /// `    • onetwothree` with two empty bullets under it.
    fn flush_inline(&mut self) {
        let tokens = self.take_inline();
        self.emit_inline(tokens);
    }

    fn start(&mut self, tag: Tag<'_>) {
        if !matches!(tag, Tag::Emphasis | Tag::Strong | Tag::Strikethrough)
            && !matches!(tag, Tag::Link { .. } | Tag::Image { .. })
            && !matches!(tag, Tag::TableCell | Tag::TableRow | Tag::TableHead)
        {
            self.flush_inline();
        }
        match tag {
            Tag::Paragraph => {
                // A paragraph inside a list item is the item's own text, so it gets no
                // gap above it; between blocks it does. Without the distinction every
                // loose list would render double-spaced.
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            Tag::Heading { level, .. } => self.open_heading(level),
            Tag::BlockQuote(kind) => {
                self.blank();
                self.quote_depth += 1;
                // GitHub's `> [!NOTE]` alerts carry their kind in the tag rather than in
                // the text, so the label has to be re-emitted or the reader loses it.
                if let Some(kind) = kind {
                    let label = match kind {
                        BlockQuoteKind::Note => "Note",
                        BlockQuoteKind::Tip => "Tip",
                        BlockQuoteKind::Important => "Important",
                        BlockQuoteKind::Warning => "Warning",
                        BlockQuoteKind::Caution => "Caution",
                    };
                    let style = self
                        .tinted(self.palette.markdown_block_quote)
                        .add_modifier(Modifier::BOLD);
                    self.push_atom(label.to_owned(), style);
                    self.inline.push(Token::HardBreak);
                }
            }
            Tag::CodeBlock(kind) => {
                self.blank();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|word| !word.is_empty())
                        .map(str::to_owned),
                    CodeBlockKind::Indented => None,
                };
                self.fence = Some(Fence {
                    language,
                    text: String::new(),
                });
            }
            Tag::List(start) => self.open_list(start),
            Tag::Item => self.open_item(),
            Tag::Emphasis => {
                let color = self.palette.markdown_emph;
                self.push_style(|style| style.fg(color.into()).add_modifier(Modifier::ITALIC));
            }
            Tag::Strong => {
                let color = self.palette.markdown_strong;
                self.push_style(|style| style.fg(color.into()).add_modifier(Modifier::BOLD));
            }
            Tag::Strikethrough => {
                self.push_style(|style| style.add_modifier(Modifier::CROSSED_OUT));
            }
            // Plan §7.1 renders a link as `[label](url)`. The destination is *not*
            // dropped in favour of the label alone: a terminal cannot be clicked, so a
            // reply that says "see the plan" with the path hidden has told the reader
            // nothing they can act on. The URL is stashed until `TagEnd::Link`, because
            // the label's own events arrive between the two.
            Tag::Link { dest_url, .. } => {
                let color = self.palette.markdown_link_text;
                self.push_style(|style| style.fg(color.into()).add_modifier(Modifier::UNDERLINED));
                self.push_atom(String::from("["), self.tinted(self.palette.markdown_link));
                self.destinations.push(dest_url.into_string());
            }
            Tag::Image { dest_url, .. } => {
                let color = self.palette.markdown_image_text;
                self.push_style(|style| style.fg(color.into()));
                self.push_atom(String::from("!["), self.tinted(self.palette.markdown_image));
                self.destinations.push(dest_url.into_string());
            }
            Tag::Table(_) => {
                self.blank();
                self.table = Some(Table::default());
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.body.push(Vec::new());
                }
            }
            Tag::TableCell => self.cell = Some(Vec::new()),
            // Definition lists, footnote bodies, metadata blocks and superscripts are
            // reached only with extensions this module does not enable, but the arms
            // exist so a future `Options` change cannot silently swallow a block.
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_)
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_inline();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Heading(_) => self.close_heading(),
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank();
            }
            TagEnd::CodeBlock => self.close_fence(),
            TagEnd::List(_) => self.close_list(),
            TagEnd::Item => self.close_item(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => self.close_destination(self.palette.markdown_link),
            TagEnd::Image => self.close_destination(self.palette.markdown_image),
            TagEnd::Table => self.close_table(),
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = false;
                }
            }
            TagEnd::TableRow => {}
            TagEnd::TableCell => {
                let cell = self.cell.take().unwrap_or_default();
                let inline = self.take_inline();
                let mut cell = cell;
                cell.extend(inline);
                if let Some(table) = self.table.as_mut() {
                    if table.in_head {
                        table.head.push(cell);
                    } else if let Some(row) = table.body.last_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    /// Flush whatever the stream left open and return the rows.
    ///
    /// A truncated document is the normal case while a reply streams, so the tail is
    /// closed here rather than treated as malformed: an unclosed fence is framed, an
    /// unclosed table is drawn, and a bare paragraph is emitted.
    fn finish(mut self) -> Vec<Row> {
        if self.fence.is_some() {
            self.close_fence();
        }
        if self.table.is_some() {
            self.close_table();
        }
        self.flush_inline();
        while self.rows.last().is_some_and(|row| row_width(row) == 0) {
            self.rows.pop();
        }
        self.rows
    }
}

/// Collapse a cell's tokens into spans on one line.
fn flatten(cell: &Cell) -> Row {
    let mut out = Vec::new();
    for token in cell {
        match token {
            Token::Word { text, style } => out.push(Span::styled(text.clone(), *style)),
            // A cell is one line by construction, so both separators are a space.
            Token::Space | Token::HardBreak => {
                if !out.is_empty() {
                    out.push(Span::raw(String::from(" ")));
                }
            }
        }
    }
    out
}

/// Break `tokens` into rows no wider than `width` columns.
///
/// Greedy, first-fit, on space boundaries — the same policy as
/// [`super::message::wrap`], so a markdown paragraph and a plain one break in the same
/// places and the transcript does not look like two renderers took turns.
///
/// Three properties this has to hold and a simpler loop would not:
///
/// * A leading space never starts a row, so a wrapped sentence does not appear indented
///   by one column at random.
/// * A word wider than the whole row is broken at cluster boundaries rather than
///   overflowing, because paths, URLs and CJK prose all reach here and none of them
///   carry a break opportunity.
/// * When the row cannot hold even one cluster the cluster is emitted anyway. One
///   column of overflow is an artefact the terminal absorbs; consuming zero bytes is a
///   hung TUI, which is the trade already made and documented in `wrap`.
fn lay_out(tokens: &[Token], width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut rows: Vec<Row> = Vec::new();
    let mut row: Row = Vec::new();
    let mut used = 0_usize;
    let mut pending_space = false;

    for token in tokens {
        match token {
            Token::HardBreak => {
                rows.push(std::mem::take(&mut row));
                used = 0;
                pending_space = false;
            }
            Token::Space => {
                if !row.is_empty() {
                    pending_space = true;
                }
            }
            Token::Word { text, style } => {
                if text.is_empty() {
                    continue;
                }
                let cost = display_width(text);
                let gap = usize::from(pending_space);
                if used + gap + cost <= width {
                    if pending_space {
                        row.push(Span::styled(String::from(" "), *style));
                        used += 1;
                    }
                    row.push(Span::styled(text.clone(), *style));
                    used += cost;
                    pending_space = false;
                    continue;
                }
                pending_space = false;
                if cost <= width {
                    if !row.is_empty() {
                        rows.push(std::mem::take(&mut row));
                    }
                    row.push(Span::styled(text.clone(), *style));
                    used = cost;
                    continue;
                }
                let mut rest = text.as_str();
                while !rest.is_empty() {
                    let room = width.saturating_sub(used);
                    let head = truncate_clusters(rest, room);
                    if head.is_empty() {
                        if used == 0 {
                            let (first, tail) = split_first_cluster(rest);
                            used += display_width(&first);
                            row.push(Span::styled(first, *style));
                            rest = tail;
                        } else {
                            rows.push(std::mem::take(&mut row));
                            used = 0;
                        }
                        continue;
                    }
                    let taken = head.len();
                    used += display_width(&head);
                    row.push(Span::styled(head, *style));
                    rest = &rest[taken..];
                    if !rest.is_empty() {
                        rows.push(std::mem::take(&mut row));
                        used = 0;
                    }
                }
            }
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod markdown_tests;
