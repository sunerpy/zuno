//! Authenticated control of logical work, independent of Worker execution leases.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_types::identity::{JobId, OperationId, PrincipalScope, RequestId, TurnId};

use crate::ApplicationError;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelJob {
    pub request_id: RequestId,
    pub expected_turn_id: TurnId,
    pub reason: String,
}

impl CancelJob {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.reason.trim().is_empty()
            || self.reason.len() > 1024
            || self.reason.chars().any(char::is_control)
        {
            return Err(ApplicationError::Invalid(
                "cancellation requires a bounded reason".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The logical tree is fenced when this receipt commits. External operations
/// have their own observed receipts; this does not claim their processes stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancellationReceipt {
    pub request_id: RequestId,
    pub job_id: JobId,
    pub turn_id: TurnId,
    pub stopped_jobs: Vec<JobId>,
    pub pending_operations: Vec<OperationId>,
}

#[async_trait]
pub trait RuntimeControl: Send + Sync {
    async fn cancel(
        &self,
        principal: &PrincipalScope,
        job: &JobId,
        request: CancelJob,
    ) -> Result<CancellationReceipt, ApplicationError>;
}
