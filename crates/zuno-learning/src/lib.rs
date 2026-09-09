//! User-learning flywheel for Zuno.
//!
//! Fast extraction records concrete experiences. Slow aggregation proposes
//! patterns and complete Skill candidates. Only resident project memories may
//! auto-promote at high confidence; Skill changes always pass explicit review,
//! offline evaluation, and source-digest CAS.

mod consolidation;
mod evaluator;
mod execution;
mod experience;
mod extraction;
mod feedback;
mod ingestion;
mod model;
mod pattern;
mod projection;
mod retrieval;
mod scheduler;
mod skill;
mod supervisor;
mod text;
mod worker;

pub use crate::consolidation::{
    ConsolidatedPattern, Consolidation, ConsolidationRequest, ConsolidationScope,
    PatternConsolidator,
};
pub use crate::evaluator::ProviderSkillEvaluator;
pub use crate::execution::{LearningAttempt, ManualReflectionGuard, run_claimed_extraction};
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
pub use crate::ingestion::LearningIngestion;
pub use crate::model::{LEARNING_EXTRACTOR_VERSION, LearningModel, LearningModelClient};
pub use crate::pattern::PatternMiner;
pub use crate::projection::LearningProjectionService;
pub use crate::retrieval::{ExperienceRetriever, RetrievedExperiences};
pub use crate::scheduler::{CompletedTaskSignals, LearningScheduleOutcome, LearningScheduler};
pub use crate::skill::{
    SkillCandidateRequest, SkillCandidateService, SkillCleanupPreparation, SkillSourceResolver,
    SkillTarget,
};
pub use crate::supervisor::{LearningSupervisor, LearningWork};
pub use crate::worker::ProjectLearningService;
pub use zuno_eval::EvaluationService;

use zuno_error::{BoxSource, DbError, LearningError, ProviderError, Recoverable, Recovery};
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
    #[error("learning extractor `{version}` provider failed: {source}")]
    ExtractorProvider {
        version: String,
        #[source]
        source: ProviderError,
    },
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
            Self::ExtractorProvider { source, .. } => Recoverable::recovery(source),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn provider_failures_keep_their_typed_recovery() {
        let retry_after = Duration::from_secs(37);
        let rate_limit = LearningServiceError::ExtractorProvider {
            version: "v1".to_owned(),
            source: ProviderError::RateLimited {
                retry_after: Some(retry_after),
            },
        };
        assert_eq!(
            rate_limit.recovery(),
            Recovery::Retry {
                after: Some(retry_after)
            }
        );

        let auth = LearningServiceError::ExtractorProvider {
            version: "v1".to_owned(),
            source: ProviderError::Auth {
                provider: "provider".to_owned(),
                source: None,
            },
        };
        assert_eq!(auth.recovery(), Recovery::Reauthenticate);

        let context = LearningServiceError::ExtractorProvider {
            version: "v1".to_owned(),
            source: ProviderError::ContextLimit {
                limit_tokens: Some(10),
                used_tokens: Some(11),
            },
        };
        assert_eq!(context.recovery(), Recovery::Compact);

        let fatal = LearningServiceError::ExtractorProvider {
            version: "v1".to_owned(),
            source: ProviderError::Fatal {
                status: Some(400),
                source: None,
            },
        };
        assert_eq!(fatal.recovery(), Recovery::Fail);
    }
}
