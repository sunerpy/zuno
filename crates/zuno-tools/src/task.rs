//! `task` — delegating a bounded unit of work to a child session.
//!
//! # The five refusals are the feature
//!
//! Everything interesting about a delegation tool happens before a child session
//! exists. A caller can name no target, two targets, a coordinator, a target it is
//! not permitted to reach, or reach for one more hop than the recursion bound
//! allows. Each of those is refused here with a message that **names the fix**,
//! because a delegation refusal is read by a model, not a human, and a model can
//! only act on a message that says what to send instead. `oh-my-opencode-slim` pays
//! for the absence of that property with an entire hook family —
//! `.omo/refs/omo-slim/src/hooks/delegate-task-retry/patterns.ts` maps nine error
//! substrings onto nine `fixHint` strings after the fact, because the tool's own
//! errors did not carry them.
//!
//! # Why there is no `load_skills`
//!
//! Two of those nine patterns are `run_in_background` and `load_skills` — arguments
//! the model *forgot*, whose fix hints are literally "add `load_skills=[]` (empty
//! array when no skill is needed)". An argument whose most common value is "the
//! empty one that means nothing" and whose omission needs a recovery hook is an
//! argument that should not exist. Skills reach a child through its agent's
//! permission set instead ([`zuno_agent::builtin::GOVERNED_TOOL_IDS`] includes
//! `skill`), which is a property of the target rather than of the call, so there is
//! nothing for a caller to remember. Passing `load_skills` anyway is
//! [`zuno_error::ToolError::InvalidArgs`] naming per-agent permissions — **not**
//! silently ignored, because a caller that believes it loaded a skill and did not
//! would then blame the child for ignoring it.
//!
//! `background` survives that same argument only because its default genuinely
//! carries information: foreground is a blocking wait the caller must opt out of.
//!
//! # Depth is measured from two places, and the bound is the larger
//!
//! Upstream walks the session's `parentID` chain (`packages/opencode/src/tool/
//! task.ts:106-117`) and compares it against `subagent_depth`. That measure cannot
//! see tool composition: a `task` call nested inside [`crate::batch`]'s `execute`
//! is a fresh turn-level call as far as session ancestry is concerned.
//! [`zuno_tool::ToolContext::depth`] is the opposite — it counts composition and is
//! `0` for every turn-level call, including one made *inside* a child session. Each
//! measure is blind to the other's recursion, so [`DelegationLimits`] is checked
//! against the maximum of the two.
//!
//! # Two ids, because they answer different questions
//!
//! A foreground delegation returns the child session id. A background dispatch
//! returns that **and** a distinct background id. Upstream sets `jobId:
//! nextSession.id` (`task.ts:279`), which makes "cancel this job" and "resume this
//! session" the same string — so a client holding one cannot tell which it has, and
//! cancelling a job that has already been promoted to a session silently means
//! something else. [`background_id`] derives a prefixed id instead and
//! [`TaskTool`] refuses a host that hands back the session id as its job id.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zuno_agent::builtin::{Agent, delegable};
use zuno_agent::model_policy::{
    Diagnostic, EffortOutcome, ModelAvailability, ModelChoice, ModelPolicy, PresetLibrary,
    Resolution, resolve_variant,
};
use zuno_error::ToolError;
use zuno_llm::effort::{EffortCapabilities, ProviderFamily, ReasoningEffort};
use zuno_tool::{PermissionAsk, ToolContext, ToolOutput, TypedTool};

/// The id the model calls, and the registry slot it fills
/// ([`crate::registry::BuiltinSlot::Task`]).
pub const WIRE_ID: &str = "task";

/// The permission key this tool gates on.
///
/// The *pattern* is the requested `subagent_type`, matching upstream
/// (`task.ts:118-127`), so a rule may permit delegation to one agent and refuse
/// another rather than treating delegation as one all-or-nothing capability.
pub const PERMISSION_KEY: &str = "task";

/// The delegation hop budget when config declares no `subagent_depth`.
///
/// Upstream's `cfg.subagent_depth ?? 1` (`task.ts:112`). One hop: the user's session
/// may delegate, and the child may not.
pub const DEFAULT_SUBAGENT_DEPTH: u32 = 1;

/// The agent a `category` call runs on.
///
/// A category names a `{model, variant}` and says nothing about *conduct*, so it
/// cannot select a specialist. omo resolves this the same way — a category forces a
/// generic executor (`oh-my-openagent/dist/index.js:136191-136258`) — and in this
/// roster the bounded generic executor is [`zuno_agent::builtin::WORKER`].
pub const GENERIC_EXECUTOR: &str = "worker";

/// The one agent a caller may never name.
///
/// Not a special case bolted on here: it is the single [`zuno_agent::builtin::Role`]
/// that is not `Subagent`, so [`delegable`] already excludes it and this constant
/// exists only to render the refusal.
pub const COORDINATOR: &str = "build";

/// The description the model reads.
///
/// Deliberately free of model ids: `model` and `effort` are pass-throughs to
/// whatever the caller's catalog resolved, and naming a model here would bake
/// today's market into the binary — the failure `zuno-agent`'s
/// [`zuno_agent::model_policy`] exists to refuse.
pub const DESCRIPTION: &str = include_str!("description/task.txt");

/// How a background child reports its terminal state.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReportDelivery {
    /// Add the report to the parent's next step and wake it.
    #[default]
    NextStep,
    /// Persist the result without adding parent input.
    Quiet,
}

/// Arguments for one delegation.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskParams {
    /// A short (3-5 words) description of the task.
    #[serde(default)]
    pub description: Option<String>,
    /// The task for the agent to perform.
    pub prompt: String,
    /// The specific agent to delegate to. Mutually exclusive with `category`.
    #[serde(default)]
    pub subagent_type: Option<String>,
    /// A preset shorthand naming the model tier to run the generic worker at.
    /// Mutually exclusive with `subagent_type`.
    #[serde(default)]
    pub category: Option<String>,
    /// Override the model for this child only, as `provider/model`.
    #[serde(default)]
    pub model: Option<String>,
    /// Override the reasoning effort for this child only.
    #[serde(default)]
    pub effort: Option<String>,
    /// Run asynchronously and report a job id immediately. Defaults to foreground.
    #[serde(default)]
    pub background: Option<bool>,
    /// What to do with the terminal report of a background dispatch.
    #[serde(default, rename = "reportDelivery")]
    pub report_delivery: Option<ReportDelivery>,
    /// Continue a previous delegation's session instead of creating a new one.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Accepted only so its removal can be explained; see the module docs.
    ///
    /// Hidden from the advertised schema, so no caller learns the name from this
    /// tool — a model that sends it learned it from another harness.
    #[serde(default)]
    #[schemars(skip)]
    pub load_skills: Option<Value>,
}

/// The recursion bound, and the reason it cannot be waived here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegationLimits {
    /// How many delegation hops may separate a session from the user's.
    pub subagent_depth: u32,
}

impl Default for DelegationLimits {
    fn default() -> Self {
        Self {
            subagent_depth: DEFAULT_SUBAGENT_DEPTH,
        }
    }
}

/// Catalog facts about one resolved model, as the layer that resolved it sees them.
///
/// Enough to answer "can this provider honour that effort" and nothing more. The
/// fields are the exact inputs [`zuno_llm::effort::resolve_effort`] already takes, so
/// this tool adds no second effort policy — it only asks the question earlier, while
/// a refusal can still be reported to the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelFacts {
    /// Which family's request shape applies.
    pub family: ProviderFamily,
    /// Whether the model produces reasoning at all.
    pub reasoning: bool,
    /// Reasoning-control shape and budget ceiling.
    pub effort: EffortCapabilities,
    /// The model's own declared variants, keyed by name.
    pub variants: BTreeMap<String, Map<String, Value>>,
}

/// The resolved catalog, narrowed to what a delegation decision needs.
///
/// A trait rather than a `&Catalog`, for the same reason
/// [`zuno_agent::model_policy::ModelAvailability`] is one: a test proving an effort is
/// refused should not have to build a models.dev document, and this tool runs on
/// paths where a catalog may not exist yet.
pub trait ProviderFacts: Send + Sync + 'static {
    /// Facts for `model`, or [`None`] when the catalog cannot reach it.
    ///
    /// [`None`] is also the availability answer, so a model this tool would send to a
    /// provider and a model [`ModelPolicy`] considers reachable are the same set —
    /// two sources of truth here would let a delegation name a model the parent
    /// session had already ruled out.
    fn facts(&self, model: &ModelChoice) -> Option<ModelFacts>;
}

/// Nothing resolves. Every named model falls through with a diagnostic.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProviders;

impl ProviderFacts for NoProviders {
    fn facts(&self, _model: &ModelChoice) -> Option<ModelFacts> {
        None
    }
}

struct FactsAvailability<'a>(&'a dyn ProviderFacts);

impl ModelAvailability for FactsAvailability<'_> {
    fn is_available(&self, model: &ModelChoice) -> bool {
        self.0.facts(model).is_some()
    }
}

/// One delegation, as the session layer receives it.
///
/// `model` and `provider_options` are the resolved answer, not the caller's request:
/// the precedence ladder has already run, so a host has nothing left to decide and
/// cannot disagree with the parent about which model a child runs on.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildTurnRequest {
    /// The session delegating.
    pub parent_session_id: String,
    /// An existing child session to continue, from `task_id`.
    pub resume_session_id: Option<String>,
    /// The agent the child runs as.
    pub agent: String,
    /// The caller's short label, used for the session title.
    pub description: Option<String>,
    /// The task text.
    pub prompt: String,
    /// The model the child runs on, or [`None`] to inherit the session's.
    pub model: Option<ModelChoice>,
    /// The canonical effort level, when one was resolved.
    pub effort: Option<ReasoningEffort>,
    /// Provider options to merge into the child's outbound request body.
    ///
    /// Produced by [`zuno_llm::effort::EffortResolution`] or by the model's own
    /// declared variant, so nothing is synthesised here.
    pub provider_options: Map<String, Value>,
    /// Whether the caller asked not to wait.
    pub background: bool,
    /// How a background child reports its terminal state.
    pub report_delivery: ReportDelivery,
}

/// What a dispatched delegation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildTurn {
    /// The child session, whether created or resumed.
    pub session_id: String,
    /// The job handle, for a background dispatch only. Never the session id.
    pub job_id: Option<String>,
    /// The child's final text, or the running-notice for a background dispatch.
    pub output: String,
}

/// Why a dispatch could not produce a child turn.
#[derive(Debug, thiserror::Error)]
pub enum ChildTurnError {
    /// The session layer refused or failed.
    #[error("{0}")]
    Host(String),
    /// `task_id` named a session that does not exist or is not a child of this one.
    #[error(
        "`task_id` `{0}` is not a resumable child of this session; drop `task_id` to \
         start a fresh delegation"
    )]
    UnknownSession(String),
}

/// The session-layer effects a delegation needs and this crate cannot perform.
///
/// Creating a session, recording its parent, and driving a turn all belong to layers
/// above this one, and `zuno-engine` exposes no seam a tool can hold: `run_turn` wants
/// a `&mut Connection`, a provider registry, and the very dispatcher that is calling
/// this tool. So the contract is stated here and satisfied there, exactly as
/// [`crate::plan_exit::PlanExitHost`] does for session messages.
#[async_trait]
pub trait ChildTurnHost: Send + Sync + 'static {
    /// How many delegation hops already separate `session_id` from the user's.
    ///
    /// `0` for a session the user is talking to directly. Walks the parent chain
    /// `zuno-db`'s session store records (todo 21), which is the only place that
    /// relationship exists.
    async fn delegation_depth(&self, session_id: &str) -> Result<u32, ChildTurnError>;

    /// Create or resume the child session and drive its turn.
    async fn dispatch(&self, request: ChildTurnRequest) -> Result<ChildTurn, ChildTurnError>;
}

/// A refusal, phrased so the caller can act on it without a recovery hook.
#[derive(Debug, thiserror::Error)]
pub enum TaskRejection {
    #[error(
        "Must provide either `category` or `subagent_type`. Add \
         `subagent_type=\"{first}\"` naming one of the valid targets ({targets}), or \
         `category=\"<preset shorthand>\"` to run the `{GENERIC_EXECUTOR}` agent at \
         that preset's model."
    )]
    NoTarget {
        targets: String,
        first: &'static str,
    },

    #[error(
        "`category` and `subagent_type` are mutually exclusive; you sent \
         `category=\"{category}\"` and `subagent_type=\"{subagent_type}\"`. Provide \
         only one: keep `subagent_type=\"{subagent_type}\"` to choose the agent, or \
         keep `category=\"{category}\"` to run the `{GENERIC_EXECUTOR}` agent at that \
         preset's model."
    )]
    BothTargets {
        subagent_type: String,
        category: String,
    },

    #[error(
        "`{COORDINATOR}` coordinates delegations and cannot be a delegation target — \
         targeting it would reopen the unbounded recursion the roster closes. Set \
         `subagent_type` to one of the valid targets: {targets}."
    )]
    CoordinatorTarget { targets: String },

    #[error(
        "Unknown agent `{requested}`. Set `subagent_type` to one of the valid \
         targets: {targets} — or, if `{requested}` is a preset shorthand, send it as \
         `category=\"{requested}\"` instead."
    )]
    UnknownTarget { requested: String, targets: String },

    #[error(
        "Subagent depth limit reached: this session is already {depth} \
         delegation hop(s) deep and `subagent_depth` is {limit}. Do this work in the \
         current session, or raise `subagent_depth` in config to allow nested \
         subagents."
    )]
    DepthExceeded { depth: u32, limit: u32 },

    #[error(
        "`load_skills` is not a parameter of `{WIRE_ID}`. Remove it: skills are \
         permission-gated per agent, so choose the `subagent_type` whose permissions \
         already grant the skill you wanted instead of naming skills in the call."
    )]
    LoadSkillsRemoved,
    #[error(
        "`reportDelivery` requires `background: true`. Remove `reportDelivery` for a \
         foreground delegation, or add `background: true` to receive the result later."
    )]
    ReportDeliveryRequiresBackground,
}

/// The guidance a `task` refusal from the permission layer carries.
///
/// [`zuno_error::ToolError::Denied`] has no source to hang prose on — by design, since
/// a denial needs a grant rather than a better call. So the fix travels on the
/// [`PermissionAsk`] instead, under [`GUIDANCE_KEY`], where it reaches the human
/// deciding *and* is recoverable by a caller inspecting the ask.
#[must_use]
pub fn denial_guidance(subagent_type: &str, targets: &[String]) -> String {
    format!(
        "`{WIRE_ID}` is not permitted for `{subagent_type}`. Grant \
         `{PERMISSION_KEY}` for pattern `{subagent_type}`, or set `subagent_type` to \
         a target the current rules allow ({}).",
        targets.join(", ")
    )
}

/// The [`PermissionAsk::metadata`] key carrying [`denial_guidance`].
pub const GUIDANCE_KEY: &str = "guidance";

/// The valid `task` targets for these capabilities, in roster order.
///
/// Delegates to [`delegable`] rather than restating the roster, so an agent added,
/// removed, or capability-gated in `zuno-agent` changes this answer with no edit here.
#[must_use]
pub fn valid_targets(vision_available: bool) -> Vec<String> {
    delegable(vision_available)
        .into_iter()
        .map(|agent: Agent| agent.name.to_owned())
        .collect()
}

/// What the precedence ladder decided, plus everything the caller must be told.
#[derive(Debug, Clone, PartialEq)]
pub struct DelegationPlan {
    /// The agent the child runs as.
    pub agent: String,
    /// The preset shorthand that selected the model, when one did.
    pub category: Option<String>,
    /// The model the child runs on.
    pub model: Option<ModelChoice>,
    /// The canonical effort level, when one resolved.
    pub effort: Option<ReasoningEffort>,
    /// Provider options for the child's outbound request.
    pub provider_options: Map<String, Value>,
    /// Everything skipped, unavailable, or unhonourable, in the order found.
    pub notes: Vec<String>,
}

/// Delegation to a child session, gated on depth and permission.
pub struct TaskTool {
    host: Arc<dyn ChildTurnHost>,
    facts: Arc<dyn ProviderFacts>,
    presets: PresetLibrary,
    session_model: Option<ModelChoice>,
    agent_overrides: BTreeMap<String, ModelChoice>,
    limits: DelegationLimits,
    vision_available: bool,
}

impl TaskTool {
    /// A tool with the default depth bound, no presets, and no session model.
    #[must_use]
    pub fn new(host: Arc<dyn ChildTurnHost>, facts: Arc<dyn ProviderFacts>) -> Self {
        Self {
            host,
            facts,
            presets: PresetLibrary::new(),
            session_model: None,
            agent_overrides: BTreeMap::new(),
            limits: DelegationLimits::default(),
            vision_available: false,
        }
    }

    /// Resolve child models against `presets`.
    #[must_use]
    pub fn with_presets(mut self, presets: PresetLibrary) -> Self {
        self.presets = presets;
        self
    }

    /// The model the parent session is running on — the ladder's floor.
    #[must_use]
    pub fn with_session_model(mut self, model: ModelChoice) -> Self {
        self.session_model = Some(model);
        self
    }

    /// A per-agent override from the user's config, as todo 64's rung 1.
    #[must_use]
    pub fn with_agent_override(mut self, agent: impl Into<String>, model: ModelChoice) -> Self {
        self.agent_overrides.insert(agent.into(), model);
        self
    }

    /// The hop budget, from `subagent_depth`.
    #[must_use]
    pub const fn with_limits(mut self, limits: DelegationLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Whether the catalog has a vision-capable model, which gates one target.
    #[must_use]
    pub const fn with_vision_available(mut self, available: bool) -> Self {
        self.vision_available = available;
        self
    }

    /// Resolve the child's model and effort.
    ///
    /// Four rungs, highest first. The top one is new; the lower three are
    /// [`ModelPolicy`]'s, called rather than reimplemented:
    ///
    /// 1. **this call's `model` / `effort` arguments** — the caller is choosing for
    ///    one child, which is more specific than anything a config file said,
    /// 2. a per-agent config override,
    /// 3. the active preset's entry for the agent, or for the `category` shorthand,
    /// 4. the parent session's model.
    ///
    /// Rung 1 is skip-on-unavailable like the rest: an unreachable or unqualified
    /// model becomes a note and the ladder continues, because refusing the whole
    /// delegation over a model name would lose work the caller has already framed.
    /// What is *not* allowed is silence — every skip is in [`DelegationPlan::notes`]
    /// and reaches the caller in the rendered output.
    #[must_use]
    pub fn plan(&self, agent: &str, category: Option<&str>, params: &TaskParams) -> DelegationPlan {
        let availability = FactsAvailability(self.facts.as_ref());
        let mut policy = ModelPolicy::new().with_library(&self.presets);
        if let Some(session) = &self.session_model {
            policy = policy.with_session_model(session.clone());
        }
        for (name, choice) in &self.agent_overrides {
            policy = policy.with_agent_override(name.clone(), choice.clone());
        }

        let lower: Resolution = match category {
            Some(shorthand) => policy.resolve_category(shorthand, &availability),
            None => policy.resolve(agent, &availability),
        };
        let mut notes = lower.render_diagnostics();

        let model = self
            .call_model(params.model.as_deref(), &availability, &mut notes)
            .or(lower.model);
        let variant = params
            .effort
            .clone()
            .or_else(|| model.as_ref().and_then(|choice| choice.variant.clone()));

        let (effort, provider_options) = self.honour(model.as_ref(), variant, &mut notes);

        DelegationPlan {
            agent: agent.to_owned(),
            category: category.map(str::to_owned),
            model,
            effort,
            provider_options,
            notes,
        }
    }

    fn call_model(
        &self,
        requested: Option<&str>,
        availability: &FactsAvailability<'_>,
        notes: &mut Vec<String>,
    ) -> Option<ModelChoice> {
        let choice = ModelChoice::new(requested?);
        if choice.provider().is_none() {
            notes.push(format!(
                "`{}` from the `model` argument is not in `provider/model` form, so no \
                 provider can be checked; the agent's configured model applies instead",
                choice.model
            ));
            return None;
        }
        if !availability.is_available(&choice) {
            notes.push(format!(
                "`{}` from the `model` argument is not in the resolved catalog; the \
                 agent's configured model applies instead",
                choice.model
            ));
            return None;
        }
        Some(choice)
    }

    /// Turn a requested variant into provider options, or say why it cannot be.
    ///
    /// Three ways an effort fails to reach a provider, each a note rather than a
    /// silent downgrade: the model is not resolvable, the model produces no
    /// reasoning at all, or the name is neither a canonical level nor one the model
    /// declares. The last case is [`resolve_variant`]'s own answer — todo 64 already
    /// decided that shape, so this only reports it.
    fn honour(
        &self,
        model: Option<&ModelChoice>,
        variant: Option<String>,
        notes: &mut Vec<String>,
    ) -> (Option<ReasoningEffort>, Map<String, Value>) {
        let Some(requested) = variant else {
            return (None, Map::new());
        };
        let Some(model) = model else {
            notes.push(format!(
                "effort `{requested}` was not applied: no model resolved for this \
                 delegation, so no provider could be asked whether it honours one"
            ));
            return (None, Map::new());
        };
        let Some(facts) = self.facts.facts(model) else {
            notes.push(format!(
                "effort `{requested}` was not applied: `{}` is not in the resolved \
                 catalog, so its reasoning support is unknown",
                model.model
            ));
            return (None, Map::new());
        };

        let (outcome, diagnostic) = resolve_variant(
            Some(&requested),
            facts.family,
            facts.effort,
            &facts.variants,
        );
        if let Some(Diagnostic::UnknownVariant { variant, declared }) = diagnostic {
            notes.push(format!(
                "effort `{variant}` is neither a canonical reasoning level nor \
                 declared by `{model}`{}; the child runs at the model's default",
                if declared.is_empty() {
                    String::new()
                } else {
                    format!(", which declares: {}", declared.join(", "))
                },
            ));
        }

        match outcome {
            EffortOutcome::Inherit => (None, Map::new()),
            EffortOutcome::ModelVariant { options, .. } => (None, options),
            EffortOutcome::Options(resolution) => {
                if !facts.reasoning && resolution.effort != ReasoningEffort::Off {
                    notes.push(format!(
                        "effort `{requested}` was not applied: `{}` produces no \
                         reasoning, so it cannot honour a reasoning effort; drop \
                         `effort` or delegate to an agent whose model reasons",
                        model.model
                    ));
                    return (None, Map::new());
                }
                (Some(resolution.effort), resolution.options)
            }
        }
    }

    fn target(&self, params: &TaskParams) -> Result<(String, Option<String>), ToolError> {
        let targets = valid_targets(self.vision_available);
        let rendered = targets.join(", ");
        match (params.subagent_type.as_deref(), params.category.as_deref()) {
            (None, None) => Err(reject(TaskRejection::NoTarget {
                targets: rendered,
                first: GENERIC_EXECUTOR,
            })),
            (Some(subagent_type), Some(category)) => Err(reject(TaskRejection::BothTargets {
                subagent_type: subagent_type.to_owned(),
                category: category.to_owned(),
            })),
            (Some(requested), None) => {
                if requested == COORDINATOR {
                    return Err(reject(TaskRejection::CoordinatorTarget {
                        targets: rendered,
                    }));
                }
                if !targets.iter().any(|name| name == requested) {
                    return Err(reject(TaskRejection::UnknownTarget {
                        requested: requested.to_owned(),
                        targets: rendered,
                    }));
                }
                Ok((requested.to_owned(), None))
            }
            (None, Some(category)) => Ok((GENERIC_EXECUTOR.to_owned(), Some(category.to_owned()))),
        }
    }

    async fn guard_depth(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        let ancestry = self
            .host
            .delegation_depth(&ctx.session_id)
            .await
            .map_err(host_failure)?;
        let depth = ancestry.max(ctx.depth);
        if depth >= self.limits.subagent_depth {
            return Err(unrecoverable(TaskRejection::DepthExceeded {
                depth,
                limit: self.limits.subagent_depth,
            }));
        }
        Ok(())
    }
}

#[async_trait]
impl TypedTool for TaskTool {
    type Params = TaskParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(&self, params: TaskParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if params.load_skills.is_some() {
            return Err(reject(TaskRejection::LoadSkillsRemoved));
        }
        let background = params.background.unwrap_or(false);
        if !background && params.report_delivery.is_some() {
            return Err(reject(TaskRejection::ReportDeliveryRequiresBackground));
        }
        let report_delivery = params.report_delivery.unwrap_or_default();

        // Argument validity precedes the human prompt, unlike upstream, which asks
        // before checking the agent exists (`task.ts:118-183`). Asking a user to
        // approve delegation to a target that cannot exist spends the one interaction
        // budget this tool has on a call that is going to fail anyway.
        let (agent, category) = self.target(&params)?;
        self.guard_depth(&ctx).await?;

        let targets = valid_targets(self.vision_available);
        let mut metadata = Map::new();
        if let Some(description) = &params.description {
            metadata.insert("description".to_owned(), Value::String(description.clone()));
        }
        metadata.insert("subagent_type".to_owned(), Value::String(agent.clone()));
        metadata.insert(
            GUIDANCE_KEY.to_owned(),
            Value::String(denial_guidance(&agent, &targets)),
        );
        ctx.ask(
            WIRE_ID,
            PermissionAsk {
                permission: PERMISSION_KEY.to_owned(),
                patterns: vec![agent.clone()],
                metadata,
                always: vec!["*".to_owned()],
            },
        )
        .await?;

        let plan = self.plan(&agent, category.as_deref(), &params);
        let turn = self
            .host
            .dispatch(ChildTurnRequest {
                parent_session_id: ctx.session_id.clone(),
                resume_session_id: params.task_id.clone(),
                agent,
                description: params.description.clone(),
                prompt: params.prompt.clone(),
                model: plan.model.clone(),
                effort: plan.effort,
                provider_options: plan.provider_options.clone(),
                background,
                report_delivery,
            })
            .await
            .map_err(host_failure)?;

        if background {
            let job = turn.job_id.as_deref().ok_or_else(|| {
                host_failure(ChildTurnError::Host(
                    "a background dispatch must report a job id distinct from the child \
                     session id"
                        .to_owned(),
                ))
            })?;
            if job == turn.session_id {
                return Err(host_failure(ChildTurnError::Host(format!(
                    "background job id `{job}` is the child session id; the two must be \
                     distinguishable"
                ))));
            }
        }

        Ok(render(&params, &plan, &turn, background))
    }
}

fn reject(rejection: TaskRejection) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(rejection),
    }
}

/// A refusal whose fix is not in the arguments, so the model must not retry it.
///
/// [`ToolError::InvalidArgs`] advertises `is_model_correctable`, and a model that
/// believes it can correct a depth limit will reissue the identical call and be
/// refused again. [`ToolError::Failed`] is the only remaining variant that both
/// carries a message and reports `Recovery::Fail` — [`ToolError::Denied`] carries no
/// source at all, which is why the permission path needs [`denial_guidance`].
fn unrecoverable(rejection: TaskRejection) -> ToolError {
    ToolError::Failed {
        tool: WIRE_ID.to_owned(),
        source: Box::new(rejection),
    }
}

fn host_failure(error: ChildTurnError) -> ToolError {
    ToolError::Failed {
        tool: WIRE_ID.to_owned(),
        source: Box::new(error),
    }
}

/// Upstream's `renderOutput` (`task.ts:64-78`), plus the background id and the
/// resolution notes.
///
/// The notes are inside the envelope rather than appended after it because a caller
/// that reads only the result body would otherwise never learn its `effort` was
/// dropped — which is precisely the silent downgrade this tool must not perform.
fn render(
    params: &TaskParams,
    plan: &DelegationPlan,
    turn: &ChildTurn,
    background: bool,
) -> ToolOutput {
    let state = if background { "running" } else { "completed" };
    let mut lines = vec![match turn.job_id.as_deref() {
        Some(job) => format!(
            "<task id=\"{}\" job=\"{job}\" state=\"{state}\" reportDelivery=\"{}\">",
            turn.session_id,
            match params.report_delivery.unwrap_or_default() {
                ReportDelivery::NextStep => "nextStep",
                ReportDelivery::Quiet => "quiet",
            }
        ),
        None => format!("<task id=\"{}\" state=\"{state}\">", turn.session_id),
    }];
    if let Some(description) = &params.description {
        lines.push(format!("<summary>{description}</summary>"));
    }
    for note in &plan.notes {
        lines.push(format!("<note>{note}</note>"));
    }
    lines.push("<task_result>".to_owned());
    lines.push(turn.output.clone());
    lines.push("</task_result>".to_owned());
    lines.push("</task>".to_owned());

    let title = params
        .description
        .clone()
        .unwrap_or_else(|| format!("Delegated to {}", plan.agent));
    ToolOutput::text(title, lines.join("\n"))
}

/// Catalog facts stated by hand, for a test or an unconfigured install.
///
/// Public for the same reason [`crate::plan_exit::RecordingHost`] is: the seam it
/// stands in for lives above this crate, so a caller integrating that seam needs
/// something to hold while doing it, and every assertion about effort honouring has
/// to be able to declare a non-reasoning model without a models.dev document.
#[derive(Debug, Clone, Default)]
pub struct FixedFacts {
    known: BTreeMap<String, ModelFacts>,
}

impl FixedFacts {
    /// No models at all.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `model` with these facts.
    #[must_use]
    pub fn with(mut self, model: impl Into<String>, facts: ModelFacts) -> Self {
        self.known.insert(model.into(), facts);
        self
    }

    /// Declare a reasoning model of `family` with no variants of its own.
    #[must_use]
    pub fn with_reasoning(self, model: impl Into<String>, family: ProviderFamily) -> Self {
        self.with(
            model,
            ModelFacts {
                family,
                reasoning: true,
                effort: EffortCapabilities::default(),
                variants: BTreeMap::new(),
            },
        )
    }

    /// Declare a model that produces no reasoning, so no effort can reach it.
    #[must_use]
    pub fn without_reasoning(self, model: impl Into<String>, family: ProviderFamily) -> Self {
        self.with(
            model,
            ModelFacts {
                family,
                reasoning: false,
                effort: EffortCapabilities::default(),
                variants: BTreeMap::new(),
            },
        )
    }
}

impl FixedFacts {
    /// Whether the declared model reasons, or [`None`] when it is not declared here.
    ///
    /// Keyed on the same `provider/model` string [`ProviderFacts::facts`] resolves, so a
    /// surface asking this and a delegation asking that cannot disagree about a model.
    #[must_use]
    pub fn reasons(&self, model: &str) -> Option<bool> {
        self.known.get(model).map(|facts| facts.reasoning)
    }
}

impl ProviderFacts for FixedFacts {
    fn facts(&self, model: &ModelChoice) -> Option<ModelFacts> {
        self.known.get(&model.model).cloned()
    }
}

/// A host that records what it was asked to dispatch and answers from a script.
///
/// The recorded [`ChildTurnRequest`] is the assertion point for "an explicit model
/// and effort reached the child's outbound request": `provider_options` is the exact
/// map a provider adapter merges into its body, so a test can merge it into a base
/// body and read back what the child would have sent.
#[derive(Debug, Default)]
pub struct RecordingHost {
    ancestry: u32,
    reuse_session_id_as_job: bool,
    next_job: AtomicU64,
    dispatched: std::sync::Mutex<Vec<ChildTurnRequest>>,
}

impl RecordingHost {
    /// A host whose sessions are all at delegation depth zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Report `depth` delegation hops for every session.
    #[must_use]
    pub const fn at_depth(mut self, depth: u32) -> Self {
        self.ancestry = depth;
        self
    }

    /// Misbehave exactly as upstream does, returning the session id as the job id.
    ///
    /// Exists so the refusal of that shape is a tested property rather than a claim.
    #[must_use]
    pub const fn conflating_ids(mut self) -> Self {
        self.reuse_session_id_as_job = true;
        self
    }

    /// Every dispatch this host received, in order.
    #[must_use]
    pub fn dispatched(&self) -> Vec<ChildTurnRequest> {
        self.dispatched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ChildTurnHost for RecordingHost {
    async fn delegation_depth(&self, _session_id: &str) -> Result<u32, ChildTurnError> {
        Ok(self.ancestry)
    }

    async fn dispatch(&self, request: ChildTurnRequest) -> Result<ChildTurn, ChildTurnError> {
        let background = request.background;
        let session_id = request
            .resume_session_id
            .clone()
            .unwrap_or_else(|| format!("ses_child_of_{}", request.parent_session_id));
        self.dispatched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        let job = if self.reuse_session_id_as_job {
            session_id.clone()
        } else {
            format!(
                "job_{:06}",
                self.next_job.fetch_add(1, Ordering::Relaxed) + 1
            )
        };
        Ok(ChildTurn {
            job_id: background.then_some(job),
            output: "done".to_owned(),
            session_id,
        })
    }
}

#[cfg(test)]
mod tests;
