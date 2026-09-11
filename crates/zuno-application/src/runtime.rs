//! Internal durable runtime contracts. These are not Web DTOs.
//!
//! A lease is data, not a credential. The host authenticates workload identities
//! and user scopes before invoking this port. Backends fence every mutation in
//! the same transaction as its checkpoint, attempt and event updates.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use zuno_types::identity::{
    ConfigurationId, ExecutionAttemptId, InputId, JobId, PrincipalKey, PrincipalScope, RequestId,
    SessionId, TurnId, WorkerInstanceId,
};

use crate::ApplicationError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigurationRef {
    pub id: ConfigurationId,
    pub version: u32,
    pub sha256: String,
}

impl ConfigurationRef {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.version == 0
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ApplicationError::Invalid(
                "invalid configuration reference".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Driver-owned reference only. Its body must identify durable state; it cannot
/// carry a Future, file descriptor, credential or process-local cancellation handle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeCheckpoint {
    pub job_id: JobId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub driver: String,
    pub schema_version: u32,
    pub reference: Value,
}

impl RuntimeCheckpoint {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.driver.is_empty()
            || self.driver.len() > 128
            || self.schema_version == 0
            || !self.reference.is_object()
            || serde_json::to_vec(&self.reference)
                .map_err(ApplicationError::storage)?
                .len()
                > 8192
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded checkpoint reference".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    Ready,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobSubmission {
    pub session_id: SessionId,
    pub request_id: RequestId,
    pub expected_input_version: u64,
    pub text: String,
    pub configuration: ConfigurationRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeJob {
    pub id: JobId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub input_id: InputId,
    pub principal: PrincipalScope,
    pub configuration: ConfigurationRef,
    pub phase: JobPhase,
    pub checkpoint: Option<RuntimeCheckpoint>,
    pub checkpoint_version: u64,
    pub input_version: u64,
    pub result: Option<Value>,
}

/// An epoch is session execution authority. Checkpoint/input versions are
/// separate counters and never substitute for an epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionLease {
    /// Ownership routing for the state service; never an authentication grant.
    pub owner: PrincipalKey,
    pub job_id: JobId,
    pub session_id: SessionId,
    pub attempt_id: ExecutionAttemptId,
    pub worker: WorkerInstanceId,
    pub epoch: u64,
    pub checkpoint_version: u64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimedJob {
    pub job: RuntimeJob,
    pub lease: ExecutionLease,
}

/// Configured positive lease lifetime. The database chooses the actual deadline.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct LeaseDuration(u32);

impl LeaseDuration {
    pub fn new(milliseconds: u32) -> Result<Self, ApplicationError> {
        if !(1_000..=300_000).contains(&milliseconds) {
            return Err(ApplicationError::Invalid(
                "lease lifetime must be between 1 and 300 seconds".to_owned(),
            ));
        }
        Ok(Self(milliseconds))
    }
    #[must_use]
    pub const fn milliseconds(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for LeaseDuration {
    type Error = ApplicationError;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<LeaseDuration> for u32 {
    fn from(value: LeaseDuration) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobFinish {
    Completed { result: Value },
    Failed { code: String },
    Cancelled { reason: String },
    Uncertain { reason: String },
}

impl JobFinish {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        match self {
            Self::Completed { result }
                if serde_json::to_vec(result)
                    .map_err(ApplicationError::storage)?
                    .len()
                    > 65_536 =>
            {
                Err(ApplicationError::Invalid(
                    "large results require an artifact reference".to_owned(),
                ))
            }
            Self::Failed { code }
                if code.trim().is_empty() || code.len() > 1_024 || code.contains('\0') =>
            {
                Err(ApplicationError::Invalid(
                    "a failure requires a bounded code".to_owned(),
                ))
            }
            Self::Cancelled { reason } | Self::Uncertain { reason }
                if reason.trim().is_empty() || reason.len() > 8_192 || reason.contains('\0') =>
            {
                Err(ApplicationError::Invalid(
                    "an interruption requires a bounded reason".to_owned(),
                ))
            }
            _ => Ok(()),
        }
    }
}

/// Runtime business operations are atomic, not a collection of CRUD calls.
#[async_trait]
pub trait RuntimeStore: Send + Sync {
    /// Admit a user input, native Job, scheduler state and events together.
    async fn submit(
        &self,
        principal: &PrincipalScope,
        request: JobSubmission,
    ) -> Result<RuntimeJob, ApplicationError>;
    async fn input_version(
        &self,
        owner: &PrincipalKey,
        session: &SessionId,
    ) -> Result<u64, ApplicationError>;
    async fn get(&self, owner: &PrincipalKey, job: &JobId) -> Result<RuntimeJob, ApplicationError>;
    /// Claim at most one job and its session; do not wait for model/network I/O.
    async fn claim(
        &self,
        worker: &WorkerInstanceId,
        duration: LeaseDuration,
    ) -> Result<Option<ClaimedJob>, ApplicationError>;
    async fn renew(
        &self,
        lease: &ExecutionLease,
        duration: LeaseDuration,
    ) -> Result<ExecutionLease, ApplicationError>;
    /// Commit a boundary and release execution capacity for another claimant.
    async fn checkpoint(
        &self,
        lease: &ExecutionLease,
        checkpoint: RuntimeCheckpoint,
    ) -> Result<RuntimeJob, ApplicationError>;
    async fn finish(
        &self,
        lease: &ExecutionLease,
        outcome: JobFinish,
    ) -> Result<RuntimeJob, ApplicationError>;
}

/// Profile-installed dispatch service. Backends own atomic admission and fair
/// claiming; the service never starts a private client-side agent loop.
#[derive(Clone)]
pub struct JobDispatcher {
    store: Arc<dyn RuntimeStore>,
}

impl JobDispatcher {
    #[must_use]
    pub fn new(store: Arc<dyn RuntimeStore>) -> Self {
        Self { store }
    }

    pub async fn dispatch(
        &self,
        principal: &PrincipalScope,
        submission: JobSubmission,
    ) -> Result<RuntimeJob, ApplicationError> {
        self.store.submit(principal, submission).await
    }
}

#[async_trait]
impl zuno_runtime::Component for JobDispatcher {
    fn id(&self) -> &str {
        "job-dispatcher"
    }
    async fn prepare(
        &self,
        context: &mut zuno_runtime::PrepareContext,
    ) -> Result<(), zuno_runtime::RuntimeError> {
        context.provide(Arc::new(self.clone()))
    }
}
