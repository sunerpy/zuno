//! Organization policy is independent of tool presentation and OAuth claims.
//! This evaluator classifies a prepared effect. It does not mint execution
//! authority: the state owner must commit approval and verify the current lease.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::num::NonZeroU64;
use zuno_types::identity::{ClientId, PrincipalKey, PrincipalKind, PrincipalScope, TenantId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OrganizationRole {
    Member,
    Approver,
    Administrator,
    Automation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationMember {
    pub owner: PrincipalKey,
    pub role: OrganizationRole,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationPolicy {
    pub tenant_id: TenantId,
    pub revision: NonZeroU64,
    pub allowed_apps: BTreeSet<ClientId>,
    /// Applications allowed to submit human decisions, typically the owned BFF.
    /// An API client must not auto-answer its own HITL requests.
    pub approval_apps: BTreeSet<ClientId>,
    /// This is an operation-approval whitelist, separate from token acceptance.
    pub auto_read_apps: BTreeSet<ClientId>,
    pub approval_lifetime_seconds: u32,
}

impl OrganizationPolicy {
    pub fn is_valid(&self) -> bool {
        !self.allowed_apps.is_empty()
            && self.allowed_apps.len() <= 128
            && self.auto_read_apps.is_subset(&self.allowed_apps)
            && !self.approval_apps.is_empty()
            && self.approval_apps.is_subset(&self.allowed_apps)
            && (30..=3600).contains(&self.approval_lifetime_seconds)
    }
}

/// Execution semantics supplied by a registered handler, never a UI hint or
/// the `readOnlyHint` annotation of an arbitrary remote MCP tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum EffectKind {
    FileRead,
    FileList,
    FileSearch,
    FileWrite,
    Process,
    Network,
    ExternalTool,
    MemoryRead,
    MemoryWrite,
    Unknown,
}

impl EffectKind {
    fn needs_environment(self) -> bool {
        matches!(
            self,
            Self::FileRead | Self::FileList | Self::FileSearch | Self::FileWrite | Self::Process
        )
    }
    fn minimal_read(self) -> bool {
        matches!(
            self,
            Self::FileRead | Self::FileList | Self::FileSearch | Self::MemoryRead
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationFact {
    Enforced,
    NotApplicable,
    Unavailable,
    Unenforced,
}

/// Trusted host facts intentionally have no wire deserializer. Gateways resolve
/// resource access/version and isolation from their own stores and handlers.
#[derive(Debug, Clone, Copy)]
pub struct PreparedEffectFacts {
    pub kind: EffectKind,
    pub resource_authorized: bool,
    pub isolation: IsolationFact,
    pub builtin_handler: bool,
    pub sensitive: bool,
    pub explicit_deny: bool,
    pub mandatory_human: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalAudience {
    Requester,
    DesignatedApprover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PolicyDenial {
    InvalidPolicy,
    Membership,
    UnsupportedActor,
    Client,
    PolicyChanged,
    Resource,
    Isolation,
    ExplicitRule,
    UnknownEffect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EnterpriseDecision {
    Automatic,
    Human { audience: ApprovalAudience },
    Denied { reason: PolicyDenial },
}

pub fn evaluate_enterprise(
    policy: &OrganizationPolicy,
    member: &OrganizationMember,
    principal: &PrincipalScope,
    effect: PreparedEffectFacts,
) -> EnterpriseDecision {
    let deny = |reason| EnterpriseDecision::Denied { reason };
    if let Some(reason) = actor_denial(policy, member, principal) {
        return deny(reason);
    }
    let client = principal
        .client_id()
        .expect("validated application identity");
    if effect.explicit_deny {
        return deny(PolicyDenial::ExplicitRule);
    }
    if !effect.resource_authorized {
        return deny(PolicyDenial::Resource);
    }
    if effect.kind == EffectKind::Unknown {
        return deny(PolicyDenial::UnknownEffect);
    }
    if effect.kind.needs_environment() && effect.isolation != IsolationFact::Enforced {
        return deny(PolicyDenial::Isolation);
    }
    if effect.sensitive || principal.kind() == PrincipalKind::Workload {
        return EnterpriseDecision::Human {
            audience: ApprovalAudience::DesignatedApprover,
        };
    }
    if effect.kind.minimal_read()
        && effect.builtin_handler
        && !effect.mandatory_human
        && policy.auto_read_apps.contains(client)
    {
        return EnterpriseDecision::Automatic;
    }
    EnterpriseDecision::Human {
        audience: ApprovalAudience::Requester,
    }
}

pub fn actor_denial(
    policy: &OrganizationPolicy,
    member: &OrganizationMember,
    principal: &PrincipalScope,
) -> Option<PolicyDenial> {
    if !policy.is_valid() {
        return Some(PolicyDenial::InvalidPolicy);
    }
    if principal.tenant_id() != &policy.tenant_id
        || principal.owner() != member.owner
        || !member.active
    {
        return Some(PolicyDenial::Membership);
    }
    let human = principal.kind() == PrincipalKind::User;
    let automation =
        principal.kind() == PrincipalKind::Workload && member.role == OrganizationRole::Automation;
    if (!human && !automation) || (human && member.role == OrganizationRole::Automation) {
        return Some(PolicyDenial::UnsupportedActor);
    }
    let Some(client) = principal.client_id() else {
        return Some(PolicyDenial::Client);
    };
    if !policy.allowed_apps.contains(client) {
        return Some(PolicyDenial::Client);
    }
    if policy.revision != principal.policy_revision() {
        return Some(PolicyDenial::PolicyChanged);
    }
    None
}

/// Called with current, store-resolved membership. Sensitive/automation
/// approvals require a different human with an assigned approval role.
pub fn can_approve(
    policy: &OrganizationPolicy,
    audience: ApprovalAudience,
    requester: &PrincipalKey,
    approver: &PrincipalScope,
    member: &OrganizationMember,
) -> bool {
    if actor_denial(policy, member, approver).is_some()
        || approver
            .client_id()
            .is_none_or(|client| !policy.approval_apps.contains(client))
        || requester.tenant_id != *approver.tenant_id()
        || approver.kind() != PrincipalKind::User
    {
        return false;
    }
    match audience {
        ApprovalAudience::Requester => approver.owner() == *requester,
        ApprovalAudience::DesignatedApprover => {
            approver.owner() != *requester
                && matches!(
                    member.role,
                    OrganizationRole::Approver | OrganizationRole::Administrator
                )
        }
    }
}

#[cfg(test)]
mod tests;
