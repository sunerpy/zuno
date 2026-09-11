use super::*;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "approval-lifecycle").await;
    let read = proposal(&f, "read", EffectKind::FileRead);
    let automatic = f.authority.admit(&f.lease, read.clone()).await.unwrap();
    assert_eq!(automatic.state, ApprovalState::Automatic);
    let mut false_deadline = f.lease.clone();
    false_deadline.expires_at_ms = i64::MAX;
    let checked = f
        .authority
        .check_execution(&false_deadline, read.clone())
        .await
        .unwrap();
    assert_eq!(checked.lease.expires_at_ms, f.lease.expires_at_ms);
    assert!(checked.valid_until_ms <= f.lease.expires_at_ms);
    let mut changed = read.clone();
    changed.binding.arguments_sha256 = "c".repeat(64);
    assert!(matches!(
        f.authority.admit(&f.lease, changed).await,
        Err(ApplicationError::Conflict)
    ));
    let mut denied = proposal(&f, "outside", EffectKind::FileRead);
    denied.facts.resource_authorized = false;
    assert!(matches!(
        f.authority.admit(&f.lease, denied).await,
        Err(ApplicationError::Forbidden)
    ));
    let mut unconfined = proposal(&f, "unconfined", EffectKind::Process);
    unconfined.facts.isolation = IsolationFact::Unenforced;
    assert!(matches!(
        f.authority.admit(&f.lease, unconfined).await,
        Err(ApplicationError::Forbidden)
    ));

    let command = proposal(&f, "command", EffectKind::Process);
    let pending = f.authority.admit(&f.lease, command.clone()).await.unwrap();
    assert_eq!(pending.state, ApprovalState::Pending);
    assert!(matches!(
        f.authority.check_execution(&f.lease, command.clone()).await,
        Err(ApplicationError::Forbidden)
    ));
    assert!(matches!(
        f.authority.approval(&f.outsider, &pending.id).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(matches!(
        f.authority.approval(&f.reviewer, &pending.id).await,
        Err(ApplicationError::NotFound)
    ));
    let api_actor = scope(f.owner.tenant_id().as_str(), "alice", "api", 1);
    assert_eq!(
        f.authority
            .approval(&api_actor, &pending.id)
            .await
            .unwrap()
            .state,
        ApprovalState::Pending
    );
    assert!(matches!(
        f.authority
            .answer(&api_actor, answer(&pending.id, "cannot-self-approve"))
            .await,
        Err(ApplicationError::Forbidden)
    ));
    let request = answer(&pending.id, "approve");
    let (first, repeated) = tokio::join!(
        f.authority.answer(&f.owner, request.clone()),
        f.authority.answer(&f.owner, request.clone())
    );
    assert_eq!(first.unwrap().state, ApprovalState::Approved);
    assert_eq!(repeated.unwrap().state, ApprovalState::Approved);
    let mut changed_answer = request;
    changed_answer.answer = ApprovalAnswer::Reject;
    assert!(matches!(
        f.authority.answer(&f.owner, changed_answer).await,
        Err(ApplicationError::Conflict)
    ));
    let count:i64=query_scalar(
        "SELECT count(*) FROM zuno_enterprise_preview.event WHERE session_id=$1 AND type='authorization.approval.answered'",
    ).bind(f.job.session_id.as_str()).fetch_one(admin).await.unwrap();
    assert_eq!(count, 1);
    f.authority
        .check_execution(&f.lease, command.clone())
        .await
        .unwrap();

    // The approval follows the logical invocation, not the old Worker attempt.
    query("UPDATE zuno_enterprise_preview.input SET state='consumed' WHERE id=$1")
        .bind(f.job.input_id.as_str())
        .execute(admin)
        .await
        .unwrap();
    f.runtime
        .checkpoint(
            &f.lease,
            RuntimeCheckpoint {
                job_id: f.job.id.clone(),
                session_id: f.job.session_id.clone(),
                turn_id: f.job.turn_id.clone(),
                driver: "default".to_owned(),
                schema_version: 2,
                reference: json!({"eventId":"approval-checkpoint"}),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        f.authority.check_execution(&f.lease, command.clone()).await,
        Err(ApplicationError::LeaseLost)
    ));
    let resumed = f
        .runtime
        .claim(
            &WorkerInstanceId::new("replacement").unwrap(),
            LeaseDuration::new(300_000).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    let reused = f
        .authority
        .admit(&resumed.lease, command.clone())
        .await
        .unwrap();
    assert_eq!(reused.id, pending.id);
    assert_eq!(reused.state, ApprovalState::Approved);
    let checked = f
        .authority
        .check_execution(&resumed.lease, command.clone())
        .await
        .unwrap();
    assert_eq!(checked.lease.attempt_id, resumed.lease.attempt_id);
    let mut changed_resource = command;
    changed_resource.binding.resources_sha256 = "d".repeat(64);
    assert!(matches!(
        f.authority
            .check_execution(&resumed.lease, changed_resource)
            .await,
        Err(ApplicationError::Conflict)
    ));
}
