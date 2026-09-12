use super::*;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "approval-boundaries").await;
    let mut sensitive = proposal(&f, "sensitive", EffectKind::Process);
    sensitive.facts.sensitive = true;
    let pending = f
        .authority
        .admit(&f.lease, sensitive.clone())
        .await
        .unwrap();
    assert_eq!(pending.audience, ApprovalAudience::DesignatedApprover);
    assert!(matches!(
        f.authority
            .answer(&f.owner, answer(&pending.id, "self"))
            .await,
        Err(ApplicationError::Forbidden)
    ));
    assert!(matches!(
        f.authority
            .answer(&f.outsider, answer(&pending.id, "outsider"))
            .await,
        Err(ApplicationError::Forbidden)
    ));
    assert_eq!(
        f.authority
            .approval(&f.reviewer, &pending.id)
            .await
            .unwrap()
            .id,
        pending.id
    );
    f.authority
        .answer(&f.reviewer, answer(&pending.id, "reviewer"))
        .await
        .unwrap();
    f.authority
        .check_execution(&f.lease, sensitive.clone())
        .await
        .unwrap();
    // Recheck the approver's current role even when a test changes it without
    // following the supported policy-revision mutation API.
    query("UPDATE zuno_enterprise_preview.organization_member SET role='member' WHERE tenant_id=$1 AND principal_id='bob'")
        .bind(f.owner.tenant_id().as_str()).execute(admin).await.unwrap();
    assert!(matches!(
        f.authority.check_execution(&f.lease, sensitive).await,
        Err(ApplicationError::Forbidden)
    ));
    assert_eq!(
        f.authority
            .approval(&f.owner, &pending.id)
            .await
            .unwrap()
            .state,
        ApprovalState::Invalidated
    );

    let expires = proposal(&f, "expires", EffectKind::Process);
    let pending = f.authority.admit(&f.lease, expires).await.unwrap();
    query("UPDATE zuno_enterprise_preview.operation_approval SET created_at=1,expires_at=2 WHERE tenant_id=$1 AND id=$2")
        .bind(f.owner.tenant_id().as_str()).bind(pending.id.as_str()).execute(admin).await.unwrap();
    assert!(matches!(
        f.authority
            .answer(&f.owner, answer(&pending.id, "late"))
            .await,
        Err(ApplicationError::Forbidden)
    ));
    assert_eq!(
        f.authority
            .approval(&f.owner, &pending.id)
            .await
            .unwrap()
            .state,
        ApprovalState::Expired
    );

    let changed = proposal(&f, "policy-changed", EffectKind::Process);
    let pending = f.authority.admit(&f.lease, changed.clone()).await.unwrap();
    query("UPDATE zuno_enterprise_preview.organization_policy SET revision=2 WHERE tenant_id=$1")
        .bind(f.owner.tenant_id().as_str())
        .execute(admin)
        .await
        .unwrap();
    let current = scope(f.owner.tenant_id().as_str(), "alice", "web", 2);
    assert!(matches!(
        f.authority
            .answer(&current, answer(&pending.id, "new-policy"))
            .await,
        Err(ApplicationError::Forbidden)
    ));
    assert_eq!(
        f.authority
            .approval(&current, &pending.id)
            .await
            .unwrap()
            .state,
        ApprovalState::Invalidated
    );
    assert!(matches!(
        f.authority.check_execution(&f.lease, changed).await,
        Err(ApplicationError::Forbidden)
    ));

    // Setup is creation-only; it must not undo a subsequent administrator revoke.
    query("UPDATE zuno_enterprise_preview.organization_member SET role='member' WHERE tenant_id=$1 AND principal_id='alice'")
        .bind(f.owner.tenant_id().as_str()).execute(admin).await.unwrap();
    assert!(
        !bootstrap_organization(migrator, &f.policy, &f.owner.owner())
            .await
            .unwrap()
    );
    assert_eq!(
        f.authority
            .access(&f.owner.owner())
            .await
            .unwrap()
            .member
            .role,
        OrganizationRole::Member
    );

    // Every user-facing read/write checks the current organization revision,
    // independently of the ownership filter or a previously valid JWT.
    let old_application = AgentApplication::new(Arc::new(backend.sessions(f.owner.clone())));
    assert!(matches!(
        old_application.session(&f.job.session_id).await,
        Err(ApplicationError::Forbidden)
    ));
    let current_application = AgentApplication::new(Arc::new(backend.sessions(current.clone())));
    current_application
        .session(&f.job.session_id)
        .await
        .unwrap();
    query("UPDATE zuno_enterprise_preview.organization_member SET active=false WHERE tenant_id=$1 AND principal_id='alice'")
        .bind(current.tenant_id().as_str()).execute(admin).await.unwrap();
    let before: i64 =
        query_scalar("SELECT count(*) FROM zuno_enterprise_preview.event WHERE tenant_id=$1")
            .bind(current.tenant_id().as_str())
            .fetch_one(admin)
            .await
            .unwrap();
    assert!(matches!(
        current_application.sessions(Default::default()).await,
        Err(ApplicationError::Forbidden)
    ));
    assert!(matches!(
        current_application
            .queue_text(zuno_application::QueueText {
                request_id: RequestId::new("revoked-input").unwrap(),
                session_id: f.job.session_id.clone(),
                text: "A revoked account must not enqueue input".to_owned(),
            })
            .await,
        Err(ApplicationError::Forbidden)
    ));
    assert!(matches!(
        f.runtime
            .submit(
                &current,
                JobSubmission {
                    selection: None,
                    session_id: f.job.session_id.clone(),
                    request_id: RequestId::new("revoked-job").unwrap(),
                    expected_input_version: 1,
                    text: "A revoked account must not dispatch".to_owned(),
                    configuration: f.job.configuration,
                }
            )
            .await,
        Err(ApplicationError::Forbidden)
    ));
    let after: i64 =
        query_scalar("SELECT count(*) FROM zuno_enterprise_preview.event WHERE tenant_id=$1")
            .bind(current.tenant_id().as_str())
            .fetch_one(admin)
            .await
            .unwrap();
    assert_eq!(
        before, after,
        "refused reads and writes leave no new session facts"
    );
}
