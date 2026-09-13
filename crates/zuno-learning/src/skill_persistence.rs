//! Skill candidate persistence is bound by the host to one owner and backend.
use std::sync::Arc;
use zuno_db::skill_candidate::{NewSkillCandidate, SkillCandidateRecord, SkillCandidateStore};
use zuno_db::{
    experience::{ExperienceRecord, ExperienceStore},
    learning_job::{LearningJobRecord, LearningJobStore},
    learning_pattern::{LearningPatternRecord, LearningPatternStore},
    learning_source::{LearningSource, LearningSourceStore},
};
use zuno_error::DbError;
use zuno_types::SkillCandidateStatus;

pub trait SkillEvidencePersistence: Send + Sync {
    fn pattern(&self, id: &str) -> Result<LearningPatternRecord, DbError>;
    fn promote_pattern(&self, id: &str, at: i64) -> Result<LearningPatternRecord, DbError>;
    fn experience(&self, id: &str) -> Result<ExperienceRecord, DbError>;
    fn experiences_for_project(
        &self,
        project: &str,
        limit: usize,
    ) -> Result<Vec<ExperienceRecord>, DbError>;
    fn job(&self, id: &str) -> Result<LearningJobRecord, DbError>;
    fn source_is_current(&self, session: &str, source: &LearningSource) -> Result<bool, DbError>;
}

/// A host returns a coordinated set of persistence providers. State-changing
/// methods retain their atomic contract; callers cannot swap one SQLite store
/// while accidentally retaining another store from a different backend.
pub trait SkillBackendBundle: Send + Sync {
    fn candidates(&self) -> Arc<dyn SkillCandidatePersistence>;
    fn evidence(&self) -> Arc<dyn SkillEvidencePersistence>;
    fn evaluation(&self) -> Arc<dyn zuno_eval::persistence::EvaluationPersistence>;
}
pub struct SqliteSkillBackend {
    pool: Arc<zuno_db::Pool>,
}
impl SqliteSkillBackend {
    pub fn new(pool: Arc<zuno_db::Pool>) -> Self {
        Self { pool }
    }
}
impl SkillBackendBundle for SqliteSkillBackend {
    fn candidates(&self) -> Arc<dyn SkillCandidatePersistence> {
        Arc::new(SqliteSkillCandidatePersistence::new(self.pool.clone()))
    }
    fn evidence(&self) -> Arc<dyn SkillEvidencePersistence> {
        Arc::new(SqliteSkillEvidence {
            patterns: LearningPatternStore::new(self.pool.clone()),
            experiences: ExperienceStore::new(self.pool.clone()),
            jobs: LearningJobStore::new(self.pool.clone()),
            sources: LearningSourceStore::new(self.pool.clone()),
        })
    }
    fn evaluation(&self) -> Arc<dyn zuno_eval::persistence::EvaluationPersistence> {
        Arc::new(zuno_eval::persistence::SqliteEvaluationPersistence::new(
            self.pool.clone(),
        ))
    }
}
struct SqliteSkillEvidence {
    patterns: LearningPatternStore,
    experiences: ExperienceStore,
    jobs: LearningJobStore,
    sources: LearningSourceStore,
}
impl SkillEvidencePersistence for SqliteSkillEvidence {
    fn pattern(&self, id: &str) -> Result<LearningPatternRecord, DbError> {
        self.patterns.get(id)
    }
    fn promote_pattern(&self, id: &str, at: i64) -> Result<LearningPatternRecord, DbError> {
        self.patterns.promote(id, at)
    }
    fn experience(&self, id: &str) -> Result<ExperienceRecord, DbError> {
        self.experiences.get(id)
    }
    fn experiences_for_project(
        &self,
        project: &str,
        limit: usize,
    ) -> Result<Vec<ExperienceRecord>, DbError> {
        self.experiences.list_for_project(project, limit)
    }
    fn job(&self, id: &str) -> Result<LearningJobRecord, DbError> {
        self.jobs.get(id)
    }
    fn source_is_current(&self, session: &str, source: &LearningSource) -> Result<bool, DbError> {
        self.sources.source_is_current(session, source)
    }
}

pub trait SkillCandidatePersistence: Send + Sync {
    fn create(&self, candidate: NewSkillCandidate) -> Result<SkillCandidateRecord, DbError>;
    fn begin_evaluation(
        &self,
        id: &str,
        now: i64,
        expires: i64,
    ) -> Result<(SkillCandidateRecord, String), DbError>;
    fn settle_evaluation(
        &self,
        id: &str,
        lease_token: &str,
        run_id: &str,
        passed: bool,
        error: Option<&str>,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError>;
    fn fail_evaluation(
        &self,
        id: &str,
        lease_token: &str,
        error: &str,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError>;
    fn begin_apply(
        &self,
        id: &str,
        operation_id: &str,
        before_content: &str,
        after_content: &str,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError>;
    fn begin_undo(&self, id: &str, now: i64) -> Result<SkillCandidateRecord, DbError>;
    fn finish_effect(
        &self,
        id: &str,
        expected_status: SkillCandidateStatus,
        status: SkillCandidateStatus,
        error: Option<&str>,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError>;
    fn reject(&self, id: &str, now: i64) -> Result<SkillCandidateRecord, DbError>;
    fn mark_stale(&self, id: &str, error: &str, now: i64) -> Result<SkillCandidateRecord, DbError>;
    fn get(&self, id: &str) -> Result<SkillCandidateRecord, DbError>;
    fn list_for_project(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<SkillCandidateRecord>, DbError>;
    fn list_referencing(
        &self,
        experience_ids: &[String],
    ) -> Result<Vec<SkillCandidateRecord>, DbError>;
    fn list_inflight(&self) -> Result<Vec<SkillCandidateRecord>, DbError>;
    fn fail_interrupted_evaluations(&self, now: i64) -> Result<usize, DbError>;
}
pub struct SqliteSkillCandidatePersistence {
    store: SkillCandidateStore,
}
impl SqliteSkillCandidatePersistence {
    pub fn new(pool: Arc<zuno_db::Pool>) -> Self {
        Self {
            store: SkillCandidateStore::new(pool),
        }
    }
}
impl SkillCandidatePersistence for SqliteSkillCandidatePersistence {
    fn create(&self, candidate: NewSkillCandidate) -> Result<SkillCandidateRecord, DbError> {
        self.store.create(candidate)
    }
    fn begin_evaluation(
        &self,
        id: &str,
        now: i64,
        expires: i64,
    ) -> Result<(SkillCandidateRecord, String), DbError> {
        self.store.begin_evaluation(id, now, expires)
    }
    fn settle_evaluation(
        &self,
        id: &str,
        lease_token: &str,
        run_id: &str,
        passed: bool,
        error: Option<&str>,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError> {
        self.store
            .settle_evaluation(id, lease_token, run_id, passed, error, now)
    }
    fn fail_evaluation(
        &self,
        id: &str,
        lease_token: &str,
        error: &str,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError> {
        self.store.fail_evaluation(id, lease_token, error, now)
    }
    fn begin_apply(
        &self,
        id: &str,
        operation_id: &str,
        before_content: &str,
        after_content: &str,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError> {
        self.store
            .begin_apply(id, operation_id, before_content, after_content, now)
    }
    fn begin_undo(&self, id: &str, now: i64) -> Result<SkillCandidateRecord, DbError> {
        self.store.begin_undo(id, now)
    }
    fn finish_effect(
        &self,
        id: &str,
        expected_status: SkillCandidateStatus,
        status: SkillCandidateStatus,
        error: Option<&str>,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError> {
        self.store
            .finish_effect(id, expected_status, status, error, now)
    }
    fn reject(&self, id: &str, now: i64) -> Result<SkillCandidateRecord, DbError> {
        self.store.reject(id, now)
    }
    fn mark_stale(&self, id: &str, error: &str, now: i64) -> Result<SkillCandidateRecord, DbError> {
        self.store.mark_stale(id, error, now)
    }
    fn get(&self, id: &str) -> Result<SkillCandidateRecord, DbError> {
        self.store.get(id)
    }
    fn list_for_project(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<SkillCandidateRecord>, DbError> {
        self.store.list_for_project(project_id, limit)
    }
    fn list_referencing(
        &self,
        experience_ids: &[String],
    ) -> Result<Vec<SkillCandidateRecord>, DbError> {
        self.store.list_referencing(experience_ids)
    }
    fn list_inflight(&self) -> Result<Vec<SkillCandidateRecord>, DbError> {
        self.store.list_inflight()
    }
    fn fail_interrupted_evaluations(&self, now: i64) -> Result<usize, DbError> {
        self.store.fail_interrupted_evaluations(now)
    }
}
