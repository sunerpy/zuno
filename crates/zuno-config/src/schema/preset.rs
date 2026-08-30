//! Team-wide model routing presets.
//!
//! Presets are configuration data, never a compiled model fallback table. The
//! runtime may ship the routing mechanism, but only the user names concrete models.

use crate::schema::agent::AgentReasoning;
use crate::schema::ordered::OrderedMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One model choice inside a preset.
///
/// A string is the concise form. The object form adds a provider-neutral reasoning
/// level without exposing provider-specific option objects in the team preset.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PresetModelConfig {
    /// A qualified `provider/model` id with no reasoning override.
    Model(String),
    /// A qualified model plus its canonical reasoning level.
    Options(PresetModelOptions),
}

impl PresetModelConfig {
    /// The configured `provider/model` id.
    #[must_use]
    pub fn model(&self) -> &str {
        match self {
            Self::Model(model) => model,
            Self::Options(options) => &options.model,
        }
    }

    /// The configured provider-neutral reasoning level, when present.
    #[must_use]
    pub const fn reasoning(&self) -> Option<AgentReasoning> {
        match self {
            Self::Model(_) => None,
            Self::Options(options) => options.reasoning,
        }
    }
}

/// The expanded form of a preset model choice.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetModelOptions {
    /// Model in `provider/model` form.
    pub model: String,
    /// Provider-neutral reasoning level for this route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<AgentReasoning>,
}

/// One named team preset.
///
/// Agent routes drive direct and delegated agent selection. Categories are optional
/// semantic shorthands for workflow nodes that should not hard-code an agent name.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetConfig {
    /// Per-agent model choices.
    #[serde(default, skip_serializing_if = "OrderedMap::is_empty")]
    pub agents: OrderedMap<PresetModelConfig>,
    /// Semantic category model choices.
    #[serde(default, skip_serializing_if = "OrderedMap::is_empty")]
    pub categories: OrderedMap<PresetModelConfig>,
}
