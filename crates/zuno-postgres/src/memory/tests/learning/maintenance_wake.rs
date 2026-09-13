use super::*;

async fn record_output(
    runtime: &PostgresLearningRuntime,
    claimed: &ClaimedLearning,
    id: &str,
    output: Value,
) -> LearningCompletion {
    let operation = match claimed.execution.input {
        LearningInput::Extraction(_) => "learning.extraction",
        LearningInput::Maintenance(_) => "learning.memory_consolidation",
    };
    let mut request = prepared(claimed, id);
    request.record.operation = operation.to_owned();
    runtime.journal(request).await.unwrap();
    let mut outcome = finished(claimed, id);
    outcome.record.operation = operation.to_owned();
    let text = output.to_string();
    let LearningModelEvent::Outcome { outcome: value, .. } = &mut outcome.record.event else {
        unreachable!()
    };
    *value = LearningModelOutcome::Completed {
        output_digest: zuno_db::learning_source::digest(&text),
        output: text.clone(),
        tool_calls: Vec::new(),
    };
    runtime.journal(outcome).await.unwrap();
    LearningCompletion {
        lease: claimed.lease.clone(),
        result: LearningOutput::decode(claimed.execution.input.phase(), &text).unwrap(),
    }
}

pub(super) async fn complete_output(
    runtime: &PostgresLearningRuntime,
    claimed: &ClaimedLearning,
    id: &str,
    output: Value,
) {
    let completion = Box::pin(record_output(runtime, claimed, id, output)).await;
    Box::pin(runtime.complete(completion)).await.unwrap();
}

pub(super) async fn manual_note(human: &PostgresMemoryService, id: &str, text: &str) {
    let staged = candidate(human.request(request(id, change(text))).await.unwrap());
    human
        .request(request(
            &format!("{id}-apply"),
            MemoryCommand::Apply {
                candidate_id: staged.id.clone(),
                expected_state: staged.state_digest,
            },
        ))
        .await
        .unwrap();
}

pub(super) async fn claim(
    runtime: &PostgresLearningRuntime,
    binding: &MemoryLearningGrant,
) -> ClaimedLearning {
    Box::pin(runtime.claim(
        WorkerInstanceId::new("maintenance-wake-worker").unwrap(),
        vec![binding.maintenance.clone()],
        30000,
    ))
    .await
    .unwrap()
    .expect("pending learning work")
}

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("maintenance-wake");
    let workspace = WorkspaceId::new("maintenance-wake").unwrap();
    setup(backend, admin, &actor, &workspace).await;
    let memory = PostgresMemoryBackend::new(backend.clone(), MemoryStoreLimits::default()).unwrap();
    let human = memory.for_user(actor.clone(), workspace.clone());
    let binding = grant(&workspace);
    let runtime =
        PostgresLearningRuntime::new(memory, actor.tenant_id().clone(), vec![binding.clone()])
            .unwrap();
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
    source(
        backend,
        admin,
        &actor,
        &workspace,
        "maintenance-wake-source",
    )
    .await;
    let extraction = claim(&runtime, &binding).await;
    let LearningInput::Extraction(input) = &extraction.execution.input else {
        panic!("extraction");
    };
    let reference = &input.sources[0].reference_id;
    complete_output(&runtime, &extraction, "extract", json!({
        "experiences":[{"kind":"user_correction","title":"Validation",
            "summary":"Use cargo test for validation","resolution":null,"confidence":1.0,
            "evidence":[{"kind":"user","source_id":reference,"excerpt":"Use cargo test for validation"}]}],
        "memories":[]
    })).await;
    let initial = claim(&runtime, &binding).await;
    assert!(matches!(
        initial.execution.input,
        LearningInput::Maintenance(_)
    ));
    complete_output(&runtime, &initial, "initial", json!({"updates":[]})).await;
    assert_eq!(
        runtime.schedule(8).await.unwrap(),
        0,
        "settled input cannot wake itself"
    );

    manual_note(&human, "correction", "Run cargo clippy after cargo test.").await;
    let restarted = PostgresLearningRuntime::new(
        PostgresMemoryBackend::new(backend.clone(), MemoryStoreLimits::default()).unwrap(),
        actor.tenant_id().clone(),
        vec![binding.clone()],
    )
    .unwrap();
    let (one, two) = tokio::join!(
        Box::pin(runtime.schedule(8)),
        Box::pin(restarted.schedule(8)),
    );
    assert_eq!(
        one.unwrap() + two.unwrap(),
        1,
        "two control-plane schedulers reconstruct exactly one maintenance wake without a new turn"
    );
    assert_eq!(
        runtime.schedule(8).await.unwrap(),
        0,
        "one pending batch per workspace"
    );
    let changed = claim(&runtime, &binding).await;
    assert_ne!(initial.execution.id, changed.execution.id);
    let LearningInput::Maintenance(input) = &changed.execution.input else {
        panic!("maintenance");
    };
    assert!(
        json!(input)
            .to_string()
            .contains("Run cargo clippy after cargo test.")
    );
    complete_output(&runtime, &changed, "changed", json!({"updates":[]})).await;
    assert_eq!(
        runtime.schedule(8).await.unwrap(),
        0,
        "unchanged correction stays consumed"
    );

    manual_note(&human, "queued-old", "Keep migration fixtures.").await;
    assert_eq!(runtime.schedule(8).await.unwrap(), 1);
    let queued = backend
        .client_learning_jobs(&actor, &workspace, Default::default())
        .await
        .unwrap()
        .items
        .remove(0);
    manual_note(&human, "queued-new", "Keep rollback evidence.").await;
    assert!(
        runtime
            .claim(
                WorkerInstanceId::new("stale-plan-worker").unwrap(),
                vec![binding.maintenance.clone()],
                30000
            )
            .await
            .unwrap()
            .is_none(),
        "stale queued snapshots are retired before model admission"
    );
    let skipped = backend
        .client_learning_job(&actor, &queued.id)
        .await
        .unwrap();
    assert_eq!(
        skipped.state,
        zuno_application::learning_api::LearningState::Skipped
    );
    assert_eq!(skipped.budget.model_requests.0, 0);
    assert_eq!(runtime.schedule(8).await.unwrap(), 1);
    let fresh = claim(&runtime, &binding).await;
    let stale_completion =
        record_output(&runtime, &fresh, "in-flight", json!({"updates":[]})).await;
    manual_note(&human, "in-flight-change", "Retain authoritative receipts.").await;
    assert!(
        runtime.complete(stale_completion).await.is_err(),
        "in-flight old revisions cannot apply"
    );
    runtime
        .stop(
            fresh.lease.clone(),
            LearningStop::Failed {
                code: "learning_settlement".to_owned(),
                detail: "Memory revisions changed".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        backend
            .client_learning_job(&actor, &fresh.execution.id)
            .await
            .unwrap()
            .budget
            .charged
            .0,
        120
    );
    assert_eq!(runtime.schedule(8).await.unwrap(), 1);
    let replacement = claim(&runtime, &binding).await;
    assert_ne!(replacement.execution.id, fresh.execution.id);
    let LearningInput::Maintenance(input) = &replacement.execution.input else {
        panic!("maintenance");
    };
    let evidence = input.experiences[0]["id"].clone();
    complete_output(&runtime, &replacement, "replacement", json!({"updates":[{
        "scope":"project","action":"add","content":"Keep the learned validation preference.","old_text":null,
        "reason":"Supported validation preference","confidence":1.0,"evidence_ids":[evidence]
    }]})).await;
    assert_eq!(
        runtime.schedule(8).await.unwrap(),
        0,
        "own maintenance commit does not reschedule"
    );
    assert!(
        contents(&human)
            .await
            .contains("Keep the learned validation preference.")
    );
    // Source loss suppresses recall immediately even before maintenance can run.
    query("UPDATE zuno_enterprise_preview.input SET prompt=jsonb_set(prompt,'{prompt,text}','\"source no longer available\"')
        WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str())
        .bind(extraction.execution.session.as_str()).execute(admin).await.unwrap();
    assert!(
        !contents(&human)
            .await
            .contains("Keep the learned validation preference.")
    );
    assert_eq!(
        runtime.schedule(8).await.unwrap(),
        1,
        "source invalidation wakes maintenance without a turn"
    );
    let retraction = claim(&runtime, &binding).await;
    complete_output(&runtime, &retraction, "retract", json!({"updates":[{
        "scope":"project","action":"remove","content":null,"old_text":"Keep the learned validation preference.",
        "reason":"Source invalidated","confidence":1.0,"evidence_ids":[]
    }]})).await;
    assert_eq!(runtime.schedule(8).await.unwrap(), 0);
    // Restore only the fixture's source so the cancellation case has valid evidence.
    query("UPDATE zuno_enterprise_preview.input SET prompt=jsonb_set(prompt,'{prompt,text}','\"Use cargo test for validation\"')
        WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str())
        .bind(extraction.execution.session.as_str()).execute(admin).await.unwrap();

    manual_note(&human, "cancel-source", "Review generated schemas.").await;
    assert_eq!(runtime.schedule(8).await.unwrap(), 1);
    let pending = backend
        .client_learning_jobs(
            &actor,
            &workspace,
            zuno_application::learning_api::LearningPageRequest {
                stage: Some(zuno_application::learning_api::LearningStage::Maintenance),
                state: Some(zuno_application::learning_api::LearningState::Queued),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    backend
        .cancel_learning_job(
            &actor,
            &pending.items[0].id,
            zuno_application::learning_api::CancelLearning {
                request_id: RequestId::new("cancel-maintenance").unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.schedule(8).await.unwrap(),
        0,
        "cancelled identical input is not rescheduled"
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
    manual_note(&human, "disabled-source", "Prefer targeted checks.").await;
    assert_eq!(
        runtime.schedule(8).await.unwrap(),
        0,
        "manual edits cannot restore revoked consent"
    );
}
