use super::*;
use zuno_application::runtime::JobFinish;
use zuno_learning::{
    LearningModelEvent, LearningModelIdentity, LearningModelOutcome, LearningModelRecord,
    LearningUsage, distributed::*,
};
use zuno_types::identity::JobId;

fn configured() -> ConfigurationRef {
    ConfigurationRef {
        id: ConfigurationId::new("learning-root").unwrap(),
        version: 1,
        sha256: "a".repeat(64),
    }
}
fn grant(workspace: &WorkspaceId) -> MemoryLearningGrant {
    let model = ConfigurationRef {
        id: ConfigurationId::new("learning-model").unwrap(),
        version: 1,
        sha256: "b".repeat(64),
    };
    let limits = LearningExecutionLimits {
        maximum_input_bytes: 32768,
        maximum_output_tokens: 512,
        request_tokens: 40000,
        total_tokens: 120000,
        maximum_attempts: 3,
        duration_ms: 60000,
    };
    let identity = LearningModelIdentity {
        provider_id: "fixture".to_owned(),
        model_id: "model".to_owned(),
        wire_id: "model".to_owned(),
    };
    MemoryLearningGrant {
        source: configured(),
        extraction: model.clone(),
        maintenance: model,
        workspace: workspace.clone(),
        extraction_limits: limits.clone(),
        maintenance_limits: limits,
        extraction_model: identity.clone(),
        maintenance_model: identity,
    }
}
async fn source(
    backend: &PostgresBackend,
    admin: &PgPool,
    actor: &PrincipalScope,
    workspace: &WorkspaceId,
    id: &str,
) -> JobId {
    let session = AgentApplication::new(Arc::new(backend.sessions(actor.clone())))
        .create_session(CreateSession {
            request_id: RequestId::new(id).unwrap(),
            workspace_id: workspace.clone(),
            title: "Learning source".to_owned(),
        })
        .await
        .unwrap()
        .id;
    let runtime = backend.runtime(actor.tenant_id().clone());
    let job = runtime
        .submit(
            actor,
            JobSubmission {
                session_id: session,
                request_id: RequestId::new(id).unwrap(),
                expected_input_version: 0,
                text: "Use cargo test for validation".to_owned(),
                configuration: configured(),
                selection: None,
            },
        )
        .await
        .unwrap();
    let claimed = runtime
        .claim(
            &WorkerInstanceId::new("source-worker").unwrap(),
            LeaseDuration::new(30000).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.job.id, job.id);
    query("UPDATE zuno_enterprise_preview.input SET state='consumed' WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(job.input_id.as_str()).execute(admin).await.unwrap();
    runtime
        .finish(
            &claimed.lease,
            JobFinish::Completed {
                result: json!({"source":"closed"}),
            },
        )
        .await
        .unwrap();
    job.id
}
fn prepared(claimed: &ClaimedLearning, id: &str) -> LearningJournalRequest {
    let input = json!({"messages":[{"role":"user","content":"source"}],"tools":[],"parameters":{"maxTokens":512}});
    LearningJournalRequest {
        lease: claimed.lease.clone(),
        record: LearningModelRecord {
            session_id: claimed.execution.session.to_string(),
            operation: "learning.extraction".to_owned(),
            event: LearningModelEvent::Request {
                request_id: id.to_owned(),
                extractor_version: zuno_learning::LEARNING_EXTRACTOR_VERSION.to_owned(),
                model: LearningModelIdentity {
                    provider_id: "fixture".to_owned(),
                    model_id: "model".to_owned(),
                    wire_id: "model".to_owned(),
                },
                prompt_digest: zuno_db::learning_source::digest(&input.to_string()),
                request: input,
                tools: Vec::new(),
            },
        },
    }
}
fn finished(claimed: &ClaimedLearning, id: &str) -> LearningJournalRequest {
    let output = json!({"experiences":[],"memories":[]}).to_string();
    LearningJournalRequest {
        lease: claimed.lease.clone(),
        record: LearningModelRecord {
            session_id: claimed.execution.session.to_string(),
            operation: "learning.extraction".to_owned(),
            event: LearningModelEvent::Outcome {
                request_id: id.to_owned(),
                outcome: LearningModelOutcome::Completed {
                    output_digest: zuno_db::learning_source::digest(&output),
                    output,
                    tool_calls: Vec::new(),
                },
                usage: LearningUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                    total_tokens: 120,
                    accounted: true,
                    provider_attempts: 1,
                    ..Default::default()
                },
            },
        },
    }
}

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("learning-runtime");
    let workspace = WorkspaceId::new("learning-workspace").unwrap();
    setup(backend, admin, &actor, &workspace).await;
    let memory = PostgresMemoryBackend::new(backend.clone(), MemoryStoreLimits::default()).unwrap();
    let human = memory.for_user(actor.clone(), workspace.clone());
    let binding = grant(&workspace);
    let runtime = PostgresLearningRuntime::new(
        memory.clone(),
        actor.tenant_id().clone(),
        vec![binding.clone()],
    )
    .unwrap();
    source(backend, admin, &actor, &workspace, "before-consent").await;
    assert_eq!(runtime.schedule(8).await.unwrap(), 0);
    let policy = human
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
    assert!(matches!(policy,MemoryReply::Policy{policy} if !policy.automatic_private));
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
    let source_id = source(backend, admin, &actor, &workspace, "after-consent").await;
    assert_eq!(runtime.schedule(8).await.unwrap(), 1);
    assert_eq!(runtime.schedule(8).await.unwrap(), 0);
    let (one, two) = tokio::join!(
        runtime.claim(
            WorkerInstanceId::new("learning-one").unwrap(),
            vec![binding.extraction.clone()],
            30000
        ),
        runtime.claim(
            WorkerInstanceId::new("learning-two").unwrap(),
            vec![binding.extraction.clone()],
            30000
        ),
    );
    let one = one.unwrap();
    let two = two.unwrap();
    assert_eq!(usize::from(one.is_some()) + usize::from(two.is_some()), 1);
    let claimed = one.or(two).unwrap();
    let mut prepared_request = prepared(&claimed, "request-one");
    runtime.journal(prepared_request.clone()).await.unwrap();
    runtime.journal(prepared_request.clone()).await.unwrap();
    if let LearningModelEvent::Request { request, .. } = &mut prepared_request.record.event {
        request["parameters"]["maxTokens"] = 256.into();
    }
    assert!(runtime.journal(prepared_request).await.is_err());
    runtime
        .journal(finished(&claimed, "request-one"))
        .await
        .unwrap();
    runtime
        .journal(finished(&claimed, "request-one"))
        .await
        .unwrap();
    let charged: i64 = query_scalar(
        "SELECT charged_tokens FROM zuno_enterprise_preview.learning_execution
        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
    )
    .bind(actor.tenant_id().as_str())
    .bind(actor.principal_id().as_str())
    .bind(claimed.execution.id.as_str())
    .fetch_one(admin)
    .await
    .unwrap();
    assert_eq!(
        charged, 120,
        "duplicate receipts cannot double-charge usage"
    );
    runtime
        .stop(
            claimed.lease.clone(),
            LearningStop::Retry {
                after_ms: Some(37000),
                detail: "lost completion response".to_owned(),
            },
        )
        .await
        .unwrap();
    let ready:i64=query_scalar("SELECT ready_at-time_updated FROM zuno_enterprise_preview.learning_execution e
        JOIN zuno_enterprise_preview.learning_job j ON j.tenant_id=e.tenant_id AND j.principal_id=e.principal_id AND j.id=e.job_id
        WHERE e.tenant_id=$1 AND e.principal_id=$2 AND e.job_id=$3").bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str())
        .bind(claimed.execution.id.as_str()).fetch_one(admin).await.unwrap();
    assert!(ready >= 37000);
    query("UPDATE zuno_enterprise_preview.learning_execution SET ready_at=0 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(claimed.execution.id.as_str()).execute(admin).await.unwrap();
    let resumed = runtime
        .claim(
            WorkerInstanceId::new("replacement").unwrap(),
            vec![binding.extraction.clone()],
            30000,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(resumed.lease.epoch > claimed.lease.epoch);
    assert_eq!(resumed.execution.tokens_charged, 120);
    assert!(
        resumed.execution.cached_output.is_some(),
        "a durable model result survives its Worker's loss"
    );
    assert!(runtime.renew(claimed.lease.clone(), 30000).await.is_err());
    let result = LearningCompletion {
        lease: resumed.lease.clone(),
        result: resumed.execution.cached_output.clone().unwrap(),
    };
    runtime.complete(result.clone()).await.unwrap();
    runtime.complete(result).await.unwrap();
    let source_count: i64 = query_scalar(
        "SELECT count(*) FROM zuno_enterprise_preview.learning_execution WHERE source_job_id=$1",
    )
    .bind(source_id.as_str())
    .fetch_one(admin)
    .await
    .unwrap();
    assert_eq!(
        source_count, 1,
        "empty extraction cannot fabricate maintenance work"
    );

    source(backend, admin, &actor, &workspace, "lost-worker").await;
    let lost = runtime
        .claim(
            WorkerInstanceId::new("lost-worker").unwrap(),
            vec![binding.extraction.clone()],
            30000,
        )
        .await
        .unwrap()
        .unwrap();
    runtime
        .journal(prepared(&lost, "lost-request"))
        .await
        .unwrap();
    query(
        "UPDATE zuno_enterprise_preview.learning_job SET lease_expires=0
        WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    )
    .bind(actor.tenant_id().as_str())
    .bind(actor.principal_id().as_str())
    .bind(lost.execution.id.as_str())
    .execute(admin)
    .await
    .unwrap();
    assert!(
        runtime
            .claim(
                WorkerInstanceId::new("recover").unwrap(),
                vec![binding.extraction.clone()],
                30000
            )
            .await
            .unwrap()
            .is_none(),
        "an unconfirmed request persists a positive retry delay"
    );
    let state:Value=query_scalar("SELECT jsonb_build_object('charged',charged_tokens,'reserved',reserved_tokens,
        'future',ready_at>floor(extract(epoch FROM clock_timestamp())*1000)::bigint)
        FROM zuno_enterprise_preview.learning_execution WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(lost.execution.id.as_str())
        .fetch_one(admin).await.unwrap();
    assert_eq!(state["charged"], binding.extraction_limits.request_tokens);
    assert_eq!(state["reserved"], 0);
    assert_eq!(state["future"], true);
    runtime
        .journal(finished(&lost, "lost-request"))
        .await
        .unwrap();
    query("UPDATE zuno_enterprise_preview.learning_execution SET ready_at=0,attempt=3,charged_tokens=$4
        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(lost.execution.id.as_str())
        .bind(binding.extraction_limits.total_tokens as i64).execute(admin).await.unwrap();
    let recovered = runtime
        .claim(
            WorkerInstanceId::new("recover").unwrap(),
            vec![binding.extraction.clone()],
            30000,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        recovered.execution.cached_output.is_some(),
        "finishing a recorded result needs no new model budget"
    );
    runtime
        .complete(LearningCompletion {
            lease: recovered.lease,
            result: recovered.execution.cached_output.unwrap(),
        })
        .await
        .unwrap();

    source(backend, admin, &actor, &workspace, "revoked-in-flight").await;
    let active = runtime
        .claim(
            WorkerInstanceId::new("revoked-worker").unwrap(),
            vec![binding.extraction],
            30000,
        )
        .await
        .unwrap()
        .unwrap();
    runtime
        .journal(prepared(&active, "revoked-request"))
        .await
        .unwrap();
    human
        .request(request(
            "disable-auto",
            MemoryCommand::SetAutomation {
                session_id: None,
                expected_revision: 2,
                enabled: false,
            },
        ))
        .await
        .unwrap();
    assert!(runtime.renew(active.lease.clone(), 30000).await.is_err());
    runtime
        .journal(finished(&active, "revoked-request"))
        .await
        .unwrap();
    assert!(
        runtime
            .complete(LearningCompletion {
                lease: active.lease,
                result: LearningOutput::Extraction(zuno_learning::LearningExtraction {
                    experiences: Vec::new(),
                    memories: Vec::new()
                },)
            })
            .await
            .is_err()
    );
    let state:String=query_scalar("SELECT status FROM zuno_enterprise_preview.learning_job WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(active.execution.id.as_str()).fetch_one(admin).await.unwrap();
    assert_eq!(
        state, "skipped",
        "a truthful late model receipt cannot restore revoked automation"
    );
}
