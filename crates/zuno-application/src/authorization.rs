//! Data-owner authorization and durable approvals. Caller scopes are supplied
//! only after host authentication. Worker APIs additionally verify a Job-bound
//! service grant: neither a scope nor an execution lease is a credential.

use crate::{ApplicationError, runtime::ExecutionLease};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::num::NonZeroU64;
use std::sync::Arc;
use zuno_permission::enterprise::{
    ApprovalAudience, EffectKind, OrganizationMember, OrganizationPolicy, PreparedEffectFacts,
};
use zuno_runtime::{Component, PrepareContext, RuntimeError};
use zuno_types::identity::{
    ApprovalId, InvocationId, JobId, OperationId, PrincipalKey, PrincipalScope, RequestId,
    SessionId, TurnId,
};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationAccess {
    pub policy: OrganizationPolicy,
    pub member: OrganizationMember,
}

/// Stable authorization target. Attempt/worker/epoch are intentionally absent.
/// The execution permission check binds the current lease separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalBinding {
    pub job_id: JobId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub invocation_id: InvocationId,
    pub operation_id: OperationId,
    pub arguments_sha256: String,
    /// Digest of the gateway's resolved resource identities and versions.
    pub resources_sha256: String,
    pub effect: EffectKind,
}

impl ApprovalBinding {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        for digest in [&self.arguments_sha256, &self.resources_sha256] {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(ApplicationError::Invalid(
                    "invalid approval binding digest".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Produced by a trusted gateway/handler after parameter, resource and boundary
/// resolution. Not a client DTO. Presentation cannot change authorization.
#[derive(Debug, Clone)]
pub struct ApprovalProposal {
    pub binding: ApprovalBinding,
    pub facts: PreparedEffectFacts,
    pub presentation: Value,
}

impl ApprovalProposal {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.binding.validate()?;
        if self.binding.effect != self.facts.kind
            || !self.presentation.is_object()
            || serde_json::to_vec(&self.presentation)
                .map_err(ApplicationError::storage)?
                .len()
                > 16_384
        {
            return Err(ApplicationError::Invalid(
                "invalid approval facts or bounded presentation".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalState {
    Pending,
    Automatic,
    Approved,
    Rejected,
    Expired,
    Invalidated,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRecord {
    pub id: ApprovalId,
    pub binding: ApprovalBinding,
    pub requester: PrincipalScope,
    pub policy_revision: NonZeroU64,
    pub audience: ApprovalAudience,
    pub state: ApprovalState,
    pub presentation: Value,
    pub decided_by: Option<PrincipalKey>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub decided_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalAnswer {
    Approve,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnswerApproval {
    pub request_id: RequestId,
    pub approval_id: ApprovalId,
    pub answer: ApprovalAnswer,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateOrganizationMember {
    pub request_id: RequestId,
    pub expected_revision: NonZeroU64,
    pub member: OrganizationMember,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateOrganizationPolicy {
    pub request_id: RequestId,
    pub expected_revision: NonZeroU64,
    pub policy: OrganizationPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationMutationReceipt {
    pub request_id: RequestId,
    pub revision: NonZeroU64,
}

/// A checked state-service response, not a transferable execution token. A
/// gateway calls the authenticated service with current resource facts before
/// admitting the actual operation; a Worker cannot assert this value as proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckedApproval {
    pub approval_id: ApprovalId,
    pub binding: ApprovalBinding,
    pub lease: ExecutionLease,
    pub valid_until_ms: i64,
}

#[async_trait]
pub trait OrganizationStore: Send + Sync {
    /// Administration requires a current human administrator using a trusted
    /// approval application. Mutation, revision, receipt and audit are atomic.
    async fn update_member(
        &self,
        actor: &PrincipalScope,
        request: UpdateOrganizationMember,
    ) -> Result<OrganizationMutationReceipt, ApplicationError>;
    async fn update_policy(
        &self,
        actor: &PrincipalScope,
        request: UpdateOrganizationPolicy,
    ) -> Result<OrganizationMutationReceipt, ApplicationError>;
    async fn access(&self, owner: &PrincipalKey) -> Result<OrganizationAccess, ApplicationError>;
    /// Record the decision and audit under the Job/session lease and current
    /// organization policy in the same transaction. Duplicate bindings reuse it.
    async fn admit(
        &self,
        lease: &ExecutionLease,
        proposal: ApprovalProposal,
    ) -> Result<ApprovalRecord, ApplicationError>;
    async fn approval(
        &self,
        viewer: &PrincipalScope,
        id: &ApprovalId,
    ) -> Result<ApprovalRecord, ApplicationError>;
    /// Answer and its request receipt/audit commit atomically; current role,
    /// tenant, application, policy revision and expiry are rechecked.
    async fn answer(
        &self,
        actor: &PrincipalScope,
        request: AnswerApproval,
    ) -> Result<ApprovalRecord, ApplicationError>;
    /// Revalidate the current lease, binding, policy and gateway-resolved facts.
    /// An existing approval never grants a broader or changed operation.
    async fn check_execution(
        &self,
        lease: &ExecutionLease,
        proposal: ApprovalProposal,
    ) -> Result<CheckedApproval, ApplicationError>;
}

#[derive(Clone)]
pub struct OrganizationAuthority {
    store: Arc<dyn OrganizationStore>,
}
impl OrganizationAuthority {
    #[must_use]
    pub fn new(store: Arc<dyn OrganizationStore>) -> Self {
        Self { store }
    }
    #[must_use]
    pub fn store(&self) -> &Arc<dyn OrganizationStore> {
        &self.store
    }
}
#[async_trait]
impl Component for OrganizationAuthority {
    fn id(&self) -> &str {
        "organization-authority"
    }
    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide(Arc::new(self.clone()))
    }
}
