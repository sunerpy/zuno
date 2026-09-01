use std::sync::Arc;
use zuno_db::experience::ExperienceStore;
use zuno_db::feedback::FeedbackStore;
use zuno_db::learning_pattern::LearningPatternStore;
use zuno_db::skill_candidate::SkillCandidateStore;
use zuno_error::DbError;
use zuno_types::LearningStateProjection;

const EXPERIENCE_LIMIT: usize = 100;
const PATTERN_LIMIT: usize = 50;
const SKILL_CANDIDATE_LIMIT: usize = 50;

/// Frontend-neutral reader for the durable learning state.
///
/// Projection remains available when extraction is disabled or its model cannot
/// start. TUI, Server, and ACP therefore read the same durable rows instead of
/// coupling visibility to the background learning runtime.
#[derive(Clone)]
pub struct LearningProjectionService {
    feedback: FeedbackStore,
    experiences: ExperienceStore,
    patterns: LearningPatternStore,
    skills: SkillCandidateStore,
}

impl LearningProjectionService {
    #[must_use]
    pub fn new(pool: Arc<zuno_db::Pool>) -> Self {
        Self {
            feedback: FeedbackStore::new(Arc::clone(&pool)),
            experiences: ExperienceStore::new(Arc::clone(&pool)),
            patterns: LearningPatternStore::new(Arc::clone(&pool)),
            skills: SkillCandidateStore::new(pool),
        }
    }

    pub fn snapshot(
        &self,
        session_id: &str,
        project_id: &str,
    ) -> Result<LearningStateProjection, DbError> {
        Ok(LearningStateProjection {
            feedback: self.feedback.list_for_session(session_id)?,
            experiences: self
                .experiences
                .list_for_project(project_id, EXPERIENCE_LIMIT)?
                .into_iter()
                .map(|record| record.projection)
                .collect(),
            patterns: self
                .patterns
                .list_visible(project_id, PATTERN_LIMIT)?
                .into_iter()
                .map(|record| record.projection)
                .collect(),
            skill_candidates: self
                .skills
                .list_for_project(project_id, SKILL_CANDIDATE_LIMIT)?
                .into_iter()
                .map(|record| record.projection)
                .collect(),
        })
    }
}
