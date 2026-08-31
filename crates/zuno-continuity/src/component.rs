use std::sync::Arc;

use async_trait::async_trait;
use zuno_db::Pool;
use zuno_harness::{ToolContributions, tool_contributions_bundle};
use zuno_runtime::{Component, HarnessProfile, PrepareContext, ProfileBundle, RuntimeError};
use zuno_tool::{Tool, erase};

use crate::history::{HistoryProvider, HistoryTool};
use crate::notes::{NotesProvider, NotesTool};
use crate::{ContinuityError, SqliteContinuityProvider};

const CONTINUITY_PROFILE_ID: &str = "zuno.continuity";
const CONTINUITY_BUNDLE_ID: &str = "zuno.continuity.service";
const CONTINUITY_COMPONENT_ID: &str = "zuno.continuity";
const CONTINUITY_TOOLS_BUNDLE_ID: &str = "zuno.continuity.tools";
const CONTINUITY_TOOLS_COMPONENT_ID: &str = "zuno.continuity.tool-contributions";

/// Final continuity selection after configuration resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContinuitySettings {
    pub history: bool,
    pub notes: bool,
}

impl ContinuitySettings {
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.history || self.notes
    }
}

/// Typed session-scoped continuity service published by the native component.
pub struct ContinuityService {
    provider: Arc<SqliteContinuityProvider>,
    settings: ContinuitySettings,
}

impl ContinuityService {
    pub fn open(pool: Arc<Pool>, settings: ContinuitySettings) -> Result<Self, ContinuityError> {
        Ok(Self {
            provider: Arc::new(SqliteContinuityProvider::open(pool, settings.notes)?),
            settings,
        })
    }

    #[must_use]
    pub const fn settings(&self) -> ContinuitySettings {
        self.settings
    }

    #[must_use]
    pub fn provider(&self) -> &Arc<SqliteContinuityProvider> {
        &self.provider
    }
}

struct ContinuityComponent {
    service: Arc<ContinuityService>,
}

#[async_trait]
impl Component for ContinuityComponent {
    fn id(&self) -> &str {
        CONTINUITY_COMPONENT_ID
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::clone(&self.service))
    }
}

/// Build the child-profile overlay that owns continuity services and tools.
///
/// `base` is the exact inherited contribution snapshot. The overlay republishes
/// that snapshot plus enabled continuity tools so downstream consumers resolve one
/// complete nearest-scope service rather than merging private registries.
pub fn profile_overlay(
    base: &ToolContributions,
    pool: Arc<Pool>,
    settings: ContinuitySettings,
) -> Result<HarnessProfile, ContinuityError> {
    let service = Arc::new(ContinuityService::open(pool, settings)?);
    let mut tools: Vec<Arc<dyn Tool>> = base.tools().to_vec();
    if settings.history {
        let provider: Arc<dyn HistoryProvider> = service.provider().clone();
        tools.push(erase(HistoryTool::new(provider)));
    }
    if settings.notes {
        let provider: Arc<dyn NotesProvider> = service.provider().clone();
        tools.push(erase(NotesTool::new(provider)));
    }
    let contributions = ToolContributions::new(tools)
        .map_err(|error| ContinuityError::Composition(error.to_string()))?;
    Ok(HarnessProfile::new(CONTINUITY_PROFILE_ID)
        .with_bundle(
            ProfileBundle::new(CONTINUITY_BUNDLE_ID)
                .with_component(ContinuityComponent { service }),
        )
        .with_bundle(tool_contributions_bundle(
            CONTINUITY_TOOLS_BUNDLE_ID,
            CONTINUITY_TOOLS_COMPONENT_ID,
            contributions,
        )))
}
