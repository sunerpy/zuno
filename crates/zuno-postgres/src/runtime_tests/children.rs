use super::*;

pub(super) async fn execution_binding(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("runtime-child-binding", "owner");
    crate::tests::install_access(admin, &actor).await;
    let parent = session(backend, &actor, "parent").await;
    let child = session(backend, &actor, "child").await;
    query("UPDATE zuno_enterprise_preview.session SET parent_id=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(child.as_str()).bind(parent.as_str())
        .execute(admin).await.unwrap();
    let runtime = backend.runtime(actor.tenant_id().clone());
    let job = runtime
        .submit(&actor, submission(&child, "child-turn", 0))
        .await
        .unwrap();
    let mut tx = scoped_transaction(&backend.pool, &actor).await.unwrap();
    let rebound = query("UPDATE zuno_enterprise_preview.agent_job SET parent_session_id=$4,subject_kind='child-session',
        subject_payload=$5 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(job.id.as_str()).bind(parent.as_str())
        .bind(json!({"kind":"childSession","sessionID":child})).execute(&mut *tx).await;
    assert!(
        rebound.is_ok(),
        "a logical child Job must keep its parent while executing in a separate child session: {rebound:?}"
    );
    tx.commit().await.unwrap();
    let claimed = runtime
        .claim(&worker("child-worker"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.job.id, job.id);
    assert_eq!(claimed.lease.session_id, child);
    assert_ne!(claimed.lease.session_id, parent);
    let parent_slot: Option<String> = query_scalar(
        "SELECT current_job_id FROM zuno_enterprise_preview.runtime_session
        WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
    )
    .bind(actor.tenant_id().as_str())
    .bind(actor.principal_id().as_str())
    .bind(parent.as_str())
    .fetch_one(admin)
    .await
    .unwrap();
    assert!(
        parent_slot.is_none(),
        "child execution cannot seize its parent's slot"
    );
}
