//! Zuno-native harness profiles and bundle composition.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use zuno_engine::driver::{AgentDriver, AgentDriverComponent, DefaultAgentDriver};
use zuno_runtime::{Component, HarnessProfile, MountContext, ProfileBundle, RuntimeError};
use zuno_tools::registry::{BUILTIN_ORDER, BuiltinSlot, CustomTool};

const CORE_BUNDLE_ID: &str = "zuno.core";
const TOOL_MANIFEST_COMPONENT_ID: &str = "zuno.tools";
const TOOL_CONTRIBUTIONS_COMPONENT_ID: &str = "zuno.tool-contributions";

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

    /// The complete native built-in surface.
    #[must_use]
    pub fn all() -> Self {
        Self {
            slots: BUILTIN_ORDER.to_vec(),
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

    async fn mount(&self, context: &mut MountContext) -> Result<(), RuntimeError> {
        context.provide(Arc::clone(&self.manifest))
    }
}

struct ToolContributionsComponent {
    contributions: Arc<ToolContributions>,
}

impl ToolContributionsComponent {
    fn new(contributions: ToolContributions) -> Self {
        Self {
            contributions: Arc::new(contributions),
        }
    }
}

#[async_trait]
impl Component for ToolContributionsComponent {
    fn id(&self) -> &str {
        TOOL_CONTRIBUTIONS_COMPONENT_ID
    }

    async fn mount(&self, context: &mut MountContext) -> Result<(), RuntimeError> {
        context.provide(Arc::clone(&self.contributions))
    }
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
            .with_component(ToolContributionsComponent::new(contributions)),
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
        ToolManifest::all(),
        contributions,
    )
}
