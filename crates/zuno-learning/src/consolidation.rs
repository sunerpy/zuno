use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_types::{ExperienceProjection, LearningPatternProjection};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationScope {
    Project,
    Global,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsolidationRequest {
    pub scope: ConsolidationScope,
    pub project_id: String,
    pub session_id: String,
    pub experiences: Vec<ExperienceProjection>,
    pub patterns: Vec<LearningPatternProjection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Consolidation {
    pub groups: Vec<ConsolidatedPattern>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConsolidatedPattern {
    pub existing_pattern_id: Option<String>,
    pub title: String,
    pub learned_rules: Vec<String>,
    pub evidence_ids: Vec<String>,
}

#[async_trait]
pub trait PatternConsolidator: Send + Sync {
    async fn consolidate(&self, request: ConsolidationRequest) -> crate::Result<Consolidation>;
}

#[async_trait]
impl PatternConsolidator for crate::LearningModelClient {
    async fn consolidate(&self, mut request: ConsolidationRequest) -> crate::Result<Consolidation> {
        let budget = (self.limits.execution_max_input_bytes as usize).saturating_sub(16_384);
        while serde_json::to_vec(&request).expect("input").len() > budget {
            if request.experiences.len() > 2 {
                request.experiences.pop();
            } else if !request.patterns.is_empty() {
                request.patterns.pop();
            } else {
                return Err(crate::model::invalid(
                    "consolidation input exceeds its budget",
                ));
            }
        }
        let prompt = if request.scope == ConsolidationScope::Global {
            "Consolidate promoted project patterns that express the same reusable rule even when phrased differently.              Cite supplied project pattern IDs as evidence_ids, drawing from independent projects.              Only reuse an existing_pattern_id from a global pattern. Never treat unreviewed project rules as approved.              Preserve unchanged rules and do not combine contradictory or project-specific requirements.              At most 32 groups and 15 rules per group. Empty groups are valid."
        } else {
            "Consolidate verified experiences that express the same reusable procedure or rule, \
             even when phrased differently. Do not group contradictory facts or unrelated tasks. \
             Cite at least two supplied experience IDs for each group. Reuse existing_pattern_id \
             for the same existing concept and preserve its unchanged rules. Do not reopen a \
             rejected pattern without new supporting evidence. Never invent facts, IDs or success. \
             At most 32 groups and 15 concise rules per group. Empty groups are valid."
        };
        self.json(
            &request.session_id,
            "learning.consolidation",
            prompt,
            serde_json::to_value(&request).expect("input"),
        )
        .await
    }
}
