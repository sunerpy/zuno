//! Execution-environment contracts. Identifiers and requests never grant access.

pub mod wire;

use crate::ApplicationError;
use crate::runtime::ExecutionLease;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zuno_types::identity::{
    EnvironmentId, EnvironmentSnapshotId, InvocationId, OperationId, PrincipalKey, SessionId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentSpec {
    pub id: EnvironmentId,
    pub session_id: SessionId,
    /// A trusted deployment selects an immutable OCI image digest.
    pub image: String,
    pub memory_bytes: u64,
    pub pids_limit: u32,
    pub cpu_millis: u32,
}
impl EnvironmentSpec {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        let Some((name, digest)) = self.image.rsplit_once("@sha256:") else {
            return Err(ApplicationError::Invalid(
                "execution images require an immutable digest".to_owned(),
            ));
        };
        if name.is_empty()
            || name.len() > 512
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._:-".contains(&byte))
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !(16 * 1024 * 1024..=64 * 1024 * 1024 * 1024).contains(&self.memory_bytes)
            || !(8..=4096).contains(&self.pids_limit)
            || !(100..=64000).contains(&self.cpu_millis)
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded environment specification".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Environment {
    pub owner: PrincipalKey,
    pub spec: EnvironmentSpec,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentSnapshot {
    pub id: EnvironmentSnapshotId,
    pub environment_id: EnvironmentId,
    pub revision: u64,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandOperation {
    pub id: OperationId,
    pub invocation_id: InvocationId,
    pub environment_id: EnvironmentId,
    pub expected_revision: u64,
    /// An argv vector, never a shell-interpolated Docker command.
    pub argv: Vec<String>,
}
impl CommandOperation {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.expected_revision == 0
            || self.argv.is_empty()
            || self.argv.len() > 128
            || self.argv[0].is_empty()
            || self
                .argv
                .iter()
                .any(|arg| arg.len() > 65536 || arg.contains('\0'))
            || self.argv.iter().map(String::len).sum::<usize>() > 262144
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded command operation".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Prepared,
    Starting,
    Running,
    Completed,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationReceipt {
    pub id: OperationId,
    pub environment_id: EnvironmentId,
    pub phase: OperationPhase,
    pub exit_code: Option<i64>,
    pub cancellation_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputChannel {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationOutput {
    pub channel: OutputChannel,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputCursor {
    pub offset: u64,
    pub prefix_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputPage {
    pub chunks: Vec<OperationOutput>,
    pub next: OutputCursor,
    pub end_of_available: bool,
}

/// Immutable facts captured by the execution owner. The original lease records
/// attribution; a late receipt does not require that lease to remain active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationCompletion {
    pub lease: ExecutionLease,
    pub operation: CommandOperation,
    pub receipt: OperationReceipt,
    pub output: Vec<OperationOutput>,
    pub output_truncated: bool,
}

pub const MAX_COMPLETION_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_COMPLETION_OUTPUT_CHUNKS: usize = 8192;

/// Frozen execution coordinates recorded by the authoritative approval writer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationAdmission {
    pub gateway_id: zuno_types::identity::GatewayId,
    pub lease: ExecutionLease,
    pub environment: Environment,
    pub operation: CommandOperation,
}

impl OperationCompletion {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.operation.validate()?;
        if self.receipt.id != self.operation.id
            || self.receipt.environment_id != self.operation.environment_id
            || !matches!(
                self.receipt.phase,
                OperationPhase::Completed | OperationPhase::Cancelled
            )
            || (self.receipt.phase == OperationPhase::Completed && self.receipt.exit_code.is_none())
            || self.output.len() > MAX_COMPLETION_OUTPUT_CHUNKS
            || self
                .output
                .iter()
                .map(|chunk| chunk.bytes.len())
                .sum::<usize>()
                > MAX_COMPLETION_OUTPUT_BYTES
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded operation completion".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Only an authenticated execution owner can publish. A successful response
/// means durable storage, not that a paused parent has already consumed it.
#[async_trait]
pub trait OperationCompletionSink: Send + Sync {
    async fn publish(&self, completion: &OperationCompletion) -> Result<(), ApplicationError>;
}

/// The gateway provides facts; the control plane owns current authorization.
/// Implementations must check the exact operation, resources and current lease.
#[async_trait]
pub trait OperationAuthority: Send + Sync {
    async fn authorize(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        operation: &CommandOperation,
    ) -> Result<(), ApplicationError>;
}

#[async_trait]
pub trait EnvironmentProvider: Send + Sync {
    async fn acquire(
        &self,
        owner: &PrincipalKey,
        spec: EnvironmentSpec,
    ) -> Result<Environment, ApplicationError>;
    async fn get(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
    ) -> Result<Environment, ApplicationError>;
    async fn snapshot(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
        expected_revision: u64,
    ) -> Result<EnvironmentSnapshot, ApplicationError>;
    async fn fork(
        &self,
        owner: &PrincipalKey,
        snapshot: &EnvironmentSnapshot,
        spec: EnvironmentSpec,
    ) -> Result<Environment, ApplicationError>;
    async fn release(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
        expected_revision: u64,
    ) -> Result<(), ApplicationError>;
}

#[async_trait]
pub trait OperationGateway: Send + Sync {
    async fn submit(
        &self,
        lease: &ExecutionLease,
        request: CommandOperation,
    ) -> Result<OperationReceipt, ApplicationError>;
    async fn inspect(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<OperationReceipt, ApplicationError>;
    async fn output(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
        cursor: OutputCursor,
        maximum_bytes: u32,
    ) -> Result<OutputPage, ApplicationError>;
    async fn cancel(
        &self,
        lease: &ExecutionLease,
        id: &OperationId,
    ) -> Result<OperationReceipt, ApplicationError>;
}
