//! Internal learning execution identity, separate from foreground Job authority.
use serde::{Deserialize, Serialize};
use zuno_types::identity::{JobId, PrincipalKey, WorkerInstanceId};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LearningExecutionLease {
    pub owner: PrincipalKey,
    pub job_id: JobId,
    pub worker: WorkerInstanceId,
    pub token: String,
    pub epoch: u64,
    pub expires_at_ms: i64,
}

impl std::fmt::Debug for LearningExecutionLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LearningExecutionLease")
            .field("owner", &self.owner)
            .field("job_id", &self.job_id)
            .field("worker", &self.worker)
            .field("epoch", &self.epoch)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

impl LearningExecutionLease {
    pub fn validate(&self) -> Result<(), crate::ApplicationError> {
        if self.epoch == 0
            || self.expires_at_ms < 0
            || self.token.len() != 64
            || !self
                .token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(crate::ApplicationError::Invalid(
                "invalid learning execution lease".to_owned(),
            ));
        }
        Ok(())
    }
}
