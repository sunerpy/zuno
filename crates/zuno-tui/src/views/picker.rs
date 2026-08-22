//! The picker family: session, model, agent, and theme.
//!
//! # One list, four callers
//!
//! Upstream ships a separate component per picker
//! (`component/dialog-session-list.tsx`, `dialog-model.tsx`, `dialog-agent.tsx`,
//! `dialog-theme-list.tsx`) over one shared `ui/dialog-select.tsx`. The shared part
//! is the whole behaviour — filter, cursor, paging, submit — so here there is one
//! [`SelectDialog`] and four constructors. Four copies of a list widget is four
//! places for the paging arithmetic to be wrong.
//!
//! # Filtering is the same ranking autocomplete uses
//!
//! [`crate::views::autocomplete::score`] rather than a second scoring rule, because a
//! user who learns that typing `sess` finds `session_list` in one surface should not
//! have to learn something different in the other.
//!
//! # The theme picker previews, and that is not cosmetic
//!
//! It renders todo 75's [`crate::theme::PaletteSampleView`] beside the list. A theme
//! name means nothing; a preview is the only way to choose one. Reusing that view
//! also means the picker is covered by the 33 committed palette snapshots.

use crate::keybind::Definition;
use crate::theme::{Mode, Resolved, ThemeRegistry};
use crate::views::autocomplete::score;
use crate::views::dialog::{Dialog, DialogOutcome, DialogStep};
use crate::views::{ViewContext, padded};
use crossterm::event::KeyEvent;
use ratatui::text::{Line, Span};
use std::sync::{Arc, PoisonError, RwLock};

#[cfg(test)]
#[path = "picker_tests.rs"]
mod tests;

/// The dialog id for the session picker.
pub const SESSION_DIALOG_ID: &str = "session_list";
/// The dialog id for the model picker.
pub const MODEL_DIALOG_ID: &str = "model_list";
/// The dialog id for the agent picker.
pub const AGENT_DIALOG_ID: &str = "agent_list";
/// The dialog id for the theme picker.
pub const THEME_DIALOG_ID: &str = "theme_list";
/// The dialog id for the MCP server list.
pub const MCP_DIALOG_ID: &str = "mcp_list";
/// The dialog id for the skill list.
pub const SKILL_DIALOG_ID: &str = "prompt_skills";

/// The MCP servers, as a filterable list.
///
/// A list and not a picker, strictly speaking: selecting a row does nothing, because an
/// MCP server is an ambient fact rather than a choice. It exists because the sidebar
/// shows a *summary* — `2 up, 1 failed` — and a failure's reason is what a user acts on,
/// which does not fit in the panel's remaining columns. The same [`SelectDialog`] is
/// reused rather than a bespoke view so that filtering by name behaves the way it does
/// everywhere else.
#[must_use]
pub fn mcp_list(context: ViewContext, servers: McpProjection) -> McpDialog {
    McpDialog::new(context, servers)
}

/// Runtime-neutral lifecycle state shown by the MCP dialog and sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpState {
    Disabled,
    Connecting,
    Connected,
    Disconnecting,
    Failed(String),
    NeedsAuth,
    NeedsClientRegistration(String),
}

/// Complete plain-data projection of one configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub state: McpState,
    pub desired_enabled: bool,
}

impl McpServer {
    fn detail(&self) -> String {
        match &self.state {
            McpState::Disabled => "○ Disabled".to_owned(),
            McpState::Connecting => "◐ Connecting".to_owned(),
            McpState::Connected => "● Connected".to_owned(),
            McpState::Disconnecting => "◐ Disconnecting".to_owned(),
            McpState::Failed(error) => format!("✗ Failed · {error}"),
            McpState::NeedsAuth => "✗ Needs authentication".to_owned(),
            McpState::NeedsClientRegistration(error) => {
                format!("✗ Needs client registration · {error}")
            }
        }
    }

    /// Project lifecycle detail onto the sidebar's compact health vocabulary.
    #[must_use]
    pub fn service(&self) -> crate::views::ambient::Service {
        use crate::views::ambient::{Health, Service};
        let health = match self.state {
            McpState::Connected => Health::Ready,
            McpState::Connecting | McpState::Disconnecting => Health::Pending,
            McpState::Disabled => Health::Disabled,
            McpState::Failed(_) | McpState::NeedsAuth | McpState::NeedsClientRegistration(_) => {
                Health::Faulted
            }
        };
        Service::new(self.name.clone(), health).detailed(self.detail())
    }
}

/// Explicit target emitted by the MCP dialog when Space is pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToggleRequest {
    pub server: String,
    pub desired_enabled: bool,
}

/// The servers plus the count of content changes they have been through.
///
/// One value behind one lock, so a reader that wants both cannot observe a generation
/// from before a replacement beside the servers from after it — which would make the
/// reader believe it had already painted the newer list.
#[derive(Debug, Default)]
struct Projected {
    generation: u64,
    servers: Vec<McpServer>,
}

/// Shared, atomically replaced MCP projection. Rendering performs no I/O.
///
/// # Why the generation exists
///
/// Both surfaces that state an MCP server's health already *derive* it from here: the
/// sidebar re-reads [`Self::snapshot`] at the top of every
/// [`crate::views::session::SessionScreen::render`], and [`McpDialog`] re-reads it inside
/// its own `lines`. So within any painted frame the two cannot disagree — and yet the
/// panel a user was looking at sat on `◐ Connecting` while the server had already timed
/// out, because **no frame was painted at all**. The lifecycle worker publishes a
/// replacement and nudges the loop with a [`crate::app::TerminalEvent::Wake`], and a wake
/// only reaches the terminal if some component reports `redraw` — which nothing did for a
/// projection change. Deriving the fact at one point is not enough; something has to
/// report that the derived fact moved.
///
/// The counter is what lets one observer answer that in constant time, and it advances
/// **only when the content actually differs**: the worker also replaces on a broadcast lag
/// and after every completed toggle, and a bump for an identical list would spend a frame
/// repainting bytes that did not change.
#[derive(Debug, Clone, Default)]
pub struct McpProjection(Arc<RwLock<Projected>>);

impl McpProjection {
    #[must_use]
    pub fn new(servers: Vec<McpServer>) -> Self {
        Self(Arc::new(RwLock::new(Projected {
            generation: 0,
            servers,
        })))
    }

    /// Publish `servers`, advancing the generation only if they differ from the current set.
    pub fn replace(&self, servers: Vec<McpServer>) {
        let mut projected = self.0.write().unwrap_or_else(PoisonError::into_inner);
        if projected.servers == servers {
            return;
        }
        projected.servers = servers;
        projected.generation = projected.generation.wrapping_add(1);
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<McpServer> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .servers
            .clone()
    }

    /// How many content changes this projection has published.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .generation
    }

    /// The generation and the servers it belongs to, under one read.
    ///
    /// Two separate calls could straddle a [`Self::replace`] and pair a stale generation
    /// with fresh servers, which is exactly the mistake that makes an observer record a
    /// frame it never painted.
    #[must_use]
    pub fn observe(&self) -> (u64, Vec<McpServer>) {
        let projected = self.0.read().unwrap_or_else(PoisonError::into_inner);
        (projected.generation, projected.servers.clone())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .servers
            .is_empty()
    }
}

/// Live MCP server list. Unlike ordinary pickers, Space emits without closing it.
pub struct McpDialog {
    context: ViewContext,
    servers: McpProjection,
    filter: String,
    cursor: usize,
    rows: usize,
}

impl McpDialog {
    fn new(context: ViewContext, servers: McpProjection) -> Self {
        Self {
            context,
            servers,
            filter: String::new(),
            cursor: 0,
            rows: 10,
        }
    }

    fn visible(&self) -> Vec<McpServer> {
        let mut ranked = self
            .servers
            .snapshot()
            .into_iter()
            .filter_map(|server| {
                let rank = score(&server.name, &self.filter)
                    .into_iter()
                    .chain(score(&server.detail(), &self.filter).map(|value| value / 2))
                    .max()?;
                Some((rank, server))
            })
            .collect::<Vec<_>>();
        ranked.sort_by_key(|(rank, server)| (std::cmp::Reverse(*rank), server.name.clone()));
        ranked.into_iter().map(|(_, server)| server).collect()
    }

    fn selected(&self) -> Option<McpServer> {
        self.visible().get(self.cursor).cloned()
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.visible().len().saturating_sub(1));
    }

    fn move_cursor(&mut self, delta: isize) {
        let length = self.visible().len();
        if length > 0 {
            self.cursor = ((self.cursor as isize + delta).rem_euclid(length as isize)) as usize;
        }
    }
}

impl Dialog for McpDialog {
    fn id(&self) -> &'static str {
        MCP_DIALOG_ID
    }

    fn title(&self) -> String {
        let count = self.visible().len();
        if self.filter.is_empty() {
            format!("MCP servers ({count})")
        } else {
            format!("MCP servers ({count}) — {}", self.filter)
        }
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        self.clamp_cursor();
        let visible = self.visible();
        if visible.is_empty() {
            return vec![padded(" no matches", width, self.context.muted())];
        }
        let first = self.cursor.saturating_sub(self.rows.saturating_sub(1));
        visible
            .iter()
            .enumerate()
            .skip(first)
            .take(self.rows)
            .map(|(position, server)| {
                let marker = if position == self.cursor { ">" } else { " " };
                let style = if position == self.cursor {
                    self.context.selected()
                } else {
                    self.context.text()
                };
                padded(
                    &format!(" {marker} {}  {}", server.name, server.detail()),
                    width,
                    style,
                )
            })
            .collect()
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓", "move"), ("space", "toggle"), ("esc", "close")]
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        vec!["dialog.mcp"]
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        match action.name {
            "dialog.select.prev" => {
                self.move_cursor(-1);
                DialogStep::Redraw
            }
            "dialog.select.next" => {
                self.move_cursor(1);
                DialogStep::Redraw
            }
            "dialog.select.home" => {
                self.cursor = 0;
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                self.cursor = self.visible().len().saturating_sub(1);
                DialogStep::Redraw
            }
            "dialog.select.page_up" => {
                self.move_cursor(-(self.rows as isize));
                DialogStep::Redraw
            }
            "dialog.select.page_down" => {
                self.move_cursor(self.rows as isize);
                DialogStep::Redraw
            }
            "dialog.mcp.toggle" => self.selected().map_or(DialogStep::Ignored, |server| {
                DialogStep::Emitted(DialogOutcome::McpToggle(McpToggleRequest {
                    server: server.name,
                    desired_enabled: !server.desired_enabled,
                }))
            }),
            "app_exit" | "session_interrupt" => DialogStep::Resolved(DialogOutcome::Cancelled),
            "input_backspace" => {
                self.filter.pop();
                self.clamp_cursor();
                DialogStep::Redraw
            }
            "dialog.select.submit" | "dialog.prompt.submit" => DialogStep::Ignored,
            _ => self.handle_typed(event),
        }
    }

    fn handle_typed(&mut self, key: &KeyEvent) -> DialogStep {
        if let Some(character) = crate::views::permission::typed_character(key) {
            self.filter.push(character);
            self.clamp_cursor();
            DialogStep::Redraw
        } else {
            DialogStep::Ignored
        }
    }
}

/// One row of a picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// What the user sees.
    pub label: String,
    /// Secondary text.
    pub description: String,
    /// The opaque value reported in [`DialogOutcome::Selected`].
    pub value: String,
    /// The heading this row belongs under, when the picker groups.
    ///
    /// Empty for every ungrouped picker, which is what makes grouping opt-in per
    /// constructor rather than a mode the list has to be told about.
    pub group: String,
}

impl Item {
    /// A row whose label is also its value.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            value: label.clone(),
            label,
            description: String::new(),
            group: String::new(),
        }
    }

    /// Put this row under the `group` heading.
    #[must_use]
    pub fn grouped(mut self, group: impl Into<String>) -> Self {
        self.group = group.into();
        self
    }

    /// Attach a description.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Override the reported value.
    #[must_use]
    pub fn valued(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self
    }
}

/// One heading's rows while [`SelectDialog::rank_groups`] orders them.
struct Group<'a> {
    name: &'a str,
    /// The best score any member scored, which is what orders the groups.
    best: u32,
    /// `(score, index)` per member, ordered as `items` holds them until sorted.
    members: Vec<(u32, usize)>,
}

/// A per-row preview renderer.
///
/// A named alias because it appears in a field, a builder parameter, and a boxed
/// value; spelled out three times it drifts.
pub type PreviewFn = dyn Fn(&Item, &ViewContext) -> Vec<Line<'static>> + Send;

/// A side effect run when the highlighted row changes.
///
/// Separate from [`PreviewFn`] even though both are handed the highlighted row,
/// because they fire at different times and only one of them may have effects. A
/// preview runs inside [`Dialog::lines`], which the host calls *after* it has already
/// drawn the base component, so a preview that re-themed the screen would leave the
/// transcript one keystroke behind the list. This runs while the keystroke is still
/// being handled, before anything is painted.
pub type HighlightFn = dyn Fn(&Item, &ViewContext) + Send;

/// A filterable list dialog.
pub struct SelectDialog {
    id: &'static str,
    heading: String,
    context: ViewContext,
    items: Vec<Item>,
    /// Indices into `items`, in ranked order.
    filtered: Vec<usize>,
    filter: String,
    cursor: usize,
    rows: usize,
    /// A per-row preview, drawn under the list. The theme picker's reason to exist.
    preview: Option<Box<PreviewFn>>,
    /// Run when the highlighted row changes. The theme picker's live switch.
    highlight: Option<Box<HighlightFn>>,
    /// The value [`Self::highlight`] was last told about.
    ///
    /// Compared against the current selection on the way out of every input entry
    /// point, which is what makes this derived rather than pushed: the cursor moves
    /// from six actions and the filter reorders the list from two more, and a hook
    /// fired at each of those eight sites is a hook the ninth forgets. One comparison
    /// at one place cannot be forgotten by a later action arm.
    ///
    /// The *value* and not the cursor index, because re-ranking can leave the index
    /// alone while putting a different row under it.
    announced: Option<String>,
}

impl SelectDialog {
    /// A picker over `items`.
    #[must_use]
    pub fn new(
        id: &'static str,
        heading: impl Into<String>,
        context: ViewContext,
        items: Vec<Item>,
    ) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            id,
            heading: heading.into(),
            context,
            items,
            filtered,
            filter: String::new(),
            cursor: 0,
            rows: 10,
            preview: None,
            highlight: None,
            announced: None,
        }
    }

    /// Attach a preview renderer for the highlighted row.
    #[must_use]
    pub fn with_preview(
        mut self,
        preview: impl Fn(&Item, &ViewContext) -> Vec<Line<'static>> + Send + 'static,
    ) -> Self {
        self.preview = Some(Box::new(preview));
        self
    }

    /// Run `highlight` whenever the highlighted row changes.
    ///
    /// Attach this **after** [`Self::selecting`]: the row the picker opens on is
    /// recorded as already announced, so opening a picker fires nothing. Attaching it
    /// first would make merely opening the theme picker repaint the screen with the
    /// theme it is already showing — harmless today, and the kind of thing that stops
    /// being harmless once a hook does more than one thing.
    #[must_use]
    pub fn with_highlight(
        mut self,
        highlight: impl Fn(&Item, &ViewContext) + Send + 'static,
    ) -> Self {
        self.announced = self.selected().map(|item| item.value.clone());
        self.highlight = Some(Box::new(highlight));
        self
    }

    /// Show at most `rows` list rows at once.
    #[must_use]
    pub const fn with_rows(mut self, rows: usize) -> Self {
        self.rows = rows;
        self
    }

    /// Start with the cursor on the item whose value is `value`.
    #[must_use]
    pub fn selecting(mut self, value: &str) -> Self {
        if let Some(position) = self
            .filtered
            .iter()
            .position(|index| self.items[*index].value == value)
        {
            self.cursor = position;
        }
        self
    }

    /// The current filter text.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The highlighted row.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The rows that pass the filter.
    #[must_use]
    pub fn visible(&self) -> Vec<&Item> {
        self.filtered
            .iter()
            .map(|index| &self.items[*index])
            .collect()
    }

    /// The highlighted item.
    #[must_use]
    pub fn selected(&self) -> Option<&Item> {
        self.filtered
            .get(self.cursor)
            .map(|index| &self.items[*index])
    }

    /// Set the filter and re-rank.
    ///
    /// # Grouping and filtering together
    ///
    /// When any item carries a [`Item::group`], groups are kept **contiguous** and ordered by
    /// their best-scoring member, and members are ordered by score inside each group.
    /// Headings therefore persist under a query, and each one still introduces a real run of
    /// rows. With no query every candidate scores alike, so both orderings collapse to the
    /// order the items were built in — which every grouped constructor sorts by name.
    ///
    /// The alternative — rank every row globally — was rejected because it interleaves
    /// providers, and a heading that introduces one row before the next heading is not a
    /// group. Dropping the headings while filtering was rejected for a worse reason: the
    /// provider is no longer repeated on each row, so a filtered list without headings
    /// would not say which provider a model belongs to at all, and two providers offering
    /// the same model name would be indistinguishable.
    ///
    /// The cost, stated because it is real: the single best-matching *row* need not be first
    /// overall. The best-matching *group* leads, and the best match within each group leads
    /// that group.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_owned();
        let mut ranked = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                // The description is searched too, matching upstream's behaviour for
                // slash commands (`autocomplete.tsx:506-507`): a user looking for
                // "the one that forks" does not know it is called `session_fork`.
                //
                // And the value, at the same weight as the description. A model's label is
                // its display name (`Claude Haiku 4.5`) while its value is the id the
                // engine takes (`…claude-haiku-4-5-20251001-v1:0`), and a user who knows
                // the id — because that is what `--model` and the config file spell —
                // otherwise types it and is told there are no matches. "No results" and
                // "searching the wrong field" look identical from the outside.
                // And the group, at the same halved weight. The grouped pickers moved the
                // provider off every row and into a heading; searching it here is what keeps
                // typing `bedrock` working after that move, which it would otherwise
                // silently stop doing.
                let best = score(&item.label, filter)
                    .into_iter()
                    .chain(score(&item.description, filter).map(|value| value / 2))
                    .chain(score(&item.value, filter).map(|value| value / 2))
                    .chain(score(&item.group, filter).map(|value| value / 2))
                    .max()?;
                Some((best, index))
            })
            .collect::<Vec<_>>();
        if self.is_grouped() {
            self.filtered = Self::rank_groups(&self.items, ranked);
        } else {
            ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            self.filtered = ranked.into_iter().map(|(_, index)| index).collect();
        }
        self.cursor = self.cursor.min(self.filtered.len().saturating_sub(1));
    }

    /// Whether any row carries a group, and so whether headings are drawn.
    fn is_grouped(&self) -> bool {
        self.items.iter().any(|item| !item.group.is_empty())
    }

    /// `scored` re-ordered so each group is one contiguous run, best group first.
    ///
    /// Groups appear in the order of their best-scoring member; ties keep the order the
    /// groups first appear in `items`, which is the name-sorted order a grouped constructor
    /// built. Within a group the items keep their `items` order for the same reason.
    fn rank_groups(items: &[Item], scored: Vec<(u32, usize)>) -> Vec<usize> {
        let mut groups: Vec<Group<'_>> = Vec::new();
        // `scored` is in `items` order, so pushing a new group the first time its name is
        // seen records groups in first-appearance order and members in name order.
        for (score, index) in scored {
            let name = items[index].group.as_str();
            match groups.iter_mut().find(|group| group.name == name) {
                Some(group) => {
                    group.best = group.best.max(score);
                    group.members.push((score, index));
                }
                None => groups.push(Group {
                    name,
                    best: score,
                    members: vec![(score, index)],
                }),
            }
        }
        groups.sort_by_key(|group| std::cmp::Reverse(group.best));
        groups
            .into_iter()
            .flat_map(|Group { mut members, .. }| {
                // Members are score-ordered too, and this is not the same as leaving them in
                // name order: with only the group ranked, `sonnet` put the cursor on the row
                // whose *id* contained it because that row sorted first alphabetically, while
                // the model actually named `Sonnet` sat below it. A picker that does not put
                // an exact name match under the cursor is worse than an unsorted one.
                //
                // Name order still governs the unfiltered list, for free rather than by a
                // branch: an empty query scores every candidate 1, so a stable sort by score
                // is a no-op and the name-sorted order this was built in survives.
                members.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
                members.into_iter().map(|(_, index)| index)
            })
            .collect()
    }

    /// The first visible row, chosen so the cursor's row is inside the row budget.
    ///
    /// Headings spend rows the items would otherwise have, so the plain
    /// `cursor - (rows - 1)` this replaces could put the cursor one or two rows past the
    /// bottom edge of a grouped list — the cursor would be on a row nobody can see, which is
    /// indistinguishable from the arrow keys having stopped working. The start is advanced
    /// until the cursor fits; the loop is bounded by the budget because each step drops one
    /// row from the window.
    fn window_start(&self) -> usize {
        let budget = self.rows.max(1);
        let mut start = self.cursor.saturating_sub(budget.saturating_sub(1));
        while start < self.cursor && self.display_rows(start, self.cursor) > budget {
            start += 1;
        }
        start
    }

    /// Rows the window `start..=end` occupies once its headings are counted.
    fn display_rows(&self, start: usize, end: usize) -> usize {
        if !self.is_grouped() {
            return end.saturating_sub(start) + 1;
        }
        let mut rows = 0;
        let mut previous: Option<&str> = None;
        for index in self.filtered.get(start..=end).unwrap_or_default() {
            let group = self.items[*index].group.as_str();
            // The topmost visible row always carries its heading, so a window opened in the
            // middle of a group still says which group that is.
            if previous != Some(group) {
                rows += 1;
                previous = Some(group);
            }
            rows += 1;
        }
        rows
    }

    /// Tell [`Self::highlight`] about the highlighted row, if it changed.
    ///
    /// Derived from the current selection rather than fired by whichever arm moved the
    /// cursor — see [`Self::announced`].
    fn refresh_highlight(&mut self) {
        if self.highlight.is_none() {
            return;
        }
        let current = self.selected().cloned();
        let value = current.as_ref().map(|item| item.value.clone());
        if value == self.announced {
            return;
        }
        self.announced = value;
        if let (Some(highlight), Some(item)) = (self.highlight.as_ref(), current.as_ref()) {
            highlight(item, &self.context);
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let length = self.filtered.len() as isize;
        // Wrapping rather than clamping: a list of five options is faster to reach
        // the end of by going up, and upstream's select wraps.
        self.cursor = ((self.cursor as isize + delta).rem_euclid(length)) as usize;
    }
}

impl Dialog for SelectDialog {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> String {
        if self.filter.is_empty() {
            format!("{} ({})", self.heading, self.filtered.len())
        } else {
            format!(
                "{} ({}) — {}",
                self.heading,
                self.filtered.len(),
                self.filter
            )
        }
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if self.filtered.is_empty() {
            lines.push(padded(" no matches", width, self.context.muted()));
            return lines;
        }
        // Keep the cursor in view by scrolling the window, not the cursor. Headings are
        // counted in the budget, so this is not simply `cursor - (rows - 1)`.
        let first = self.window_start();
        let grouped = self.is_grouped();
        let mut heading: Option<&str> = None;
        for (position, index) in self.filtered.iter().enumerate().skip(first) {
            if lines.len() >= self.rows {
                break;
            }
            let item = &self.items[*index];
            // A heading is emitted here and never added to `filtered`, which is what makes
            // it unselectable: the cursor indexes `filtered`, so there is no index it could
            // hold that names a heading. A cursor-skipping rule would be the other design,
            // and the row it forgot to skip would be a dead row the user can land on.
            if grouped && heading != Some(item.group.as_str()) {
                heading = Some(item.group.as_str());
                lines.push(padded(
                    &format!(" {}", item.group),
                    width,
                    self.context.accent(),
                ));
                if lines.len() >= self.rows {
                    break;
                }
            }
            let style = if position == self.cursor {
                self.context.selected()
            } else {
                self.context.text()
            };
            let marker = if position == self.cursor { ">" } else { " " };
            // Indented one column under its heading when grouped, so the heading reads as a
            // heading rather than as another row that happens to have no marker.
            let indent = if grouped { "  " } else { "" };
            let body = if item.description.is_empty() {
                format!(" {marker} {indent}{}", item.label)
            } else {
                format!(" {marker} {indent}{}  {}", item.label, item.description)
            };
            lines.push(padded(&body, width, style));
        }
        if let (Some(preview), Some(item)) = (self.preview.as_ref(), self.selected()) {
            lines.push(padded("", width, self.context.surface()));
            lines.extend(preview(item, &self.context));
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("↑↓", "move"),
            ("pgup/pgdn", "page"),
            // The filter has been typeable since this dialog shipped and nothing said so:
            // with 114 models the list reads as something you can only scroll. A capability
            // no surface announces is one the user does not have.
            ("type", "search"),
            ("enter", "select"),
            ("esc", "cancel"),
        ]
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        let step = self.dispatch(action, event);
        self.refresh_highlight();
        step
    }

    fn handle_typed(&mut self, key: &KeyEvent) -> DialogStep {
        let step = self.type_into_filter(key);
        self.refresh_highlight();
        step
    }
}

impl SelectDialog {
    /// Every action arm, with the highlight bookkeeping factored out to its one caller.
    fn dispatch(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        match action.name {
            "dialog.select.prev" => {
                self.move_cursor(-1);
                DialogStep::Redraw
            }
            "dialog.select.next" => {
                self.move_cursor(1);
                DialogStep::Redraw
            }
            "dialog.select.page_up" => {
                self.move_cursor(-(self.rows as isize));
                DialogStep::Redraw
            }
            "dialog.select.page_down" => {
                self.move_cursor(self.rows as isize);
                DialogStep::Redraw
            }
            "dialog.select.home" => {
                self.cursor = 0;
                DialogStep::Redraw
            }
            "dialog.select.end" => {
                self.cursor = self.filtered.len().saturating_sub(1);
                DialogStep::Redraw
            }
            "dialog.select.submit" | "dialog.prompt.submit" => match self.selected() {
                Some(item) => DialogStep::Resolved(DialogOutcome::Selected {
                    dialog: self.id,
                    value: item.value.clone(),
                }),
                None => DialogStep::Ignored,
            },
            // `session_interrupt` is the action the table binds to escape, and every
            // dialog footer here advertises `esc cancel`. Without this arm the dialog
            // ignored it, `DialogHost` absorbed it as an unrecognised action, and a
            // picker could only be left by choosing something — a hint that lies, and
            // the worse kind, because it names a way out that does not exist.
            "app_exit" | "session_interrupt" => DialogStep::Resolved(DialogOutcome::Cancelled),
            "input_backspace" => {
                let mut filter = self.filter.clone();
                filter.pop();
                self.set_filter(&filter);
                DialogStep::Redraw
            }
            _ => self.type_into_filter(event),
        }
    }

    /// Append a typed character to the filter, the other way the selection can change.
    fn type_into_filter(&mut self, key: &KeyEvent) -> DialogStep {
        if let Some(character) = crate::views::permission::typed_character(key) {
            let filter = format!("{}{character}", self.filter);
            self.set_filter(&filter);
            return DialogStep::Redraw;
        }
        DialogStep::Ignored
    }
}

/// A session, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// The session id.
    pub id: String,
    /// Its title.
    pub title: String,
    /// A human-readable age or timestamp.
    pub when: String,
}

/// A row operation emitted by the session list.
///
/// The dialog sends the id and the title it actually displayed. The id remains the
/// durable identity consumed by the host; the title lets the next modal be pre-filled
/// without putting a database read in the view layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDialogAction {
    /// Open the rename prompt for this session.
    Rename { id: String, title: String },
    /// Delete this session after the list's second-keypress confirmation.
    Delete { id: String, title: String },
}

/// The session picker plus row actions that do not belong on generic lists.
///
/// Rename and delete are deliberately wrapped around [`SelectDialog`] rather than
/// added to it: a model row cannot be renamed, an agent row cannot be deleted, and a
/// generic list that silently accepted those actions would make the active key scopes
/// part of every picker's API.
pub struct SessionDialog {
    select: SelectDialog,
    delete_confirmation: Option<String>,
}

impl SessionDialog {
    fn new(select: SelectDialog) -> Self {
        Self {
            select,
            delete_confirmation: None,
        }
    }

    /// Start with the cursor on the session whose id is `value`.
    #[must_use]
    pub fn selecting(mut self, value: &str) -> Self {
        self.select = self.select.selecting(value);
        self
    }

    /// The highlighted row index.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.select.cursor()
    }

    fn selected_action(&self, delete: bool) -> Option<SessionDialogAction> {
        let item = self.select.selected()?;
        Some(if delete {
            SessionDialogAction::Delete {
                id: item.value.clone(),
                title: item.label.clone(),
            }
        } else {
            SessionDialogAction::Rename {
                id: item.value.clone(),
                title: item.label.clone(),
            }
        })
    }
}

impl Dialog for SessionDialog {
    fn id(&self) -> &'static str {
        self.select.id()
    }

    fn title(&self) -> String {
        self.select.title()
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let armed = self.delete_confirmation.as_deref();
        let selected = self.select.selected().map(|item| item.value.as_str());
        let selected_row = (armed == selected && armed.is_some()).then(|| {
            self.select
                .cursor
                .saturating_sub(self.select.window_start())
        });
        let mut lines = self.select.lines(width);
        if let Some(row) = selected_row
            && row < lines.len()
        {
            lines[row] = padded(
                " > Press ctrl+d again to confirm deletion",
                width,
                self.select.context.selected(),
            );
        }
        lines
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        // Session-specific operations come first because the dialog footer keeps whole
        // pairs from the head until the next one no longer fits. Putting generic
        // navigation first made ordinary narrow terminals show move/page/search while
        // silently hiding the only discoverability for rename and destructive delete.
        // These two pairs fit together at the 40-column supported minimum.
        vec![
            ("ctrl+r", "rename"),
            ("ctrl+d", "delete twice"),
            ("enter", "select"),
            ("↑↓", "move"),
            ("type", "search"),
            ("pgup/pgdn", "page"),
            ("esc", "cancel"),
        ]
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> DialogStep {
        match action.name {
            "session_rename" => match self.selected_action(false) {
                Some(request) => DialogStep::Resolved(DialogOutcome::Session(request)),
                None => DialogStep::Ignored,
            },
            "session_delete" => {
                let Some(item) = self.select.selected().cloned() else {
                    self.delete_confirmation = None;
                    return DialogStep::Ignored;
                };
                if self.delete_confirmation.as_deref() == Some(item.value.as_str()) {
                    self.delete_confirmation = None;
                    // Deletion is executed by the runtime host. Keep the picker mounted
                    // while that happens so a successful composition remount can replace it
                    // with the refreshed list instead of exposing the transcript between
                    // consecutive deletes. A refusal likewise leaves the same list available.
                    DialogStep::Emitted(DialogOutcome::Session(SessionDialogAction::Delete {
                        id: item.value,
                        title: item.label,
                    }))
                } else {
                    self.delete_confirmation = Some(item.value);
                    DialogStep::Redraw
                }
            }
            _ => {
                self.delete_confirmation = None;
                self.select.handle_action(action, event)
            }
        }
    }

    fn handle_typed(&mut self, key: &KeyEvent) -> DialogStep {
        self.delete_confirmation = None;
        self.select.handle_typed(key)
    }
}

/// The session picker.
#[must_use]
pub fn session_picker(context: ViewContext, sessions: Vec<SessionEntry>) -> SessionDialog {
    let items = sessions
        .into_iter()
        .map(|session| {
            Item::new(session.title)
                .described(session.when)
                .valued(session.id)
        })
        .collect();
    SessionDialog::new(SelectDialog::new(
        SESSION_DIALOG_ID,
        "Sessions",
        context,
        items,
    ))
}

/// A model, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    /// `provider/model`, the value the engine accepts.
    pub id: String,
    /// The model's display name.
    pub name: String,
    /// The provider's display name.
    pub provider: String,
    /// Whether this model declares reasoning support in the catalog.
    ///
    /// Carried per row so that picking a model teaches the screen whether a reasoning
    /// level applies to it. Without it, a level chosen on a reasoning model would keep
    /// its key looking live after switching to a model that ignores it.
    pub reasoning: bool,
}

/// The model picker, grouped by provider and sorted by name inside each provider.
///
/// The value is `provider/model` rather than a bare model id, because a bare id is
/// exactly the unqualified form the model policy treats as unavailable
/// (`zuno-agent/src/model_policy.rs`).
///
/// # Why a heading and not a column
///
/// Measured at 120x34 with 114 models, the flat list repeated `amazon-bedrock` on every one
/// of its rows: a hundred-odd copies of the one fact that was the same everywhere, and no
/// answer at all to "what else is there". The provider moves into a heading, which states it
/// once per run and makes the runs countable.
///
/// None of the four reference implementations does this, and each was checked: `codex` builds
/// one flat `SelectionItem` per preset
/// (`.omo/refs/codex/codex-rs/tui/src/chatwidget/model_popups.rs:198-220`), `omo-slim` hands
/// the host a flattened list with the provider id as each row's description
/// (`.omo/refs/omo-slim/src/tui-preset.ts:513-525`), `jcode`'s terminal picker makes the
/// provider a per-row switchable option, and its desktop picker walks provider → connection →
/// model as three separate stages
/// (`.omo/refs/jcode/crates/jcode-desktop2/src/model_picker.rs:3-9`). The staged walk was
/// rejected: it hides every model until a provider is chosen, so it cannot answer "which
/// provider has the model I want" — which is the question a search box exists for.
#[must_use]
pub fn model_picker(context: ViewContext, models: Vec<ModelEntry>) -> SelectDialog {
    let mut models = models;
    // Sorted here rather than relied upon from the host: the ordering is what makes each
    // provider's rows one contiguous run, and a heading drawn over a list that is not
    // actually grouped would repeat the same heading further down.
    models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.name.cmp(&right.name))
    });
    let items = models
        .into_iter()
        .map(|model| {
            Item::new(model.name)
                .grouped(model.provider)
                .valued(model.id)
        })
        .collect();
    SelectDialog::new(MODEL_DIALOG_ID, "Models", context, items)
}

/// An agent, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    /// The agent's name.
    pub name: String,
    /// Its one-line description.
    pub description: String,
}

/// The agent picker.
#[must_use]
pub fn agent_picker(context: ViewContext, agents: Vec<AgentEntry>) -> SelectDialog {
    let items = agents
        .into_iter()
        .map(|agent| Item::new(agent.name).described(agent.description))
        .collect();
    SelectDialog::new(AGENT_DIALOG_ID, "Agents", context, items)
}

/// The discovered skills, as a filterable list.
///
/// A list rather than a launcher: a skill is invoked by naming it in a prompt, so what
/// this surface is for is finding out what the name is. Choosing a row therefore reports
/// the name for the transcript to state, and does not start anything — a picker that
/// silently did nothing on enter would read as broken.
#[must_use]
pub fn skill_list(
    context: ViewContext,
    skills: Vec<crate::views::ambient::SkillSummary>,
) -> SelectDialog {
    let items = skills
        .into_iter()
        // Whitespace collapsed: a skill's description is a paragraph in `SKILL.md`, and a
        // list row is one line. Left as-is, an embedded newline ends the row early and the
        // rest of the sentence renders as a second, unlabelled row.
        .map(|skill| Item::new(skill.name).described(flatten(&skill.description)))
        .collect();
    SelectDialog::new(SKILL_DIALOG_ID, "Skills", context, items)
}

/// Collapse whitespace so a paragraph fits one list row.
fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The theme picker, previewing each theme's resolved palette.
///
/// `mode` is the light/dark mode the preview resolves in, so a user picking in dark
/// mode sees the dark variant of a theme that declares both.
#[must_use]
pub fn theme_picker(context: ViewContext, registry: &ThemeRegistry, mode: Mode) -> SelectDialog {
    let names = registry.names();
    let items = names
        .iter()
        .map(|name| {
            let layer = registry
                .layer_of(name)
                .map_or_else(String::new, |layer| format!("{layer:?}").to_lowercase());
            Item::new(name.clone()).described(layer)
        })
        .collect::<Vec<_>>();
    // Every theme is resolved once here rather than on each frame: resolution walks
    // colour references and a picker redraws on every keystroke. Shared with the
    // highlight hook rather than resolved twice — an `Arc` because both closures own
    // their captures and a registry cannot be borrowed past this function.
    let resolved = Arc::new(
        names
            .iter()
            .map(|name| (name.clone(), registry.resolve(name, mode)))
            .collect::<Vec<(String, Resolved)>>(),
    );
    // The theme actually in force, not a configuration guess: after an in-session
    // switch the file still names the theme the user started with, so reopening the
    // picker from the file would put the cursor on a theme that is no longer showing.
    let active = context.theme().name.clone();
    let previews = Arc::clone(&resolved);
    SelectDialog::new(THEME_DIALOG_ID, "Themes", context, items)
        .with_rows(8)
        .with_preview(move |item, context| {
            let Some((_, resolved)) = previews.iter().find(|(name, _)| *name == item.value) else {
                return Vec::new();
            };
            preview_lines(resolved, context)
        })
        .selecting(&active)
        // After `selecting`, so opening the picker announces nothing. This is the whole
        // live switch: moving the cursor repaints every surface in the context's tree,
        // because they all read the one theme this writes.
        .with_highlight(move |item, context| {
            if let Some((_, resolved)) = resolved.iter().find(|(name, _)| *name == item.value) {
                context.set_theme(resolved);
            }
        })
}

/// Six swatch rows summarising a palette, the theme picker's preview.
///
/// A subset of [`crate::theme::PaletteSampleView`]'s fifty-odd rows: a picker has
/// eight rows to spare, and these six are the ones a user judges a theme by.
#[must_use]
pub fn preview_lines(resolved: &Resolved, context: &ViewContext) -> Vec<Line<'static>> {
    let palette = &resolved.palette;
    let swatch = |label: &str, color: crate::theme::Rgba| {
        Span::styled(
            format!(" {label} "),
            ratatui::style::Style::new()
                .fg(crate::theme::selected_foreground(palette, Some(color)).into())
                .bg(color.into()),
        )
    };
    vec![
        Line::from(vec![Span::styled(
            format!(" {} ({:?})", resolved.name, resolved.mode),
            context.title(),
        )]),
        Line::from(vec![
            swatch("primary", palette.primary),
            swatch("accent", palette.accent),
            swatch("error", palette.error),
            swatch("warning", palette.warning),
            swatch("success", palette.success),
            swatch("info", palette.info),
        ]),
    ]
}
