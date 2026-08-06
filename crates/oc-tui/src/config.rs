//! The TUI-only configuration surface.
//!
//! This schema is deliberately **not** part of [`oc_config`]. None of these keys
//! appears in the main configuration vocabulary
//! (`packages/core/src/v1/config/config.ts`); upstream declares them in
//! `packages/tui/src/config/index.tsx:53-66` and loads them from separate
//! `tui.json`/`tui.jsonc` files
//! (`packages/opencode/src/config/tui.ts:27,196-206`). Modelling them in
//! `oc-config` would advertise keys the real binary rejects there, so they live
//! next to the only code that consumes them.
//!
//! # Unknown keys are ignored, on purpose
//!
//! Upstream's `Info` is a plain `Schema.Struct`, and Effect Schema's default
//! `onExcessProperty` is `"ignore"`. This module matches that rather than
//! `oc-config`'s top-level `deny_unknown_fields`, which is reserved for the main
//! config's explicit `unrecognized_keys` pass. The practical consequence is that
//! the surfaces this todo does not own — `theme`, `attention`, `plugin`,
//! `plugin_enabled` — are silently tolerated until the todos that own them add
//! their fields, instead of turning a partially landed schema into a parse error.
//!
//! Keybind names are the one exception: an unrecognized *keybind* is reported,
//! because upstream reports it too (`packages/tui/src/config/keybind.ts:450-451`).
//! A typo there is silent breakage of a key the user believes they rebound.
//!
//! # Scope
//!
//! Discovery and the multi-file merge order (`tui.ts:150-206`) are not
//! implemented here. This module owns the *vocabulary* and its defaults; the
//! layer that walks directories and merges files is a separate concern.

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::fmt;
use std::num::{NonZeroU16, NonZeroU64};
use std::time::Duration;

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

/// The leader-key timeout applied when the user configures none.
///
/// `packages/tui/src/config/index.tsx:21` — `LeaderTimeoutDefault = 2000`.
pub const DEFAULT_LEADER_TIMEOUT: Duration = Duration::from_millis(2000);

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
}

/// How a diff is laid out (`index.tsx:30-32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStyle {
    /// Adapt the column count to the terminal width.
    Auto,
    /// Always render a single stacked column.
    Stacked,
}

/// Scroll acceleration settings (`index.tsx:27-29`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

/// Prompt size settings (`index.tsx:45-51`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub struct PromptConfig {
    /// Maximum textarea height in rows.
    #[serde(default)]
    pub max_height: Option<NonZeroU16>,
    /// Maximum home prompt width.
    #[serde(default)]
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

/// The TUI configuration exactly as written, before defaults are applied.
///
/// Field set from `packages/tui/src/config/index.tsx:53-66`. The keys this todo
/// does not own (`theme`, `attention`, `plugin`, `plugin_enabled`) are absent and
/// tolerated by the ignore-unknown policy documented on this module, so the todos
/// that own them add one field each without touching anything here.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct TuiConfig {
    /// JSON Schema reference, accepted and unused.
    #[serde(default, rename = "$schema")]
    pub schema: Option<String>,
    /// Per-action keybind overrides, keyed by action name.
    #[serde(default)]
    pub keybinds: BTreeMap<String, BindingValue>,
    /// Leader-key timeout in milliseconds.
    #[serde(default)]
    pub leader_timeout: Option<NonZeroU64>,
    /// Prompt size settings.
    #[serde(default)]
    pub prompt: Option<PromptConfig>,
    /// Lines scrolled per wheel notch.
    #[serde(default)]
    pub scroll_speed: Option<f64>,
    /// Scroll acceleration settings.
    #[serde(default)]
    pub scroll_acceleration: Option<ScrollAcceleration>,
    /// Diff rendering style.
    #[serde(default)]
    pub diff_style: Option<DiffStyle>,
    /// Whether mouse capture is enabled.
    #[serde(default)]
    pub mouse: Option<bool>,
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
    /// Whether mouse capture is enabled (`index.tsx:115` — default `true`).
    pub mouse: bool,
}

impl TuiConfig {
    /// Parse one TUI configuration document from JSON text.
    pub fn from_json_str(text: &str) -> Result<Self, TuiConfigError> {
        serde_json::from_str(text).map_err(|error| TuiConfigError::Parse {
            message: error.to_string(),
        })
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
        })
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
        }
    }
}
