//! Zuno-native harness profiles and bundle composition.

use async_trait::async_trait;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
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
const TOOL_MANIFEST_COMPONENT_ID: &str = "zuno.tools";
const TOOL_CONTRIBUTIONS_COMPONENT_ID: &str = "zuno.tool-contributions";
const PRODUCT_CAPABILITY_SCOPE: &str = "profile";
const PRODUCT_CAPABILITY_VERSION: CapabilityVersion = CapabilityVersion::new(1, 0);

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
    HarnessProfile::new(id).with_bundle(
        ProfileBundle::new(CORE_BUNDLE_ID)
            .with_component(AgentDriverComponent::new(driver))
            .with_component(ToolManifestComponent::new(tools))
            .with_component(ToolContributionsComponent::new(
                TOOL_CONTRIBUTIONS_COMPONENT_ID,
                contributions,
            )),
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
    profile_with_tools(
        "default",
        Arc::new(DefaultAgentDriver),
        ToolManifest::standard(),
        contributions,
    )
    .with_bundle(
        ProfileBundle::new(HOST_PLANNING_BUNDLE_ID).with_component(HostPlanningCapabilityComponent),
    )
}
