//! The view layer: the chat transcript, the input editor, autocomplete, and the
//! dialog set.
//!
//! # Capability, not pixels
//!
//! Upstream's view layer is 204 files and 31,729 lines
//! (`packages/tui/src/**`). This module reproduces its **capabilities** — what a
//! user can see and do — and deliberately not its layout. Where a concrete number
//! or vocabulary is a contract rather than a style choice (the permission prompt's
//! three replies, the `diff_style` fork, the scroll multiplier curve) it is ported
//! exactly and cited; where it is chrome, it is not.
//!
//! # Two disciplines inherited from the layers below
//!
//! A view never names a key and never names a colour.
//!
//! Keys arrive as resolved [`crate::keybind::Definition`] actions through
//! [`crate::keybind::KeyDispatcher`], so rebinding is invisible here. Colours come
//! from the resolved [`crate::theme::Palette`] carried by [`ViewContext`]. Both are
//! enforced by tests rather than convention: [`views_tests`] scans every file in
//! this directory for a literal colour and for a raw key spelling, with a floor on
//! the number of files scanned so the scan cannot pass by looking at nothing.
//!
//! # Dialogs are state, not calls
//!
//! No view in this module awaits an answer. A dialog is a value in the component
//! tree that renders, receives actions, and *resolves by emitting a result* — see
//! [`dialog`] for why the alternative deadlocks against the plugin host's terminal
//! lease.
//!
//! # Rendering is assertable without a terminal
//!
//! Every view is a [`crate::app::Component`], so
//! [`crate::app::render_offscreen`] draws it into a ratatui `TestBackend` buffer
//! with no TTY. That is how each view in this module is tested, including the two
//! the plan names specifically: a permission prompt resolving to each of
//! `once`/`always`/`reject`, and a message that renders incrementally as provider
//! deltas arrive.

use crate::config::{DiffStyle, ResolvedTuiConfig};
use crate::theme::{Mode, Palette, Resolved, ThemeRegistry};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::{Arc, PoisonError, RwLock};

pub mod ambient;
pub mod autocomplete;
pub mod basics;
pub mod diagnostics;
pub mod dialog;
pub mod diff;
pub mod diff_browser;
pub mod editor;
pub mod external;
pub mod help;
mod highlight;
pub mod lsp;
pub mod markdown;
pub mod message;
pub mod palette;
pub mod permission;
pub mod picker;
pub mod question;
pub mod scroll;
pub mod session;
pub mod slash;
pub mod subagent;
pub mod toast;
pub mod tool;
pub mod welcome;

#[cfg(test)]
#[path = "views/views_tests.rs"]
mod views_tests;

/// The terminal width above which an `auto` diff becomes two columns.
///
/// `packages/tui/src/routes/session/permission.tsx:41` — `dimensions().width > 120`.
/// A threshold rather than a ratio because a split diff needs a usable code column
/// on each side, and 60 columns is about the floor for that.
pub const SPLIT_DIFF_MIN_WIDTH: u16 = 120;

/// One resolved theme, shared by every clone of a [`ViewContext`].
///
/// `Arc<Resolved>` inside the lock rather than `Resolved`, so a reader clones a
/// pointer and drops the guard immediately instead of copying a fifty-field palette
/// or holding a lock across a render. See [`ViewContext`] for why the cell exists at
/// all.
type ThemeCell = Arc<RwLock<Arc<Resolved>>>;

/// Everything a view needs from the layers below it.
///
/// One value rather than a palette argument threaded through every `render`,
/// because the two things travel together: a view that paints also needs the
/// user's size, scroll, and diff preferences.
///
/// # Why the theme is shared and the configuration is not
///
/// A `ViewContext` is **cloned into every component at construction** —
/// [`session::SessionScreen::new`] hands one to each of its five children, every
/// picker gets one, [`dialog::DialogHost`] keeps its own. If the palette were an
/// owned field, changing one component's copy would re-theme that component and
/// nothing else, and the screen would render half in the new theme and half in the
/// old. Half a screen re-themed is worse than none.
///
/// Two shapes were weighed:
///
/// * **Push a `retheme` call down the component tree.** Every construction site and
///   every future one has to remember to forward it, and the one that forgets leaves
///   a surface painting yesterday's colours with nothing failing. It is the same trap
///   as the modal banner that outlived its dialog, which is why
///   [`crate::keybind::ActionComponent::observe_modal`] is *derived* every frame
///   rather than pushed on open and close.
/// * **Share one theme behind interior mutability**, which is what this is. There is
///   exactly one `Resolved` in the process; every painter reads it when it paints, so
///   there is no notification to forget and no component that can hold a stale
///   palette. Adding a view later costs nothing.
///
/// The configuration stays owned per clone because nothing changes it after startup:
/// [`crate::config::ResolvedTuiConfig`] is resolved once from the discovered files
/// and a second source of truth for it would buy nothing.
///
/// # Locking
///
/// [`Self::palette`] and [`Self::theme`] take the read lock, clone an `Arc`, and drop
/// the guard before returning, so no lock is ever held across a render or across a
/// call into another component. The cell is a leaf: nothing acquires
/// [`crate::app::UiState`]'s mutex while holding it, which matters because
/// `handle_event` — where [`Self::set_theme`] is reached from — already runs under
/// that mutex. A poisoned lock is read through rather than panicked on; a palette
/// somebody panicked beside is still a palette, and aborting the renderer over it
/// would turn a cosmetic fault into a dead terminal.
#[derive(Debug, Clone)]
pub struct ViewContext {
    /// The resolved theme. The only legal source of a colour in this module.
    theme: ThemeCell,
    /// The resolved TUI configuration.
    pub config: ResolvedTuiConfig,
}

/// A borrow of the active palette, held for as long as the caller needs it.
///
/// Returned instead of `&Palette` because the palette lives behind a lock, and
/// instead of `Palette` because a colour read happens hundreds of times per frame.
/// It `Deref`s, so `context.palette().text` and `selected_foreground(&palette, …)`
/// both read the way a plain field would.
///
/// Holding one pins the theme it was taken from, which is the property a single
/// `render` wants: a frame paints one theme even if a re-theme lands mid-frame.
#[derive(Debug, Clone)]
pub struct PaletteRef(Arc<Resolved>);

impl std::ops::Deref for PaletteRef {
    type Target = Palette;

    fn deref(&self) -> &Self::Target {
        &self.0.palette
    }
}

impl ViewContext {
    /// A context over a resolved theme and configuration.
    #[must_use]
    pub fn new(resolved: &Resolved, config: ResolvedTuiConfig) -> Self {
        Self {
            theme: Arc::new(RwLock::new(Arc::new(resolved.clone()))),
            config,
        }
    }

    /// The built-in default theme with default settings.
    ///
    /// The shape every test starts from, and the shape the TUI uses before a
    /// configuration file has been read.
    #[must_use]
    pub fn defaults() -> Self {
        let registry = ThemeRegistry::new();
        let resolved = registry.resolve(crate::theme::DEFAULT_THEME, Mode::Dark);
        Self::new(&resolved, ResolvedTuiConfig::default())
    }

    /// The theme in force, including the name it resolved under and its mode.
    ///
    /// The mode is read from here rather than re-derived, so the picker previews in
    /// the same light/dark mode the host resolved at startup
    /// (`ThemeRegistry::refresh_system_theme`, falling back to [`Mode::Dark`]). A
    /// second mode policy in the view layer would disagree with the first the day the
    /// terminal reported light.
    #[must_use]
    pub fn theme(&self) -> Arc<Resolved> {
        Arc::clone(&self.theme.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The colours in force.
    #[must_use]
    pub fn palette(&self) -> PaletteRef {
        PaletteRef(self.theme())
    }

    /// Repaint every surface in this context's tree with `resolved`.
    ///
    /// `&self` on purpose: the whole point is that a component holding a clone can
    /// change what *every* clone paints with, which an owned field could not express.
    ///
    /// This is a live view-layer switch and nothing else. It does not write the user's
    /// configuration file — the theme lasts for this session, and persisting it is a
    /// separate decision about mutating a file the user owns — and it does not touch
    /// session or turn state: no channel is sent on, so a running turn cannot be
    /// disturbed by a colour change. That is deliberate, and it is why the CLI's
    /// selection channel (which rebuilds the turn host) is *not* the route a theme
    /// takes.
    pub fn set_theme(&self, resolved: &Resolved) {
        let mut theme = self.theme.write().unwrap_or_else(PoisonError::into_inner);
        *theme = Arc::new(resolved.clone());
    }

    /// Body text on the panel background.
    #[must_use]
    pub fn text(&self) -> Style {
        let palette = self.palette();
        Style::new()
            .fg(palette.text.into())
            .bg(palette.background_panel.into())
    }

    /// De-emphasised text: labels, hints, and metadata.
    #[must_use]
    pub fn muted(&self) -> Style {
        let palette = self.palette();
        Style::new()
            .fg(palette.text_muted.into())
            .bg(palette.background_panel.into())
    }

    /// A section title.
    #[must_use]
    pub fn title(&self) -> Style {
        self.text().add_modifier(Modifier::BOLD)
    }

    /// An accent used for the active element's border and marker.
    #[must_use]
    pub fn accent(&self) -> Style {
        let palette = self.palette();
        Style::new()
            .fg(palette.border_active.into())
            .bg(palette.background_panel.into())
    }

    /// The style for a row the cursor is on.
    ///
    /// `crate::theme::selected_foreground` decides the foreground, because a theme
    /// that set `selectedListItemText` means it and a theme that did not needs a
    /// contrast-derived answer (`packages/tui/src/theme/index.ts:98-110`).
    #[must_use]
    pub fn selected(&self) -> Style {
        let palette = self.palette();
        let background = palette.primary;
        Style::new()
            .fg(crate::theme::selected_foreground(&palette, Some(background)).into())
            .bg(background.into())
    }

    /// A warning, used by every prompt that asks a human to decide.
    #[must_use]
    pub fn warning(&self) -> Style {
        let palette = self.palette();
        Style::new()
            .fg(palette.warning.into())
            .bg(palette.background_panel.into())
    }

    /// A failure or a rejection.
    #[must_use]
    pub fn error(&self) -> Style {
        let palette = self.palette();
        Style::new()
            .fg(palette.error.into())
            .bg(palette.background_panel.into())
    }

    /// A completed operation.
    #[must_use]
    pub fn success(&self) -> Style {
        let palette = self.palette();
        Style::new()
            .fg(palette.success.into())
            .bg(palette.background_panel.into())
    }

    /// Reasoning text, dimmed by the theme's own `thinkingOpacity`.
    ///
    /// Upstream composites `warning` at `thinkingOpacity` over the background
    /// (`routes/session/index.tsx:1645`). Terminals have no alpha channel, so the
    /// composite is performed here with [`crate::theme::tint`] and a concrete
    /// colour is emitted.
    #[must_use]
    pub fn thinking(&self) -> Style {
        let palette = self.palette();
        let color = crate::theme::tint(
            palette.background,
            palette.warning,
            palette.thinking_opacity,
        );
        Style::new()
            .fg(color.into())
            .bg(palette.background_panel.into())
    }

    /// The fill used behind a whole surface.
    #[must_use]
    pub fn surface(&self) -> Style {
        let palette = self.palette();
        Style::new()
            .fg(palette.text.into())
            .bg(palette.background_panel.into())
    }

    /// The fill used behind an inset element such as a footer or a menu.
    #[must_use]
    pub fn element(&self) -> Style {
        let palette = self.palette();
        Style::new()
            .fg(palette.text.into())
            .bg(palette.background_element.into())
    }

    /// `style` re-seated on the inset-element surface.
    ///
    /// Every named style on this type fixes a background as well as a foreground, and all but
    /// [`Self::selected`] fix it to `background_panel`. That is correct on the surfaces filled
    /// with [`Self::surface`] and wrong inside anything filled with [`Self::element`]: the span
    /// then repaints the element's background back to the panel's, one cell at a time, and the
    /// region loses the boundary its fill was drawn to give it. The prompt band paid for this
    /// exactly — filled in `element`, then written over in `accent` and `muted`, leaving a
    /// two-tone box whose gutter was the surface colour.
    ///
    /// Foreground and modifiers are kept, so a caller still says *what kind* of text it is and
    /// only the seat changes.
    #[must_use]
    pub fn on_element(&self, style: Style) -> Style {
        style.bg(self.palette().background_element.into())
    }

    /// How many columns wide a diff should be laid out, for this width.
    ///
    /// `permission.tsx:38-42`: an explicit `stacked` is always one column; `auto`
    /// splits only when the terminal is wide enough to carry two.
    #[must_use]
    pub fn diff_columns(&self, width: u16) -> DiffColumns {
        match self.config.diff_style {
            Some(DiffStyle::Stacked) => DiffColumns::Unified,
            Some(DiffStyle::Auto) | None => {
                if width > SPLIT_DIFF_MIN_WIDTH {
                    DiffColumns::Split
                } else {
                    DiffColumns::Unified
                }
            }
        }
    }
}

/// The layout a diff is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffColumns {
    /// One stacked column, additions and removals interleaved.
    Unified,
    /// Two columns, before on the left and after on the right.
    Split,
}

/// Pad `text` to `width` so a styled span fills its row.
///
/// Rows are padded rather than left ragged because a background colour that stops
/// mid-row reads as a rendering bug, and because a padded row makes an off-screen
/// buffer assertion positional instead of a substring search.
#[must_use]
pub fn padded(text: &str, width: u16, style: Style) -> Line<'static> {
    let width = usize::from(width);
    let mut owned = truncate(text, width);
    let used = display_width(&owned);
    if used < width {
        owned.extend(std::iter::repeat_n(' ', width - used));
    }
    Line::from(Span::styled(owned, style))
}

/// The terminal columns `text` occupies.
///
/// Not `chars().count()`. A CJK glyph occupies two cells, so a row padded by character
/// count overflows its frame by one column per wide glyph — measured as a skill
/// description running past the right edge, wrapping onto the next line, and pushing
/// every row below it down. Both mistakes are invisible to a test that counts characters,
/// which is why [`views_tests`] asserts columns.
///
/// The alternative — counting characters so that this helper agrees with every other width
/// decision in the module — was tried and is the wrong trade. Agreement is worth having, but
/// it is achieved by routing the other decisions through [`padded`] and [`truncate`], not by
/// making all of them undercount together: rows that agree with each other and disagree with
/// the terminal still wrap.
#[must_use]
pub fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// The longest prefix of `text` that fits in `width` columns.
///
/// Stops before a wide glyph that would straddle the boundary rather than splitting it:
/// half of a double-width cell is not something a terminal can draw, and writing one
/// leaves the rest of the row shifted by a column.
#[must_use]
pub fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars() {
        let cost = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + cost > width {
            break;
        }
        out.push(character);
        used += cost;
    }
    out
}

/// The terminal width at or above which the ambient sidebar is drawn.
///
/// A sidebar earns its columns only when the transcript keeps a usable measure
/// beside it. [`ambient::SIDEBAR_WIDTH`] out of 120 leaves 86 for the reply, which is
/// about where prose stops being comfortable; the same panel taken out of 100 would
/// leave the answer narrower than the column describing it.
///
/// Width is a necessary condition and not the only one: the panel is also withheld while the
/// transcript is empty, because there is nothing for it to describe then. That term lives in
/// [`session`]'s `sidebar_drawn` rather than in this constant, so the boundary a used session
/// observes stays exactly 120.
pub const SIDEBAR_MIN_WIDTH: u16 = 120;

/// The spelling a user would actually press for `action`, or `None` when unbound.
///
/// Overrides carried by the resolved configuration win over the shipped table, and that
/// is the whole point: a hint reading `enter` after the user rebound `input_submit` is a
/// lie, and a surface that told one would be worse than one that stayed quiet. The first
/// spelling is taken because a hint has room for one and the table lists its preferred
/// spelling first.
///
/// Note what this does **not** yet buy in production: nothing populates
/// [`crate::config::ResolvedTuiConfig::keybinds`] on the `tui` path — `cmd/tui.rs` builds
/// a `ResolvedTuiConfig::default()`, and `tui.json` discovery is explicitly out of scope
/// for [`crate::config`] (see that module's `# Scope`). So every hint today renders the
/// shipped default. The override branch is reachable and tested, and it is what makes
/// this function correct the day discovery lands; it is not a claim that a user's
/// `tui.json` is being read now.
#[must_use]
pub fn key_label(action: &str, context: &ViewContext) -> Option<String> {
    if let Some(value) = context.config.keybinds.get(action) {
        // A binding the user explicitly disabled yields nothing rather than falling
        // back to the default, which would advertise a key they switched off.
        return value
            .spellings()
            .first()
            .map(|spelling| (*spelling).to_owned());
    }
    crate::keybind::definition(action)?
        .keys
        .split(',')
        .map(str::trim)
        .find(|spelling| !spelling.is_empty() && *spelling != "none")
        .map(str::to_owned)
}

/// The chord a user can actually press for `action`, or `None` when there is none.
///
/// [`key_label`] is not enough on its own, and the difference is not cosmetic. It reads
/// [`crate::keybind::Definition::keys`], which is the **upstream** spelling and is
/// deliberately `"none"` for the six actions this build binds itself through
/// [`crate::keybind::SHIPPED_DEFAULTS`] — that table cannot move into `DEFINITIONS`
/// because `DEFINITIONS` is asserted row-for-row against upstream's own fixture. So for
/// `tool_details`, `display_thinking`, `diff_open`, `help_show`, `prompt_skills` and
/// `mcp_list`, `key_label` reports "unbound" about actions the running binary has bound.
/// Measured: the transcript's collapse notice rendered `… 9 more lines` with no key,
/// while `<leader>o` was live the whole time.
///
/// It also substitutes the leader token, which [`key_label`] cannot: a hint reading
/// `<leader>o` names a key no keyboard has.
///
/// Building a [`crate::keybind::Keymap`] per call rather than caching one: it is
/// constructed from the resolved configuration and this is reached once per collapsed
/// tool result, not per cell. A cached keymap would need invalidating on a config change
/// and would be a second source of truth for the binding — the trap
/// [`ViewContext`]'s shared theme exists to avoid.
///
/// `views/welcome.rs` resolves the same fact through its own private `spelling()`, which
/// predates this and does the same two steps. That is one derivation too many and should
/// collapse onto this function; it is left alone here only because this change's scope
/// does not include that file.
#[must_use]
pub fn pressable_label(action: &str, context: &ViewContext) -> Option<String> {
    crate::keybind::Keymap::from_config(&context.config)
        .ok()
        .and_then(|keymap| keymap.sequences(action).into_iter().next())
        .or_else(|| key_label(action, context))
        // A spelling that still carries the leader token after resolution is unreadable,
        // and an unreadable key is worse than a missing one — the same filter
        // `welcome.rs` applies for the same reason.
        .filter(|spelling| !spelling.contains(crate::keybind::LEADER_TOKEN))
}

/// A `key label` hint pair, the footer vocabulary every prompt shares.
#[must_use]
pub fn hint(key: &str, label: &str, context: &ViewContext) -> Vec<Span<'static>> {
    let palette = context.palette();
    vec![
        Span::styled(key.to_owned(), context.element()),
        Span::styled(String::from(" "), context.element()),
        Span::styled(
            label.to_owned(),
            Style::new()
                .fg(palette.text_muted.into())
                .bg(palette.background_element.into()),
        ),
        Span::styled(String::from("  "), context.element()),
    ]
}

/// Fill `area`'s cells with `style` before painting over them.
///
/// ratatui leaves untouched cells at `Color::Reset`, which on a terminal whose own
/// background differs from the theme's shows through as a stripe. Every surface in
/// this module fills first for that reason.
pub fn fill(buffer: &mut ratatui::buffer::Buffer, area: ratatui::layout::Rect, style: Style) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buffer[(x, y)].set_style(style);
            buffer[(x, y)].set_symbol(" ");
        }
    }
}

/// Whether a colour is the terminal's default rather than a theme colour.
///
/// Used by the palette-discipline test to tell "the view painted nothing here"
/// from "the view painted a literal".
#[must_use]
pub const fn is_reset(color: Color) -> bool {
    matches!(color, Color::Reset)
}

/// Test helpers shared by every view's test module.
///
/// In the crate rather than each test file because there are twelve of them and a
/// buffer-to-rows helper copied twelve times drifts twelve ways.
#[cfg(test)]
pub(crate) mod testkit {
    use crate::keybind::{Definition, definition};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::buffer::Buffer;

    /// One string per buffer row, trailing blanks trimmed.
    pub(crate) fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// A press of `code` with no modifiers.
    pub(crate) const fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// The binding-table row named `name`, panicking when the name is not real.
    ///
    /// Looking the action up rather than constructing a `Definition` is what keeps a
    /// view test from asserting against an action the shipped table does not have.
    pub(crate) fn action(name: &'static str) -> &'static Definition {
        definition(name).unwrap_or_else(|| panic!("`{name}` is not in the binding table"))
    }
}
