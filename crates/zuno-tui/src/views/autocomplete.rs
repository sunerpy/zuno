//! Autocomplete for the prompt: slash commands, `@` files, agents, and references.
//!
//! # Two triggers, and the rules for each are the oracle's
//!
//! `packages/tui/src/component/prompt/autocomplete.tsx:643-706`:
//!
//! - **`/`** opens only at column zero with no whitespace before the cursor
//!   (`:696`), and closes once the query grows past `\S+\s+\S+\s*` (`:684`) — a
//!   slash command has one word, so a second word means the user moved on to prose.
//! - **`@`** opens at the nearest `@` before the cursor with no whitespace between
//!   (`:702-705`), which is what lets `@src/main.rs` complete while `hi @ there`
//!   does not.
//!
//! Both are ported here as pure functions over `(text, cursor)`, so the trigger rule
//! is testable without an editor, a terminal, or a file system.
//!
//! # Sources are a trait, because two of the three are I/O
//!
//! Files come from a disk walk and agents from the agent registry. Neither belongs
//! in a render path, and neither may be required by a test. [`CompletionSource`] is
//! the seam; [`StaticSource`] is the test double, while [`SlashSource`] projects the
//! runtime command router used by the production prompt.
//!
//! # Ranking is a subsequence match with a prefix bonus
//!
//! Upstream uses Fuse.js with `threshold: 0.5` for `@` and `0` — exact — for `/`
//! (`autocomplete.tsx:507-510`). Fuse's scoring is not reproducible from its
//! configuration, so this ranks by a documented rule instead: a prefix match beats a
//! word-boundary match beats a scattered subsequence, ties broken by the candidate's
//! own order. Deterministic, which is what a test can assert; the exactness of `/`
//! is preserved by requiring a prefix match for that trigger.

use crate::app::{AppEvent, Component, EventResult, TerminalEvent};
use crate::keybind::{Definition, PendingPrefix};
use crate::views::slash::SlashRouter;
use crate::views::{ViewContext, display_width, fill, hint, padded, truncate};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[cfg(test)]
#[path = "autocomplete_tests.rs"]
mod tests;

/// Which trigger opened the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A slash command at the start of the prompt.
    Command,
    /// An `@` reference: a file, an agent, or an MCP resource.
    Reference,
}

/// What kind of thing a candidate is, which decides its glyph and its insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// A slash command.
    Command,
    /// A file path.
    File,
    /// A directory, which re-triggers completion rather than finishing it.
    Directory,
    /// An agent.
    Agent,
    /// A named reference or MCP resource.
    Reference,
}

impl CandidateKind {
    /// The marker naming this kind, or `None` when `display` already opens with it.
    ///
    /// A command is built as `/{name}` and an agent as `@{name}`, so emitting the marker
    /// for those two printed the sigil twice: `/mcp` rendered as ` / /mcp`.
    #[must_use]
    pub const fn marker(self) -> Option<&'static str> {
        match self {
            Self::Command | Self::Agent => None,
            Self::File => Some("≡"),
            Self::Directory => Some("▸"),
            Self::Reference => Some("◈"),
        }
    }

    /// The marker column's contents, a space when there is no marker.
    ///
    /// The column is spent either way: dropping it for some rows would step their
    /// `display` one column left of the rest of the same list.
    #[must_use]
    pub const fn marker_cell(self) -> &'static str {
        match self.marker() {
            Some(marker) => marker,
            None => " ",
        }
    }
}

/// One completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What the user sees.
    pub display: String,
    /// What is inserted when it is chosen.
    pub insert: String,
    /// Secondary text.
    pub description: String,
    /// What it is.
    pub kind: CandidateKind,
    search: Vec<String>,
}

impl Candidate {
    /// A candidate whose display text is also what gets inserted.
    #[must_use]
    pub fn new(display: impl Into<String>, kind: CandidateKind) -> Self {
        let display = display.into();
        Self {
            insert: display.clone(),
            display,
            description: String::new(),
            kind,
            search: Vec::new(),
        }
    }

    /// Attach a description.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Override the inserted text.
    #[must_use]
    pub fn inserting(mut self, insert: impl Into<String>) -> Self {
        self.insert = insert.into();
        self
    }

    fn searching(mut self, terms: impl IntoIterator<Item = String>) -> Self {
        self.search.extend(terms);
        self
    }
}

/// Where candidates come from.
///
/// One method rather than one per kind: the query and the trigger together decide
/// what is relevant, and a source that wants to answer differently for `/` and `@`
/// can branch on the trigger itself.
pub trait CompletionSource: Send {
    /// Candidates for `query` under `trigger`, unranked.
    fn candidates(&self, trigger: Trigger, query: &str) -> Vec<Candidate>;
}

/// A source over a fixed list used by tests and host-projected reference data.
#[derive(Debug, Default, Clone)]
pub struct StaticSource {
    commands: Vec<Candidate>,
    references: Vec<Candidate>,
}

impl StaticSource {
    /// An empty source.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            commands: Vec::new(),
            references: Vec::new(),
        }
    }

    /// Add a slash command.
    #[must_use]
    pub fn command(mut self, name: &str, description: &str) -> Self {
        self.commands.push(
            Candidate::new(format!("/{name}"), CandidateKind::Command)
                .described(description)
                .inserting(format!("/{name} ")),
        );
        self
    }

    /// Add an agent.
    #[must_use]
    pub fn agent(mut self, name: &str, description: &str) -> Self {
        self.references.push(
            Candidate::new(format!("@{name}"), CandidateKind::Agent)
                .described(description)
                .inserting(format!("@{name} ")),
        );
        self
    }

    /// Add a file.
    #[must_use]
    pub fn file(mut self, path: &str) -> Self {
        self.references
            .push(Candidate::new(path, CandidateKind::File).inserting(format!("@{path} ")));
        self
    }

    /// Add a directory, whose completion appends a separator and re-triggers.
    #[must_use]
    pub fn directory(mut self, path: &str) -> Self {
        self.references.push(
            Candidate::new(format!("{path}/"), CandidateKind::Directory)
                .inserting(format!("@{path}/")),
        );
        self
    }
}

impl CompletionSource for StaticSource {
    fn candidates(&self, trigger: Trigger, _query: &str) -> Vec<Candidate> {
        match trigger {
            Trigger::Command => self.commands.clone(),
            Trigger::Reference => self.references.clone(),
        }
    }
}

/// Production slash candidates projected from the merged UI/catalog router.
#[derive(Debug, Clone)]
pub struct SlashSource {
    router: SlashRouter,
}

impl SlashSource {
    /// Build the production source over `router`.
    #[must_use]
    pub const fn new(router: SlashRouter) -> Self {
        Self { router }
    }
}

impl CompletionSource for SlashSource {
    fn candidates(&self, trigger: Trigger, _query: &str) -> Vec<Candidate> {
        if trigger != Trigger::Command {
            return Vec::new();
        }
        self.router
            .commands()
            .iter()
            .map(|command| {
                Candidate::new(format!("/{}", command.name), CandidateKind::Command)
                    .described(&command.description)
                    .inserting(format!("/{} ", command.name))
                    .searching(command.aliases.clone())
            })
            .collect()
    }
}

/// A detected trigger and the query it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    /// Which trigger fired.
    pub trigger: Trigger,
    /// The query text after the trigger character.
    pub query: String,
    /// Character offset of the trigger character itself, so a completion knows what
    /// to replace.
    pub start: usize,
}

/// Decide whether autocomplete should be open for `text` with the cursor at
/// `cursor` characters in.
///
/// The rules are cited in the module docs and are the reason this is a free function:
/// it is the part worth testing exhaustively, and it needs neither state nor I/O.
#[must_use]
pub fn detect(text: &str, cursor: usize) -> Option<Activation> {
    let characters: Vec<char> = text.chars().collect();
    let cursor = cursor.min(characters.len());
    let before: String = characters[..cursor].iter().collect();

    if before.starts_with('/') && !before[1..].contains(char::is_whitespace) {
        return Some(Activation {
            trigger: Trigger::Command,
            query: before[1..].to_owned(),
            start: 0,
        });
    }
    // `autocomplete.tsx:702-705`: the nearest `@` before the cursor, with no
    // whitespace between it and the cursor.
    let at = characters[..cursor]
        .iter()
        .rposition(|character| *character == '@')?;
    if characters[at + 1..cursor]
        .iter()
        .any(|character| character.is_whitespace())
    {
        return None;
    }
    // An `@` must start a token: `user@host` is an address, not a reference.
    if at > 0 && !characters[at - 1].is_whitespace() {
        return None;
    }
    Some(Activation {
        trigger: Trigger::Reference,
        query: characters[at + 1..cursor].iter().collect(),
        start: at,
    })
}

/// How well `candidate` matches `query`, higher is better, `None` for no match.
///
/// An empty query matches everything at the lowest score, which keeps the list
/// visible the moment the trigger is typed.
#[must_use]
pub fn score(candidate: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(1);
    }
    let haystack = candidate.to_lowercase();
    let needle = query.to_lowercase();
    if haystack.starts_with(&needle) {
        return Some(1000);
    }
    // A word-boundary hit: the query starts a path segment or a word.
    if haystack
        .split(['/', '-', '_', '.', ' '])
        .any(|segment| segment.starts_with(&needle))
    {
        return Some(500);
    }
    if haystack.contains(&needle) {
        return Some(250);
    }
    // Scattered subsequence, the loosest accepted match.
    let mut characters = haystack.chars();
    for wanted in needle.chars() {
        characters.find(|character| *character == wanted)?;
    }
    Some(10)
}

/// The literal spaces a candidate row spends on separation: one leading, one after the
/// marker, and two between the display and its description.
const OVERLAY_ROW_PADDING: usize = 4;

/// Rows the popup spends on its own hint line.
const OVERLAY_HINT_ROWS: u16 = 1;

/// The separation columns [`hint`] puts around one `(key, label)` pair.
///
/// One space between the key and its label, two after it. Kept beside the only reader rather
/// than inferred at the call site, because it is a property of `hint` and a stale copy here
/// under-measures the row and clips the last pair.
const HINT_PAIR_PADDING: usize = 3;

/// The narrowest popup worth centring, matching `§11.4`'s floor for a readable list.
///
/// Below this the popup takes the whole of `main` instead — see
/// [`AutocompleteView::overlay_frame`], which clamps rather than refusing to draw.
const OVERLAY_MIN_COLS: u16 = 30;

/// The autocomplete popup.
pub struct AutocompleteView {
    context: ViewContext,
    source: Box<dyn CompletionSource>,
    reference_source: Option<Box<dyn CompletionSource>>,
    activation: Option<Activation>,
    matches: Vec<Candidate>,
    cursor: usize,
    /// Rows shown at once (`autocomplete.tsx` caps its list similarly).
    visible_rows: usize,
}

impl AutocompleteView {
    /// A popup over `source`.
    #[must_use]
    pub fn new(context: ViewContext, source: Box<dyn CompletionSource>) -> Self {
        Self {
            context,
            source,
            reference_source: None,
            activation: None,
            matches: Vec::new(),
            cursor: 0,
            visible_rows: 10,
        }
    }

    /// Replace the slash-command source without disturbing host-projected references.
    ///
    /// The two sources have different owners: the screen rebuilds its command router when
    /// catalog commands arrive, while the CLI owns the prebuilt filesystem index. Replacing
    /// the whole view here would silently discard that index whenever those operations occur
    /// in the opposite order.
    pub fn set_source(&mut self, source: Box<dyn CompletionSource>) {
        self.source = source;
    }

    /// Install the host-owned source used for `@` completion.
    pub fn set_reference_source(&mut self, source: Box<dyn CompletionSource>) {
        self.reference_source = Some(source);
    }

    /// Whether the popup is showing.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.activation.is_some() && !self.matches.is_empty()
    }

    /// The ranked matches.
    #[must_use]
    pub fn matches(&self) -> &[Candidate] {
        &self.matches
    }

    /// The highlighted row.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The active trigger, when open.
    #[must_use]
    pub fn activation(&self) -> Option<&Activation> {
        self.activation.as_ref()
    }

    /// Re-evaluate the trigger and refresh the list for the editor's current state.
    ///
    /// Called on every keystroke, which is why detection is cheap and ranking is a
    /// single pass.
    pub fn refresh(&mut self, text: &str, cursor: usize) {
        let Some(activation) = detect(text, cursor) else {
            self.activation = None;
            self.matches.clear();
            self.cursor = 0;
            return;
        };
        // `autocomplete.tsx:684`: a slash command is one word, so a second word ends
        // the completion rather than filtering it further.
        if activation.trigger == Trigger::Command && activation.query.contains(char::is_whitespace)
        {
            self.activation = None;
            self.matches.clear();
            return;
        }
        let source = if activation.trigger == Trigger::Reference {
            self.reference_source.as_deref().unwrap_or(&*self.source)
        } else {
            &*self.source
        };
        let candidates = source.candidates(activation.trigger, &activation.query);
        let mut ranked = candidates
            .into_iter()
            .filter_map(|candidate| {
                let against = match activation.trigger {
                    // A command's display carries the leading `/`, which the query
                    // does not.
                    Trigger::Command => candidate.display.trim_start_matches('/').to_owned(),
                    Trigger::Reference => candidate.display.trim_start_matches('@').to_owned(),
                };
                let mut match_score = score(&against, &activation.query);
                if activation.trigger == Trigger::Command {
                    match_score = candidate
                        .search
                        .iter()
                        .chain(std::iter::once(&candidate.description))
                        .filter_map(|term| score(term, &activation.query))
                        .chain(match_score)
                        .max();
                }
                let score = match_score?;
                // The exact-match requirement upstream gives `/` (`threshold: 0`)
                // becomes "must be a prefix": a slash command the user half-typed
                // should not match by scattered letters.
                if activation.trigger == Trigger::Command
                    && !activation.query.is_empty()
                    && score < 500
                {
                    return None;
                }
                Some((score, candidate))
            })
            .collect::<Vec<_>>();
        // A stable sort keeps the source's own order for equal scores, so a
        // deliberate ordering upstream survives ranking.
        ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.matches = ranked.into_iter().map(|(_, candidate)| candidate).collect();
        if activation.trigger == Trigger::Command {
            self.matches.truncate(10);
        }
        self.cursor = self.cursor.min(self.matches.len().saturating_sub(1));
        self.activation = Some(activation);
    }

    /// Close the popup.
    pub fn hide(&mut self) {
        self.activation = None;
        self.matches.clear();
        self.cursor = 0;
    }

    /// The highlighted candidate.
    #[must_use]
    pub fn selected(&self) -> Option<&Candidate> {
        self.matches.get(self.cursor)
    }

    /// Apply the highlighted candidate to `text`, returning the new text and cursor.
    ///
    /// Replacement rather than insertion: the trigger and the partial query the user
    /// typed are consumed, which is what makes tab-completion idempotent.
    #[must_use]
    pub fn complete(&self, text: &str) -> Option<(String, usize)> {
        let activation = self.activation.as_ref()?;
        let candidate = self.selected()?;
        let characters: Vec<char> = text.chars().collect();
        let head: String = characters[..activation.start].iter().collect();
        let consumed = activation.start + 1 + activation.query.chars().count();
        let tail: String = characters[consumed.min(characters.len())..]
            .iter()
            .collect();
        let inserted = &candidate.insert;
        let cursor = activation.start + inserted.chars().count();
        Some((format!("{head}{inserted}{tail}"), cursor))
    }

    /// Act on one resolved binding. Returns whether the popup consumed it.
    pub fn handle_action(&mut self, action: &'static Definition) -> AutocompleteStep {
        if !self.is_open() {
            return AutocompleteStep::Ignored;
        }
        match action.name {
            "prompt.autocomplete.prev" => {
                self.cursor = (self.cursor + self.matches.len() - 1) % self.matches.len();
                AutocompleteStep::Redraw
            }
            "prompt.autocomplete.next" => {
                self.cursor = (self.cursor + 1) % self.matches.len();
                AutocompleteStep::Redraw
            }
            "prompt.autocomplete.hide" => {
                self.hide();
                AutocompleteStep::Redraw
            }
            "prompt.autocomplete.select" | "prompt.autocomplete.complete" => {
                AutocompleteStep::Complete
            }
            _ => AutocompleteStep::Ignored,
        }
    }

    /// Highlight the candidate painted under one absolute pointer coordinate.
    ///
    /// The final overlay row is the hint bar and is intentionally not selectable.
    pub fn select_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        if !self.is_open()
            || column < area.left()
            || column >= area.right()
            || row < area.top()
            || row >= area.bottom().saturating_sub(OVERLAY_HINT_ROWS)
        {
            return false;
        }
        let target = usize::from(row.saturating_sub(area.top()));
        let first = self
            .cursor
            .saturating_sub(self.visible_rows.saturating_sub(1));
        let index = first.saturating_add(target);
        if index >= self.matches.len() || target >= self.visible_rows {
            return false;
        }
        self.cursor = index;
        true
    }

    /// Rows the popup needs, its hint row included.
    ///
    /// The hint row is counted here rather than added by the caller because this is what
    /// "how tall is the popup" has to mean: a caller that sized a frame from a list-only
    /// count would hand the popup one row too few, and `render` would drop a candidate to
    /// make room for the hints — losing content to chrome without saying so.
    #[must_use]
    pub fn height(&self) -> u16 {
        self.list_height().saturating_add(OVERLAY_HINT_ROWS)
    }

    /// Rows the candidate list alone occupies.
    fn list_height(&self) -> u16 {
        u16::try_from(self.matches.len().min(self.visible_rows)).unwrap_or(u16::MAX)
    }

    /// The keys the hint row advertises, and what each one does.
    ///
    /// One list, read by both the row and [`Self::content_width`]. A width computed from a
    /// second copy is a width that stops matching the row when one of them is edited, and the
    /// symptom is a clipped hint — measured as `esc dis` on a 120-column frame.
    const HINTS: [(&'static str, &'static str); 3] =
        [("↑↓", "move"), ("tab", "complete"), ("esc", "dismiss")];

    /// The hint row shown along the popup's bottom edge.
    ///
    /// Built from the same [`hint`] helper every dialog footer uses, so the keys are spelled
    /// once and cannot drift between the two surfaces.
    #[must_use]
    pub fn hint_row(&self, width: u16) -> Line<'static> {
        let mut spans = Vec::new();
        for (key, label) in Self::HINTS {
            spans.extend(hint(key, label, &self.context));
        }
        let used = u16::try_from(
            spans
                .iter()
                .map(|span| display_width(&span.content))
                .sum::<usize>(),
        )
        .unwrap_or(u16::MAX);
        // Padded to the popup's own width so the hint row carries the popup's background
        // across its whole edge rather than letting the frame behind it show through the
        // tail of the row.
        spans.push(ratatui::text::Span::styled(
            " ".repeat(usize::from(width.saturating_sub(used))),
            self.context.element(),
        ));
        Line::from(spans)
    }

    /// Columns the widest row the popup will draw wants, hint row included.
    ///
    /// The hint row is measured too, and that is not incidental: sized from the candidates
    /// alone, a popup listing one short command came out narrower than its own hints and the
    /// last of them rendered as `esc dis` on a 120-column frame. A surface that clips the row
    /// explaining it is worse than one with no hints, because a half-spelled key reads as a key.
    fn content_width(&self) -> u16 {
        let candidates = self
            .matches
            .iter()
            .take(self.visible_rows)
            .map(|candidate| {
                // The literal spaces of `" {marker} {display}  {description}"`, in terminal
                // columns rather than characters so a CJK description is measured as the cells
                // it occupies.
                let marker = display_width(candidate.kind.marker_cell());
                let body =
                    display_width(&candidate.display) + display_width(&candidate.description);
                marker + body + OVERLAY_ROW_PADDING
            })
            .max()
            .unwrap_or(0);
        let hints = Self::HINTS
            .iter()
            .map(|(key, label)| {
                // `hint` spells a pair as `" {key} {label} "`, so four columns of separation.
                display_width(key) + display_width(label) + HINT_PAIR_PADDING
            })
            .sum::<usize>();
        u16::try_from(candidates.max(hints)).unwrap_or(u16::MAX)
    }

    /// Where the popup floats inside `main`, and nothing else.
    ///
    /// A [`Rect`] rather than a [`ratatui::layout::Constraint`], and that is the whole
    /// contract: the popup opens and closes on a keystroke, so a popup that took part in
    /// the screen's vertical split would reflow the transcript on every character typed.
    /// It is drawn over `main` instead, after `main` has been painted.
    ///
    /// Centred on both axes. Vertically, because a list anchored to the bottom edge sat
    /// under the caret it was completing and read as part of the status strip. Horizontally
    /// and content-derived rather than full width, for the reason
    /// [`crate::views::dialog`] gives fixed tiers over fractions: a 200-column popup
    /// holding a nine-column command is a sparse band, not a list.
    ///
    /// Degradation at narrow widths is a clamp, never a refusal: the popup takes what is
    /// available once it cannot have [`OVERLAY_MIN_COLS`], because a completion list is the
    /// one surface that has to survive the pane a user has squeezed. Returns `None` only
    /// when `main` has no rows for it at all, which is the 20x10 case where the transcript
    /// region is a single row.
    #[must_use]
    pub fn overlay_frame(&self, main: Rect) -> Option<Rect> {
        if !self.is_open() || main.width == 0 || main.height == 0 {
            return None;
        }
        let height = self.height().min(main.height);
        if height == 0 {
            return None;
        }
        let width = self
            .content_width()
            .clamp(OVERLAY_MIN_COLS.min(main.width), main.width);
        Some(Rect {
            x: main.x + (main.width - width) / 2,
            y: main.y + (main.height - height) / 2,
            width,
            height,
        })
    }

    /// The rendered rows.
    #[must_use]
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let first = self
            .cursor
            .saturating_sub(self.visible_rows.saturating_sub(1));
        self.matches
            .iter()
            .enumerate()
            .skip(first)
            .take(self.visible_rows)
            .map(|(index, candidate)| {
                let style = if index == self.cursor {
                    self.context.selected()
                } else {
                    self.context.element()
                };
                let body = if candidate.description.is_empty() {
                    format!(" {} {}", candidate.kind.marker_cell(), candidate.display)
                } else {
                    format!(
                        " {} {}  {}",
                        candidate.kind.marker_cell(),
                        candidate.display,
                        candidate.description
                    )
                };
                padded(&body, width, style)
            })
            .collect()
    }
}

/// What the popup did with an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutocompleteStep {
    /// Not for the popup.
    Ignored,
    /// State changed.
    Redraw,
    /// The host should apply [`AutocompleteView::complete`].
    Complete,
}

impl Component for AutocompleteView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if !self.is_open() || area.width == 0 || area.height == 0 {
            return;
        }
        fill(frame.buffer_mut(), area, self.context.element());
        // The hint row is dropped rather than allowed to evict a candidate when the frame
        // is down to one row: the list is what the user opened, and a popup showing only
        // its own keys explains a list that is not there.
        let mut rows = self.lines(area.width);
        let list_rows = usize::from(area.height.saturating_sub(OVERLAY_HINT_ROWS));
        if area.height > OVERLAY_HINT_ROWS {
            rows.truncate(list_rows);
            rows.push(self.hint_row(area.width));
        }
        Paragraph::new(rows)
            .style(self.context.element())
            .render(area, frame.buffer_mut());
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

/// The widest a single which-key cell may be, including its key and gap.
///
/// A cap rather than "share the width equally": with three continuations a frame-wide
/// third of a 200-column terminal puts 60 columns of gap between a key and its
/// description, which reads as two unrelated lists.
const WHICH_KEY_MAX_CELL: u16 = 34;

/// The narrowest cell worth drawing: a key, a space, and something of a description.
const WHICH_KEY_MIN_CELL: u16 = 14;

/// The width at which a key and a useful description can normally coexist.
const WHICH_KEY_COMFORT_CELL: u16 = 24;

/// The largest leader-help overlay, leaving transcript context visible on wide terminals.
const WHICH_KEY_MAX_WIDTH: u16 = 96;

/// The title that distinguishes leader help from transcript or tool output.
const WHICH_KEY_TITLE: &str = " Next key ";

/// A which-key surface: the actions reachable from the pending leader sequence.
///
/// Autocomplete's neighbour rather than its own module because it is the same
/// affordance — a filtered list of what the next keystroke can do.
///
/// # Why this is a layer and not just a renderer
///
/// The panel has to *close* on the leader timeout. Following [`crate::views::toast`],
/// it holds its own deadline and arms one wake, and [`Self::render`] prunes before
/// drawing so a dropped wake still cannot leave a stale panel on screen. Polling was
/// rejected there for the same reason it is rejected here: a fourth redraw tier would
/// defeat the deep idle the scheduler exists to reach.
pub struct WhichKeyView {
    context: ViewContext,
    prefix: PendingPrefix,
    shown: Option<Instant>,
    timeout: Duration,
    waker: Option<mpsc::Sender<TerminalEvent>>,
}

impl WhichKeyView {
    /// A which-key surface over `context`.
    #[must_use]
    pub fn new(context: ViewContext) -> Self {
        let timeout = context.config.leader_timeout;
        Self {
            context,
            prefix: PendingPrefix::default(),
            shown: None,
            timeout,
            waker: None,
        }
    }

    /// Send expiry wakes on `waker`.
    #[must_use]
    pub fn with_waker(mut self, waker: mpsc::Sender<TerminalEvent>) -> Self {
        self.waker = Some(waker);
        self
    }

    /// Whether a prefix is in flight and the panel should be drawn.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.shown.is_some() && self.prefix.is_active()
    }

    /// Take the dispatcher's new prefix, reporting whether the screen changed.
    ///
    /// `now` is a parameter rather than `Instant::now()` so expiry is assertable without
    /// a sleep, matching [`crate::views::toast::ToastLayer::prune`].
    pub fn observe(&mut self, prefix: &PendingPrefix, now: Instant) -> bool {
        let was = self.is_active();
        if prefix.is_active() {
            self.prefix = prefix.clone();
            self.shown = Some(now);
            self.arm();
        } else {
            self.prefix = PendingPrefix::default();
            self.shown = None;
        }
        was != self.is_active()
    }

    /// Drop a prefix that has outlived the leader timeout by `now`.
    pub fn prune(&mut self, now: Instant) -> bool {
        let expired = self
            .shown
            .is_some_and(|shown| now.saturating_duration_since(shown) >= self.timeout);
        if expired {
            self.shown = None;
            self.prefix = PendingPrefix::default();
        }
        expired
    }

    /// Rows the panel wants in a frame `available` rows tall.
    ///
    /// Never more than half the frame: the panel exists to explain the next keystroke,
    /// and one that covers the transcript it is explaining has taken more than it gave.
    #[must_use]
    pub fn desired_height(&self, available: u16) -> u16 {
        self.desired_height_for(WHICH_KEY_MAX_WIDTH, available)
    }

    fn desired_height_for(&self, width: u16, available: u16) -> u16 {
        if !self.is_active() || available == 0 {
            return 0;
        }
        let ceiling = (available / 2).max(1).min(available);
        if ceiling < 3 {
            return ceiling;
        }
        let content_width = width.saturating_sub(2).max(1);
        let max_content_rows = ceiling.saturating_sub(2).max(1);
        let entries = self.prefix.continuations.len();
        let content_rows = (1..=max_content_rows)
            .find(|rows| {
                let (columns, _) = Self::plan_columns(content_width, *rows, entries);
                usize::from(*rows) * usize::from(columns) >= entries
            })
            .unwrap_or(max_content_rows);
        content_rows.saturating_add(2).min(ceiling)
    }

    /// The grid: how many columns of what width to use for `entries` over `rows`.
    ///
    /// Readability wins over silent completeness. A 100-column frame can technically fit
    /// six 14-column cells, but each would reduce `List all sessions` to `List all ses`.
    /// Limit the column count to cells with useful descriptive width and let the final cell
    /// state `+N more` when the available rows cannot carry every continuation.
    fn plan_columns(width: u16, rows: u16, entries: usize) -> (u16, u16) {
        if width < WHICH_KEY_MIN_CELL || rows == 0 {
            return (1, width);
        }
        let fits = (width / WHICH_KEY_MIN_CELL).max(1);
        let roomy = (width / WHICH_KEY_COMFORT_CELL).max(1);
        let needed = entries.div_ceil(usize::from(rows).max(1));
        let columns = u16::try_from(needed)
            .unwrap_or(u16::MAX)
            .clamp(1, fits)
            .min(roomy);
        let cell = (width / columns).min(WHICH_KEY_MAX_CELL);
        (columns, cell)
    }

    fn overlay_frame(&self, area: Rect) -> Option<Rect> {
        if !self.is_active() || area.width == 0 || area.height == 0 {
            return None;
        }
        let horizontal_gutter: u16 = if area.width >= WHICH_KEY_MIN_CELL.saturating_add(4) {
            2
        } else {
            0
        };
        let width = area
            .width
            .saturating_sub(horizontal_gutter.saturating_mul(2))
            .clamp(1, WHICH_KEY_MAX_WIDTH);
        let height = self.desired_height_for(width, area.height).max(1);
        Some(Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        })
    }

    fn render_grid(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        fill(frame.buffer_mut(), area, self.context.element());
        let rows = area.height;
        let total = self.prefix.continuations.len();
        let (columns, cell) = Self::plan_columns(area.width, rows, total);
        let capacity = usize::from(rows) * usize::from(columns);
        let shown = if total > capacity {
            capacity.saturating_sub(1)
        } else {
            total
        };
        let key_style = self
            .context
            .on_element(self.context.accent())
            .add_modifier(Modifier::BOLD);
        let description_style = self.context.on_element(self.context.text());
        let muted_style = self.context.on_element(self.context.muted());

        for row in 0..rows {
            let mut spans = Vec::new();
            for column in 0..columns {
                let index = usize::from(row) + usize::from(column) * usize::from(rows);
                if index < shown {
                    let entry = &self.prefix.continuations[index];
                    let keys = truncate(&entry.keys, usize::from(cell).saturating_sub(3));
                    let key = format!(" {keys}");
                    let key_width = display_width(&key);
                    let description_room = usize::from(cell).saturating_sub(key_width + 1);
                    let description = truncate(entry.definition.description, description_room);
                    let description = if description.is_empty() {
                        String::new()
                    } else {
                        format!(" {description}")
                    };
                    let used = key_width + display_width(&description);
                    spans.push(Span::styled(key, key_style));
                    spans.push(Span::styled(description, description_style));
                    spans.push(Span::styled(
                        " ".repeat(usize::from(cell).saturating_sub(used)),
                        self.context.element(),
                    ));
                } else if index == shown && total > capacity {
                    spans.extend(
                        padded(&format!(" +{} more", total - shown), cell, muted_style).spans,
                    );
                } else {
                    spans.extend(padded("", cell, self.context.element()).spans);
                }
            }
            Paragraph::new(vec![Line::from(spans)])
                .style(self.context.element())
                .render(
                    Rect {
                        x: area.x,
                        y: area.y + row,
                        width: area.width,
                        height: 1,
                    },
                    frame.buffer_mut(),
                );
        }
    }

    fn arm(&self) {
        let Some(waker) = self.waker.clone() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let timeout = self.timeout;
        handle.spawn(async move {
            tokio::time::sleep(timeout).await;
            let _dropped_when_busy = waker.try_send(TerminalEvent::Wake);
        });
    }
}

impl Component for WhichKeyView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.prune(Instant::now());
        let Some(panel) = self.overlay_frame(area) else {
            return;
        };
        if panel.width < 4 || panel.height < 4 {
            self.render_grid(frame, panel);
            return;
        }

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(self.context.on_element(self.context.accent()))
            .title(Line::from(Span::styled(
                WHICH_KEY_TITLE,
                self.context.on_element(self.context.title()),
            )))
            .style(self.context.element());
        let inner = block.inner(panel);
        block.render(panel, frame.buffer_mut());
        self.render_grid(frame, inner);
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}
