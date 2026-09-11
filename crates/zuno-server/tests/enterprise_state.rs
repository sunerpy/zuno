//! HTTPS and PostgreSQL contract. The provider and tool are deterministic fixtures.

use async_trait::async_trait;
use axum::{Router, http::StatusCode, routing::post};
use futures::stream;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx_core::{query::query, raw_sql::raw_sql};
use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
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
use zuno_engine::state::{InputMaterialization, TurnPersistence, TurnStateError, TurnStateScope};
use zuno_error::{ProviderError, ToolError};
use zuno_identity::worker::{JobGrantAuthority, WorkerAuthority, WorkerSubject};
use zuno_identity::{
    AccessTokenVerifier, IdentityError, OAuth2ClaimsPolicy, OAuth2IntrospectionConfig,
    OAuth2IntrospectionVerifier, TokenIntrospector,
};
use zuno_llm::cache::{DynamicContext, McpToolStatus};
use zuno_llm::event::{FinishReason, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};
use zuno_permission::enterprise::OrganizationPolicy;
use zuno_postgres::{PostgresBackend, PostgresOptions, bootstrap_organization, migrate};
use zuno_server::enterprise_state::WorkerStateService;
use zuno_tool::{AllowAll, Tool, ToolContext, ToolOutput};
use zuno_types::identity::*;
use zuno_worker::{AccessTokenSource, WorkerClient};

#[path = "enterprise_state/browser.rs"]
mod browser;

#[derive(Deserialize)]
struct Fixture {
    admin_url: String,
    runtime_url: String,
    migration_url: String,
    root_certificate: PathBuf,
    runtime_role: String,
}
impl Fixture {
    fn options(&self, url: &str, database: &str) -> PostgresOptions {
        let prefix = url
            .strip_suffix("/postgres")
            .expect("isolated fixture database");
        PostgresOptions {
            url: format!("{prefix}/{database}"),
            root_certificate: Some(self.root_certificate.clone()),
            max_connections: 4,
        }
    }
}

struct Introspection;
#[async_trait]
impl TokenIntrospector for Introspection {
    async fn introspect(&self, token: &str) -> Result<Value, IdentityError> {
        if token != "worker-token" && token != "user-token" {
            return Ok(json!({"active":false}));
        }
        Ok(json!({
            "active":true,"iss":"https://issuer.example","aud":"state-api","sub":"worker-service",
            "client_id":"worker-client","scope":"worker",
            "actor":if token=="worker-token" {"workload"} else {"user"},
            "exp":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()+3600,
        }))
    }
}
struct Token(&'static str);
#[async_trait]
impl AccessTokenSource for Token {
    async fn token(&self) -> Result<zuno_auth::Secret, TurnStateError> {
        Ok(zuno_auth::Secret::new(self.0))
    }
}

#[derive(Debug)]
struct Script {
    responses: Mutex<VecDeque<Vec<StreamEvent>>>,
    calls: AtomicUsize,
}
impl Provider for Script {
    fn id(&self) -> &str {
        "wire-test"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }
    fn stream(&self, _request: CompletionRequest) -> ProviderStream<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(stream::iter(
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap()
                .into_iter()
                .map(Ok::<_, ProviderError>),
        ))
    }
}
struct Resolver;
impl AgentModelResolver for Resolver {
    fn resolve_agent(&self, name: &str) -> Option<ResolvedAgent> {
        (name == "build").then(|| {
            ResolvedAgent::new("build", "Inspect the fixture.")
                .with_max_steps(NonZeroU32::new(4).unwrap())
        })
    }
    fn resolve_model(&self, provider: &str, model: &str) -> Option<ResolvedModel> {
        (provider == "wire-test" && model == "model")
            .then(|| ResolvedModel::new(Spec::new("wire-test"), "model", ApiSurface::Default))
    }
}
struct Inspect(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Inspect {
    fn id(&self) -> &str {
        "inspect"
    }
    fn description(&self) -> &str {
        "Read the in-memory fixture."
    }
    fn raw_parameters_schema(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn execute(&self, _args: Value, _context: ToolContext) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("Fixture", "Read fixture"))
    }
}

struct DeferredDispatcher {
    inner: ToolRegistryDispatcher,
    turn_id: zuno_types::identity::TurnId,
    reference: Mutex<Option<zuno_types::wait::WaitRef>>,
}

#[async_trait]
impl zuno_engine::r#loop::ToolDispatcher for DeferredDispatcher {
    fn available_tools(&self) -> zuno_engine::r#loop::AvailableTools {
        self.inner.available_tools()
    }

    async fn prepare(
        &self,
        request: zuno_engine::r#loop::DispatchRequest,
    ) -> zuno_engine::r#loop::PreparedToolDispatch {
        use zuno_types::identity::{InvocationId, OperationId, WaitId};
        use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};
        if request.call.id != "waiting" {
            return self.inner.prepare(request).await;
        }
        let reference = WaitRef {
            id: WaitId::new("wire-wait").unwrap(),
            turn_id: self.turn_id.clone(),
            invocation_id: InvocationId::new(&request.call.id).unwrap(),
            arguments_sha256: zuno_orchestration::sha256_json(&request.call.input),
            target: WaitTarget::Operation {
                operation_id: OperationId::new("wire-operation").unwrap(),
            },
            continuation: WaitContinuation::CurrentTurn,
        };
        *self.reference.lock().unwrap() = Some(reference.clone());
        zuno_engine::r#loop::PreparedToolDispatch::Pending(reference)
    }
}

async fn tls_server(router: Router, fixture: &Fixture) -> (url::Url, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    tls_server_at(router, fixture, listener).await
}

async fn tls_server_at(
    router: Router,
    fixture: &Fixture,
    listener: tokio::net::TcpListener,
) -> (url::Url, tokio::task::JoinHandle<()>) {
    let root = fixture.root_certificate.parent().unwrap();
    let certs = CertificateDer::pem_file_iter(root.join("server.crt"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(root.join("server.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let service = hyper_util::service::TowerToHyperService::new(router.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let io = hyper_util::rt::TokioIo::new(tls);
                let builder = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                );
                let _ = builder.serve_connection_with_upgrades(io, service).await;
            });
        }
    });
    (
        url::Url::parse(&format!("https://localhost:{}/", address.port())).unwrap(),
        handle,
    )
}

#[tokio::test]
#[ignore = "run scripts/check_enterprise_postgres.py for the isolated TLS/PostgreSQL fixture"]
async fn authenticated_workers_resume_the_kernel_over_https_without_database_credentials() {
    let fixture: Fixture = serde_json::from_slice(
        &std::fs::read(std::env::var("ZUNO_POSTGRES_TEST_CONFIG").unwrap()).unwrap(),
    )
    .unwrap();
    let admin = fixture
        .options(&fixture.admin_url, "postgres")
        .connect()
        .await
        .unwrap();
    raw_sql("CREATE DATABASE zuno_worker_fixture OWNER zuno_preview_migrator")
        .execute(&admin)
        .await
        .unwrap();
    let migrator = fixture
        .options(&fixture.migration_url, "zuno_worker_fixture")
        .connect()
        .await
        .unwrap();
    let admin = fixture
        .options(&fixture.admin_url, "zuno_worker_fixture")
        .connect()
        .await
        .unwrap();
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    let backend =
        PostgresBackend::connect(fixture.options(&fixture.runtime_url, "zuno_worker_fixture"))
            .await
            .unwrap();
    let claims:OAuth2ClaimsPolicy=serde_json::from_value(json!({
        "tenantId":"wire-enterprise","audience":"state-api","allowedClients":["worker-client"],
        "requiredScopes":["worker"],"principalKind":"workload","actorClaim":{"claim":"actor","value":"workload"},
        "clockSkewSeconds":0,
    })).unwrap();
    let verifier = Arc::new(OAuth2IntrospectionVerifier::with_introspector(
        OAuth2IntrospectionConfig::new(
            "https://issuer.example",
            "https://issuer.example/introspect",
            claims,
        )
        .unwrap(),
        Arc::new(Introspection),
    ));
    let verified = verifier.verify("worker-token").await.unwrap();
    let tenant = verified.tenant_id().clone();
    let authority = Arc::new(
        WorkerAuthority::new(
            verifier,
            [WorkerSubject {
                tenant_id: tenant.clone(),
                principal_id: verified.principal_id().clone(),
                client_id: verified.client_id().clone(),
            }]
            .into(),
        )
        .unwrap(),
    );
    let grants = Arc::new(
        JobGrantAuthority::new("key".to_owned(), vec![("key".to_owned(), vec![7; 32])]).unwrap(),
    );
    let actor = PrincipalScope::new(
        tenant.clone(),
        PrincipalId::new("alice").unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("web").unwrap()),
        NonZeroU64::MIN,
    );
    bootstrap_organization(
        &migrator,
        &OrganizationPolicy {
            tenant_id: tenant.clone(),
            revision: NonZeroU64::MIN,
            allowed_apps: [ClientId::new("web").unwrap()].into(),
            approval_apps: [ClientId::new("web").unwrap()].into(),
            auto_read_apps: [ClientId::new("web").unwrap()].into(),
            approval_lifetime_seconds: 300,
        },
        &actor.owner(),
    )
    .await
    .unwrap();
    let workspace = WorkspaceId::new("workspace").unwrap();
    backend
        .register_workspace(&actor, &workspace, "Workspace")
        .await
        .unwrap();
    let app = AgentApplication::new(Arc::new(backend.sessions(actor.clone())));
    let session = app
        .create_session(CreateSession {
            request_id: RequestId::new("session").unwrap(),
            workspace_id: workspace,
            title: "HTTP".to_owned(),
        })
        .await
        .unwrap();
    query("UPDATE zuno_enterprise_preview.session SET agent='build',model=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(tenant.as_str()).bind(actor.principal_id().as_str()).bind(session.id.as_str())
        .bind(json!({"providerID":"wire-test","modelID":"model"})).execute(&admin).await.unwrap();
    let runtime = backend.runtime(tenant.clone());
    let configuration = ConfigurationRef {
        id: ConfigurationId::new("fixture").unwrap(),
        version: 1,
        sha256: "1".repeat(64),
    };
    let job = runtime
        .submit(
            &actor,
            JobSubmission {
                session_id: session.id.clone(),
                request_id: RequestId::new("input").unwrap(),
                expected_input_version: 0,
                text: "Inspect fixture".to_owned(),
                configuration: configuration.clone(),
            },
        )
        .await
        .unwrap();
    let failures = Arc::new(AtomicUsize::new(0));
    let failure_count = Arc::clone(&failures);
    let routes = WorkerStateService::new(
        backend.clone(),
        authority,
        grants,
        tenant,
        LeaseDuration::new(30_000).unwrap(),
    )
    .router()
    .route(
        "/failure/internal/worker/v1/claim",
        post(move || {
            let failures = Arc::clone(&failure_count);
            async move {
                failures.fetch_add(1, Ordering::SeqCst);
                StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    )
    .route(
        "/redirect/internal/worker/v1/claim",
        post(|| async { axum::response::Redirect::temporary("/internal/worker/v1/claim") }),
    );
    let (url, server) = tls_server(routes, &fixture).await;
    let ca =
        reqwest::Certificate::from_pem(&std::fs::read(&fixture.root_certificate).unwrap()).unwrap();
    assert!(
        WorkerClient::new(
            url::Url::parse("http://localhost/").unwrap(),
            Arc::new(Token("worker-token")),
            None
        )
        .is_err()
    );
    let untrusted = WorkerClient::new(url.clone(), Arc::new(Token("worker-token")), None).unwrap();
    assert!(
        untrusted
            .claim(WorkerInstanceId::new("untrusted").unwrap())
            .await
            .is_err()
    );
    let denied =
        WorkerClient::new(url.clone(), Arc::new(Token("user-token")), Some(ca.clone())).unwrap();
    assert!(
        denied
            .claim(WorkerInstanceId::new("user").unwrap())
            .await
            .is_err()
    );
    let failure = WorkerClient::new(
        url.join("failure/").unwrap(),
        Arc::new(Token("worker-token")),
        Some(ca.clone()),
    )
    .unwrap();
    assert!(
        failure
            .claim(WorkerInstanceId::new("failed").unwrap())
            .await
            .is_err()
    );
    assert_eq!(failures.load(Ordering::SeqCst), 1);
    let redirect = WorkerClient::new(
        url.join("redirect/").unwrap(),
        Arc::new(Token("worker-token")),
        Some(ca.clone()),
    )
    .unwrap();
    assert!(
        redirect
            .claim(WorkerInstanceId::new("redirected").unwrap())
            .await
            .is_err()
    );
    let client = WorkerClient::new(url, Arc::new(Token("worker-token")), Some(ca)).unwrap();
    let first = client
        .claim(WorkerInstanceId::new("first").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.input.text, "Inspect fixture");
    client.renew(&first).await.unwrap();
    let state = Arc::new(
        client
            .persistence(&first, "/executor/workspace".to_owned())
            .unwrap(),
    );
    let scope = TurnStateScope {
        owner: actor.owner(),
        session_id: session.id.to_string(),
    };
    assert_eq!(
        state.session(&scope).await.unwrap().directory.as_deref(),
        Some("/executor/workspace")
    );
    let model = first.input.model.as_ref().unwrap();
    state.consume_input(&scope,InputMaterialization {
                turn_id: None,
        input_id:Some(first.input.id.to_string()),
        message:MessageRecord::from_json(json!({
            "id":first.input.id,"sessionID":session.id,"role":"user","time":{"created":0},
            "agent":first.input.agent,"model":{"providerID":model.provider_id,"modelID":model.model_id},
        })).unwrap(),
        parts:vec![PartRecord::from_json(json!({
            "id":"wire-input","sessionID":session.id,"messageID":first.input.id,"type":"text","text":first.input.text,
        }),0).unwrap()],
    }).await.unwrap();
    let script = Arc::new(Script {
        responses: Mutex::new(VecDeque::from([
            vec![
                StreamEvent::ToolUseStart {
                    id: "call".to_owned(),
                    name: "inspect".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "call".to_owned(),
                    delta: "{}".to_owned(),
                },
                StreamEvent::ToolUseEnd {
                    id: "call".to_owned(),
                },
                StreamEvent::ToolUseStart {
                    id: "waiting".to_owned(),
                    name: "inspect".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "waiting".to_owned(),
                    delta: "{}".to_owned(),
                },
                StreamEvent::ToolUseEnd {
                    id: "waiting".to_owned(),
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
    let mut providers = ProviderRegistry::new();
    let source = Arc::clone(&script);
    providers.register("wire-test", move |_| source.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = DeferredDispatcher {
        inner: ToolRegistryDispatcher::new(
            vec![Arc::new(Inspect(Arc::clone(&calls)))],
            vec![],
            Arc::new(AllowAll),
            AuthorizationPolicy::Strict,
            McpToolStatus::Ready,
        ),
        turn_id: job.turn_id.clone(),
        reference: Mutex::new(None),
    };
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
                state.clone(),
                &providers,
                &Resolver,
                &dispatcher,
                &interrupt
            )
            .with_principal_scope(actor.clone()),
            sender
        ),
        async { while receiver.recv().await.is_some() {} },
    );
    assert!(
        matches!(outcome, Ok(AdvanceOutcome::Waiting { .. })),
        "{outcome:?}"
    );
    assert!(
        client
            .claim(WorkerInstanceId::new("waiting").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    use zuno_engine::wait::WaitCompletionStore;
    let completion = zuno_engine::wait::WaitCompletion::tool_result(
        zuno_types::identity::CompletionId::new("wire-completed").unwrap(),
        dispatcher.reference.lock().unwrap().clone().unwrap(),
        zuno_engine::r#loop::ToolDispatchResult::success(ToolOutput::text(
            "Remote",
            "Verified completion",
        )),
    );
    let fact = runtime.publish(&scope, &completion).await.unwrap();
    assert_eq!(runtime.publish(&scope, &completion).await.unwrap(), fact);
    let second = client
        .claim(WorkerInstanceId::new("second").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(second.lease().unwrap().epoch > first.lease().unwrap().epoch);
    assert!(state.touch(&scope).await.is_err());
    let checkpoint: CheckpointRef =
        serde_json::from_value(second.job.checkpoint.as_ref().unwrap().reference.clone()).unwrap();
    let state = Arc::new(
        client
            .persistence(&second, "/executor/workspace".to_owned())
            .unwrap(),
    );
    let (sender, mut receiver) = event_channel();
    let (outcome, _) = tokio::join!(
        advance_turn(
            request().resume(checkpoint),
            TurnContext::from_persistence(state, &providers, &Resolver, &dispatcher, &interrupt)
                .with_principal_scope(actor.clone()),
            sender
        ),
        async { while receiver.recv().await.is_some() {} },
    );
    assert!(
        matches!(outcome, Ok(AdvanceOutcome::Progressed { .. })),
        "{outcome:?}"
    );
    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    // Reconstruct from the Job after losing the consumption response.
    let third = client
        .claim(WorkerInstanceId::new("third").unwrap())
        .await
        .unwrap()
        .unwrap();
    let checkpoint: CheckpointRef =
        serde_json::from_value(third.job.checkpoint.as_ref().unwrap().reference.clone()).unwrap();
    let state = Arc::new(
        client
            .persistence(&third, "/executor/workspace".to_owned())
            .unwrap(),
    );
    let (sender, mut receiver) = event_channel();
    let (outcome, _) = tokio::join!(
        advance_turn(
            request().resume(checkpoint),
            TurnContext::from_persistence(state, &providers, &Resolver, &dispatcher, &interrupt)
                .with_principal_scope(actor.clone()),
            sender,
        ),
        async { while receiver.recv().await.is_some() {} },
    );
    assert!(
        matches!(outcome, Ok(AdvanceOutcome::Completed { steps: 2, .. })),
        "{outcome:?}"
    );
    assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
        JobPhase::Completed
    );
    server.abort();
    let _ = server.await;
}
