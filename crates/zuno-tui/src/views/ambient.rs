//! Ambient state: token spend, language servers, MCP servers, and skills.
//!
//! # These are facts, not actions, so they get a panel rather than a view
//!
//! An LSP server's health and an MCP server's connection are things a user checks,
//! not things a user does. Giving each its own full-screen surface would mean a
//! keystroke to answer "is my MCP up", and a user who has to ask cannot notice the
//! answer changed. So they are drawn continuously beside the transcript, and the
//! only interaction is collapsing a section that has grown long.
//!
//! # Nothing here knows where a fact came from
//!
//! [`Ambient`] is plain data: strings, counts, and a three-way state. `zuno-tui`
//! deliberately does not depend on `zuno-lsp`, `zuno-mcp` or `zuno-catalog`, so the
//! host converts its own types into these and this module cannot reach back into
//! execution. That is the same discipline the transcript follows with
//! [`zuno_engine::r#loop::TurnEvent`], and it is why a sidebar can be asserted
//! off-screen with no servers running.
//!
//! # The sidebar is dropped, never squeezed
//!
//! Below [`crate::views::SIDEBAR_MIN_WIDTH`] the panel is not drawn at all. A
//! sidebar narrowed until its server names truncate tells the user less than no
//! sidebar while costing the reply the columns it needed —
//! [`crate::views::session::SessionScreen`] makes that call, and
//! [`SIDEBAR_WIDTH`] is why the threshold is where it is.

use crate::app::{AppEvent, Component, EventResult};
use crate::views::{ViewContext, display_width, fill, padded, truncate};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

#[cfg(test)]
#[path = "ambient_tests.rs"]
mod tests;

/// Columns the sidebar occupies when it is drawn.
///
/// Wide enough for `typescript-language-server` plus its state glyph without
/// truncating, which is the longest name the shipped LSP registry can produce.
pub const SIDEBAR_WIDTH: u16 = 34;

/// The glyph that marks a git branch.
///
/// Declared here because this panel is where the field was first drawn, and
/// [`crate::views::message::StatusView::BRANCH_GLYPH`] defers to it so the two surfaces
/// that state the branch cannot drift to two different glyphs. Only the spacing differs
/// between them, deliberately — see the footer that renders it.
pub const BRANCH_GLYPH: &str = "⑂";

/// The glyph marking a collapsible section that is open.
pub const OPEN_GLYPH: &str = "▾";

/// The glyph marking a collapsible section that is closed.
pub const CLOSED_GLYPH: &str = "▸";

/// How healthy a background service is, reduced to what a colour can carry.
///
/// Three states rather than each subsystem's own enum, because the panel's job is to
/// let a user scan for trouble: an LSP that is `Degraded` and an MCP that
/// `NeedsAuth` are the same colour of problem, and the specific word goes in the
/// detail column beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Answering.
    Ready,
    /// Working on it, or not yet started.
    Pending,
    /// Needs attention.
    Faulted,
    /// Deliberately switched off, which is not a fault.
    Disabled,
}

impl Health {
    /// The glyph drawn in the row's gutter.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Ready => "●",
            Self::Pending => "◐",
            Self::Faulted => "✗",
            Self::Disabled => "○",
        }
    }
}

/// One background service, as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Its name.
    pub name: String,
    /// Its health.
    pub health: Health,
    /// A short human-readable detail, such as a root path or a failure reason.
    pub detail: String,
}

impl Service {
    /// A service with no detail.
    #[must_use]
    pub fn new(name: impl Into<String>, health: Health) -> Self {
        Self {
            name: name.into(),
            health,
            detail: String::new(),
        }
    }

    /// Attach a detail.
    #[must_use]
    pub fn detailed(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// One skill, as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    /// The skill's name, which is also the command it claims.
    pub name: String,
    /// Its one-line description.
    pub description: String,
    /// Whether this session successfully loaded the skill through the `skill` tool.
    pub loaded: bool,
}

/// Every ambient fact the sidebar states.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ambient {
    /// What this session is about, as the model named it after the opening exchange.
    ///
    /// [`None`] until it is named, and that state is drawn as *nothing* — no heading, no
    /// placeholder row. A session acquires its name one small-model request after the
    /// first prompt, so any stand-in would be on screen for about a second and would
    /// occupy the row the real name is about to take; the panel would visibly reflow for
    /// no information. It is also `None` for every session that could not be named,
    /// which is the same absence and deliberately indistinguishable here — the reason is
    /// reported on the transcript, where a user can act on it.
    pub title: Option<String>,
    /// The working directory, already abbreviated.
    pub directory: Option<String>,
    /// The checkout's branch.
    pub branch: Option<String>,
    /// The agent answering.
    pub agent: Option<String>,
    /// `provider/model`.
    pub model: Option<String>,
    /// Token accounting, folded by the transcript and handed over each frame.
    pub tokens: crate::views::message::TokenUsage,
    /// Whether those token figures are durable, unavailable, or not reported yet.
    pub usage_state: crate::views::message::UsageState,
    /// How full the context window is, when the model declares one.
    pub context_used: Option<u64>,
    /// Language servers.
    pub lsp: Vec<Service>,
    /// MCP servers.
    pub mcp: Vec<Service>,
    /// Discovered skills.
    pub skills: Vec<SkillSummary>,
    /// The build's version.
    pub version: Option<String>,
}

/// The session name and a counter that advances when it changes.
#[derive(Debug, Default)]
struct Titled {
    generation: u64,
    title: Option<String>,
}

/// The session's name, published by whoever generates it and read by the panel.
///
/// # Why a projection and not a turn event
///
/// The name is produced by the turn prelude on the driver task while the panel is
/// drawn on the render loop, so the value must cross task ownership. A projection
/// carries that state without widening the turn event stream with presentation-only
/// data.
///
/// # The counter exists because a wake alone paints nothing
///
/// Copied deliberately from [`crate::views::picker::McpProjection`], whose own header
/// records the failure: a wake only reaches the terminal if some component reports
/// `redraw`, so a projection that changed with nothing reporting it leaves the old frame
/// on screen. The generation lets one observer answer "did this move" in constant time,
/// and it advances **only** on a real change — a bump for an identical title would spend a
/// frame repainting bytes that did not change.
#[derive(Debug, Clone, Default)]
pub struct SessionTitle(std::sync::Arc<std::sync::RwLock<Titled>>);

impl SessionTitle {
    /// A projection already holding `title`, for a session that was resumed.
    #[must_use]
    pub fn new(title: Option<String>) -> Self {
        Self(std::sync::Arc::new(std::sync::RwLock::new(Titled {
            generation: 0,
            title,
        })))
    }

    /// Publish `title`, advancing the generation only if it differs.
    pub fn replace(&self, title: Option<String>) {
        let mut titled = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if titled.title == title {
            return;
        }
        titled.title = title;
        titled.generation = titled.generation.wrapping_add(1);
    }

    /// How many content changes this projection has published.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }

    /// The generation and the title it belongs to, under one read.
    ///
    /// One lock for both, for the reason [`crate::views::picker::McpProjection::observe`]
    /// documents: two calls could straddle a [`Self::replace`] and pair a stale generation
    /// with a fresh title, which makes an observer record a frame it never painted.
    #[must_use]
    pub fn observe(&self) -> (u64, Option<String>) {
        let titled = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (titled.generation, titled.title.clone())
    }
}

/// The most rows the session name may occupy.
///
/// Two, not one and not unbounded. The generator caps a title at 100 characters and this
/// panel is [`SIDEBAR_WIDTH`] wide, so a long title needs four rows to be shown whole —
/// and those rows come out of the server lists below it, which report state that changes.
/// One row is the other failure: at roughly thirty columns it holds about four words,
/// which is often not enough to tell two sessions in the same repository apart. Two rows
/// covers an ordinary title outright and cuts the rest, which is the cheaper loss.
pub const TITLE_MAX_ROWS: usize = 2;

/// Break `text` into at most `rows` rows of `width` columns, marking a cut.
///
/// Prefers a whitespace break and falls back to a column break, which is not a
/// nicety — a Chinese title contains no spaces at all, so a word-only wrapper returns
/// the whole title as one over-long row and the panel then clips it at the frame edge,
/// losing the tail with no mark that anything was dropped.
///
/// Counted in **columns** throughout, for the reason [`elide_left`] documents: a CJK
/// title measures far fewer characters than the cells it occupies, so a character-counted
/// wrapper produces rows that satisfy their own arithmetic and still overrun the panel.
fn wrap(text: &str, width: usize, rows: usize) -> Vec<String> {
    if width == 0 || rows == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() && lines.len() < rows {
        if display_width(rest) <= width {
            lines.push(rest.to_owned());
            return lines;
        }
        // The last row cannot continue anywhere, so it is elided rather than broken: a
        // clean word break on the final row silently discards the remainder.
        if lines.len() + 1 == rows {
            lines.push(format!(
                "{}{}",
                truncate(rest, width.saturating_sub(1)),
                crate::views::message::ELIDED
            ));
            return lines;
        }
        let head = truncate(rest, width);
        // Back up to the last space *inside* the row, unless the row already ends on a
        // boundary — `rest[head.len()..]` starting with whitespace means the cut is
        // already between words and moving it would waste a column for nothing.
        let taken = if rest[head.len()..].starts_with(char::is_whitespace) {
            head.len()
        } else {
            head.rfind(char::is_whitespace).map_or(head.len(), |at| at)
        };
        let (line, remainder) = rest.split_at(taken);
        let line = line.trim_end();
        if line.is_empty() {
            // A single glyph wider than the row: emit the column break rather than
            // looping forever on a break point that cannot advance.
            lines.push(head.clone());
            rest = rest[head.len()..].trim_start();
            continue;
        }
        lines.push(line.to_owned());
        rest = remainder.trim_start();
    }
    lines
}

/// The fewest columns a detail needs before it says more than the glyph beside it.
///
/// Below this the text is dropped rather than elided: a two-character stub of
/// `configured` is a fragment a reader has to decode, and the health glyph already
/// carries the state.
pub const DETAIL_MIN_COLUMNS: usize = 6;

/// Keep the last `width` columns of `text`, marking the cut with an ellipsis.
///
/// The tail is kept because that is what identifies a path or a failure message; a cut
/// at the other end preserves the prefix every sibling shares.
///
/// `width` is **columns**, and the ellipsis is charged one of them. Counting characters
/// instead returned a string that satisfied the caller's arithmetic and still overran the
/// panel by one column per wide glyph: a Chinese failure reason came back "short enough",
/// was laid out flush against the right edge, and had its tail — the part this function
/// exists to preserve — clipped off the frame.
#[must_use]
pub fn elide_left(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return truncate(text, width);
    }
    // Walk back from the end, keeping the longest suffix that fits beside the mark. Char
    // boundaries are the step, so a two-column glyph is either taken whole or not at all —
    // half of one is not something a terminal can draw.
    let mut kept = text.len();
    for (index, _) in text.char_indices().rev() {
        if 1 + display_width(&text[index..]) > width {
            break;
        }
        kept = index;
    }
    format!("{}{}", crate::views::message::ELIDED, &text[kept..])
}

/// Abbreviate a token count for a row that has no space for the grouped form.
///
/// The status strip has one row shared with the turn state, so `12.3k` is worth more
/// there than `12,345`; the sidebar shows the exact figure.
#[must_use]
pub fn compact(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format!("{:.1}k", value as f64 / 1_000.0),
        _ => format!("{:.1}m", value as f64 / 1_000_000.0),
    }
}

/// Which sidebar sections are expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expanded {
    /// Whether the LSP list is open.
    pub lsp: bool,
    /// Whether the MCP list is open.
    pub mcp: bool,
    /// Whether the skill list is open.
    pub skills: bool,
}

impl Default for Expanded {
    /// LSP and MCP open, skills closed.
    ///
    /// Skills are numerous and static — a shipped install has dozens and they do not
    /// change while the session runs — so their count is the interesting part and
    /// their names are not. Servers are few and can break, so their names are.
    fn default() -> Self {
        Self {
            lsp: true,
            mcp: true,
            skills: false,
        }
    }
}

/// One collapsible section of the panel.
///
/// Named rather than indexed because a hit map keyed by position would have to be read
/// against the same row order that produced it, and the two would drift the first time a
/// section moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// The language-server list.
    Lsp,
    /// The MCP server list.
    Mcp,
    /// The discovered-skill list.
    Skills,
}

/// The panel's rows plus the index of each section heading among them.
struct PanelRows {
    lines: Vec<Line<'static>>,
    headers: Vec<(usize, Section)>,
}

/// The ambient panel.
pub struct SidebarView {
    context: ViewContext,
    ambient: Ambient,
    expanded: Expanded,
    /// Where each section's heading was drawn in the frame that was drawn.
    ///
    /// Absolute screen rows, recorded by [`Component::render`] from the same `lines()`
    /// output it paints and the same `Rect` it paints into. Derived once, in the one place
    /// that knows both — a map maintained anywhere else would have to be told about a
    /// resize, and the resize that forgot to tell it is a click landing on the wrong
    /// section.
    ///
    /// Cleared by [`Self::forget_hit_targets`] whenever the owner does not draw the panel,
    /// so a click at the sidebar's old coordinates cannot toggle a section the user can no
    /// longer see.
    hits: Vec<(Rect, Section)>,
}

impl SidebarView {
    /// A panel over `context` with nothing resolved yet.
    #[must_use]
    pub fn new(context: ViewContext) -> Self {
        Self {
            context,
            ambient: Ambient::default(),
            expanded: Expanded::default(),
            hits: Vec::new(),
        }
    }

    /// The facts, mutably, for the host that resolves them.
    pub const fn ambient_mut(&mut self) -> &mut Ambient {
        &mut self.ambient
    }

    /// The facts.
    #[must_use]
    pub const fn ambient(&self) -> &Ambient {
        &self.ambient
    }

    /// Which sections are open.
    #[must_use]
    pub const fn expanded(&self) -> Expanded {
        self.expanded
    }

    /// Open or close the LSP section.
    pub const fn toggle_lsp(&mut self) {
        self.expanded.lsp = !self.expanded.lsp;
    }

    /// Open or close the MCP section.
    pub const fn toggle_mcp(&mut self) {
        self.expanded.mcp = !self.expanded.mcp;
    }

    /// Open or close the skill section.
    pub const fn toggle_skills(&mut self) {
        self.expanded.skills = !self.expanded.skills;
    }

    /// Open or close `section`.
    pub const fn toggle(&mut self, section: Section) {
        match section {
            Section::Lsp => self.toggle_lsp(),
            Section::Mcp => self.toggle_mcp(),
            Section::Skills => self.toggle_skills(),
        }
    }

    /// Toggle whichever section's heading occupies `(column, row)`, if any.
    ///
    /// Absolute frame coordinates, because that is what a `MouseEvent` carries and
    /// translating at the boundary is what keeps the caller from having to know this
    /// panel's geometry.
    ///
    /// The whole heading row is the target, rule column included: the row holds nothing but
    /// the label and its summary, so there is no neighbouring control a generous target
    /// could steal from — and a two-column triangle is a target most users miss.
    ///
    /// Answers `false` when nothing was hit, which is how the owner tells "consumed" from
    /// "pass it on". It is also `false` for every coordinate while mouse capture is off,
    /// because [`Component::render`] records no targets then.
    pub fn click(&mut self, column: u16, row: u16) -> bool {
        let Some(section) = self
            .hits
            .iter()
            .find(|(area, _)| {
                column >= area.left()
                    && column < area.right()
                    && row >= area.top()
                    && row < area.bottom()
            })
            .map(|(_, section)| *section)
        else {
            return false;
        };
        self.toggle(section);
        true
    }

    /// Discard the recorded heading positions.
    ///
    /// Called by the owner on any frame that does not draw this panel — the user hid it, or
    /// the pane fell under [`crate::views::SIDEBAR_MIN_WIDTH`]. Without it the last drawn
    /// geometry would keep answering clicks aimed at whatever now occupies those columns.
    pub fn forget_hit_targets(&mut self) {
        self.hits.clear();
    }

    fn health_style(&self, health: Health) -> Style {
        match health {
            Health::Ready => self.context.success(),
            Health::Pending => self.context.warning(),
            Health::Faulted => self.context.error(),
            Health::Disabled => self.context.muted(),
        }
    }

    fn heading(&self, label: &str, summary: &str, open: Option<bool>, width: u16) -> Line<'static> {
        let glyph = match open {
            Some(true) => OPEN_GLYPH,
            Some(false) => CLOSED_GLYPH,
            None => " ",
        };
        let head = format!("{glyph} {label}");
        let columns = usize::from(width);
        let used = display_width(&head) + display_width(summary);
        let gap = columns.saturating_sub(used).max(1);
        Line::from(vec![
            Span::styled(head, self.context.title()),
            Span::styled(" ".repeat(gap), self.context.surface()),
            Span::styled(summary.to_owned(), self.context.muted()),
        ])
    }

    fn service_row(&self, service: &Service, width: u16) -> Line<'static> {
        let columns = usize::from(width);
        // Truncated here rather than left to the frame: a name wider than the panel gets
        // cut either way, and cutting it in columns is what keeps the cut off the middle
        // of a wide glyph.
        let head = truncate(
            &format!("  {} {}", service.health.glyph(), service.name),
            columns,
        );
        let head_columns = display_width(&head);
        let mut spans = vec![Span::styled(head, self.health_style(service.health))];
        // The detail is dropped, never abbreviated to a stub. A long server name leaves
        // two or three columns, and `…d` is not a shorter way of saying `configured` —
        // it is a fragment a reader has to decode, while the health glyph already
        // carries the state the detail was repeating.
        //
        // Both the room and the pad are counted in columns. Counting characters made a
        // CJK-named server lose its detail outright: `◐ 语言服务器` measured 9 characters
        // where it occupies 14, so `正在启动中` was laid out starting past the right edge
        // and the reason the server had not come up never reached the screen — the one
        // thing the detail column exists to say.
        let room = columns.saturating_sub(head_columns + 1);
        let tail = if !service.detail.is_empty() && room >= DETAIL_MIN_COLUMNS {
            elide_left(&service.detail, room)
        } else {
            String::new()
        };
        let pad = columns.saturating_sub(head_columns + display_width(&tail));
        spans.push(Span::styled(" ".repeat(pad), self.context.surface()));
        if !tail.is_empty() {
            spans.push(Span::styled(tail, self.context.muted()));
        }
        Line::from(spans)
    }

    /// What a section with nothing in it says, so every section says it the same way.
    ///
    /// `none` rather than `0`. [`Self::summarise`] already answers `none` for an empty
    /// server list, so a panel that printed `Skills 0` two sections below `MCP none` made a
    /// reader stop and work out whether the two were reporting the same thing. They are.
    pub const EMPTY_SECTION: &'static str = "none";

    /// How a section states a plain count, with zero spelled [`Self::EMPTY_SECTION`].
    fn tally(total: usize) -> String {
        if total == 0 {
            return Self::EMPTY_SECTION.to_owned();
        }
        total.to_string()
    }

    fn summarise(services: &[Service]) -> String {
        if services.is_empty() {
            return Self::EMPTY_SECTION.to_owned();
        }
        let faulted = services
            .iter()
            .filter(|service| service.health == Health::Faulted)
            .count();
        let ready = services
            .iter()
            .filter(|service| service.health == Health::Ready)
            .count();
        if faulted > 0 {
            return format!("{ready} up, {faulted} failed");
        }
        format!("{ready}/{}", services.len())
    }

    /// Whether a collapsible section may advertise itself as collapsible.
    ///
    /// `None` — which [`Self::heading`] draws as a blank, exactly like the non-collapsible
    /// `Context` heading — whenever mouse capture is off, because a click is the only way
    /// to actuate one. A `▾` a user cannot press is worse than no `▾`: it invites a gesture
    /// the build has switched off and reports nothing when the gesture is made.
    ///
    /// Nothing is hidden by this. The state still holds, the summary still states the
    /// count, and each section's contents have their own keyboard surface — the MCP list,
    /// the status census and the skill selector — so the triangle is an affordance for a
    /// space problem, not the only route to the facts behind it.
    const fn disclosure(&self, open: bool) -> Option<bool> {
        if self.context.config.mouse {
            Some(open)
        } else {
            None
        }
    }

    /// The rows this panel draws at `width`.
    ///
    /// Public for the same reason the transcript's is: a row list is the readable
    /// surface to assert against, and the buffer test then proves the rows land.
    #[must_use]
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        self.rows(width).lines
    }

    /// The rows and, in the same pass, which of them is a section heading.
    ///
    /// One pass rather than a second method that recomputes the offsets: two collectors
    /// over one fact is how this panel came to advertise `0 lsp` while twenty servers ran,
    /// and here the drift would put a click on the row above the header it aimed at.
    fn rows(&self, width: u16) -> PanelRows {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut headers: Vec<(usize, Section)> = Vec::new();
        let blank = || padded("", width, self.context.surface());

        // Above `Context`, and above it specifically because this is the one row that says
        // *which* session the numbers below belong to. Everything else in the panel
        // describes the machine — token spend, servers, skills — and is true of any session
        // this build could be running; the name is the only fact that identifies this one,
        // so it reads first for the same reason a document's title precedes its body.
        //
        // Styled `title()` rather than `text()` so it does not read as another data row.
        if let Some(name) = &self.ambient.title {
            for row in wrap(name, usize::from(width), TITLE_MAX_ROWS) {
                lines.push(padded(&row, width, self.context.title()));
            }
            lines.push(blank());
        }

        let tokens = &self.ambient.tokens;
        lines.push(self.heading("Context", "", None, width));
        if self.ambient.usage_state == crate::views::message::UsageState::Unavailable {
            lines.push(padded("  — usage unavailable", width, self.context.muted()));
        } else if self.ambient.usage_state == crate::views::message::UsageState::NotReported {
            lines.push(padded(
                "  no usage reported yet",
                width,
                self.context.muted(),
            ));
        } else {
            lines.push(padded(
                &format!(
                    "  {} tokens",
                    crate::views::message::thousands(tokens.total())
                ),
                width,
                self.context.text(),
            ));
            lines.push(padded(
                &format!(
                    "  {} in · {} out",
                    compact(tokens.input),
                    compact(tokens.output)
                ),
                width,
                self.context.muted(),
            ));
            if tokens.cache_read > 0 || tokens.cache_write > 0 {
                lines.push(padded(
                    &format!(
                        "  {} cached · {} written",
                        compact(tokens.cache_read),
                        compact(tokens.cache_write)
                    ),
                    width,
                    self.context.muted(),
                ));
            }
            if let Some(percent) = self.ambient.context_used {
                lines.push(padded(
                    &format!("  {percent}% of the window"),
                    width,
                    if percent >= 80 {
                        self.context.warning()
                    } else {
                        self.context.muted()
                    },
                ));
            }
        }

        if !self.ambient.lsp.is_empty() {
            lines.push(blank());
            headers.push((lines.len(), Section::Lsp));
            lines.push(self.heading(
                "LSP",
                &Self::summarise(&self.ambient.lsp),
                self.disclosure(self.expanded.lsp),
                width,
            ));
            if self.expanded.lsp {
                for service in &self.ambient.lsp {
                    lines.push(self.service_row(service, width));
                }
            }
        }

        lines.push(blank());
        headers.push((lines.len(), Section::Mcp));
        lines.push(self.heading(
            "MCP",
            &Self::summarise(&self.ambient.mcp),
            self.disclosure(self.expanded.mcp),
            width,
        ));
        if self.expanded.mcp {
            if self.ambient.mcp.is_empty() {
                lines.push(padded("  none configured", width, self.context.muted()));
            } else {
                for service in &self.ambient.mcp {
                    lines.push(self.service_row(service, width));
                }
            }
        }

        lines.push(blank());
        headers.push((lines.len(), Section::Skills));
        let loaded = self
            .ambient
            .skills
            .iter()
            .filter(|skill| skill.loaded)
            .count();
        let skill_summary = if self.ambient.skills.is_empty() {
            Self::tally(0)
        } else {
            format!("{loaded}/{} loaded", self.ambient.skills.len())
        };
        lines.push(self.heading(
            "Skills",
            &skill_summary,
            self.disclosure(self.expanded.skills),
            width,
        ));
        if self.expanded.skills {
            if self.ambient.skills.is_empty() {
                lines.push(padded("  none discovered", width, self.context.muted()));
            } else {
                for skill in &self.ambient.skills {
                    lines.push(padded(
                        &format!("  {} {}", if skill.loaded { "✓" } else { "·" }, skill.name),
                        width,
                        if skill.loaded {
                            self.context.success()
                        } else {
                            self.context.text()
                        },
                    ));
                }
            }
        }

        PanelRows { lines, headers }
    }

    /// The rows drawn at the very bottom of the panel, whatever the content above.
    ///
    /// Location and version are pinned there rather than at the top because they are
    /// the two facts a user reads once and then stops looking at, and the top of the
    /// panel is where the numbers that change belong.
    #[must_use]
    pub fn footer_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(directory) = &self.ambient.directory {
            // The tail of a path is what identifies it, so an over-long one is cut at
            // the front and marked. A path cut at the end keeps the part every sibling
            // directory shares and discards the part that tells them apart.
            lines.push(padded(
                &elide_left(directory, usize::from(width)),
                width,
                self.context.muted(),
            ));
        }
        if let Some(branch) = &self.ambient.branch {
            // Its own condition, not nested under the directory. A branch is a fact about
            // the checkout and stays true whether or not the host resolved a display path,
            // and the nesting meant the one field that changes as a user works could be
            // suppressed by the absence of the one that does not.
            //
            // Written `⑂ main`, spaced, where the status strip writes `⑂main` tight
            // (`message.rs::BRANCH_GLYPH`). Both surfaces state the branch and the
            // difference is deliberate: the strip compacts every segment because it shares
            // one row with the turn state, while this panel owns the row and the space is
            // what stops the glyph reading as part of the name.
            lines.push(padded(
                &format!("{BRANCH_GLYPH} {branch}"),
                width,
                self.context.muted(),
            ));
        }
        if let Some(version) = &self.ambient.version {
            lines.push(padded(
                &format!("● zuno {version}"),
                width,
                self.context.success(),
            ));
        }
        lines
    }
}

impl Component for SidebarView {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        // Cleared before any early return, so a frame that draws nothing leaves no target
        // behind. Every path below that does draw refills it.
        self.hits.clear();
        fill(frame.buffer_mut(), area, self.context.surface());
        if area.width == 0 || area.height == 0 {
            return;
        }
        // The left rule is the same split border the dialog host draws, so a panel and
        // a prompt read as the same family of surface.
        for y in area.top()..area.bottom() {
            let cell = &mut frame.buffer_mut()[(area.left(), y)];
            cell.set_style(
                Style::new()
                    .fg(self.context.palette().border_subtle.into())
                    .bg(self.context.palette().background_panel.into()),
            );
            cell.set_symbol(ratatui::symbols::line::VERTICAL);
        }
        let inner = Rect {
            x: area.left() + 2,
            y: area.top(),
            width: area.width.saturating_sub(3),
            height: area.height,
        };
        if inner.width == 0 {
            return;
        }

        let footer = self.footer_lines(inner.width);
        let body_rows = usize::from(inner.height).saturating_sub(footer.len());
        let rows = self.rows(inner.width);
        // Recorded from `body_rows` — the count actually painted — not from `headers`. The
        // panel is clipped by `take` whenever the pane is short, and a heading that was
        // dropped has no row on screen for a click to land on. Recording it anyway would
        // make the section below the fold answer to a click on whatever survived there.
        if self.context.config.mouse {
            self.hits = rows
                .headers
                .iter()
                .filter(|(index, _)| *index < body_rows)
                .filter_map(|(index, section)| {
                    let offset = u16::try_from(*index).ok()?;
                    Some((
                        Rect {
                            y: inner.y.checked_add(offset)?,
                            height: 1,
                            // The whole panel row, not just the heading's own columns: the
                            // rule and the two-column indent belong to no other control.
                            x: area.x,
                            width: area.width,
                        },
                        *section,
                    ))
                })
                .collect();
        }
        let body = rows.lines.into_iter().take(body_rows).collect::<Vec<_>>();
        Paragraph::new(body)
            .style(self.context.surface())
            .render(inner, frame.buffer_mut());

        if footer.is_empty() {
            return;
        }
        let Ok(height) = u16::try_from(footer.len()) else {
            return;
        };
        if height > inner.height {
            return;
        }
        let region = Rect {
            y: inner.bottom() - height,
            height,
            ..inner
        };
        Paragraph::new(footer)
            .style(self.context.surface())
            .render(region, frame.buffer_mut());
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        // Token usage reaches the panel through its owner, which already folds the
        // provider stream for the transcript; folding it twice would be two copies of
        // the same running total drifting apart.
        EventResult::IGNORED
    }
}
