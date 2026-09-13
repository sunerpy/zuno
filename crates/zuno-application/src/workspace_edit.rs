//! Explicit, bounded file changes. Expected content and the complete proposed
//! bytes are approval inputs, not shell commands or host filesystem paths.
use crate::{
    ApplicationError,
    environment::{Environment, EnvironmentSnapshot},
    runtime::ExecutionLease,
    workspace_merge::WorkspacePath,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_types::identity::{EnvironmentId, InvocationId, OperationId};

pub const MAX_EDIT_FILES: usize = 32;
pub const MAX_EDIT_BYTES: usize = 262144;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileExpectation {
    Absent,
    File { sha256: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceFileEdit {
    pub path: WorkspacePath,
    pub expected: FileExpectation,
    /// None deletes an existing regular file. Text, including empty text,
    /// replaces or creates exactly this file.
    pub content: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceEditOperation {
    pub id: OperationId,
    pub invocation_id: InvocationId,
    pub environment_id: EnvironmentId,
    pub expected_revision: u64,
    pub edits: Vec<WorkspaceFileEdit>,
}
impl WorkspaceEditOperation {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        let invalid = || ApplicationError::Invalid("invalid bounded workspace edit".to_owned());
        if self.expected_revision == 0
            || self.edits.is_empty()
            || self.edits.len() > MAX_EDIT_FILES
            || self
                .edits
                .iter()
                .filter_map(|edit| edit.content.as_ref())
                .map(String::len)
                .sum::<usize>()
                > MAX_EDIT_BYTES
        {
            return Err(invalid());
        }
        let mut paths = std::collections::BTreeSet::new();
        for edit in &self.edits {
            if edit.path.as_str() == "." || !paths.insert(edit.path.clone()) {
                return Err(invalid());
            }
            if edit
                .content
                .as_ref()
                .is_some_and(|content| content.contains('\0'))
            {
                return Err(invalid());
            }
            match &edit.expected {
                FileExpectation::Absent if edit.content.is_none() => return Err(invalid()),
                FileExpectation::File { sha256 }
                    if sha256.len() != 64
                        || !sha256
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
                {
                    return Err(invalid());
                }
                _ => {}
            }
        }
        Ok(())
    }
    pub fn digest(&self) -> String {
        zuno_orchestration::sha256_json(&serde_json::json!(self))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEditState {
    Preparing,
    Committed,
    Cancelled,
    Uncertain,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceEditReceipt {
    pub id: OperationId,
    pub environment_id: EnvironmentId,
    pub state: WorkspaceEditState,
    pub request_digest: String,
    pub revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceEditAdmission {
    pub gateway_id: zuno_types::identity::GatewayId,
    pub lease: ExecutionLease,
    pub environment: Environment,
    pub operation: WorkspaceEditOperation,
    pub base: EnvironmentSnapshot,
    pub review: Vec<WorkspaceEditReview>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceEditReview {
    pub path: WorkspacePath,
    pub before: Option<String>,
    pub after: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceEditView {
    pub approval_id: zuno_types::identity::ApprovalId,
    pub operation_id: OperationId,
    pub review: Vec<WorkspaceEditReview>,
    pub admitted: bool,
}
impl WorkspaceEditAdmission {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.operation.validate()?;
        self.environment.spec.validate()?;
        if self.environment.owner != self.lease.owner
            || self.environment.spec.session_id != self.lease.session_id
            || self.environment.spec.id != self.operation.environment_id
            || self.environment.revision != self.operation.expected_revision
            || self.base.environment_id != self.environment.spec.id
            || self.base.revision != self.environment.revision
            || self.base.sha256.len() != 64
            || !self
                .base
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.base.bytes > 512 * 1024 * 1024
            || self.review.len() != self.operation.edits.len()
            || self
                .review
                .iter()
                .filter_map(|item| item.before.as_ref())
                .map(String::len)
                .sum::<usize>()
                > MAX_EDIT_BYTES
            || serde_json::to_vec(self)
                .map_err(ApplicationError::storage)?
                .len()
                > 524288
        {
            return Err(ApplicationError::Conflict);
        }
        for (change, review) in self.operation.edits.iter().zip(&self.review) {
            if change.path != review.path
                || change.content != review.after
                || match (&change.expected, &review.before) {
                    (FileExpectation::Absent, None) => false,
                    (FileExpectation::File { sha256 }, Some(before)) => {
                        zuno_orchestration::sha256_text(before) != *sha256
                    }
                    _ => true,
                }
            {
                return Err(ApplicationError::Conflict);
            }
        }
        Ok(())
    }
    pub fn arguments_digest(&self) -> String {
        zuno_orchestration::sha256_json(&serde_json::json!([self.operation, self.review]))
    }
    pub fn resources_digest(&self) -> String {
        zuno_orchestration::sha256_json(&serde_json::json!([
            self.gateway_id,
            self.environment,
            self.base
        ]))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceEditCompletion {
    pub admission: WorkspaceEditAdmission,
    pub receipt: WorkspaceEditReceipt,
}
impl WorkspaceEditCompletion {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        let admission = &self.admission;
        admission.validate()?;
        if admission.environment.owner != admission.lease.owner
            || admission.environment.spec.session_id != admission.lease.session_id
            || admission.environment.spec.id != admission.operation.environment_id
            || admission.environment.revision != admission.operation.expected_revision
            || self.receipt.id != admission.operation.id
            || self.receipt.environment_id != admission.operation.environment_id
            || self.receipt.request_digest != admission.operation.digest()
            || self.receipt.revision
                != match self.receipt.state {
                    WorkspaceEditState::Committed => admission
                        .operation
                        .expected_revision
                        .checked_add(1)
                        .ok_or(ApplicationError::Conflict)?,
                    WorkspaceEditState::Cancelled => admission.operation.expected_revision,
                    _ => return Err(ApplicationError::Conflict),
                }
        {
            return Err(ApplicationError::Conflict);
        }
        Ok(())
    }
}
#[async_trait]
pub trait WorkspaceEditAuthority: Send + Sync {
    async fn authorize_edit(
        &self,
        admission: &WorkspaceEditAdmission,
    ) -> Result<(), ApplicationError>;
}
#[async_trait]
pub trait WorkspaceEditCompletionSink: Send + Sync {
    async fn publish_edit(
        &self,
        completion: &WorkspaceEditCompletion,
    ) -> Result<(), ApplicationError>;
}
