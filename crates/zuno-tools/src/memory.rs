//! `memory_propose` records an auditable resident-memory candidate.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_memory::{MemoryProposal, MemoryService};
use zuno_tool::{ToolContext, ToolOutput, ToolReplayPolicy, TypedTool};
use zuno_types::MemorySource;

/// The only model-visible memory mutation entry point.
pub const MEMORY_TOOL_ID: &str = "memory_propose";

/// Prompt-visible guidance for candidate creation.
pub const DESCRIPTION: &str = include_str!("description/memory-propose.txt");

/// Which resident store the candidate targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryTarget {
    Global,
    Project,
}

impl From<MemoryTarget> for zuno_types::MemoryScope {
    fn from(target: MemoryTarget) -> Self {
        match target {
            MemoryTarget::Global => Self::Global,
            MemoryTarget::Project => Self::Project,
        }
    }
}

/// Candidate mutation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryAction {
    Add,
    Replace,
    Remove,
}

impl From<MemoryAction> for zuno_types::MemoryAction {
    fn from(action: MemoryAction) -> Self {
        match action {
            MemoryAction::Add => Self::Add,
            MemoryAction::Replace => Self::Replace,
            MemoryAction::Remove => Self::Remove,
        }
    }
}

/// Arguments for one durable candidate.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryParams {
    /// `global` for cross-project preferences, `project` for repository rules.
    pub target: MemoryTarget,
    /// Add, replace, or remove one resident entry.
    pub action: MemoryAction,
    /// New full entry text. Required for add and replace.
    #[serde(default)]
    pub content: Option<String>,
    /// Unique substring locating an existing entry. Required for replace and remove.
    #[serde(default)]
    pub old_text: Option<String>,
    /// Why this fact is durable and reusable.
    pub reason: String,
    /// Confidence from 0 to 1.
    #[schemars(range(min = 0.0, max = 1.0))]
    pub confidence: f64,
}

/// Candidate-producing tool used by foreground turns and isolated reflection.
#[derive(Clone)]
pub struct MemoryTool {
    service: Arc<MemoryService>,
    source: MemorySource,
}

impl MemoryTool {
    #[must_use]
    pub fn new(service: Arc<MemoryService>) -> Self {
        Self {
            service,
            source: MemorySource::Tool,
        }
    }

    #[must_use]
    pub fn reflection(service: Arc<MemoryService>) -> Self {
        Self {
            service,
            source: MemorySource::Reflection,
        }
    }

    #[must_use]
    pub fn configured(enabled: bool, service: Arc<MemoryService>) -> Option<Self> {
        enabled.then(|| Self::new(service))
    }
}

#[async_trait]
impl TypedTool for MemoryTool {
    type Params = MemoryParams;

    fn id(&self) -> &str {
        MEMORY_TOOL_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    async fn run(&self, params: MemoryParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let candidate = self
            .service
            .propose(MemoryProposal {
                scope: params.target.into(),
                action: params.action.into(),
                content: params.content,
                old_text: params.old_text,
                reason: params.reason,
                confidence: params.confidence,
                source: self.source,
                source_session_id: Some(ctx.session_id),
                source_message_id: Some(ctx.message_id),
            })
            .map_err(|source| {
                if source.is_model_correctable() {
                    ToolError::InvalidArgs {
                        tool: MEMORY_TOOL_ID.to_owned(),
                        source: Box::new(source),
                    }
                } else {
                    ToolError::Failed {
                        tool: MEMORY_TOOL_ID.to_owned(),
                        source: Box::new(source),
                    }
                }
            })?;
        let proposal = candidate.projection;
        Ok(ToolOutput::text(
            format!("Memory candidate {}", proposal.id),
            format!(
                "{} {} candidate is {}; review it with /memory",
                proposal.scope.as_str(),
                proposal.action.as_str(),
                proposal.status.as_str()
            ),
        )
        .with_metadata(
            "memory_candidate",
            json!({
                "id": proposal.id,
                "target": proposal.scope.as_str(),
                "action": proposal.action.as_str(),
                "status": proposal.status.as_str(),
                "confidence": proposal.confidence,
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zuno_memory::{PromotionPolicy, ScopeLimits, ScopePaths};
    use zuno_tool::{AllowAll, NeverInterrupted, erase};

    fn service(directory: &TempDir) -> Arc<MemoryService> {
        let pool =
            Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
        let mut connection = pool.open_connection().expect("database connection");
        zuno_db::migration::apply(&mut connection).expect("initialize schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                 VALUES ('project', '/tmp/project', 1, 1, '[]');
                 INSERT INTO session (
                     id, project_id, slug, directory, title, version, time_created, time_updated
                 ) VALUES (
                     'session', 'project', 'memory-tool', '/tmp/project',
                     'Memory tool', '1', 1, 1
                 );",
            )
            .expect("seed tool session");
        drop(connection);
        Arc::new(MemoryService::new(
            pool,
            ScopePaths::at(
                directory.path().join("MEMORY.md"),
                directory.path().join("RULES.md"),
            ),
            ScopeLimits::default(),
            PromotionPolicy::Review,
        ))
    }

    fn context() -> ToolContext {
        ToolContext::new(
            "session",
            "message",
            "call",
            "build",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    #[tokio::test]
    async fn proposal_is_pending_and_does_not_write_resident_memory() {
        let directory = TempDir::new().expect("temp dir");
        let service = service(&directory);
        let tool = erase(MemoryTool::new(Arc::clone(&service)));
        let output = tool
            .execute(
                json!({
                    "target": "project",
                    "action": "add",
                    "content": "run cargo test",
                    "reason": "repository gate",
                    "confidence": 0.95
                }),
                context(),
            )
            .await
            .expect("proposal");
        assert!(output.output.contains("pending"));
        assert!(!directory.path().join("RULES.md").exists());
        assert_eq!(service.candidates().expect("candidates").len(), 1);
    }

    #[test]
    fn definition_is_never_replayable() {
        let directory = TempDir::new().expect("temp dir");
        let tool = erase(MemoryTool::new(service(&directory)));
        assert_eq!(tool.id(), MEMORY_TOOL_ID);
        assert_eq!(tool.replay_policy(), ToolReplayPolicy::Never);
    }
}
