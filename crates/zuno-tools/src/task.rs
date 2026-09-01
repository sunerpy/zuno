//! `task` — delegating a bounded unit of work to a child session.
//!
//! # Refusals happen before a child exists
//!
//! Everything interesting about a delegation tool happens before a child session
//! exists. A caller can name a coordinator, an unknown or forbidden Agent, provide
//! an incomplete contract, or reach for one more hop than the recursion bound
//! allows. Each refusal names the concrete fix because it is read by a model, not a
//! human. The model-facing wire intentionally has one routing field, `agent`; model,
//! reasoning, category, Skill, and capability routing remain host-owned policy.
//!
//! The model supplies a typed [`DelegationContract`] rather than an ambiguous title
//! plus free-form prompt. Required outcome, instructions, and evidence fields are
//! validated before permission or dispatch. Scope, constraints, and dependencies stay
//! structured on the wire and are rendered exactly once into the child prompt.
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
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zuno_agent::builtin::{Agent, delegable};
use zuno_agent::model_policy::{
    Diagnostic, EffortOutcome, ModelAvailability, ModelChoice, ModelPolicy, PresetLibrary,
    Resolution, resolve_variant,
};
use zuno_error::ToolError;
use zuno_llm::effort::{EffortCapabilities, ProviderFamily, ReasoningEffort};
use zuno_orchestration::{AttemptSnapshot, sha256_json};
use zuno_tool::{
    InterruptHandle, PermissionAsk, ToolConcurrencyPolicy, ToolContext, ToolOutput, ToolUiIntent,
    TypedTool,
};

/// The id the model calls, and the registry slot it fills
/// ([`crate::registry::BuiltinSlot::Task`]).
pub const WIRE_ID: &str = "task";

/// The permission key this tool gates on.
///
/// The *pattern* is the requested `agent`, so a rule may permit delegation to one
/// Agent and refuse another rather than treating delegation as one all-or-nothing
/// capability.
pub const PERMISSION_KEY: &str = "task";

/// Durable metadata key for client-facing child-session identity and state.
pub const METADATA_SUBAGENT_KEY: &str = "subagent";

/// The delegation hop budget when config declares no `subagent_depth`.
///
/// Upstream's `cfg.subagent_depth ?? 1` (`task.ts:112`). One hop: the user's session
/// may delegate, and the child may not.
pub const DEFAULT_SUBAGENT_DEPTH: u32 = 1;

/// The generic executor used by workflow-owned category routing.
///
/// A category names a `{model, variant}` and says nothing about *conduct*, so it
/// cannot select a specialist. omo resolves this the same way — a category forces a
/// generic executor (`oh-my-openagent/dist/index.js:136191-136258`) — and in this
/// roster the bounded generic executor is [`zuno_agent::builtin::GENERAL`].
pub const GENERIC_EXECUTOR: &str = "general";

/// The one agent a caller may never name.
///
/// Not a special case bolted on here: it is the single [`zuno_agent::builtin::Role`]
/// that is not `Subagent`, so [`delegable`] already excludes it and this constant
/// exists only to render the refusal.
pub const COORDINATOR: &str = "orchestrator";

/// The description the model reads.
///
/// Deliberately free of model ids: direct delegations name an Agent, while model
/// and reasoning routing come from the resolved parent/config/preset policy.
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

/// Explicit filesystem or subsystem ownership for one delegation.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationScope {
    /// Paths or surfaces the child owns.
    #[serde(default)]
    pub include: Vec<String>,
    /// Paths or surfaces the child must leave alone.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Positive and negative requirements for one delegation.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationConstraints {
    /// Conditions the child must preserve or satisfy.
    #[serde(default)]
    pub must: Vec<String>,
    /// Actions or outcomes the child must avoid.
    #[serde(default)]
    pub must_not: Vec<String>,
}

/// The model-visible work agreement for one delegated turn.
#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationContract {
    /// The concrete outcome this delegation advances.
    pub objective: String,
    /// The artifact or answer the child must return.
    pub deliverable: String,
    /// Task-specific execution guidance.
    pub instructions: String,
    /// Observable evidence that proves the delegation succeeded.
    pub success_evidence: String,
    /// Explicit in-scope and out-of-scope ownership.
    #[serde(default)]
    pub scope: Option<DelegationScope>,
    /// Positive and negative requirements.
    #[serde(default)]
    pub constraints: Option<DelegationConstraints>,
    /// Facts or prior work this delegation depends on.
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl DelegationContract {
    fn validate(&self) -> Result<(), TaskRejection> {
        for (field, value) in [
            ("objective", self.objective.as_str()),
            ("deliverable", self.deliverable.as_str()),
            ("instructions", self.instructions.as_str()),
            ("success_evidence", self.success_evidence.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(TaskRejection::EmptyContractField { field });
            }
        }
        if let Some(scope) = &self.scope {
            validate_items("scope.include", &scope.include)?;
            validate_items("scope.exclude", &scope.exclude)?;
        }
        if let Some(constraints) = &self.constraints {
            validate_items("constraints.must", &constraints.must)?;
            validate_items("constraints.must_not", &constraints.must_not)?;
        }
        validate_items("dependencies", &self.dependencies)
    }

    fn render_prompt(&self) -> String {
        let mut sections = vec![
            format!("Objective:\n{}", self.objective.trim()),
            format!("Deliverable:\n{}", self.deliverable.trim()),
            format!("Instructions:\n{}", self.instructions.trim()),
            format!("Success evidence:\n{}", self.success_evidence.trim()),
        ];
        if let Some(scope) = &self.scope {
            push_list(&mut sections, "Include", &scope.include);
            push_list(&mut sections, "Exclude", &scope.exclude);
        }
        if let Some(constraints) = &self.constraints {
            push_list(&mut sections, "Must", &constraints.must);
            push_list(&mut sections, "Must not", &constraints.must_not);
        }
        push_list(&mut sections, "Dependencies", &self.dependencies);
        sections.join("\n\n")
    }
}

/// Stable parent-local identity for one semantic delegation.
///
/// Scheduling details such as foreground/background delivery, model routing, and a
/// newly allocated child session are deliberately excluded. A retry of the same
/// Agent and contract therefore cannot evade durable reconciliation by changing
/// execution mechanics.
#[must_use]
pub fn delegation_logical_key(agent: &str, contract: &DelegationContract) -> String {
    let value = json!({
        "schemaVersion": 1,
        "agent": agent,
        "contract": contract,
    });
    format!("delegation:v1:{}", sha256_json(&value))
}

fn validate_items(field: &'static str, values: &[String]) -> Result<(), TaskRejection> {
    if let Some(index) = values.iter().position(|value| value.trim().is_empty()) {
        return Err(TaskRejection::EmptyContractItem {
            field,
            index: index + 1,
        });
    }
    Ok(())
}

fn push_list(sections: &mut Vec<String>, heading: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let body = values
        .iter()
        .map(|value| format!("- {}", value.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    sections.push(format!("{heading}:\n{body}"));
}

/// Arguments for one delegation.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskParams {
    /// The typed work agreement passed to the child.
    #[serde(flatten)]
    pub contract: DelegationContract,
    /// The specific Agent to delegate to.
    pub agent: String,
    /// Run asynchronously and report a job id immediately. Defaults to foreground.
    #[serde(default)]
    pub background: Option<bool>,
    /// What to do with the terminal report of a background dispatch.
    #[serde(default, rename = "reportDelivery")]
    pub report_delivery: Option<ReportDelivery>,
    /// Continue a previous delegation's session instead of creating a new one.
    #[serde(default)]
    pub task_id: Option<String>,
}

/// Arguments exposed only when the session's durable model-selection policy is enabled.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectableTaskParams {
    /// The typed work agreement passed to the child.
    #[serde(flatten)]
    pub contract: DelegationContract,
    /// The specific Agent to delegate to.
    pub agent: String,
    /// Run asynchronously and report a job id immediately. Defaults to foreground.
    #[serde(default)]
    pub background: Option<bool>,
    /// What to do with the terminal report of a background dispatch.
    #[serde(default, rename = "reportDelivery")]
    pub report_delivery: Option<ReportDelivery>,
    /// Continue a previous delegation's session instead of creating a new one.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Exact allowlisted `provider/model` identity for this child.
    #[serde(default)]
    pub model: Option<String>,
    /// Exact variant declared by `model`; valid only together with `model`.
    #[serde(default)]
    pub effort: Option<String>,
}

impl SelectableTaskParams {
    fn into_parts(self) -> (TaskParams, DelegationModelRequest) {
        (
            TaskParams {
                contract: self.contract,
                agent: self.agent,
                background: self.background,
                report_delivery: self.report_delivery,
                task_id: self.task_id,
            },
            DelegationModelRequest {
                model: self.model,
                effort: self.effort,
            },
        )
    }
}

/// Immutable child model-selection authority frozen into one durable session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentModelPolicy {
    enabled: bool,
    allowed_models: Vec<String>,
    digest: String,
}

impl Default for SubagentModelPolicy {
    fn default() -> Self {
        Self::new(false, Vec::<String>::new()).expect("the disabled subagent model policy is valid")
    }
}

impl SubagentModelPolicy {
    /// Build a canonical policy with a sorted exact allowlist.
    ///
    /// # Errors
    ///
    /// Enabled policies require at least one unique non-empty `provider/model`.
    pub fn new(
        enabled: bool,
        allowed_models: impl IntoIterator<Item = String>,
    ) -> Result<Self, SubagentModelPolicyError> {
        let mut allowed_models = allowed_models.into_iter().collect::<Vec<_>>();
        if allowed_models.iter().any(|model| {
            model.trim().is_empty()
                || model.split_once('/').is_none_or(|(provider, model)| {
                    provider.is_empty() || model.is_empty() || model.contains('/')
                })
        }) {
            return Err(SubagentModelPolicyError::InvalidModel);
        }
        allowed_models.sort();
        let mut unique = BTreeSet::new();
        for model in &allowed_models {
            if !unique.insert(model.as_str()) {
                return Err(SubagentModelPolicyError::Duplicate(model.clone()));
            }
        }
        if enabled && allowed_models.is_empty() {
            return Err(SubagentModelPolicyError::EmptyEnabledAllowlist);
        }
        let digest = subagent_policy_digest(enabled, &allowed_models);
        Ok(Self {
            enabled,
            allowed_models,
            digest,
        })
    }

    /// Verify a decoded durable policy before trusting it.
    pub fn validate(&self) -> Result<(), SubagentModelPolicyError> {
        let canonical = Self::new(self.enabled, self.allowed_models.clone())?;
        if canonical.allowed_models != self.allowed_models || canonical.digest != self.digest {
            return Err(SubagentModelPolicyError::DigestMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn allowed_models(&self) -> &[String] {
        &self.allowed_models
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    #[must_use]
    pub fn allows(&self, model: &str) -> bool {
        self.enabled
            && self
                .allowed_models
                .binary_search_by(|candidate| candidate.as_str().cmp(model))
                .is_ok()
    }
}

fn subagent_policy_digest(enabled: bool, allowed_models: &[String]) -> String {
    sha256_json(&json!({
        "enabled": enabled,
        "allowedModels": allowed_models,
    }))
}

/// Invalid configuration or corrupt durable state for child model selection.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SubagentModelPolicyError {
    #[error("an enabled subagent model selection policy requires a non-empty allowlist")]
    EmptyEnabledAllowlist,
    #[error("each allowed subagent model must be an exact non-empty `provider/model` identity")]
    InvalidModel,
    #[error("allowed subagent model `{0}` is listed more than once")]
    Duplicate(String),
    #[error("the durable subagent model selection policy digest is invalid")]
    DigestMismatch,
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
    /// Immutable parent Attempt which admitted this delegation.
    pub parent_attempt: Option<Arc<AttemptSnapshot>>,
    /// Workflow template owning this child, when delegated by `workflow`.
    pub workflow: Option<String>,
    /// Workflow node owning this child, when delegated by `workflow`.
    pub workflow_node: Option<String>,
    /// An existing child session to continue, from `task_id`.
    pub resume_session_id: Option<String>,
    /// Stable semantic identity used to reject unreconciled duplicate work.
    pub logical_key: String,
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
    /// Durable model-selection authority inherited by the child session.
    pub subagent_model_policy: SubagentModelPolicy,
    /// Exact model field supplied by the caller, for continuation validation.
    pub requested_model: Option<String>,
    /// Exact effort field supplied by the caller, for continuation validation.
    pub requested_effort: Option<String>,
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
    /// The durable state reached by this dispatch.
    pub state: ChildTurnState,
    /// The child's final text, or the running-notice for a background dispatch.
    pub output: String,
    /// Host-generated terminal evidence for a completed foreground child.
    ///
    /// Background children publish the same shape through their durable Job result
    /// and optional next-step report after settlement.
    pub report_metadata: Option<Value>,
}

/// The durable state reached by one delegated child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildTurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}

impl ChildTurnState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Uncertain => "uncertain",
        }
    }
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

    /// Create or resume the child session and drive its turn under the parent's
    /// cancellation signal.
    async fn dispatch(
        &self,
        request: ChildTurnRequest,
        interrupt: Arc<dyn InterruptHandle>,
    ) -> Result<ChildTurn, ChildTurnError>;
}

/// A refusal, phrased so the caller can act on it without a recovery hook.
#[derive(Debug, thiserror::Error)]
pub enum TaskRejection {
    #[error(
        "`{COORDINATOR}` coordinates delegations and cannot be a delegation target — \
         targeting it would reopen the unbounded recursion the roster closes. Set \
         `agent` to one of the valid targets: {targets}."
    )]
    CoordinatorTarget { targets: String },

    #[error(
        "Unknown Agent `{requested}`. Set `agent` to one of the valid targets: \
         {targets}."
    )]
    UnknownTarget { requested: String, targets: String },

    #[error(
        "Subagent depth limit reached: this session is already {depth} \
         delegation hop(s) deep and `subagent_depth` is {limit}. Do this work in the \
         current session, or raise `subagent_depth` in config to allow nested \
         subagents."
    )]
    DepthExceeded { depth: u32, limit: u32 },

    #[error("`{field}` must not be empty in the delegation contract.")]
    EmptyContractField { field: &'static str },
    #[error("`{field}` item {index} must not be empty in the delegation contract.")]
    EmptyContractItem { field: &'static str, index: usize },
    #[error(
        "`reportDelivery` requires `background: true`. Remove `reportDelivery` for a \
         foreground delegation, or add `background: true` to receive the result later."
    )]
    ReportDeliveryRequiresBackground,
    #[error("`effort` requires an explicit allowlisted `model`; add `model` or remove `effort`.")]
    EffortRequiresModel,
    #[error(
        "Model `{requested}` is not authorized for this session. Choose exactly one of: {allowed}."
    )]
    ModelNotAllowed { requested: String, allowed: String },
    #[error("Model `{requested}` is authorized but is not present in the resolved model catalog.")]
    ModelUnavailable { requested: String },
    #[error(
        "Effort `{requested}` is not a variant declared by `{model}`. Choose exactly one of: {declared}."
    )]
    EffortNotDeclared {
        requested: String,
        model: String,
        declared: String,
    },
}

/// The guidance a `task` refusal from the permission layer carries.
///
/// [`zuno_error::ToolError::Denied`] has no source to hang prose on — by design, since
/// a denial needs a grant rather than a better call. So the fix travels on the
/// [`PermissionAsk`] instead, under [`GUIDANCE_KEY`], where it reaches the human
/// deciding *and* is recoverable by a caller inspecting the ask.
#[must_use]
pub fn denial_guidance(agent: &str, targets: &[String]) -> String {
    format!(
        "`{WIRE_ID}` is not permitted for `{agent}`. Grant \
         `{PERMISSION_KEY}` for pattern `{agent}`, or set `agent` to \
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

/// A validated, composition-owned set of agents the `task` tool may target.
///
/// The default constructor still derives Zuno's native roster from
/// [`valid_targets`]. A composition that also resolves configured or extension
/// agents replaces that roster with this exact set, so the tool never advertises
/// an agent the child-turn host cannot start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationTargets {
    names: Vec<String>,
}

impl DelegationTargets {
    /// Validate one exact target roster while preserving declaration order.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty identifier, a duplicate, or the primary
    /// coordinator. Agent-name syntax is validated by the catalog before this
    /// same-process boundary.
    pub fn new(names: impl IntoIterator<Item = String>) -> Result<Self, DelegationTargetError> {
        let mut resolved = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for name in names {
            if name.trim().is_empty() {
                return Err(DelegationTargetError::Empty);
            }
            if name == COORDINATOR {
                return Err(DelegationTargetError::Coordinator);
            }
            if !seen.insert(name.clone()) {
                return Err(DelegationTargetError::Duplicate(name));
            }
            resolved.push(name);
        }
        Ok(Self { names: resolved })
    }

    /// Target names in stable catalog order.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.names
    }
}

/// Invalid composition input for [`DelegationTargets`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DelegationTargetError {
    /// Agent identities are never blank.
    #[error("a delegation target cannot be empty")]
    Empty,
    /// The user-facing coordinator cannot recursively target itself.
    #[error("`{COORDINATOR}` is the primary coordinator and cannot be a delegation target")]
    Coordinator,
    /// One identity must map to one child-agent definition.
    #[error("delegation target `{0}` is registered more than once")]
    Duplicate(String),
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

/// Internal model-routing inputs shared by task, Council, and Workflow.
///
/// Council and Workflow already own their work contracts and only need the common
/// model/effort resolution ladder. Keeping this type separate prevents them from
/// fabricating a model-facing [`DelegationContract`] merely to reuse routing policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DelegationModelRequest {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Delegation to a child session, gated on depth and permission.
#[derive(Clone)]
pub struct TaskTool {
    host: Arc<dyn ChildTurnHost>,
    facts: Arc<dyn ProviderFacts>,
    presets: PresetLibrary,
    session_model: Option<ModelChoice>,
    agent_overrides: BTreeMap<String, ModelChoice>,
    limits: DelegationLimits,
    vision_available: bool,
    targets: Option<DelegationTargets>,
    subagent_model_policy: SubagentModelPolicy,
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
            targets: None,
            subagent_model_policy: SubagentModelPolicy::default(),
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

    /// Replace the native target roster with the composition's exact resolved set.
    #[must_use]
    pub fn with_targets(mut self, targets: DelegationTargets) -> Self {
        self.targets = Some(targets);
        self
    }

    /// Bind the tool schema and every child request to one durable session policy.
    #[must_use]
    pub fn with_subagent_model_policy(mut self, policy: SubagentModelPolicy) -> Self {
        self.subagent_model_policy = policy;
        self
    }

    /// Expose the opt-in schema that includes `model` and `effort`.
    #[must_use]
    pub fn selectable(self) -> SelectableTaskTool {
        SelectableTaskTool(self)
    }

    pub(crate) fn subagent_model_policy(&self) -> SubagentModelPolicy {
        self.subagent_model_policy.clone()
    }

    pub(crate) fn targets(&self) -> Vec<String> {
        self.targets.as_ref().map_or_else(
            || valid_targets(self.vision_available),
            |targets| targets.as_slice().to_vec(),
        )
    }

    /// Resolve the child's model and effort for direct or host-owned delegation.
    ///
    /// The model-facing `task` wire always passes an empty `request`; configured
    /// workflows and Council may provide a host-owned route without exposing those
    /// fields to the model. Resolution reuses [`ModelPolicy`] rather than creating a
    /// second policy:
    ///
    /// 1. an optional host-owned model/reasoning request,
    /// 2. a per-agent config override,
    /// 3. the active preset's entry for the agent or workflow category,
    /// 4. the parent session's model.
    ///
    /// Rung 1 is skip-on-unavailable like the rest: an unreachable or unqualified
    /// model becomes a note and the ladder continues, because refusing the whole
    /// delegation over a model name would lose work the caller has already framed.
    /// What is *not* allowed is silence — every skip is in [`DelegationPlan::notes`]
    /// and reaches the caller in the rendered output.
    #[must_use]
    pub(crate) fn plan(
        &self,
        agent: &str,
        category: Option<&str>,
        request: &DelegationModelRequest,
    ) -> DelegationPlan {
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
            .call_model(request.model.as_deref(), &availability, &mut notes)
            .or(lower.model);
        let variant = request
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

    fn target(&self, requested: &str) -> Result<String, ToolError> {
        let targets = self.targets();
        let rendered = targets.join(", ");
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
        Ok(requested.to_owned())
    }

    pub(crate) async fn guard_depth(&self, ctx: &ToolContext) -> Result<(), ToolError> {
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

    fn strict_plan(
        &self,
        agent: &str,
        request: &DelegationModelRequest,
    ) -> Result<DelegationPlan, ToolError> {
        if request.effort.is_some() && request.model.is_none() {
            return Err(reject(TaskRejection::EffortRequiresModel));
        }
        let Some(requested) = request.model.as_deref() else {
            return Ok(self.plan(agent, None, &DelegationModelRequest::default()));
        };
        if !self.subagent_model_policy.allows(requested) {
            return Err(reject(TaskRejection::ModelNotAllowed {
                requested: requested.to_owned(),
                allowed: self.subagent_model_policy.allowed_models().join(", "),
            }));
        }
        let model = ModelChoice::new(requested);
        let facts = self.facts.facts(&model).ok_or_else(|| {
            reject(TaskRejection::ModelUnavailable {
                requested: requested.to_owned(),
            })
        })?;
        let (effort, provider_options) = match request.effort.as_deref() {
            None => (None, Map::new()),
            Some(requested_effort) => {
                let Some(options) = facts.variants.get(requested_effort) else {
                    return Err(reject(TaskRejection::EffortNotDeclared {
                        requested: requested_effort.to_owned(),
                        model: requested.to_owned(),
                        declared: facts
                            .variants
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", "),
                    }));
                };
                (requested_effort.parse().ok(), options.clone())
            }
        };
        Ok(DelegationPlan {
            agent: agent.to_owned(),
            category: None,
            model: Some(model),
            effort,
            provider_options,
            notes: Vec::new(),
        })
    }

    async fn run_with_request(
        &self,
        params: TaskParams,
        request: DelegationModelRequest,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        params.contract.validate().map_err(reject)?;
        let background = params.background.unwrap_or(false);
        if !background && params.report_delivery.is_some() {
            return Err(reject(TaskRejection::ReportDeliveryRequiresBackground));
        }
        let report_delivery = params.report_delivery.unwrap_or_default();

        let agent = self.target(&params.agent)?;
        self.guard_depth(&ctx).await?;

        let targets = self.targets();
        let mut metadata = Map::new();
        metadata.insert(
            "objective".to_owned(),
            Value::String(params.contract.objective.clone()),
        );
        metadata.insert("agent".to_owned(), Value::String(agent.clone()));
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
                ..PermissionAsk::default()
            },
        )
        .await?;

        let plan = if self.subagent_model_policy.enabled() {
            self.strict_plan(&agent, &request)?
        } else {
            self.plan(&agent, None, &DelegationModelRequest::default())
        };
        let logical_key = delegation_logical_key(&agent, &params.contract);
        let requested_model = request.model.clone();
        let requested_effort = request.effort.clone();
        let turn = self
            .host
            .dispatch(
                ChildTurnRequest {
                    parent_session_id: ctx.session_id.clone(),
                    parent_attempt: ctx.orchestration_snapshot().cloned(),
                    workflow: None,
                    workflow_node: None,
                    resume_session_id: params.task_id.clone(),
                    logical_key,
                    agent,
                    description: Some(params.contract.objective.clone()),
                    prompt: params.contract.render_prompt(),
                    model: plan.model.clone(),
                    effort: plan.effort,
                    provider_options: plan.provider_options.clone(),
                    subagent_model_policy: self.subagent_model_policy.clone(),
                    requested_model,
                    requested_effort,
                    background,
                    report_delivery,
                },
                Arc::clone(&ctx.interrupt),
            )
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

#[async_trait]
impl TypedTool for TaskTool {
    type Params = TaskParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn ui_intent(&self) -> ToolUiIntent {
        ToolUiIntent::Subagent
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::IsolatedBackground
    }

    async fn run(&self, params: TaskParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.run_with_request(params, DelegationModelRequest::default(), ctx)
            .await
    }
}

/// Opt-in `task` definition whose schema includes explicit model and effort fields.
#[derive(Clone)]
pub struct SelectableTaskTool(TaskTool);

#[async_trait]
impl TypedTool for SelectableTaskTool {
    type Params = SelectableTaskParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn ui_intent(&self) -> ToolUiIntent {
        ToolUiIntent::Subagent
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::IsolatedBackground
    }

    async fn run(
        &self,
        params: SelectableTaskParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let (params, request) = params.into_parts();
        self.0.run_with_request(params, request, ctx).await
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
/// Resolution notes stay inside the envelope so configured routing fallbacks remain
/// visible rather than silently changing the child model or reasoning level.
fn render(
    params: &TaskParams,
    plan: &DelegationPlan,
    turn: &ChildTurn,
    background: bool,
) -> ToolOutput {
    let state = turn.state.as_str();
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
    lines.push(format!("<summary>{}</summary>", params.contract.objective));
    for note in &plan.notes {
        lines.push(format!("<note>{note}</note>"));
    }
    lines.push("<task_result>".to_owned());
    lines.push(turn.output.clone());
    lines.push("</task_result>".to_owned());
    lines.push("</task>".to_owned());

    let title = params.contract.objective.clone();
    let report_delivery = if background {
        match params.report_delivery.unwrap_or_default() {
            ReportDelivery::NextStep => "nextStep",
            ReportDelivery::Quiet => "quiet",
        }
    } else {
        "foreground"
    };
    let mut metadata = json!({
        "sessionId": turn.session_id,
        "jobId": turn.job_id,
        "agent": plan.agent,
        "objective": params.contract.objective,
        "deliverable": params.contract.deliverable,
        "successEvidence": params.contract.success_evidence,
        "contract": params.contract,
        "state": state,
        "background": background,
        "reportDelivery": report_delivery,
        "model": plan.model.as_ref().map(|model| model.model.as_str()),
        "effort": plan.effort.map(ReasoningEffort::as_str),
    });
    if let Some(report) = &turn.report_metadata {
        metadata["report"] = report.clone();
    }
    let output =
        ToolOutput::text(title, lines.join("\n")).with_metadata(METADATA_SUBAGENT_KEY, metadata);
    if background && params.report_delivery.unwrap_or_default() == ReportDelivery::NextStep {
        output.with_continuation(zuno_tool::ToolContinuation::YieldUntilInput)
    } else {
        output
    }
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
    report_metadata: Option<Value>,
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

    /// Attach host-generated foreground report evidence.
    #[must_use]
    pub fn with_report_metadata(mut self, metadata: Value) -> Self {
        self.report_metadata = Some(metadata);
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

    async fn dispatch(
        &self,
        request: ChildTurnRequest,
        _interrupt: Arc<dyn InterruptHandle>,
    ) -> Result<ChildTurn, ChildTurnError> {
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
            state: if background {
                ChildTurnState::Running
            } else {
                ChildTurnState::Completed
            },
            output: "done".to_owned(),
            report_metadata: (!background)
                .then(|| self.report_metadata.clone())
                .flatten(),
            session_id,
        })
    }
}

#[cfg(test)]
mod tests;
