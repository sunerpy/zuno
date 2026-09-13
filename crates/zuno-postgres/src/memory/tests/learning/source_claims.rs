use super::*;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("source-claims");
    let workspace = WorkspaceId::new("source-claims").unwrap();
    setup(backend, admin, &actor, &workspace).await;
    let memory = PostgresMemoryBackend::new(backend.clone(), MemoryStoreLimits::default()).unwrap();
    let human = memory.for_user(actor.clone(), workspace.clone());
    human
        .request(request(
            "generation",
            MemoryCommand::SetPolicy {
                session_id: None,
                expected_revision: 0,
                use_memories: true,
                generate_private: true,
            },
        ))
        .await
        .unwrap();
    human
        .request(request(
            "automation",
            MemoryCommand::SetAutomation {
                session_id: None,
                expected_revision: 1,
                enabled: true,
            },
        ))
        .await
        .unwrap();
    let runtime =
        PostgresLearningRuntime::new(memory, actor.tenant_id().clone(), vec![grant(&workspace)])
            .unwrap();
    let root = source(backend, admin, &actor, &workspace, "claim-atomic-root").await;
    raw_sql("CREATE FUNCTION zuno_enterprise_preview.fail_source_claim() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.principal_id='source-claims' THEN RAISE EXCEPTION 'injected source claim failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER fail_source_claim BEFORE INSERT ON zuno_enterprise_preview.learning_source_claim
        FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.fail_source_claim();").execute(admin).await.unwrap();
    assert!(runtime.schedule(8).await.is_err());
    raw_sql(
        "DROP TRIGGER fail_source_claim ON zuno_enterprise_preview.learning_source_claim;
        DROP FUNCTION zuno_enterprise_preview.fail_source_claim();",
    )
    .execute(admin)
    .await
    .unwrap();
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.learning_execution
        WHERE tenant_id=$1 AND principal_id=$2 AND source_job_id=$3"
        )
        .bind(actor.tenant_id().as_str())
        .bind(actor.principal_id().as_str())
        .bind(root.as_str())
        .fetch_one(admin)
        .await
        .unwrap(),
        0,
        "failed claim rolls back learning Job and activity"
    );
    let pending: bool = query_scalar(
        "SELECT source_version>scanned_version FROM zuno_enterprise_preview.learning_root_scan
        WHERE tenant_id=$1 AND principal_id=$2 AND root_job_id=$3",
    )
    .bind(actor.tenant_id().as_str())
    .bind(actor.principal_id().as_str())
    .bind(root.as_str())
    .fetch_one(admin)
    .await
    .unwrap();
    assert!(pending, "failed scheduling cannot acknowledge the wake");
    let competing = runtime.clone();
    let (one, two) = tokio::join!(
        Box::pin(runtime.schedule(8)),
        Box::pin(competing.schedule(8))
    );
    assert_eq!(one.unwrap() + two.unwrap(), 1);
    let job = backend
        .client_learning_jobs(&actor, &workspace, Default::default())
        .await
        .unwrap()
        .items
        .remove(0);
    backend
        .cancel_learning_job(
            &actor,
            &job.id,
            zuno_application::learning_api::CancelLearning {
                request_id: RequestId::new("cancel-source-batch").unwrap(),
            },
        )
        .await
        .unwrap();
    let mut tx = scoped_transaction(&backend.pool, &actor).await.unwrap();
    crate::learning_sources::completed(&mut tx, &actor.owner(), root.as_str())
        .await
        .unwrap();
    crate::learning_sources::completed(&mut tx, &actor.owner(), root.as_str())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        runtime.schedule(8).await.unwrap(),
        0,
        "duplicate completion cannot replay a cancelled captured source"
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.learning_source_claim
        WHERE tenant_id=$1 AND principal_id=$2 AND root_job_id=$3"
        )
        .bind(actor.tenant_id().as_str())
        .bind(actor.principal_id().as_str())
        .bind(root.as_str())
        .fetch_one(admin)
        .await
        .unwrap(),
        1
    );
    let raced = source(backend, admin, &actor, &workspace, "claim-version-race").await;
    raw_sql("CREATE FUNCTION zuno_enterprise_preview.advance_source_version() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF NEW.principal_id='source-claims' THEN
            UPDATE zuno_enterprise_preview.learning_root_scan SET source_version=source_version+1
              WHERE tenant_id=NEW.tenant_id AND principal_id=NEW.principal_id AND root_job_id=NEW.root_job_id;
          END IF;
          RETURN NEW;
        END $$;
        CREATE TRIGGER advance_source_version AFTER INSERT ON zuno_enterprise_preview.learning_source_claim
        FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.advance_source_version();")
        .execute(admin).await.unwrap();
    assert_eq!(runtime.schedule(8).await.unwrap(), 1);
    raw_sql(
        "DROP TRIGGER advance_source_version ON zuno_enterprise_preview.learning_source_claim;
        DROP FUNCTION zuno_enterprise_preview.advance_source_version();",
    )
    .execute(admin)
    .await
    .unwrap();
    let pending: bool = query_scalar(
        "SELECT source_version>scanned_version FROM zuno_enterprise_preview.learning_root_scan
        WHERE tenant_id=$1 AND principal_id=$2 AND root_job_id=$3",
    )
    .bind(actor.tenant_id().as_str())
    .bind(actor.principal_id().as_str())
    .bind(raced.as_str())
    .fetch_one(admin)
    .await
    .unwrap();
    assert!(
        pending,
        "a newer completion version is not swallowed by an older scan acknowledgement"
    );
    assert_eq!(
        runtime.schedule(8).await.unwrap(),
        0,
        "the residual wake does not replay captured sources"
    );
    human
        .request(request(
            "disable",
            MemoryCommand::SetAutomation {
                session_id: None,
                expected_revision: 2,
                enabled: false,
            },
        ))
        .await
        .unwrap();
}
