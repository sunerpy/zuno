use super::*;
use futures::StreamExt as _;
use std::sync::atomic::AtomicBool;
use zuno_engine::{
    budget::NoopBudgetPolicy,
    driver::{AgentDriver, DefaultAgentDriver},
};
use zuno_worker::runtime::{
    WorkerError, WorkerObserver, WorkerRuntime, WorkerServiceFactory, WorkerSettings,
    WorkerTurnServices,
};

#[derive(Debug)]
struct SlowProvider {
    script: Arc<Script>,
}
impl Provider for SlowProvider {
    fn id(&self) -> &str {
        "wire-test"
    }
    fn capabilities(&self) -> Capabilities {
        self.script.capabilities()
    }
    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        let mut response = Some(self.script.stream(request));
        Box::pin(
            stream::once(async {
                tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
            })
            .flat_map(move |_| response.take().unwrap()),
        )
    }
}

struct Factory {
    configuration: ConfigurationRef,
    providers: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistryDispatcher>,
    input_times: Mutex<Vec<i64>>,
}

// A mounted driver may retain its event capability until profile disposal.
// That lifetime must not extend a completed bounded advance.
struct RetainedEventsDriver {
    sender: Mutex<Option<zuno_engine::r#loop::TurnEventSender>>,
}
impl AgentDriver for RetainedEventsDriver {
    fn name(&self) -> &str {
        "default-with-retained-events"
    }
    fn supports_advance(&self) -> bool {
        true
    }
    fn advance<'a>(
        &'a self,
        request: AdvanceRequest,
        context: TurnContext<'a>,
        events: zuno_engine::r#loop::TurnEventSender,
    ) -> futures::future::BoxFuture<'a, Result<AdvanceOutcome, zuno_engine::advance::AdvanceError>>
    {
        *self.sender.lock().unwrap() = Some(events.clone());
        DefaultAgentDriver.advance(request, context, events)
    }
    fn drive<'a>(
        &'a self,
        request: RunTurnRequest,
        context: TurnContext<'a>,
        events: zuno_engine::r#loop::TurnEventSender,
    ) -> futures::future::BoxFuture<
        'a,
        Result<zuno_engine::r#loop::TurnOutcome, zuno_engine::r#loop::TurnError>,
    > {
        DefaultAgentDriver.drive(request, context, events)
    }
}
#[async_trait]
impl WorkerServiceFactory for Factory {
    fn configurations(&self) -> Vec<ConfigurationRef> {
        vec![self.configuration.clone()]
    }
    async fn resolve(
        &self,
        execution: &zuno_worker::WorkerExecution,
    ) -> Result<WorkerTurnServices, WorkerError> {
        self.input_times
            .lock()
            .unwrap()
            .push(execution.input.created_at_ms);
        // Secret/configuration loading can outlive the initial lease too.
        tokio::time::sleep(std::time::Duration::from_millis(1400)).await;
        Ok(WorkerTurnServices {
            configuration: self.configuration.clone(),
            providers: self.providers.clone(),
            resolver: Arc::new(Resolver),
            dispatcher: self.tools.clone(),
            driver: Arc::new(RetainedEventsDriver {
                sender: Mutex::new(None),
            }),
            budget: Arc::new(NoopBudgetPolicy),
            dynamic_context: DynamicContext::default(),
            executor_directory: "/workspace".to_owned(),
            steps_per_advance: NonZeroU32::MIN,
        })
    }
}

struct Observer {
    shutdown: InterruptSignal,
    completed: AtomicUsize,
    failures: Mutex<Vec<String>>,
}
impl WorkerObserver for Observer {
    fn event(&self, _job: &JobId, _event: zuno_engine::r#loop::TurnEvent) {}
    fn settled(&self, _job: &JobId, result: &Result<AdvanceOutcome, WorkerError>) {
        match result {
            Ok(AdvanceOutcome::Completed { .. }) => {
                self.completed.fetch_add(1, Ordering::SeqCst);
                self.shutdown.fire();
            }
            Err(error) => {
                self.failures.lock().unwrap().push(format!("{error:?}"));
                self.shutdown.fire();
            }
            _ => {}
        }
    }
}

#[tokio::test]
#[ignore = "run scripts/check_enterprise_postgres.py for real TLS and PostgreSQL"]
async fn worker_runtimes_renew_and_resume_shared_kernel_without_replaying_input_or_tools() {
    let fixture: Fixture = serde_json::from_slice(
        &std::fs::read(std::env::var("ZUNO_POSTGRES_TEST_CONFIG").unwrap()).unwrap(),
    )
    .unwrap();
    let cluster = fixture
        .options(&fixture.admin_url, "postgres")
        .connect()
        .await
        .unwrap();
    raw_sql("CREATE DATABASE zuno_runtime_fixture OWNER zuno_preview_migrator")
        .execute(&cluster)
        .await
        .unwrap();
    let migrator = fixture
        .options(&fixture.migration_url, "zuno_runtime_fixture")
        .connect()
        .await
        .unwrap();
    let admin = fixture
        .options(&fixture.admin_url, "zuno_runtime_fixture")
        .connect()
        .await
        .unwrap();
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    let backend =
        PostgresBackend::connect(fixture.options(&fixture.runtime_url, "zuno_runtime_fixture"))
            .await
            .unwrap();
    let tenant = TenantId::new("worker-runtime").unwrap();
    let actor = PrincipalScope::new(
        tenant.clone(),
        PrincipalId::new("alice").unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("web").unwrap()),
        NonZeroU64::MIN,
    );
    let app = actor.client_id().unwrap().clone();
    bootstrap_organization(
        &migrator,
        &OrganizationPolicy {
            tenant_id: tenant.clone(),
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
        .register_workspace(&actor, &workspace, "Runtime")
        .await
        .unwrap();
    let session = AgentApplication::new(Arc::new(backend.sessions(actor.clone())))
        .create_session(CreateSession {
            request_id: RequestId::new("session").unwrap(),
            workspace_id: workspace,
            title: "Runtime".to_owned(),
        })
        .await
        .unwrap();
    query("UPDATE zuno_enterprise_preview.session SET agent='build',model=$1 WHERE tenant_id=$2 AND id=$3")
        .bind(json!({"providerID":"wire-test","modelID":"model"})).bind(tenant.as_str()).bind(session.id.as_str())
        .execute(&admin).await.unwrap();
    let configuration = ConfigurationRef {
        id: ConfigurationId::new("runtime").unwrap(),
        version: 1,
        sha256: "8".repeat(64),
    };
    let runtime = backend.runtime(tenant.clone());
    let job = runtime
        .submit(
            &actor,
            JobSubmission {
                selection: None,
                session_id: session.id,
                request_id: RequestId::new("job").unwrap(),
                expected_input_version: 0,
                text: "Run the fixture".to_owned(),
                configuration: configuration.clone(),
            },
        )
        .await
        .unwrap();
    let policy:OAuth2ClaimsPolicy=serde_json::from_value(json!({
        "tenantId":tenant,"audience":"state-api","allowedClients":["worker-client"],
        "requiredScopes":["worker"],"principalKind":"workload","actorClaim":{"claim":"actor","value":"workload"},
    })).unwrap();
    let verifier = Arc::new(OAuth2IntrospectionVerifier::with_introspector(
        OAuth2IntrospectionConfig::new(
            "https://issuer.example",
            "https://issuer.example/introspection",
            policy,
        )
        .unwrap(),
        Arc::new(Introspection),
    ));
    let identity = verifier.verify("worker-token").await.unwrap();
    let workers = Arc::new(
        WorkerAuthority::new(
            verifier,
            [WorkerSubject {
                tenant_id: identity.tenant_id().clone(),
                principal_id: identity.principal_id().clone(),
                client_id: identity.client_id().clone(),
            }]
            .into(),
        )
        .unwrap(),
    );
    let grants = Arc::new(
        JobGrantAuthority::new(
            "current".to_owned(),
            vec![("current".to_owned(), vec![6; 32])],
        )
        .unwrap(),
    );
    let service = WorkerStateService::new(
        backend.clone(),
        workers,
        grants,
        tenant.clone(),
        LeaseDuration::new(1000).unwrap(),
    );
    let reject_renewal = Arc::new(AtomicBool::new(false));
    let rejected_renewals = Arc::new(AtomicUsize::new(0));
    let failure_switch = reject_renewal.clone();
    let failure_count = rejected_renewals.clone();
    let blocked_claim = Arc::new(tokio::sync::Notify::new());
    let blocked_notice = blocked_claim.clone();
    let router = service
        .router()
        .route(
            "/blocked/internal/worker/v1/claim",
            post(move || {
                let notice = blocked_notice.clone();
                async move {
                    notice.notify_one();
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        )
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let enabled = failure_switch.clone();
                let count = failure_count.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let bytes = axum::body::to_bytes(
                        body,
                        zuno_engine::state::wire::MAX_WORKER_FRAME_BYTES,
                    )
                    .await
                    .unwrap();
                    let delayed_boundary = parts.uri.path()
                        == format!("/{}", zuno_worker::STATE_PATH)
                        && zuno_engine::state::wire::StateRequest::decode(&bytes).is_ok_and(
                            |request| {
                                matches!(
                                    request.command,
                                    zuno_engine::state::wire::StateCommand::CommitAdvance { .. }
                                )
                            },
                        );
                    let request =
                        axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
                    if request.uri().path() == format!("/{}", zuno_worker::RENEW_PATH)
                        && enabled.load(Ordering::SeqCst)
                    {
                        count.fetch_add(1, Ordering::SeqCst);
                        axum::response::IntoResponse::into_response(StatusCode::SERVICE_UNAVAILABLE)
                    } else {
                        let response = next.run(request).await;
                        if delayed_boundary && response.status().is_success() {
                            // The database has released this lease, but its
                            // checkpoint acknowledgement is still in transit.
                            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        }
                        response
                    }
                }
            },
        ));
    let (endpoint, server) = tls_server(router, &fixture).await;
    let certificate =
        reqwest::Certificate::from_pem(&std::fs::read(&fixture.root_certificate).unwrap()).unwrap();
    let http = reqwest::Client::builder()
        .add_root_certificate(certificate.clone())
        .build()
        .unwrap();
    let rejected = http
        .post(endpoint.join(zuno_worker::CLAIM_PATH).unwrap())
        .bearer_auth("worker-token")
        .json(&json!({"version":3,"worker":"old","configurations":[configuration.clone()]}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
        JobPhase::Ready
    );
    let incompatible = http
        .post(endpoint.join(zuno_worker::CLAIM_PATH).unwrap())
        .bearer_auth("worker-token")
        .json(&json!({
            "version":zuno_engine::state::wire::WORKER_PROTOCOL_VERSION,
            "worker":"different-deployment",
            "configurations":[{"id":"runtime","version":2,"sha256":"9".repeat(64)}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(incompatible.status(), StatusCode::OK);
    assert_eq!(
        incompatible.json::<serde_json::Value>().await.unwrap(),
        json!(null)
    );
    assert_eq!(
        runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
        JobPhase::Ready
    );

    let script = Arc::new(Script {
        responses: Mutex::new(VecDeque::from([
            vec![
                StreamEvent::ToolUseStart {
                    id: "once".to_owned(),
                    name: "inspect".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "once".to_owned(),
                    delta: "{}".to_owned(),
                },
                StreamEvent::ToolUseEnd {
                    id: "once".to_owned(),
                },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
            ],
            vec![
                StreamEvent::TextDelta("Done".to_owned()),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ],
        ])),
        calls: AtomicUsize::new(0),
    });
    let provider = Arc::new(SlowProvider {
        script: script.clone(),
    });
    let mut providers = ProviderRegistry::new();
    providers.register("wire-test", move |_| provider.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(Factory {
        configuration: configuration.clone(),
        providers: Arc::new(providers),
        tools: Arc::new(ToolRegistryDispatcher::new(
            vec![Arc::new(Inspect(calls.clone()))],
            vec![],
            Arc::new(AllowAll),
            AuthorizationPolicy::Strict,
            McpToolStatus::Ready,
        )),
        input_times: Mutex::new(Vec::new()),
    });
    let shutdown = InterruptSignal::new();
    let observer = Arc::new(Observer {
        shutdown: shutdown.clone(),
        completed: AtomicUsize::new(0),
        failures: Mutex::new(Vec::new()),
    });
    let settings = WorkerSettings {
        slots: 1,
        poll_interval: std::time::Duration::from_millis(100),
        // The requested interval exceeds the lease. The runtime must bound it
        // with the actual grant lifetime instead of letting authority expire.
        renew_interval: std::time::Duration::from_secs(5),
        drain_timeout: std::time::Duration::from_secs(5),
    };
    let blocked_client = WorkerClient::new(
        endpoint.join("blocked/").unwrap(),
        Arc::new(Token("worker-token")),
        Some(certificate.clone()),
    )
    .unwrap();
    let client =
        WorkerClient::new(endpoint, Arc::new(Token("worker-token")), Some(certificate)).unwrap();
    let first = WorkerRuntime::new(
        client.clone(),
        WorkerInstanceId::new("first").unwrap(),
        factory.clone(),
        observer.clone(),
        settings,
    )
    .unwrap();
    let second = WorkerRuntime::new(
        client.clone(),
        WorkerInstanceId::new("second").unwrap(),
        factory.clone(),
        observer.clone(),
        settings,
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let (a, b) = tokio::join!(first.run(shutdown.clone()), second.run(shutdown));
        a.unwrap();
        b.unwrap();
    })
    .await
    .unwrap();
    let persisted_phase = runtime.get(&actor.owner(), &job.id).await.unwrap().phase;
    assert!(
        observer.failures.lock().unwrap().is_empty(),
        "{:?}; persisted phase: {:?}",
        observer.failures.lock().unwrap(),
        persisted_phase
    );
    assert_eq!(observer.completed.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
        JobPhase::Completed
    );
    let recorded: i64 = sqlx_core::query_scalar::query_scalar(
        "SELECT time_created FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND id=$2",
    )
    .bind(tenant.as_str())
    .bind(job.input_id.as_str())
    .fetch_one(&admin)
    .await
    .unwrap();
    {
        let times = factory.input_times.lock().unwrap();
        assert!(times.len() >= 2);
        assert!(times.iter().all(|time| *time == recorded));
    }
    let messages: i64 = sqlx_core::query_scalar::query_scalar(
        "SELECT count(*) FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND id=$2",
    )
    .bind(tenant.as_str())
    .bind(job.input_id.as_str())
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(messages, 1);

    let next = runtime
        .submit(
            &actor,
            JobSubmission {
                selection: None,
                session_id: job.session_id,
                request_id: RequestId::new("lost-renewal").unwrap(),
                expected_input_version: 1,
                text: "Do not run after losing authority".to_owned(),
                configuration: configuration.clone(),
            },
        )
        .await
        .unwrap();
    let execution = client
        .claim(
            WorkerInstanceId::new("lost-worker").unwrap(),
            std::slice::from_ref(&configuration),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.job.id, next.id);
    assert!(matches!(
        client
            .finish(
                &execution,
                zuno_application::runtime::JobFinish::Completed { result: json!({}) }
            )
            .await,
        Err(zuno_engine::state::TurnStateError::InvalidData)
    ));
    reject_renewal.store(true, Ordering::SeqCst);
    let lost = zuno_worker::runtime::advance_claimed(
        &client,
        &execution,
        factory.as_ref(),
        observer.as_ref(),
        std::time::Duration::from_millis(100),
    )
    .await;
    assert!(matches!(lost, Err(WorkerError::LeaseLost)));
    assert_eq!(rejected_renewals.load(Ordering::SeqCst), 1);
    assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.get(&actor.owner(), &next.id).await.unwrap().phase,
        JobPhase::Running,
        "loss of authority must not manufacture a terminal outcome"
    );
    let draining = InterruptSignal::new();
    let stopped = WorkerRuntime::new(
        blocked_client,
        WorkerInstanceId::new("draining").unwrap(),
        factory,
        observer,
        settings,
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let (result, ()) = tokio::join!(stopped.run(draining.clone()), async {
            blocked_claim.notified().await;
            draining.fire();
        });
        result.unwrap();
    })
    .await
    .expect("shutdown must interrupt an outstanding claim without waiting for HTTP timeout");
    server.abort();
    let _ = server.await;
}
