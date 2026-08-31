//! User-learning flywheel for Zuno.
//!
//! Fast extraction records concrete experiences. Slow aggregation proposes
//! patterns and complete Skill candidates. Only resident project memories may
//! auto-promote at high confidence; Skill changes always pass explicit review,
//! offline evaluation, and source-digest CAS.

mod experience;
mod extraction;
mod feedback;
mod pattern;
mod projection;
mod retrieval;
mod scheduler;
mod skill;

pub use crate::experience::{
    ExperienceService, ExtractionPersistence, ManualExperienceRequest, MemoryPromotionResult,
    SessionExperienceCleanup,
};
pub use crate::extraction::{
    ExtractedEvidence, ExtractedEvidenceKind, ExtractedExperience, ExtractedExperienceKind,
    ExtractedMemory, ExtractedMemoryAction, ExtractedMemoryScope, ExtractionRequest,
    LearningExtraction, LearningExtractor,
};
pub use crate::feedback::FeedbackService;
pub use crate::pattern::PatternMiner;
pub use crate::projection::LearningProjectionService;
pub use crate::retrieval::{ExperienceRetriever, RetrievedExperiences};
pub use crate::scheduler::{CompletedTaskSignals, LearningScheduleOutcome, LearningScheduler};
pub use crate::skill::{
    SkillCandidateRequest, SkillCandidateService, SkillCleanupPreparation, SkillSourceResolver,
    SkillTarget,
};
pub use zuno_eval::EvaluationService;

use zuno_error::{BoxSource, DbError, LearningError};
use zuno_eval::EvaluationError;
use zuno_memory::MemoryServiceError;

#[derive(Debug, thiserror::Error)]
pub enum LearningServiceError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Learning(#[from] LearningError),
    #[error(transparent)]
    Memory(#[from] MemoryServiceError),
    #[error(transparent)]
    Evaluation(#[from] EvaluationError),
    #[error("learning extractor `{version}` failed")]
    Extractor {
        version: String,
        #[source]
        source: BoxSource,
    },
}

pub type Result<T> = std::result::Result<T, LearningServiceError>;

pub(crate) fn digest_text(text: &str) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))
}
