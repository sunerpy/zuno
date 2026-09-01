use crate::{Result, digest_text};
use std::sync::Arc;
use zuno_config::ResolvedLearningConfig;
use zuno_db::experience::{ExperienceRecord, ExperienceStore};
use zuno_types::ExperienceKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievedExperiences {
    pub items: Vec<ExperienceRecord>,
    pub content: String,
    pub source: String,
    pub digest: String,
    pub estimated_tokens: u32,
}

#[derive(Clone)]
pub struct ExperienceRetriever {
    store: ExperienceStore,
    max_items: usize,
    max_context_tokens: u32,
}

impl ExperienceRetriever {
    #[must_use]
    pub fn new(pool: Arc<zuno_db::Pool>, config: &ResolvedLearningConfig) -> Self {
        Self {
            store: ExperienceStore::new(pool),
            max_items: config.retrieval_max_items as usize,
            max_context_tokens: config.retrieval_max_context_tokens,
        }
    }

    /// Retrieve project-local experiences first and keep the rendered section
    /// inside the configured prompt budget.
    pub fn retrieve(&self, project_id: &str, query: &str) -> Result<RetrievedExperiences> {
        let candidates = if query.trim().is_empty() {
            self.store.list_for_project(project_id, self.max_items)?
        } else {
            self.store.search(project_id, query, self.max_items)?
        };
        let mut items = Vec::new();
        let mut blocks = Vec::new();
        let mut used_tokens = 0_u32;
        for record in candidates {
            let block = render(&record);
            let tokens = estimate_tokens(&block);
            if used_tokens.saturating_add(tokens) > self.max_context_tokens {
                continue;
            }
            used_tokens = used_tokens.saturating_add(tokens);
            blocks.push(block);
            items.push(record);
        }
        let content = blocks.join("\n\n");
        let ids = items
            .iter()
            .map(|item| item.projection.id.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let source = format!("learning://project/{project_id}/experiences?ids={ids}");
        Ok(RetrievedExperiences {
            digest: digest_text(&content),
            items,
            content,
            source,
            estimated_tokens: used_tokens,
        })
    }

    /// Explicit deep search uses the same SQLite FTS provider but a caller-owned limit.
    pub fn search(
        &self,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ExperienceRecord>> {
        self.store
            .search(project_id, query, limit)
            .map_err(Into::into)
    }
}

fn render(record: &ExperienceRecord) -> String {
    let projection = &record.projection;
    let resolution = projection.resolution.as_deref().map_or_else(
        || "Resolution: none recorded".to_owned(),
        |resolution| format!("Resolution: {resolution}"),
    );
    let warning = if projection.kind == ExperienceKind::UnresolvedIssue {
        "\nStatus: UNRESOLVED. Treat this only as an open issue, never as guidance."
    } else {
        ""
    };
    format!(
        "<experience id=\"{}\" kind=\"{}\">\nTitle: {}\nObservation: {}\n{}{}\n</experience>",
        projection.id,
        projection.kind.as_str(),
        projection.title,
        projection.summary,
        resolution,
        warning,
    )
}

fn estimate_tokens(value: &str) -> u32 {
    let characters = value.chars().count();
    u32::try_from(characters.div_ceil(4)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_config::ResolvedLearningConfig;
    use zuno_db::experience::{ExperienceEvidenceKind, NewExperience, NewExperienceEvidence};
    use zuno_db::migration;
    use zuno_paths::DbLocation;

    #[test]
    fn unresolved_records_are_labeled_and_budgeted() {
        let pool = Arc::new(zuno_db::Pool::open(&DbLocation::Memory).expect("pool"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection
                .execute_batch(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('project-1', '/workspace', 1, 1, '[]');",
                )
                .expect("project");
        }
        ExperienceStore::new(pool.clone())
            .create_manual(NewExperience {
                id: "experience-1".to_owned(),
                project_id: "project-1".to_owned(),
                session_id: None,
                source_message_id: None,
                extraction_job_id: None,
                extraction_ordinal: None,
                kind: ExperienceKind::UnresolvedIssue,
                title: "Intermittent timeout".to_owned(),
                summary: "The cause is not known yet.".to_owned(),
                resolution: None,
                confidence: 9000,
                fingerprint: "fingerprint".to_owned(),
                evidence: vec![NewExperienceEvidence {
                    id: "evidence-1".to_owned(),
                    kind: ExperienceEvidenceKind::User,
                    source_id: None,
                    excerpt: "not known".to_owned(),
                    digest: "digest".to_owned(),
                }],
                time_created: 1,
            })
            .expect("experience");
        let config = ResolvedLearningConfig {
            retrieval_max_context_tokens: 1_200,
            ..ResolvedLearningConfig::default()
        };
        let retrieved = ExperienceRetriever::new(pool, &config)
            .retrieve("project-1", "timeout")
            .expect("retrieve");
        assert!(retrieved.content.contains("UNRESOLVED"));
        assert!(retrieved.estimated_tokens <= 1_200);
        assert_eq!(retrieved.items.len(), 1);
    }
}
