//! `memory_update` records an auditable resident-memory candidate.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use zuno_error::ToolError;
use zuno_memory::{MemoryProposal, MemoryService};
use zuno_tool::{ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy, TypedTool};
use zuno_types::MemorySource;

/// The only model-visible memory mutation entry point.
pub const MEMORY_TOOL_ID: &str = "memory_update";

/// Prompt-visible guidance for candidate creation.
pub const DESCRIPTION: &str = include_str!("description/memory-update.txt");

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
    /// Scope revision from memory_read or the current prompt. A replace/remove
    /// without this field must identify the exact full old entry, not a substring.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub expected_revision: Option<i64>,
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
    fn presentation(&self) -> zuno_types::activity::InvocationPresentation {
        zuno_types::activity::InvocationPresentation::builtin(
            zuno_types::activity::InvocationAction::MemoryWrite,
        )
    }

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

    fn effect(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ManagedMemory
    }

    async fn run(&self, params: MemoryParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if params
            .expected_revision
            .is_some_and(|revision| revision < 1)
        {
            return Err(ToolError::InvalidArgs {
                tool: MEMORY_TOOL_ID.to_owned(),
                source: Box::new(std::io::Error::other(
                    "expected_revision must be a positive memory revision",
                )),
            });
        }
        let origin = ctx.permission_origin();
        let session_id = origin.session_id().to_owned();
        let message_id = origin.message_id().to_owned();
        let candidate = self
            .service
            .update_from_model(
                MemoryProposal {
                    scope: params.target.into(),
                    action: params.action.into(),
                    content: params.content,
                    old_text: params.old_text,
                    reason: params.reason,
                    confidence: params.confidence,
                    source: self.source,
                    source_session_id: Some(session_id.clone()),
                    source_message_id: Some(message_id),
                },
                params.expected_revision,
                &session_id,
            )
            .map_err(|source| {
                if matches!(&source, zuno_memory::MemoryServiceError::Denied) {
                    ToolError::Denied {
                        tool: MEMORY_TOOL_ID.to_owned(),
                        denial: None,
                    }
                } else if source.is_model_correctable() {
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
        let revision = (candidate.projection.status == zuno_types::MemoryCandidateStatus::Applied)
            .then(|| {
                candidate.base_revision.map(|revision| {
                    revision + i64::from(candidate.before_entries != candidate.after_entries)
                })
            })
            .flatten();
        let proposal = candidate.projection;
        let applied = proposal.status == zuno_types::MemoryCandidateStatus::Applied;
        Ok(ToolOutput::text(
            format!(
                "Memory {} {}",
                if applied { "updated" } else { "change" },
                proposal.id
            ),
            if applied {
                format!(
                    "{} {} applied at revision {}; later memory snapshots will use it.",
                    proposal.scope.as_str(),
                    proposal.action.as_str(),
                    revision.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                )
            } else {
                format!(
                    "{} {} is {}; the configured review policy requires /memory.",
                    proposal.scope.as_str(),
                    proposal.action.as_str(),
                    proposal.status.as_str(),
                )
            },
        )
        .with_metadata(
            "memory_candidate",
            json!({
                "id": proposal.id,
                "target": proposal.scope.as_str(),
                "action": proposal.action.as_str(),
                "status": proposal.status.as_str(),
                "confidence": proposal.confidence,
                "revision": revision,
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

    #[tokio::test]
    async fn memory_reads_and_writes_keep_the_immutable_call_origin() {
        let directory = TempDir::new().unwrap();
        let service = service(&directory);
        let mut forged = context();
        forged.session_id = "another-session".to_owned();
        forged.message_id = "another-message".to_owned();
        erase(MemoryTool::new(service.clone()))
            .execute(
                json!({"target":"project","action":"add","content":"Keep original attribution.",
                "reason":"Verified convention","confidence":1.0}),
                forged.clone(),
            )
            .await
            .unwrap();
        let candidates = service.candidates().unwrap();
        assert_eq!(candidates[0].source_session_id.as_deref(), Some("session"));
        assert_eq!(candidates[0].source_message_id.as_deref(), Some("message"));
        erase(crate::MemoryReadTool::new(service))
            .execute(json!({}), forged)
            .await
            .unwrap();
    }

    #[test]
    fn definition_is_never_replayable() {
        let directory = TempDir::new().expect("temp dir");
        let tool = erase(MemoryTool::new(service(&directory)));
        assert_eq!(tool.id(), MEMORY_TOOL_ID);
        assert_eq!(tool.replay_policy(), ToolReplayPolicy::Never);
    }
}
