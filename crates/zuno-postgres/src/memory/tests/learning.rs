use super::*;
use zuno_application::runtime::JobFinish;
use zuno_learning::{
    LearningModelEvent, LearningModelIdentity, LearningModelOutcome, LearningModelRecord,
    LearningUsage, distributed::*,
};
use zuno_types::identity::JobId;

mod child_evidence;
mod input_limits;
mod maintenance_wake;
mod quota;
mod skill;
mod source_claims;

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
    Box::pin(quota::exercise(backend, admin)).await;
    Box::pin(skill::exercise(backend, admin)).await;
    Box::pin(runtime_contracts(backend, admin)).await;
    Box::pin(maintenance_wake::exercise(backend, admin)).await;
    Box::pin(input_limits::exercise(backend, admin)).await;
    Box::pin(child_evidence::exercise(backend, admin)).await;
    Box::pin(source_claims::exercise(backend, admin)).await;
}

// Finish each independent suite before polling another large fixture. Keeping
// them inside this runtime test retains its poll frame throughout nested work.
async fn runtime_contracts(backend: &PostgresBackend, admin: &PgPool) {
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
    let page = backend
        .client_learning_jobs(&actor, &workspace, Default::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    let queued = &page.items[0];
    assert_eq!(
        queued.state,
        zuno_application::learning_api::LearningState::Queued
    );
    assert_eq!(queued.source_job_id, source_id);
    let serialized = json!(queued).to_string();
    for private in [
        "leaseToken",
        "configuration",
        "inputDigest",
        "prompt",
        "sources",
        "credential",
    ] {
        assert!(
            !serialized.contains(private),
            "private field leaked: {private}"
        );
    }
    let foreign = principal("learning-foreign");
    setup(backend, admin, &foreign, &workspace).await;
    assert!(matches!(
        backend.client_learning_job(&foreign, &queued.id).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(
        backend
            .client_learning_jobs(&foreign, &workspace, Default::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
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

    source(backend, admin, &actor, &workspace, "managed-cancellation").await;
    let cancellable = runtime
        .claim(
            WorkerInstanceId::new("cancel-worker").unwrap(),
            vec![binding.extraction.clone()],
            30000,
        )
        .await
        .unwrap()
        .unwrap();
    runtime
        .journal(prepared(&cancellable, "cancel-request"))
        .await
        .unwrap();
    let control = zuno_application::learning_api::CancelLearning {
        request_id: RequestId::new("cancel-learning").unwrap(),
    };
    assert!(matches!(
        backend
            .cancel_learning_job(&foreign, &cancellable.execution.id, control.clone())
            .await,
        Err(ApplicationError::NotFound)
    ));
    raw_sql("CREATE FUNCTION public.refuse_learning_frame() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'injected learning projection failure'; END $$;
        CREATE TRIGGER refuse_learning_frame BEFORE INSERT ON zuno_enterprise_preview.activity_frame
          FOR EACH ROW WHEN(NEW.item_id LIKE 'learning:%') EXECUTE FUNCTION public.refuse_learning_frame();")
        .execute(admin).await.unwrap();
    assert!(
        backend
            .cancel_learning_job(&actor, &cancellable.execution.id, control.clone())
            .await
            .is_err()
    );
    raw_sql("DROP TRIGGER refuse_learning_frame ON zuno_enterprise_preview.activity_frame; DROP FUNCTION public.refuse_learning_frame();")
        .execute(admin).await.unwrap();
    let before_cancel = backend
        .client_learning_job(&actor, &cancellable.execution.id)
        .await
        .unwrap();
    assert_eq!(
        before_cancel.state,
        zuno_application::learning_api::LearningState::Running
    );
    assert_eq!(
        before_cancel.budget.reserved.0,
        binding.extraction_limits.request_tokens
    );
    let cancelled = backend
        .cancel_learning_job(&actor, &cancellable.execution.id, control.clone())
        .await
        .unwrap();
    assert_eq!(
        cancelled.job.state,
        zuno_application::learning_api::LearningState::Cancelled
    );
    assert!(!cancelled.job.can_cancel);
    assert_eq!(cancelled.job.budget.reserved.0, 0);
    assert_eq!(cancelled.job.budget.unconfirmed_requests.0, 1);
    let repeated = backend
        .cancel_learning_job(&actor, &cancellable.execution.id, control.clone())
        .await
        .unwrap();
    assert_eq!(json!(cancelled), json!(repeated));
    assert!(
        backend
            .cancel_learning_job(&actor, &claimed.execution.id, control)
            .await
            .is_err(),
        "request identity cannot be rebound"
    );
    assert!(
        runtime
            .renew(cancellable.lease.clone(), 30000)
            .await
            .is_err()
    );
    runtime
        .journal(finished(&cancellable, "cancel-request"))
        .await
        .unwrap();
    let resolved = backend
        .client_learning_job(&actor, &cancellable.execution.id)
        .await
        .unwrap();
    assert_eq!(
        resolved.state,
        zuno_application::learning_api::LearningState::Cancelled
    );
    assert_eq!(resolved.budget.charged.0, 120);
    assert_eq!(resolved.budget.unconfirmed_requests.0, 0);
    use zuno_application::activity::{ActivityPersistence, HistoryQuery};
    let history = backend
        .activity(actor.clone())
        .history(&cancellable.execution.session, HistoryQuery::default())
        .await
        .unwrap();
    let item = history
        .items
        .iter()
        .find(|item| item.record.id == format!("learning:{}", cancellable.execution.id))
        .unwrap();
    assert!(matches!(
        item.record.item,
        zuno_types::activity::SessionItem::Background {
            activity_kind: Some(zuno_types::activity::BackgroundKind::MemoryExtraction),
            state: zuno_types::activity::WorkState::Cancelled,
            ..
        }
    ));
    assert!(
        item.record
            .actions
            .iter()
            .any(|action| matches!(action, zuno_types::activity::UiAction::ViewLearning { .. }))
    );
    assert!(!item.record.actions.iter().any(|action| matches!(
        action,
        zuno_types::activity::UiAction::CancelLearning { .. }
    )));
    let first = backend
        .client_learning_jobs(
            &actor,
            &workspace,
            zuno_application::learning_api::LearningPageRequest {
                limit: zuno_application::PageSize::new(1).unwrap(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let second = backend
        .client_learning_jobs(
            &actor,
            &workspace,
            zuno_application::learning_api::LearningPageRequest {
                limit: zuno_application::PageSize::new(1).unwrap(),
                before: first.before.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(second.items.len(), 1);
    assert_ne!(first.items[0].id, second.items[0].id);

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
