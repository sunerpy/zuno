use super::*;
use zuno_types::identity::PrincipalId;

fn principal(name: &str) -> PrincipalScope {
    PrincipalScope::new(
        TenantId::new("tenant").unwrap(),
        PrincipalId::new(name).unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("web").unwrap()),
        NonZeroU64::new(3).unwrap(),
    )
}
fn policy() -> OrganizationPolicy {
    OrganizationPolicy {
        tenant_id: TenantId::new("tenant").unwrap(),
        revision: NonZeroU64::new(3).unwrap(),
        allowed_apps: [
            ClientId::new("web").unwrap(),
            ClientId::new("external").unwrap(),
        ]
        .into(),
        auto_read_apps: [ClientId::new("web").unwrap()].into(),
        approval_apps: [ClientId::new("web").unwrap()].into(),
        approval_lifetime_seconds: 300,
    }
}
fn member(principal: &PrincipalScope, role: OrganizationRole) -> OrganizationMember {
    OrganizationMember {
        owner: principal.owner(),
        role,
        active: true,
    }
}
fn read() -> PreparedEffectFacts {
    PreparedEffectFacts {
        kind: EffectKind::FileRead,
        resource_authorized: true,
        isolation: IsolationFact::Enforced,
        builtin_handler: true,
        sensitive: false,
        explicit_deny: false,
        mandatory_human: false,
    }
}

#[test]
fn automatic_read_never_overrides_resource_isolation_or_explicit_denial() {
    let p = principal("alice");
    let m = member(&p, OrganizationRole::Administrator);
    assert_eq!(
        evaluate_enterprise(&policy(), &m, &p, read()),
        EnterpriseDecision::Automatic
    );
    for (effect, reason) in [
        (
            PreparedEffectFacts {
                resource_authorized: false,
                ..read()
            },
            PolicyDenial::Resource,
        ),
        (
            PreparedEffectFacts {
                isolation: IsolationFact::Unavailable,
                ..read()
            },
            PolicyDenial::Isolation,
        ),
        (
            PreparedEffectFacts {
                isolation: IsolationFact::Unenforced,
                ..read()
            },
            PolicyDenial::Isolation,
        ),
        (
            PreparedEffectFacts {
                explicit_deny: true,
                ..read()
            },
            PolicyDenial::ExplicitRule,
        ),
    ] {
        assert_eq!(
            evaluate_enterprise(&policy(), &m, &p, effect),
            EnterpriseDecision::Denied { reason }
        );
    }
}

#[test]
fn a_whitelisted_application_does_not_automatically_approve_commands_or_changes() {
    let p = principal("alice");
    let m = member(&p, OrganizationRole::Member);
    for kind in [
        EffectKind::Process,
        EffectKind::FileWrite,
        EffectKind::Network,
        EffectKind::ExternalTool,
        EffectKind::MemoryWrite,
    ] {
        assert_eq!(
            evaluate_enterprise(&policy(), &m, &p, PreparedEffectFacts { kind, ..read() }),
            EnterpriseDecision::Human {
                audience: ApprovalAudience::Requester
            }
        );
    }
    for effect in [
        PreparedEffectFacts {
            builtin_handler: false,
            ..read()
        },
        PreparedEffectFacts {
            mandatory_human: true,
            ..read()
        },
    ] {
        assert_eq!(
            evaluate_enterprise(&policy(), &m, &p, effect),
            EnterpriseDecision::Human {
                audience: ApprovalAudience::Requester
            }
        );
    }
    assert_eq!(
        evaluate_enterprise(
            &policy(),
            &m,
            &p,
            PreparedEffectFacts {
                sensitive: true,
                ..read()
            }
        ),
        EnterpriseDecision::Human {
            audience: ApprovalAudience::DesignatedApprover
        }
    );
}

#[test]
fn current_membership_policy_revision_and_exact_app_are_required() {
    let p = principal("alice");
    let m = member(&p, OrganizationRole::Member);
    let suspended = OrganizationMember {
        active: false,
        ..m.clone()
    };
    assert_eq!(
        evaluate_enterprise(&policy(), &suspended, &p, read()),
        EnterpriseDecision::Denied {
            reason: PolicyDenial::Membership
        }
    );
    let changed = OrganizationPolicy {
        revision: NonZeroU64::new(4).unwrap(),
        ..policy()
    };
    assert_eq!(
        evaluate_enterprise(&changed, &m, &p, read()),
        EnterpriseDecision::Denied {
            reason: PolicyDenial::PolicyChanged
        }
    );
    let unknown = PrincipalScope::new(
        p.tenant_id().clone(),
        p.principal_id().clone(),
        PrincipalKind::User,
        Some(ClientId::new("web-other").unwrap()),
        p.policy_revision(),
    );
    assert_eq!(
        evaluate_enterprise(&policy(), &m, &unknown, read()),
        EnterpriseDecision::Denied {
            reason: PolicyDenial::Client
        }
    );
    let external = PrincipalScope::new(
        p.tenant_id().clone(),
        p.principal_id().clone(),
        PrincipalKind::User,
        Some(ClientId::new("external").unwrap()),
        p.policy_revision(),
    );
    assert_eq!(
        evaluate_enterprise(&policy(), &m, &external, read()),
        EnterpriseDecision::Human {
            audience: ApprovalAudience::Requester
        }
    );
    assert!(!can_approve(
        &policy(),
        ApprovalAudience::Requester,
        &p.owner(),
        &external,
        &m
    ));
}

#[test]
fn designated_approvals_require_another_current_human_approver() {
    let requester = principal("alice");
    let other = principal("bob");
    let admin = member(&requester, OrganizationRole::Administrator);
    let reviewer = member(&other, OrganizationRole::Approver);
    assert!(can_approve(
        &policy(),
        ApprovalAudience::Requester,
        &requester.owner(),
        &requester,
        &admin
    ));
    assert!(!can_approve(
        &policy(),
        ApprovalAudience::DesignatedApprover,
        &requester.owner(),
        &requester,
        &admin
    ));
    assert!(can_approve(
        &policy(),
        ApprovalAudience::DesignatedApprover,
        &requester.owner(),
        &other,
        &reviewer
    ));
    assert!(!can_approve(
        &policy(),
        ApprovalAudience::Requester,
        &requester.owner(),
        &other,
        &reviewer
    ));
    assert!(!can_approve(
        &policy(),
        ApprovalAudience::DesignatedApprover,
        &requester.owner(),
        &other,
        &member(&other, OrganizationRole::Member)
    ));
    let revoked = OrganizationPolicy {
        allowed_apps: [ClientId::new("external").unwrap()].into(),
        auto_read_apps: BTreeSet::new(),
        approval_apps: [ClientId::new("external").unwrap()].into(),
        ..policy()
    };
    assert!(!can_approve(
        &revoked,
        ApprovalAudience::DesignatedApprover,
        &requester.owner(),
        &other,
        &reviewer
    ));
}
