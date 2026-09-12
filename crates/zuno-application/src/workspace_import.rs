//! Empty-session workspace initialization. A user supplies archive bytes, while
//! the data owner fixes ownership, deployment and first-input admission.
use crate::{
    ApplicationError,
    environment::{EnvironmentSnapshot, EnvironmentSpec},
    runtime::ConfigurationRef,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_types::{
    activity::Counter,
    identity::{GatewayId, PrincipalScope, RequestId, SessionId, WorkspaceImportId},
};

pub const MAX_IMPORT_BYTES: u64 = 512 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceUploadRequest {
    pub session_id: SessionId,
    pub import_id: WorkspaceImportId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceInitializationCompletion {
    pub assignment: WorkspaceImportAssignment,
    pub receipt: WorkspaceImportReceipt,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceImportState {
    Uploading,
    Initializing,
    Ready,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BeginWorkspaceImport {
    pub request_id: RequestId,
    pub expected_input_version: Counter,
    pub sha256: String,
    pub bytes: Counter,
}
impl BeginWorkspaceImport {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.expected_input_version.0 != 0
            || self.bytes.0 == 0
            || self.bytes.0 > MAX_IMPORT_BYTES
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded initial workspace archive".to_owned(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceImportView {
    pub id: WorkspaceImportId,
    pub session_id: SessionId,
    pub state: WorkspaceImportState,
    pub sha256: String,
    pub bytes: Counter,
    pub created_at: Counter,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceImportAssignment {
    pub id: WorkspaceImportId,
    pub principal: PrincipalScope,
    pub session_id: SessionId,
    pub configuration: ConfigurationRef,
    pub gateway_id: GatewayId,
    pub environment: EnvironmentSpec,
    pub sha256: String,
    pub bytes: u64,
}
impl WorkspaceImportAssignment {
    pub fn snapshot_id(&self) -> zuno_types::identity::EnvironmentSnapshotId {
        zuno_types::identity::EnvironmentSnapshotId::new(format!(
            "import-{}",
            zuno_orchestration::sha256_json(&serde_json::json!([self.principal.owner(), self.id]))
        ))
        .expect("derived identity")
    }
    pub fn source_environment_id(&self) -> zuno_types::identity::EnvironmentId {
        zuno_types::identity::EnvironmentId::new(self.snapshot_id().as_str())
            .expect("same logical ID grammar")
    }
    pub fn validate_receipt(
        &self,
        receipt: &WorkspaceImportReceipt,
    ) -> Result<(), ApplicationError> {
        if receipt.import_id != self.id
            || receipt.archive_sha256 != self.sha256
            || receipt.environment.owner != self.principal.owner()
            || receipt.environment.spec != self.environment
            || receipt.environment.revision != 1
            || receipt.snapshot.id != self.snapshot_id()
            || receipt.snapshot.environment_id != self.source_environment_id()
            || receipt.snapshot.revision != 1
            || receipt.snapshot.bytes == 0
            || receipt.snapshot.bytes > MAX_IMPORT_BYTES
            || receipt.snapshot.sha256.len() != 64
            || !receipt
                .snapshot
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ApplicationError::Conflict);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceImportReceipt {
    pub import_id: WorkspaceImportId,
    pub archive_sha256: String,
    pub snapshot: EnvironmentSnapshot,
    pub environment: crate::environment::Environment,
}
#[async_trait]
pub trait WorkspaceInitializationAuthority: Send + Sync {
    async fn authorize_initialization(
        &self,
        assignment: &WorkspaceImportAssignment,
    ) -> Result<Option<WorkspaceImportReceipt>, ApplicationError>;
    async fn initialized(
        &self,
        assignment: &WorkspaceImportAssignment,
        receipt: &WorkspaceImportReceipt,
    ) -> Result<(), ApplicationError>;
}

#[async_trait]
pub trait WorkspaceImportStore: Send + Sync {
    async fn begin_import(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
        request: BeginWorkspaceImport,
        configuration: ConfigurationRef,
        gateway: GatewayId,
        environment: EnvironmentSpec,
    ) -> Result<WorkspaceImportView, ApplicationError>;
    async fn import_view(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
        id: &WorkspaceImportId,
    ) -> Result<WorkspaceImportView, ApplicationError>;
    async fn cancel_import(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
        id: &WorkspaceImportId,
    ) -> Result<WorkspaceImportView, ApplicationError>;
}
