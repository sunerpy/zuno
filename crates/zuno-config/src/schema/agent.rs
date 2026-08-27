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
use schemars::JsonSchema;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::num::NonZeroU32;

/// Where an agent may be used (`config/agent.ts:26`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Only reachable as a subagent.
    Subagent,
    /// Only reachable as a primary agent.
    Primary,
    /// Both.
    All,
}

/// Provider-neutral reasoning level selected for this agent.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentReasoning {
    /// Disable model reasoning when supported.
    Off,
    /// Use a small reasoning budget.
    Low,
    /// Use the normal reasoning budget.
    Medium,
    /// Use a large reasoning budget.
    High,
    /// Use the strongest broadly available effort.
    Xhigh,
    /// Use the strongest effort exposed by the model.
    Max,
}

impl AgentReasoning {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// A named theme colour (`config/agent.ts:9`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize)]
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

/// Keys carried outside the provider-options map.
///
/// `name` belongs to the containing map or Markdown frontmatter and is therefore
/// preserved in [`AgentConfig::extra`] without becoming a provider option.
pub const SWEEP_EXEMPT_KEYS: &[&str] = &["name"];

/// Field names that are not part of Zuno's agent schema.
pub const UNSUPPORTED_AGENT_KEYS: &[&str] = &["maxSteps"];

/// One entry of the `agents` map, or one Markdown agent definition's frontmatter.
///
/// Deserialization performs the oracle's sweep: any key this struct does not name
/// is copied into [`options`](Self::options) *and* kept verbatim in
/// [`extra`](Self::extra).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize)]
pub struct AgentConfig {
    /// Model in `provider/model` form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Default model variant, applied only with the agent's configured model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Provider-neutral reasoning level, applied only with the configured model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<AgentReasoning>,
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
    /// Exact model-visible tool allowlist for this agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Exact child-agent allowlist for direct delegation and workflows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegates: Option<Vec<String>>,
    /// Skills loaded at the start of every turn for this agent.
    #[serde(rename = "requiredSkills", skip_serializing_if = "Option::is_none")]
    pub required_skills: Option<Vec<String>>,
    /// Per-tool permissions for this agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionConfig>,
    /// Every key this struct does not name, verbatim and unswept.
    ///
    /// Provider-specific keys are also copied into [`options`](Self::options).
    #[serde(flatten)]
    pub extra: JsonMap,
}

fn validate_name_list(field: &str, values: Option<&[String]>) -> Result<(), String> {
    let Some(values) = values else {
        return Ok(());
    };
    if values.is_empty() {
        return Err(format!("agent `{field}` must not be empty when present"));
    }
    let mut unique = std::collections::BTreeSet::new();
    for value in values {
        if value.trim().is_empty() {
            return Err(format!("agent `{field}` entries must not be empty"));
        }
        if !unique.insert(value.as_str()) {
            return Err(format!("agent `{field}` contains duplicate `{value}`"));
        }
    }
    Ok(())
}

impl<'de> Deserialize<'de> for AgentConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = AgentWire::deserialize(deserializer)?;
        for key in UNSUPPORTED_AGENT_KEYS {
            if wire.extra.contains_key(*key) {
                let replacement = match *key {
                    "maxSteps" => "steps",
                    _ => unreachable!("unsupported agent keys have a native replacement"),
                };
                return Err(de::Error::custom(format!(
                    "unsupported agent field `{key}`; use `{replacement}` instead"
                )));
            }
        }
        if (wire.variant.is_some() || wire.reasoning.is_some()) && wire.model.is_none() {
            return Err(de::Error::custom(
                "agent `variant` and `reasoning` require an explicit `model`",
            ));
        }
        if wire.variant.is_some() && wire.reasoning.is_some() {
            return Err(de::Error::custom(
                "agent `variant` and `reasoning` are mutually exclusive",
            ));
        }
        validate_name_list("tools", wire.tools.as_deref()).map_err(de::Error::custom)?;
        validate_name_list("delegates", wire.delegates.as_deref()).map_err(de::Error::custom)?;
        validate_name_list("requiredSkills", wire.required_skills.as_deref())
            .map_err(de::Error::custom)?;
        Ok(wire.sweep())
    }
}

#[derive(JsonSchema, Deserialize)]
struct AgentWire {
    model: Option<String>,
    variant: Option<String>,
    reasoning: Option<AgentReasoning>,
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
    tools: Option<Vec<String>>,
    delegates: Option<Vec<String>>,
    #[serde(rename = "requiredSkills")]
    required_skills: Option<Vec<String>>,
    permission: Option<PermissionConfig>,
    #[serde(flatten)]
    extra: JsonMap,
}

impl AgentWire {
    /// Copy provider-specific top-level keys into the provider-options map.
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
            reasoning: self.reasoning,
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
            tools: self.tools,
            delegates: self.delegates,
            required_skills: self.required_skills,
            permission: self.permission,
            extra: self.extra,
        }
    }
}
