//! Actual kernel execution against PostgreSQL; this is not HTTP/Docker evidence.

use crate::{PostgresBackend, bootstrap_organization};
use async_trait::async_trait;
use futures::stream;
use serde_json::{Value, json};
use sqlx_core::raw_sql::raw_sql;
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::PgPool;
use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use zuno_application::runtime::{
    ConfigurationRef, JobPhase, JobSubmission, LeaseDuration, RuntimeStore,
};
use zuno_application::{AgentApplication, CreateSession};
use zuno_db::message::{MessageRecord, PartRecord};
use zuno_engine::advance::{AdvanceOutcome, AdvanceRequest, CheckpointRef};
use zuno_engine::dispatch::{AuthorizationPolicy, ToolRegistryDispatcher};
use zuno_engine::interrupt::InterruptSignal;
use zuno_engine::r#loop::{
    AgentModelResolver, ResolvedAgent, ResolvedModel, RunTurnRequest, TurnContext, advance_turn,
    event_channel,
};
use zuno_engine::state::{InputMaterialization, TurnPersistence, TurnStateScope};
use zuno_error::{ProviderError, ToolError};
use zuno_llm::cache::{DynamicContext, McpToolStatus};
use zuno_llm::event::{FinishReason, PromptAccounting, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};
use zuno_permission::enterprise::OrganizationPolicy;
use zuno_tool::{AllowAll, Tool, ToolContext, ToolOutput};
use zuno_types::identity::*;

mod approval_wait;
#[path = "turn_tests/waits.rs"]
mod waits;

#[derive(Debug)]
struct Script {
    replies: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: AtomicUsize,
}

impl Provider for Script {
    fn id(&self) -> &str {
        "turn-test"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }
    fn stream(&self, _request: CompletionRequest) -> ProviderStream<'_> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let events = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted reply");
        Box::pin(stream::iter(events.into_iter().map(Ok::<_, ProviderError>)))
    }
}

struct Resolver;
impl AgentModelResolver for Resolver {
    fn resolve_agent(&self, name: &str) -> Option<ResolvedAgent> {
        (name == "build").then(|| {
            ResolvedAgent::new("build", "Read the fixture and answer.")
                .with_max_steps(NonZeroU32::new(4).unwrap())
        })
    }
    fn resolve_model(&self, provider: &str, model: &str) -> Option<ResolvedModel> {
        (provider == "turn-test" && model == "model")
            .then(|| ResolvedModel::new(Spec::new("turn-test"), "model", ApiSurface::Default))
    }
}

struct Inspect(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Inspect {
    fn id(&self) -> &str {
        "inspect"
    }
    fn description(&self) -> &str {
        "Read the in-memory test fixture."
    }
    fn raw_parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn execute(&self, _args: Value, _context: ToolContext) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("Fixture", "Observed fixture"))
    }
}

fn usage() -> StreamEvent {
    StreamEvent::TokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(10),
        reasoning_tokens: Some(2),
        cache_read_input_tokens: Some(20),
        cache_write_input_tokens: Some(5),
        accounting: PromptAccounting::CacheBesideInput,
    }
}

pub(crate) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    waits::exercise(backend, admin, migrator).await;
    approval_wait::exercise(backend, admin, migrator).await;
    let actor = PrincipalScope::new(
        TenantId::new("turn-contract").unwrap(),
        PrincipalId::new("alice").unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("enterprise-web").unwrap()),
        NonZeroU64::MIN,
    );
    let app = actor.client_id().unwrap().clone();
    bootstrap_organization(
        migrator,
        &OrganizationPolicy {
            tenant_id: actor.tenant_id().clone(),
            revision: NonZeroU64::MIN,
            allowed_apps: [app.clone()].into(),
            approval_apps: [app.clone()].into(),
            auto_read_apps: [app].into(),
            approval_lifetime_seconds: 300,
        },
        &actor.owner(),
    )
    .await
    .unwrap();
    let workspace = WorkspaceId::new("workspace").unwrap();
    backend
        .register_workspace(&actor, &workspace, "Fixture")
        .await
        .unwrap();
    let application = AgentApplication::new(Arc::new(backend.sessions(actor.clone())));
    let session = application
        .create_session(CreateSession {
            request_id: RequestId::new("root").unwrap(),
            workspace_id: workspace,
            title: "Root".to_owned(),
        })
        .await
        .unwrap();
    query(
        "UPDATE zuno_enterprise_preview.session SET agent='build',model=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(session.id.as_str())
        .bind(json!({"providerID":"turn-test","modelID":"model"})).execute(admin).await.unwrap();
    let runtime = backend.runtime(actor.tenant_id().clone());
    let configuration = ConfigurationRef {
        id: ConfigurationId::new("fixture").unwrap(),
        version: 1,
        sha256: "1".repeat(64),
    };
    let job = runtime
        .submit(
            &actor,
            JobSubmission {
                selection: None,
                session_id: session.id.clone(),
                request_id: RequestId::new("input").unwrap(),
                expected_input_version: 0,
                text: "Inspect the fixture".to_owned(),
                configuration: configuration.clone(),
            },
        )
        .await
        .unwrap();
    let duration = LeaseDuration::new(300_000).unwrap();
    let first = runtime
        .claim(&WorkerInstanceId::new("worker-first").unwrap(), duration)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.job.id, job.id);
    let state = backend
        .turn_state(first.lease.clone(), "/workspace".to_owned())
        .unwrap();
    let scope = TurnStateScope {
        owner: actor.owner(),
        session_id: session.id.to_string(),
    };
    let foreign = TurnStateScope {
        owner: PrincipalKey {
            tenant_id: actor.tenant_id().clone(),
            principal_id: PrincipalId::new("bob").unwrap(),
        },
        session_id: session.id.to_string(),
    };
    assert!(state.history(&foreign).await.is_err());
    raw_sql(
        "CREATE FUNCTION public.zuno_expire_during_write() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN UPDATE zuno_enterprise_preview.runtime_session
           SET lease_expires=floor(extract(epoch FROM clock_timestamp())*1000)::bigint-1
           WHERE tenant_id=NEW.tenant_id AND principal_id=NEW.principal_id AND session_id=NEW.session_id;
           RETURN NEW; END $$;
         CREATE TRIGGER zuno_expire_during_write BEFORE INSERT ON zuno_enterprise_preview.message
           FOR EACH ROW WHEN (NEW.id='expire-during-commit') EXECUTE FUNCTION public.zuno_expire_during_write();",
    ).execute(admin).await.unwrap();
    let expired = state.commit_assistant(&scope,&zuno_db::assistant_commit::AssistantCommit {
        message:MessageRecord::from_json(json!({
            "id":"expire-during-commit","sessionID":session.id,"role":"assistant","time":{"created":1}
        })).unwrap(),
        parts:vec![],persisted_at_ms:1,context_limit:None,context_usage:None,
    }).await;
    assert!(matches!(
        expired,
        Err(zuno_engine::r#loop::TurnError::State(
            zuno_engine::state::TurnStateError::LeaseLost
        ))
    ));
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.message WHERE id='expire-during-commit'"
        )
        .fetch_one(admin)
        .await
        .unwrap(),
        0
    );
    raw_sql("DROP TRIGGER zuno_expire_during_write ON zuno_enterprise_preview.message; DROP FUNCTION public.zuno_expire_during_write()")
        .execute(admin).await.unwrap();
    state
        .consume_input(
            &scope,
            InputMaterialization {
                turn_id: None,
                input_id: Some(job.input_id.to_string()),
                message: MessageRecord::from_json(json!({
                    "id":job.input_id,"sessionID":session.id,"role":"user","time":{"created":0},
                    "agent":"build","model":{"providerID":"turn-test","modelID":"model"},
                }))
                .unwrap(),
                parts: vec![
                    PartRecord::from_json(
                        json!({
                            "id":"root-input-part","sessionID":session.id,"messageID":job.input_id,
                            "type":"text","text":"Inspect the fixture",
                        }),
                        0,
                    )
                    .unwrap(),
                ],
            },
        )
        .await
        .unwrap();
    let script = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([
            vec![
                StreamEvent::ToolUseStart {
                    id: "inspect-call".to_owned(),
                    name: "inspect".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "inspect-call".to_owned(),
                    delta: "{}".to_owned(),
                },
                StreamEvent::ToolUseEnd {
                    id: "inspect-call".to_owned(),
                },
                usage(),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
            ],
            vec![
                StreamEvent::TextDelta("Done".to_owned()),
                usage(),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ],
        ])),
        requests: AtomicUsize::new(0),
    });
    let mut providers = ProviderRegistry::new();
    let cloned = Arc::clone(&script);
    providers.register("turn-test", move |_| cloned.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = ToolRegistryDispatcher::new(
        vec![Arc::new(Inspect(Arc::clone(&calls)))],
        vec![],
        Arc::new(AllowAll),
        AuthorizationPolicy::Strict,
        McpToolStatus::Ready,
    );
    let request = || {
        AdvanceRequest::new(
            RunTurnRequest::new(
                session.id.to_string(),
                job.turn_id.to_string(),
                DynamicContext::default(),
            ),
            configuration.sha256.clone(),
            NonZeroU32::MIN,
        )
        .unwrap()
    };
    let interrupt = InterruptSignal::new();
    let (sender, mut receiver) = event_channel();
    let (outcome, _) = tokio::join!(
        advance_turn(
            request(),
            TurnContext::from_persistence(
                Arc::new(state.clone()),
                &providers,
                &Resolver,
                &dispatcher,
                &interrupt,
            )
            .with_principal_scope(actor.clone()),
            sender
        ),
        async { while receiver.recv().await.is_some() {} },
    );
    assert!(
        matches!(outcome, Ok(AdvanceOutcome::Progressed { .. })),
        "{outcome:?}"
    );
    // Discard the caller's success response. The next lease reconstructs the
    // checkpoint exclusively from durable Job state.
    drop(outcome);
    assert_eq!(
        runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
        JobPhase::Ready
    );
    // Preserve an exact schema-3 completed-step shape across an upgrade, then
    // lose a claimant before it starts advancing that checkpoint.
    query("UPDATE zuno_enterprise_preview.event SET data=jsonb_set(data,'{schemaVersion}','3'::jsonb)
           WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND type='runtime.driver.advance'")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(session.id.as_str())
        .execute(admin).await.unwrap();
    query("UPDATE zuno_enterprise_preview.runtime_job SET checkpoint=jsonb_set(checkpoint,'{schemaVersion}','3'::jsonb)
           WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(job.id.as_str())
        .execute(admin).await.unwrap();
    let abandoned = runtime
        .claim(
            &WorkerInstanceId::new("legacy-abandoned").unwrap(),
            duration,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(abandoned.job.checkpoint.as_ref().unwrap().schema_version, 3);
    query(
        "UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=0
           WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
    )
    .bind(actor.tenant_id().as_str())
    .bind(actor.principal_id().as_str())
    .bind(session.id.as_str())
    .execute(admin)
    .await
    .unwrap();
    let second = runtime
        .claim(&WorkerInstanceId::new("worker-second").unwrap(), duration)
        .await
        .unwrap()
        .unwrap();
    assert!(second.lease.epoch > first.lease.epoch);
    assert_eq!(second.job.checkpoint_version, 1);
    assert!(matches!(
        state.touch(&scope).await,
        Err(zuno_engine::r#loop::TurnError::State(
            zuno_engine::state::TurnStateError::LeaseLost
        ))
    ));
    let stored: Value = query_scalar(
        "SELECT data FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3
         AND type='runtime.driver.advance' ORDER BY sequence DESC LIMIT 1",
    ).bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(session.id.as_str())
        .fetch_one(admin).await.unwrap();
    assert_eq!(stored.pointer("/state/checkpoint/steps"), Some(&json!(1)));
    assert_eq!(
        stored.pointer("/state/checkpoint/toolCallsDispatched"),
        Some(&json!(1))
    );
    let reference: CheckpointRef =
        serde_json::from_value(second.job.checkpoint.unwrap().reference).unwrap();
    let next = backend
        .turn_state(second.lease.clone(), "/workspace".to_owned())
        .unwrap();
    let (sender, mut receiver) = event_channel();
    let (outcome, _) = tokio::join!(
        advance_turn(
            request().resume(reference),
            TurnContext::from_persistence(
                Arc::new(next.clone()),
                &providers,
                &Resolver,
                &dispatcher,
                &interrupt,
            )
            .with_principal_scope(actor.clone()),
            sender
        ),
        async { while receiver.recv().await.is_some() {} },
    );
    assert!(
        matches!(outcome, Ok(AdvanceOutcome::Completed { steps: 2, .. })),
        "{outcome:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(script.requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
        JobPhase::Completed
    );
    let totals = query(
        "SELECT tokens_input,tokens_output,tokens_reasoning,tokens_cache_read,tokens_cache_write FROM zuno_enterprise_preview.session
         WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(session.id.as_str())
        .fetch_one(admin).await.unwrap();
    assert_eq!(totals.get::<i64, _>("tokens_input"), 200);
    assert_eq!(totals.get::<i64, _>("tokens_output"), 16);
    assert_eq!(totals.get::<i64, _>("tokens_reasoning"), 4);
    assert_eq!(totals.get::<i64, _>("tokens_cache_read"), 40);
    assert_eq!(totals.get::<i64, _>("tokens_cache_write"), 10);
    let receipt = query(
        "SELECT state,turn_id,applied_at,completed_at FROM zuno_enterprise_preview.input_execution_receipt
         WHERE tenant_id=$1 AND principal_id=$2 AND input_id=$3",
    ).bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(job.input_id.as_str())
        .fetch_one(admin).await.unwrap();
    assert_eq!(receipt.get::<String, _>("state"), "completed");
    assert_eq!(receipt.get::<String, _>("turn_id"), job.turn_id.as_str());
    assert!(receipt.get::<Option<i64>, _>("applied_at").is_some());
    assert!(receipt.get::<Option<i64>, _>("completed_at").is_some());
    assert!(
        next.history(&scope).await.is_err(),
        "a completed attempt no longer has Worker read authority"
    );

    // A failed runtime checkpoint must roll back the driver journal and the
    // capacity release together, after the real provider/tool step has run.
    let next_job = runtime
        .submit(
            &actor,
            JobSubmission {
                selection: None,
                session_id: session.id.clone(),
                request_id: RequestId::new("rollback-input").unwrap(),
                expected_input_version: 1,
                text: "Inspect the fixture".to_owned(),
                configuration: configuration.clone(),
            },
        )
        .await
        .unwrap();
    let third = runtime
        .claim(&WorkerInstanceId::new("worker-third").unwrap(), duration)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(third.job.id, next_job.id);
    let failed_state = backend
        .turn_state(third.lease.clone(), "/workspace".to_owned())
        .unwrap();
    failed_state.consume_input(&scope,InputMaterialization {
                turn_id: None,
        input_id:Some(next_job.input_id.to_string()),
        message:MessageRecord::from_json(json!({
            "id":next_job.input_id,"sessionID":session.id,"role":"user","time":{"created":0},
            "agent":"build","model":{"providerID":"turn-test","modelID":"model"},
        })).unwrap(),
        parts:vec![PartRecord::from_json(json!({
            "id":"rollback-input-part","sessionID":session.id,"messageID":next_job.input_id,
            "type":"text","text":"Inspect the fixture",
        }),0).unwrap()],
    }).await.unwrap();
    let seed = failed_state.context_usage(&scope).await.unwrap();
    let unchanged = zuno_types::context_usage::ContextUsageWrite {
        expected_revision: seed.persisted_revision,
        tracker: seed.tracker.clone(),
    };
    failed_state
        .commit_context_usage(&scope, &unchanged)
        .await
        .unwrap();
    let mut stale = unchanged.clone();
    stale.expected_revision = Some(u64::MAX);
    stale.tracker.observe_history_epoch(
        i64::try_from(seed.tracker.snapshot().context_epoch + 1).unwrap(),
        zuno_db::message::now_millis(),
    );
    assert!(
        failed_state
            .commit_context_usage(&scope, &stale)
            .await
            .is_err()
    );
    assert_eq!(
        failed_state.context_usage(&scope).await.unwrap().tracker,
        seed.tracker
    );

    raw_sql(
        "CREATE FUNCTION public.zuno_refuse_context_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected context commit failure'; END $$;
         CREATE TRIGGER zuno_refuse_context_commit BEFORE INSERT ON zuno_enterprise_preview.event
         FOR EACH ROW WHEN(NEW.type='session.context.usage') EXECUTE FUNCTION public.zuno_refuse_context_commit();",
    ).execute(admin).await.unwrap();
    let before_context: Value = query_scalar(
        "SELECT jsonb_build_object('tokens',tokens_input,'estimate',tokens_estimated_pending_prompt,'sequence',event_sequence)
         FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(session.id.as_str())
        .fetch_one(admin).await.unwrap();
    let preparation = zuno_engine::context_usage::ContextRequestPreparation {
        before: seed.clone(),
        identity: zuno_types::context_usage::ContextRequestIdentity {
            request_id: "atomic-request".to_owned(),
            request_sequence: 0,
            attempt: 1,
            context_epoch: seed.tracker.snapshot().context_epoch,
            provider_id: "turn-test".to_owned(),
            model_id: "model".to_owned(),
            source: zuno_types::context_usage::ContextUsageSource::Main,
            turn_id: Some(next_job.turn_id.to_string()),
            time_started: zuno_db::message::now_millis(),
            request_context_tokens: Some(10),
            history_prefix: None,
        },
        prompt_tokens: 10,
        tail_tokens: Some(0),
        context_limit: Some(1000),
    };
    let start = zuno_engine::state::ProviderRequestCommit {
        assistant: MessageRecord::from_json(json!({
            "id":"atomic-assistant","sessionID":session.id,"role":"assistant",
            "requestID":"atomic-request","time":{"created":9999999999999_i64},
        })).unwrap(),
        event: zuno_db::event_log::NewSessionEvent::new("session.provider.request", json!({
            "turnID":next_job.turn_id,"requestID":"atomic-request","assistantMessageID":"atomic-assistant","status":"started",
        }).as_object().unwrap().clone()).unwrap(),
        estimated_prompt_tokens: 10, context_limit: Some(1000), context: preparation,
    };
    assert!(
        failed_state
            .start_provider_request(&scope, start)
            .await
            .is_err()
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.message WHERE id='atomic-assistant'"
        )
        .fetch_one(admin)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        failed_state.context_usage(&scope).await.unwrap().tracker,
        seed.tracker
    );
    let after_context: Value = query_scalar(
        "SELECT jsonb_build_object('tokens',tokens_input,'estimate',tokens_estimated_pending_prompt,'sequence',event_sequence)
         FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(session.id.as_str())
        .fetch_one(admin).await.unwrap();
    assert_eq!(
        after_context, before_context,
        "provider start and context commit roll back together"
    );
    raw_sql("DROP TRIGGER zuno_refuse_context_commit ON zuno_enterprise_preview.event; DROP FUNCTION public.zuno_refuse_context_commit()")
        .execute(admin).await.unwrap();
    script.replies.lock().unwrap().push_back(vec![
        StreamEvent::ToolUseStart {
            id: "rollback-call".to_owned(),
            name: "inspect".to_owned(),
        },
        StreamEvent::ToolInputDelta {
            id: "rollback-call".to_owned(),
            delta: "{}".to_owned(),
        },
        StreamEvent::ToolUseEnd {
            id: "rollback-call".to_owned(),
        },
        usage(),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
    ]);
    raw_sql(
        "CREATE FUNCTION public.zuno_refuse_checkpoint() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected checkpoint failure'; END $$;
         CREATE TRIGGER zuno_refuse_checkpoint BEFORE INSERT ON zuno_enterprise_preview.event
           FOR EACH ROW WHEN (NEW.type='runtime.checkpoint.committed') EXECUTE FUNCTION public.zuno_refuse_checkpoint();",
    ).execute(admin).await.unwrap();
    let failed_request = AdvanceRequest::new(
        RunTurnRequest::new(
            session.id.to_string(),
            next_job.turn_id.to_string(),
            DynamicContext::default(),
        ),
        configuration.sha256.clone(),
        NonZeroU32::MIN,
    )
    .unwrap();
    let (sender, mut receiver) = event_channel();
    let (failed, _) = tokio::join!(
        advance_turn(
            failed_request.clone(),
            TurnContext::from_persistence(
                Arc::new(failed_state.clone()),
                &providers,
                &Resolver,
                &dispatcher,
                &interrupt,
            )
            .with_principal_scope(actor.clone()),
            sender
        ),
        async { while receiver.recv().await.is_some() {} },
    );
    assert!(failed.is_err());
    raw_sql("DROP TRIGGER zuno_refuse_checkpoint ON zuno_enterprise_preview.event; DROP FUNCTION public.zuno_refuse_checkpoint()")
        .execute(admin).await.unwrap();
    let failed_job = runtime.get(&actor.owner(), &next_job.id).await.unwrap();
    assert_eq!(failed_job.phase, JobPhase::Running);
    assert_eq!(failed_job.checkpoint_version, 0);
    assert!(failed_job.checkpoint.is_none());
    let journal:Value=query_scalar(
        "SELECT data FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3
         AND type='runtime.driver.advance' ORDER BY sequence DESC LIMIT 1",
    ).bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(session.id.as_str())
        .fetch_one(admin).await.unwrap();
    assert_eq!(journal.pointer("/state/phase"), Some(&json!("started")));
    assert!(matches!(
        failed_state.begin_advance(&scope, &failed_request).await,
        Err(zuno_engine::advance::AdvanceError::NeedsInspection)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(script.requests.load(Ordering::SeqCst), 3);
    query("UPDATE zuno_enterprise_preview.organization_member SET active=false WHERE tenant_id=$1 AND principal_id=$2")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).execute(admin).await.unwrap();
    assert!(matches!(
        failed_state.history(&scope).await,
        Err(zuno_engine::r#loop::TurnError::State(
            zuno_engine::state::TurnStateError::Forbidden
        ))
    ));
}
