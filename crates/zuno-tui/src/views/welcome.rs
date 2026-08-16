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
//! # Key hints are resolved, never spelled
//!
//! Every hint names an **action**, and its spelling is read back out of the user's own
//! configuration through [`crate::views::key_label`]. A rebound `input_submit`
//! therefore changes what this screen advertises — the property a hard-coded
//! `enter send` would quietly lose. It is also the only reason the grid can be
//! trusted: a hint that lies is worse than a hint that is missing.
//!
//! # The wordmark is painted per cell
//!
//! Letterforms and their drop shadow share one template, distinguished by which glyph
//! a cell holds: a block cell takes the brand colour, a box-drawing cell takes the
//! brand tinted toward the background. Emitting one span per cell is what makes the
//! shadow possible at all — a single styled string could only be one colour — and it
//! is the difference between a wordmark and ASCII art.

use crate::app::{AppEvent, Component, EventResult};
use crate::views::{ViewContext, fill, key_label, padded};
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

/// The hint grid, in reading order.
///
/// Chosen for the capabilities a user has to be able to reach on a first launch, with
/// the exit chord last because it is the one needed when nothing else has worked.
///
/// **Every entry must be an action [`crate::views::session::SessionScreen`] routes.** An
/// advertised key that nothing handles is worse than an absent one: the user presses it,
/// nothing happens, and they cannot tell a missing feature from a broken program. That is
/// the defect class this whole surface exists to remove, so re-introducing it *here* would
/// be the worst possible place. `welcome_tests` asserts the property rather than trusting
/// this note — `command_list` was on this list, bound to `ctrl+p`, and reached nothing.
///
/// It is back, because it now routes: [`crate::views::palette`] is what the forty-three
/// rows the binding table ships with `keys: "none"` are reached through, so the palette is
/// the entry point for a third of the application's capabilities rather than a convenience.
/// It is listed second, right after `send`, for that reason.
pub const HINTS: [Hint; 12] = [
    ("input_submit", "send"),
    ("command_list", "commands"),
    ("model_list", "models"),
    ("agent_list", "agents"),
    ("input_newline", "newline"),
    ("session_list", "sessions"),
    ("theme_list", "themes"),
    ("display_thinking", "thinking"),
    ("tool_details", "tool output"),
    ("help_show", "help"),
    ("mcp_list", "mcp"),
    ("app_exit", "cancel / exit"),
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
                self.context.palette.background_panel,
                self.context.palette.primary,
                SHADOW_MIX,
            )
            .into())
            .bg(self.context.palette.background_panel.into())
    }

    fn brand(&self) -> Style {
        Style::new()
            .fg(self.context.palette.primary.into())
            .bg(self.context.palette.background_panel.into())
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

    /// The hint grid, in as many columns as `width` affords.
    ///
    /// The column count is derived rather than fixed so that eighty columns gets two
    /// readable columns and two hundred gets four, instead of one layout being cramped
    /// and the other stranded in the left third of the screen.
    fn hint_rows(&self, width: u16, rows_available: usize) -> Vec<Line<'static>> {
        let resolved = HINTS
            .iter()
            .filter_map(|(action, label)| Some((key_label(action, &self.context)?, *label)))
            .collect::<Vec<_>>();
        if resolved.is_empty() || rows_available == 0 {
            return Vec::new();
        }
        let cell = resolved
            .iter()
            .map(|(key, label)| key.chars().count() + 1 + label.chars().count())
            .max()
            .unwrap_or(1)
            + 3;
        let columns = (usize::from(width) / cell).clamp(1, 4);
        let capacity = columns * rows_available;
        let shown = &resolved[..resolved.len().min(capacity)];
        let rows = shown.len().div_ceil(columns);
        (0..rows)
            .map(|row| {
                let mut spans = Vec::new();
                for column in 0..columns {
                    // Column-major, so the first column reads top to bottom — how a
                    // reader scans a key list they are still learning.
                    let Some((key, label)) = shown.get(column * rows + row) else {
                        continue;
                    };
                    let used = key.chars().count() + 1 + label.chars().count();
                    spans.push(Span::styled(key.clone(), self.context.accent()));
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

    fn centred(&self, text: &str, width: u16, style: Style) -> Line<'static> {
        let columns = usize::from(width);
        let indent = columns.saturating_sub(text.chars().count()) / 2;
        Line::from(vec![
            Span::styled(" ".repeat(indent), self.context.surface()),
            Span::styled(text.to_owned(), style),
        ])
    }

    fn tip_row(&self, width: u16) -> Line<'static> {
        let text = self.tip();
        let marker = "● tip   ";
        let length = marker.chars().count() + text.chars().count();
        let indent = usize::from(width).saturating_sub(length) / 2;
        Line::from(vec![
            Span::styled(" ".repeat(indent), self.context.surface()),
            Span::styled(marker.to_owned(), self.context.warning()),
            Span::styled(text.to_owned(), self.context.muted()),
        ])
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

        if self.tips_visible {
            body.push(padded("", width, self.context.surface()));
            body.push(self.tip_row(width));
        }

        // The grid takes whatever rows are left, so a short terminal loses hint rows
        // one at a time instead of losing the whole grid or overflowing the region.
        let spare = usize::from(height).saturating_sub(body.len() + 2);
        let grid = self.hint_rows(width, spare.min(3));
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
