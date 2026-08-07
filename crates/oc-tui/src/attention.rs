//! Attention: telling a user who is looking somewhere else that the session
//! wants them back.
//!
//! Five things are worth interrupting for — a permission request, a question, an
//! error, a finished session, and a finished subagent — and upstream's built-in
//! notifications plugin (`packages/tui/src/feature-plugins/system/notifications.ts:35-86`)
//! is the list. Each one carries a message and one of six named sound slots
//! (`packages/plugin/src/tui.ts:235`). This module owns that map and the delivery
//! rules around it (`packages/tui/src/attention.ts`).
//!
//! # A cue is a pair, and either half can be off
//!
//! Delivery has two independent channels: a desktop notification and an audio
//! cue. The configuration has a switch for each (`notifications`, `sound`) plus a
//! master `enabled` that is **off by default** (`config/index.tsx:103`). So the
//! interesting states are not "on" and "off" but a matrix, and
//! [`Attention::notify`] reports which halves actually fired rather than a single
//! boolean. A cue whose audio is unavailable is still a cue if the notification
//! landed — that degradation is the normal case here, for the reason below.
//!
//! # No audio ships with this crate, on purpose
//!
//! Upstream's built-in pack is six `.mp3` files imported from `@opencode-ai/ui`
//! (`attention.ts:17-22`), a package this port excludes. That import is the only
//! runtime reference from the ported tree into an excluded package, and it is
//! assets rather than code. Vendoring them would mean asserting a licence for
//! them, and none can be asserted: `packages/ui/package.json` says MIT for the
//! package, but the 90 audio files in `packages/ui/src/assets/audio/` carry no
//! attribution or provenance record of their own, and sound libraries are
//! routinely licensed on terms that forbid redistributing the assets standalone.
//! Shipping bytes whose licence cannot be stated is worse than shipping silence.
//!
//! So [`builtin_pack`] is registered under the same id upstream uses
//! (`opencode.default`) and is **empty**. Out of the box the audio half of every
//! cue is unavailable and [`Attention::notify`] returns a
//! [`AttentionDiagnostic::MissingSound`] naming the pack and the slot. Nothing
//! panics, nothing is silently dropped, and the notification half is unaffected.
//!
//! ## Supplying a pack
//!
//! Two paths, and they compose — a per-slot override beats the pack, which beats
//! the built-in, exactly as upstream orders the candidates (`attention.ts:146-150`).
//!
//! Point individual slots at files from `tui.json`:
//!
//! ```json
//! {
//!   "attention": {
//!     "enabled": true,
//!     "volume": 0.4,
//!     "sounds": {
//!       "permission": "~/.config/opencode/sounds/permission.mp3",
//!       "done": "~/.config/opencode/sounds/done.mp3"
//!     }
//!   }
//! }
//! ```
//!
//! Or register a whole pack and select it by id, which is what a plugin does:
//!
//! ```
//! use oc_tui::attention::{Attention, SoundName, SoundPack};
//!
//! let mut attention = Attention::new(Default::default());
//! attention.register_pack(
//!     SoundPack::new("acme.chimes")
//!         .with_name("Acme Chimes")
//!         .with_sound(SoundName::Done, "/opt/acme/done.mp3"),
//! );
//! assert!(attention.activate_pack("acme.chimes"));
//! assert_eq!(attention.active_pack(), "acme.chimes");
//! ```
//!
//! The six slot names are the compatibility surface even though the bytes are
//! not: a user supplying a pack needs to know which six files to provide, so
//! [`SoundName`] keeps upstream's spellings verbatim.
//!
//! # Neither channel touches hardware from this crate
//!
//! [`Notifier`] and [`SoundPlayer`] are traits with inert defaults, for the same
//! reason [`crate::app::TerminalLifecycle`] is one: a test must be able to prove
//! the routing without a display server or an audio device, and this crate must
//! not acquire either as a dependency to do it.
//!
//! [`OscNotifier`] is the real notification path and writes to any [`std::io::Write`],
//! so a test sink is a `Vec<u8>`. [`SilentPlayer`] is the real audio path today
//! and plays nothing; a decoding backend is deliberately deferred rather than
//! pulled in to play files the crate does not ship.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

#[cfg(test)]
#[path = "attention_tests.rs"]
mod tests;

/// The notification title used when the session has none (`attention.ts:41`).
pub const DEFAULT_TITLE: &str = "opencode";

/// The id of the pack this crate registers (`attention.ts:42`).
pub const DEFAULT_PACK_ID: &str = "opencode.default";

/// Upstream's title length cap (`attention.ts:44`).
pub const TITLE_LIMIT: usize = 80;

/// Upstream's message length cap (`attention.ts:45`).
pub const MESSAGE_LIMIT: usize = 240;

/// The volume applied when the user configures none (`config/index.tsx:106`).
pub const DEFAULT_VOLUME: f64 = 0.4;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// One of the six sound slots a cue can name (`packages/plugin/src/tui.ts:235`).
///
/// A slot is a role, not a file: the same file may serve several slots, and a
/// pack that fills only some of them is normal. [`SoundName::Default`] is the
/// slot a caller reaches when it asks for a cue without naming one
/// (`attention.ts:200`), so it is not one of the five event classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SoundName {
    /// The fallback slot for a cue that names no slot.
    Default,
    /// A question is waiting for an answer.
    Question,
    /// A permission request is waiting for a decision.
    Permission,
    /// Something failed.
    Error,
    /// A session finished.
    Done,
    /// A subagent's session finished.
    SubagentDone,
}

impl SoundName {
    /// Every slot, in the order `TuiAttentionSoundNames` declares them.
    ///
    /// Ordered because it is the order a user reads in documentation, and because
    /// a `sounds` table rendered back out should not depend on iteration luck.
    pub const ALL: [Self; 6] = [
        Self::Default,
        Self::Question,
        Self::Permission,
        Self::Error,
        Self::Done,
        Self::SubagentDone,
    ];

    /// The slot's configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Question => "question",
            Self::Permission => "permission",
            Self::Error => "error",
            Self::Done => "done",
            Self::SubagentDone => "subagent_done",
        }
    }
}

impl fmt::Display for SoundName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The five things worth interrupting a user for.
///
/// Derived from the event handlers in
/// `packages/tui/src/feature-plugins/system/notifications.ts:35-86` rather than
/// invented: each handler is one class, and the two `session.status` outcomes are
/// distinct classes because they resolve to different slots (`:77`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventClass {
    /// `permission.asked` — a tool needs a decision (`notifications.ts:49-53`).
    PermissionNeeded,
    /// `question.asked` — the model needs an answer (`notifications.ts:35-39`).
    QuestionAsked,
    /// `session.error` — the turn failed (`notifications.ts:80-86`).
    Error,
    /// `session.status` went idle for a root session (`notifications.ts:77`).
    Done,
    /// `session.status` went idle for a session with a parent (`notifications.ts:77`).
    SubagentDone,
}

impl EventClass {
    /// Every class, in the order this module documents and tests them.
    pub const ALL: [Self; 5] = [
        Self::PermissionNeeded,
        Self::QuestionAsked,
        Self::Error,
        Self::Done,
        Self::SubagentDone,
    ];

    /// The sound slot this class asks for.
    #[must_use]
    pub const fn sound(self) -> SoundName {
        match self {
            Self::PermissionNeeded => SoundName::Permission,
            Self::QuestionAsked => SoundName::Question,
            Self::Error => SoundName::Error,
            Self::Done => SoundName::Done,
            Self::SubagentDone => SoundName::SubagentDone,
        }
    }

    /// The message upstream's plugin passes for this class.
    ///
    /// [`EventClass::Error`] has no fixed message — `sessionErrorMessage`
    /// (`notifications.ts:20-27`) picks one from the error — so its default here
    /// is that function's own fallback, and callers that know better pass their
    /// own through [`AttentionRequest::with_message`].
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::PermissionNeeded => "Permission needs input",
            Self::QuestionAsked => "Question needs input",
            Self::Error => "Session error",
            Self::Done | Self::SubagentDone => "Session done",
        }
    }

    /// Whether this class describes a subagent's session.
    ///
    /// Upstream suppresses the desktop notification for any subagent event
    /// (`notifications.ts:15` — `isSubagent ? false : { when: "blurred" }`), so
    /// [`EventClass::SubagentDone`] is structurally sound-only. It is not a
    /// separate policy knob: a subagent finishing is background progress, and a
    /// desktop notification per subagent would be the loudest thing in the app.
    #[must_use]
    pub const fn is_subagent(self) -> bool {
        matches!(self, Self::SubagentDone)
    }

    /// The cue this class resolves to, before any configuration is applied.
    #[must_use]
    pub fn cue(self) -> Cue {
        Cue {
            class: self,
            message: self.message().to_owned(),
            sound: self.sound(),
            // `sound: { name, when: "always" }` for every class
            // (`notifications.ts:16`): audio is worth playing whether or not the
            // window has focus, because it reaches a user who is not looking.
            sound_when: When::Always,
            notification_when: if self.is_subagent() {
                None
            } else {
                Some(When::Blurred)
            },
        }
    }
}

/// When a channel is allowed to fire, relative to terminal focus.
///
/// `TuiAttentionWhen` (`packages/plugin/src/tui.ts:233`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// Regardless of focus.
    Always,
    /// Only while the terminal has focus.
    Focused,
    /// Only while the terminal does not have focus.
    Blurred,
}

/// What the TUI last learned about terminal focus.
///
/// `Unknown` is a real third state, not a missing value: a terminal that never
/// reported focus cannot be assumed to be either, and a focus-conditional channel
/// therefore declines rather than guessing (`attention.ts:107-112`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusState {
    /// No focus event has arrived.
    #[default]
    Unknown,
    /// The terminal has focus.
    Focused,
    /// The terminal does not have focus.
    Blurred,
}

/// Why a cue was not delivered at all.
///
/// `TuiAttentionNotifySkipReason` (`packages/plugin/src/tui.ts:283-289`), less
/// `renderer_destroyed`, which belongs to the renderer's lifetime rather than to
/// attention policy and is represented here by simply not calling `notify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `attention.enabled` is false.
    AttentionDisabled,
    /// The message normalized to nothing.
    EmptyMessage,
    /// The channel wanted focus and the terminal has it.
    Focused,
    /// The channel wanted a blurred terminal and the terminal has focus.
    Blurred,
    /// The terminal never reported focus, so a focus-conditional channel declined.
    FocusUnknown,
}

impl SkipReason {
    /// The wire spelling upstream uses for this reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AttentionDisabled => "attention_disabled",
            Self::EmptyMessage => "empty_message",
            Self::Focused => "focused",
            Self::Blurred => "blurred",
            Self::FocusUnknown => "focus_unknown",
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One event class rendered into the two things a cue is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    /// The class this cue came from.
    pub class: EventClass,
    /// The notification body.
    pub message: String,
    /// The sound slot to look up in the active pack.
    pub sound: SoundName,
    /// The focus condition on the audio channel.
    pub sound_when: When,
    /// The focus condition on the notification channel, or `None` when this class
    /// suppresses notifications outright.
    pub notification_when: Option<When>,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The `attention` block exactly as written (`config/index.tsx:36-43`).
///
/// Every key is optional, so a user who wants only sound writes only `enabled`
/// and the rest keeps its default.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct AttentionSettings {
    /// Master switch. Absent means off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Whether the desktop-notification channel is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<bool>,
    /// Whether the audio channel is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound: Option<bool>,
    /// Playback volume in `0.0..=1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    /// The id of the pack to take sounds from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound_pack: Option<String>,
    /// Per-slot file overrides, which beat the active pack.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sounds: BTreeMap<SoundName, PathBuf>,
}

impl AttentionSettings {
    /// Apply the upstream defaults (`config/index.tsx:102-109`).
    ///
    /// An out-of-range `volume` is clamped with a diagnostic rather than
    /// rejected. Upstream's schema does reject it, but its runtime *also* clamps
    /// (`clampVolume`, `attention.ts:77-80`) — so the value is already treated as
    /// untrusted, and refusing to start a TUI over a loudness setting trades a
    /// working session for a pedantic one.
    #[must_use]
    pub fn resolve(&self) -> (ResolvedAttention, Vec<AttentionDiagnostic>) {
        let mut diagnostics = Vec::new();
        let volume = match self.volume {
            None => DEFAULT_VOLUME,
            Some(volume) => {
                let clamped = clamp_volume(volume);
                if (clamped - volume).abs() > f64::EPSILON || volume.is_nan() {
                    diagnostics.push(AttentionDiagnostic::VolumeClamped {
                        configured: volume,
                        used: clamped,
                    });
                }
                clamped
            }
        };
        let resolved = ResolvedAttention {
            enabled: self.enabled.unwrap_or(false),
            notifications: self.notifications.unwrap_or(true),
            sound: self.sound.unwrap_or(true),
            volume,
            sound_pack: self
                .sound_pack
                .clone()
                .unwrap_or_else(|| DEFAULT_PACK_ID.to_owned()),
            sounds: self.sounds.clone(),
        };
        (resolved, diagnostics)
    }
}

/// The `attention` block with every default applied (`config/index.tsx:70-77`).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAttention {
    /// Master switch. `false` silences both channels.
    pub enabled: bool,
    /// Whether the desktop-notification channel is used.
    pub notifications: bool,
    /// Whether the audio channel is used.
    pub sound: bool,
    /// Playback volume in `0.0..=1.0`.
    pub volume: f64,
    /// The id of the pack to take sounds from.
    pub sound_pack: String,
    /// Per-slot file overrides, which beat the active pack.
    pub sounds: BTreeMap<SoundName, PathBuf>,
}

impl Default for ResolvedAttention {
    fn default() -> Self {
        AttentionSettings::default().resolve().0
    }
}

/// `clampVolume` (`attention.ts:77-80`): a non-finite volume is silence.
fn clamp_volume(volume: f64) -> f64 {
    if !volume.is_finite() {
        return 0.0;
    }
    volume.clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Sound packs
// ---------------------------------------------------------------------------

/// A named set of files, one per slot (`packages/plugin/src/tui.ts:252-256`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SoundPack {
    id: String,
    name: Option<String>,
    sounds: BTreeMap<SoundName, PathBuf>,
}

impl SoundPack {
    /// A pack with no sounds yet.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            sounds: BTreeMap::new(),
        }
    }

    /// Attach a human-readable name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Fill one slot.
    #[must_use]
    pub fn with_sound(mut self, name: SoundName, file: impl Into<PathBuf>) -> Self {
        self.sounds.insert(name, file.into());
        self
    }

    /// The pack's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The pack's human-readable name, when it has one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The file filling one slot, when the pack fills it.
    #[must_use]
    pub fn sound(&self, name: SoundName) -> Option<&Path> {
        self.sounds.get(&name).map(PathBuf::as_path)
    }

    /// Whether the pack fills no slot at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sounds.is_empty()
    }

    /// Drop an empty id and any slot pointing at an empty path.
    ///
    /// `normalizePack` (`attention.ts:91-107`) — a pack with no usable id cannot
    /// be selected, so registering it would create a name nothing can reach.
    fn normalized(self) -> Option<Self> {
        let id = self.id.trim().to_owned();
        if id.is_empty() {
            return None;
        }
        Some(Self {
            id,
            name: self
                .name
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty()),
            sounds: self
                .sounds
                .into_iter()
                .filter(|(_, file)| !file.as_os_str().is_empty())
                .collect(),
        })
    }
}

/// The built-in pack: the right id, and no files.
///
/// Registered so `sound_pack: "opencode.default"` resolves to *something* and the
/// resulting diagnostic can name a pack that exists. See this module's header for
/// why it is empty and how to fill it.
#[must_use]
pub fn builtin_pack() -> SoundPack {
    SoundPack::new(DEFAULT_PACK_ID).with_name("OpenCode Default")
}

/// What one registered pack looks like from outside
/// (`packages/plugin/src/tui.ts:258-263`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundPackInfo {
    /// The pack's id.
    pub id: String,
    /// The pack's human-readable name, when it has one.
    pub name: Option<String>,
    /// Whether this pack is the one currently supplying sounds.
    pub active: bool,
    /// Whether this crate registered the pack itself.
    pub builtin: bool,
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Something that made a cue quieter than configured.
///
/// Every variant names the thing at fault, because a silent cue is indistinguishable
/// from a working one until something says why. Nothing here is an error: attention
/// is the least important subsystem in the process and must never be the reason a
/// turn fails.
#[derive(Debug, Clone, PartialEq)]
pub enum AttentionDiagnostic {
    /// `sound_pack` names a pack nobody registered.
    UnknownSoundPack {
        /// The configured id.
        pack: String,
        /// The ids that are registered, sorted.
        available: Vec<String>,
    },
    /// No candidate file exists for a slot, so the audio half was dropped.
    MissingSound {
        /// The pack that was consulted.
        pack: String,
        /// The slot that has no file.
        name: SoundName,
    },
    /// Every candidate file for a slot failed to play.
    SoundUnplayable {
        /// The slot that was requested.
        name: SoundName,
        /// The candidates that were tried, in order.
        candidates: Vec<PathBuf>,
    },
    /// The configured volume was outside `0.0..=1.0`.
    VolumeClamped {
        /// What the configuration said.
        configured: f64,
        /// What was used instead.
        used: f64,
    },
}

impl fmt::Display for AttentionDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSoundPack { pack, available } => write!(
                formatter,
                "no sound pack named {pack:?} is registered (available: {}); falling back to {DEFAULT_PACK_ID:?}",
                render_list(available)
            ),
            Self::MissingSound { pack, name } => write!(
                formatter,
                "sound pack {pack:?} has no {name} sound and none is configured under `attention.sounds.{name}`; notifying without audio"
            ),
            Self::SoundUnplayable { name, candidates } => write!(
                formatter,
                "no candidate for the {name} sound could be played ({}); notifying without audio",
                render_list(
                    &candidates
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                )
            ),
            Self::VolumeClamped { configured, used } => write!(
                formatter,
                "`attention.volume` must be between 0 and 1, but the configuration has `{configured}`; using `{used}`"
            ),
        }
    }
}

/// Render ids for a message: `"a", "b"`, or `none` when there are none.
fn render_list(items: &[String]) -> String {
    if items.is_empty() {
        return "none".to_owned();
    }
    items
        .iter()
        .map(|item| format!("{item:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// The desktop-notification channel.
///
/// A trait rather than a call into a notification library so a test can observe
/// the routing without a display server, which `cargo test` does not have.
pub trait Notifier: Send + Sync {
    /// Show `message` under `title`. Returns whether it was shown.
    fn notify(&self, title: &str, message: &str) -> bool;
}

/// A notifier that shows nothing.
///
/// The default, so constructing an [`Attention`] never touches the outside world.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn notify(&self, _title: &str, _message: &str) -> bool {
        false
    }
}

/// The terminal-native notification channel: `OSC 777`.
///
/// Upstream delegates to OpenTUI's `renderer.triggerNotification`
/// (`attention.ts:30,185`), whose implementation is not vendored here. Its
/// synchronous `boolean` return is the tell: a native notification API is
/// asynchronous on every platform, so what it can be doing is writing an escape
/// sequence and letting the emulator raise the notification. `OSC 777` is the
/// sequence that carries a title and a body and is understood by kitty, WezTerm,
/// foot, and urxvt; emulators that do not understand it ignore it, which is the
/// correct failure for a courtesy channel.
///
/// Writing to a `W: Write` rather than to `stderr` directly is what makes this
/// testable at all — and it is also correct, because a TUI owning the alternate
/// screen must decide for itself which stream a sequence goes to.
pub struct OscNotifier<W> {
    sink: Mutex<W>,
}

impl<W: Write + Send> OscNotifier<W> {
    /// Write notifications to `sink`.
    pub const fn new(sink: W) -> Self {
        Self {
            sink: Mutex::new(sink),
        }
    }

    /// Take the sink back.
    pub fn into_inner(self) -> W {
        match self.sink.into_inner() {
            Ok(sink) => sink,
            // A poisoned lock means a previous write panicked. The sink is still a
            // sink; refusing to return it would lose the only handle to it.
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl<W: Write + Send> Notifier for OscNotifier<W> {
    fn notify(&self, title: &str, message: &str) -> bool {
        let Ok(mut sink) = self.sink.lock() else {
            return false;
        };
        // `;` and the string terminator would end the sequence early and leak the
        // rest of the text onto the screen as garbage, so they are stripped rather
        // than escaped: OSC has no escaping mechanism to use.
        let title = sanitize_osc(title);
        let message = sanitize_osc(message);
        let written =
            write!(sink, "\u{1b}]777;notify;{title};{message}\u{1b}\\").and_then(|()| sink.flush());
        written.is_ok()
    }
}

/// Remove the bytes that would terminate or split an OSC sequence.
fn sanitize_osc(text: &str) -> String {
    text.chars()
        .filter(|character| *character != ';' && *character != '\u{1b}' && *character != '\u{7}')
        .collect()
}

/// The audio channel.
///
/// A trait for the same reason [`Notifier`] is one, plus a second: this crate has
/// no audio backend, and a test must not be the thing that decides it needs one.
pub trait SoundPlayer: Send + Sync {
    /// Play `file` at `volume` in `0.0..=1.0`. Returns whether it played.
    ///
    /// A `false` return means "try the next candidate", which is how upstream
    /// walks its candidate list (`attention.ts:152-166`), so a missing or
    /// undecodable file is not a failure of the cue.
    fn play(&self, file: &Path, volume: f64) -> bool;
}

/// A player that plays nothing.
///
/// The default and, today, the only real implementation. A decoding backend would
/// pull a device-level dependency (and its system libraries) into a crate that
/// ships no audio to decode; the honest ordering is to add one when there is
/// something to play, behind this trait, without changing any caller.
#[derive(Debug, Clone, Copy, Default)]
pub struct SilentPlayer;

impl SoundPlayer for SilentPlayer {
    fn play(&self, _file: &Path, _volume: f64) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Requests and outcomes
// ---------------------------------------------------------------------------

/// One request to interrupt the user.
///
/// `TuiAttentionNotifyInput` (`packages/plugin/src/tui.ts:276-281`) restated
/// around [`EventClass`], because every in-tree caller is an event handler and
/// none of them hand-builds a cue.
#[derive(Debug, Clone, PartialEq)]
pub struct AttentionRequest {
    cue: Cue,
    title: Option<String>,
    volume: Option<f64>,
}

impl AttentionRequest {
    /// The request an event class produces on its own.
    #[must_use]
    pub fn new(class: EventClass) -> Self {
        Self {
            cue: class.cue(),
            title: None,
            volume: None,
        }
    }

    /// Use the session title instead of [`DEFAULT_TITLE`] (`notifications.ts:13`).
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Replace the class's default message.
    ///
    /// The error class needs this: `sessionErrorMessage`
    /// (`notifications.ts:20-27`) distinguishes "Session aborted" and "Model
    /// stopped responding" from the generic case.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.cue.message = message.into();
        self
    }

    /// Override the configured volume for this cue only (`attention.ts:82-88`).
    #[must_use]
    pub fn with_volume(mut self, volume: f64) -> Self {
        self.volume = Some(volume);
        self
    }

    /// Suppress the notification half of this cue.
    #[must_use]
    pub fn without_notification(mut self) -> Self {
        self.cue.notification_when = None;
        self
    }

    /// The cue this request carries.
    #[must_use]
    pub fn cue(&self) -> &Cue {
        &self.cue
    }
}

/// What actually happened.
///
/// `TuiAttentionNotifyResult` (`packages/plugin/src/tui.ts:291-296`) plus the
/// diagnostics, which upstream sends to `console.debug` and therefore cannot
/// assert on.
#[derive(Debug, Clone, PartialEq)]
pub struct AttentionOutcome {
    /// Whether the notification was shown.
    pub notification: bool,
    /// Whether a sound played.
    pub sound: bool,
    /// Why nothing was delivered, when nothing was.
    pub skipped: Option<SkipReason>,
    /// Everything that made the cue quieter than configured.
    pub diagnostics: Vec<AttentionDiagnostic>,
}

impl AttentionOutcome {
    /// Whether either channel delivered.
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.notification || self.sound
    }

    /// A cue that was not delivered at all.
    fn skipped(reason: SkipReason) -> Self {
        Self {
            notification: false,
            sound: false,
            skipped: Some(reason),
            diagnostics: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// Delivers cues, holding the configuration, the packs, and the two channels.
///
/// `createTuiAttention` (`attention.ts:113`) as a value rather than a closure
/// bundle, so a test can set focus and read diagnostics without a renderer.
pub struct Attention {
    config: ResolvedAttention,
    focus: FocusState,
    packs: BTreeMap<String, SoundPack>,
    builtin: SoundPack,
    active_pack: Option<String>,
    notifier: Arc<dyn Notifier>,
    player: Arc<dyn SoundPlayer>,
    load_diagnostics: Vec<AttentionDiagnostic>,
}

impl fmt::Debug for Attention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The two channels are trait objects with no useful rendering, and their
        // absence from the output is not a loss: what a reader wants here is the
        // policy state.
        formatter
            .debug_struct("Attention")
            .field("config", &self.config)
            .field("focus", &self.focus)
            .field("packs", &self.packs.keys().collect::<Vec<_>>())
            .field("active_pack", &self.active_pack())
            .finish_non_exhaustive()
    }
}

impl Attention {
    /// Build a host with the inert channels.
    #[must_use]
    pub fn new(config: ResolvedAttention) -> Self {
        let builtin = builtin_pack();
        Self {
            config,
            focus: FocusState::default(),
            packs: BTreeMap::from([(builtin.id().to_owned(), builtin.clone())]),
            builtin,
            active_pack: None,
            notifier: Arc::new(NullNotifier),
            player: Arc::new(SilentPlayer),
            load_diagnostics: Vec::new(),
        }
    }

    /// Build a host from the raw configuration block, retaining its diagnostics.
    ///
    /// The clamp diagnostic is produced once at load rather than on every cue, so
    /// it is held here and reported with the first outcome.
    #[must_use]
    pub fn from_settings(settings: &AttentionSettings) -> Self {
        let (config, load_diagnostics) = settings.resolve();
        Self {
            load_diagnostics,
            ..Self::new(config)
        }
    }

    /// Route notifications through `notifier`.
    #[must_use]
    pub fn with_notifier(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Route audio through `player`.
    #[must_use]
    pub fn with_player(mut self, player: Arc<dyn SoundPlayer>) -> Self {
        self.player = player;
        self
    }

    /// The resolved configuration.
    #[must_use]
    pub const fn config(&self) -> &ResolvedAttention {
        &self.config
    }

    /// Record a focus change (`attention.ts:126-131`).
    pub const fn set_focus(&mut self, focus: FocusState) {
        self.focus = focus;
    }

    /// Register a pack, replacing any pack with the same id.
    ///
    /// Returns whether it was registered; an unnamed pack is refused
    /// (`attention.ts:91-94`).
    pub fn register_pack(&mut self, pack: SoundPack) -> bool {
        match pack.normalized() {
            Some(pack) => {
                self.packs.insert(pack.id().to_owned(), pack);
                true
            }
            None => false,
        }
    }

    /// Remove a registered pack. The built-in pack cannot be removed.
    pub fn unregister_pack(&mut self, id: &str) -> bool {
        if id == self.builtin.id() {
            return false;
        }
        self.packs.remove(id).is_some()
    }

    /// Select a registered pack for this process. Returns whether it exists.
    ///
    /// Upstream can persist the choice in its key-value store
    /// (`attention.ts:171-177`); that store belongs to another crate, so the
    /// selection here lives for the process and the configured `sound_pack`
    /// remains the durable answer.
    pub fn activate_pack(&mut self, id: &str) -> bool {
        if !self.packs.contains_key(id) {
            return false;
        }
        self.active_pack = Some(id.to_owned());
        true
    }

    /// The id of the pack currently supplying sounds.
    ///
    /// The explicit selection wins, then the configured `sound_pack`
    /// (`attention.ts:135-138`). The name is returned even when no pack answers to
    /// it, because that is the value a diagnostic has to quote.
    #[must_use]
    pub fn active_pack(&self) -> &str {
        self.active_pack
            .as_deref()
            .unwrap_or(&self.config.sound_pack)
    }

    /// Every registered pack (`attention.ts:186-195`).
    #[must_use]
    pub fn packs(&self) -> Vec<SoundPackInfo> {
        let active = self.active_pack().to_owned();
        self.packs
            .values()
            .map(|pack| SoundPackInfo {
                id: pack.id().to_owned(),
                name: pack.name().map(str::to_owned),
                active: pack.id() == active,
                builtin: pack.id() == self.builtin.id(),
            })
            .collect()
    }

    /// The candidate files for a slot, in priority order and deduplicated.
    ///
    /// `soundCandidates` (`attention.ts:146-150`): the user's own override, then
    /// the active pack, then the built-in. Empty here by default, which is the
    /// whole reason [`AttentionDiagnostic::MissingSound`] exists.
    #[must_use]
    pub fn sound_candidates(&self, name: SoundName) -> Vec<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        let active = self.packs.get(self.active_pack());
        let sources = [
            self.config.sounds.get(&name).map(PathBuf::as_path),
            active.and_then(|pack| pack.sound(name)),
            self.builtin.sound(name),
        ];
        for file in sources.into_iter().flatten() {
            if file.as_os_str().is_empty() {
                continue;
            }
            if !candidates.iter().any(|existing| existing == file) {
                candidates.push(file.to_owned());
            }
        }
        candidates
    }

    /// Deliver a cue.
    ///
    /// Order matters and follows `attention.ts:169-206`: the master switch first,
    /// then the message, then each channel independently. The two channels are
    /// evaluated in full even when one of them declines, because "the notification
    /// was suppressed but the sound played" is a delivered cue and reporting it as
    /// a skip would be a lie.
    pub fn notify(&mut self, request: &AttentionRequest) -> AttentionOutcome {
        let mut diagnostics = std::mem::take(&mut self.load_diagnostics);

        // The master switch is checked before anything else and before either
        // channel is consulted, so `enabled: false` cannot reach a notifier or a
        // player at all.
        if !self.config.enabled {
            let mut outcome = AttentionOutcome::skipped(SkipReason::AttentionDisabled);
            outcome.diagnostics = diagnostics;
            return outcome;
        }

        let message = normalize_text(&request.cue.message, "", MESSAGE_LIMIT);
        if message.is_empty() {
            let mut outcome = AttentionOutcome::skipped(SkipReason::EmptyMessage);
            outcome.diagnostics = diagnostics;
            return outcome;
        }
        let title = normalize_text(
            request.title.as_deref().unwrap_or_default(),
            DEFAULT_TITLE,
            TITLE_LIMIT,
        );

        let notification_skip = request
            .cue
            .notification_when
            .and_then(|when| focus_skip(when, self.focus));
        let notification_requested =
            self.config.notifications && request.cue.notification_when.is_some();
        let notification = notification_requested
            && notification_skip.is_none()
            && self.notifier.notify(&title, &message);

        let sound_requested = self.config.sound;
        let sound_skip = if sound_requested {
            focus_skip(request.cue.sound_when, self.focus)
        } else {
            None
        };
        let sound = sound_requested
            && sound_skip.is_none()
            && self.play_sound(
                request.cue.sound,
                clamp_volume(request.volume.unwrap_or(self.config.volume)),
                &mut diagnostics,
            );

        // A skip reason is reported only when *nothing* was delivered; otherwise
        // the caller would see `ok: true` next to a reason it was not.
        let skipped = if notification || sound {
            None
        } else {
            notification_skip.or(sound_skip)
        };

        AttentionOutcome {
            notification,
            sound,
            skipped,
            diagnostics,
        }
    }

    /// Walk the candidates for a slot, recording why the audio half went quiet.
    fn play_sound(
        &self,
        name: SoundName,
        volume: f64,
        diagnostics: &mut Vec<AttentionDiagnostic>,
    ) -> bool {
        let configured = self.active_pack().to_owned();
        if !self.packs.contains_key(&configured) {
            let mut available: Vec<String> = self.packs.keys().cloned().collect();
            available.sort();
            diagnostics.push(AttentionDiagnostic::UnknownSoundPack {
                pack: configured.clone(),
                available,
            });
        }

        let candidates = self.sound_candidates(name);
        if candidates.is_empty() {
            diagnostics.push(AttentionDiagnostic::MissingSound {
                pack: configured,
                name,
            });
            return false;
        }
        for file in &candidates {
            if self.player.play(file, volume) {
                return true;
            }
        }
        diagnostics.push(AttentionDiagnostic::SoundUnplayable { name, candidates });
        false
    }
}

/// Whether a channel declines given its focus condition and the focus state.
///
/// `focusSkip` (`attention.ts:107-112`). `Always` never declines; anything else
/// declines on an unknown focus, because a guess here is either a notification
/// the user did not want or a silence they did.
fn focus_skip(when: When, focus: FocusState) -> Option<SkipReason> {
    match (when, focus) {
        (When::Always, _) => None,
        (_, FocusState::Unknown) => Some(SkipReason::FocusUnknown),
        (When::Blurred, FocusState::Focused) => Some(SkipReason::Focused),
        (When::Focused, FocusState::Blurred) => Some(SkipReason::Blurred),
        (When::Blurred, FocusState::Blurred) | (When::Focused, FocusState::Focused) => None,
    }
}

/// `normalizeText` (`attention.ts:66-75`): flatten a message into one safe line.
///
/// A notification body is a single line in someone's system tray, so newlines
/// become spaces and control bytes disappear — including the ANSI escapes a
/// session title picks up from model output, which would otherwise be shown
/// literally or reinterpreted by the emulator. The limit counts characters rather
/// than bytes, so a CJK title is truncated where a reader expects.
fn normalize_text(input: &str, fallback: &str, limit: usize) -> String {
    let stripped = strip_ansi(input);
    let mut flattened = String::with_capacity(stripped.len());
    let mut pending_break = false;
    for character in stripped.chars() {
        match character {
            // Upstream's pattern is `[ \t]*[\r\n]+[ \t]*` -> one space, so the
            // whitespace on *both* sides of the break collapses with it.
            '\r' | '\n' => {
                while flattened.ends_with([' ', '\t']) {
                    flattened.pop();
                }
                pending_break = true;
            }
            ' ' | '\t' if pending_break => {}
            _ => {
                if pending_break {
                    pending_break = false;
                    if !flattened.is_empty() {
                        flattened.push(' ');
                    }
                }
                if is_control(character) {
                    continue;
                }
                flattened.push(character);
            }
        }
    }
    let trimmed = flattened.trim();
    let text = if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    };
    text.chars().take(limit).collect()
}

/// The control ranges upstream strips (`attention.ts:70`).
///
/// `\t` survives as a space via the newline collapse above; everything else in
/// C0, DEL, and C1 goes, because a tray or a terminal would each interpret them
/// differently.
fn is_control(character: char) -> bool {
    matches!(character, '\u{0}'..='\u{9}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// Remove ANSI escape sequences.
///
/// Hand-rolled rather than pulled in as a dependency: the shapes that reach a
/// session title are CSI (`ESC [ … final`) and the string sequences
/// (`OSC`/`DCS`/`SOS`/`PM`/`APC`, terminated by `BEL` or `ESC \`), and covering
/// them is a dozen lines. Anything unrecognized after `ESC` drops the `ESC` and
/// keeps the text, which is the conservative direction: a stray letter is
/// harmless, a stray `ESC` is not.
fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        match chars.next() {
            // CSI: parameter and intermediate bytes, then a final byte in `@`..`~`.
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // String sequences run until BEL or ST (`ESC \`).
            Some(']' | 'P' | 'X' | '^' | '_') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-character sequence such as `ESC c`; the second byte is part of
            // the sequence and is dropped with it.
            Some(_) | None => {}
        }
    }
    output
}
