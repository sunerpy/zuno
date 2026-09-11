use super::*;
use sqlx_core::raw_sql::raw_sql;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "organization-administration").await;
    let target = scope(f.owner.tenant_id().as_str(), "new-user", "web", 1);
    let request = UpdateOrganizationMember {
        request_id: RequestId::new("member").unwrap(),
        expected_revision: NonZeroU64::MIN,
        member: OrganizationMember {
            owner: target.owner(),
            role: OrganizationRole::Member,
            active: true,
        },
    };
    assert!(matches!(
        f.authority
            .update_member(&f.reviewer, request.clone())
            .await,
        Err(ApplicationError::Forbidden)
    ));
    let api_admin = scope(f.owner.tenant_id().as_str(), "alice", "api", 1);
    assert!(matches!(
        f.authority.update_member(&api_admin, request.clone()).await,
        Err(ApplicationError::Forbidden)
    ));
    let (one, two) = tokio::join!(
        f.authority.update_member(&f.owner, request.clone()),
        f.authority.update_member(&f.owner, request.clone())
    );
    assert_eq!(one.unwrap().revision.get(), 2);
    assert_eq!(two.unwrap().revision.get(), 2);
    assert_eq!(
        f.authority
            .access(&target.owner())
            .await
            .unwrap()
            .member
            .role,
        OrganizationRole::Member
    );
    let current = scope(f.owner.tenant_id().as_str(), "alice", "web", 2);
    let mut changed = request;
    changed.member.role = OrganizationRole::Administrator;
    assert!(matches!(
        f.authority.update_member(&current, changed).await,
        Err(ApplicationError::Conflict)
    ));

    // A failed audit must also roll back membership and the policy revision.
    raw_sql(
        "CREATE FUNCTION zuno_enterprise_preview.refuse_org_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.type='organization.member' THEN RAISE EXCEPTION 'injected organization audit failure'; END IF; RETURN NEW; END $$;
         CREATE TRIGGER refuse_org_audit BEFORE INSERT ON zuno_enterprise_preview.organization_audit
           FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.refuse_org_audit();",
    ).execute(admin).await.unwrap();
    let revoke = UpdateOrganizationMember {
        request_id: RequestId::new("revoke").unwrap(),
        expected_revision: NonZeroU64::new(2).unwrap(),
        member: OrganizationMember {
            owner: target.owner(),
            role: OrganizationRole::Member,
            active: false,
        },
    };
    assert!(
        f.authority
            .update_member(&current, revoke.clone())
            .await
            .is_err()
    );
    let unchanged = f.authority.access(&target.owner()).await.unwrap();
    assert!(unchanged.member.active);
    assert_eq!(unchanged.policy.revision.get(), 2);
    raw_sql("DROP TRIGGER refuse_org_audit ON zuno_enterprise_preview.organization_audit; DROP FUNCTION zuno_enterprise_preview.refuse_org_audit()")
        .execute(admin).await.unwrap();
    assert_eq!(
        f.authority
            .update_member(&current, revoke)
            .await
            .unwrap()
            .revision
            .get(),
        3
    );
    assert!(
        !f.authority
            .access(&target.owner())
            .await
            .unwrap()
            .member
            .active
    );

    let current = scope(f.owner.tenant_id().as_str(), "alice", "web", 3);
    let mut policy = f.policy.clone();
    policy.revision = NonZeroU64::new(4).unwrap();
    policy.auto_read_apps.clear();
    let request = UpdateOrganizationPolicy {
        request_id: RequestId::new("policy").unwrap(),
        expected_revision: NonZeroU64::new(3).unwrap(),
        policy,
    };
    let (one, two) = tokio::join!(
        f.authority.update_policy(&current, request.clone()),
        f.authority.update_policy(&current, request)
    );
    assert_eq!(one.unwrap().revision.get(), 4);
    assert_eq!(two.unwrap().revision.get(), 4);
    let updated = f.authority.access(&f.owner.owner()).await.unwrap();
    assert_eq!(updated.policy.revision.get(), 4);
    assert!(updated.policy.auto_read_apps.is_empty());
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.organization_audit WHERE tenant_id=$1 AND type='organization.policy'")
        .bind(f.owner.tenant_id().as_str()).fetch_one(admin).await.unwrap(),1);
}
