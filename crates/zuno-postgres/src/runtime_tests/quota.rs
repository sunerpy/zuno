use super::*;
use zuno_application::quota::*;
use zuno_types::activity::Counter;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let alice = principal("quota-fixture", "alice");
    let bob = principal("quota-fixture", "bob");
    for actor in [&alice, &bob] {
        crate::tests::install_access(admin, actor).await;
    }
    // Fixture accounts are members by default; policy mutation requires an
    // active administrator using an approved review application.
    query("UPDATE zuno_enterprise_preview.organization_member SET role='administrator' WHERE tenant_id=$1 AND principal_id=$2")
        .bind(alice.tenant_id().as_str()).bind(alice.principal_id().as_str()).execute(admin).await.unwrap();
    let quotas = backend.quotas();
    let runtime = backend.runtime(alice.tenant_id().clone());
    let original = quotas.snapshot(&alice).await.unwrap();
    assert_eq!(original.policy.limits, QuotaLimits::default());
    let limits = QuotaLimits {
        root_sessions: 3,
        root_jobs: 1,
        child_jobs: 1,
        executions: 1,
        learning_jobs: 1,
        learning_executions: 1,
    };
    let change = ReplaceQuotaPolicy {
        request_id: RequestId::new("limits").unwrap(),
        expected_revision: original.policy.revision,
        limits: limits.clone(),
    };
    // Source fixtures permit web, while their default review application may
    // differ. Use the existing policy row rather than silently changing actor.
    query("UPDATE zuno_enterprise_preview.organization_policy SET approval_apps='[\"web\"]' WHERE tenant_id=$1")
        .bind(alice.tenant_id().as_str()).execute(admin).await.unwrap();
    assert!(quotas.replace(&bob, change.clone()).await.is_err());
    let policy = quotas.replace(&alice, change.clone()).await.unwrap();
    assert_eq!(policy.limits, limits);
    assert_eq!(quotas.replace(&alice, change).await.unwrap(), policy);
    let a1 = session(backend, &alice, "first").await;
    let a2 = session(backend, &alice, "second").await;
    let b1 = session(backend, &bob, "first").await;
    let one = submission(&a1, "a1", 0);
    let two = submission(&a2, "a2", 0);
    let (first, second) = tokio::join!(
        runtime.submit(&alice, one.clone()),
        runtime.submit(&alice, two.clone())
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let (accepted, blocked) = if let Ok(job) = first {
        (job, two)
    } else {
        (second.unwrap(), one)
    };
    assert!(matches!(
        runtime.submit(&alice, blocked.clone()).await,
        Err(ApplicationError::QuotaExceeded(QuotaResource::RootJobs))
    ));
    assert_eq!(
        runtime
            .input_version(&alice.owner(), &blocked.session_id)
            .await
            .unwrap(),
        0,
        "quota rejection must not consume input or CAS"
    );
    let request = if accepted.session_id == a1 {
        submission(&a1, "a1", 0)
    } else {
        submission(&a2, "a2", 0)
    };
    assert_eq!(
        runtime.submit(&alice, request).await.unwrap().id,
        accepted.id,
        "existing receipt survives a full quota"
    );
    let bob_job = runtime
        .submit(&bob, submission(&b1, "bob", 0))
        .await
        .unwrap();
    let q1 = worker("q1");
    let q2 = worker("q2");
    let (left, right) = tokio::join!(
        runtime.claim(&q1, duration()),
        runtime.claim(&q2, duration())
    );
    let left = left.unwrap().unwrap();
    let right = right.unwrap().unwrap();
    assert_ne!(left.lease.owner, right.lease.owner);
    assert!([&accepted.id, &bob_job.id].contains(&&left.job.id));
    let (claimed, other) = if left.lease.owner == alice.owner() {
        (left, right)
    } else {
        (right, left)
    };
    let mut increased = limits;
    increased.root_jobs = 3;
    let increased = quotas
        .replace(
            &alice,
            ReplaceQuotaPolicy {
                request_id: RequestId::new("more-roots").unwrap(),
                expected_revision: policy.revision,
                limits: increased,
            },
        )
        .await
        .unwrap();
    let pending = runtime.submit(&alice, blocked).await.unwrap();
    assert!(
        runtime
            .claim(&worker("no-spare"), duration())
            .await
            .unwrap()
            .is_none(),
        "capped owner must not acquire another execution"
    );
    consume(admin, &claimed.job).await;
    runtime
        .checkpoint(&claimed.lease, checkpoint(&claimed.job))
        .await
        .unwrap();
    let resumed = runtime
        .claim(&worker("q-resume"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.lease.owner, alice.owner());
    runtime
        .finish(
            &resumed.lease,
            JobFinish::Cancelled {
                reason: "quota fixture".to_owned(),
            },
        )
        .await
        .unwrap();
    runtime
        .finish(
            &other.lease,
            JobFinish::Cancelled {
                reason: "quota fixture".to_owned(),
            },
        )
        .await
        .unwrap();
    let remainder = runtime
        .claim(&worker("q-next"), duration())
        .await
        .unwrap()
        .unwrap();
    assert!([&pending.id, &claimed.job.id].contains(&&remainder.job.id));
    runtime
        .finish(
            &remainder.lease,
            JobFinish::Cancelled {
                reason: "quota fixture".to_owned(),
            },
        )
        .await
        .unwrap();
    // Three concurrent session creations leave exactly one durable receipt.
    let application = AgentApplication::new(Arc::new(backend.sessions(alice.clone())));
    let create = |id: &str| CreateSession {
        request_id: RequestId::new(id).unwrap(),
        workspace_id: WorkspaceId::new("runtime-workspace").unwrap(),
        title: id.to_owned(),
    };
    let (third, fourth) = tokio::join!(
        application.create_session(create("third")),
        application.create_session(create("fourth"))
    );
    assert_eq!(usize::from(third.is_ok()) + usize::from(fourth.is_ok()), 1);
    let snapshot = quotas.snapshot(&alice).await.unwrap();
    assert_eq!(
        snapshot
            .usage
            .iter()
            .find(|u| u.resource == QuotaResource::RootSessions)
            .unwrap()
            .used,
        Counter(3)
    );
    raw_sql("CREATE FUNCTION public.refuse_quota_audit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'injected quota audit failure'; END $$;
        CREATE TRIGGER refuse_quota_audit BEFORE INSERT ON zuno_enterprise_preview.organization_quota_request FOR EACH ROW EXECUTE FUNCTION public.refuse_quota_audit();")
        .execute(admin).await.unwrap();
    assert!(
        quotas
            .replace(
                &alice,
                ReplaceQuotaPolicy {
                    request_id: RequestId::new("rollback").unwrap(),
                    expected_revision: increased.revision,
                    limits: QuotaLimits::default()
                }
            )
            .await
            .is_err()
    );
    raw_sql("DROP TRIGGER refuse_quota_audit ON zuno_enterprise_preview.organization_quota_request; DROP FUNCTION public.refuse_quota_audit();")
        .execute(admin).await.unwrap();
    assert_eq!(quotas.snapshot(&alice).await.unwrap().policy, increased);
}
