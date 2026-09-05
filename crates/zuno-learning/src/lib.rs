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
mod text;

pub use crate::experience::{
    ExperienceService, ExtractionPersistence, ManualExperienceRequest, MemoryPromotionResult,
    SessionExperienceCleanup,
};
pub use crate::extraction::{
    ExtractedEvidence, ExtractedEvidenceKind, ExtractedExperience, ExtractedExperienceKind,
    ExtractedMemory, ExtractedMemoryAction, ExtractedMemoryScope, ExtractionJobPayload,
    ExtractionRequest, ExtractionTrigger, LearningExtraction, LearningExtractor,
    decode_extraction_job_payload,
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

use zuno_error::{BoxSource, DbError, LearningError, Recoverable, Recovery};
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
    #[error("learning extractor `{version}` failed: {source}")]
    Extractor {
        version: String,
        #[source]
        source: BoxSource,
    },
}

impl LearningServiceError {
    /// What a caller should do next, decided from this error's shape alone.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }
}

/// The learning workers settle durable job rows from this classification, so it is
/// deliberately conservative: only a failure that is permanent *whatever* the
/// worker does next answers [`Recovery::Fail`]. Anything whose cause is boxed
/// behind a provider or evaluator boundary answers `Retry`, which leaves the job
/// `running` for the lease reconciler instead of settling it as permanently failed.
impl Recoverable for LearningServiceError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Database(error) => Recoverable::recovery(error),
            Self::Learning(error) => Recoverable::recovery(error),
            Self::Memory(MemoryServiceError::Database(error)) => Recoverable::recovery(error),
            Self::Memory(MemoryServiceError::Resident(_) | MemoryServiceError::Invalid(_)) => {
                Recovery::Fail
            }
            Self::Evaluation(EvaluationError::Db(error)) => Recoverable::recovery(error),
            Self::Evaluation(
                EvaluationError::InvalidSnapshot | EvaluationError::EmptySuite { .. },
            ) => Recovery::Fail,
            Self::Evaluation(EvaluationError::Evaluator { .. }) | Self::Extractor { .. } => {
                Recovery::Retry { after: None }
            }
        }
    }
}

pub type Result<T> = std::result::Result<T, LearningServiceError>;

pub(crate) fn digest_text(text: &str) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))
}
