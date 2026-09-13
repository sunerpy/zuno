//! Organization-owned Memory has its own namespace and explicit membership.
//! Global/project residency is independent of ownership or sharing.
use crate::{ApplicationError, PageSize};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_types::{
    activity::Counter,
    identity::{MemorySpaceId, PrincipalId, PrincipalScope, RequestId, WorkspaceId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SharedMemoryRole {
    Reader,
    Contributor,
    Reviewer,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedMemoryMember {
    pub principal_id: PrincipalId,
    pub role: SharedMemoryRole,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigureSharedMemory {
    pub request_id: RequestId,
    pub expected_revision: Counter,
    pub title: String,
    pub workspace_id: WorkspaceId,
    pub enabled: bool,
    pub members: Vec<SharedMemoryMember>,
    pub character_limit: u32,
}
impl ConfigureSharedMemory {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        let mut members = std::collections::BTreeSet::new();
        if self.title.trim().is_empty()
            || self.title.len() > 256
            || self.title.chars().any(char::is_control)
            || !(1..=32768).contains(&self.character_limit)
            || self.members.len() > 256
            || self
                .members
                .iter()
                .any(|member| !members.insert(member.principal_id.clone()))
            || (self.enabled
                && !self
                    .members
                    .iter()
                    .any(|member| member.role == SharedMemoryRole::Reviewer))
        {
            return Err(ApplicationError::Invalid(
                "invalid shared Memory configuration".to_owned(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedMemorySpace {
    pub id: MemorySpaceId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub enabled: bool,
    pub policy_revision: Counter,
    pub document_revision: Counter,
    pub entries: Vec<String>,
    pub digest: String,
    pub role: SharedMemoryRole,
    pub character_limit: u32,
    /// Reviewed entries retained for history but not eligible for current recall.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedEvidenceBinding {
    pub content: String,
    pub grants: Vec<RequestId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedEvidenceTransition {
    pub before: Vec<SharedEvidenceBinding>,
    pub after: Vec<SharedEvidenceBinding>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShareMemoryEvidence {
    pub request_id: RequestId,
    pub evidence_id: String,
    pub expected_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RevokeSharedEvidence {
    pub request_id: RequestId,
    pub expected_revision: Counter,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SharedEvidenceKind {
    UserStatement,
    SuccessfulOperation,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedEvidenceGrant {
    pub id: RequestId,
    pub space_id: MemorySpaceId,
    pub author: PrincipalId,
    pub revision: Counter,
    pub kind: SharedEvidenceKind,
    pub excerpt: String,
    pub evidence_digest: String,
    pub active: bool,
    pub current: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedEvidencePage {
    pub items: Vec<SharedEvidenceGrant>,
    pub after: Option<RequestId>,
}
#[async_trait]
pub trait SharedEvidenceStore: Send + Sync {
    async fn share(
        &self,
        actor: &PrincipalScope,
        space: &MemorySpaceId,
        request: ShareMemoryEvidence,
    ) -> Result<SharedEvidenceGrant, ApplicationError>;
    async fn revoke(
        &self,
        actor: &PrincipalScope,
        space: &MemorySpaceId,
        id: &RequestId,
        request: RevokeSharedEvidence,
    ) -> Result<SharedEvidenceGrant, ApplicationError>;
    async fn list(
        &self,
        actor: &PrincipalScope,
        space: &MemorySpaceId,
        after: Option<&RequestId>,
        limit: PageSize,
    ) -> Result<SharedEvidencePage, ApplicationError>;
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SharedMemoryEdit {
    Add { content: String },
    Replace { old_text: String, content: String },
    Remove { old_text: String },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposeSharedMemory {
    pub request_id: RequestId,
    pub expected_revision: Counter,
    pub edits: Vec<SharedMemoryEdit>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<SharedEvidenceBinding>,
}
impl ProposeSharedMemory {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.edits.is_empty()
            || self.edits.len() > 32
            || self.evidence.len() > 32
            || self.evidence.iter().any(|binding| {
                binding.grants.is_empty() || binding.grants.len() > 16 || binding.content.is_empty()
            })
            || self.reason.trim().is_empty()
            || self.reason.len() > 2048
            || self.reason.contains('\0')
            || serde_json::to_vec(self)
                .map_err(ApplicationError::storage)?
                .len()
                > 65536
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded shared Memory proposal".to_owned(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SharedMemoryChangeState {
    Pending,
    Applied,
    Rejected,
    Undone,
    Invalidated,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedMemoryChange {
    pub id: RequestId,
    pub space_id: MemorySpaceId,
    pub author: PrincipalId,
    pub base_revision: Counter,
    pub policy_revision: Counter,
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub reason: String,
    pub state: SharedMemoryChangeState,
    pub state_digest: String,
    pub decided_by: Option<PrincipalId>,
    pub applied_revision: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<SharedEvidenceTransition>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SharedMemoryDecision {
    Apply,
    Reject,
    Undo,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewSharedMemory {
    pub request_id: RequestId,
    pub change_id: RequestId,
    pub expected_state: String,
    pub decision: SharedMemoryDecision,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedMemoryPage {
    pub items: Vec<SharedMemorySpace>,
    pub after: Option<MemorySpaceId>,
}
#[async_trait]
pub trait SharedMemoryStore: Send + Sync {
    async fn configure(
        &self,
        actor: &PrincipalScope,
        id: &MemorySpaceId,
        request: ConfigureSharedMemory,
    ) -> Result<SharedMemorySpace, ApplicationError>;
    async fn list(
        &self,
        actor: &PrincipalScope,
        workspace: &WorkspaceId,
        after: Option<&MemorySpaceId>,
        limit: PageSize,
    ) -> Result<SharedMemoryPage, ApplicationError>;
    async fn read(
        &self,
        actor: &PrincipalScope,
        id: &MemorySpaceId,
    ) -> Result<SharedMemorySpace, ApplicationError>;
    async fn propose(
        &self,
        actor: &PrincipalScope,
        id: &MemorySpaceId,
        request: ProposeSharedMemory,
    ) -> Result<SharedMemoryChange, ApplicationError>;
    async fn change(
        &self,
        actor: &PrincipalScope,
        id: &MemorySpaceId,
        change: &RequestId,
    ) -> Result<SharedMemoryChange, ApplicationError>;
    async fn review(
        &self,
        actor: &PrincipalScope,
        id: &MemorySpaceId,
        request: ReviewSharedMemory,
    ) -> Result<SharedMemoryChange, ApplicationError>;
}
