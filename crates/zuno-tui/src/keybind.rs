//! The keybind engine: the full upstream binding table, leader sequences, user
//! overrides, and conflict reporting.
//!
//! # Why the table lives here and not in a view
//!
//! The 184-entry table is the compatibility surface. A user who rebound
//! `session_compact` in a config written for the real binary must find the same
//! key doing the same thing here, so the table is data in one place rather than
//! `if key == 'x'` scattered through render code. Resolution therefore runs
//! key -> action, and [`KeyDispatcher`] hands the resolved [`Definition`] down to
//! an [`ActionComponent`]. A view never learns which key produced its action.
//!
//! Oracle: `packages/tui/src/config/keybind.ts` — `Definitions` (`:45-240`),
//! `CommandMap` (`:256-420`), `LeaderDefault` (`:41`). All 184 entries are
//! reproduced verbatim in [`DEFINITIONS`], and
//! `tests/fixtures/upstream-keybinds-1.18.13.tsv` is a mechanically extracted
//! copy of the same source that the tests diff the table against. An upstream
//! bump regenerates the fixture and the diff names whatever moved.
//!
//! # Scopes, and why conflicts are scope-local
//!
//! Upstream attaches bindings to renderables, so `diff_next_file` (`n`) and
//! `dialog.select.next` (`down`, `ctrl+n`) coexist because only one of their
//! owners is ever focused. Flattening that into one global map would make the
//! shipped defaults look like dozens of conflicts and force an arbitrary winner.
//!
//! Each action therefore carries a `scope`, derived mechanically from its name:
//! the namespace before the last `.` for a dotted name, otherwise the segment
//! before the first `_`. Measured against the real table this yields 39 scopes
//! and **zero** duplicate `(scope, sequence)` pairs — the defaults are internally
//! consistent, so any conflict a build reports comes from user config.
//!
//! Resolution takes an ordered active scope chain, which is the focus chain in
//! data form: the first scope with a match wins. That is explicit precedence,
//! not a silent duplicate. A duplicate *within* one scope has no such ordering
//! and is reported by [`Keymap::from_config`] with every action named.
//!
//! # Sequences
//!
//! `<leader>q` is not a special case. A spelling is a whitespace-separated list
//! of chords in which `<leader>` expands to the configured leader chord, so the
//! engine is a plain prefix matcher over chord sequences and supports
//! multi-chord bindings the upstream table happens not to use. A single action
//! can carry several comma-separated spellings, and one spelling may mix a plain
//! chord with a leader sequence — `app_exit` is `ctrl+c,ctrl+d,<leader>q`
//! (`keybind.ts:48`).

use crate::app::{AppEvent, Component, EventResult, TerminalEvent};
use crate::config::{BindingValue, ResolvedTuiConfig};
use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers as CrosstermModifiers,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::BitOr;
use std::time::{Duration, Instant};

#[cfg(test)]
#[path = "keybind_tests.rs"]
mod tests;

/// The name of the entry that configures the leader chord rather than an action.
pub const LEADER: &str = "leader";

/// The action that leaves the application.
pub const APP_EXIT: &str = "app_exit";

/// The token a spelling uses to mean "the configured leader chord".
pub const LEADER_TOKEN: &str = "<leader>";

/// One row of the upstream binding table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Definition {
    /// The configuration key, e.g. `session_compact` or `dialog.select.next`.
    pub name: &'static str,
    /// The conflict-detection namespace derived from [`Self::name`].
    pub scope: &'static str,
    /// The default spelling, comma-separated, or `none` for unbound.
    pub keys: &'static str,
    /// The command this action dispatches (`CommandMap`, falling back to `name`).
    pub command: &'static str,
    /// Whether the terminal's own handling of the key is suppressed.
    pub prevent_default: Option<bool>,
    /// The human-readable description shown by help and which-key surfaces.
    pub description: &'static str,
}

impl Definition {
    /// Whether this row configures the leader chord instead of an action.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.name == LEADER
    }
}

/// The keys this build gives to actions upstream ships with no key at all.
///
/// # Why these are not spelled in [`DEFINITIONS`]
///
/// [`DEFINITIONS`] is the compatibility surface and is asserted row-for-row against
/// `tests/fixtures/upstream-keybinds-1.18.13.tsv`, `keys` column included. Editing a
/// `"none"` row there to give it a chord would make this build claim upstream ships a
/// key it does not, and the parity guard would fail — correctly. So a binding this
/// build chooses is applied the same way a user's own binding is: as an override, in
/// [`crate::config::TuiConfig::resolve`], with the user's entry winning.
///
/// # Why bind them at all
///
/// Upstream reaches these through its command palette, so `"none"` costs it nothing.
/// Here an action with no key and no palette entry is unreachable, which means the
/// surface behind it is unreachable — the failure this project has removed repeatedly:
/// machinery built, tested, documented, and impossible to open. Each row below is an
/// action whose surface exists and works, and whose only missing piece was a key.
///
/// Every spelling is leader-prefixed or a function key. Neither can shadow text typed
/// into the prompt, which a bare letter would: an unmatched chord falls through to the
/// editor, but a *matched* one does not, so a bare `d` here would stop `d` reaching the
/// prompt. See [`crate::views::session::scopes`] for the other half of that hazard.
pub const SHIPPED_DEFAULTS: &[(&str, &str)] = &[
    // `f1` rather than `?`: `?` is typed with shift, so the event arrives carrying
    // `SHIFT` while the spelling `?` declares no modifier, and the two never match.
    // `f1` needs no modifier and is what every terminal user tries first for help.
    ("help_show", "f1"),
    // `d` for diff. Its scope carries the diff viewer's own bare keys, which is why it
    // has to be a leader sequence — see `views::session::scopes`.
    ("diff_open", "<leader>d"),
    // `i` for the model's inner reasoning. `t` would read better but belongs to
    // `theme_list`.
    ("display_thinking", "<leader>i"),
    // `o` for a tool's output, which is what the toggle actually hides and shows.
    ("tool_details", "<leader>o"),
    // `k` for skills: `s` is `status_view` and `x` is `session_export`.
    ("prompt_skills", "<leader>k"),
    // `p` for the protocol servers. `m` is `model_list` and `c` is `session_compact`.
    ("mcp_list", "<leader>p"),
];

/// The spelling this build resolves `definition` to before user config is consulted.
///
/// Applied here, in the one funnel every keymap is built through, rather than in
/// [`crate::config::TuiConfig::resolve`]: the host builds its config with
/// `ResolvedTuiConfig::default()` and never calls `resolve`, so a default applied there
/// would be a key the table claimed and the running binary did not have.
fn shipped_spelling(definition: &Definition) -> &'static str {
    if definition.keys != NO_KEY {
        return definition.keys;
    }
    SHIPPED_DEFAULTS
        .iter()
        .find(|(action, _)| *action == definition.name)
        .map_or(definition.keys, |(_, spelling)| *spelling)
}

/// The sentinel [`DEFINITIONS`] uses for an action upstream ships with no key.
pub const NO_KEY: &str = "none";

/// The default spelling recorded for `name`, if the table has that action.
#[must_use]
pub fn default_spelling(name: &str) -> Option<&'static str> {
    DEFINITIONS
        .iter()
        .chain(LOCAL_DEFINITIONS.iter())
        .find(|definition| definition.name == name)
        .map(|definition| definition.keys)
}

/// The table row for `name`, if it exists.
#[must_use]
pub fn definition(name: &str) -> Option<&'static Definition> {
    DEFINITIONS
        .iter()
        .chain(LOCAL_DEFINITIONS.iter())
        .find(|definition| definition.name == name)
}

/// Whether `chord` is a single-chord spelling the table gives to [`APP_EXIT`].
///
/// Exit intent is a property of the **chord**, not of the action a scope chain
/// happened to resolve it to. `ctrl+c` and `ctrl+d` are each claimed by several
/// scopes — `input_clear`, `input_delete`, `session_delete`, `stash_delete` — so an
/// action name cannot tell a component whether the user asked to leave. Asking
/// about the chord can, and it is also what stops `delete`, the other spelling of
/// `input_delete`, from quitting an application it was never bound to exit.
///
/// Derived from the table rather than hard-coded so that regenerating [`DEFINITIONS`]
/// from a newer upstream keeps this in step. Leader sequences are excluded because a
/// multi-chord spelling cannot be recognised from one press; `<leader>q` still
/// reaches [`APP_EXIT`] through ordinary resolution.
#[must_use]
pub fn is_exit_chord(chord: Chord) -> bool {
    definition(APP_EXIT).is_some_and(|exit| {
        exit.keys
            .split(',')
            .map(str::trim)
            .filter(|spelling| !spelling.contains(LEADER_TOKEN))
            .filter_map(|spelling| Chord::parse(spelling).ok())
            .any(|bound| bound == chord)
    })
}

/// Whether `event` asked to leave the application.
///
/// The seam a component uses when it holds a [`Definition`] and a [`KeyEvent`] but
/// must not depend on which scope won: see [`is_exit_chord`].
#[must_use]
pub fn is_exit_request(event: &KeyEvent) -> bool {
    Chord::from_key_event(event).is_some_and(is_exit_chord)
}

/// The chord modifiers a terminal can report.
///
/// Mirrors `crossterm::event::KeyModifiers` rather than upstream's `KeyStroke`
/// (`keybind.ts:8-15`), because crossterm is what actually delivers events here.
/// The spelling token `alt` sets [`Modifiers::ALT`]; `meta` sets
/// [`Modifiers::META`], which is the flag upstream's `KeyStroke.meta` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    /// No modifiers.
    pub const NONE: Self = Self(0);
    /// Control.
    pub const CTRL: Self = Self(1 << 0);
    /// Alt / Option.
    pub const ALT: Self = Self(1 << 1);
    /// Shift.
    pub const SHIFT: Self = Self(1 << 2);
    /// Super / Command / Windows.
    pub const SUPER: Self = Self(1 << 3);
    /// Hyper.
    pub const HYPER: Self = Self(1 << 4);
    /// Meta.
    pub const META: Self = Self(1 << 5);

    /// Rendering and parsing order, which is also upstream's (`ctrl+alt+shift+k`).
    const NAMED: [(Self, &'static str); 6] = [
        (Self::CTRL, "ctrl"),
        (Self::ALT, "alt"),
        (Self::SHIFT, "shift"),
        (Self::SUPER, "super"),
        (Self::HYPER, "hyper"),
        (Self::META, "meta"),
    ];

    /// Whether every flag in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no flag is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The same flags with everything in `other` cleared.
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    fn parse_token(token: &str) -> Option<Self> {
        Self::NAMED
            .iter()
            .find(|(_, name)| *name == token)
            .map(|(flag, _)| *flag)
    }
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl fmt::Display for Modifiers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (flag, name) in Modifiers::NAMED {
            if self.contains(flag) {
                write!(formatter, "{name}+")?;
            }
        }
        Ok(())
    }
}

/// A physical key, restricted to what both crossterm and the table use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// A printable character. Space is `Char(' ')`.
    Char(char),
    /// Enter / Return.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Shift-Tab.
    BackTab,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Insert.
    Insert,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// A function key.
    Function(u8),
}

impl Key {
    /// Every accepted spelling of a named key, first spelling being canonical.
    const NAMED: [(Self, &'static [&'static str]); 15] = [
        (Self::Enter, &["return", "enter"]),
        (Self::Escape, &["escape", "esc"]),
        (Self::Tab, &["tab"]),
        (Self::BackTab, &["backtab"]),
        (Self::Backspace, &["backspace"]),
        (Self::Delete, &["delete", "del"]),
        (Self::Insert, &["insert", "ins"]),
        (Self::Left, &["left"]),
        (Self::Right, &["right"]),
        (Self::Up, &["up"]),
        (Self::Down, &["down"]),
        (Self::Home, &["home"]),
        (Self::End, &["end"]),
        (Self::PageUp, &["pageup", "pgup"]),
        (Self::PageDown, &["pagedown", "pgdn"]),
    ];

    fn parse(token: &str) -> Option<Self> {
        if token == "space" {
            return Some(Self::Char(' '));
        }
        let lowered = token.to_ascii_lowercase();
        if let Some((key, _)) = Self::NAMED
            .iter()
            .find(|(_, spellings)| spellings.contains(&lowered.as_str()))
        {
            return Some(*key);
        }
        if let Some(number) = lowered.strip_prefix('f')
            && !number.is_empty()
            && let Ok(index) = number.parse::<u8>()
            && (1..=24).contains(&index)
        {
            return Some(Self::Function(index));
        }
        let mut characters = token.chars();
        match (characters.next(), characters.next()) {
            (Some(character), None) => Some(Self::Char(character)),
            _ => None,
        }
    }

    fn from_crossterm(code: KeyCode) -> Option<Self> {
        Some(match code {
            KeyCode::Char(character) => Self::Char(character),
            KeyCode::Enter => Self::Enter,
            KeyCode::Esc => Self::Escape,
            KeyCode::Tab => Self::Tab,
            KeyCode::BackTab => Self::BackTab,
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Delete => Self::Delete,
            KeyCode::Insert => Self::Insert,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::F(index) => Self::Function(index),
            _ => return None,
        })
    }
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char(' ') => formatter.write_str("space"),
            Self::Char(character) => write!(formatter, "{character}"),
            Self::Function(index) => write!(formatter, "f{index}"),
            other => {
                let (_, spellings) = Key::NAMED
                    .iter()
                    .find(|(key, _)| key == other)
                    .ok_or(fmt::Error)?;
                formatter.write_str(spellings[0])
            }
        }
    }
}

/// One key press: modifiers plus a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    /// The modifier flags, already normalized.
    pub modifiers: Modifiers,
    /// The key, already normalized.
    pub key: Key,
}

impl Chord {
    /// Build a normalized chord.
    ///
    /// Two normalizations exist because a terminal cannot report the difference
    /// the un-normalized forms would imply:
    ///
    /// * An uppercase ASCII letter becomes lowercase plus shift, so the table's
    ///   `E` (`keybind.ts:64`) and a user's `shift+e` are one chord, and an event
    ///   arriving as `E` with or without the shift flag still matches.
    /// * Shift is cleared from a non-alphabetic character, because the shifted
    ///   glyph already encodes it: `?` (`keybind.ts:75`) is what a terminal sends
    ///   for shift-slash, sometimes with the flag set and sometimes without.
    #[must_use]
    pub fn new(modifiers: Modifiers, key: Key) -> Self {
        match key {
            Key::Char(character) if character.is_ascii_uppercase() => Self {
                modifiers: modifiers | Modifiers::SHIFT,
                key: Key::Char(character.to_ascii_lowercase()),
            },
            Key::Char(character) if !character.is_alphabetic() => Self {
                modifiers: modifiers.without(Modifiers::SHIFT),
                key,
            },
            _ => Self { modifiers, key },
        }
    }

    /// Parse a single chord spelling such as `ctrl+alt+shift+k` or `]`.
    pub fn parse(spelling: &str) -> Result<Self, SpellingError> {
        let (modifier_part, key_part) = split_chord(spelling)?;
        let mut modifiers = Modifiers::NONE;
        for token in modifier_part.split('+').filter(|part| !part.is_empty()) {
            let flag = Modifiers::parse_token(&token.to_ascii_lowercase())
                .ok_or_else(|| SpellingError::UnknownModifier(token.to_owned()))?;
            modifiers = modifiers | flag;
        }
        let key =
            Key::parse(key_part).ok_or_else(|| SpellingError::UnknownKey(key_part.to_owned()))?;
        Ok(Self::new(modifiers, key))
    }

    /// Convert a crossterm key event, or `None` for a key this engine does not model.
    #[must_use]
    pub fn from_key_event(event: &KeyEvent) -> Option<Self> {
        let key = Key::from_crossterm(event.code)?;
        let mut modifiers = Modifiers::NONE;
        for (crossterm_flag, flag) in [
            (CrosstermModifiers::CONTROL, Modifiers::CTRL),
            (CrosstermModifiers::ALT, Modifiers::ALT),
            (CrosstermModifiers::SHIFT, Modifiers::SHIFT),
            (CrosstermModifiers::SUPER, Modifiers::SUPER),
            (CrosstermModifiers::HYPER, Modifiers::HYPER),
            (CrosstermModifiers::META, Modifiers::META),
        ] {
            if event.modifiers.contains(crossterm_flag) {
                modifiers = modifiers | flag;
            }
        }
        Some(Self::new(modifiers, key))
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", self.modifiers, self.key)
    }
}

/// A chord spelling could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpellingError {
    /// The spelling had no key part.
    #[error("the key spelling is empty")]
    Empty,
    /// A `+`-separated token was not a modifier name.
    #[error("`{0}` is not a modifier; expected one of ctrl, alt, shift, super, hyper, meta")]
    UnknownModifier(String),
    /// The final token was not a key name or a single character.
    #[error("`{0}` is not a key name")]
    UnknownKey(String),
}

/// Split a chord spelling into its modifier prefix and its key token.
///
/// A trailing `+` is the key itself, so `ctrl++` binds control-plus rather than
/// producing an empty key token.
fn split_chord(spelling: &str) -> Result<(&str, &str), SpellingError> {
    if spelling.is_empty() {
        return Err(SpellingError::Empty);
    }
    match spelling.rfind('+') {
        None => Ok(("", spelling)),
        Some(index) if index + 1 < spelling.len() => {
            Ok((&spelling[..index], &spelling[index + 1..]))
        }
        Some(index) => {
            let head = &spelling[..index];
            Ok((head.strip_suffix('+').unwrap_or(head), "+"))
        }
    }
}

/// Expand one spelling into a chord sequence, substituting the leader chord.
pub fn parse_sequence(spelling: &str, leader: Chord) -> Result<Vec<Chord>, SpellingError> {
    let mut sequence = Vec::new();
    for (index, part) in spelling.split(LEADER_TOKEN).enumerate() {
        if index > 0 {
            sequence.push(leader);
        }
        for token in part.split_whitespace() {
            sequence.push(Chord::parse(token)?);
        }
    }
    if sequence.is_empty() {
        return Err(SpellingError::Empty);
    }
    Ok(sequence)
}

/// Render a chord sequence the way a conflict report and a which-key panel show it.
#[must_use]
pub fn render_sequence(sequence: &[Chord]) -> String {
    sequence
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Why two bindings in one scope cannot both work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    /// Several actions claim the same sequence.
    Duplicate,
    /// A short sequence fires first, making a longer one starting with it dead.
    PrefixShadow {
        /// The longer sequence that can never be reached.
        longer: String,
    },
}

/// One reported keybind collision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The scope both bindings live in.
    pub scope: String,
    /// The colliding sequence, rendered.
    pub sequence: String,
    /// What kind of collision this is.
    pub kind: ConflictKind,
    /// Every action involved, sorted, always at least two.
    pub actions: Vec<String>,
}

impl fmt::Display for Conflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ConflictKind::Duplicate => write!(
                formatter,
                "`{}` is bound to {} in scope `{}`",
                self.sequence,
                join_actions(&self.actions),
                self.scope
            ),
            ConflictKind::PrefixShadow { longer } => write!(
                formatter,
                "`{}` is bound to {} in scope `{}`, which shadows the longer sequence `{longer}`",
                self.sequence,
                join_actions(&self.actions),
                self.scope
            ),
        }
    }
}

/// Name every action, so a report never hides one behind a first-wins rule.
fn join_actions(actions: &[String]) -> String {
    match actions {
        [] => "nothing".to_owned(),
        [only] => format!("`{only}`"),
        [first, second] => format!("both `{first}` and `{second}`"),
        [head @ .., last] => format!(
            "all of {}, and `{last}`",
            head.iter()
                .map(|action| format!("`{action}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The keymap could not be built from the configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeybindError {
    /// The configuration named keybinds the table does not have.
    #[error("unrecognized keybind{}: {}", plural(names.len()), names.join(", "))]
    UnknownActions {
        /// The offending names, sorted.
        names: Vec<String>,
    },
    /// A spelling could not be parsed.
    #[error("`{action}` has an invalid key spelling `{spelling}`: {source}")]
    InvalidSpelling {
        /// The action whose spelling failed.
        action: String,
        /// The rejected spelling.
        spelling: String,
        /// Why it was rejected.
        source: SpellingError,
    },
    /// The leader was unbound while leader sequences still exist.
    #[error(
        "the leader key cannot be unbound while {count} binding{} use `<leader>`; rebind `leader` instead",
        plural(*count)
    )]
    LeaderDisabled {
        /// How many bindings would have become unreachable.
        count: usize,
    },
    /// One or more scopes contain colliding bindings.
    #[error("{}", render_conflicts(conflicts))]
    Conflicts {
        /// Every collision found, in scope then sequence order.
        conflicts: Vec<Conflict>,
    },
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn render_conflicts(conflicts: &[Conflict]) -> String {
    let mut rendered = format!(
        "{} keybind conflict{}:",
        conflicts.len(),
        plural(conflicts.len())
    );
    for conflict in conflicts {
        rendered.push_str("\n  ");
        rendered.push_str(&conflict.to_string());
    }
    rendered
}

/// One resolvable binding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    sequence: Vec<Chord>,
    definition: &'static Definition,
    prevent_default: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    chords: Vec<Chord>,
    since: Instant,
}

/// What one key press meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A complete sequence resolved to an action.
    Action {
        /// The table row that fired.
        definition: &'static Definition,
        /// Whether the terminal's own handling should be suppressed.
        prevent_default: Option<bool>,
    },
    /// The chord begins a longer sequence; the engine is waiting for the rest.
    Pending,
    /// Nothing in the active scopes matched. Any pending sequence was abandoned.
    Unmatched,
}

/// The pending leader sequence and everything it can still become.
///
/// Carried together because a consumer that held only the chords would have to find the
/// continuations itself, from a scope chain only [`KeyDispatcher`] knows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingPrefix {
    /// The chords pressed so far. Empty once the sequence resolved or was abandoned.
    pub chords: Vec<Chord>,
    /// What the next press can do.
    pub continuations: Vec<Continuation>,
}

impl PendingPrefix {
    /// Whether a sequence is in flight.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.chords.is_empty()
    }

    /// The chords pressed so far, rendered as one label.
    #[must_use]
    pub fn label(&self) -> String {
        render_sequence(&self.chords)
    }
}

/// One way a pending sequence can be completed.
///
/// `keys` is the remainder still to press, not the whole sequence: after `ctrl+x` the
/// user needs to know `d`, and showing `ctrl+x d` would read as a second leader press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuation {
    /// The chords left to press, rendered.
    pub keys: String,
    /// The row that will fire.
    pub definition: &'static Definition,
}

/// The resolved binding table plus the pending-sequence state machine.
#[derive(Debug, Clone)]
pub struct Keymap {
    leader: Chord,
    leader_timeout: Duration,
    scopes: BTreeMap<&'static str, Vec<Binding>>,
    pending: Option<Pending>,
}

impl Keymap {
    /// Build the shipped defaults with no overrides.
    pub fn defaults() -> Result<Self, KeybindError> {
        Self::from_config(&ResolvedTuiConfig::default())
    }

    /// Build the table with the configuration's overrides and leader timeout applied.
    ///
    /// Every conflict is collected before returning, so one bad config produces
    /// one report naming all of them rather than a fix-and-rerun loop.
    pub fn from_config(config: &ResolvedTuiConfig) -> Result<Self, KeybindError> {
        let overrides = &config.keybinds;
        let unknown = overrides
            .keys()
            .filter(|name| definition(name).is_none())
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Err(KeybindError::UnknownActions { names: unknown });
        }

        let leader = Self::resolve_leader(overrides)?;
        let mut scopes: BTreeMap<&'static str, Vec<Binding>> = BTreeMap::new();
        for definition in DEFINITIONS
            .iter()
            .chain(LOCAL_DEFINITIONS.iter())
            .filter(|row| !row.is_leader())
        {
            let value = overrides
                .get(definition.name)
                .cloned()
                .unwrap_or_else(|| BindingValue::parse(shipped_spelling(definition)));
            let prevent_default = match &value {
                BindingValue::Keys(items) => items
                    .iter()
                    .find_map(|item| item.prevent_default)
                    .or(definition.prevent_default),
                BindingValue::Disabled => definition.prevent_default,
            };
            let entries = scopes.entry(definition.scope).or_default();
            for spelling in value.spellings() {
                let sequence = parse_sequence(spelling, leader).map_err(|source| {
                    KeybindError::InvalidSpelling {
                        action: definition.name.to_owned(),
                        spelling: spelling.to_owned(),
                        source,
                    }
                })?;
                // The same action listing one spelling twice is a typo, not a
                // conflict; only distinct actions colliding is worth reporting.
                if entries.iter().any(|entry| {
                    entry.definition.name == definition.name && entry.sequence == sequence
                }) {
                    continue;
                }
                entries.push(Binding {
                    sequence,
                    definition,
                    prevent_default,
                });
            }
        }

        let conflicts = collect_conflicts(&scopes);
        if !conflicts.is_empty() {
            return Err(KeybindError::Conflicts { conflicts });
        }

        Ok(Self {
            leader,
            leader_timeout: config.leader_timeout,
            scopes,
            pending: None,
        })
    }

    fn resolve_leader(overrides: &BTreeMap<String, BindingValue>) -> Result<Chord, KeybindError> {
        let row = definition(LEADER).ok_or_else(|| KeybindError::UnknownActions {
            names: vec![LEADER.to_owned()],
        })?;
        let value = overrides
            .get(LEADER)
            .cloned()
            .unwrap_or_else(|| BindingValue::parse(row.keys));
        let spellings = value.spellings();
        let Some(spelling) = spellings.first() else {
            return Err(KeybindError::LeaderDisabled {
                count: DEFINITIONS
                    .iter()
                    .filter(|definition| definition.keys.contains(LEADER_TOKEN))
                    .count(),
            });
        };
        Chord::parse(spelling).map_err(|source| KeybindError::InvalidSpelling {
            action: LEADER.to_owned(),
            spelling: (*spelling).to_owned(),
            source,
        })
    }

    /// The configured leader chord.
    #[must_use]
    pub const fn leader(&self) -> Chord {
        self.leader
    }

    /// The configured leader timeout.
    #[must_use]
    pub const fn leader_timeout(&self) -> Duration {
        self.leader_timeout
    }

    /// The chords accumulated so far while a sequence is incomplete.
    #[must_use]
    pub fn pending(&self) -> &[Chord] {
        self.pending
            .as_ref()
            .map_or(&[], |pending| pending.chords.as_slice())
    }

    /// Every scope that has at least one binding.
    #[must_use]
    pub fn scope_names(&self) -> Vec<&'static str> {
        self.scopes.keys().copied().collect()
    }

    /// The rendered sequences bound to `action`, in table order.
    #[must_use]
    pub fn sequences(&self, action: &str) -> Vec<String> {
        self.scopes
            .values()
            .flatten()
            .filter(|binding| binding.definition.name == action)
            .map(|binding| render_sequence(&binding.sequence))
            .collect()
    }

    /// What the pending sequence can still complete to, in `scopes` order.
    ///
    /// Derived from the same two things [`Self::resolve`] reads — this `scopes` map and
    /// [`starts_with`] — so a which-key panel cannot name a key the next press will not
    /// honour. Living in the view instead is what would allow that disagreement.
    ///
    /// Scope order is precedence, as in [`Self::resolve`]: the first scope that binds a
    /// sequence owns it, so a later scope's row for the same remainder is dropped rather
    /// than listed twice. Empty while nothing is pending — the question it answers is
    /// "I pressed the leader, what now?".
    ///
    /// # The order is load-bearing, and it is not alphabetical
    ///
    /// Rows come back in scope-precedence order and, within a scope, in [`DEFINITIONS`]
    /// order. Sorting by spelling was tried and reverted: it puts `1`-`9` first, and the
    /// leader's nine `session_quick_switch_*` rows then fill a narrow panel with nine lines of
    /// `Switch to session in quick slot N` while `List all sessions` and
    /// `Create a new session` fall past the cut. A caller that re-sorts this reintroduces
    /// that, so [`crate::views::autocomplete`] has a test holding the order.
    #[must_use]
    pub fn continuations(&self, scopes: &[&str]) -> Vec<Continuation> {
        let prefix = self.pending();
        if prefix.is_empty() {
            return Vec::new();
        }
        let mut claimed: BTreeSet<Vec<Chord>> = BTreeSet::new();
        let mut seen: BTreeSet<&'static str> = BTreeSet::new();
        let mut found = Vec::new();
        for scope in scopes {
            for binding in self.scopes.get(scope).into_iter().flatten() {
                if !starts_with(&binding.sequence, prefix) {
                    continue;
                }
                // A sequence already claimed by an earlier scope resolves there, so
                // listing this row would advertise a key that fires something else.
                if !claimed.insert(binding.sequence.clone()) {
                    continue;
                }
                if !seen.insert(binding.definition.name) {
                    continue;
                }
                found.push(Continuation {
                    keys: render_sequence(&binding.sequence[prefix.len()..]),
                    definition: binding.definition,
                });
            }
        }
        found
    }

    /// Drop a pending sequence that has outlived the configured timeout.
    ///
    /// Callers with a timer tick can invoke this so a which-key panel closes on
    /// time; [`Self::resolve`] applies the same rule on the next key press, so
    /// correctness never depends on a tick arriving.
    pub fn expire(&mut self, now: Instant) -> bool {
        let expired = self
            .pending
            .as_ref()
            .is_some_and(|pending| now.duration_since(pending.since) >= self.leader_timeout);
        if expired {
            self.pending = None;
        }
        expired
    }

    /// Resolve one chord against an ordered active scope chain.
    ///
    /// `now` is a parameter rather than an internal `Instant::now()` so the
    /// timeout is testable without sleeping.
    pub fn resolve(&mut self, scopes: &[&str], chord: Chord, now: Instant) -> Resolution {
        self.expire(now);
        let mut candidate = self.pending().to_vec();
        candidate.push(chord);

        for scope in scopes {
            if let Some(binding) = self
                .scopes
                .get(scope)
                .into_iter()
                .flatten()
                .find(|binding| binding.sequence == candidate)
            {
                self.pending = None;
                return Resolution::Action {
                    definition: binding.definition,
                    prevent_default: binding.prevent_default,
                };
            }
        }

        let extendable = scopes.iter().any(|scope| {
            self.scopes
                .get(scope)
                .into_iter()
                .flatten()
                .any(|binding| starts_with(&binding.sequence, &candidate))
        });
        if extendable {
            self.pending = Some(Pending {
                chords: candidate,
                since: now,
            });
            return Resolution::Pending;
        }

        self.pending = None;
        Resolution::Unmatched
    }
}

fn starts_with(sequence: &[Chord], prefix: &[Chord]) -> bool {
    sequence.len() > prefix.len() && sequence.starts_with(prefix)
}

fn collect_conflicts(scopes: &BTreeMap<&'static str, Vec<Binding>>) -> Vec<Conflict> {
    let mut conflicts = Vec::new();
    for (scope, bindings) in scopes {
        let mut by_sequence: BTreeMap<Vec<Chord>, Vec<&'static str>> = BTreeMap::new();
        for binding in bindings {
            by_sequence
                .entry(binding.sequence.clone())
                .or_default()
                .push(binding.definition.name);
        }
        for (sequence, mut actions) in by_sequence {
            if actions.len() > 1 {
                actions.sort_unstable();
                conflicts.push(Conflict {
                    scope: (*scope).to_owned(),
                    sequence: render_sequence(&sequence),
                    kind: ConflictKind::Duplicate,
                    actions: actions.iter().map(|name| (*name).to_owned()).collect(),
                });
                continue;
            }
            if let Some(longer) = bindings
                .iter()
                .find(|binding| starts_with(&binding.sequence, &sequence))
            {
                let mut actions = vec![actions[0].to_owned(), longer.definition.name.to_owned()];
                actions.sort_unstable();
                conflicts.push(Conflict {
                    scope: (*scope).to_owned(),
                    sequence: render_sequence(&sequence),
                    kind: ConflictKind::PrefixShadow {
                        longer: render_sequence(&longer.sequence),
                    },
                    actions,
                });
            }
        }
    }
    conflicts
}

/// A component that acts on resolved keybind actions instead of raw keys.
pub trait ActionComponent: Component {
    /// Act on one resolved binding.
    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> EventResult;

    /// Scopes owned by the currently focused overlay, in resolution order.
    fn focused_scopes(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Observe a change to the pending sequence, for a which-key surface.
    ///
    /// Called on every resolution, including with an inactive prefix when a sequence
    /// completes or is abandoned. A consumer that only heard about *arriving* prefixes
    /// could never learn one had gone.
    fn pending_changed(&mut self, _pending: &PendingPrefix) -> EventResult {
        EventResult::IGNORED
    }

    /// Dialogs this component asked for while handling the last action.
    ///
    /// A component below [`crate::views::dialog::DialogHost`] cannot open a dialog
    /// itself: the host owns the stack, and the host owns *it*. Before this seam
    /// existed, every picker in [`crate::views::picker`] was therefore constructible
    /// only from its own tests — model, agent, session and theme switching were four
    /// finished surfaces that no key press could reach.
    ///
    /// The request is the built dialog rather than a description of one, because the
    /// component is what holds the list to put in it. The host only opens what it is
    /// handed, so no inventory has to travel up.
    fn drain_dialogs(&mut self) -> Vec<Box<dyn crate::views::dialog::Dialog>> {
        Vec::new()
    }

    /// Transient notices this component asked to show.
    ///
    /// The same seam as [`Self::drain_dialogs`], and for the same reason: the slot lives
    /// in [`crate::views::dialog::DialogHost`] because `§11.4` puts a toast *above* the
    /// modal stack, and a component below the host cannot paint above it. A base that
    /// drew its own toast would have it hidden by any open dialog — which is precisely
    /// the moment a "copied" confirmation matters, because the transcript behind the
    /// modal cannot be read either.
    fn drain_toasts(&mut self) -> Vec<crate::views::toast::Toast> {
        Vec::new()
    }

    /// Region a dialog may use when it belongs to a base-owned surface.
    ///
    /// Overlay dialogs return `None` and use the whole frame. Composer prompts ask for
    /// their base's input region so they align with the transcript column, avoid the
    /// sidebar, and grow upward from the same bottom edge as the editor they replace.
    fn dialog_region(&self, _dialog: &'static str, _area: Rect) -> Option<Rect> {
        None
    }

    /// Observe the answer to a dialog this component asked for.
    fn apply_dialog_outcome(
        &mut self,
        _dialog: &'static str,
        _outcome: &crate::views::dialog::DialogOutcome,
    ) -> EventResult {
        EventResult::IGNORED
    }

    /// Observe which modal currently owns the keyboard, or `None` for none.
    ///
    /// A component below [`crate::views::dialog::DialogHost`] cannot see the stack, and
    /// some of what it draws depends on it: a transcript that keeps spinning `working`
    /// while a permission prompt asks the user to decide is claiming the process is busy
    /// when it is in fact waiting, and those two states are mutually exclusive.
    ///
    /// Called by the host on every frame from the stack it is about to draw, rather than
    /// pushed when a dialog opens or closes. One derived call site cannot disagree with
    /// what is on screen; two notification sites — `open` and each of the pops — can, and
    /// the failure mode is a banner that outlives its dialog.
    fn observe_modal(&mut self, _active: Option<&'static str>) {}
}

/// Turns key presses into actions before the component tree sees them.
///
/// This is the seam that keeps bindings out of view code: the wrapped component
/// receives a [`Definition`], never a key, so rebinding a key changes nothing
/// below this point.
pub struct KeyDispatcher {
    keymap: Keymap,
    scopes: Vec<String>,
    inner: Box<dyn ActionComponent>,
}

impl KeyDispatcher {
    /// Wrap a component, resolving keys in the given ordered scope chain.
    #[must_use]
    pub fn new(keymap: Keymap, scopes: Vec<String>, inner: Box<dyn ActionComponent>) -> Self {
        Self {
            keymap,
            scopes,
            inner,
        }
    }

    /// Replace the active scope chain when focus moves.
    pub fn set_scopes(&mut self, scopes: Vec<String>) {
        self.scopes = scopes;
    }

    /// The keymap, for help and which-key surfaces.
    #[must_use]
    pub const fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Resolve and dispatch one key event at an explicit instant.
    pub fn dispatch_key(&mut self, event: &KeyEvent, now: Instant) -> EventResult {
        let Some(chord) = Chord::from_key_event(event) else {
            return EventResult::IGNORED;
        };
        let mut scopes = self.inner.focused_scopes();
        scopes.extend(self.scopes.iter().map(String::as_str));
        match self.keymap.resolve(&scopes, chord, now) {
            Resolution::Action {
                definition,
                prevent_default: _,
            } => {
                let cleared = self.inner.pending_changed(&PendingPrefix::default());
                self.inner.handle_action(definition, event).merge(cleared)
            }
            Resolution::Pending => {
                let prefix = PendingPrefix {
                    chords: self.keymap.pending().to_vec(),
                    continuations: self.keymap.continuations(&scopes),
                };
                EventResult::REDRAW.merge(self.inner.pending_changed(&prefix))
            }
            // Cleared here too, and this is the branch that made it necessary: an
            // abandoned sequence left the last prefix standing, so a which-key panel
            // opened by `ctrl+x` stayed open over every later keystroke.
            //
            // Only the redraw bit is kept. `handled` must stay false: this branch is what
            // lets an unmatched key fall through to the editor and be typed, and merging
            // a consumer's `REDRAW` here swallowed the very keystroke that abandoned the
            // sequence — `ctrl+x` then `z` stopped putting `z` in the prompt.
            Resolution::Unmatched => {
                let cleared = self.inner.pending_changed(&PendingPrefix::default());
                EventResult {
                    handled: false,
                    redraw: cleared.redraw,
                }
            }
        }
    }
}

impl Component for KeyDispatcher {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.inner.render(frame, area);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        if let AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Key(key))) = event
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            let result = self.dispatch_key(key, Instant::now());
            if result.handled {
                return result;
            }
        }
        self.inner.handle_event(event)
    }
}

/// Every binding upstream ships, in `packages/tui/src/config/keybind.ts` order.
///
/// Generated from that file; see the module docs for the extraction command's
/// home. The `leader` row configures the leader chord rather than an action.
pub const DEFINITIONS: &[Definition] = &[
    Definition {
        name: "leader",
        scope: "leader",
        keys: "ctrl+x",
        command: "leader",
        prevent_default: None,
        description: "Leader key for keybind combinations",
    },
    Definition {
        name: "app_exit",
        scope: "app",
        keys: "ctrl+c,ctrl+d,<leader>q",
        command: "app.exit",
        prevent_default: None,
        description: "Exit the application",
    },
    Definition {
        name: "app_debug",
        scope: "app",
        keys: "none",
        command: "app.debug",
        prevent_default: None,
        description: "Toggle debug panel",
    },
    Definition {
        name: "app_console",
        scope: "app",
        keys: "none",
        command: "app.console",
        prevent_default: None,
        description: "Toggle console",
    },
    Definition {
        name: "app_heap_snapshot",
        scope: "app",
        keys: "none",
        command: "app.heap_snapshot",
        prevent_default: None,
        description: "Write heap snapshot",
    },
    Definition {
        name: "app_toggle_animations",
        scope: "app",
        keys: "none",
        command: "app.toggle.animations",
        prevent_default: None,
        description: "Toggle animations",
    },
    Definition {
        name: "app_toggle_file_context",
        scope: "app",
        keys: "none",
        command: "app.toggle.file_context",
        prevent_default: None,
        description: "Toggle file context",
    },
    Definition {
        name: "app_toggle_diffwrap",
        scope: "app",
        keys: "none",
        command: "app.toggle.diffwrap",
        prevent_default: None,
        description: "Toggle diff wrapping",
    },
    Definition {
        name: "app_toggle_paste_summary",
        scope: "app",
        keys: "none",
        command: "app.toggle.paste_summary",
        prevent_default: None,
        description: "Toggle paste summary",
    },
    Definition {
        name: "app_toggle_session_directory_filter",
        scope: "app",
        keys: "none",
        command: "app.toggle.session_directory_filter",
        prevent_default: None,
        description: "Toggle session directory filtering",
    },
    Definition {
        name: "command_list",
        scope: "command",
        keys: "ctrl+p",
        command: "command.palette.show",
        prevent_default: None,
        description: "List available commands",
    },
    Definition {
        name: "help_show",
        scope: "help",
        keys: "none",
        command: "help.show",
        prevent_default: None,
        description: "Open help dialog",
    },
    Definition {
        name: "docs_open",
        scope: "docs",
        keys: "none",
        command: "docs.open",
        prevent_default: None,
        description: "Open documentation",
    },
    Definition {
        name: "diff_open",
        scope: "diff",
        keys: "none",
        command: "diff.open",
        prevent_default: None,
        description: "Open diff viewer",
    },
    Definition {
        name: "diff_close",
        scope: "diff",
        keys: "escape,q",
        command: "diff.close",
        prevent_default: None,
        description: "Close diff viewer",
    },
    Definition {
        name: "diff_toggle",
        scope: "diff",
        keys: "enter,space",
        command: "diff.toggle",
        prevent_default: None,
        description: "Toggle diff viewer item",
    },
    Definition {
        name: "diff_expand",
        scope: "diff",
        keys: "right",
        command: "diff.expand",
        prevent_default: None,
        description: "Expand diff viewer item",
    },
    Definition {
        name: "diff_expand_all",
        scope: "diff",
        keys: "E",
        command: "diff.expand_all",
        prevent_default: None,
        description: "Expand all diff viewer folders",
    },
    Definition {
        name: "diff_collapse",
        scope: "diff",
        keys: "left",
        command: "diff.collapse",
        prevent_default: None,
        description: "Collapse diff viewer item",
    },
    Definition {
        name: "diff_switch_focus",
        scope: "diff",
        keys: "tab",
        command: "diff.switch_focus",
        prevent_default: None,
        description: "Switch diff viewer focus",
    },
    Definition {
        name: "diff_next_hunk",
        scope: "diff",
        keys: "]",
        command: "diff.next_hunk",
        prevent_default: None,
        description: "Jump to next diff hunk",
    },
    Definition {
        name: "diff_previous_hunk",
        scope: "diff",
        keys: "[",
        command: "diff.previous_hunk",
        prevent_default: None,
        description: "Jump to previous diff hunk",
    },
    Definition {
        name: "diff_next_file",
        scope: "diff",
        keys: "n",
        command: "diff.next_file",
        prevent_default: None,
        description: "Jump to next diff file",
    },
    Definition {
        name: "diff_previous_file",
        scope: "diff",
        keys: "p",
        command: "diff.previous_file",
        prevent_default: None,
        description: "Jump to previous diff file",
    },
    Definition {
        name: "diff_toggle_file_tree",
        scope: "diff",
        keys: "b",
        command: "diff.toggle_file_tree",
        prevent_default: None,
        description: "Toggle diff viewer file tree",
    },
    Definition {
        name: "diff_single_patch",
        scope: "diff",
        keys: "s",
        command: "diff.single_patch",
        prevent_default: None,
        description: "Toggle single patch view",
    },
    Definition {
        name: "diff_switch_source",
        scope: "diff",
        keys: "d",
        command: "diff.switch_source",
        prevent_default: None,
        description: "Switch diff viewer source",
    },
    Definition {
        name: "diff_toggle_view",
        scope: "diff",
        keys: "v",
        command: "diff.toggle_view",
        prevent_default: None,
        description: "Toggle diff viewer split or unified view",
    },
    Definition {
        name: "diff_help",
        scope: "diff",
        keys: "?",
        command: "diff.help",
        prevent_default: None,
        description: "Show more diff viewer shortcuts",
    },
    Definition {
        name: "editor_open",
        scope: "editor",
        keys: "<leader>e",
        command: "prompt.editor",
        prevent_default: None,
        description: "Open external editor",
    },
    Definition {
        name: "theme_list",
        scope: "theme",
        keys: "<leader>t",
        command: "theme.switch",
        prevent_default: None,
        description: "List available themes",
    },
    Definition {
        name: "theme_switch_mode",
        scope: "theme",
        keys: "none",
        command: "theme.switch_mode",
        prevent_default: None,
        description: "Switch between light and dark theme mode",
    },
    Definition {
        name: "theme_mode_lock",
        scope: "theme",
        keys: "none",
        command: "theme.mode.lock",
        prevent_default: None,
        description: "Lock or unlock theme mode",
    },
    Definition {
        name: "sidebar_toggle",
        scope: "sidebar",
        keys: "<leader>b",
        command: "session.sidebar.toggle",
        prevent_default: None,
        description: "Toggle sidebar",
    },
    Definition {
        name: "scrollbar_toggle",
        scope: "scrollbar",
        keys: "none",
        command: "session.toggle.scrollbar",
        prevent_default: None,
        description: "Toggle session scrollbar",
    },
    Definition {
        name: "status_view",
        scope: "status",
        keys: "<leader>s",
        command: "zuno.status",
        prevent_default: None,
        description: "View status",
    },
    Definition {
        name: "debug_view",
        scope: "debug",
        keys: "none",
        command: "zuno.debug",
        prevent_default: None,
        description: "View debug info",
    },
    Definition {
        name: "session_export",
        scope: "session",
        keys: "<leader>x",
        command: "session.export",
        prevent_default: None,
        description: "Export session to editor",
    },
    Definition {
        name: "session_copy",
        scope: "session",
        keys: "none",
        command: "session.copy",
        prevent_default: None,
        description: "Copy session transcript",
    },
    Definition {
        name: "session_move",
        scope: "session",
        keys: "none",
        command: "session.move",
        prevent_default: None,
        description: "Move session",
    },
    Definition {
        name: "session_new",
        scope: "session",
        keys: "<leader>n",
        command: "session.new",
        prevent_default: None,
        description: "Create a new session",
    },
    Definition {
        name: "session_list",
        scope: "session",
        keys: "<leader>l",
        command: "session.list",
        prevent_default: None,
        description: "List all sessions",
    },
    Definition {
        name: "session_timeline",
        scope: "session",
        keys: "<leader>g",
        command: "session.timeline",
        prevent_default: None,
        description: "Show session timeline",
    },
    Definition {
        name: "session_fork",
        scope: "session",
        keys: "none",
        command: "session.fork",
        prevent_default: None,
        description: "Fork session from message",
    },
    Definition {
        name: "session_rename",
        scope: "session",
        keys: "ctrl+r",
        command: "session.rename",
        prevent_default: None,
        description: "Rename session",
    },
    Definition {
        name: "session_delete",
        scope: "session",
        keys: "ctrl+d",
        command: "session.delete",
        prevent_default: None,
        description: "Delete session",
    },
    Definition {
        name: "session_share",
        scope: "session",
        keys: "none",
        command: "session.share",
        prevent_default: None,
        description: "Share current session",
    },
    Definition {
        name: "session_unshare",
        scope: "session",
        keys: "none",
        command: "session.unshare",
        prevent_default: None,
        description: "Unshare current session",
    },
    Definition {
        name: "session_interrupt",
        scope: "session",
        keys: "escape",
        command: "session.interrupt",
        prevent_default: None,
        description: "Interrupt current session",
    },
    Definition {
        name: "session_background",
        scope: "session",
        keys: "ctrl+b",
        command: "session.background",
        prevent_default: None,
        description: "Background synchronous subagents",
    },
    Definition {
        name: "session_compact",
        scope: "session",
        keys: "<leader>c",
        command: "session.compact",
        prevent_default: None,
        description: "Compact the session",
    },
    Definition {
        name: "session_toggle_timestamps",
        scope: "session",
        keys: "none",
        command: "session.toggle.timestamps",
        prevent_default: None,
        description: "Toggle message timestamps",
    },
    Definition {
        name: "session_toggle_generic_tool_output",
        scope: "session",
        keys: "none",
        command: "session.toggle.generic_tool_output",
        prevent_default: None,
        description: "Toggle generic tool output",
    },
    Definition {
        name: "session_queued_prompts",
        scope: "session",
        keys: "<leader>q",
        command: "session.queued_prompts",
        prevent_default: None,
        description: "Manage queued prompts",
    },
    Definition {
        name: "session_child_first",
        scope: "session",
        keys: "<leader>down",
        command: "session.child.first",
        prevent_default: None,
        description: "Go to first child session",
    },
    Definition {
        name: "session_child_cycle",
        scope: "session",
        keys: "right",
        command: "session.child.next",
        prevent_default: None,
        description: "Go to next child session",
    },
    Definition {
        name: "session_child_cycle_reverse",
        scope: "session",
        keys: "left",
        command: "session.child.previous",
        prevent_default: None,
        description: "Go to previous child session",
    },
    Definition {
        name: "session_parent",
        scope: "session",
        keys: "up",
        command: "session.parent",
        prevent_default: None,
        description: "Go to parent session",
    },
    Definition {
        name: "session_pin_toggle",
        scope: "session",
        keys: "ctrl+f",
        command: "session.pin.toggle",
        prevent_default: None,
        description: "Pin or unpin session in the session list",
    },
    Definition {
        name: "session_quick_switch_1",
        scope: "session",
        keys: "<leader>1",
        command: "session.quick_switch.1",
        prevent_default: None,
        description: "Switch to session in quick slot 1",
    },
    Definition {
        name: "session_quick_switch_2",
        scope: "session",
        keys: "<leader>2",
        command: "session.quick_switch.2",
        prevent_default: None,
        description: "Switch to session in quick slot 2",
    },
    Definition {
        name: "session_quick_switch_3",
        scope: "session",
        keys: "<leader>3",
        command: "session.quick_switch.3",
        prevent_default: None,
        description: "Switch to session in quick slot 3",
    },
    Definition {
        name: "session_quick_switch_4",
        scope: "session",
        keys: "<leader>4",
        command: "session.quick_switch.4",
        prevent_default: None,
        description: "Switch to session in quick slot 4",
    },
    Definition {
        name: "session_quick_switch_5",
        scope: "session",
        keys: "<leader>5",
        command: "session.quick_switch.5",
        prevent_default: None,
        description: "Switch to session in quick slot 5",
    },
    Definition {
        name: "session_quick_switch_6",
        scope: "session",
        keys: "<leader>6",
        command: "session.quick_switch.6",
        prevent_default: None,
        description: "Switch to session in quick slot 6",
    },
    Definition {
        name: "session_quick_switch_7",
        scope: "session",
        keys: "<leader>7",
        command: "session.quick_switch.7",
        prevent_default: None,
        description: "Switch to session in quick slot 7",
    },
    Definition {
        name: "session_quick_switch_8",
        scope: "session",
        keys: "<leader>8",
        command: "session.quick_switch.8",
        prevent_default: None,
        description: "Switch to session in quick slot 8",
    },
    Definition {
        name: "session_quick_switch_9",
        scope: "session",
        keys: "<leader>9",
        command: "session.quick_switch.9",
        prevent_default: None,
        description: "Switch to session in quick slot 9",
    },
    Definition {
        name: "stash_delete",
        scope: "stash",
        keys: "ctrl+d",
        command: "stash.delete",
        prevent_default: None,
        description: "Delete stash entry",
    },
    Definition {
        name: "model_provider_list",
        scope: "model",
        keys: "ctrl+a",
        command: "model.dialog.provider",
        prevent_default: None,
        description: "Open provider list from model dialog",
    },
    Definition {
        name: "model_favorite_toggle",
        scope: "model",
        keys: "ctrl+f",
        command: "model.dialog.favorite",
        prevent_default: None,
        description: "Toggle model favorite status",
    },
    Definition {
        name: "model_list",
        scope: "model",
        keys: "<leader>m",
        command: "model.list",
        prevent_default: None,
        description: "List available models",
    },
    Definition {
        name: "model_cycle_recent",
        scope: "model",
        keys: "f2",
        command: "model.cycle_recent",
        prevent_default: None,
        description: "Next recently used model",
    },
    Definition {
        name: "model_cycle_recent_reverse",
        scope: "model",
        keys: "shift+f2",
        command: "model.cycle_recent_reverse",
        prevent_default: None,
        description: "Previous recently used model",
    },
    Definition {
        name: "model_cycle_favorite",
        scope: "model",
        keys: "none",
        command: "model.cycle_favorite",
        prevent_default: None,
        description: "Next favorite model",
    },
    Definition {
        name: "model_cycle_favorite_reverse",
        scope: "model",
        keys: "none",
        command: "model.cycle_favorite_reverse",
        prevent_default: None,
        description: "Previous favorite model",
    },
    Definition {
        name: "mcp_list",
        scope: "mcp",
        keys: "none",
        command: "mcp.list",
        prevent_default: None,
        description: "List MCP servers",
    },
    Definition {
        name: "provider_connect",
        scope: "provider",
        keys: "none",
        command: "provider.connect",
        prevent_default: None,
        description: "Connect provider",
    },
    Definition {
        name: "console_org_switch",
        scope: "console",
        keys: "none",
        command: "console.org.switch",
        prevent_default: None,
        description: "Switch console organization",
    },
    Definition {
        name: "agent_list",
        scope: "agent",
        keys: "<leader>a",
        command: "agent.list",
        prevent_default: None,
        description: "List agents",
    },
    Definition {
        name: "agent_cycle",
        scope: "agent",
        keys: "tab",
        command: "agent.cycle",
        prevent_default: None,
        description: "Next agent",
    },
    Definition {
        name: "agent_cycle_reverse",
        scope: "agent",
        // `shift+tab` alone is unresolvable: crossterm reports the press as
        // `KeyCode::BackTab` *with* `SHIFT`, so `from_key_event` yields `shift+backtab`,
        // while the spelling parses to `SHIFT` + `Key::Tab` — a chord no key event can
        // equal. The row therefore shipped resolving to `Unmatched`, which no handler
        // could have rescued. `backtab` covers terminals that omit the modifier and
        // `shift+tab` the Kitty protocol, which really does report `Tab` with `SHIFT`.
        keys: "shift+backtab,backtab,shift+tab",
        command: "agent.cycle.reverse",
        prevent_default: None,
        description: "Previous agent",
    },
    Definition {
        name: "variant_cycle",
        scope: "variant",
        keys: "alt+t",
        command: "variant.cycle",
        prevent_default: None,
        description: "Cycle model variants",
    },
    Definition {
        name: "variant_list",
        scope: "variant",
        keys: "none",
        command: "variant.list",
        prevent_default: None,
        description: "List model variants",
    },
    Definition {
        name: "messages_page_up",
        scope: "messages",
        keys: "pageup,ctrl+alt+b",
        command: "session.page.up",
        prevent_default: None,
        description: "Scroll messages up by one page",
    },
    Definition {
        name: "messages_page_down",
        scope: "messages",
        keys: "pagedown,ctrl+alt+f",
        command: "session.page.down",
        prevent_default: None,
        description: "Scroll messages down by one page",
    },
    Definition {
        name: "messages_line_up",
        scope: "messages",
        keys: "up,ctrl+alt+y",
        command: "session.line.up",
        prevent_default: None,
        description: "Scroll messages up by one line",
    },
    Definition {
        name: "messages_line_down",
        scope: "messages",
        keys: "down,ctrl+alt+e",
        command: "session.line.down",
        prevent_default: None,
        description: "Scroll messages down by one line",
    },
    Definition {
        name: "messages_half_page_up",
        scope: "messages",
        keys: "ctrl+alt+u",
        command: "session.half.page.up",
        prevent_default: None,
        description: "Scroll messages up by half page",
    },
    Definition {
        name: "messages_half_page_down",
        scope: "messages",
        keys: "ctrl+alt+d",
        command: "session.half.page.down",
        prevent_default: None,
        description: "Scroll messages down by half page",
    },
    Definition {
        name: "messages_first",
        scope: "messages",
        keys: "ctrl+g,home",
        command: "session.first",
        prevent_default: None,
        description: "Navigate to first message",
    },
    Definition {
        name: "messages_last",
        scope: "messages",
        keys: "ctrl+alt+g,end",
        command: "session.last",
        prevent_default: None,
        description: "Navigate to last message",
    },
    Definition {
        name: "messages_next",
        scope: "messages",
        keys: "none",
        command: "session.message.next",
        prevent_default: None,
        description: "Navigate to next message",
    },
    Definition {
        name: "messages_previous",
        scope: "messages",
        keys: "none",
        command: "session.message.previous",
        prevent_default: None,
        description: "Navigate to previous message",
    },
    Definition {
        name: "messages_last_user",
        scope: "messages",
        keys: "none",
        command: "session.messages_last_user",
        prevent_default: None,
        description: "Navigate to last user message",
    },
    Definition {
        name: "messages_copy",
        scope: "messages",
        keys: "<leader>y",
        command: "messages.copy",
        prevent_default: None,
        description: "Copy message",
    },
    Definition {
        name: "messages_undo",
        scope: "messages",
        keys: "<leader>u",
        command: "session.undo",
        prevent_default: None,
        description: "Undo message",
    },
    Definition {
        name: "messages_redo",
        scope: "messages",
        keys: "<leader>r",
        command: "session.redo",
        prevent_default: None,
        description: "Redo message",
    },
    Definition {
        name: "messages_toggle_conceal",
        scope: "messages",
        keys: "<leader>h",
        command: "session.toggle.conceal",
        prevent_default: None,
        description: "Toggle code block concealment in messages",
    },
    Definition {
        name: "tool_details",
        scope: "tool",
        keys: "none",
        command: "session.toggle.actions",
        prevent_default: None,
        description: "Toggle tool details visibility",
    },
    Definition {
        name: "messages_transcript",
        scope: "messages",
        keys: "ctrl+t",
        command: "session.transcript",
        prevent_default: None,
        description: "Toggle full activity transcript",
    },
    Definition {
        name: "display_thinking",
        scope: "display",
        keys: "none",
        command: "session.toggle.thinking",
        prevent_default: None,
        description: "Toggle thinking blocks visibility",
    },
    Definition {
        name: "prompt_submit",
        scope: "prompt",
        keys: "none",
        command: "prompt.submit",
        prevent_default: None,
        description: "Submit prompt",
    },
    Definition {
        name: "prompt_editor_context_clear",
        scope: "prompt",
        keys: "none",
        command: "prompt.editor_context.clear",
        prevent_default: None,
        description: "Clear editor context",
    },
    Definition {
        name: "prompt_skills",
        scope: "prompt",
        keys: "none",
        command: "prompt.skills",
        prevent_default: None,
        description: "Open skill selector",
    },
    Definition {
        name: "prompt_stash",
        scope: "prompt",
        keys: "none",
        command: "prompt.stash",
        prevent_default: None,
        description: "Stash prompt",
    },
    Definition {
        name: "prompt_stash_pop",
        scope: "prompt",
        keys: "none",
        command: "prompt.stash.pop",
        prevent_default: None,
        description: "Pop stashed prompt",
    },
    Definition {
        name: "prompt_stash_list",
        scope: "prompt",
        keys: "none",
        command: "prompt.stash.list",
        prevent_default: None,
        description: "List stashed prompts",
    },
    Definition {
        name: "workspace_set",
        scope: "workspace",
        keys: "none",
        command: "workspace.set",
        prevent_default: None,
        description: "Set workspace",
    },
    Definition {
        name: "input_clear",
        scope: "input",
        keys: "ctrl+c",
        command: "prompt.clear",
        prevent_default: None,
        description: "Clear input field",
    },
    Definition {
        name: "input_paste",
        scope: "input",
        keys: "ctrl+v",
        command: "prompt.paste",
        prevent_default: Some(false),
        description: "Paste from clipboard",
    },
    Definition {
        name: "input_submit",
        scope: "input",
        keys: "return",
        command: "input.submit",
        prevent_default: None,
        description: "Submit input",
    },
    Definition {
        name: "input_newline",
        scope: "input",
        keys: "shift+return,ctrl+return,alt+return,ctrl+j",
        command: "input.newline",
        prevent_default: None,
        description: "Insert newline in input",
    },
    Definition {
        name: "input_move_left",
        scope: "input",
        keys: "left,ctrl+b",
        command: "input.move.left",
        prevent_default: None,
        description: "Move cursor left in input",
    },
    Definition {
        name: "input_move_right",
        scope: "input",
        keys: "right,ctrl+f",
        command: "input.move.right",
        prevent_default: None,
        description: "Move cursor right in input",
    },
    Definition {
        name: "input_move_up",
        scope: "input",
        keys: "up",
        command: "input.move.up",
        prevent_default: None,
        description: "Move cursor up in input",
    },
    Definition {
        name: "input_move_down",
        scope: "input",
        keys: "down",
        command: "input.move.down",
        prevent_default: None,
        description: "Move cursor down in input",
    },
    Definition {
        name: "input_select_left",
        scope: "input",
        keys: "shift+left",
        command: "input.select.left",
        prevent_default: None,
        description: "Select left in input",
    },
    Definition {
        name: "input_select_right",
        scope: "input",
        keys: "shift+right",
        command: "input.select.right",
        prevent_default: None,
        description: "Select right in input",
    },
    Definition {
        name: "input_select_up",
        scope: "input",
        keys: "shift+up",
        command: "input.select.up",
        prevent_default: None,
        description: "Select up in input",
    },
    Definition {
        name: "input_select_down",
        scope: "input",
        keys: "shift+down",
        command: "input.select.down",
        prevent_default: None,
        description: "Select down in input",
    },
    Definition {
        name: "input_line_home",
        scope: "input",
        keys: "ctrl+a",
        command: "input.line.home",
        prevent_default: None,
        description: "Move to start of line in input",
    },
    Definition {
        name: "input_line_end",
        scope: "input",
        keys: "ctrl+e",
        command: "input.line.end",
        prevent_default: None,
        description: "Move to end of line in input",
    },
    Definition {
        name: "input_select_line_home",
        scope: "input",
        keys: "ctrl+shift+a",
        command: "input.select.line.home",
        prevent_default: None,
        description: "Select to start of line in input",
    },
    Definition {
        name: "input_select_line_end",
        scope: "input",
        keys: "ctrl+shift+e",
        command: "input.select.line.end",
        prevent_default: None,
        description: "Select to end of line in input",
    },
    Definition {
        name: "input_visual_line_home",
        scope: "input",
        keys: "alt+a",
        command: "input.visual.line.home",
        prevent_default: None,
        description: "Move to start of visual line in input",
    },
    Definition {
        name: "input_visual_line_end",
        scope: "input",
        keys: "alt+e",
        command: "input.visual.line.end",
        prevent_default: None,
        description: "Move to end of visual line in input",
    },
    Definition {
        name: "input_select_visual_line_home",
        scope: "input",
        keys: "alt+shift+a",
        command: "input.select.visual.line.home",
        prevent_default: None,
        description: "Select to start of visual line in input",
    },
    Definition {
        name: "input_select_visual_line_end",
        scope: "input",
        keys: "alt+shift+e",
        command: "input.select.visual.line.end",
        prevent_default: None,
        description: "Select to end of visual line in input",
    },
    Definition {
        name: "input_buffer_home",
        scope: "input",
        keys: "home",
        command: "input.buffer.home",
        prevent_default: None,
        description: "Move to start of buffer in input",
    },
    Definition {
        name: "input_buffer_end",
        scope: "input",
        keys: "end",
        command: "input.buffer.end",
        prevent_default: None,
        description: "Move to end of buffer in input",
    },
    Definition {
        name: "input_select_buffer_home",
        scope: "input",
        keys: "shift+home",
        command: "input.select.buffer.home",
        prevent_default: None,
        description: "Select to start of buffer in input",
    },
    Definition {
        name: "input_select_buffer_end",
        scope: "input",
        keys: "shift+end",
        command: "input.select.buffer.end",
        prevent_default: None,
        description: "Select to end of buffer in input",
    },
    Definition {
        name: "input_delete_line",
        scope: "input",
        keys: "ctrl+shift+d",
        command: "input.delete.line",
        prevent_default: None,
        description: "Delete line in input",
    },
    Definition {
        name: "input_delete_to_line_end",
        scope: "input",
        keys: "ctrl+k",
        command: "input.delete.to.line.end",
        prevent_default: None,
        description: "Delete to end of line in input",
    },
    Definition {
        name: "input_delete_to_line_start",
        scope: "input",
        keys: "ctrl+u",
        command: "input.delete.to.line.start",
        prevent_default: None,
        description: "Delete to start of line in input",
    },
    Definition {
        name: "input_backspace",
        scope: "input",
        keys: "backspace,shift+backspace",
        command: "input.backspace",
        prevent_default: None,
        description: "Backspace in input",
    },
    Definition {
        name: "input_delete",
        scope: "input",
        keys: "ctrl+d,delete,shift+delete",
        command: "input.delete",
        prevent_default: None,
        description: "Delete character in input",
    },
    Definition {
        name: "input_undo",
        scope: "input",
        keys: "ctrl+-,super+z",
        command: "input.undo",
        prevent_default: None,
        description: "Undo in input",
    },
    Definition {
        name: "input_redo",
        scope: "input",
        keys: "ctrl+.,super+shift+z",
        command: "input.redo",
        prevent_default: None,
        description: "Redo in input",
    },
    Definition {
        name: "input_word_forward",
        scope: "input",
        keys: "alt+f,alt+right,ctrl+right",
        command: "input.word.forward",
        prevent_default: None,
        description: "Move word forward in input",
    },
    Definition {
        name: "input_word_backward",
        scope: "input",
        keys: "alt+b,alt+left,ctrl+left",
        command: "input.word.backward",
        prevent_default: None,
        description: "Move word backward in input",
    },
    Definition {
        name: "input_select_word_forward",
        scope: "input",
        keys: "alt+shift+f,alt+shift+right",
        command: "input.select.word.forward",
        prevent_default: None,
        description: "Select word forward in input",
    },
    Definition {
        name: "input_select_word_backward",
        scope: "input",
        keys: "alt+shift+b,alt+shift+left",
        command: "input.select.word.backward",
        prevent_default: None,
        description: "Select word backward in input",
    },
    Definition {
        name: "input_delete_word_forward",
        scope: "input",
        keys: "alt+d,alt+delete,ctrl+delete",
        command: "input.delete.word.forward",
        prevent_default: None,
        description: "Delete word forward in input",
    },
    Definition {
        name: "input_delete_word_backward",
        scope: "input",
        keys: "ctrl+w,ctrl+backspace,alt+backspace",
        command: "input.delete.word.backward",
        prevent_default: None,
        description: "Delete word backward in input",
    },
    Definition {
        name: "input_select_all",
        scope: "input",
        keys: "super+a",
        command: "input.select.all",
        prevent_default: None,
        description: "Select all in input",
    },
    Definition {
        name: "history_previous",
        scope: "history",
        keys: "up",
        command: "prompt.history.previous",
        prevent_default: None,
        description: "Previous history item",
    },
    Definition {
        name: "history_next",
        scope: "history",
        keys: "down",
        command: "prompt.history.next",
        prevent_default: None,
        description: "Next history item",
    },
    Definition {
        name: "dialog.select.prev",
        scope: "dialog.select",
        keys: "up,ctrl+p",
        command: "dialog.select.prev",
        prevent_default: None,
        description: "Move to previous dialog item",
    },
    Definition {
        name: "dialog.select.next",
        scope: "dialog.select",
        keys: "down,ctrl+n",
        command: "dialog.select.next",
        prevent_default: None,
        description: "Move to next dialog item",
    },
    Definition {
        name: "dialog.select.page_up",
        scope: "dialog.select",
        keys: "pageup",
        command: "dialog.select.page_up",
        prevent_default: None,
        description: "Move up one page in dialog",
    },
    Definition {
        name: "dialog.select.page_down",
        scope: "dialog.select",
        keys: "pagedown",
        command: "dialog.select.page_down",
        prevent_default: None,
        description: "Move down one page in dialog",
    },
    Definition {
        name: "dialog.select.home",
        scope: "dialog.select",
        keys: "home",
        command: "dialog.select.home",
        prevent_default: None,
        description: "Move to first dialog item",
    },
    Definition {
        name: "dialog.select.end",
        scope: "dialog.select",
        keys: "end",
        command: "dialog.select.end",
        prevent_default: None,
        description: "Move to last dialog item",
    },
    Definition {
        name: "dialog.select.submit",
        scope: "dialog.select",
        keys: "return",
        command: "dialog.select.submit",
        prevent_default: None,
        description: "Submit selected dialog item",
    },
    Definition {
        name: "dialog.prompt.submit",
        scope: "dialog.prompt",
        keys: "return",
        command: "dialog.prompt.submit",
        prevent_default: None,
        description: "Submit dialog prompt",
    },
    Definition {
        name: "dialog.mcp.toggle",
        scope: "dialog.mcp",
        keys: "space",
        command: "dialog.mcp.toggle",
        prevent_default: None,
        description: "Toggle MCP in MCP dialog",
    },
    Definition {
        name: "dialog.move_session.new",
        scope: "dialog.move_session",
        keys: "ctrl+m",
        command: "dialog.move_session.new",
        prevent_default: None,
        description: "New project copy",
    },
    Definition {
        name: "dialog.move_session.delete",
        scope: "dialog.move_session",
        keys: "ctrl+d",
        command: "dialog.move_session.delete",
        prevent_default: None,
        description: "Delete project copy",
    },
    Definition {
        name: "dialog.move_session.refresh",
        scope: "dialog.move_session",
        keys: "ctrl+r",
        command: "dialog.move_session.refresh",
        prevent_default: None,
        description: "Refresh project copies",
    },
    Definition {
        name: "prompt.autocomplete.prev",
        scope: "prompt.autocomplete",
        keys: "up,ctrl+p",
        command: "prompt.autocomplete.prev",
        prevent_default: None,
        description: "Move to previous autocomplete item",
    },
    Definition {
        name: "prompt.autocomplete.next",
        scope: "prompt.autocomplete",
        keys: "down,ctrl+n",
        command: "prompt.autocomplete.next",
        prevent_default: None,
        description: "Move to next autocomplete item",
    },
    Definition {
        name: "prompt.autocomplete.hide",
        scope: "prompt.autocomplete",
        keys: "escape",
        command: "prompt.autocomplete.hide",
        prevent_default: None,
        description: "Hide autocomplete",
    },
    Definition {
        name: "prompt.autocomplete.select",
        scope: "prompt.autocomplete",
        keys: "return",
        command: "prompt.autocomplete.select",
        prevent_default: None,
        description: "Select autocomplete item",
    },
    Definition {
        name: "prompt.autocomplete.complete",
        scope: "prompt.autocomplete",
        keys: "tab",
        command: "prompt.autocomplete.complete",
        prevent_default: None,
        description: "Complete autocomplete item",
    },
    Definition {
        name: "permission.prompt.fullscreen",
        scope: "permission.prompt",
        keys: "ctrl+f",
        command: "permission.prompt.fullscreen",
        prevent_default: None,
        description: "Toggle permission prompt fullscreen",
    },
    Definition {
        name: "plugins.toggle",
        scope: "plugins",
        keys: "space",
        command: "plugins.toggle",
        prevent_default: None,
        description: "Toggle plugin",
    },
    Definition {
        name: "dialog.plugins.install",
        scope: "dialog.plugins",
        keys: "shift+i",
        command: "dialog.plugins.install",
        prevent_default: None,
        description: "Install plugin from plugin dialog",
    },
    Definition {
        name: "terminal_suspend",
        scope: "terminal",
        keys: "ctrl+z",
        command: "terminal.suspend",
        prevent_default: None,
        description: "Suspend terminal",
    },
    Definition {
        name: "terminal_title_toggle",
        scope: "terminal",
        keys: "none",
        command: "terminal.title.toggle",
        prevent_default: None,
        description: "Toggle terminal title",
    },
    Definition {
        name: "tips_toggle",
        scope: "tips",
        keys: "<leader>h",
        command: "tips.toggle",
        prevent_default: None,
        description: "Toggle tips on home screen",
    },
    Definition {
        name: "plugin_manager",
        scope: "plugin",
        keys: "none",
        command: "plugins.list",
        prevent_default: None,
        description: "Open plugin manager dialog",
    },
    Definition {
        name: "plugin_install",
        scope: "plugin",
        keys: "none",
        command: "plugins.install",
        prevent_default: None,
        description: "Install plugin",
    },
    Definition {
        name: "which_key_toggle",
        scope: "which",
        keys: "ctrl+alt+k",
        command: "which-key.toggle",
        prevent_default: None,
        description: "Toggle which-key panel",
    },
    Definition {
        name: "which_key_layout_toggle",
        scope: "which",
        keys: "ctrl+alt+shift+k",
        command: "which-key.layout.toggle",
        prevent_default: None,
        description: "Switch which-key layout",
    },
    Definition {
        name: "which_key_pending_toggle",
        scope: "which",
        keys: "ctrl+alt+shift+p",
        command: "which-key.pending.toggle",
        prevent_default: None,
        description: "Toggle which-key pending preview",
    },
    Definition {
        name: "which_key_group_previous",
        scope: "which",
        keys: "ctrl+alt+left,ctrl+alt+[",
        command: "which-key.group.previous",
        prevent_default: None,
        description: "Previous which-key group",
    },
    Definition {
        name: "which_key_group_next",
        scope: "which",
        keys: "ctrl+alt+right,ctrl+alt+]",
        command: "which-key.group.next",
        prevent_default: None,
        description: "Next which-key group",
    },
    Definition {
        name: "which_key_scroll_up",
        scope: "which",
        keys: "ctrl+alt+up,ctrl+alt+p",
        command: "which-key.scroll.up",
        prevent_default: None,
        description: "Scroll which-key up",
    },
    Definition {
        name: "which_key_scroll_down",
        scope: "which",
        keys: "ctrl+alt+down,ctrl+alt+n",
        command: "which-key.scroll.down",
        prevent_default: None,
        description: "Scroll which-key down",
    },
    Definition {
        name: "which_key_page_up",
        scope: "which",
        keys: "ctrl+alt+pageup",
        command: "which-key.page.up",
        prevent_default: None,
        description: "Page which-key up",
    },
    Definition {
        name: "which_key_page_down",
        scope: "which",
        keys: "ctrl+alt+pagedown",
        command: "which-key.page.down",
        prevent_default: None,
        description: "Page which-key down",
    },
    Definition {
        name: "which_key_home",
        scope: "which",
        keys: "ctrl+alt+home",
        command: "which-key.home",
        prevent_default: None,
        description: "Jump to first which-key binding",
    },
    Definition {
        name: "which_key_end",
        scope: "which",
        keys: "ctrl+alt+end",
        command: "which-key.end",
        prevent_default: None,
        description: "Jump to last which-key binding",
    },
];

/// Zuno-native dialog bindings that are not part of the imported baseline.
///
/// Kept outside [`DEFINITIONS`] so the mechanically extracted baseline remains
/// auditable row-for-row. These still pass through the same parser, conflict
/// detection, user overrides, and action dispatcher as every upstream binding.
pub const LOCAL_DEFINITIONS: &[Definition] = &[
    Definition {
        name: "subagent_cancel",
        scope: "dialog.subagent",
        keys: "x",
        command: "subagent.cancel",
        prevent_default: None,
        description: "Confirm cancellation of the selected subagent job",
    },
    Definition {
        name: "background_cancel",
        scope: "dialog.background",
        keys: "x",
        command: "background.cancel",
        prevent_default: None,
        description: "Confirm cancellation of the selected background terminal",
    },
    Definition {
        name: "ps_view",
        scope: "background",
        keys: "none",
        command: "background.list",
        prevent_default: None,
        description: "List background terminals",
    },
    Definition {
        name: "memory_view",
        scope: "memory",
        keys: "none",
        command: "memory.list",
        prevent_default: None,
        description: "Review resident memory",
    },
    Definition {
        name: "memory_apply",
        scope: "dialog.memory",
        keys: "a",
        command: "memory.apply",
        prevent_default: None,
        description: "Approve the selected memory candidate",
    },
    Definition {
        name: "memory_edit",
        scope: "dialog.memory",
        keys: "e",
        command: "memory.edit",
        prevent_default: None,
        description: "Edit and approve the selected memory candidate",
    },
    Definition {
        name: "memory_reject",
        scope: "dialog.memory",
        keys: "r",
        command: "memory.reject",
        prevent_default: None,
        description: "Reject the selected memory candidate",
    },
    Definition {
        name: "memory_undo",
        scope: "dialog.memory",
        keys: "u",
        command: "memory.undo",
        prevent_default: None,
        description: "Undo the selected applied memory candidate",
    },
    Definition {
        name: "memory_remove",
        scope: "dialog.memory",
        keys: "x",
        command: "memory.remove",
        prevent_default: None,
        description: "Confirm removal of the selected resident entry",
    },
];
