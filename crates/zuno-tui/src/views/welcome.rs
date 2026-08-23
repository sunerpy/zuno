//! The welcome surface: what this is, where you are, and what to press.
//!
//! # Why an empty transcript was the worst possible first frame
//!
//! Measured on a 200×50 terminal before this module existed, `zuno tui` painted two
//! non-empty rows out of fifty: the word `idle` on its old status row, and a cursor.
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
//! # A row is earned by being the only place a fact appears
//!
//! Occupying space nothing else wants is not a licence to say things twice. Measured at
//! 120×34, the first draft spent twenty-two rows and stated the agent and model before
//! any reply had resolved them, the branch three times over (here, the old status trailer,
//! and the sidebar's footer), and a tagline that named no
//! fact at all — and at forty columns that tagline, the census and the tip were each cut
//! mid-word, so the rows that duplicated the most were also the ones that read as broken.
//!
//! The rule the surviving rows are chosen by is *whether the fact survives without them*,
//! and the answer differs per carrier because both other carriers degrade:
//!
//! * Agent and model belong to the reply identity, so the empty state does not claim a
//!   turn has resolved either one. The first response introduces that row.
//! * The branch stays here, priced at zero extra rows by sharing the directory's row.
//!   The sidebar is intentionally absent on this surface and the idle footer prioritizes
//!   the directory and command discovery.
//! * The sidebar is not drawn beside this screen **at any width** — see
//!   [`crate::views::session`]'s `sidebar_drawn`, which withholds the panel until there is a
//!   transcript for it to sit beside. So everything it alone would carry — the directory, the
//!   version, the census — stays here, and the duplication this bullet used to accept at 120
//!   columns no longer happens: below `SIDEBAR_MIN_WIDTH` the panel was already absent, and
//!   above it the panel is now absent too until the first message lands.
//!
//! Of the four references, `codex` states its model and directory and nothing else,
//! `jcode` draws no welcome at all on an authenticated session, and `claw-code` spends
//! sixteen rows. Eighteen is inside that band; twenty-two was above all of it.
//!
//! # Fourteen rows, and then nine of them above the input and four below
//!
//! Eighteen was inside the reference band and still wrong, and the reason is positional
//! rather than editorial. Every row above the input is a row the input sits further from the
//! middle of the frame, and cutting rows only ever moves it closer — it cannot get it there.
//! Measured at 120×32 the eighteen-row block left nine dead rows under the prompt and none
//! above the brand; the fourteen-row block that replaced it still put the band at rows 23–26
//! of 32. On a twenty-four-row pane the arithmetic makes it impossible outright: half the
//! frame is twelve rows and the block plus the fixed footer needed fifteen.
//!
//! So the trim below is real and kept, and the *position* is fixed separately by splitting
//! the surface at the input — [`WelcomeView::head`] above, [`WelcomeView::foot`] below. That
//! is also how the reference lays it out: `opencode` shows its logo, then the input, then its
//! hint row and tip line underneath. Nothing here is cut to achieve the split.
//!
//! Two blocks were cut, and both were cut for the same reason: something else already
//! answers the question they answered.
//!
//! * **The tip row is hidden by default** rather than deleted. It was the one block on this
//!   screen carrying neither a fact nor a key — prose about behaviour, which the behaviour
//!   itself teaches on the second turn. `tips_toggle` is a real upstream action, so
//!   deleting the row would have left a bound key that reaches nothing, which is precisely
//!   the defect class this surface exists to remove. Hidden-by-default turns that key into
//!   "show me a tip", and [`Self::next_tip`] already treats a hidden row that way.
//! * **The slash grid fell from six commands to three**, and lost its blank separator, so
//!   keys and commands now read as one two-row block: what you type on the left, what it
//!   does on the right. `/` lists every command and the palette chord lists all 184
//!   bindings, so rows four through six were teaching a list the user can already open —
//!   whereas nothing lists the send, newline and exit keys, which is why those three stayed.
//!
//! What survives does so because it has no other carrier at some supported width. The
//! location and census rows are the only carriers at **every** width, since the sidebar is
//! withheld from this screen entirely; `type / for commands` is now the *whole* of
//! command discovery, so it is the least cuttable row on the screen; and the key row spells
//! what `/` cannot express.
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
//! once, on an old status row whose hard-coded exit key went stale the moment overrides
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
/// Derived rather than chosen, and re-derived when the hints moved below the prompt. The
/// screen is now nine rows above the input — six of letterform, a separator and two facts —
/// and four below it: a separator, the lead line, and the two hint rows sharing one more
/// separator. With the prompt's four-row band and the one-row frame footer the whole screen
/// is eighteen rows, so eighteen is the first frame that holds all of it and twenty is that
/// figure with two rows to spare.
///
/// The spare rows are not slack, they are what the centring spends: at exactly eighteen the
/// band would have to sit flush against the head with nothing above the wordmark, which is the
/// flush-to-the-edge layout the arrangement exists to avoid. Two rows is the least that leaves
/// one on each side.
///
/// **Measured against the frame, not against the region the head is painted into.** The owner
/// bounds the tail by this head's height, so a fit decision that read the shrunken region
/// would be a fixpoint: a shorter region drops the wordmark, a shorter head allows a longer
/// tail, and a longer tail shortens the region again. See [`WelcomeView::head_rows`].
pub const WORDMARK_MIN_HEIGHT: u16 = 20;

/// The one-row brand used when the wordmark does not fit.
pub const COMPACT_BRAND: &str = "▌ ZUNO";

/// What separates two facts sharing the census row.
///
/// Tight, where the location row spaces its branch glyph out. Five facts share this row
/// and the wide `   ·   ` form spent twenty-eight of its columns on air — enough that at
/// forty columns the row was cut mid-count. A row carrying several facts compacts; a row
/// owning one fact does not.
const CENSUS_GAP: &str = " · ";

/// The rotating hints shown one at a time under the brand.
///
/// A pool rather than one fixed line, because a tip is worth reading only once. Each
/// entry is prose and names no key: keys belong in the grid below, where they are
/// resolved rather than spelled.
pub const TIPS: [&str; 12] = [
    "type a question and send it; there is no mode to enter first",
    "the reply identity names the agent and model actually in use",
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
/// Three rather than six, and the cut is about what a *list* is for.
///
/// Six commands over two rows plus a separator spent four rows advertising a list `/` opens
/// in one keystroke, on the screen that had just taught `/`. Three is enough to make the
/// convention concrete — the reader sees `/name does thing` and generalises — and the row
/// they cost is a row the input sits closer to the middle of the frame.
///
/// Which three: one for each question a first launch actually asks. `model` is what answers,
/// `session` is how to get back to yesterday, and `help` is the row that supersedes the
/// whole grid by listing every key. `agent`, `theme` and `mcp` went because each is a
/// setting a user goes looking for once they already know `/` exists, which the row above
/// has just told them.
pub const SLASH_HINTS: [SlashHint; 3] = [
    ("model", "switch model"),
    ("session", "past sessions"),
    ("help", "all keys"),
];

/// The facts the welcome screen states outright.
///
/// Every field is optional because the host resolves them at different moments, and a
/// welcome screen that waited for all of them would be blank exactly when it matters
/// most. An absent fact is omitted rather than shown as a placeholder: `unknown` in
/// the model row would be indistinguishable from a model actually called `unknown`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// There is deliberately **no agent or model field**. Those facts belong to the reply
/// identity once a turn resolves them; the empty state must not imply that a reply exists.
/// The fields are gone rather than merely unrendered so a caller cannot quietly restore
/// the premature duplicate.
pub struct WelcomeFacts {
    /// The working directory, already abbreviated for display.
    pub directory: Option<String>,
    /// The version-control branch, when the directory is a checkout.
    ///
    /// Kept because the sidebar is withheld from the welcome screen at every width and the
    /// fixed footer prioritizes the directory and command discovery. This is the only carrier
    /// here, and it shares the directory's row, so the cost is zero rows.
    pub branch: Option<String>,
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

    /// What this build is and what it brought, e.g. `zuno 0.1.0 · 13 tools · 2 mcp`.
    ///
    /// The version leads its own row rather than owning one. Both halves are read-once
    /// static facts — a user checks them and then stops looking — so the split into
    /// `zuno 0.1.0` above `13 tools · …` spent a row separating two things nobody reads
    /// separately. `codex` puts its version on the wordmark line for the same reason; ours
    /// is block glyphs and cannot carry text, so the census row is where it goes.
    ///
    /// A zero count is still shown rather than dropped: `0 mcp` is precisely the fact a
    /// user chasing a missing MCP tool needs, and omitting it would read as "not
    /// measured".
    ///
    /// **Segments are dropped from the end, never cut through.** That is the whole reason
    /// this takes a width. At forty columns the old pair of rows rendered
    /// `13 tools   ·   2 mcp   ·   1 lsp   ·   0` — a count severed from its noun, which a
    /// reader cannot tell from a smaller number. Whole facts or nothing, which is the rule
    /// [`crate::views::message::StatusView::line`] already applies to its own trailers.
    #[must_use]
    pub fn census(&self, width: u16) -> Option<String> {
        let segments = [
            self.version
                .as_ref()
                .map(|version| format!("zuno {version}")),
            self.tools.map(|count| format!("{count} tools")),
            self.mcp.map(|count| format!("{count} mcp")),
            self.lsp.map(|count| format!("{count} lsp")),
            self.skills.map(|count| format!("{count} skills")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let first = segments.first()?.clone();
        // Terminal columns, not `chars().count()`: a CJK skill count or a version string
        // carrying a wide glyph is otherwise under-measured by one column per glyph, and
        // the row overflows the frame — a mistake this crate has already made and now
        // measures for.
        (1..=segments.len())
            .rev()
            .map(|kept| segments[..kept].join(CENSUS_GAP))
            .find(|row| display_width(row) <= usize::from(width))
            // Even one segment can exceed a very narrow frame. Returning it anyway keeps
            // the row present and lets the paragraph clip it, which is what every other
            // row on this screen does; returning `None` would make the census vanish
            // entirely at the widths where the terminal is least self-explanatory.
            .or(Some(first))
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
                version: None,
                tools: None,
                mcp: None,
                lsp: None,
                skills: None,
            },
            tip: 0,
            tips_visible: false,
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
    ///
    /// Pinning an index does **not** reveal the row: the shipped composition hides it, so a
    /// fixture that named an index and thereby also un-hid it would be measuring a screen no
    /// user sees. [`Self::next_tip`] is what shows it, which is also what the bound key does.
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

    /// `entries` laid out in as many columns as `width` affords.
    ///
    /// The column count is derived rather than fixed so that eighty columns gets two
    /// readable columns and two hundred gets four, instead of one layout being cramped
    /// and the other stranded in the left third of the screen.
    ///
    /// Taken as a slice rather than reading a constant, because the grid is drawn twice —
    /// once for keys and once for slash commands — and each group then gets a cell width
    /// measured from its own entries. One shared width would pad the shorter group out to
    /// the longer group's widest row and open a gap the eye reads as a missing column.
    ///
    /// **No row budget, and that is what makes the block measurable.** This used to take the
    /// rows still unspent on the frame and truncate to them, which made the block's height a
    /// function of the height it was given — and the owner has to know that height *before*
    /// it can size the region, so the dependency was circular. Both groups are three entries
    /// now, so the widest they ever get is three rows at forty columns; a frame too short for
    /// that clips from the bottom, which drops the slash row before the key row, in the same
    /// priority order the budget used to enforce.
    fn grid(&self, entries: &[(String, &'static str)], width: u16) -> Vec<Line<'static>> {
        if entries.is_empty() {
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
        let shown = entries;
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

    /// The two hint groups as one block: keys, then slash commands, with no blank between.
    ///
    /// Keys come first because nothing anywhere else announces the send, newline or exit
    /// spelling, whereas `/` is taught by the line above and typing it lists every command —
    /// so if a short frame clips this block, the row it clips is the cheaper one.
    ///
    /// **The blank separator between the groups is gone.** It was there to say "these are two
    /// lists", and that reading was the problem: both groups share one accent-then-muted
    /// grammar — what you type on the left, what it does on the right — so the eye already
    /// learns one column meaning, and the blank spent a row insisting on a distinction the
    /// reader does not need to make.
    fn hint_block(&self, width: u16) -> Vec<Line<'static>> {
        let keymap = self.keymap();
        let resolved = KEY_HINTS
            .iter()
            .filter_map(|(action, label)| Some((self.spelling(keymap.as_ref(), action)?, *label)))
            .collect::<Vec<_>>();
        let mut block = self.grid(&resolved, width);

        let commands = SLASH_HINTS
            .iter()
            .map(|(name, label)| (format!("/{name}"), *label))
            .collect::<Vec<_>>();
        block.extend(self.grid(&commands, width));
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

    /// Every row this screen draws above the prompt at `width` by `height`, positioned.
    ///
    /// Public because it is the assertable surface: a claim about rows is readable
    /// where the same claim about cells is not, and the off-screen buffer test then
    /// proves the rows reach cells.
    ///
    /// `height` is both the region and the frame here, which is what a standalone render
    /// is. The composite passes them separately — see [`Self::lines_in`].
    #[must_use]
    pub fn lines(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        self.lines_in(width, height, height)
    }

    /// How many rows [`Self::head`] would occupy at `width` on a `frame`-row terminal.
    ///
    /// The owner calls this *before* it splits the frame, because the tail that centres the
    /// prompt is bounded by the rows the head needs — so the head's height is an input to the
    /// layout rather than an output of it. Measured by building the head rather than counted
    /// from a constant: a constant would be a second copy of the composition, and the copy
    /// that drifted would put the input a row off centre with nothing to notice it.
    #[must_use]
    pub fn head_rows(&self, width: u16, frame: u16) -> u16 {
        u16::try_from(self.head(width, frame).len()).unwrap_or(u16::MAX)
    }

    /// How many rows [`Self::foot`] would occupy at `width`.
    ///
    /// No `frame` argument, and the asymmetry with [`Self::head_rows`] is real: nothing in the
    /// foot degrades by height. The wordmark does, which is the whole reason the head has to
    /// be measured against the frame rather than the region it lands in.
    #[must_use]
    pub fn foot_rows(&self, width: u16) -> u16 {
        u16::try_from(self.foot(width).len()).unwrap_or(u16::MAX)
    }

    /// The head bottom-anchored in a `region`-row area, its fit decided by `frame`.
    ///
    /// **Bottom-anchored rather than centred, and the owner is why.** The head has to sit
    /// directly on top of the prompt, while the rows that push the band down to the middle
    /// belong *above* it. Centring within the region instead would split those rows in two
    /// and open a gap between the head and prompt, which is the "two blocks a third of a
    /// screen apart" reading this arrangement exists to avoid.
    ///
    /// `frame` rather than `region` decides the wordmark, for the reason
    /// [`WORDMARK_MIN_HEIGHT`] records: the region is derived from the head's height, so a
    /// head whose height depended on the region would be a fixpoint.
    fn lines_in(&self, width: u16, region: u16, frame: u16) -> Vec<Line<'static>> {
        let body = self.head(width, frame);
        let leading = usize::from(region).saturating_sub(body.len());
        let mut lines = Vec::with_capacity(leading + body.len());
        lines.extend(std::iter::repeat_n(
            padded("", width, self.context.surface()),
            leading,
        ));
        lines.extend(body);
        lines
    }

    /// What the screen *is*: the brand, and the facts that identify this checkout.
    ///
    /// The half that goes above the prompt, and it is short on purpose — see
    /// [`Self::foot`] for why the rest goes below.
    fn head(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        let mut body = Vec::new();
        if Self::wordmark_fits(width, height) {
            let indent = usize::from(width.saturating_sub(WORDMARK_WIDTH)) / 2;
            for row in WORDMARK {
                body.push(self.wordmark_row(row, indent));
            }
        } else {
            body.push(self.centred(COMPACT_BRAND, width, self.brand()));
        }

        let mut facts = Vec::new();
        if let Some(location) = self.facts.location() {
            facts.push((location, self.context.text()));
        }
        if let Some(census) = self.facts.census(width) {
            facts.push((census, self.context.muted()));
        }
        if !facts.is_empty() {
            body.push(padded("", width, self.context.surface()));
            for (text, style) in facts {
                body.push(self.centred(&text, width, style));
            }
        }

        body
    }

    /// How to reach everything else: the lead line, the optional tip, and the hint grid.
    ///
    /// # These rows are below the prompt, and that is what puts the prompt in the middle
    ///
    /// Every row this screen states above the input is a row the input sits further from the
    /// centre of the frame, and the arithmetic is unforgiving: the owner can only centre the
    /// band if the rows above it fit in half the frame minus the fixed footer. With all fourteen rows
    /// above, half of a twenty-four-row pane is twelve and the block alone needed fifteen — so
    /// on the shortest common terminal the band could not reach the middle *at any* tail
    /// length. Measured at 120x32 the band landed at rows 23–26 of 32 with fourteen dead rows
    /// under it, which is what was reported: an input box pinned near the bottom.
    ///
    /// Splitting is what makes it reachable rather than merely closer. The head is nine rows
    /// (six of letterform, a spacer and two facts), so the rows above the band are
    /// `9 + 1` — inside half of twenty-four — and the band centres exactly from there up.
    ///
    /// The split is also the reference layout. `opencode` puts its logo above the input and
    /// its `tab agents` / `ctrl+alt+l commands` hint row and its tip line *below*, and the
    /// reason is the same one that governs every row on this surface: the brand answers "what
    /// is this", which a reader wants before they type, and the hints answer "what else can I
    /// do", which they want after. Reading order and centring want the same arrangement.
    ///
    /// Nothing is cut to achieve it. All three groups still render, the tip still toggles, and
    /// the grid still degrades by column count — they are simply on the other side of the
    /// input, where the rows were dead anyway.
    fn foot(&self, width: u16) -> Vec<Line<'static>> {
        // The prompt's own spacer row is inside the band and carries the band's background, so
        // this blank is the first row of the *body* surface below the composer. Without it the
        // lead line sits flush against the box.
        let mut body = vec![padded("", width, self.context.surface())];
        body.push(self.command_row(width));

        if self.tips_visible {
            body.push(padded("", width, self.context.surface()));
            body.push(self.tip_row(width));
        }

        let grid = self.hint_block(width);
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

        body
    }

    /// Draw the rows that belong below the prompt into `area`, top-anchored.
    ///
    /// Top-anchored, not centred in the region: the lead line has to read as belonging to the
    /// input directly above it, and a hint group floating in the middle of the lower third
    /// reads as a third block on the screen. Clipped from the bottom when the region is short,
    /// which drops the slash row before the key row for the reason [`Self::grid`] records.
    ///
    /// A separate entry point rather than a taller [`Component::render`] area, because the two
    /// halves are on opposite sides of the prompt band the session owns, so there is no
    /// single `Rect` that could carry both.
    pub fn render_foot(&mut self, frame: &mut Frame<'_>, area: Rect) {
        fill(frame.buffer_mut(), area, self.context.surface());
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = self
            .foot(area.width)
            .into_iter()
            .take(usize::from(area.height))
            .collect::<Vec<_>>();
        Paragraph::new(lines)
            .style(self.context.surface())
            .render(area, frame.buffer_mut());
    }
}

impl Component for WelcomeView {
    /// Draws the head only. The owner draws the rest with [`Self::render_foot`].
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        fill(frame.buffer_mut(), area, self.context.surface());
        if area.width == 0 || area.height == 0 {
            return;
        }
        // `frame.area().height`, not `area.height`: the owner shrank this region by the tail
        // it computed from this head's own height, so reading the region back would let a
        // head that lost the wordmark earn a longer tail and lose it again. The frame is the
        // one height both sides can agree on. See `Self::head_rows`.
        let frame_height = frame.area().height;
        let lines = self
            .lines_in(area.width, area.height, frame_height)
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
