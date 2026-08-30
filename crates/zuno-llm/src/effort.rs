//! Provider-neutral reasoning effort and provider-specific request options.
//!
//! A session chooses one [`ReasoningEffort`]. The provider adapters do not each
//! reinterpret that choice: they call [`resolve_effort`] and merge the returned
//! options into their outbound body. A model catalog may declare an exact option
//! object for a level; that object wins over every generic rule here. This is how
//! newer models can use native adaptive reasoning while older models in the same
//! provider family continue to use token budgets, without model-name policy in
//! the binary.

use crate::registry::ApiSurface;
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
    /// Lower these options to `surface`'s wire vocabulary and merge them into an
    /// outbound JSON body.
    ///
    /// The merge is recursive so that a nested wire placement — Gemini's
    /// `generationConfig.thinkingConfig` — lands beside the sampling fields the
    /// provider already wrote there rather than replacing the whole object.
    ///
    /// `surface` is required rather than inferred because one option name reaches
    /// the wire under two different names depending on it: see [`lower_to_wire`].
    ///
    /// Provider adapters should start from the same base body for each request.
    /// Reusing a body previously decorated for another effort can retain fields
    /// that do not exist in the new variant.
    pub fn apply_to(&self, body: &mut Map<String, Value>, surface: ApiSurface) {
        merge_objects(body, &lower_to_wire(&self.options, surface));
    }
}

/// Translate SDK provider-option names into the field names `surface` reads.
///
/// # Why this exists at all
///
/// [`resolve_effort`] deals in *SDK provider-option* names, not wire names, and so
/// does every model catalog that declares [`DeclaredVariants`]. That is the same
/// vocabulary the oracle's `provider-options` layer uses, and the oracle lowers it
/// in each protocol writer just before serialising:
///
/// - `packages/llm/src/protocols/openai-chat.ts:335,340` reads `reasoningEffort`
///   and writes `reasoning_effort`.
/// - `packages/llm/src/protocols/openai-responses.ts:459,472` reads the same
///   option and writes `reasoning: { effort }` — **the same name, a different
///   wire shape, chosen by the surface.** `reasoningSummary` joins that object as
///   `summary`; Chat Completions has no equivalent field, so it is omitted there.
/// - `packages/llm/src/protocols/anthropic-messages.ts:494-503` (`lowerThinking`)
///   accepts `budgetTokens` or `budget_tokens` and always writes `budget_tokens`.
/// - `packages/llm/src/protocols/gemini.ts:292-330` reads `thinkingConfig` from
///   provider options and writes it *inside* `generationConfig`.
///
/// A name that must become two different things cannot be resolved where the
/// surface is unknown, and a name that arrives verbatim from a catalog cannot be
/// fixed at construction time. So the translation belongs here, at the moment an
/// option map becomes body keys, and both callers — this module's
/// [`EffortResolution::apply_to`] and
/// [`CompletionRequest::apply_parameters`](crate::registry::CompletionRequest::apply_parameters)
/// — go through it.
///
/// # What is deliberately left alone
///
/// Names that are *already* wire names pass through untouched, because they are
/// not SDK options that happen to look wrong:
///
/// - OpenRouter's `reasoning.effort` is OpenRouter's documented request field.
/// - Bedrock's `reasoningConfig` / `maxReasoningEffort` is Amazon Nova 2's
///   documented `additionalModelRequestFields` shape, which is genuinely
///   camelCase.
///
/// The function is idempotent: applying it to a map already in wire vocabulary
/// changes nothing, because every rule keys off the SDK spelling.
#[must_use]
pub fn lower_to_wire(options: &Map<String, Value>, surface: ApiSurface) -> Map<String, Value> {
    let mut wire = Map::new();
    for (name, value) in options {
        match name.as_str() {
            OPEN_AI_EFFORT_OPTION => lower_open_ai_effort(&mut wire, value.clone(), surface),
            OPEN_AI_SUMMARY_OPTION => lower_open_ai_summary(&mut wire, value.clone(), surface),
            ANTHROPIC_THINKING_OPTION => {
                merge_objects(&mut wire, &object_entry(name, lower_thinking(value)));
            }
            GOOGLE_THINKING_OPTION => {
                merge_objects(
                    &mut wire,
                    &object_entry(
                        GOOGLE_GENERATION_CONFIG_FIELD,
                        json!({ GOOGLE_THINKING_OPTION: value.clone() }),
                    ),
                );
            }
            _ => {
                wire.insert(name.clone(), value.clone());
            }
        }
    }
    wire
}

// `_OPTION` names are SDK provider-option spellings; `_FIELD` names are wire
// spellings. `GOOGLE_THINKING_OPTION` is deliberately both.
const OPEN_AI_EFFORT_OPTION: &str = "reasoningEffort";
const OPEN_AI_SUMMARY_OPTION: &str = "reasoningSummary";
const OPEN_AI_CHAT_EFFORT_FIELD: &str = "reasoning_effort";
const OPEN_AI_RESPONSES_REASONING_FIELD: &str = "reasoning";
const OPEN_AI_RESPONSES_EFFORT_FIELD: &str = "effort";
const OPEN_AI_RESPONSES_SUMMARY_FIELD: &str = "summary";
const ANTHROPIC_THINKING_OPTION: &str = "thinking";
const ANTHROPIC_BUDGET_OPTION: &str = "budgetTokens";
const ANTHROPIC_BUDGET_FIELD: &str = "budget_tokens";
const GOOGLE_THINKING_OPTION: &str = "thinkingConfig";
const GOOGLE_GENERATION_CONFIG_FIELD: &str = "generationConfig";

fn lower_open_ai_effort(wire: &mut Map<String, Value>, effort: Value, surface: ApiSurface) {
    match surface {
        ApiSurface::Responses => merge_objects(
            wire,
            &object_entry(
                OPEN_AI_RESPONSES_REASONING_FIELD,
                json!({ OPEN_AI_RESPONSES_EFFORT_FIELD: effort }),
            ),
        ),
        ApiSurface::Default | ApiSurface::Chat | ApiSurface::Messages => {
            wire.insert(OPEN_AI_CHAT_EFFORT_FIELD.to_owned(), effort);
        }
    }
}

fn lower_open_ai_summary(wire: &mut Map<String, Value>, summary: Value, surface: ApiSurface) {
    if surface == ApiSurface::Responses {
        merge_objects(
            wire,
            &object_entry(
                OPEN_AI_RESPONSES_REASONING_FIELD,
                json!({ OPEN_AI_RESPONSES_SUMMARY_FIELD: summary }),
            ),
        );
    }
}

/// Rewriting field by field rather than replacing the object keeps `type`, and
/// every thinking field added later, instead of silently dropping it.
fn lower_thinking(thinking: &Value) -> Value {
    let Some(fields) = thinking.as_object() else {
        return thinking.clone();
    };
    let mut lowered = Map::new();
    for (name, value) in fields {
        if name == ANTHROPIC_BUDGET_OPTION {
            lowered.insert(ANTHROPIC_BUDGET_FIELD.to_owned(), value.clone());
        } else {
            lowered.insert(name.clone(), value.clone());
        }
    }
    Value::Object(lowered)
}

fn object_entry(name: &str, value: Value) -> Map<String, Value> {
    let mut entry = Map::new();
    entry.insert(name.to_owned(), value);
    entry
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

    /// Every rename the wire demands, per family and per surface.
    ///
    /// Four rules, each citing the oracle protocol writer that performs it. The
    /// exhaustive walk in [`lowering_renames_every_sdk_option_the_wire_spells_differently`]
    /// asserts that a combination absent from this table survives *byte-identically*,
    /// so a family added later whose SDK option name differs from its wire field
    /// fails that test until it is listed here. That is the property the old
    /// SDK-vocabulary-only table did not have.
    fn wire_renames(
        family: ProviderFamily,
        surface: ApiSurface,
    ) -> &'static [(&'static [&'static str], &'static [&'static str])] {
        match (family, surface) {
            // `openai-responses.ts:459,472`
            (ProviderFamily::OpenAi, ApiSurface::Responses) => {
                &[(&["reasoningEffort"], &["reasoning", "effort"])]
            }
            // `openai-chat.ts:335,340`
            (ProviderFamily::OpenAi, _) => &[(&["reasoningEffort"], &["reasoning_effort"])],
            // `anthropic-messages.ts:494-503` (`lowerThinking`)
            (ProviderFamily::Anthropic, _) => &[(
                &["thinking", "budgetTokens"],
                &["thinking", "budget_tokens"],
            )],
            // `gemini.ts:292-330`
            (ProviderFamily::Google, _) => {
                &[(&["thinkingConfig"], &["generationConfig", "thinkingConfig"])]
            }
            // OpenRouter's `reasoning.effort` and Bedrock's `reasoningConfig` are
            // already the documented request fields.
            (ProviderFamily::Bedrock | ProviderFamily::OpenRouter, _) => &[],
        }
    }

    /// Move `from` to `to`, creating intermediate objects. A generic path mover
    /// driven by [`wire_renames`], deliberately *not* a second implementation of
    /// [`lower_to_wire`]: it cannot rename a key the table does not name.
    fn move_path(root: &mut Map<String, Value>, from: &[&str], to: &[&str]) {
        let Some(value) = remove_path(root, from) else {
            return;
        };
        let (last, parents) = to.split_last().expect("a destination path has a leaf");
        let mut cursor = root;
        for step in parents {
            cursor = cursor
                .entry((*step).to_owned())
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("intermediate path steps are objects");
        }
        cursor.insert((*last).to_owned(), value);
    }

    fn remove_path(root: &mut Map<String, Value>, path: &[&str]) -> Option<Value> {
        let (last, parents) = path.split_last()?;
        let mut cursor = root;
        for step in parents {
            cursor = cursor.get_mut(*step)?.as_object_mut()?;
        }
        cursor.remove(*last)
    }

    /// The lowered body must equal the resolved options with exactly the declared
    /// renames applied, for every family, level and surface.
    ///
    /// This is the assertion the suite was missing. `resolve_effort` speaks SDK
    /// provider-option names, so a table that only checks *its* output — which is
    /// all [`effort_table_covers_every_level_and_provider_family`] did, and still
    /// does — passes while `reasoningEffort` goes out on a wire that reads
    /// `reasoning_effort`. Both halves are production functions here; only the
    /// four-rule rename table is written by hand.
    #[test]
    fn lowering_renames_every_sdk_option_the_wire_spells_differently() {
        let variants = DeclaredVariants::new();
        let surfaces = [
            ApiSurface::Default,
            ApiSurface::Chat,
            ApiSurface::Responses,
            ApiSurface::Messages,
        ];
        let mut combinations = 0;
        for family in ProviderFamily::ALL {
            for effort in ReasoningEffort::ALL {
                let sdk = resolve_effort(family, effort, EffortCapabilities::default(), &variants)
                    .options;
                for surface in surfaces {
                    let mut expected = sdk.clone();
                    for (from, to) in wire_renames(family, surface) {
                        move_path(&mut expected, from, to);
                    }
                    let wire = lower_to_wire(&sdk, surface);
                    assert_eq!(
                        wire, expected,
                        "{family:?} at {effort} on {surface:?} did not lower to its wire fields"
                    );
                    assert_eq!(
                        lower_to_wire(&wire, surface),
                        wire,
                        "{family:?} at {effort} on {surface:?} is not idempotent, so a body \
                         already in wire vocabulary would be rewritten again"
                    );
                    combinations += 1;
                }
            }
        }
        assert_eq!(
            combinations,
            ProviderFamily::ALL.len() * ReasoningEffort::ALL.len() * surfaces.len()
        );
    }

    /// No lowered body may still carry an SDK option name.
    ///
    /// A belt-and-braces check independent of the rename table: it names the three
    /// spellings the wire never accepts and asserts they are gone whatever route
    /// produced the body. If a future family reintroduces one, this fails even if
    /// its rename row was added incorrectly.
    #[test]
    fn no_lowered_body_carries_an_sdk_option_name() {
        let variants = DeclaredVariants::new();
        for family in ProviderFamily::ALL {
            for effort in ReasoningEffort::ALL {
                for surface in [
                    ApiSurface::Default,
                    ApiSurface::Chat,
                    ApiSurface::Responses,
                    ApiSurface::Messages,
                ] {
                    let resolution =
                        resolve_effort(family, effort, EffortCapabilities::default(), &variants);
                    let mut body = Map::new();
                    resolution.apply_to(&mut body, surface);
                    let serialised = Value::Object(body.clone()).to_string();
                    for option in [OPEN_AI_EFFORT_OPTION, ANTHROPIC_BUDGET_OPTION] {
                        assert!(
                            !serialised.contains(&format!("\"{option}\"")),
                            "{family:?} at {effort} on {surface:?} shipped the SDK option \
                             `{option}` to the wire: {serialised}"
                        );
                    }
                    // `thinkingConfig` is a real Gemini field, but only nested
                    // inside `generationConfig`; at the top level it is the
                    // unlowered SDK option.
                    assert!(
                        !body.contains_key(GOOGLE_THINKING_OPTION),
                        "{family:?} at {effort} on {surface:?} left `{GOOGLE_THINKING_OPTION}` at \
                         the body root instead of inside `{GOOGLE_GENERATION_CONFIG_FIELD}`: \
                         {serialised}"
                    );
                }
            }
        }
    }

    /// The resolver's own output vocabulary, which is SDK provider options.
    ///
    /// Deliberately left asserting the camelCase SDK names: that *is* what
    /// `resolve_effort` contracts to return, and a catalog's declared variants are
    /// written in the same vocabulary. What changed is that this is no longer the
    /// only table — see
    /// [`lowering_renames_every_sdk_option_the_wire_spells_differently`], which
    /// pins what actually reaches a body. On its own this test passed happily while
    /// the wire was wrong.
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

    /// Changing the level must move exactly one wire field, on either surface.
    ///
    /// The assertion changed from the SDK name `reasoningEffort` to the two wire
    /// fields it becomes, and gained the Responses half. The original could only
    /// ever observe the pre-lowering name, which is why it stayed green while the
    /// level shipped under a field no endpoint reads.
    #[test]
    fn switching_medium_to_xhigh_changes_only_the_effort_field() {
        let variants = DeclaredVariants::new();
        let base = json!({
            "model": "catalog-selected-model",
            "store": false,
            "input": "hello"
        });
        let decorated = |effort, surface| {
            let mut body = object(base.clone());
            resolve_effort(
                ProviderFamily::OpenAi,
                effort,
                EffortCapabilities::default(),
                &variants,
            )
            .apply_to(&mut body, surface);
            body
        };

        for (surface, field, medium_value, xhigh_value) in [
            (
                ApiSurface::Chat,
                "reasoning_effort",
                json!("medium"),
                json!("xhigh"),
            ),
            (
                ApiSurface::Responses,
                "reasoning",
                json!({"effort": "medium"}),
                json!({"effort": "xhigh"}),
            ),
        ] {
            let medium = decorated(ReasoningEffort::Medium, surface);
            let xhigh = decorated(ReasoningEffort::Xhigh, surface);
            let changed: Vec<&str> = medium
                .keys()
                .filter(|key| medium.get(*key) != xhigh.get(*key))
                .map(String::as_str)
                .collect();
            assert_eq!(changed, [field], "on {surface:?}");
            assert_eq!(medium[field], medium_value, "on {surface:?}");
            assert_eq!(xhigh[field], xhigh_value, "on {surface:?}");
            assert!(
                !medium.contains_key(OPEN_AI_EFFORT_OPTION),
                "the SDK option name must not survive onto the body: {medium:?}"
            );
        }
    }

    #[test]
    fn reasoning_summary_is_nested_only_on_the_responses_surface() {
        let options = object(json!({
            "reasoningEffort": "max",
            "reasoningSummary": "auto"
        }));

        assert_eq!(
            Value::Object(lower_to_wire(&options, ApiSurface::Responses)),
            json!({"reasoning": {"effort": "max", "summary": "auto"}})
        );
        assert_eq!(
            Value::Object(lower_to_wire(&options, ApiSurface::Chat)),
            json!({"reasoning_effort": "max"}),
            "Chat Completions has no reasoning-summary request field"
        );
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
