//! Zuno-native harness profiles and bundle composition.

use async_trait::async_trait;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use zuno_engine::budget::TurnAllowance;
use zuno_engine::driver::{AgentDriver, AgentDriverComponent, DefaultAgentDriver};
use zuno_orchestration::{CapabilitySnapshot, sha256_json, sha256_text};
use zuno_runtime::{
    CapabilityContract, CapabilityDefinitionError, CapabilityKey, CapabilityProvenance,
    CapabilityScope, CapabilityVersion, Component, HarnessProfile, PrepareContext, ProfileBundle,
    RuntimeError,
};
use zuno_tools::registry::{BuiltinSlot, CustomTool, DEFAULT_BUILTINS};

const CORE_BUNDLE_ID: &str = "zuno.core";
const ORCHESTRATION_CAPABILITIES_BUNDLE_ID: &str = "zuno.orchestration-capabilities";
const ORCHESTRATION_CAPABILITIES_COMPONENT_ID: &str = "zuno.orchestration-capabilities";
const HOST_PLANNING_BUNDLE_ID: &str = "zuno.host-planning";
const HOST_PLANNING_COMPONENT_ID: &str = "zuno.host-planning";
const TURN_ALLOWANCE_BUNDLE_ID: &str = "zuno.turn-allowance";
const TURN_ALLOWANCE_COMPONENT_ID: &str = "zuno.turn-allowance";
const TOOL_MANIFEST_COMPONENT_ID: &str = "zuno.tools";
const TOOL_CONTRIBUTIONS_COMPONENT_ID: &str = "zuno.tool-contributions";
const PUBLIC_HTTP_COMPONENT_ID: &str = "zuno.public-http";
const PRODUCT_CAPABILITY_SCOPE: &str = "profile";
const PRODUCT_CAPABILITY_VERSION: CapabilityVersion = CapabilityVersion::new(1, 0);

/// The token budget the default host grants a goal nobody put a number on.
///
/// Forty steps at a 200,000-token window. Both factors are already this workspace's:
/// 200,000 is the context window the engine's own budget tests assume and the one the
/// configuration fixtures use most, and a forty-step turn is the runaway the budget
/// module names as the case it exists to stop. Every provider
/// request re-sends the whole prompt, the prompt cannot exceed the window, and cache
/// reads are charged, so 40 × 200,000 = 8,000,000 tokens is the most one such turn can
/// cost. A goal that gets here without anyone having set a budget has had one full
/// runaway's worth of allowance; the next number should come from a human, and the
/// stop says so. A host that wants unlimited autonomy says so with
/// [`TurnAllowance::UNLIMITED`] rather than with a larger number.
pub const DEFAULT_GOAL_TOKEN_BUDGET: u64 = 40 * 200_000;

/// The allowance the standard profile grants a turn.
///
/// Only the token default is set. The tool-call and wall-time ceilings stay at none
/// because the workspace assumes no number for them, and a ceiling invented here
/// would stop legitimate long turns on a guess; a host that has measured its own
/// turns sets them through [`default_profile_with_tools_and_allowance`].
pub const DEFAULT_TURN_ALLOWANCE: TurnAllowance = TurnAllowance {
    default_token_budget: Some(DEFAULT_GOAL_TOKEN_BUDGET),
    max_tool_calls: None,
    max_duration: None,
};

/// Product descriptor families projected into the runtime-named capability plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductCapabilityKind {
    /// A native executable tool whose object remains in [`ToolContributions`].
    Tool,
    /// An immutable Agent Profile descriptor.
    AgentProfile,
    /// An immutable workflow-template descriptor.
    WorkflowTemplate,
}

impl ProductCapabilityKind {
    const fn namespace(self) -> &'static str {
        match self {
            Self::Tool => "zuno.tool",
            Self::AgentProfile => "zuno.agent-profile",
            Self::WorkflowTemplate => "zuno.workflow-template",
        }
    }

    const fn interface(self) -> &'static str {
        match self {
            Self::Tool => "zuno.tool/v1",
            Self::AgentProfile => "zuno.agent-profile/v1",
            Self::WorkflowTemplate => "zuno.workflow-template/v1",
        }
    }
}

/// Build the stable runtime key used by product components and consumers.
pub fn named_capability_key(
    kind: ProductCapabilityKind,
    name: impl Into<String>,
) -> Result<CapabilityKey, CapabilityDefinitionError> {
    CapabilityKey::new(
        kind.namespace(),
        name,
        PRODUCT_CAPABILITY_VERSION,
        CapabilityScope::new(PRODUCT_CAPABILITY_SCOPE)?,
    )
}

/// Build a Skill key whose isolation scope preserves same-name sources.
///
/// Skill discovery deliberately keeps colliding names independently addressable.
/// Hashing the stable source identity into the scope preserves that behavior while
/// keeping the human-facing Skill name unchanged in the key.
pub fn skill_capability_key(
    name: impl Into<String>,
    source: &str,
) -> Result<CapabilityKey, CapabilityDefinitionError> {
    let source = source.trim();
    if source.is_empty() {
        return Err(CapabilityDefinitionError::Empty("skill source"));
    }
    CapabilityKey::new(
        "zuno.skill",
        name,
        PRODUCT_CAPABILITY_VERSION,
        CapabilityScope::new(format!("source:{}", sha256_text(source)))?,
    )
}

/// The ordered built-in tool surface exposed by one harness profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolManifest {
    slots: Vec<BuiltinSlot>,
}

impl ToolManifest {
    /// Build a manifest, rejecting duplicate slots before profile activation.
    pub fn new(slots: impl IntoIterator<Item = BuiltinSlot>) -> Result<Self, ToolManifestError> {
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        for slot in slots {
            if !seen.insert(slot) {
                return Err(ToolManifestError::Duplicate(slot));
            }
            ordered.push(slot);
        }
        Ok(Self { slots: ordered })
    }

    /// The complete default-host built-in surface.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            slots: DEFAULT_BUILTINS.to_vec(),
        }
    }

    /// Ordered slots contributed by this profile.
    #[must_use]
    pub fn slots(&self) -> &[BuiltinSlot] {
        &self.slots
    }

    /// Whether this profile includes `slot`.
    #[must_use]
    pub fn contains(&self, slot: BuiltinSlot) -> bool {
        self.slots.contains(&slot)
    }
}

/// Invalid native tool-manifest declarations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolManifestError {
    /// A slot may appear only once because order is semantically significant.
    #[error("tool slot `{}` is declared more than once", .0.wire_id())]
    Duplicate(BuiltinSlot),
}

/// Native tools contributed by one harness profile.
#[derive(Clone, Default)]
pub struct ToolContributions {
    tools: Vec<CustomTool>,
}

impl ToolContributions {
    /// Build a contribution set, rejecting duplicate wire ids inside the profile.
    ///
    /// A contributed id may intentionally replace a built-in. Duplicate contributed
    /// ids are rejected because their winner would otherwise depend on bundle order.
    pub fn new(
        tools: impl IntoIterator<Item = CustomTool>,
    ) -> Result<Self, ToolContributionsError> {
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        for tool in tools {
            let id = tool.id().to_owned();
            if !seen.insert(id.clone()) {
                return Err(ToolContributionsError::Duplicate(id));
            }
            ordered.push(tool);
        }
        Ok(Self { tools: ordered })
    }

    /// Ordered tools contributed by this profile.
    #[must_use]
    pub fn tools(&self) -> &[CustomTool] {
        &self.tools
    }
}

impl std::fmt::Debug for ToolContributions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolContributions")
            .field(
                "ids",
                &self.tools.iter().map(|tool| tool.id()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Invalid native tool contributions.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolContributionsError {
    /// One profile may contribute a wire id only once.
    #[error("tool `{0}` is contributed more than once")]
    Duplicate(String),
}

/// Typed marker granting the host permission to maintain durable Plans.
///
/// This is independent from the model-visible `plan_update` tool. A profile may
/// expose that tool without granting host planning, or hide the tool while the
/// default host continues to classify work and persist recovery state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostPlanningCapability;

struct HostPlanningCapabilityComponent;

#[async_trait]
impl Component for HostPlanningCapabilityComponent {
    fn id(&self) -> &str {
        HOST_PLANNING_COMPONENT_ID
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::new(HostPlanningCapability))
    }
}

/// Publishes the host's [`TurnAllowance`] as a typed profile service.
///
/// A profile service rather than a constant read by the goal store, so the number a
/// run stops on is visible where the host composes its runtime and can differ per
/// profile; a store-level constant would make every host's default the same and
/// would hide the choice from the profile that is supposed to own it.
struct TurnAllowanceComponent {
    allowance: Arc<TurnAllowance>,
}

impl TurnAllowanceComponent {
    fn new(allowance: TurnAllowance) -> Self {
        Self {
            allowance: Arc::new(allowance),
        }
    }
}

#[async_trait]
impl Component for TurnAllowanceComponent {
    fn id(&self) -> &str {
        TURN_ALLOWANCE_COMPONENT_ID
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::clone(&self.allowance))
    }
}

struct ToolManifestComponent {
    manifest: Arc<ToolManifest>,
}

impl ToolManifestComponent {
    fn new(manifest: ToolManifest) -> Self {
        Self {
            manifest: Arc::new(manifest),
        }
    }
}

#[async_trait]
impl Component for ToolManifestComponent {
    fn id(&self) -> &str {
        TOOL_MANIFEST_COMPONENT_ID
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::clone(&self.manifest))
    }
}

struct ToolContributionsComponent {
    id: String,
    contributions: Arc<ToolContributions>,
}

struct PublicHttpComponent {
    client: Arc<zuno_network::PublicHttpClient>,
}

impl PublicHttpComponent {
    fn new(client: Arc<zuno_network::PublicHttpClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Component for PublicHttpComponent {
    fn id(&self) -> &str {
        PUBLIC_HTTP_COMPONENT_ID
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::clone(&self.client))
    }
}

impl ToolContributionsComponent {
    fn new(id: impl Into<String>, contributions: ToolContributions) -> Self {
        Self {
            id: id.into(),
            contributions: Arc::new(contributions),
        }
    }
}

#[async_trait]
impl Component for ToolContributionsComponent {
    fn id(&self) -> &str {
        &self.id
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::clone(&self.contributions))?;
        for tool in self.contributions.tools() {
            let identity = tool.definition().schema_identity();
            provide_named_capability(
                context,
                ProductCapabilityKind::Tool,
                tool.id(),
                format!("profile-contribution://tool/{}", tool.id()),
                None,
                identity.schema_sha256,
            )?;
        }
        Ok(())
    }
}

struct OrchestrationCapabilitiesComponent {
    snapshot: Arc<CapabilitySnapshot>,
}

impl OrchestrationCapabilitiesComponent {
    fn new(snapshot: Arc<CapabilitySnapshot>) -> Self {
        Self { snapshot }
    }
}

#[async_trait]
impl Component for OrchestrationCapabilitiesComponent {
    fn id(&self) -> &str {
        ORCHESTRATION_CAPABILITIES_COMPONENT_ID
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::clone(&self.snapshot))?;
        let package = format!("{}@{}", self.snapshot.pack.id, self.snapshot.pack.version);

        for profile in &self.snapshot.profiles {
            provide_named_capability(
                context,
                ProductCapabilityKind::AgentProfile,
                &profile.name,
                &profile.source_id,
                Some(package.clone()),
                descriptor_digest(profile)?,
            )?;
        }
        for workflow in &self.snapshot.workflows {
            provide_named_capability(
                context,
                ProductCapabilityKind::WorkflowTemplate,
                &workflow.name,
                &workflow.source_id,
                Some(package.clone()),
                descriptor_digest(workflow)?,
            )?;
        }
        for skill in &self.snapshot.skills {
            provide_capability(
                context,
                skill_capability_key(&skill.name, &skill.source).map_err(component_error)?,
                "zuno.skill/v1",
                &skill.source,
                Some(package.clone()),
                descriptor_digest(skill)?,
            )?;
        }
        Ok(())
    }
}

fn descriptor_digest(descriptor: &impl Serialize) -> Result<String, RuntimeError> {
    let value = serde_json::to_value(descriptor).map_err(component_error)?;
    Ok(sha256_json(&value))
}

fn provide_named_capability(
    context: &mut PrepareContext,
    kind: ProductCapabilityKind,
    name: impl Into<String>,
    source: impl Into<String>,
    package: Option<String>,
    schema_digest: String,
) -> Result<(), RuntimeError> {
    let key = named_capability_key(kind, name).map_err(component_error)?;
    provide_capability(
        context,
        key,
        kind.interface(),
        source,
        package,
        schema_digest,
    )
}

fn provide_capability(
    context: &mut PrepareContext,
    key: CapabilityKey,
    interface: &'static str,
    source: impl Into<String>,
    package: Option<String>,
    schema_digest: String,
) -> Result<(), RuntimeError> {
    let contract =
        CapabilityContract::new(interface, Some(schema_digest)).map_err(component_error)?;
    let provenance = CapabilityProvenance::new(source, package).map_err(component_error)?;
    context.provide_capability(key, contract, provenance)
}

fn component_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Component(error.to_string())
}

/// Bundle the immutable orchestration snapshot and its named product descriptors.
///
/// The snapshot remains available as a typed Rust service. The named plane carries
/// descriptor identities only and therefore cannot bypass native scheduling,
/// authorization, or lifecycle ownership.
#[must_use]
pub fn orchestration_capabilities_bundle(snapshot: Arc<CapabilitySnapshot>) -> ProfileBundle {
    ProfileBundle::new(ORCHESTRATION_CAPABILITIES_BUNDLE_ID)
        .with_component(OrchestrationCapabilitiesComponent::new(snapshot))
}

/// Bundle the allowance a host grants every turn, as a typed profile service.
///
/// The host resolves it with `runtime.service::<TurnAllowance>()` when it builds a
/// turn's budget policy. A profile that mounts no such bundle publishes no
/// allowance, and a host must read that absence as [`TurnAllowance::UNLIMITED`]:
/// the runtime treats an absent optional service as not configured, never as a
/// default it invents. Custom profiles mount this explicitly rather than
/// inheriting a number sized for the interactive host, for the same reason they
/// opt into host planning.
#[must_use]
pub fn turn_allowance_bundle(allowance: TurnAllowance) -> ProfileBundle {
    ProfileBundle::new(TURN_ALLOWANCE_BUNDLE_ID)
        .with_component(TurnAllowanceComponent::new(allowance))
}

/// Build a profile bundle that publishes one complete native tool snapshot.
///
/// A child runtime can mount this bundle to shadow inherited contributions after
/// adding session-scoped providers. The caller supplies a distinct component id
/// because component registration, replacement, and disposal are identity based.
#[must_use]
pub fn tool_contributions_bundle(
    bundle_id: impl Into<String>,
    component_id: impl Into<String>,
    contributions: ToolContributions,
) -> ProfileBundle {
    ProfileBundle::new(bundle_id)
        .with_component(ToolContributionsComponent::new(component_id, contributions))
}

/// Build a profile from an arbitrary driver and native tool surface.
#[must_use]
pub fn profile(
    id: impl Into<String>,
    driver: Arc<dyn AgentDriver>,
    tools: ToolManifest,
) -> HarnessProfile {
    profile_with_tools(id, driver, tools, ToolContributions::default())
}

/// Build a profile with native tool implementations contributed by its bundles.
#[must_use]
pub fn profile_with_tools(
    id: impl Into<String>,
    driver: Arc<dyn AgentDriver>,
    tools: ToolManifest,
    contributions: ToolContributions,
) -> HarnessProfile {
    profile_with_tools_and_public_http(
        id,
        driver,
        tools,
        contributions,
        Arc::new(zuno_network::PublicHttpClient::new()),
    )
}

/// Build a profile with an explicitly owned public-internet transport.
///
/// This is the injection seam for host-specific DNS resolution and public-target
/// policy. `webfetch` consumes the activated typed service rather than constructing
/// a process-global client.
#[must_use]
pub fn profile_with_tools_and_public_http(
    id: impl Into<String>,
    driver: Arc<dyn AgentDriver>,
    tools: ToolManifest,
    contributions: ToolContributions,
    public_http: Arc<zuno_network::PublicHttpClient>,
) -> HarnessProfile {
    HarnessProfile::new(id).with_bundle(
        ProfileBundle::new(CORE_BUNDLE_ID)
            .with_component(AgentDriverComponent::new(driver))
            .with_component(ToolManifestComponent::new(tools))
            .with_component(ToolContributionsComponent::new(
                TOOL_CONTRIBUTIONS_COMPONENT_ID,
                contributions,
            ))
            .with_component(PublicHttpComponent::new(public_http)),
    )
}

/// The standard interactive Zuno harness.
#[must_use]
pub fn default_profile() -> HarnessProfile {
    default_profile_with_tools(ToolContributions::default())
}

/// The standard interactive harness plus native process-owned tool contributions.
///
/// This is the composition seam for capabilities whose provider is created by the
/// hosting process rather than compiled into `zuno-harness`. They still mount as a
/// typed profile service and therefore use the same transactional activation as a
/// custom harness.
#[must_use]
pub fn default_profile_with_tools(contributions: ToolContributions) -> HarnessProfile {
    default_profile_with_tools_and_allowance(contributions, DEFAULT_TURN_ALLOWANCE)
}

/// The standard interactive harness under an allowance the host chose.
///
/// The seam for a host with its own view of what an unbudgeted goal may spend or
/// how long a turn may run, including [`TurnAllowance::UNLIMITED`] for one that
/// genuinely wants no ceiling: that choice is then written in the profile rather
/// than implied by a missing number.
#[must_use]
pub fn default_profile_with_tools_and_allowance(
    contributions: ToolContributions,
    allowance: TurnAllowance,
) -> HarnessProfile {
    profile_with_tools(
        "default",
        Arc::new(DefaultAgentDriver),
        ToolManifest::standard(),
        contributions,
    )
    .with_bundle(
        ProfileBundle::new(HOST_PLANNING_BUNDLE_ID).with_component(HostPlanningCapabilityComponent),
    )
    .with_bundle(turn_allowance_bundle(allowance))
}
