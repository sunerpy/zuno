use super::*;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let alice = principal("runtime-recovery", "alice");
    let bob = principal("runtime-recovery", "bob");
    let store = backend.runtime(alice.tenant_id().clone());
    let uncertain_session = session(backend, &alice, "uncertain").await;
    let uncertain = store
        .submit(&alice, submission(&uncertain_session, "uncertain", 0))
        .await
        .unwrap();
    let lease = store
        .claim(&worker("lost"), duration())
        .await
        .unwrap()
        .unwrap()
        .lease;
    consume(admin, &uncertain).await;
    store
        .submit(&alice, submission(&uncertain_session, "after-uncertain", 1))
        .await
        .unwrap();
    query("UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=0 WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
        .bind(alice.tenant_id().as_str()).bind(alice.principal_id().as_str()).bind(uncertain_session.as_str()).execute(admin).await.unwrap();
    assert!(matches!(
        store.renew(&lease, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
    assert!(
        store
            .claim(&worker("reconcile"), duration())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get(&alice.owner(), &uncertain.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Uncertain
    );
    let available = session(backend, &bob, "after-other-worker-loss").await;
    let available_job = store
        .submit(&bob, submission(&available, "available", 0))
        .await
        .unwrap();
    let other = store
        .claim(&worker("live"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(other.job.id, available_job.id);
    store
        .finish(
            &other.lease,
            JobFinish::Cancelled {
                reason: "fixture complete".to_owned(),
            },
        )
        .await
        .unwrap();

    let atomic_session = session(backend, &bob, "atomic").await;
    raw_sql(
        "CREATE FUNCTION zuno_enterprise_preview.refuse_runtime_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.type='runtime.job.admitted' THEN RAISE EXCEPTION 'injected runtime audit failure'; END IF; RETURN NEW; END $$;
         CREATE TRIGGER refuse_runtime_audit BEFORE INSERT ON zuno_enterprise_preview.event
         FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.refuse_runtime_audit();",
    ).execute(admin).await.unwrap();
    assert!(
        store
            .submit(&bob, submission(&atomic_session, "atomic", 0))
            .await
            .is_err()
    );
    assert_eq!(
        store
            .input_version(&bob.owner(), &atomic_session)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.agent_job WHERE parent_session_id=$1"
        )
        .bind(atomic_session.as_str())
        .fetch_one(admin)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.input WHERE session_id=$1"
        )
        .bind(atomic_session.as_str())
        .fetch_one(admin)
        .await
        .unwrap(),
        0
    );
    raw_sql("DROP TRIGGER refuse_runtime_audit ON zuno_enterprise_preview.event; DROP FUNCTION zuno_enterprise_preview.refuse_runtime_audit()")
        .execute(admin).await.unwrap();
    store
        .submit(&bob, submission(&atomic_session, "atomic", 0))
        .await
        .unwrap();

    // Bounded owner catalog scans must progress past more than one empty page.
    let active = principal("runtime-fairness", "zz-active");
    let active_session = session(backend, &active, "fairness").await;
    let fair = backend.runtime(active.tenant_id().clone());
    let ready = fair
        .submit(&active, submission(&active_session, "ready", 0))
        .await
        .unwrap();
    for index in 0..65 {
        query("INSERT INTO zuno_enterprise_preview.runtime_owner_schedule(tenant_id,principal_id) VALUES($1,$2)")
            .bind(active.tenant_id().as_str()).bind(format!("idle-{index:03}")).execute(admin).await.unwrap();
    }
    assert!(
        fair.claim(&worker("first-page"), duration())
            .await
            .unwrap()
            .is_none()
    );
    let claimed = fair
        .claim(&worker("second-page"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.job.id, ready.id);
    fair.finish(
        &claimed.lease,
        JobFinish::Cancelled {
            reason: "fixture complete".to_owned(),
        },
    )
    .await
    .unwrap();
}
