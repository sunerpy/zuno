//! The TUI-only configuration surface.
//!
//! This schema is deliberately **not** part of [`zuno_config`]. None of these keys
//! appears in the main configuration vocabulary
//! (`packages/core/src/v1/config/config.ts`); upstream declares them in
//! `packages/tui/src/config/index.tsx:53-66` and loads them from separate
//! `tui.json`/`tui.jsonc` files
//! (`packages/opencode/src/config/tui.ts:27,196-206`). Modelling them in
//! `zuno-config` would advertise keys the real binary rejects there, so they live
//! next to the only code that consumes them.
//!
//! # Unknown keys are ignored, on purpose
//!
//! Upstream's `Info` is a plain `Schema.Struct`, and Effect Schema's default
//! `onExcessProperty` is `"ignore"`. This module matches that rather than
//! `zuno-config`'s top-level `deny_unknown_fields`, which is reserved for the main
//! config's explicit `unrecognized_keys` pass. The practical consequence is that
//! the surfaces this todo does not own — `plugin`, `plugin_enabled` — are silently
//! tolerated until the todos that own them add their fields, instead of turning a
//! partially landed schema into a parse error.
//!
//! Keybind names are the one exception: an unrecognized *keybind* is reported,
//! because upstream reports it too (`packages/tui/src/config/keybind.ts:450-451`).
//! A typo there is silent breakage of a key the user believes they rebound.
//!
//! # Scope: this module reads files, but does not choose them
//!
//! [`ResolvedTuiConfig::discover`] reads and merges the multi-file layer stack
//! (`tui.ts:150-206`), but the *caller* supplies the ordered candidate paths.
//! `zuno-tui` depends on `zuno-engine`, `zuno-llm` and `zuno-permission` and
//! deliberately has no `zuno-paths` dependency: path policy — which XDG root, which
//! project marker, how far up the tree to walk — belongs to the layer that already
//! owns it, `zuno-cli`, which resolves paths and projects data into the TUI. Taking
//! a path list keeps this a leaf crate, and keeps the merge order auditable by the
//! one layer that can see every candidate at once instead of being reconstructed
//! from a directory walk buried here.
//!
//! Layers are ordered from lowest to highest precedence: **later paths win**. The
//! win is per key, not per document — a file that sets only `theme` leaves a lower
//! layer's `keybinds` intact — and nested objects merge the same way, so a file
//! that sets only `prompt.max_height` does not erase `prompt.max_width`.
//!
//! # Where a bad layer is reported, and how precisely
//!
//! A missing file is not an error; it is a layer that contributes nothing. A file
//! that exists but cannot be read or parsed is an error that **names the path**
//! ([`TuiConfigError::Read`], [`TuiConfigError::ParseFile`]).
//!
//! Shape errors are therefore per layer, but the range checks in
//! [`TuiConfig::resolve`] run once on the *merged* document and name the key rather
//! than a path. That asymmetry is deliberate: after merging, an out-of-range value
//! has no single file to blame, and validating each layer separately would reject a
//! bad value that a higher layer already replaced — failing on configuration that
//! has no effect. This diverges from `zuno-config`, which validates every layer
//! (`discovery.rs:150-155`), because that crate's layers each stand alone as a
//! published schema and these do not.

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::num::{NonZeroU16, NonZeroU64};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

/// The leader-key timeout applied when the user configures none.
///
/// Five seconds keeps the discoverability panel readable without slowing a completed
/// chord: a valid continuation still dispatches as soon as its next key arrives.
pub const DEFAULT_LEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// The smallest scroll speed upstream accepts (`index.tsx:26`).
pub const MIN_SCROLL_SPEED: f64 = 0.001;

/// A TUI configuration value could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TuiConfigError {
    /// The document was not valid JSON, or a value had the wrong shape.
    #[error("failed to parse the TUI configuration: {message}")]
    Parse {
        /// The deserializer's own message, which already names the failing key.
        message: String,
    },
    /// A numeric value fell outside the range upstream requires.
    #[error("`{key}` must be {expected}, but the configuration has `{found}`")]
    OutOfRange {
        /// The dotted configuration key.
        key: &'static str,
        /// What the value has to satisfy.
        expected: &'static str,
        /// The rejected value, rendered.
        found: String,
    },
    /// A layer's file exists but could not be read.
    ///
    /// A *missing* file is not this error — it is a layer that contributes nothing
    /// ([`TuiConfig::from_path`]). This is the case where the file is there, so the
    /// user's intent is visible on disk, but unreachable: a permission bit, a
    /// directory where a file was expected. Continuing would start a TUI that
    /// ignores settings the user can see.
    #[error("failed to read the TUI configuration at {path}: {message}")]
    Read {
        /// The layer that could not be read.
        path: PathBuf,
        /// The operating system's own message.
        ///
        /// A rendered `String` rather than the `std::io::Error`, because this enum
        /// is `Clone + Eq` — which is what lets a test assert a whole error value —
        /// and `io::Error` is neither. [`Parse`](Self::Parse) already keeps its
        /// `serde_json::Error` this way.
        message: String,
    },
    /// A layer was read but its contents were rejected.
    ///
    /// The path is the one thing [`Parse`](Self::Parse) cannot carry: with several
    /// layers in play, "invalid JSON" without a filename leaves the user opening
    /// every candidate looking for the typo.
    #[error("failed to parse the TUI configuration at {path}: {message}")]
    ParseFile {
        /// The offending layer.
        path: PathBuf,
        /// The deserializer's own message, which already names the failing key.
        message: String,
    },
}

/// How a diff is laid out (`index.tsx:30-32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStyle {
    /// Adapt the column count to the terminal width.
    Auto,
    /// Always render a single stacked column.
    Stacked,
}

/// Scroll acceleration settings (`index.tsx:27-29`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct ScrollAcceleration {
    /// Whether velocity accumulates across consecutive scroll events.
    pub enabled: bool,
}

/// The home prompt's maximum width (`index.tsx:48-50`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxWidth {
    /// Scale with the terminal width.
    Auto,
    /// A fixed column cap.
    Columns(NonZeroU16),
}

impl<'de> Deserialize<'de> for MaxWidth {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MaxWidthVisitor;

        impl Visitor<'_> for MaxWidthVisitor {
            type Value = MaxWidth;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a positive integer column count or the string \"auto\"")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                u16::try_from(value)
                    .ok()
                    .and_then(NonZeroU16::new)
                    .map(MaxWidth::Columns)
                    .ok_or_else(|| E::custom(format!("`prompt.max_width` must be a positive integer no larger than {}, but the configuration has `{value}`", u16::MAX)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value == "auto" {
                    return Ok(MaxWidth::Auto);
                }
                Err(E::custom(format!(
                    "`prompt.max_width` accepts a positive integer or \"auto\", but the configuration has `{value}`"
                )))
            }
        }

        deserializer.deserialize_any(MaxWidthVisitor)
    }
}

impl Serialize for MaxWidth {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // The two shapes have to stay asymmetric, because the deserializer above
        // distinguishes them by JSON type, not by a tag.
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Columns(columns) => serializer.serialize_u16(columns.get()),
        }
    }
}

/// Prompt size settings (`index.tsx:45-51`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct PromptConfig {
    /// Maximum textarea height in rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<NonZeroU16>,
    /// Maximum home prompt width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<MaxWidth>,
}

/// One key spelling for a binding.
///
/// Upstream's `BindingItem` is `string | KeyStroke | BindingObject`
/// (`keybind.ts:8-27`). The object form is not exotic: the default table itself
/// uses it for `input_paste` (`keybind.ts:162`), so the type has to exist even
/// before a user writes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingItem {
    /// The key spelling, e.g. `ctrl+c` or `<leader>q`. A comma-separated string
    /// carries several spellings and is split by the keybind engine.
    pub key: String,
    /// Whether the terminal's own handling of this key is suppressed.
    pub prevent_default: Option<bool>,
}

impl BindingItem {
    /// A plain string spelling with no flags.
    #[must_use]
    pub fn plain(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            prevent_default: None,
        }
    }
}

impl<'de> Deserialize<'de> for BindingItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ItemVisitor;

        impl<'de> Visitor<'de> for ItemVisitor {
            type Value = BindingItem;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a key spelling, or an object with a `key` field")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(BindingItem::plain(value))
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut key: Option<String> = None;
                let mut prevent_default = None;
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "key" => key = Some(map.next_value()?),
                        "preventDefault" => prevent_default = Some(map.next_value()?),
                        // `event`, `fallthrough`, and the `StructWithRest` tail
                        // (`keybind.ts:20-24`) are accepted and ignored: the
                        // engine here has no renderable tree to fall through to.
                        _ => {
                            map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                key.map(|key| BindingItem {
                    key,
                    prevent_default,
                })
                .ok_or_else(|| de::Error::missing_field("key"))
            }
        }

        deserializer.deserialize_any(ItemVisitor)
    }
}

impl Serialize for BindingItem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.prevent_default {
            // The string form is what a user writes and what the default table
            // holds, so emitting the object form unconditionally would rewrite
            // every plain spelling into noise on the way back out.
            None => serializer.serialize_str(&self.key),
            Some(prevent_default) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("key", &self.key)?;
                // Upstream spells this key in camelCase (`keybind.ts:8-27`); the
                // deserializer above reads that spelling and nothing else.
                map.serialize_entry("preventDefault", &prevent_default)?;
                map.end()
            }
        }
    }
}

/// What the user asked one binding to be.
///
/// Mirrors `BindingValueSchema` (`keybind.ts:28-33`): `false`, the literal
/// `"none"`, one item, or an array of items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingValue {
    /// The action has no key. `false` and `"none"` are the same request.
    Disabled,
    /// One or more key spellings.
    Keys(Vec<BindingItem>),
}

impl BindingValue {
    /// Build a value from an upstream-style comma-separated spelling.
    ///
    /// `"none"` is the disabling sentinel, not a key named "none".
    #[must_use]
    pub fn parse(spelling: &str) -> Self {
        if spelling == "none" {
            return Self::Disabled;
        }
        Self::Keys(vec![BindingItem::plain(spelling)])
    }

    /// The individual spellings this value contributes, with `,` already split.
    #[must_use]
    pub fn spellings(&self) -> Vec<&str> {
        match self {
            Self::Disabled => Vec::new(),
            Self::Keys(items) => items
                .iter()
                .flat_map(|item| item.key.split(','))
                .map(str::trim)
                .filter(|spelling| !spelling.is_empty() && *spelling != "none")
                .collect(),
        }
    }
}

impl<'de> Deserialize<'de> for BindingValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = BindingValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("`false`, a key spelling, or an array of key spellings")
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                if value {
                    return Err(E::custom(
                        "`true` is not a keybind value; use a key spelling, or `false` to unbind",
                    ));
                }
                Ok(BindingValue::Disabled)
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(BindingValue::parse(value))
            }

            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                let item = BindingItem::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(BindingValue::Keys(vec![item]))
            }

            fn visit_seq<S: de::SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element::<BindingItem>()? {
                    items.push(item);
                }
                if items.is_empty() {
                    return Ok(BindingValue::Disabled);
                }
                Ok(BindingValue::Keys(items))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

impl Serialize for BindingValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            // `false` rather than the `"none"` sentinel: both deserialize back to
            // `Disabled`, and `false` cannot be misread as a key named "none".
            Self::Disabled => serializer.serialize_bool(false),
            // A lone item is emitted bare, matching how the single-spelling form is
            // written; wrapping it in an array would round-trip but churn the file.
            Self::Keys(items) => match items.as_slice() {
                [item] => item.serialize(serializer),
                items => items.serialize(serializer),
            },
        }
    }
}

/// The TUI configuration exactly as written, before defaults are applied.
///
/// Field set from `packages/tui/src/config/index.tsx:53-66`, plus `theme` and
/// `attention`, which those layers added here rather than redefining the type. The
/// keys still unowned (`plugin`, `plugin_enabled`) are absent and tolerated by the
/// ignore-unknown policy documented on this module, so the todos that own them add
/// one field each without touching anything here.
///
/// Every field skips serializing when unset, so writing a parsed configuration
/// back out yields only the keys the user actually spoke about instead of a wall
/// of nulls and empty objects.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct TuiConfig {
    /// JSON Schema reference, accepted and unused.
    #[serde(default, rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Per-action keybind overrides, keyed by action name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keybinds: BTreeMap<String, BindingValue>,
    /// Leader-key timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_timeout: Option<NonZeroU64>,
    /// Prompt size settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptConfig>,
    /// Lines scrolled per wheel notch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_speed: Option<f64>,
    /// Scroll acceleration settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_acceleration: Option<ScrollAcceleration>,
    /// Diff rendering style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_style: Option<DiffStyle>,
    /// Whether application mouse handling is enabled.
    ///
    /// Disabled by default so the terminal retains native drag selection and copy
    /// across the transcript, sidebar, prompt, dialogs, and notices. Set this to
    /// `true` only when click-to-toggle sections and wheel scrolling are preferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouse: Option<bool>,
    /// The theme to render with.
    ///
    /// Any name any theme layer provides, including the special value `system`,
    /// which derives a palette from the terminal's own colours. Absent means the
    /// built-in default ([`crate::theme::DEFAULT_THEME`]). A name no layer provides
    /// is not an error: [`crate::theme::ThemeRegistry::resolve`] falls back and
    /// reports a diagnostic naming it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Notification and sound-cue settings.
    ///
    /// The vocabulary lives with the only code that reads it
    /// ([`crate::attention::AttentionSettings`]), so this stays a single field the
    /// way `theme` does. Absent means every default, and the master default is
    /// **off** — nothing here makes noise until a user asks for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<crate::attention::AttentionSettings>,
}

impl TuiConfig {
    /// The configured theme name, or `None` when the key was omitted.
    #[must_use]
    pub fn theme(&self) -> Option<&str> {
        self.theme.as_deref()
    }
}

/// Host facts that change how the configuration resolves.
///
/// `packages/tui/src/config/index.tsx:83-86`, supplied by
/// `packages/opencode/src/config/tui.ts:216` as `process.platform !== "win32"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Whether the host can be suspended with `SIGTSTP`.
    pub terminal_suspend: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            terminal_suspend: cfg!(unix),
        }
    }
}

/// The TUI configuration with every default applied.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTuiConfig {
    /// Keybind overrides, after the terminal-suspend rewrite below.
    pub keybinds: BTreeMap<String, BindingValue>,
    /// Leader-key timeout.
    pub leader_timeout: Duration,
    /// Prompt size settings.
    pub prompt: PromptConfig,
    /// Lines scrolled per wheel notch, when configured.
    pub scroll_speed: Option<f64>,
    /// Scroll acceleration settings, when configured.
    pub scroll_acceleration: Option<ScrollAcceleration>,
    /// Diff rendering style, when configured.
    pub diff_style: Option<DiffStyle>,
    /// Whether application mouse handling is enabled. Defaults to `true` so the
    /// transcript can provide pane-bounded selection and a draggable scrollbar.
    pub mouse: bool,
    /// The theme name to render with, with the default already applied.
    ///
    /// A `String` rather than an `Option<String>` because this struct is the layer
    /// where defaults live, and the absent key has exactly one meaning:
    /// [`crate::theme::DEFAULT_THEME`]. Keeping the `Option` here would push that
    /// same `unwrap_or` onto every render site, which is how a default drifts.
    /// [`crate::theme::ThemeRegistry::resolve`] takes a name, so a caller passes
    /// this field directly.
    ///
    /// An unknown name — the empty string included — is carried through verbatim
    /// rather than normalized or rejected. The raw [`TuiConfig::theme`] field
    /// already documents that a name no layer provides is not an error: the
    /// registry falls back and reports a diagnostic *naming* it. Rewriting `""` to
    /// the default here would be the one behaviour that loses what the user wrote,
    /// and rejecting it would make a cosmetic key fatal while a mere typo stays
    /// survivable — the inconsistent half of "reject rather than ignore". Carrying
    /// the value is the option that neither ignores nor over-rejects.
    pub theme: String,
    /// Notification and sound-cue settings, deliberately still unresolved.
    ///
    /// This is [`crate::attention::AttentionSettings`] and *not* `ResolvedAttention`
    /// because [`crate::attention::Attention::from_settings`] is the constructor a
    /// caller wants, and it takes the settings: it derives the resolved block and
    /// the load-time diagnostics together, so the volume-clamp report survives.
    /// Storing the resolved block here would force a caller onto
    /// `Attention::new`, which takes no diagnostics and would silently drop that
    /// report — resolving early would cost information, not save work.
    pub attention: crate::attention::AttentionSettings,
}

/// Overwrite `base` only when the higher layer spoke.
///
/// The whole per-key merge rests on this: `None` is "said nothing", so it must not
/// be able to clear a value a lower layer set. `Option::or` reads the other way
/// round and is the easy way to invert the precedence by accident.
fn merge_option<T>(base: &mut Option<T>, higher: Option<T>) {
    if let Some(value) = higher {
        *base = Some(value);
    }
}

/// Merge `higher`'s prompt keys over `base`'s, one key at a time.
///
/// Replacing the whole block would make `{"prompt": {"max_height": 12}}` in a
/// higher layer erase a lower layer's `max_width`, which is the per-document
/// override this module exists to avoid.
fn merge_prompt(base: &mut Option<PromptConfig>, higher: Option<PromptConfig>) {
    let Some(higher) = higher else { return };
    match base {
        Some(base) => {
            merge_option(&mut base.max_height, higher.max_height);
            merge_option(&mut base.max_width, higher.max_width);
        }
        None => *base = Some(higher),
    }
}

/// Merge `higher`'s attention keys over `base`'s, one key at a time.
///
/// Lives here rather than on [`crate::attention::AttentionSettings`] because merge
/// order is this module's concern, not the audio layer's; every field it touches is
/// public. `sounds` is a map, so it unions per slot: a higher layer that overrides
/// the permission cue keeps a lower layer's done cue.
fn merge_attention(
    base: &mut Option<crate::attention::AttentionSettings>,
    higher: Option<crate::attention::AttentionSettings>,
) {
    let Some(higher) = higher else { return };
    match base {
        Some(base) => {
            merge_option(&mut base.enabled, higher.enabled);
            merge_option(&mut base.notifications, higher.notifications);
            merge_option(&mut base.sound, higher.sound);
            merge_option(&mut base.volume, higher.volume);
            merge_option(&mut base.sound_pack, higher.sound_pack);
            base.sounds.extend(higher.sounds);
        }
        None => *base = Some(higher),
    }
}

impl TuiConfig {
    /// Parse one TUI configuration document from JSON text.
    pub fn from_json_str(text: &str) -> Result<Self, TuiConfigError> {
        Self::parse_json(text).map_err(|message| TuiConfigError::Parse { message })
    }

    /// The one parser, so the two error skins cannot drift apart.
    fn parse_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|error| error.to_string())
    }

    /// Read one configuration layer from `path`.
    ///
    /// `Ok(None)` is a file that is not there: the overwhelmingly common case, since
    /// a caller offers every candidate path and a user writes at most one or two.
    ///
    /// # Errors
    ///
    /// [`TuiConfigError::Read`] when the file exists but cannot be read, and
    /// [`TuiConfigError::ParseFile`] when it is not valid JSON or a value has the
    /// wrong shape. Both name `path`.
    pub fn from_path(path: &Path) -> Result<Option<Self>, TuiConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            // Only `NotFound` is absence. Anything else — a permission bit, a
            // directory — is a file the user can see, and skipping it would be the
            // silent ignore this schema refuses everywhere else.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(TuiConfigError::Read {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                });
            }
        };
        Self::parse_json(&text)
            .map(Some)
            .map_err(|message| TuiConfigError::ParseFile {
                path: path.to_path_buf(),
                message,
            })
    }

    /// Fold `higher` onto `self` key by key, with `higher` winning every collision.
    ///
    /// Scalars and whole binding values are replaced; maps union per key; the two
    /// nested blocks ([`PromptConfig`], [`crate::attention::AttentionSettings`])
    /// merge field-wise. This mirrors `zuno-config`'s rule — objects merge deeply,
    /// scalars are replaced (`discovery.rs:6-8`) — so the two crates do not teach a
    /// user two different precedence models.
    pub fn merge(&mut self, higher: Self) {
        merge_option(&mut self.schema, higher.schema);
        // Per key, so a higher layer that rebinds one action leaves every other
        // binding a lower layer set. Replacing the map would make the nearest file
        // the only file whose keybinds exist at all.
        self.keybinds.extend(higher.keybinds);
        merge_option(&mut self.leader_timeout, higher.leader_timeout);
        merge_prompt(&mut self.prompt, higher.prompt);
        merge_option(&mut self.scroll_speed, higher.scroll_speed);
        merge_option(&mut self.scroll_acceleration, higher.scroll_acceleration);
        merge_option(&mut self.diff_style, higher.diff_style);
        merge_option(&mut self.mouse, higher.mouse);
        merge_option(&mut self.theme, higher.theme);
        merge_attention(&mut self.attention, higher.attention);
    }

    /// Read every layer in `paths` and merge them, lowest precedence first.
    ///
    /// `paths` is ordered by the caller: index `0` is the weakest layer and the last
    /// entry wins. Absent files drop out silently, so a caller offers candidates
    /// rather than having to probe for them first.
    ///
    /// # Errors
    ///
    /// As [`from_path`](Self::from_path), for the first layer that exists and cannot
    /// be read or parsed. Later layers are not read: reporting the first failure is
    /// what keeps the message about one file.
    pub fn layered(paths: &[PathBuf]) -> Result<Self, TuiConfigError> {
        let mut merged = Self::default();
        for path in paths {
            if let Some(layer) = Self::from_path(path)? {
                merged.merge(layer);
            }
        }
        Ok(merged)
    }

    /// Apply defaults and the host-dependent rewrites.
    ///
    /// The one rewrite upstream performs is the terminal-suspend fork
    /// (`index.tsx:90-98`): on a host that cannot suspend, `ctrl+z` is worth more
    /// as undo than as a no-op, so `terminal_suspend` is unbound and `ctrl+z` is
    /// *prepended* to `input_undo` — but only when the user has not already
    /// spoken about `input_undo` themselves.
    pub fn resolve(self, options: ResolveOptions) -> Result<ResolvedTuiConfig, TuiConfigError> {
        if let Some(speed) = self.scroll_speed
            && (speed.is_nan() || speed < MIN_SCROLL_SPEED)
        {
            return Err(TuiConfigError::OutOfRange {
                key: "scroll_speed",
                expected: "at least 0.001",
                found: speed.to_string(),
            });
        }

        let mut keybinds = self.keybinds;
        if !options.terminal_suspend {
            keybinds.insert("terminal_suspend".to_owned(), BindingValue::Disabled);
            // The default spelling is read from the binding table rather than
            // restated here, so this rewrite cannot drift away from it.
            keybinds.entry("input_undo".to_owned()).or_insert_with(|| {
                match crate::keybind::default_spelling("input_undo") {
                    Some(default) => BindingValue::parse(&format!("ctrl+z,{default}")),
                    None => BindingValue::parse("ctrl+z"),
                }
            });
        }

        Ok(ResolvedTuiConfig {
            keybinds,
            leader_timeout: self
                .leader_timeout
                .map_or(DEFAULT_LEADER_TIMEOUT, |ms| Duration::from_millis(ms.get())),
            prompt: self.prompt.unwrap_or_default(),
            scroll_speed: self.scroll_speed,
            scroll_acceleration: self.scroll_acceleration,
            diff_style: self.diff_style,
            mouse: self.mouse.unwrap_or(true),
            theme: self
                .theme
                .unwrap_or_else(|| crate::theme::DEFAULT_THEME.to_owned()),
            attention: self.attention.unwrap_or_default(),
        })
    }
}

impl ResolvedTuiConfig {
    /// Read every layer in `paths`, merge them, and apply defaults — the whole
    /// file-to-usable-configuration path in one call.
    ///
    /// `paths` runs from lowest to highest precedence, so the last entry wins; see
    /// this module's header for why the caller owns that order. Absent files
    /// contribute nothing, so passing every candidate is the expected use.
    ///
    /// Merging finishes before [`TuiConfig::resolve`] runs, which is what the
    /// terminal-suspend rewrite depends on: that rewrite only prepends `ctrl+z` to
    /// `input_undo` when the user has *not* spoken about `input_undo`, and resolving
    /// per layer would hide an `input_undo` written in a lower layer behind a higher
    /// layer that is silent about it.
    ///
    /// # Errors
    ///
    /// As [`TuiConfig::layered`] for a layer that cannot be read or parsed, naming
    /// the path; and as [`TuiConfig::resolve`] for a value outside its legal range in
    /// the merged result, naming the key.
    pub fn discover(paths: &[PathBuf], options: ResolveOptions) -> Result<Self, TuiConfigError> {
        TuiConfig::layered(paths)?.resolve(options)
    }
}

impl Default for ResolvedTuiConfig {
    fn default() -> Self {
        Self {
            keybinds: BTreeMap::new(),
            leader_timeout: DEFAULT_LEADER_TIMEOUT,
            prompt: PromptConfig::default(),
            scroll_speed: None,
            scroll_acceleration: None,
            diff_style: None,
            mouse: true,
            theme: crate::theme::DEFAULT_THEME.to_owned(),
            attention: crate::attention::AttentionSettings::default(),
        }
    }
}
