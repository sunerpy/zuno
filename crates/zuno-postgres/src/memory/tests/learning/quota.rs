use super::*;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let workspace = WorkspaceId::new("quota-learning").unwrap();
    let alice = principal("quota-learning-a");
    let bob = principal("quota-learning-b");
    let memory = PostgresMemoryBackend::new(backend.clone(), Default::default()).unwrap();
    for actor in [&alice, &bob] {
        setup(backend, admin, actor, &workspace).await;
        let human = memory.for_user(actor.clone(), workspace.clone());
        human
            .request(request(
                "generate",
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
                "automatic",
                MemoryCommand::SetAutomation {
                    session_id: None,
                    expected_revision: 1,
                    enabled: true,
                },
            ))
            .await
            .unwrap();
    }
    let binding = grant(&workspace);
    let runtime =
        PostgresLearningRuntime::new(memory, alice.tenant_id().clone(), vec![binding.clone()])
            .unwrap();
    for (actor, id) in [
        (&alice, "quota-a-first"),
        (&alice, "quota-a-second"),
        (&bob, "quota-b-first"),
    ] {
        Box::pin(source(backend, admin, actor, &workspace, id)).await;
    }
    query("UPDATE zuno_enterprise_preview.organization_quota SET limits=jsonb_set(jsonb_set(limits,'{learningJobs}','1'),'{learningExecutions}','1') WHERE tenant_id=$1")
        .bind(alice.tenant_id().as_str()).execute(admin).await.unwrap();
    assert_eq!(
        Box::pin(runtime.schedule(8)).await.unwrap(),
        2,
        "one queued Job per owner"
    );
    assert_eq!(
        Box::pin(runtime.schedule(8)).await.unwrap(),
        0,
        "full queues retain the next source without acknowledging it"
    );
    let first = Box::pin(runtime.claim(
        WorkerInstanceId::new("quota-learning-worker").unwrap(),
        vec![binding.extraction.clone()],
        30000,
    ))
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.lease.owner, alice.owner());
    let first_id = first.execution.id.clone();
    backend
        .cancel_learning_job(
            &alice,
            &first_id,
            zuno_application::learning_api::CancelLearning {
                request_id: RequestId::new("cancel-first").unwrap(),
            },
        )
        .await
        .unwrap();
    // Cancellation frees the first owner's queue, but that owner's next Job
    // must not jump ahead of the already waiting second owner.
    assert_eq!(Box::pin(runtime.schedule(8)).await.unwrap(), 1);
    let second = Box::pin(runtime.claim(
        WorkerInstanceId::new("quota-learning-worker").unwrap(),
        vec![binding.extraction.clone()],
        30000,
    ))
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.lease.owner, bob.owner());
    let third = Box::pin(runtime.claim(
        WorkerInstanceId::new("quota-learning-other").unwrap(),
        vec![binding.extraction.clone()],
        30000,
    ))
    .await
    .unwrap()
    .unwrap();
    assert_eq!(third.lease.owner, alice.owner());
    assert_ne!(third.execution.id, first_id);
    assert!(
        Box::pin(runtime.claim(
            WorkerInstanceId::new("quota-learning-full").unwrap(),
            vec![binding.extraction],
            30000
        ))
        .await
        .unwrap()
        .is_none()
    );
    for (actor, job) in [(&bob, second.execution.id), (&alice, third.execution.id)] {
        backend
            .cancel_learning_job(
                actor,
                &job,
                zuno_application::learning_api::CancelLearning {
                    request_id: RequestId::new(format!("finish-{job}")).unwrap(),
                },
            )
            .await
            .unwrap();
    }
    // Other Memory contracts share this fixture tenant. Restore policy without
    // discarding queue history or any source claims.
    query("UPDATE zuno_enterprise_preview.organization_quota SET limits=$2 WHERE tenant_id=$1")
        .bind(alice.tenant_id().as_str())
        .bind(json!(zuno_application::quota::QuotaLimits::default()))
        .execute(admin)
        .await
        .unwrap();
    Box::pin(deferred_maintenance(backend, admin)).await;
}

async fn deferred_maintenance(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("quota-maintenance");
    let workspace = WorkspaceId::new("quota-maintenance").unwrap();
    setup(backend, admin, &actor, &workspace).await;
    let memory = PostgresMemoryBackend::new(backend.clone(), Default::default()).unwrap();
    let human = memory.for_user(actor.clone(), workspace.clone());
    human
        .request(request(
            "generate",
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
            "automatic",
            MemoryCommand::SetAutomation {
                session_id: None,
                expected_revision: 1,
                enabled: true,
            },
        ))
        .await
        .unwrap();
    let binding = grant(&workspace);
    let runtime =
        PostgresLearningRuntime::new(memory, actor.tenant_id().clone(), vec![binding.clone()])
            .unwrap();
    for id in ["first", "second"] {
        Box::pin(source(backend, admin, &actor, &workspace, id)).await;
    }
    Box::pin(runtime.schedule(8)).await.unwrap();
    let claimed = Box::pin(runtime.claim(
        WorkerInstanceId::new("quota-maintenance-worker").unwrap(),
        vec![binding.extraction.clone()],
        30000,
    ))
    .await
    .unwrap()
    .unwrap();
    let remaining: String=query_scalar("SELECT id FROM zuno_enterprise_preview.learning_job WHERE tenant_id=$1 AND principal_id=$2 AND status='queued' LIMIT 1")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).fetch_one(admin).await.unwrap();
    query("UPDATE zuno_enterprise_preview.organization_quota SET limits=jsonb_set(limits,'{learningJobs}','1') WHERE tenant_id=$1")
        .bind(actor.tenant_id().as_str()).execute(admin).await.unwrap();
    let LearningInput::Extraction(input) = &claimed.execution.input else {
        panic!("extraction");
    };
    let reference = &input.sources[0].reference_id;
    Box::pin(maintenance_wake::complete_output(&runtime,&claimed,"quota-extract",json!({
        "experiences":[{"kind":"user_correction","title":"Validation","summary":"Use cargo test for validation","resolution":null,"confidence":1.0,
            "evidence":[{"kind":"user","source_id":reference,"excerpt":"Use cargo test for validation"}]}],"memories":[]
    }))).await;
    assert_eq!(
        backend
            .client_learning_job(&actor, &claimed.execution.id)
            .await
            .unwrap()
            .state,
        zuno_application::learning_api::LearningState::Completed
    );
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.learning_execution WHERE tenant_id=$1 AND principal_id=$2 AND phase='maintenance'")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).fetch_one(admin).await.unwrap(),0);
    backend
        .cancel_learning_job(
            &actor,
            &JobId::new(remaining).unwrap(),
            zuno_application::learning_api::CancelLearning {
                request_id: RequestId::new("free-queue").unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        Box::pin(runtime.schedule(8)).await.unwrap(),
        1,
        "durable maintenance wake remains after queue capacity returns"
    );
    let maintenance = Box::pin(runtime.claim(
        WorkerInstanceId::new("quota-maintenance-worker").unwrap(),
        vec![binding.maintenance],
        30000,
    ))
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        maintenance.execution.input,
        LearningInput::Maintenance(_)
    ));
    Box::pin(maintenance_wake::complete_output(
        &runtime,
        &maintenance,
        "quota-maintain",
        json!({"updates":[]}),
    ))
    .await;
    query("UPDATE zuno_enterprise_preview.organization_quota SET limits=$2 WHERE tenant_id=$1")
        .bind(actor.tenant_id().as_str())
        .bind(json!(zuno_application::quota::QuotaLimits::default()))
        .execute(admin)
        .await
        .unwrap();
}
