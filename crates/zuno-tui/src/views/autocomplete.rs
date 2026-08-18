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
use crate::views::{ViewContext, display_width, fill, padded, truncate};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
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
    /// The glyph shown beside a candidate of this kind.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Command => "/",
            Self::File => "≡",
            Self::Directory => "▸",
            Self::Agent => "@",
            Self::Reference => "◈",
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
                if activation.trigger == Trigger::Command && score < 500 {
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

    /// Rows the popup needs.
    #[must_use]
    pub fn height(&self) -> u16 {
        u16::try_from(self.matches.len().min(self.visible_rows)).unwrap_or(u16::MAX)
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
                    format!(" {} {}", candidate.kind.glyph(), candidate.display)
                } else {
                    format!(
                        " {} {}  {}",
                        candidate.kind.glyph(),
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
        if !self.is_open() {
            return;
        }
        fill(frame.buffer_mut(), area, self.context.element());
        Paragraph::new(self.lines(area.width))
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
        if !self.is_active() {
            return 0;
        }
        let ceiling = (available / 2).max(1);
        let wanted = u16::try_from(self.prefix.continuations.len()).unwrap_or(u16::MAX);
        wanted.min(ceiling)
    }

    /// The grid: how many columns of what width to use for `entries` over `rows`.
    ///
    /// Takes the entry count, and that is the whole point. Packing the width full of
    /// minimum-width columns instead produced seven 14-column cells on a 100-column
    /// frame, which cut every description to eleven characters — nine rows reading
    /// `Switch to s`, a panel that names keys and explains nothing. So: use the fewest
    /// columns that hold the entries, then spend the leftover width on making them
    /// legible. Same rule as `§7.1`'s degradation order — content before decoration.
    fn plan_columns(width: u16, rows: u16, entries: usize) -> (u16, u16) {
        if width < WHICH_KEY_MIN_CELL || rows == 0 {
            return (1, width);
        }
        let fits = (width / WHICH_KEY_MIN_CELL).max(1);
        let needed = entries.div_ceil(usize::from(rows).max(1));
        let columns = u16::try_from(needed).unwrap_or(u16::MAX).clamp(1, fits);
        let cell = (width / columns).min(WHICH_KEY_MAX_CELL);
        (columns, cell)
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
        if !self.is_active() || area.width == 0 || area.height == 0 {
            return;
        }
        fill(frame.buffer_mut(), area, self.context.element());

        let rows = area.height;
        let total = self.prefix.continuations.len();
        let (columns, cell) = Self::plan_columns(area.width, rows, total);
        let capacity = usize::from(rows) * usize::from(columns);
        // The last cell becomes a count when the grid cannot hold everything, so the
        // panel never implies the leader has fewer continuations than it does.
        let shown = if total > capacity {
            capacity.saturating_sub(1)
        } else {
            total
        };

        for row in 0..rows {
            let mut spans = Vec::new();
            for column in 0..columns {
                let index = usize::from(row) + usize::from(column) * usize::from(rows);
                let text = if index < shown {
                    let entry = &self.prefix.continuations[index];
                    let keys = truncate(&entry.keys, usize::from(cell).saturating_sub(2));
                    let used = display_width(&keys);
                    let room = usize::from(cell).saturating_sub(used + 2);
                    format!(" {keys} {}", truncate(entry.definition.description, room))
                } else if index == shown && total > capacity {
                    format!(" +{} more", total - shown)
                } else {
                    String::new()
                };
                let style = if index < shown {
                    self.context.accent()
                } else {
                    self.context.muted()
                };
                spans.extend(padded(&text, cell, style).spans);
            }
            let region = Rect {
                x: area.x,
                y: area.y + row,
                width: area.width,
                height: 1,
            };
            Paragraph::new(vec![Line::from(spans)])
                .style(self.context.element())
                .render(region, frame.buffer_mut());
        }
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}
