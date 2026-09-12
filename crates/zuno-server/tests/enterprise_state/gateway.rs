use super::*;
use zuno_application::{
    authorization::{AnswerApproval, ApprovalAnswer, ApprovalState, OrganizationStore},
    environment::{
        CommandOperation, Environment, OperationAuthority, OperationCompletionSink, OperationPhase,
        wire::*,
    },
};
use zuno_identity::gateway::{GatewayServiceAuthority, GatewayTicketAuthority};
use zuno_server::{
    enterprise_gateway::GatewayControlService,
    gateway_configuration::{ConfiguredGateways, GatewayDeployment},
    gateway_execution::GatewayExecutionService,
};
use zuno_worker::gateway::{GatewayClient, GatewayStateClient};

const IMAGE: &str = "public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce";

async fn lose_first_completion_response(
    axum::extract::State(lost): axum::extract::State<Arc<std::sync::atomic::AtomicBool>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let completion = request.uri().path().ends_with("/gateway/v1/completion");
    let response = next.run(request).await;
    if completion && response.status().is_success() && !lost.swap(true, Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    response
}

struct Services;
#[async_trait]
impl TokenIntrospector for Services {
    async fn introspect(&self, token: &str) -> Result<Value, IdentityError> {
        if !["worker", "gateway", "user"].contains(&token) {
            return Ok(json!({"active":false}));
        }
        Ok(json!({
            "active":true,"iss":"https://issuer.example","aud":"state-api","sub":token,
            "client_id":format!("{token}-app"),"scope":"service",
            "actor":if token=="user" {"user"} else {"workload"},
            "exp":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()+3600,
        }))
    }
}

async fn reply(
    worker: &WorkerClient,
    execution: &zuno_worker::WorkerExecution,
    gateway: &GatewayClient,
    command: GatewayCommand,
) -> Result<GatewayReply, zuno_application::ApplicationError> {
    let request = GatewayRequest::new(command)?;
    let issued = worker
        .gateway_ticket(execution, &request)
        .await
        .map_err(|_| zuno_application::ApplicationError::Unavailable)?;
    gateway.execute(&issued, &request).await
}

#[tokio::test]
#[ignore = "run scripts/check_enterprise_postgres.py; rootless Docker is additionally exercised when its socket is supplied"]
async fn gateway_requests_are_scoped_authenticated_and_still_require_current_human_approval() {
    let fixture: Fixture = serde_json::from_slice(
        &std::fs::read(std::env::var("ZUNO_POSTGRES_TEST_CONFIG").unwrap()).unwrap(),
    )
    .unwrap();
    let cluster = fixture
        .options(&fixture.admin_url, "postgres")
        .connect()
        .await
        .unwrap();
    raw_sql("CREATE DATABASE zuno_gateway_fixture OWNER zuno_preview_migrator")
        .execute(&cluster)
        .await
        .unwrap();
    let migrator = fixture
        .options(&fixture.migration_url, "zuno_gateway_fixture")
        .connect()
        .await
        .unwrap();
    let admin = fixture
        .options(&fixture.admin_url, "zuno_gateway_fixture")
        .connect()
        .await
        .unwrap();
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    let backend =
        PostgresBackend::connect(fixture.options(&fixture.runtime_url, "zuno_gateway_fixture"))
            .await
            .unwrap();
    let tenant = TenantId::new("gateway-http").unwrap();
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
            auto_read_apps: [app.clone()].into(),
            approval_apps: [app].into(),
            approval_lifetime_seconds: 300,
        },
        &actor.owner(),
    )
    .await
    .unwrap();
    let workspace = WorkspaceId::new("workspace").unwrap();
    backend
        .register_workspace(&actor, &workspace, "Gateway fixture")
        .await
        .unwrap();
    let session = AgentApplication::new(Arc::new(backend.sessions(actor.clone())))
        .create_session(CreateSession {
            request_id: RequestId::new("session").unwrap(),
            workspace_id: workspace,
            title: "Gateway".to_owned(),
        })
        .await
        .unwrap();
    let configuration = ConfigurationRef {
        id: ConfigurationId::new("gateway").unwrap(),
        version: 1,
        sha256: "7".repeat(64),
    };
    let runtime = backend.runtime(tenant.clone());
    let job = runtime
        .submit(
            &actor,
            JobSubmission {
                selection: None,
                session_id: session.id.clone(),
                request_id: RequestId::new("job").unwrap(),
                expected_input_version: 0,
                text: "gateway".to_owned(),
                configuration: configuration.clone(),
            },
        )
        .await
        .unwrap();
    let policy:OAuth2ClaimsPolicy=serde_json::from_value(json!({
        "tenantId":tenant,"audience":"state-api","allowedClients":["worker-app","gateway-app","user-app"],
        "requiredScopes":["service"],"principalKind":"workload","actorClaim":{"claim":"actor","value":"workload"},
    })).unwrap();
    let verifier = Arc::new(OAuth2IntrospectionVerifier::with_introspector(
        OAuth2IntrospectionConfig::new(
            "https://issuer.example",
            "https://issuer.example/introspect",
            policy,
        )
        .unwrap(),
        Arc::new(Services),
    ));
    let worker_identity = verifier.verify("worker").await.unwrap();
    let gateway_identity = verifier.verify("gateway").await.unwrap();
    let subject = |identity: &zuno_identity::VerifiedIdentity| WorkerSubject {
        tenant_id: identity.tenant_id().clone(),
        principal_id: identity.principal_id().clone(),
        client_id: identity.client_id().clone(),
    };
    let workers = Arc::new(
        WorkerAuthority::new(verifier.clone(), [subject(&worker_identity)].into()).unwrap(),
    );
    let gateways = Arc::new(
        GatewayServiceAuthority::new(
            verifier,
            [(
                subject(&gateway_identity),
                GatewayId::new("gateway").unwrap(),
            )]
            .into(),
        )
        .unwrap(),
    );
    let grants = Arc::new(
        JobGrantAuthority::new(
            "current".to_owned(),
            vec![("current".to_owned(), vec![4; 32])],
        )
        .unwrap(),
    );
    let tickets = Arc::new(
        GatewayTicketAuthority::new(
            "current".to_owned(),
            vec![("current".to_owned(), vec![5; 32])],
            30000,
        )
        .unwrap(),
    );
    let execution_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let execution_url = url::Url::parse(&format!(
        "https://localhost:{}/",
        execution_listener.local_addr().unwrap().port()
    ))
    .unwrap();
    let resolver = Arc::new(
        ConfiguredGateways::new(vec![GatewayDeployment {
            tenant: tenant.clone(),
            configuration: configuration.clone(),
            gateway_id: GatewayId::new("gateway").unwrap(),
            endpoint: execution_url,
            image: IMAGE.to_owned(),
            memory_bytes: 64 * 1024 * 1024,
            pids_limit: 32,
            cpu_millis: 500,
        }])
        .unwrap(),
    );
    let control = GatewayControlService::new(
        backend.clone(),
        tenant.clone(),
        workers.clone(),
        grants.clone(),
        gateways,
        tickets,
        resolver,
    );
    let worker_state = WorkerStateService::new(
        backend.clone(),
        workers,
        grants,
        tenant.clone(),
        LeaseDuration::new(300000).unwrap(),
    );
    let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let routes =
        worker_state
            .router()
            .merge(control.router())
            .layer(axum::middleware::from_fn_with_state(
                lost.clone(),
                lose_first_completion_response,
            ));
    let (endpoint, control_server) = tls_server(routes, &fixture).await;
    let certificate =
        reqwest::Certificate::from_pem(&std::fs::read(&fixture.root_certificate).unwrap()).unwrap();
    let worker = WorkerClient::new(
        endpoint.clone(),
        Arc::new(Token("worker")),
        Some(certificate.clone()),
    )
    .unwrap();
    let state = GatewayStateClient::new(
        endpoint.clone(),
        Arc::new(Token("gateway")),
        Some(certificate.clone()),
    )
    .unwrap();
    let execution = worker
        .claim(
            WorkerInstanceId::new("first").unwrap(),
            std::slice::from_ref(&configuration),
        )
        .await
        .unwrap()
        .unwrap();
    let request = GatewayRequest::new(GatewayCommand::Acquire).unwrap();
    let issued = worker.gateway_ticket(&execution, &request).await.unwrap();
    let context = state.resolve(&issued.ticket, &request).await.unwrap();
    assert_eq!(context.lease, execution.lease().unwrap());
    assert_eq!(context.assignment.environment.session_id, session.id);
    assert!(
        state
            .resolve(
                &issued.ticket,
                &GatewayRequest::new(GatewayCommand::Get).unwrap()
            )
            .await
            .is_err()
    );
    let impostor = GatewayStateClient::new(
        endpoint.clone(),
        Arc::new(Token("worker")),
        Some(certificate.clone()),
    )
    .unwrap();
    assert!(impostor.resolve(&issued.ticket, &request).await.is_err());
    let user =
        WorkerClient::new(endpoint, Arc::new(Token("user")), Some(certificate.clone())).unwrap();
    assert!(user.gateway_ticket(&execution, &request).await.is_err());
    let mut wrong = CommandOperation {
        id: OperationId::new("once").unwrap(),
        invocation_id: InvocationId::new("once").unwrap(),
        environment_id: EnvironmentId::new("another-session").unwrap(),
        expected_revision: 1,
        argv: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf 'ran\\n' >> /workspace/once; printf 'done\\n'".to_owned(),
        ],
    };
    assert!(
        worker
            .gateway_ticket(
                &execution,
                &GatewayRequest::new(GatewayCommand::PrepareCommand {
                    operation: wrong.clone()
                })
                .unwrap()
            )
            .await
            .is_err()
    );
    wrong.environment_id = context.assignment.environment.id.clone();
    let command = wrong;
    let environment = Environment {
        owner: actor.owner(),
        spec: context.assignment.environment.clone(),
        revision: 1,
    };
    let approval = state
        .prepare(GatewayOperationRequest {
            lease: context.lease.clone(),
            environment: environment.clone(),
            operation: command.clone(),
        })
        .await
        .unwrap();
    assert_eq!(approval.state, ApprovalState::Pending);
    assert!(
        state
            .authorize(&context.lease, &environment, &command)
            .await
            .is_err()
    );
    let mut changed = environment.clone();
    changed.spec.cpu_millis += 1;
    assert!(
        state
            .prepare(GatewayOperationRequest {
                lease: context.lease.clone(),
                environment: changed,
                operation: command.clone(),
            })
            .await
            .is_err()
    );
    let socket = std::env::var_os("ZUNO_ROOTLESS_DOCKER_SOCKET");
    assert!(
        socket.is_some() || std::env::var_os("ZUNO_GATEWAY_TEST_REQUIRED").is_none(),
        "the gateway execution gate requires a rootless Docker socket"
    );
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut host = None;
    let mut delivery = None;
    let mut execution_server = None;
    let client = GatewayClient::new(Some(certificate)).unwrap();
    if let Some(socket) = socket {
        let service = GatewayExecutionService::connect(
            GatewayId::new("gateway").unwrap(),
            &PathBuf::from(socket),
            &directory.path().join("gateway.sqlite"),
            state.clone(),
        )
        .await
        .unwrap();
        host = Some(service.environments());
        delivery = Some(service.clone());
        execution_server = Some(
            tls_server_at(service.router(), &fixture, execution_listener)
                .await
                .1,
        );
        let result = client.execute(&issued, &request).await.unwrap();
        assert!(matches!(result, GatewayReply::Environment(_)));
        assert!(
            reply(
                &worker,
                &execution,
                &client,
                GatewayCommand::SubmitCommand {
                    operation: command.clone()
                }
            )
            .await
            .is_err()
        );
    }
    backend
        .organizations(tenant.clone())
        .answer(
            &actor,
            AnswerApproval {
                request_id: RequestId::new("approve").unwrap(),
                approval_id: approval.id,
                answer: ApprovalAnswer::Approve,
            },
        )
        .await
        .unwrap();
    state
        .authorize(&context.lease, &environment, &command)
        .await
        .unwrap();
    if host.is_some() {
        // Lose the submission response and recover through authoritative inspect.
        reply(
            &worker,
            &execution,
            &client,
            GatewayCommand::SubmitCommand {
                operation: command.clone(),
            },
        )
        .await
        .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let result = reply(
                &worker,
                &execution,
                &client,
                GatewayCommand::Inspect {
                    operation_id: command.id.clone(),
                },
            )
            .await
            .unwrap();
            let GatewayReply::Operation(receipt) = result else {
                panic!("operation")
            };
            if receipt.phase == OperationPhase::Completed {
                assert_eq!(receipt.exit_code, Some(0));
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "gateway command did not finish"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // The write advanced the environment revision. Re-submission with the
        // old version must not restart it; callers query the original receipt.
        assert!(matches!(
            reply(
                &worker,
                &execution,
                &client,
                GatewayCommand::SubmitCommand {
                    operation: command.clone()
                }
            )
            .await,
            Err(zuno_application::ApplicationError::Conflict),
        ));
        let result = reply(
            &worker,
            &execution,
            &client,
            GatewayCommand::Output {
                operation_id: command.id.clone(),
                cursor: Default::default(),
                maximum_bytes: 65536,
            },
        )
        .await
        .unwrap();
        let GatewayReply::Output(output) = result else {
            panic!("output")
        };
        let bytes = output
            .chunks
            .into_iter()
            .flat_map(|chunk| chunk.bytes)
            .collect::<Vec<_>>();
        assert_eq!(bytes, b"done\n");
        // Read the persisted file through a second, independently approved operation.
        let GatewayReply::Environment(current) =
            reply(&worker, &execution, &client, GatewayCommand::Get)
                .await
                .unwrap()
        else {
            panic!("environment")
        };
        let verification = CommandOperation {
            id: OperationId::new("verify").unwrap(),
            invocation_id: InvocationId::new("verify").unwrap(),
            argv: vec!["cat".to_owned(), "/workspace/once".to_owned()],
            expected_revision: current.revision,
            ..command.clone()
        };
        let GatewayReply::Approval(approval) = reply(
            &worker,
            &execution,
            &client,
            GatewayCommand::PrepareCommand {
                operation: verification.clone(),
            },
        )
        .await
        .unwrap() else {
            panic!("approval")
        };
        backend
            .organizations(tenant.clone())
            .answer(
                &actor,
                AnswerApproval {
                    request_id: RequestId::new("approve-verification").unwrap(),
                    approval_id: approval.id,
                    answer: ApprovalAnswer::Approve,
                },
            )
            .await
            .unwrap();
        reply(
            &worker,
            &execution,
            &client,
            GatewayCommand::SubmitCommand {
                operation: verification.clone(),
            },
        )
        .await
        .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let GatewayReply::Operation(receipt) = reply(
                &worker,
                &execution,
                &client,
                GatewayCommand::Inspect {
                    operation_id: verification.id.clone(),
                },
            )
            .await
            .unwrap() else {
                panic!("operation")
            };
            if receipt.phase == OperationPhase::Completed {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let GatewayReply::Output(output) = reply(
            &worker,
            &execution,
            &client,
            GatewayCommand::Output {
                operation_id: verification.id,
                cursor: Default::default(),
                maximum_bytes: 65536,
            },
        )
        .await
        .unwrap() else {
            panic!("output")
        };
        assert_eq!(
            output
                .chunks
                .into_iter()
                .flat_map(|chunk| chunk.bytes)
                .collect::<Vec<_>>(),
            b"ran\n"
        );
    }
    query("UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=0 WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
        .bind(tenant.as_str()).bind(actor.principal_id().as_str()).bind(job.session_id.as_str()).execute(&admin).await.unwrap();
    assert!(worker.gateway_ticket(&execution, &request).await.is_err());
    assert!(state.resolve(&issued.ticket, &request).await.is_err());
    if let Some(delivery) = delivery {
        assert!(
            delivery.deliver_completions(128).await.is_err(),
            "a lost post-commit response must remain unacknowledged at the gateway"
        );
        assert!(lost.load(Ordering::SeqCst));
        assert_eq!(delivery.deliver_completions(128).await.unwrap(), 2);
        assert_eq!(delivery.deliver_completions(128).await.unwrap(), 0);
        let saved = backend
            .gateway_operations(GatewayId::new("gateway").unwrap())
            .completion(&actor.owner(), &command.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.receipt.exit_code, Some(0));
        assert_eq!(saved.operation, command);
        assert!(
            backend
                .gateway_operations(GatewayId::new("other-gateway").unwrap())
                .complete(&saved)
                .await
                .is_err()
        );
        let mut forged = saved.clone();
        forged.lease.attempt_id = ExecutionAttemptId::new("unadmitted-attempt").unwrap();
        assert!(state.publish(&forged).await.is_err());
        let mut changed = saved.clone();
        changed.output = vec![zuno_application::environment::OperationOutput {
            channel: zuno_application::environment::OutputChannel::Stdout,
            bytes: b"changed".to_vec(),
        }];
        assert!(state.publish(&changed).await.is_err());
        state.publish(&saved).await.unwrap();
        let count:i64=sqlx_core::query_scalar::query_scalar(
            "SELECT count(*) FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2 AND type='runtime.operation.completed'",
        ).bind(tenant.as_str()).bind(actor.principal_id().as_str()).fetch_one(&admin).await.unwrap();
        assert_eq!(
            count, 2,
            "lost acknowledgements must not duplicate completion facts"
        );
    }
    if let Some(host) = host {
        let environment = host
            .get(&actor.owner(), &context.assignment.environment.id)
            .await
            .unwrap();
        host.release(&actor.owner(), &environment.spec.id, environment.revision)
            .await
            .unwrap();
    }
    if let Some(server) = execution_server {
        server.abort();
        let _ = server.await;
    }
    control_server.abort();
    let _ = control_server.await;
}
