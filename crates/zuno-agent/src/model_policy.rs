//! Which model an agent runs on, and how hard it thinks — by preset, never by a
//! built-in chain.
//!
//! # The inversion this module exists to adopt
//!
//! `oh-my-opencode-slim` defaults every agent's model to nothing at all
//! (`omo-slim`), and says why in the
//! comment above the table:
//!
//! > Default models for each agent.
//! > All set to undefined so agents follow the global/session model.
//! > Users can override per-agent via oh-my-opencode-slim.json agents.\<name\>.model.
//!
//! That is the whole design. [`ModelPolicy::resolve`] returns the session model for
//! every agent until something the *user* supplied says otherwise, so a fresh
//! install has one model to configure rather than six.
//!
//! # The anti-pattern, named so it stays refused
//!
//! Slim's parent ships the opposite: `AGENT_MODEL_REQUIREMENTS`
//! (`oh-my-openagent/dist/index.js:24467`) and `CATEGORY_MODEL_REQUIREMENTS`
//! (`:24652`) — per-agent and per-category `fallbackChain`s, each rung a concrete
//! model id plus up to ten provider ids that are entitled to serve it. Eight
//! categories, each with two to five rungs. Two costs, both certain:
//!
//! 1. **It rots on every model release.** Each rung encodes a model that exists
//!    today and a list of providers that carry it today. A model rename is a code
//!    change; a new provider is a code change; a retired model is a silent
//!    fallthrough to something the user did not choose.
//! 2. **It cannot be corrected by configuration.** The chain is compiled in, so a
//!    user whose provider is not on a rung's list gets the *next* rung's model with
//!    no way to say "no, this one".
//!
//! So nothing here names a model. Preset *shape* and data enter through the canonical
//! [`zuno_config::schema::Config`] type. A test walks every non-test source file in
//! this crate and fails on a model-id-shaped token, which is what keeps the rule true
//! after this module is no longer the newest thing in the crate.
//!
//! # Categories survive only as a preset shorthand
//!
//! omo's eight categories are a genuinely useful idea buried in a hardcoded table:
//! a caller that knows a task is *cheap and mechanical* should not have to know
//! which model that means. Here a category is a key in the active preset
//! ([`ModelPreset::with_category`]) and nothing else. There is no built-in category
//! list: two presets may declare different categories, or none, and
//! [`ModelPolicy::resolve_category`] answers from whichever preset is active. An
//! unknown category is a diagnostic and the session model, not an error.
//!
//! # Effort goes through todo 31's canonical levels, and defers when the model knows better
//!
//! A preset entry's `variant` is read by [`read_variant`]: when it spells one of
//! [`ReasoningEffort`]'s six canonical levels it resolves through
//! [`zuno_llm::effort::resolve_effort`], so this module adds no second effort policy.
//! When it does not, it is a name only the model's own catalog entry can explain,
//! and [`resolve_variant`] hands back the catalog's exact option object without
//! synthesising one.
//!
//! That deferral is the *shape* of omo's thinking-budget special case for one
//! vendor's older models (`oh-my-openagent/dist/index.js:28822-28829`): it returns a
//! 32,000-token budget for them and **deliberately returns `{}`** for that vendor's
//! newer models so their native variants take over. The shape is worth having; its
//! implementation is not — omo picks the branch by matching the model name against a
//! hand-written `is…OrLaterModel` predicate, so every release needs a new predicate,
//! and the two predicates guarding that one branch are themselves named after four
//! model families. Here the same branch is taken on a catalog fact: the model
//! declares the variant, or it does not.

use std::collections::BTreeMap;
use std::fmt;

use zuno_config::schema::agent::AgentConfig;
use zuno_config::schema::ordered::OrderedMap;
use zuno_config::schema::preset::PresetModelConfig;
use zuno_config::schema::{Config, JsonMap};
use zuno_llm::catalog::Catalog;
use zuno_llm::effort::{
    DeclaredVariants, EffortCapabilities, EffortResolution, ProviderFamily, ReasoningEffort,
};

/// A model and, optionally, the variant to run it at.
///
/// The only thing a preset entry ever names. `model` is in the oracle's
/// `provider/model` form (`zuno_llm::catalog::resolved::ResolvedModel::qualified_id`);
/// `variant` is either a canonical [`ReasoningEffort`] or a name the model's catalog
/// entry declares — [`read_variant`] decides which, and neither this type nor its
/// callers need to know in advance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// The model, in `provider/model` form.
    pub model: String,
    /// The variant to run it at, if the preset names one.
    pub variant: Option<String>,
}

impl ModelChoice {
    /// A choice naming a model and no variant.
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            variant: None,
        }
    }

    /// The same choice, at `variant`.
    #[must_use]
    pub fn with_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = Some(variant.into());
        self
    }

    /// The provider half of `provider/model`, when the id is qualified.
    ///
    /// [`None`] means the id cannot be looked up in a resolved catalog, because a
    /// bare model id does not say which provider serves it. That is reported as
    /// [`Diagnostic::ModelNotQualified`] rather than guessed at: picking the first
    /// provider that happens to carry the id is exactly the entitlement guessing
    /// `CATEGORY_MODEL_REQUIREMENTS` hardcodes.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.split().map(|(provider, _)| provider)
    }

    /// The model half of `provider/model`, when the id is qualified.
    #[must_use]
    pub fn model_id(&self) -> Option<&str> {
        self.split().map(|(_, model)| model)
    }

    fn split(&self) -> Option<(&str, &str)> {
        let (provider, model) = self.model.split_once('/')?;
        (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
    }
}

impl fmt::Display for ModelChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.variant {
            Some(variant) => write!(f, "{} ({variant})", self.model),
            None => f.write_str(&self.model),
        }
    }
}

/// One named preset: a flat agent-to-choice map, plus optional category shorthands.
///
/// Flat is the shape slim's installer emits (`src/cli/providers.ts:11-56`, five
/// presets keyed by agent name) and it is the right one: a preset is a *complete
/// answer* for a set of agents, so switching presets is one config edit rather than
/// six. Nothing about a preset is validated against the roster here — a preset may
/// name an agent this build does not have, and [`ModelPolicy::resolve`] simply never
/// asks about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelPreset {
    name: String,
    agents: BTreeMap<String, ModelChoice>,
    categories: BTreeMap<String, ModelChoice>,
}

impl ModelPreset {
    /// An empty preset called `name`.
    ///
    /// Empty is meaningful, not a placeholder: every agent falls through to the
    /// session model, which is the [`Default`] behaviour this module inherits from
    /// slim's all-`undefined` table.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Route `agent` to `choice`.
    #[must_use]
    pub fn with_agent(mut self, agent: impl Into<String>, choice: ModelChoice) -> Self {
        self.agents.insert(agent.into(), choice);
        self
    }

    /// Route the `category` shorthand to `choice`.
    #[must_use]
    pub fn with_category(mut self, category: impl Into<String>, choice: ModelChoice) -> Self {
        self.categories.insert(category.into(), choice);
        self
    }

    /// The preset's name, as the user selects it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The choice for `agent`, if this preset has an opinion about it.
    #[must_use]
    pub fn agent(&self, agent: &str) -> Option<&ModelChoice> {
        self.agents.get(agent)
    }

    /// The choice for the `category` shorthand, if this preset declares it.
    #[must_use]
    pub fn category(&self, category: &str) -> Option<&ModelChoice> {
        self.categories.get(category)
    }

    /// Agent names this preset routes, sorted.
    #[must_use]
    pub fn agents(&self) -> Vec<&str> {
        self.agents.keys().map(String::as_str).collect()
    }

    /// Category shorthands this preset declares, sorted.
    ///
    /// Whatever the preset says, and nothing else. There is no built-in list to
    /// compare against, which is the difference between a shorthand and
    /// `CATEGORY_MODEL_REQUIREMENTS`.
    #[must_use]
    pub fn categories(&self) -> Vec<&str> {
        self.categories.keys().map(String::as_str).collect()
    }
}

/// Every preset the user has, and the one they selected.
///
/// [`Default`] is the empty library with nothing selected — which resolves every
/// agent to the session model. A preset-less install is the normal case, not a
/// degraded one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresetLibrary {
    presets: BTreeMap<String, ModelPreset>,
    selected: Option<String>,
}

impl PresetLibrary {
    /// A library with no presets and nothing selected.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `preset`, replacing any preset of the same name.
    #[must_use]
    pub fn with_preset(mut self, preset: ModelPreset) -> Self {
        self.presets.insert(preset.name.clone(), preset);
        self
    }

    /// Select `name`.
    ///
    /// A name that is not in the library is accepted here and reported as
    /// [`Diagnostic::UnknownPreset`] at resolution time. Rejecting it at selection
    /// would mean a stale `preset` key in a config file stops the program from
    /// starting, and slim — which hit this in practice — chose the same way:
    /// "Missing preset → warning, continue with empty preset"
    /// (`src/config/codemap.md:201`), with `src/index.ts:216-218` clearing the stale
    /// name rather than failing.
    #[must_use]
    pub fn select(mut self, name: impl Into<String>) -> Self {
        self.selected = Some(name.into());
        self
    }

    /// The selected preset's name, whether or not it exists.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The selected preset, when the name resolves to one.
    #[must_use]
    pub fn active(&self) -> Option<&ModelPreset> {
        self.presets.get(self.selected.as_deref()?)
    }

    /// One preset by name.
    #[must_use]
    pub fn preset(&self, name: &str) -> Option<&ModelPreset> {
        self.presets.get(name)
    }

    /// Every preset name, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.presets.keys().map(String::as_str).collect()
    }

    /// Whether the library holds no presets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.presets.is_empty()
    }

    /// Build the runtime library from the one canonical configuration schema.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let mut library = PresetLibrary::new();
        for (name, configured) in config.presets.iter().flat_map(|presets| presets.iter()) {
            let mut preset = ModelPreset::named(name);
            for (agent, choice) in &configured.agents {
                preset = preset.with_agent(agent, configured_choice(choice));
            }
            for (category, choice) in &configured.categories {
                preset = preset.with_category(category, configured_choice(choice));
            }
            library = library.with_preset(preset);
        }
        match &config.preset {
            Some(selected) => library.select(selected),
            None => library,
        }
    }
}

fn configured_choice(config: &PresetModelConfig) -> ModelChoice {
    let mut choice = ModelChoice::new(config.model());
    if let Some(reasoning) = config.reasoning() {
        choice = choice.with_variant(reasoning.as_str());
    }
    choice
}

/// Which rung of the ladder produced a model.
///
/// The ladder is short on purpose — three rungs, one test each. `small_model`
/// routing for the engine's internal agents is *not* a fourth rung: the engine
/// already carries it (`zuno-engine/src/compaction.rs:404`), and duplicating it here
/// would give two places to disagree about which model titles a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSource {
    /// Nothing usable named a model, so the session or global model stands.
    ///
    /// The default, and the destination of every fallthrough.
    SessionModel,
    /// The active preset's entry for this agent.
    Preset {
        /// The preset that answered.
        preset: String,
    },
    /// The active preset's entry for a category shorthand.
    Category {
        /// The preset that answered.
        preset: String,
        /// The shorthand the caller asked for.
        category: String,
    },
    /// A per-agent override in the user's config.
    AgentOverride,
}

impl fmt::Display for ModelSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionModel => f.write_str("the session model"),
            Self::Preset { preset } => write!(f, "preset `{preset}`"),
            Self::Category { preset, category } => {
                write!(f, "category `{category}` in preset `{preset}`")
            }
            Self::AgentOverride => f.write_str("a per-agent config override"),
        }
    }
}

/// Something the caller should be told, having already been given an answer.
///
/// Every variant is emitted *alongside* a usable resolution. That is the load-bearing
/// property: the plan's failure scenario is a preset naming a model the user cannot
/// reach, and the required behaviour is the session model plus a diagnostic — because
/// the alternative, refusing to start, punishes the user for a preset file they may
/// not have written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnostic {
    /// The selected preset is not in the library.
    UnknownPreset {
        /// The name that was selected.
        selected: String,
        /// The presets that do exist.
        available: Vec<String>,
    },
    /// The active preset declares no such category.
    UnknownCategory {
        /// The shorthand that was asked for.
        category: String,
        /// The preset that was asked.
        preset: String,
        /// The shorthands it does declare.
        available: Vec<String>,
    },
    /// A named model is not in the resolved catalog.
    ModelUnavailable {
        /// The model that was named.
        model: String,
        /// Who named it.
        source: ModelSource,
    },
    /// A named model is not in `provider/model` form, so it cannot be looked up.
    ModelNotQualified {
        /// The model that was named.
        model: String,
        /// Who named it.
        source: ModelSource,
    },
    /// A variant is neither a canonical effort level nor declared by the model.
    UnknownVariant {
        /// The variant that was asked for.
        variant: String,
        /// The variant names the model declares.
        declared: Vec<String>,
    },
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPreset {
                selected,
                available,
            } => write!(
                f,
                "preset `{selected}` is not defined{}; every agent falls through to \
                 the session model",
                list(available, ", available presets: ")
            ),
            Self::UnknownCategory {
                category,
                preset,
                available,
            } => write!(
                f,
                "preset `{preset}` declares no category `{category}`{}; falling \
                 through to the session model",
                list(available, ", it declares: ")
            ),
            Self::ModelUnavailable { model, source } => write!(
                f,
                "`{model}` from {source} is not in the resolved catalog; falling \
                 through to the session model"
            ),
            Self::ModelNotQualified { model, source } => write!(
                f,
                "`{model}` from {source} is not in `provider/model` form, so no \
                 provider can be checked; falling through to the session model"
            ),
            Self::UnknownVariant { variant, declared } => write!(
                f,
                "variant `{variant}` is neither a reasoning effort level \
                 ({levels}) nor declared by the model{declared}; no effort options \
                 were set",
                levels = ReasoningEffort::ALL.map(ReasoningEffort::as_str).join(", "),
                declared = list(declared, ", which declares: "),
            ),
        }
    }
}

fn list(values: &[String], prefix: &str) -> String {
    if values.is_empty() {
        return String::new();
    }
    format!("{prefix}{}", values.join(", "))
}

/// One agent's answer: the model, why, and what the caller should be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The agent, or the category shorthand, this answers for.
    pub subject: String,
    /// The model to use.
    ///
    /// [`None`] only when there is no session model either — an unconfigured
    /// install, where the caller has nothing to run and must say so.
    pub model: Option<ModelChoice>,
    /// Which rung answered.
    pub source: ModelSource,
    /// What was skipped on the way down, in the order it was skipped.
    pub diagnostics: Vec<Diagnostic>,
}

impl Resolution {
    /// Whether the answer is the session model rather than a chosen one.
    #[must_use]
    pub fn inherits_session_model(&self) -> bool {
        self.source == ModelSource::SessionModel
    }

    /// The diagnostics, rendered one per line, for a log or a `doctor` command.
    #[must_use]
    pub fn render_diagnostics(&self) -> Vec<String> {
        self.diagnostics
            .iter()
            .map(|diagnostic| format!("{}: {diagnostic}", self.subject))
            .collect()
    }
}

/// Whether a model can actually be reached.
///
/// A trait rather than a `&Catalog` parameter for two reasons. Resolution runs before
/// a catalog exists in some call paths (`agent list` on a machine with no
/// credentials), and a test proving the fallthrough should not have to build a
/// models.dev document to do it. [`AnyModel`] and [`NoModel`] cover both ends.
pub trait ModelAvailability {
    /// Whether `model`, in `provider/model` form, resolved.
    fn is_available(&self, model: &ModelChoice) -> bool;
}

/// Availability is not being checked.
///
/// For callers with no resolved catalog. Every named model is taken at face value,
/// which is the correct answer to "is this reachable" when the question cannot be
/// asked — the provider will say so at request time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnyModel;

impl ModelAvailability for AnyModel {
    fn is_available(&self, _model: &ModelChoice) -> bool {
        true
    }
}

/// Nothing is available.
///
/// Exists for the test that proves every rung falls through, and for a caller that
/// wants the session model unconditionally.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoModel;

impl ModelAvailability for NoModel {
    fn is_available(&self, _model: &ModelChoice) -> bool {
        false
    }
}

/// The resolved catalog is the real answer.
///
/// Todo 26's resolution has already applied config overrides, availability, and the
/// blacklist/whitelist filters in the oracle's order, so a model present here is one
/// `opencode models` would print. A bare model id is *not* available: see
/// [`ModelChoice::provider`].
impl ModelAvailability for Catalog {
    fn is_available(&self, model: &ModelChoice) -> bool {
        match model.split() {
            Some((provider, id)) => self.model(provider, id).is_some(),
            None => false,
        }
    }
}

/// Resolves an agent's model from overrides, the active preset, and the session.
///
/// Precedence, highest first:
///
/// 1. a per-agent override in the user's config ([`Self::with_agent_overrides`]),
/// 2. the active preset's entry for that agent,
/// 3. the session or global model.
///
/// A rung whose model is unavailable is skipped with a [`Diagnostic`] and the next
/// rung is tried. The session model is the floor and is never checked for
/// availability — there is nothing below it, and it is the selection the user is
/// already running on, so second-guessing it here would replace a working session
/// with `None`.
#[derive(Debug, Clone, Default)]
pub struct ModelPolicy<'a> {
    library: Option<&'a PresetLibrary>,
    overrides: BTreeMap<String, ModelChoice>,
    session: Option<ModelChoice>,
}

impl<'a> ModelPolicy<'a> {
    /// A policy with no presets, no overrides, and no session model.
    ///
    /// Resolves every agent to [`ModelSource::SessionModel`] with `model: None`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve against `library`'s active preset.
    #[must_use]
    pub fn with_library(mut self, library: &'a PresetLibrary) -> Self {
        self.library = Some(library);
        self
    }

    /// The model the session is running on.
    #[must_use]
    pub fn with_session_model(mut self, session: ModelChoice) -> Self {
        self.session = Some(session);
        self
    }

    /// Override one agent.
    #[must_use]
    pub fn with_agent_override(mut self, agent: impl Into<String>, choice: ModelChoice) -> Self {
        self.overrides.insert(agent.into(), choice);
        self
    }

    /// Take overrides from the user's `agents` config block.
    ///
    /// `agents.<name>.model`, `.reasoning`, and `.variant` already exist in the schema.
    /// This reads the same keys so a user who has configured a
    /// model for one agent does not have to learn a second mechanism to keep it.
    /// An entry with no `model` is not an override — a `variant`-only entry has no
    /// model to attach to and is left to the rung that supplies one.
    #[must_use]
    pub fn with_agent_overrides(mut self, agents: &OrderedMap<AgentConfig>) -> Self {
        for (name, config) in agents.iter() {
            if let Some(model) = &config.model {
                let mut choice = ModelChoice::new(model.clone());
                choice.variant = config
                    .reasoning
                    .map(|effort| effort.as_str().to_owned())
                    .or_else(|| config.variant.clone());
                self.overrides.insert(name.to_owned(), choice);
            }
        }
        self
    }

    /// The model for `agent`.
    pub fn resolve(&self, agent: &str, availability: &impl ModelAvailability) -> Resolution {
        let mut diagnostics = Vec::new();
        let candidates = [
            self.overrides
                .get(agent)
                .map(|choice| (choice.clone(), ModelSource::AgentOverride)),
            self.active_preset(&mut diagnostics).and_then(|preset| {
                preset.agent(agent).map(|choice| {
                    (
                        choice.clone(),
                        ModelSource::Preset {
                            preset: preset.name().to_owned(),
                        },
                    )
                })
            }),
        ];
        self.first_available(agent, candidates, diagnostics, availability)
    }

    /// The model for a `category` shorthand.
    ///
    /// Answers from the active preset's category map and from nowhere else. There is
    /// no built-in table to consult, so a category means whatever the selected preset
    /// says it means — and an unselected or silent preset means the session model.
    pub fn resolve_category(
        &self,
        category: &str,
        availability: &impl ModelAvailability,
    ) -> Resolution {
        let mut diagnostics = Vec::new();
        let candidate = self.active_preset(&mut diagnostics).and_then(|preset| {
            let Some(choice) = preset.category(category) else {
                diagnostics.push(Diagnostic::UnknownCategory {
                    category: category.to_owned(),
                    preset: preset.name().to_owned(),
                    available: preset.categories().into_iter().map(str::to_owned).collect(),
                });
                return None;
            };
            Some((
                choice.clone(),
                ModelSource::Category {
                    preset: preset.name().to_owned(),
                    category: category.to_owned(),
                },
            ))
        });
        self.first_available(category, [candidate], diagnostics, availability)
    }

    /// Every agent in the roster for these capabilities, resolved.
    ///
    /// Iterates [`crate::builtin::roster`] rather than a list of names, so an agent
    /// added or capability-gated there is resolved here with no edit.
    pub fn resolve_roster(
        &self,
        vision_available: bool,
        availability: &impl ModelAvailability,
    ) -> Vec<Resolution> {
        crate::builtin::roster(vision_available)
            .into_iter()
            .map(|agent| self.resolve(agent.name, availability))
            .collect()
    }

    fn active_preset(&self, diagnostics: &mut Vec<Diagnostic>) -> Option<&'a ModelPreset> {
        let library = self.library?;
        let selected = library.selected()?;
        let active = library.active();
        if active.is_none() {
            diagnostics.push(Diagnostic::UnknownPreset {
                selected: selected.to_owned(),
                available: library.names().into_iter().map(str::to_owned).collect(),
            });
        }
        active
    }

    fn first_available<const N: usize>(
        &self,
        subject: &str,
        candidates: [Option<(ModelChoice, ModelSource)>; N],
        mut diagnostics: Vec<Diagnostic>,
        availability: &impl ModelAvailability,
    ) -> Resolution {
        for (choice, source) in candidates.into_iter().flatten() {
            if choice.split().is_none() {
                diagnostics.push(Diagnostic::ModelNotQualified {
                    model: choice.model,
                    source,
                });
                continue;
            }
            if !availability.is_available(&choice) {
                diagnostics.push(Diagnostic::ModelUnavailable {
                    model: choice.model,
                    source,
                });
                continue;
            }
            return Resolution {
                subject: subject.to_owned(),
                model: Some(choice),
                source,
                diagnostics,
            };
        }

        Resolution {
            subject: subject.to_owned(),
            model: self.session.clone(),
            source: ModelSource::SessionModel,
            diagnostics,
        }
    }
}

/// What a preset's `variant` string turned out to mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Variant {
    /// One of [`ReasoningEffort`]'s six canonical levels.
    Effort(ReasoningEffort),
    /// A name only the model's own catalog entry can explain.
    Named(String),
}

/// Read a `variant` string.
///
/// Canonical first: `off`, `low`, `medium`, `high`, `xhigh`, `max` are todo 31's
/// levels and resolve through its table for every provider family. Anything else is
/// a model-declared name — slim's presets contain one (`thinking`,
/// `src/cli/providers.ts:48`), and inventing a mapping for it here would be a second
/// effort policy that disagrees with the first.
#[must_use]
pub fn read_variant(raw: &str) -> Variant {
    raw.parse::<ReasoningEffort>()
        .map_or_else(|_| Variant::Named(raw.to_owned()), Variant::Effort)
}

/// Provider options for a resolved agent's variant.
#[derive(Debug, Clone, PartialEq)]
pub enum EffortOutcome {
    /// The preset named no variant, so whatever the session already asked for stands.
    Inherit,
    /// A canonical effort level, resolved through todo 31.
    Options(EffortResolution),
    /// The model declares this variant; its own option object applies unchanged.
    ///
    /// This is the branch omo takes by matching a model name
    /// (`dist/index.js:28824`, returning `{}` "so native variants take over"). Here
    /// the option object comes from the catalog, so nothing is synthesised and no
    /// predicate needs updating when a model ships.
    ModelVariant {
        /// The variant the model declares.
        variant: String,
        /// The catalog's exact options for it.
        options: JsonMap,
    },
}

/// Lift a model's catalog-declared variants into todo 31's effort-keyed form.
///
/// `ResolvedModel::variants` is keyed by *name*; [`DeclaredVariants`] is keyed by
/// canonical effort. A variant whose name is a canonical level is therefore that
/// level's exact option object, and wins over the generic provider-family mapping
/// inside [`zuno_llm::effort::resolve_effort`]. Names that are not levels are left
/// alone — they reach a caller through [`EffortOutcome::ModelVariant`] instead.
#[must_use]
pub fn declared_variants(model_variants: &BTreeMap<String, JsonMap>) -> DeclaredVariants {
    let mut declared = DeclaredVariants::new();
    for (name, options) in model_variants {
        if let Variant::Effort(effort) = read_variant(name) {
            declared = declared.with(effort, options.clone());
        }
    }
    declared
}

/// Resolve a resolved agent's variant into provider options.
///
/// `model_variants` is the model's `ResolvedModel::variants`; `capabilities` and
/// `family` are the same catalog facts every provider adapter already passes to
/// [`zuno_llm::effort::resolve_effort`]. A variant that is neither a canonical level
/// nor declared by the model yields [`EffortOutcome::Inherit`] and a
/// [`Diagnostic::UnknownVariant`] — the same "diagnose, do not refuse" rule the model
/// ladder follows.
#[must_use]
pub fn resolve_variant(
    variant: Option<&str>,
    family: ProviderFamily,
    capabilities: EffortCapabilities,
    model_variants: &BTreeMap<String, JsonMap>,
) -> (EffortOutcome, Option<Diagnostic>) {
    let Some(raw) = variant else {
        return (EffortOutcome::Inherit, None);
    };

    match read_variant(raw) {
        Variant::Effort(effort) => {
            let declared = declared_variants(model_variants);
            let resolution =
                zuno_llm::effort::resolve_effort(family, effort, capabilities, &declared);
            (EffortOutcome::Options(resolution), None)
        }
        Variant::Named(name) => match model_variants.get(&name) {
            Some(options) => (
                EffortOutcome::ModelVariant {
                    variant: name,
                    options: options.clone(),
                },
                None,
            ),
            None => (
                EffortOutcome::Inherit,
                Some(Diagnostic::UnknownVariant {
                    variant: name,
                    declared: model_variants.keys().cloned().collect(),
                }),
            ),
        },
    }
}

/// Whether `raw` is spelled like a model id.
///
/// The guard for this crate's central promise, shared by the roster's prose scan
/// (`builtin::tests`) and this module's source scan, so there is one definition of
/// what a model id looks like rather than two that drift.
///
/// Test-only on purpose: preset *data* is exactly where model ids belong, so no
/// shipping code has a use for this. The family needles are assembled from split
/// literals for the same reason `zuno-llm/src/effort.rs:578-585` does it — a scanner
/// that spells its own needles in full flags itself.
#[cfg(test)]
pub(crate) fn looks_like_model_id(raw: &str) -> bool {
    let token = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '-');
    let lower = token.to_ascii_lowercase();

    let families = [
        ["cl", "aude"].concat(),
        ["gp", "t-"].concat(),
        ["gp", "t4"].concat(),
        ["gem", "ini"].concat(),
        ["son", "net"].concat(),
        ["op", "us"].concat(),
        ["ha", "iku"].concat(),
        ["ki", "mi"].concat(),
        ["gl", "m-"].concat(),
        ["gr", "ok"].concat(),
        ["lla", "ma"].concat(),
        ["mist", "ral"].concat(),
        ["qw", "en"].concat(),
        ["deep", "seek"].concat(),
        ["code", "stral"].concat(),
    ];
    if families.iter().any(|family| lower.contains(family)) {
        return true;
    }
    for reasoning in ["o1", "o3", "o4"] {
        if lower.starts_with(reasoning) && lower.len() > reasoning.len() {
            return true;
        }
    }

    // A `provider/model` pair. Two shapes have to be excluded, both of which appear in
    // this crate's own prose:
    //
    // * a digit on the right-hand side is what separates a model id from an ordinary
    //   source path — `src/auth.ts` and `utils/parser.ts` appear in the upstream
    //   prompts this crate carries, and neither is a model id;
    // * a `:` makes it a *citation*, `dist/index.js:24475`, which is how every
    //   reference in this crate is written. No provider spells a model with a colon,
    //   so excluding it costs no coverage — and the family check above runs first, so
    //   a real id remains caught however it is punctuated.
    let mut halves = lower.split('/');
    let (Some(left), Some(right), None) = (halves.next(), halves.next(), halves.next()) else {
        return false;
    };
    !left.is_empty()
        && !right.is_empty()
        && !lower.starts_with('/')
        && !lower.starts_with('.')
        && !lower.contains(':')
        && right.contains(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests;
