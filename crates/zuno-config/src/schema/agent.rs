//! Agent configuration, including the unknown-key sweep into `options`.
//!
//! Oracle: `packages/core/src/v1/config/agent.ts:7-89`.
//!
//! The important part is `normalize` at `:62-81`. The agent schema is
//! `StructWithRest(..., [Record(String, Any)])`, and decoding copies every key
//! *not* in `KNOWN_KEYS` (`:43-60`) into `options`. That is the mechanism by which
//! `reasoningEffort`, `thinking`, and any other provider-specific knob written at
//! the top level of an agent definition reaches the provider — so it is reproduced
//! here rather than left to a later pass.

use crate::schema::JsonMap;
use crate::schema::permission::PermissionConfig;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::num::NonZeroU32;

/// Where an agent may be used (`config/agent.ts:26`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Only reachable as a subagent.
    Subagent,
    /// Only reachable as a primary agent.
    Primary,
    /// Both.
    All,
}

/// A named theme colour (`config/agent.ts:9`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeColor {
    /// Primary.
    Primary,
    /// Secondary.
    Secondary,
    /// Accent.
    Accent,
    /// Success.
    Success,
    /// Warning.
    Warning,
    /// Error.
    Error,
    /// Info.
    Info,
}

/// An agent's colour: a six-digit hex code, or a theme colour
/// (`config/agent.ts:7-10`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AgentColor {
    /// A theme colour.
    Theme(ThemeColor),
    /// A `#rrggbb` hex code, validated on the way in.
    Hex(String),
}

impl<'de> Deserialize<'de> for AgentColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AgentColorVisitor;

        impl Visitor<'_> for AgentColorVisitor {
            type Value = AgentColor;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a '#rrggbb' hex code or a theme colour name")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                let theme = match value {
                    "primary" => Some(ThemeColor::Primary),
                    "secondary" => Some(ThemeColor::Secondary),
                    "accent" => Some(ThemeColor::Accent),
                    "success" => Some(ThemeColor::Success),
                    "warning" => Some(ThemeColor::Warning),
                    "error" => Some(ThemeColor::Error),
                    "info" => Some(ThemeColor::Info),
                    _ => None,
                };
                if let Some(theme) = theme {
                    return Ok(AgentColor::Theme(theme));
                }
                if is_hex_color(value) {
                    return Ok(AgentColor::Hex(value.to_owned()));
                }
                Err(de::Error::invalid_value(
                    de::Unexpected::Str(value),
                    &"a '#rrggbb' hex code or a theme colour name",
                ))
            }
        }

        deserializer.deserialize_str(AgentColorVisitor)
    }
}

/// The oracle's `/^#[0-9a-fA-F]{6}$/` (`config/agent.ts:8`), without a regex
/// dependency.
fn is_hex_color(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('#') else {
        return false;
    };
    digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Keys that reach [`AgentConfig::extra`] but must NOT be swept into `options`.
///
/// All three are in the oracle's `KNOWN_KEYS` (`config/agent.ts:43-60`), so the
/// oracle never sweeps them either:
///
/// * `name` — carried by the map key or the Markdown frontmatter, not a schema
///   field even in the oracle.
/// * `tools` and `maxSteps` — deprecated. This schema deliberately does not name
///   them (they belong to the legacy-rejection pass), but sweeping them into
///   provider options would be worse than either accepting or rejecting them:
///   a deprecated key would silently become an API argument.
pub const SWEEP_EXEMPT_KEYS: &[&str] = &["name", "tools", "maxSteps"];

/// One entry of the `agent` map, or one Markdown agent definition's frontmatter.
///
/// Deserialization performs the oracle's sweep: any key this struct does not name
/// is copied into [`options`](Self::options) *and* kept verbatim in
/// [`extra`](Self::extra).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct AgentConfig {
    /// Model in `provider/model` form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Default model variant, applied only with the agent's configured model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// System prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Remove this agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable: Option<bool>,
    /// When to use the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the agent may be used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<AgentMode>,
    /// Hide the agent from the `@` autocomplete menu.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Provider options, including every swept unknown key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<JsonMap>,
    /// Display colour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<AgentColor>,
    /// Maximum agentic iterations before a text-only response is forced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<NonZeroU32>,
    /// Per-tool permissions for this agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionConfig>,
    /// Every key this struct does not name, verbatim and unswept.
    ///
    /// A later pass reads this to reject deprecated keys; the sweep has already
    /// copied the non-exempt ones into [`options`](Self::options).
    #[serde(flatten)]
    pub extra: JsonMap,
}

impl<'de> Deserialize<'de> for AgentConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(AgentWire::deserialize(deserializer)?.sweep())
    }
}

#[derive(Deserialize)]
struct AgentWire {
    model: Option<String>,
    variant: Option<String>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    prompt: Option<String>,
    disable: Option<bool>,
    description: Option<String>,
    mode: Option<AgentMode>,
    hidden: Option<bool>,
    options: Option<JsonMap>,
    color: Option<AgentColor>,
    steps: Option<NonZeroU32>,
    permission: Option<PermissionConfig>,
    #[serde(flatten)]
    extra: JsonMap,
}

impl AgentWire {
    /// `normalize` from `config/agent.ts:62-81`, minus the parts that belong to
    /// later passes: the `tools`-to-`permission` translation and the
    /// `maxSteps`-to-`steps` fallback both act on keys this schema rejects.
    fn sweep(self) -> AgentConfig {
        let mut options = self.options;
        for (key, value) in &self.extra {
            if SWEEP_EXEMPT_KEYS.contains(&key.as_str()) {
                continue;
            }
            options
                .get_or_insert_with(JsonMap::new)
                .insert(key.clone(), value.clone());
        }
        AgentConfig {
            model: self.model,
            variant: self.variant,
            temperature: self.temperature,
            top_p: self.top_p,
            prompt: self.prompt,
            disable: self.disable,
            description: self.description,
            mode: self.mode,
            hidden: self.hidden,
            options,
            color: self.color,
            steps: self.steps,
            permission: self.permission,
            extra: self.extra,
        }
    }
}
