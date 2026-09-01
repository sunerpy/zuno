//! Typed failures for user-experience learning and reviewed Skill changes.

use crate::recovery::{Recoverable, Recovery};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum LearningError {
    #[error("learning request field `{field}` is invalid")]
    InvalidRequest { field: String, detail: String },

    #[error(
        "feedback for message `{message_id}` changed from revision {expected_revision} to {current_revision}"
    )]
    FeedbackRevisionConflict {
        message_id: String,
        expected_revision: i64,
        current_revision: i64,
    },

    #[error("Skill candidate `{candidate_id}` requires explicit human approval")]
    SkillReviewRequired { candidate_id: String },

    #[error("Skill candidate `{candidate_id}` no longer matches its source")]
    SkillSourceDrift {
        candidate_id: String,
        expected_digest: String,
        observed_digest: String,
    },

    #[error("Skill candidate `{candidate_id}` did not pass its evaluation suite")]
    EvaluationRejected { candidate_id: String },

    #[error("learning operation `{operation}` could not access {path}")]
    Io {
        operation: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl LearningError {
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }
}

impl Recoverable for LearningError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::InvalidRequest { .. }
            | Self::FeedbackRevisionConflict { .. }
            | Self::SkillReviewRequired { .. }
            | Self::SkillSourceDrift { .. }
            | Self::EvaluationRejected { .. }
            | Self::Io { .. } => Recovery::Fail,
        }
    }
}
