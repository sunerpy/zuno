use super::*;
use zuno_application::runtime::{ClaimedJob, JobFinish, RuntimeJob};
use zuno_engine::r#loop::{AvailableTools, DispatchRequest, PreparedToolDispatch, ToolDispatcher};
use zuno_engine::wait::{WaitCompletion, WaitCompletionStore};
use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};

struct Deferred {
    inner: ToolRegistryDispatcher,
    job: RuntimeJob,
    target: WaitTarget,
    references: Mutex<Vec<WaitRef>>,
    early: Option<crate::PostgresRuntimeStore>,
}

#[async_trait]
impl ToolDispatcher for Deferred {
    fn available_tools(&self) -> AvailableTools {
        self.inner.available_tools()
    }
    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        if request.call.id != "wait" {
            return self.inner.prepare(request).await;
        }
        let reference = WaitRef {
            id: WaitId::new(format!("wait_{}", self.job.turn_id)).unwrap(),
            turn_id: self.job.turn_id.clone(),
            invocation_id: InvocationId::new(&request.call.id).unwrap(),
            arguments_sha256: zuno_orchestration::sha256_json(&request.call.input),
            target: self.target.clone(),
            continuation: WaitContinuation::CurrentTurn,
        };
        if let Some(runtime) = &self.early {
            runtime
                .publish_completion(
                    &self.job.principal.owner(),
                    &self.job.id,
                    &completed(reference.clone()),
                )
                .await
                .unwrap();
        }
        self.references.lock().unwrap().push(reference.clone());
        PreparedToolDispatch::Pending(reference)
    }
}

fn completed(reference: WaitRef) -> WaitCompletion {
    WaitCompletion::tool_result(
        CompletionId::new(format!("completion_{}", reference.turn_id)).unwrap(),
        reference,
        zuno_engine::r#loop::ToolDispatchResult::success(ToolOutput::text(
            "Remote",
            "Verified remote result",
        )),
    )
}

pub(super) async fn advance(
    backend: &PostgresBackend,
    claimed: &ClaimedJob,
    providers: &ProviderRegistry,
    dispatcher: &dyn ToolDispatcher,
) -> Result<AdvanceOutcome, zuno_engine::advance::AdvanceError> {
    let mut request = AdvanceRequest::new(
        RunTurnRequest::new(
            claimed.job.session_id.to_string(),
            claimed.job.turn_id.to_string(),
            DynamicContext::default(),
        ),
        claimed.job.configuration.sha256.clone(),
        NonZeroU32::MIN,
    )
    .unwrap();
    if let Some(checkpoint) = &claimed.job.checkpoint {
        request = request.resume(serde_json::from_value(checkpoint.reference.clone()).unwrap());
    }
    let state = backend
        .turn_state(claimed.lease.clone(), "/workspace".to_owned())
        .unwrap();
    let interrupt = InterruptSignal::new();
    let (sender, mut receiver) = event_channel();
    let (outcome, _) = tokio::join!(
        advance_turn(
            request,
            TurnContext::from_persistence(
                Arc::new(state),
                providers,
                &Resolver,
                dispatcher,
                &interrupt,
            )
            .with_principal_scope(claimed.job.principal.clone()),
            sender
        ),
        async { while receiver.recv().await.is_some() {} },
    );
    outcome
}

pub(super) async fn seed_job(
    backend: &PostgresBackend,
    admin: &PgPool,
    actor: &PrincipalScope,
    name: &str,
) -> RuntimeJob {
    let workspace = WorkspaceId::new("workspace").unwrap();
    backend
        .register_workspace(actor, &workspace, "Fixture")
        .await
        .unwrap();
    let application = AgentApplication::new(Arc::new(backend.sessions(actor.clone())));
    let session = application
        .create_session(CreateSession {
            request_id: RequestId::new(format!("session-{name}")).unwrap(),
            workspace_id: workspace,
            title: name.to_owned(),
        })
        .await
        .unwrap();
    query("UPDATE zuno_enterprise_preview.session SET agent='build',model=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(session.id.as_str())
        .bind(json!({"providerID":"turn-test","modelID":"model"})).execute(admin).await.unwrap();
    backend
        .runtime(actor.tenant_id().clone())
        .submit(
            actor,
            JobSubmission {
                session_id: session.id,
                request_id: RequestId::new(format!("input-{name}")).unwrap(),
                expected_input_version: 0,
                text: name.to_owned(),
                configuration: ConfigurationRef {
                    id: ConfigurationId::new("fixture").unwrap(),
                    version: 1,
                    sha256: "2".repeat(64),
                },
            },
        )
        .await
        .unwrap()
}

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    for scenario in ["normal", "early", "timer", "paused"] {
        let actor = PrincipalScope::new(
            TenantId::new(format!("wait-{scenario}")).unwrap(),
            PrincipalId::new("alice").unwrap(),
            PrincipalKind::User,
            Some(ClientId::new("web").unwrap()),
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
        let runtime = backend.runtime(actor.tenant_id().clone());
        let job = seed_job(backend, admin, &actor, "root").await;
        let duration = LeaseDuration::new(300_000).unwrap();
        let first = runtime
            .claim(&WorkerInstanceId::new("first").unwrap(), duration)
            .await
            .unwrap()
            .unwrap();
        let state = backend
            .turn_state(first.lease.clone(), "/workspace".to_owned())
            .unwrap();
        let scope = TurnStateScope {
            owner: actor.owner(),
            session_id: job.session_id.to_string(),
        };
        state.consume_input(&scope, InputMaterialization {
                turn_id: None,
            input_id: Some(job.input_id.to_string()),
            message: MessageRecord::from_json(json!({
                "id":job.input_id,"sessionID":job.session_id,"role":"user","time":{"created":0},
                "agent":"build","model":{"providerID":"turn-test","modelID":"model"},
            })).unwrap(),
            parts: vec![PartRecord::from_json(json!({
                "id":format!("part_{}",job.input_id),"sessionID":job.session_id,"messageID":job.input_id,
                "type":"text","text":"root",
            }), 0).unwrap()],
        }).await.unwrap();
        let mut tools = Vec::new();
        for id in ["before", "wait", "after"] {
            tools.extend([
                StreamEvent::ToolUseStart {
                    id: id.to_owned(),
                    name: "inspect".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: id.to_owned(),
                    delta: "{}".to_owned(),
                },
                StreamEvent::ToolUseEnd { id: id.to_owned() },
            ]);
        }
        tools.extend([
            usage(),
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]);
        let script = Arc::new(Script {
            replies: Mutex::new(VecDeque::from([
                tools,
                vec![
                    StreamEvent::TextDelta("Complete".to_owned()),
                    usage(),
                    StreamEvent::MessageEnd {
                        stop_reason: Some(FinishReason::Stop),
                    },
                ],
            ])),
            requests: AtomicUsize::new(0),
        });
        let mut providers = ProviderRegistry::new();
        let shared = script.clone();
        providers.register("turn-test", move |_| shared.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let dispatcher = Deferred {
            inner: ToolRegistryDispatcher::new(
                vec![Arc::new(Inspect(calls.clone()))],
                vec![],
                Arc::new(AllowAll),
                AuthorizationPolicy::Strict,
                McpToolStatus::Ready,
            ),
            job: job.clone(),
            target: if scenario == "timer" {
                WaitTarget::Timer { deadline_ms: 1 }
            } else {
                WaitTarget::Operation {
                    operation_id: OperationId::new("external-once").unwrap(),
                }
            },
            references: Mutex::new(Vec::new()),
            early: (scenario == "early").then(|| runtime.clone()),
        };
        let outcome = advance(backend, &first, &providers, &dispatcher)
            .await
            .unwrap();
        assert!(matches!(outcome, AdvanceOutcome::Waiting { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let reference = dispatcher.references.lock().unwrap()[0].clone();
        let suspended = runtime.get(&actor.owner(), &job.id).await.unwrap();
        assert_eq!(
            suspended.phase,
            if scenario == "early" {
                JobPhase::Ready
            } else {
                JobPhase::Waiting
            }
        );
        assert!(
            state.touch(&scope).await.is_err(),
            "suspension releases the original lease"
        );

        if scenario == "normal" {
            let other = seed_job(backend, admin, &actor, "independent").await;
            let independent = runtime
                .claim(&WorkerInstanceId::new("independent").unwrap(), duration)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                independent.job.id, other.id,
                "a waiting parent occupies no execution slot"
            );
            runtime
                .finish(
                    &independent.lease,
                    JobFinish::Cancelled {
                        reason: "test completed".to_owned(),
                    },
                )
                .await
                .unwrap();
            assert!(
                runtime
                    .claim(&WorkerInstanceId::new("idle").unwrap(), duration)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        if scenario == "paused" {
            query("UPDATE zuno_enterprise_preview.runtime_job SET phase='paused' WHERE tenant_id=$1 AND job_id=$2")
                .bind(actor.tenant_id().as_str()).bind(job.id.as_str()).execute(admin).await.unwrap();
        }
        if scenario != "timer" {
            let fact = completed(reference.clone());
            let event = runtime.publish(&scope, &fact).await.unwrap();
            assert_eq!(
                runtime
                    .publish_completion(&actor.owner(), &job.id, &fact)
                    .await
                    .unwrap(),
                event
            );
            let mut changed = fact.clone();
            let zuno_engine::wait::WaitOutcome::ToolResult { result } = &mut changed.outcome else {
                panic!("tool result")
            };
            result.output.output = "wrong replacement".to_owned();
            assert!(
                runtime
                    .publish_completion(&actor.owner(), &job.id, &changed)
                    .await
                    .is_err()
            );
            let mut foreign = actor.owner();
            foreign.principal_id = PrincipalId::new("bob").unwrap();
            assert!(
                runtime
                    .publish_completion(&foreign, &job.id, &fact)
                    .await
                    .is_err()
            );
        }
        if scenario == "paused" {
            assert_eq!(
                runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
                JobPhase::Paused
            );
            assert!(
                runtime
                    .claim(&WorkerInstanceId::new("paused").unwrap(), duration)
                    .await
                    .unwrap()
                    .is_none()
            );
            continue;
        }
        let left_worker = WorkerInstanceId::new("left").unwrap();
        let right_worker = WorkerInstanceId::new("right").unwrap();
        let (left, right) = tokio::join!(
            runtime.claim(&left_worker, duration),
            runtime.claim(&right_worker, duration)
        );
        let mut claims = [left.unwrap(), right.unwrap()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(
            claims.len(),
            1,
            "{scenario}: one Worker claims the ready Job"
        );
        let mut claimant = claims.pop().unwrap();
        assert!(claimant.lease.epoch > first.lease.epoch);
        if scenario == "normal" {
            raw_sql(
                "CREATE FUNCTION public.zuno_reject_wait_commit() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN RAISE EXCEPTION 'injected wait checkpoint failure'; END $$;
                 CREATE TRIGGER zuno_reject_wait_commit BEFORE INSERT ON zuno_enterprise_preview.event
                 FOR EACH ROW WHEN(NEW.type='runtime.driver.advance' AND NEW.data#>>'{state,phase}'='checkpointed')
                 EXECUTE FUNCTION public.zuno_reject_wait_commit();",
            ).execute(admin).await.unwrap();
            assert!(
                advance(backend, &claimant, &providers, &dispatcher)
                    .await
                    .is_err()
            );
            let consumed: i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.runtime_wait WHERE tenant_id=$1 AND state='consumed'")
                .bind(actor.tenant_id().as_str()).fetch_one(admin).await.unwrap();
            assert_eq!(consumed, 0);
            let result: Value = query_scalar("SELECT data FROM zuno_enterprise_preview.part WHERE tenant_id=$1 AND session_id=$2 AND data->>'callID'='wait'")
                .bind(actor.tenant_id().as_str()).bind(job.session_id.as_str()).fetch_one(admin).await.unwrap();
            assert_eq!(result["state"]["status"], "pending");
            raw_sql("DROP TRIGGER zuno_reject_wait_commit ON zuno_enterprise_preview.event; DROP FUNCTION public.zuno_reject_wait_commit()")
                .execute(admin).await.unwrap();
            // Lose this Worker before a consumption commit. The unchanged exact
            // wait checkpoint remains a safe recovery boundary.
            query("UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=1 WHERE tenant_id=$1 AND session_id=$2")
                .bind(actor.tenant_id().as_str()).bind(job.session_id.as_str()).execute(admin).await.unwrap();
            claimant = runtime
                .claim(&WorkerInstanceId::new("takeover").unwrap(), duration)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(matches!(
            advance(backend, &claimant, &providers, &dispatcher)
                .await
                .unwrap(),
            AdvanceOutcome::Progressed { .. }
        ));
        assert_eq!(
            script.requests.load(Ordering::SeqCst),
            1,
            "consumption makes no provider request"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Discard the response and reconstruct the authoritative next checkpoint.
        let next = runtime
            .claim(&WorkerInstanceId::new("resume").unwrap(), duration)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.job.checkpoint_version, 2);
        assert!(matches!(
            advance(backend, &next, &providers, &dispatcher)
                .await
                .unwrap(),
            AdvanceOutcome::Completed { steps: 2, .. }
        ));
        assert_eq!(script.requests.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(dispatcher.references.lock().unwrap().len(), 1);
        assert_eq!(
            runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
            JobPhase::Completed
        );
        let consumed: i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.runtime_wait WHERE tenant_id=$1 AND state='consumed'")
            .bind(actor.tenant_id().as_str()).fetch_one(admin).await.unwrap();
        assert_eq!(consumed, 1);
        let usage: i64 = query_scalar(
            "SELECT tokens_input FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND id=$2",
        )
        .bind(actor.tenant_id().as_str())
        .bind(job.session_id.as_str())
        .fetch_one(admin)
        .await
        .unwrap();
        assert_eq!(usage, 200);
    }
}
