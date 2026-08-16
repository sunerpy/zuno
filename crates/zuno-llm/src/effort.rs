//! Provider-neutral reasoning effort and provider-specific request options.
//!
//! A session chooses one [`ReasoningEffort`]. The provider adapters do not each
//! reinterpret that choice: they call [`resolve_effort`] and merge the returned
//! options into their outbound body. A model catalog may declare an exact option
//! object for a level; that object wins over every generic rule here. This is how
//! newer models can use native adaptive reasoning while older models in the same
//! provider family continue to use token budgets, without model-name policy in
//! the binary.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::str::FromStr;

/// The user-facing, provider-neutral reasoning scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Disable reasoning when the provider supports doing so.
    Off,
    /// Spend the smallest useful reasoning budget.
    Low,
    /// Use the provider's normal reasoning budget.
    Medium,
    /// Use a large reasoning budget.
    High,
    /// Use the strongest broadly available native effort.
    Xhigh,
    /// Use the strongest effort exposed by the model.
    Max,
}

impl ReasoningEffort {
    /// All levels, weakest to strongest.
    pub const ALL: [Self; 6] = [
        Self::Off,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    /// The canonical configuration spelling.
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

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = InvalidReasoningEffort;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => Err(InvalidReasoningEffort {
                value: value.to_owned(),
            }),
        }
    }
}

/// A configured effort name was not one of the canonical levels.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown reasoning effort `{value}`; expected one of off, low, medium, high, xhigh, max")]
pub struct InvalidReasoningEffort {
    /// The rejected configuration value.
    pub value: String,
}

/// Request-shape families with distinct reasoning controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFamily {
    /// OpenAI and genuinely compatible bodies using `reasoningEffort`.
    OpenAi,
    /// Anthropic bodies using `thinking` and, for adaptive models, `effort`.
    Anthropic,
    /// Amazon Bedrock bodies using `reasoningConfig`.
    Bedrock,
    /// Google Generative AI and Vertex bodies using `thinkingConfig`.
    Google,
    /// OpenRouter bodies using `reasoning.effort`.
    OpenRouter,
}

impl ProviderFamily {
    /// Every request-shape family covered by this resolver.
    pub const ALL: [Self; 5] = [
        Self::OpenAi,
        Self::Anthropic,
        Self::Bedrock,
        Self::Google,
        Self::OpenRouter,
    ];
}

/// Token budgets used when a model exposes budget-based reasoning.
///
/// The upper two defaults preserve the oracle's generic provider values. Lower
/// levels are deliberately explicit rather than inferred from a model name, and
/// a catalog can replace the complete option object through [`DeclaredVariants`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffortBudget {
    /// Budget for `low`.
    pub low: u32,
    /// Budget for `medium`.
    pub medium: u32,
    /// Budget for `high`.
    pub high: u32,
    /// Budget for `xhigh`.
    pub xhigh: u32,
    /// Budget for `max`.
    pub max: u32,
}

impl Default for EffortBudget {
    fn default() -> Self {
        Self {
            low: 1_024,
            medium: 4_096,
            high: 16_000,
            xhigh: 24_000,
            max: 31_999,
        }
    }
}

impl EffortBudget {
    fn tokens(self, effort: ReasoningEffort, ceiling: Option<u32>) -> Option<u32> {
        let tokens = match effort {
            ReasoningEffort::Off => return None,
            ReasoningEffort::Low => self.low,
            ReasoningEffort::Medium => self.medium,
            ReasoningEffort::High => self.high,
            ReasoningEffort::Xhigh => self.xhigh,
            ReasoningEffort::Max => self.max,
        };
        Some(ceiling.map_or(tokens, |limit| tokens.min(limit)))
    }
}

/// Model-declared reasoning capabilities that alter a generic family mapping.
///
/// These are catalog facts, not facts inferred from a model id. In particular,
/// `adaptive` selects the native adaptive shape and `token_budget` selects a
/// budget shape for Bedrock or Google models whose family also supports a named
/// effort shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EffortCapabilities {
    /// The model accepts an adaptive reasoning control.
    pub adaptive: bool,
    /// The model uses token budgets rather than named effort levels.
    pub token_budget: bool,
    /// Maximum budget accepted by this model after output-token constraints.
    pub max_budget_tokens: Option<u32>,
}

/// Exact model variant option objects, keyed by canonical effort.
///
/// Catalog resolution constructs this value from the model's declared variants.
/// Keeping the values as JSON preserves provider-specific fields that the spine
/// must not attempt to understand.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeclaredVariants {
    options: BTreeMap<ReasoningEffort, Map<String, Value>>,
}

impl DeclaredVariants {
    /// No model-specific overrides.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the exact provider options for `effort`.
    #[must_use]
    pub fn with(mut self, effort: ReasoningEffort, options: Map<String, Value>) -> Self {
        self.options.insert(effort, options);
        self
    }

    /// Return the model-declared options for `effort`, if present.
    #[must_use]
    pub fn get(&self, effort: ReasoningEffort) -> Option<&Map<String, Value>> {
        self.options.get(&effort)
    }

    /// Whether the model declares no effort variants.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.options.is_empty()
    }
}

/// Which rule produced an effort option object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionSource {
    /// The model catalog supplied an exact variant object.
    DeclaredVariant,
    /// The provider-family fallback supplied the object.
    GenericMapping,
}

/// Provider options for one canonical effort selection.
#[derive(Debug, Clone, PartialEq)]
pub struct EffortResolution {
    /// The canonical level requested by the user.
    pub effort: ReasoningEffort,
    /// Whether a model declaration or a generic mapping won.
    pub source: ResolutionSource,
    /// The exact options to merge into the provider's outbound body.
    pub options: Map<String, Value>,
}

impl EffortResolution {
    /// Recursively merge the resolved options into an outbound JSON body.
    ///
    /// Provider adapters should start from the same base body for each request.
    /// Reusing a body previously decorated for another effort can retain fields
    /// that do not exist in the new variant.
    pub fn apply_to(&self, body: &mut Map<String, Value>) {
        merge_objects(body, &self.options);
    }
}

/// Resolve one canonical effort into the request shape for `family`.
///
/// An exact model-declared variant always wins. Generic mappings use only the
/// provider family and declared capabilities; this function never reads a model
/// id, release date, or marketing name.
#[must_use]
pub fn resolve_effort(
    family: ProviderFamily,
    effort: ReasoningEffort,
    capabilities: EffortCapabilities,
    variants: &DeclaredVariants,
) -> EffortResolution {
    if let Some(options) = variants.get(effort) {
        return EffortResolution {
            effort,
            source: ResolutionSource::DeclaredVariant,
            options: options.clone(),
        };
    }

    let budget = EffortBudget::default();
    let value = match family {
        ProviderFamily::OpenAi => open_ai_options(effort),
        ProviderFamily::Anthropic => anthropic_options(effort, capabilities, budget),
        ProviderFamily::Bedrock => bedrock_options(effort, capabilities, budget),
        ProviderFamily::Google => google_options(effort, capabilities, budget),
        ProviderFamily::OpenRouter => open_router_options(effort),
    };

    EffortResolution {
        effort,
        source: ResolutionSource::GenericMapping,
        options: object(value),
    }
}

fn open_ai_options(effort: ReasoningEffort) -> Value {
    json!({ "reasoningEffort": open_ai_effort(effort) })
}

fn anthropic_options(
    effort: ReasoningEffort,
    capabilities: EffortCapabilities,
    budget: EffortBudget,
) -> Value {
    if effort == ReasoningEffort::Off {
        return json!({ "thinking": { "type": "disabled" } });
    }
    if capabilities.adaptive {
        return json!({
            "thinking": { "type": "adaptive" },
            "effort": effort.as_str(),
        });
    }
    let tokens = budget
        .tokens(effort, capabilities.max_budget_tokens)
        .expect("off effort returned before budget lookup");
    json!({ "thinking": { "type": "enabled", "budgetTokens": tokens } })
}

fn bedrock_options(
    effort: ReasoningEffort,
    capabilities: EffortCapabilities,
    budget: EffortBudget,
) -> Value {
    if effort == ReasoningEffort::Off {
        return json!({ "reasoningConfig": { "type": "disabled" } });
    }
    if capabilities.token_budget {
        let tokens = budget
            .tokens(effort, capabilities.max_budget_tokens)
            .expect("off effort returned before budget lookup");
        return json!({
            "reasoningConfig": { "type": "enabled", "budgetTokens": tokens }
        });
    }
    let mode = if capabilities.adaptive {
        "adaptive"
    } else {
        "enabled"
    };
    json!({
        "reasoningConfig": {
            "type": mode,
            "maxReasoningEffort": effort.as_str(),
        }
    })
}

fn google_options(
    effort: ReasoningEffort,
    capabilities: EffortCapabilities,
    budget: EffortBudget,
) -> Value {
    if effort == ReasoningEffort::Off {
        return json!({
            "thinkingConfig": { "includeThoughts": false, "thinkingBudget": 0 }
        });
    }
    if capabilities.token_budget {
        let tokens = budget
            .tokens(effort, capabilities.max_budget_tokens)
            .expect("off effort returned before budget lookup");
        return json!({
            "thinkingConfig": { "includeThoughts": true, "thinkingBudget": tokens }
        });
    }
    json!({
        "thinkingConfig": {
            "includeThoughts": true,
            "thinkingLevel": google_effort(effort),
        }
    })
}

fn open_router_options(effort: ReasoningEffort) -> Value {
    json!({ "reasoning": { "effort": open_ai_effort(effort) } })
}

const fn open_ai_effort(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Off => "none",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh | ReasoningEffort::Max => "xhigh",
    }
}

const fn google_effort(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Off => "none",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High | ReasoningEffort::Xhigh | ReasoningEffort::Max => "high",
    }
}

fn object(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("effort mappings are JSON objects")
        .clone()
}

fn merge_objects(target: &mut Map<String, Value>, update: &Map<String, Value>) {
    for (key, value) in update {
        match (target.get_mut(key), value) {
            (Some(Value::Object(target_object)), Value::Object(update_object)) => {
                merge_objects(target_object, update_object);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_table_covers_every_level_and_provider_family() {
        let rows: [(ProviderFamily, [Value; 6]); 5] = [
            (
                ProviderFamily::OpenAi,
                [
                    json!({"reasoningEffort": "none"}),
                    json!({"reasoningEffort": "low"}),
                    json!({"reasoningEffort": "medium"}),
                    json!({"reasoningEffort": "high"}),
                    json!({"reasoningEffort": "xhigh"}),
                    json!({"reasoningEffort": "xhigh"}),
                ],
            ),
            (
                ProviderFamily::Anthropic,
                [
                    json!({"thinking": {"type": "disabled"}}),
                    json!({"thinking": {"type": "enabled", "budgetTokens": 1024}}),
                    json!({"thinking": {"type": "enabled", "budgetTokens": 4096}}),
                    json!({"thinking": {"type": "enabled", "budgetTokens": 16000}}),
                    json!({"thinking": {"type": "enabled", "budgetTokens": 24000}}),
                    json!({"thinking": {"type": "enabled", "budgetTokens": 31999}}),
                ],
            ),
            (
                ProviderFamily::Bedrock,
                [
                    json!({"reasoningConfig": {"type": "disabled"}}),
                    json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "low"}}),
                    json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "medium"}}),
                    json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "high"}}),
                    json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "xhigh"}}),
                    json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "max"}}),
                ],
            ),
            (
                ProviderFamily::Google,
                [
                    json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}}),
                    json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "low"}}),
                    json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "medium"}}),
                    json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}}),
                    json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}}),
                    json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}}),
                ],
            ),
            (
                ProviderFamily::OpenRouter,
                [
                    json!({"reasoning": {"effort": "none"}}),
                    json!({"reasoning": {"effort": "low"}}),
                    json!({"reasoning": {"effort": "medium"}}),
                    json!({"reasoning": {"effort": "high"}}),
                    json!({"reasoning": {"effort": "xhigh"}}),
                    json!({"reasoning": {"effort": "xhigh"}}),
                ],
            ),
        ];

        let variants = DeclaredVariants::new();
        let mut combinations = 0;
        for (family, expected) in rows {
            for (index, effort) in ReasoningEffort::ALL.into_iter().enumerate() {
                let resolution =
                    resolve_effort(family, effort, EffortCapabilities::default(), &variants);
                assert_eq!(resolution.source, ResolutionSource::GenericMapping);
                assert_eq!(Value::Object(resolution.options), expected[index]);
                combinations += 1;
            }
        }

        assert_eq!(ProviderFamily::ALL.len(), 5);
        assert_eq!(
            combinations,
            ReasoningEffort::ALL.len() * ProviderFamily::ALL.len()
        );
    }

    #[test]
    fn declared_variant_takes_precedence_over_generic_mapping() {
        let declared = json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "effort": "xhigh"
        });
        let variants =
            DeclaredVariants::new().with(ReasoningEffort::Xhigh, object(declared.clone()));

        let resolution = resolve_effort(
            ProviderFamily::Anthropic,
            ReasoningEffort::Xhigh,
            EffortCapabilities::default(),
            &variants,
        );

        assert_eq!(resolution.source, ResolutionSource::DeclaredVariant);
        assert_eq!(Value::Object(resolution.options), declared);
    }

    #[test]
    fn capabilities_select_adaptive_and_budget_shapes_without_model_names() {
        let no_variants = DeclaredVariants::new();
        let anthropic = resolve_effort(
            ProviderFamily::Anthropic,
            ReasoningEffort::Max,
            EffortCapabilities {
                adaptive: true,
                ..EffortCapabilities::default()
            },
            &no_variants,
        );
        assert_eq!(
            Value::Object(anthropic.options),
            json!({"thinking": {"type": "adaptive"}, "effort": "max"})
        );

        let google = resolve_effort(
            ProviderFamily::Google,
            ReasoningEffort::Xhigh,
            EffortCapabilities {
                token_budget: true,
                max_budget_tokens: Some(20_000),
                ..EffortCapabilities::default()
            },
            &no_variants,
        );
        assert_eq!(
            Value::Object(google.options),
            json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 20000}})
        );
    }

    #[test]
    fn switching_medium_to_xhigh_changes_only_the_effort_field() {
        let variants = DeclaredVariants::new();
        let base = json!({
            "model": "catalog-selected-model",
            "store": false,
            "input": "hello"
        });
        let mut medium = object(base.clone());
        resolve_effort(
            ProviderFamily::OpenAi,
            ReasoningEffort::Medium,
            EffortCapabilities::default(),
            &variants,
        )
        .apply_to(&mut medium);
        let mut xhigh = object(base);
        resolve_effort(
            ProviderFamily::OpenAi,
            ReasoningEffort::Xhigh,
            EffortCapabilities::default(),
            &variants,
        )
        .apply_to(&mut xhigh);

        let changed: Vec<&str> = medium
            .keys()
            .filter(|key| medium.get(*key) != xhigh.get(*key))
            .map(String::as_str)
            .collect();
        assert_eq!(changed, ["reasoningEffort"]);
        assert_eq!(medium["reasoningEffort"], "medium");
        assert_eq!(xhigh["reasoningEffort"], "xhigh");
    }

    #[test]
    fn policy_sources_contain_no_model_id_literals() {
        let sources = [include_str!("effort.rs"), include_str!("cache.rs")];
        let forbidden_prefixes = [
            ["g", "pt-"].concat(),
            ["cl", "aude-"].concat(),
            ["ge", "mini-"].concat(),
            ["gr", "ok-"].concat(),
            ["gl", "m-"].concat(),
            ["qw", "en-"].concat(),
        ];

        for source in sources {
            for prefix in &forbidden_prefixes {
                let quoted = format!("\"{prefix}");
                assert!(
                    !source.contains(&quoted),
                    "policy source contains a model-id literal beginning with {quoted}"
                );
            }
        }
    }

    #[test]
    fn invalid_effort_is_typed_and_actionable() {
        let error = "extreme".parse::<ReasoningEffort>().unwrap_err();
        assert_eq!(error.value, "extreme");
        assert!(
            error
                .to_string()
                .contains("off, low, medium, high, xhigh, max")
        );
    }
}
