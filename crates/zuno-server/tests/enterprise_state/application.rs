use super::*;
use zuno_application::authorization::{
    ApprovalBinding, ApprovalProposal, ApprovalState, OrganizationStore,
};
use zuno_application::runtime::{JobInputModel, JobInputSelection};
use zuno_permission::enterprise::{EffectKind, IsolationFact, PreparedEffectFacts};
use zuno_server::enterprise_application::{ApplicationWorkspace, EnterpriseApplication};

struct Users;
#[async_trait]
impl TokenIntrospector for Users {
    async fn introspect(&self, token: &str) -> Result<Value, IdentityError> {
        let (subject, client, actor) = match token {
            "alice" => ("alice", "web", "user"),
            "alice-api" => ("alice", "api", "user"),
            "bob" => ("bob", "web", "user"),
            "workload" => ("alice", "web", "workload"),
            _ => return Ok(json!({"active":false})),
        };
        Ok(json!({
            "active":true,"iss":"https://api-issuer.example","aud":"enterprise-api",
            "sub":subject,"client_id":client,"scope":"agent","actor":actor,
            "exp":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()+3600,
        }))
    }
}

#[tokio::test]
#[ignore = "run scripts/check_enterprise_postgres.py for real TLS and PostgreSQL"]
async fn public_application_keeps_users_isolated_and_requires_current_policy_for_jobs_and_approvals()
 {
    let fixture: Fixture = serde_json::from_slice(
        &std::fs::read(std::env::var("ZUNO_POSTGRES_TEST_CONFIG").unwrap()).unwrap(),
    )
    .unwrap();
    let cluster = fixture
        .options(&fixture.admin_url, "postgres")
        .connect()
        .await
        .unwrap();
    raw_sql("CREATE DATABASE zuno_application_fixture OWNER zuno_preview_migrator")
        .execute(&cluster)
        .await
        .unwrap();
    let migrator = fixture
        .options(&fixture.migration_url, "zuno_application_fixture")
        .connect()
        .await
        .unwrap();
    let admin = fixture
        .options(&fixture.admin_url, "zuno_application_fixture")
        .connect()
        .await
        .unwrap();
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    let backend =
        PostgresBackend::connect(fixture.options(&fixture.runtime_url, "zuno_application_fixture"))
            .await
            .unwrap();
    let tenant = TenantId::new("application-contract").unwrap();
    let policy: OAuth2ClaimsPolicy = serde_json::from_value(json!({
        "tenantId":tenant,"audience":"enterprise-api","allowedClients":["web","api"],
        "requiredScopes":["agent"],"principalKind":"user",
        "actorClaim":{"claim":"actor","value":"user"},
    }))
    .unwrap();
    let verifier = Arc::new(OAuth2IntrospectionVerifier::with_introspector(
        OAuth2IntrospectionConfig::new(
            "https://api-issuer.example",
            "https://api-issuer.example/introspect",
            policy,
        )
        .unwrap(),
        Arc::new(Users),
    ));
    let alice = verifier
        .verify("alice")
        .await
        .unwrap()
        .attribution(NonZeroU64::MIN);
    let api_alice = verifier
        .verify("alice-api")
        .await
        .unwrap()
        .attribution(NonZeroU64::MIN);
    let bob = verifier
        .verify("bob")
        .await
        .unwrap()
        .attribution(NonZeroU64::MIN);
    bootstrap_organization(
        &migrator,
        &OrganizationPolicy {
            tenant_id: tenant.clone(),
            revision: NonZeroU64::MIN,
            allowed_apps: [
                alice.client_id().unwrap().clone(),
                api_alice.client_id().unwrap().clone(),
            ]
            .into(),
            approval_apps: [alice.client_id().unwrap().clone()].into(),
            auto_read_apps: [alice.client_id().unwrap().clone()].into(),
            approval_lifetime_seconds: 300,
        },
        &alice.owner(),
    )
    .await
    .unwrap();
    query("INSERT INTO zuno_enterprise_preview.organization_member(tenant_id,principal_id,role,active)
           VALUES($1,$2,'member',true)")
        .bind(tenant.as_str()).bind(bob.principal_id().as_str()).execute(&admin).await.unwrap();
    let configuration = ConfigurationRef {
        id: ConfigurationId::new("application").unwrap(),
        version: 1,
        sha256: "6".repeat(64),
    };
    let selection = JobInputSelection {
        agent: "build".to_owned(),
        model: JobInputModel {
            provider_id: "wire-test".to_owned(),
            model_id: "model".to_owned(),
        },
    };
    let application = EnterpriseApplication::new(
        backend.clone(),
        tenant.clone(),
        vec![ApplicationWorkspace {
            id: WorkspaceId::new("workspace").unwrap(),
            title: "Workspace".to_owned(),
            configuration: configuration.clone(),
            selection: selection.clone(),
        }],
    )
    .unwrap();
    let (endpoint, server) = tls_server(application.api_router(verifier), &fixture).await;
    let api = |path: &str| endpoint.join(&format!("api/v1/{path}")).unwrap();
    let certificate =
        reqwest::Certificate::from_pem(&std::fs::read(&fixture.root_certificate).unwrap()).unwrap();
    let http = reqwest::Client::builder()
        .add_root_certificate(certificate)
        .build()
        .unwrap();

    for token in [None, Some("unknown"), Some("workload")] {
        let request = http.get(api("workspaces"));
        let request = if let Some(token) = token {
            request.bearer_auth(token)
        } else {
            request
        };
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    }
    let workspaces = http
        .get(api("workspaces"))
        .bearer_auth("alice")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(workspaces, json!([{"id":"workspace","title":"Workspace"}]));

    let creation = json!({"requestId":"same","workspaceId":"workspace","title":"Investigate"});
    let session = http
        .post(api("sessions"))
        .bearer_auth("alice")
        .json(&creation)
        .send()
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    let session: Value = session.json().await.unwrap();
    let repeated = http
        .post(api("sessions"))
        .bearer_auth("alice")
        .json(&creation)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(session, repeated);
    let other = http
        .post(api("sessions"))
        .bearer_auth("bob")
        .json(&creation)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_ne!(session["id"], other["id"]);
    let session_id = session["id"].as_str().unwrap();
    let session_path = format!("sessions/{session_id}");
    assert_eq!(
        http.get(api(&session_path))
            .bearer_auth("bob")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let page = http
        .get(api("sessions"))
        .bearer_auth("alice")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(page["items"], json!([session]));
    assert_eq!(
        http.get(api("sessions?beforeUpdatedAt=1"))
            .bearer_auth("alice")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(
        !http
            .get(api("sessions?limit=0"))
            .bearer_auth("alice")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );

    let turn_path = format!("{session_path}/turns");
    let input = json!({"requestId":"turn","expectedInputVersion":"0","text":"Inspect the configured workspace"});
    let response = http
        .post(api(&turn_path))
        .bearer_auth("alice")
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let job: Value = response.json().await.unwrap();
    assert_eq!(job["inputVersion"], "1");
    assert_eq!(job["phase"], "ready");
    for private in [
        "lease",
        "grant",
        "checkpoint",
        "configuration",
        "principal",
        "result",
    ] {
        assert!(
            job.get(private).is_none(),
            "private Worker field leaked: {private}"
        );
    }
    assert_eq!(
        http.post(api(&turn_path))
            .bearer_auth("alice")
            .json(&input)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        job
    );
    let mut changed = input.clone();
    changed["text"] = json!("changed");
    assert_eq!(
        http.post(api(&turn_path))
            .bearer_auth("alice")
            .json(&changed)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        http.post(api(&turn_path))
            .bearer_auth("alice")
            .json(&json!({
                "requestId":"stale","expectedInputVersion":"0","text":"stale input"
            }))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        http.post(api(&turn_path))
            .bearer_auth("alice")
            .json(&json!({
                "requestId":"malformed","expectedInputVersion":"00","text":"malformed version"
            }))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mut forged = input.clone();
    forged["principal"] = json!({"principalId":bob.principal_id()});
    assert!(
        !http
            .post(api(&turn_path))
            .bearer_auth("alice")
            .json(&forged)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let job_path = format!("jobs/{}", job["id"].as_str().unwrap());
    assert_eq!(
        http.get(api(&job_path))
            .bearer_auth("bob")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        http.get(api(&job_path))
            .bearer_auth("alice")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        job
    );

    let runtime = backend.runtime(tenant.clone());
    let claimed = runtime
        .claim(
            &WorkerInstanceId::new("fixture").unwrap(),
            LeaseDuration::new(300_000).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    let primary = backend
        .worker_state(claimed.lease.clone())
        .primary_input()
        .await
        .unwrap();
    assert_eq!(primary.agent.as_deref(), Some("build"));
    assert_eq!(primary.model, Some(selection.model));
    let authority = backend.organizations(tenant.clone());
    let approval = authority
        .admit(
            &claimed.lease,
            ApprovalProposal {
                binding: ApprovalBinding {
                    job_id: claimed.job.id.clone(),
                    session_id: claimed.job.session_id.clone(),
                    turn_id: claimed.job.turn_id.clone(),
                    invocation_id: InvocationId::new("command").unwrap(),
                    operation_id: OperationId::new("operation").unwrap(),
                    arguments_sha256: "a".repeat(64),
                    resources_sha256: "b".repeat(64),
                    effect: EffectKind::Process,
                },
                facts: PreparedEffectFacts {
                    kind: EffectKind::Process,
                    resource_authorized: true,
                    isolation: IsolationFact::Enforced,
                    builtin_handler: true,
                    sensitive: false,
                    explicit_deny: false,
                    mandatory_human: false,
                },
                presentation: json!({"argv":["printf","hello"]}),
            },
        )
        .await
        .unwrap();
    assert_eq!(approval.state, ApprovalState::Pending);
    let approval_path = format!("approvals/{}/answer", approval.id);
    let decision = json!({"requestId":"answer","answer":"approve"});
    for token in ["bob", "alice-api"] {
        assert_eq!(
            http.post(api(&approval_path))
                .bearer_auth(token)
                .json(&decision)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let approved = http
        .post(api(&approval_path))
        .bearer_auth("alice")
        .json(&decision)
        .send()
        .await
        .unwrap();
    assert_eq!(approved.status(), StatusCode::OK);
    assert_eq!(approved.json::<Value>().await.unwrap()["state"], "approved");
    assert_eq!(
        http.post(api(&approval_path))
            .bearer_auth("alice")
            .json(&decision)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["state"],
        "approved"
    );

    query("UPDATE zuno_enterprise_preview.organization_member SET active=false WHERE tenant_id=$1 AND principal_id=$2")
        .bind(tenant.as_str()).bind(alice.principal_id().as_str()).execute(&admin).await.unwrap();
    for path in [
        "workspaces".to_owned(),
        "sessions".to_owned(),
        session_path.clone(),
        job_path,
        format!("{session_path}/input-version"),
        format!("approvals/{}", approval.id),
    ] {
        assert_eq!(
            http.get(api(&path))
                .bearer_auth("alice")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        http.post(api("sessions"))
            .bearer_auth("alice")
            .json(&creation)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http.post(api(&turn_path))
            .bearer_auth("alice")
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http.get(api("sessions"))
            .bearer_auth("bob")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    server.abort();
    let _ = server.await;
}
