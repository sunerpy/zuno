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

pub mod ambient;
pub mod autocomplete;
pub mod dialog;
pub mod diff;
pub mod editor;
pub mod external;
pub mod help;
pub mod message;
pub mod permission;
pub mod picker;
pub mod question;
pub mod scroll;
pub mod session;
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

/// Everything a view needs from the layers below it.
///
/// One value rather than a palette argument threaded through every `render`,
/// because the two things travel together: a view that paints also needs the
/// user's size, scroll, and diff preferences, and both are immutable for the life
/// of a frame.
#[derive(Debug, Clone)]
pub struct ViewContext {
    /// The resolved colours. The only legal source of a colour in this module.
    pub palette: Palette,
    /// The resolved TUI configuration.
    pub config: ResolvedTuiConfig,
}

impl ViewContext {
    /// A context over a resolved theme and configuration.
    #[must_use]
    pub fn new(resolved: &Resolved, config: ResolvedTuiConfig) -> Self {
        Self {
            palette: resolved.palette.clone(),
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

    /// Body text on the panel background.
    #[must_use]
    pub fn text(&self) -> Style {
        Style::new()
            .fg(self.palette.text.into())
            .bg(self.palette.background_panel.into())
    }

    /// De-emphasised text: labels, hints, and metadata.
    #[must_use]
    pub fn muted(&self) -> Style {
        Style::new()
            .fg(self.palette.text_muted.into())
            .bg(self.palette.background_panel.into())
    }

    /// A section title.
    #[must_use]
    pub fn title(&self) -> Style {
        self.text().add_modifier(Modifier::BOLD)
    }

    /// An accent used for the active element's border and marker.
    #[must_use]
    pub fn accent(&self) -> Style {
        Style::new()
            .fg(self.palette.border_active.into())
            .bg(self.palette.background_panel.into())
    }

    /// The style for a row the cursor is on.
    ///
    /// `crate::theme::selected_foreground` decides the foreground, because a theme
    /// that set `selectedListItemText` means it and a theme that did not needs a
    /// contrast-derived answer (`packages/tui/src/theme/index.ts:98-110`).
    #[must_use]
    pub fn selected(&self) -> Style {
        let background = self.palette.primary;
        Style::new()
            .fg(crate::theme::selected_foreground(&self.palette, Some(background)).into())
            .bg(background.into())
    }

    /// A warning, used by every prompt that asks a human to decide.
    #[must_use]
    pub fn warning(&self) -> Style {
        Style::new()
            .fg(self.palette.warning.into())
            .bg(self.palette.background_panel.into())
    }

    /// A failure or a rejection.
    #[must_use]
    pub fn error(&self) -> Style {
        Style::new()
            .fg(self.palette.error.into())
            .bg(self.palette.background_panel.into())
    }

    /// A completed operation.
    #[must_use]
    pub fn success(&self) -> Style {
        Style::new()
            .fg(self.palette.success.into())
            .bg(self.palette.background_panel.into())
    }

    /// Reasoning text, dimmed by the theme's own `thinkingOpacity`.
    ///
    /// Upstream composites `warning` at `thinkingOpacity` over the background
    /// (`routes/session/index.tsx:1645`). Terminals have no alpha channel, so the
    /// composite is performed here with [`crate::theme::tint`] and a concrete
    /// colour is emitted.
    #[must_use]
    pub fn thinking(&self) -> Style {
        let color = crate::theme::tint(
            self.palette.background,
            self.palette.warning,
            self.palette.thinking_opacity,
        );
        Style::new()
            .fg(color.into())
            .bg(self.palette.background_panel.into())
    }

    /// The fill used behind a whole surface.
    #[must_use]
    pub fn surface(&self) -> Style {
        Style::new()
            .fg(self.palette.text.into())
            .bg(self.palette.background_panel.into())
    }

    /// The fill used behind an inset element such as a footer or a menu.
    #[must_use]
    pub fn element(&self) -> Style {
        Style::new()
            .fg(self.palette.text.into())
            .bg(self.palette.background_element.into())
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
    let mut owned = text.chars().take(width).collect::<String>();
    let len = owned.chars().count();
    if len < width {
        owned.extend(std::iter::repeat_n(' ', width - len));
    }
    Line::from(Span::styled(owned, style))
}

/// The terminal width at or above which the ambient sidebar is drawn.
///
/// A sidebar earns its columns only when the transcript keeps a usable measure
/// beside it. [`ambient::SIDEBAR_WIDTH`] out of 120 leaves 86 for the reply, which is
/// about where prose stops being comfortable; the same panel taken out of 100 would
/// leave the answer narrower than the column describing it.
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

/// A `key label` hint pair, the footer vocabulary every prompt shares.
#[must_use]
pub fn hint(key: &str, label: &str, context: &ViewContext) -> Vec<Span<'static>> {
    vec![
        Span::styled(key.to_owned(), context.element()),
        Span::styled(String::from(" "), context.element()),
        Span::styled(
            label.to_owned(),
            Style::new()
                .fg(context.palette.text_muted.into())
                .bg(context.palette.background_element.into()),
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
