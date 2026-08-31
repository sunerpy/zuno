//! Explicit deep search over durable project experiences.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use zuno_error::ToolError;
use zuno_learning::ExperienceRetriever;
use zuno_tool::{ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy, TypedTool};

pub const WIRE_ID: &str = "experience_search";
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;

pub const DESCRIPTION: &str = "Search durable project experience records with SQLite full-text \
search. Use this for deeper recall than the small project-experience section already injected \
into the prompt. Unresolved issues are returned as open issues, never as verified guidance.";

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperienceSearchParams {
    /// Full-text query over titles, observations, resolutions, and evidence.
    pub query: String,
    /// Maximum records to return (default 20, maximum 50).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Clone)]
pub struct ExperienceSearchTool {
    retriever: ExperienceRetriever,
    project_id: String,
}

impl ExperienceSearchTool {
    #[must_use]
    pub fn new(retriever: ExperienceRetriever, project_id: impl Into<String>) -> Self {
        Self {
            retriever,
            project_id: project_id.into(),
        }
    }
}

#[async_trait]
impl TypedTool for ExperienceSearchTool {
    type Params = ExperienceSearchParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn run(
        &self,
        params: ExperienceSearchParams,
        _ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let query = params.query.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: WIRE_ID.to_owned(),
                source: Box::new(std::io::Error::other(
                    "experience_search.query must not be empty",
                )),
            });
        }
        let limit = usize::try_from(params.limit.unwrap_or(DEFAULT_LIMIT as u32))
            .unwrap_or(MAX_LIMIT)
            .clamp(1, MAX_LIMIT);
        let records = self
            .retriever
            .search(&self.project_id, query, limit)
            .map_err(|source| ToolError::Failed {
                tool: WIRE_ID.to_owned(),
                source: Box::new(source),
            })?;
        let output = records
            .into_iter()
            .map(|record| {
                let experience = record.projection;
                json!({
                    "id": experience.id,
                    "kind": experience.kind.as_str(),
                    "title": experience.title,
                    "summary": experience.summary,
                    "resolution": experience.resolution,
                    "status": experience.status.as_str(),
                    "confidence": experience.confidence,
                    "sessionID": experience.session_id,
                    "sourceMessageID": experience.source_message_id,
                    "timeUpdated": experience.time_updated,
                })
            })
            .collect::<Vec<_>>();
        Ok(ToolOutput::text(
            format!("experience search: {query}"),
            serde_json::to_string_pretty(&output)
                .expect("experience search output is serializable"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use zuno_config::ResolvedLearningConfig;
    use zuno_db::experience::{
        ExperienceEvidenceKind, ExperienceStore, NewExperience, NewExperienceEvidence,
    };
    use zuno_db::migration;
    use zuno_paths::DbLocation;
    use zuno_tool::{AllowAll, NeverInterrupted, ToolContext, erase};
    use zuno_types::ExperienceKind;

    #[tokio::test]
    async fn search_is_read_only_replay_safe_and_project_scoped() {
        let pool = Arc::new(zuno_db::Pool::open(&DbLocation::Memory).expect("pool"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection
                .execute_batch(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('project-1', '/workspace', 1, 1, '[]'),
                            ('project-2', '/other', 1, 1, '[]');",
                )
                .expect("projects");
        }
        for (id, project_id) in [("experience-1", "project-1"), ("experience-2", "project-2")] {
            ExperienceStore::new(pool.clone())
                .create_manual(NewExperience {
                    id: id.to_owned(),
                    project_id: project_id.to_owned(),
                    session_id: None,
                    source_message_id: None,
                    extraction_job_id: None,
                    extraction_ordinal: None,
                    kind: ExperienceKind::Procedure,
                    title: "Cargo gate".to_owned(),
                    summary: "Run cargo check before publishing.".to_owned(),
                    resolution: Some("cargo check --workspace".to_owned()),
                    confidence: 10_000,
                    fingerprint: format!("fingerprint-{id}"),
                    evidence: vec![NewExperienceEvidence {
                        id: format!("evidence-{id}"),
                        kind: ExperienceEvidenceKind::User,
                        source_id: None,
                        excerpt: "cargo check".to_owned(),
                        digest: format!("digest-{id}"),
                    }],
                    time_created: 1,
                })
                .expect("experience");
        }
        let tool = erase(ExperienceSearchTool::new(
            ExperienceRetriever::new(pool, &ResolvedLearningConfig::default()),
            "project-1",
        ));
        assert_eq!(tool.replay_policy(), ToolReplayPolicy::Safe);
        let output = tool
            .execute(
                json!({"query":"cargo","limit":50}),
                ToolContext::new(
                    "session",
                    "message",
                    "call",
                    "build",
                    Arc::new(AllowAll),
                    Arc::new(NeverInterrupted),
                ),
            )
            .await
            .expect("search");
        assert!(output.output.contains("experience-1"));
        assert!(!output.output.contains("experience-2"));
    }
}
