//! The welcome surface: what this is, where you are, and what to press.
//!
//! # Why an empty transcript was the worst possible first frame
//!
//! Measured on a 200×50 terminal before this module existed, `zuno tui` painted two
//! non-empty rows out of fifty: the word `idle` on the status strip, and a cursor.
//! Nothing named the working directory, the model, the agent, or a single key — so
//! the only way to discover that the cancel chord leaves was to press it. That is not
//! a cosmetic gap. A blank alternate screen is also indistinguishable from a
//! rendering failure, which is the same "no results versus cannot see the data"
//! ambiguity every other surface in this crate refuses.
//!
//! # It is an empty state, not a splash
//!
//! The screen is drawn **only while the transcript has no messages**, which is exactly
//! when those rows are otherwise blank. It is never dismissed, never waited on, and
//! never covers content: the first submitted prompt replaces it, because
//! [`crate::views::session::SessionScreen`] draws the transcript in the same region.
//! A welcome screen that had to be closed would be noise on the second launch; one
//! that occupies only space nothing else wants cannot be.
//!
//! # `/` is taught first, and a key is only spelled when nothing else can teach it
//!
//! The measured complaint about the first draft of this grid was not that it lied — it
//! was that it could not be read. It advertised `<leader>m models`, and nothing on the
//! screen said what `<leader>` was, so the one row meant to make model switching
//! discoverable instead required the user to already know the convention. A hint nobody
//! can decode is indistinguishable from no hint at all.
//!
//! So the grid is split by *how a capability is reached*, and each half is presented in
//! its own vocabulary:
//!
//! * Anything with a slash spelling is advertised as `/model`, `/agent`, … The line
//!   above already says `type / for commands`, so these rows read as worked examples of
//!   a convention the user has just been handed, and typing `/` lists the rest.
//! * A key is spelled out only for what has no slash spelling — sending, inserting a
//!   newline, leaving. Nothing else on any surface announces those, so dropping them
//!   would leave a genuine hole.
//!
//! # Key spellings are resolved, never written down
//!
//! [`KEY_HINTS`] names **actions**; the spelling comes back out of a
//! [`crate::keybind::Keymap`] built from the user's own configuration, so a rebound
//! `input_submit` changes what this screen advertises. That is the property a
//! hard-coded `enter send` quietly loses — and this project has already paid for it
//! once, on a status strip whose hard-coded exit key went stale the moment overrides
//! became real.
//!
//! The keymap rather than [`crate::views::key_label`] is what makes the leader token
//! decodable: `key_label` reads the static table and hands back the *raw* spelling, so
//! `<leader>m` reaches the screen verbatim. Only the keymap substitutes the configured
//! leader chord — which is also why a spelling still carrying the token after resolution
//! is dropped rather than drawn. A hint that lies is worse than a missing one, and a
//! hint nobody can read is worse than both.
//!
//! # The wordmark is painted per cell
//!
//! Letterforms and their drop shadow share one template, distinguished by which glyph
//! a cell holds: a block cell takes the brand colour, a box-drawing cell takes the
//! brand tinted toward the background. Emitting one span per cell is what makes the
//! shadow possible at all — a single styled string could only be one colour — and it
//! is the difference between a wordmark and ASCII art.

use crate::app::{AppEvent, Component, EventResult};
use crate::keybind::Keymap;
use crate::views::{ViewContext, display_width, fill, key_label, padded};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

#[cfg(test)]
#[path = "welcome_tests.rs"]
mod tests;

/// The wordmark, one string per row.
///
/// Block cells (`█`) are the letterform; every other non-space cell is its shadow.
/// The two are separated at paint time rather than stored as two layers, because a
/// mask that drifted from the glyphs would be invisible until a letter lost an edge.
pub const WORDMARK: [&str; 6] = [
    "███████╗██╗   ██╗███╗   ██╗ ██████╗ ",
    "╚══███╔╝██║   ██║████╗  ██║██╔═══██╗",
    "  ███╔╝ ██║   ██║██╔██╗ ██║██║   ██║",
    " ███╔╝  ██║   ██║██║╚██╗██║██║   ██║",
    "███████╗╚██████╔╝██║ ╚████║╚██████╔╝",
    "╚══════╝ ╚═════╝ ╚═╝  ╚═══╝ ╚═════╝ ",
];

/// Columns the wordmark occupies.
pub const WORDMARK_WIDTH: u16 = 36;

/// How far the shadow is tinted from the background toward the brand colour.
///
/// Low enough to read as depth rather than as a second letterform, high enough to
/// survive the several shipped themes whose panel background sits close to their
/// primary.
const SHADOW_MIX: f64 = 0.34;

/// The narrowest terminal that still gets the wordmark.
///
/// Below this the letterforms would be clipped mid-glyph, which reads as a rendering
/// fault rather than as a narrow window, so the compact brand row is used instead.
pub const WORDMARK_MIN_WIDTH: u16 = WORDMARK_WIDTH + 4;

/// The shortest terminal that still gets the wordmark.
///
/// Six wordmark rows plus the facts, a tip and one hint row is sixteen; below twenty
/// the brand would crowd out the information it exists to introduce.
pub const WORDMARK_MIN_HEIGHT: u16 = 20;

/// The one-row brand used when the wordmark does not fit.
pub const COMPACT_BRAND: &str = "▌ ZUNO";

/// The one-line description under the brand.
pub const TAGLINE: &str = "a coding agent that lives in your terminal";

/// The rotating hints shown one at a time under the brand.
///
/// A pool rather than one fixed line, because a tip is worth reading only once. Each
/// entry is prose and names no key: keys belong in the grid below, where they are
/// resolved rather than spelled.
pub const TIPS: [&str; 12] = [
    "type a question and send it; there is no mode to enter first",
    "the status strip always names the agent and model actually in use",
    "every tool call carries its own status glyph, so a stall is visible",
    "reasoning is collapsed by default, and says how many lines it is hiding",
    "a patch is rendered as a diff, with line numbers, right in the transcript",
    "the sidebar carries token spend, LSP servers, MCP servers and skills",
    "switching model or agent takes effect on the next turn, not the running one",
    "a permission prompt never stops the rest of the screen from updating",
    "the first cancel press stops the running turn; the second one exits",
    "every picker filters as you type, ranked the way autocomplete ranks",
    "the theme picker previews a palette before you commit to it",
    "a narrow terminal drops the sidebar rather than truncating the reply",
];

/// One hint: an action name and the words describing it.
pub type Hint = (&'static str, &'static str);

/// The keys the grid still spells out, in reading order.
///
/// Deliberately only three, and deliberately the three with **no slash spelling**.
/// Everything the earlier eleven-entry list advertised as a leader chord — models,
/// agents, sessions, themes, mcp, thinking, help — is now reached by `/`, and a `/name`
/// is self-explanatory where `<leader>m` was not. What is left is what `/` cannot
/// express: submitting, inserting a newline, and leaving. Nothing else on any surface
/// announces those, so they are the rows that would leave a real hole if dropped.
///
/// Order is send, newline, exit: the two a user needs in the first ten seconds, then the
/// one they need when nothing else has worked.
///
/// **Every entry must be an action [`crate::views::session::SessionScreen`] routes.** An
/// advertised key that nothing handles is worse than an absent one: the user presses it,
/// nothing happens, and they cannot tell a missing feature from a broken program. That is
/// the defect class this whole surface exists to remove, so re-introducing it *here* would
/// be the worst possible place. `welcome_tests` asserts the property rather than trusting
/// this note — `command_list` was on this list, bound to `ctrl+p`, and reached nothing.
pub const KEY_HINTS: [Hint; 3] = [
    ("input_submit", "send"),
    ("input_newline", "newline"),
    ("app_exit", "cancel / exit"),
];

/// One slash hint: a command name without its `/`, and the words describing it.
pub type SlashHint = (&'static str, &'static str);

/// The slash commands the grid advertises, in reading order.
///
/// Names, not actions, because the name **is** what the user types — resolving an action
/// back to a spelling would be inventing a second copy of
/// [`crate::views::slash::SlashRouter`]'s naming rule, and the two would drift. The
/// property that matters is asserted instead: `welcome_tests` requires every name here to
/// resolve through the real router to a UI action the session screen routes. That is what
/// makes a typo, a renamed command, and a name the router deliberately excludes all fail
/// loudly rather than render as an inert row.
///
/// Order pairs them the way the two-column layouts fall out at 60 and 80 columns —
/// `model`/`agent` (what answers), `session`/`theme` (what the workspace looks like),
/// `mcp`/`help` (what is available). `help` is last because it is the row that supersedes
/// this whole grid: it lists every key, which is the question a hint grid can only ever
/// answer partially.
pub const SLASH_HINTS: [SlashHint; 6] = [
    ("model", "switch model"),
    ("agent", "switch agent"),
    ("session", "past sessions"),
    ("theme", "change theme"),
    ("mcp", "mcp servers"),
    ("help", "all keys"),
];

/// The facts the welcome screen states outright.
///
/// Every field is optional because the host resolves them at different moments, and a
/// welcome screen that waited for all of them would be blank exactly when it matters
/// most. An absent fact is omitted rather than shown as a placeholder: `unknown` in
/// the model row would be indistinguishable from a model actually called `unknown`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WelcomeFacts {
    /// The working directory, already abbreviated for display.
    pub directory: Option<String>,
    /// The version-control branch, when the directory is a checkout.
    pub branch: Option<String>,
    /// `provider/model`, as the plan resolved it.
    pub model: Option<String>,
    /// The agent that will answer.
    pub agent: Option<String>,
    /// The build's version string.
    pub version: Option<String>,
    /// How many tools the session offers.
    pub tools: Option<usize>,
    /// How many MCP servers are configured.
    pub mcp: Option<usize>,
    /// How many language servers are configured.
    pub lsp: Option<usize>,
    /// How many skills were discovered.
    pub skills: Option<usize>,
}

impl WelcomeFacts {
    /// The location row, when a directory or a branch is known.
    #[must_use]
    pub fn location(&self) -> Option<String> {
        match (&self.directory, &self.branch) {
            (Some(directory), Some(branch)) => Some(format!("{directory}   ⑂ {branch}")),
            (Some(directory), None) => Some(directory.clone()),
            (None, Some(branch)) => Some(format!("⑂ {branch}")),
            (None, None) => None,
        }
    }

    /// The `agent · model` row, when either is known.
    #[must_use]
    pub fn engine(&self) -> Option<String> {
        match (&self.agent, &self.model) {
            (Some(agent), Some(model)) => Some(format!("{agent}   ·   {model}")),
            (Some(agent), None) => Some(agent.clone()),
            (None, Some(model)) => Some(model.clone()),
            (None, None) => None,
        }
    }

    /// The capability census, e.g. `13 tools · 2 mcp · 1 lsp · 25 skills`.
    ///
    /// A zero count is shown rather than dropped: `0 mcp` is precisely the fact a user
    /// chasing a missing MCP tool needs, and omitting the row would read as
    /// "not measured".
    #[must_use]
    pub fn inventory(&self) -> Option<String> {
        let parts = [
            self.tools.map(|count| format!("{count} tools")),
            self.mcp.map(|count| format!("{count} mcp")),
            self.lsp.map(|count| format!("{count} lsp")),
            self.skills.map(|count| format!("{count} skills")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if parts.is_empty() {
            return None;
        }
        Some(parts.join("   ·   "))
    }
}

/// The welcome surface.
pub struct WelcomeView {
    context: ViewContext,
    facts: WelcomeFacts,
    tip: usize,
    tips_visible: bool,
}

impl WelcomeView {
    /// A welcome screen over `context`, with no facts resolved yet.
    #[must_use]
    pub const fn new(context: ViewContext) -> Self {
        Self {
            context,
            facts: WelcomeFacts {
                directory: None,
                branch: None,
                model: None,
                agent: None,
                version: None,
                tools: None,
                mcp: None,
                lsp: None,
                skills: None,
            },
            tip: 0,
            tips_visible: true,
        }
    }

    /// State the facts the host resolved.
    #[must_use]
    pub fn with_facts(mut self, facts: WelcomeFacts) -> Self {
        self.facts = facts;
        self
    }

    /// Show the tip at `index`, wrapped into the pool.
    ///
    /// The caller chooses rather than this module rolling a die, so a test can pin a
    /// tip and a host can vary it per launch without either owning the pool.
    #[must_use]
    pub const fn with_tip(mut self, index: usize) -> Self {
        self.tip = index;
        self
    }

    /// The facts, mutably, for a host that resolves them after construction.
    pub const fn facts_mut(&mut self) -> &mut WelcomeFacts {
        &mut self.facts
    }

    /// Advance to the next tip, or bring the row back if it was hidden.
    ///
    /// One action does both because "show me another" and "show me one" are the same
    /// request from a user who hid the row and changed their mind.
    pub const fn next_tip(&mut self) {
        if self.tips_visible {
            self.tip = self.tip.wrapping_add(1);
        } else {
            self.tips_visible = true;
        }
    }

    /// Hide the tip row.
    pub const fn hide_tips(&mut self) {
        self.tips_visible = false;
    }

    /// Whether the tip row is showing.
    #[must_use]
    pub const fn tips_visible(&self) -> bool {
        self.tips_visible
    }

    /// The tip currently selected.
    #[must_use]
    pub fn tip(&self) -> &'static str {
        TIPS[self.tip % TIPS.len()]
    }

    /// Whether the full wordmark fits in `width` by `height`.
    #[must_use]
    pub const fn wordmark_fits(width: u16, height: u16) -> bool {
        width >= WORDMARK_MIN_WIDTH && height >= WORDMARK_MIN_HEIGHT
    }

    fn shadow(&self) -> Style {
        Style::new()
            .fg(crate::theme::tint(
                self.context.palette().background_panel,
                self.context.palette().primary,
                SHADOW_MIX,
            )
            .into())
            .bg(self.context.palette().background_panel.into())
    }

    fn brand(&self) -> Style {
        Style::new()
            .fg(self.context.palette().primary.into())
            .bg(self.context.palette().background_panel.into())
            .add_modifier(Modifier::BOLD)
    }

    fn wordmark_row(&self, row: &str, indent: usize) -> Line<'static> {
        let mut spans = Vec::with_capacity(row.chars().count() + 1);
        if indent > 0 {
            spans.push(Span::styled(" ".repeat(indent), self.context.surface()));
        }
        for character in row.chars() {
            let style = if character == '█' {
                self.brand()
            } else if character == ' ' {
                self.context.surface()
            } else {
                self.shadow()
            };
            spans.push(Span::styled(character.to_string(), style));
        }
        Line::from(spans)
    }

    /// The keymap this screen reads spellings out of, or `None` if it cannot be built.
    ///
    /// Built here rather than threaded in from the host because
    /// [`crate::views::session::SessionScreen`] constructs this screen with nothing but a
    /// [`ViewContext`] — and the context already carries the resolved configuration a
    /// keymap is built from, so there is no second source of truth for one to disagree
    /// with. A configuration whose bindings conflict yields `None` and the grid falls back
    /// to the static table; a welcome screen that blanked its hints over a keybind
    /// conflict would hide the very rows that explain how to fix it.
    fn keymap(&self) -> Option<Keymap> {
        Keymap::from_config(&self.context.config).ok()
    }

    /// The chords a user would actually press for `action`, or `None` when unbound.
    ///
    /// The keymap is asked first because it is the only thing that substitutes the leader
    /// chord — `key_label` hands back the raw spelling, which is how `<leader>m` once
    /// reached the screen as a token nothing on it defined. It also honours
    /// [`crate::keybind::SHIPPED_DEFAULTS`], so an action this build binds and upstream
    /// does not is advertised rather than silently omitted.
    ///
    /// A spelling that still carries the leader token after resolution is dropped. That
    /// only happens when there is no keymap *and* the user's raw spelling is a leader
    /// sequence, and in that corner an unreadable row is worse than a missing one.
    fn spelling(&self, keymap: Option<&Keymap>, action: &str) -> Option<String> {
        keymap
            .map_or_else(
                || key_label(action, &self.context),
                |keymap| keymap.sequences(action).into_iter().next(),
            )
            .filter(|spelling| !spelling.contains(crate::keybind::LEADER_TOKEN))
    }

    /// `entries` laid out in as many columns as `width` affords, capped at `rows_available`.
    ///
    /// The column count is derived rather than fixed so that eighty columns gets two
    /// readable columns and two hundred gets four, instead of one layout being cramped
    /// and the other stranded in the left third of the screen.
    ///
    /// Taken as a slice rather than reading a constant, because the grid is drawn twice —
    /// once for keys and once for slash commands — and each group then gets a cell width
    /// measured from its own entries. One shared width would pad the shorter group out to
    /// the longer group's widest row and open a gap the eye reads as a missing column.
    fn grid(
        &self,
        entries: &[(String, &'static str)],
        width: u16,
        rows_available: usize,
    ) -> Vec<Line<'static>> {
        if entries.is_empty() || rows_available == 0 {
            return Vec::new();
        }
        // Terminal columns, not characters. A label carrying a wide glyph is otherwise
        // under-measured by one column per glyph, and the row overflows its frame — a
        // mistake this crate has already made and now measures for.
        let cell = entries
            .iter()
            .map(|(lead, label)| display_width(lead) + 1 + display_width(label))
            .max()
            .unwrap_or(1)
            + 3;
        let columns = (usize::from(width) / cell).clamp(1, 4);
        let capacity = columns * rows_available;
        let shown = &entries[..entries.len().min(capacity)];
        let rows = shown.len().div_ceil(columns);
        (0..rows)
            .map(|row| {
                let mut spans = Vec::new();
                for column in 0..columns {
                    // Column-major, so the first column reads top to bottom — how a
                    // reader scans a list they are still learning.
                    let Some((lead, label)) = shown.get(column * rows + row) else {
                        continue;
                    };
                    let used = display_width(lead) + 1 + display_width(label);
                    spans.push(Span::styled(lead.clone(), self.context.accent()));
                    spans.push(Span::styled(String::from(" "), self.context.surface()));
                    spans.push(Span::styled((*label).to_owned(), self.context.muted()));
                    spans.push(Span::styled(
                        " ".repeat(cell.saturating_sub(used)),
                        self.context.surface(),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }

    /// The two hint groups, keys above slash commands, within `rows_available` rows.
    ///
    /// Keys are laid out first and are served from the budget first. `/` is already taught
    /// by the line above and typing it lists every slash command, so a slash row lost to a
    /// short terminal costs a shortcut the user can still find; nothing anywhere announces
    /// the send key, so a key row lost costs a capability. Two rows is enough for all
    /// three key hints down to sixty columns, which is the width the earlier single grid
    /// dropped the exit hint at.
    ///
    /// Both groups share one accent-then-muted grammar — what you type on the left, what
    /// it does on the right — so the eye learns one column meaning rather than two.
    fn hint_block(&self, width: u16, rows_available: usize) -> Vec<Line<'static>> {
        let keymap = self.keymap();
        let resolved = KEY_HINTS
            .iter()
            .filter_map(|(action, label)| Some((self.spelling(keymap.as_ref(), action)?, *label)))
            .collect::<Vec<_>>();
        let keys = self.grid(&resolved, width, rows_available.min(2));

        let commands = SLASH_HINTS
            .iter()
            .map(|(name, label)| (format!("/{name}"), *label))
            .collect::<Vec<_>>();
        // The separator is charged to the budget before the slash rows are measured, so a
        // terminal with one row to spare spends it on a hint instead of on a blank.
        let spent = keys.len() + usize::from(!keys.is_empty());
        let slash = self.grid(
            &commands,
            width,
            rows_available.saturating_sub(spent).min(3),
        );

        let mut block = keys;
        if !block.is_empty() && !slash.is_empty() {
            // A zero-width line, not a padded one: the caller centres the block on its
            // widest row, and a full-width blank would report the frame's own width and
            // pin the indent to zero.
            block.push(Line::default());
        }
        block.extend(slash);
        block
    }

    fn centred(&self, text: &str, width: u16, style: Style) -> Line<'static> {
        let columns = usize::from(width);
        let indent = columns.saturating_sub(display_width(text)) / 2;
        Line::from(vec![
            Span::styled(" ".repeat(indent), self.context.surface()),
            Span::styled(text.to_owned(), style),
        ])
    }

    fn tip_row(&self, width: u16) -> Line<'static> {
        let text = self.tip();
        let marker = "● tip   ";
        let length = display_width(marker) + display_width(text);
        let indent = usize::from(width).saturating_sub(length) / 2;
        Line::from(vec![
            Span::styled(" ".repeat(indent), self.context.surface()),
            Span::styled(marker.to_owned(), self.context.warning()),
            Span::styled(text.to_owned(), self.context.muted()),
        ])
    }

    /// The lead line: `/` first, then the palette key that offers the same list.
    ///
    /// `/` comes first and unqualified because it is the one thing a user can act on with
    /// no prior knowledge — it needs no modifier, no leader, and no configuration to be
    /// read out of. The palette chord follows as the equivalent for someone who already
    /// works in chords, and it is resolved through the same [`Self::spelling`] helper every
    /// other key on this screen goes through, so there is exactly one place a stale
    /// spelling could come from.
    fn command_row(&self, width: u16) -> Line<'static> {
        let mut spans = vec![
            Span::styled(String::from("type "), self.context.muted()),
            Span::styled(String::from("/"), self.context.accent()),
            Span::styled(String::from(" for commands"), self.context.muted()),
        ];
        if let Some(key) = self.spelling(self.keymap().as_ref(), "command_list") {
            spans.push(Span::styled(String::from("   "), self.context.surface()));
            spans.push(Span::styled(key, self.context.accent()));
            spans.push(Span::styled(
                String::from(" command palette"),
                self.context.muted(),
            ));
        }
        let width_used = spans.iter().map(Span::width).sum::<usize>();
        spans.insert(
            0,
            Span::styled(
                " ".repeat(usize::from(width).saturating_sub(width_used) / 2),
                self.context.surface(),
            ),
        );
        Line::from(spans)
    }

    /// Every row this screen draws at `width` by `height`, already centred.
    ///
    /// Public because it is the assertable surface: a claim about rows is readable
    /// where the same claim about cells is not, and the off-screen buffer test then
    /// proves the rows reach cells.
    #[must_use]
    pub fn lines(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        let mut body = Vec::new();
        if Self::wordmark_fits(width, height) {
            let indent = usize::from(width.saturating_sub(WORDMARK_WIDTH)) / 2;
            for row in WORDMARK {
                body.push(self.wordmark_row(row, indent));
            }
            body.push(padded("", width, self.context.surface()));
            body.push(self.centred(TAGLINE, width, self.context.muted()));
        } else {
            body.push(self.centred(COMPACT_BRAND, width, self.brand()));
            body.push(self.centred(TAGLINE, width, self.context.muted()));
        }

        let mut facts = Vec::new();
        if let Some(location) = self.facts.location() {
            facts.push((location, self.context.text()));
        }
        if let Some(engine) = self.facts.engine() {
            facts.push((engine, self.context.accent()));
        }
        if let Some(inventory) = self.facts.inventory() {
            facts.push((inventory, self.context.muted()));
        }
        if let Some(version) = self.facts.version.clone() {
            facts.push((format!("zuno {version}"), self.context.muted()));
        }
        if !facts.is_empty() {
            body.push(padded("", width, self.context.surface()));
            for (text, style) in facts {
                body.push(self.centred(&text, width, style));
            }
        }

        body.push(padded("", width, self.context.surface()));
        body.push(self.command_row(width));

        if self.tips_visible {
            body.push(padded("", width, self.context.surface()));
            body.push(self.tip_row(width));
        }

        // The grid takes whatever rows are left, so a short terminal loses hint rows
        // one at a time instead of losing the whole grid or overflowing the region. Six is
        // the block's own ceiling — two key rows, a separator, three slash rows — reached
        // only at sixty columns, where both groups fall to two columns.
        let spare = usize::from(height).saturating_sub(body.len() + 2);
        let grid = self.hint_block(width, spare.min(6));
        if !grid.is_empty() {
            body.push(padded("", width, self.context.surface()));
            let widest = grid
                .iter()
                .map(ratatui::text::Line::width)
                .max()
                .unwrap_or_default();
            let indent = usize::from(width).saturating_sub(widest) / 2;
            for line in grid {
                let mut spans = vec![Span::styled(" ".repeat(indent), self.context.surface())];
                spans.extend(line.spans);
                body.push(Line::from(spans));
            }
        }

        // Centre vertically by padding above, so the block sits on the optical middle
        // rather than clinging to the top of a tall terminal.
        let leading = usize::from(height).saturating_sub(body.len()) / 2;
        let mut lines = Vec::with_capacity(leading + body.len());
        lines.extend(std::iter::repeat_n(
            padded("", width, self.context.surface()),
            leading,
        ));
        lines.extend(body);
        lines
    }
}

impl Component for WelcomeView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        fill(frame.buffer_mut(), area, self.context.surface());
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = self
            .lines(area.width, area.height)
            .into_iter()
            .take(usize::from(area.height))
            .collect::<Vec<_>>();
        Paragraph::new(lines)
            .style(self.context.surface())
            .render(area, frame.buffer_mut());
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        // This screen is derived from facts the host states and from the transcript
        // being empty, which its owner decides. Nothing here reacts to an event.
        EventResult::IGNORED
    }
}
