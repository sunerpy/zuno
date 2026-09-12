//! Independent executable roles against a real TLS issuer/provider, PostgreSQL
//! and the task-owned rootless Docker daemon.

use aws_lc_rs::{
    rsa::{KeyPair, KeySize},
    signature::KeyPair as _,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, raw_sql::raw_sql};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zuno_engine::interrupt::InterruptSignal;
use zuno_enterprise::config::*;
use zuno_postgres::PostgresOptions;
use zuno_types::identity::*;

#[path = "processes/browser.rs"]
mod browser;

#[derive(Deserialize)]
struct Fixture {
    admin_url: String,
    migration_url: String,
    runtime_url: String,
    root_certificate: PathBuf,
    runtime_role: String,
}
impl Fixture {
    fn options(&self, url: &str, database: &str) -> PostgresOptions {
        PostgresOptions {
            url: format!("{}/{database}", url.strip_suffix("/postgres").unwrap()),
            root_certificate: Some(self.root_certificate.clone()),
            max_connections: 4,
        }
    }
    fn tls(&self, listen: SocketAddr) -> TlsConfig {
        let parent = self.root_certificate.parent().unwrap();
        TlsConfig {
            listen,
            certificate_file: parent.join("server.crt"),
            private_key_file: parent.join("server.key"),
            max_connections: 64,
        }
    }
}

struct Issuer {
    origin: String,
    key: KeyPair,
    model_requests: AtomicUsize,
    codes: std::sync::Mutex<BTreeMap<String, browser::Code>>,
}
impl Issuer {
    fn token(&self, subject: &str, client: &str, user: bool) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({
            "iss":self.origin,"aud":if user {"native-api"} else {"native-state"},
            "sub":subject,"client_id":client,"actor":if user {"user"} else {"workload"},
            "scope":if user {"agent"} else {"service"},"iat":now,"exp":now+3600,
            "jti":uuid::Uuid::new_v4().to_string(),
        });
        self.sign("at+jwt", claims)
    }
    fn sign(&self, typ: &str, claims: Value) -> String {
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({"alg":"RS256","typ":typ,"kid":"native"})).unwrap()
            ),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &aws_lc_rs::signature::RSA_PKCS1_SHA256,
                &aws_lc_rs::rand::SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .unwrap();
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
    }
}
async fn metadata(State(issuer): State<Arc<Issuer>>) -> Json<Value> {
    Json(
        json!({"issuer":issuer.origin,"jwks_uri":format!("{}/jwks",issuer.origin),
        "authorization_endpoint":format!("{}/authorize",issuer.origin),
        "token_endpoint":format!("{}/token",issuer.origin),"response_types_supported":["code"]}),
    )
}
async fn keys(State(issuer): State<Arc<Issuer>>) -> Json<Value> {
    let key =
        aws_lc_rs::signature::RsaPublicKeyComponents::<Vec<u8>>::from(issuer.key.public_key());
    Json(
        json!({"keys":[{"kid":"native","kty":"RSA","alg":"RS256","use":"sig",
        "n":URL_SAFE_NO_PAD.encode(key.n),"e":URL_SAFE_NO_PAD.encode(key.e)}]}),
    )
}
async fn model(
    State(issuer): State<Arc<Issuer>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers[header::AUTHORIZATION], "Bearer fixture-model-key");
    issuer.model_requests.fetch_add(1, Ordering::SeqCst);
    // Ensure two one-slot Workers can claim distinct ready sessions.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let messages = body["messages"].as_array().unwrap();
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .to_string();
    let name = if user.contains("alice") {
        "alice"
    } else {
        assert!(user.contains("bob"));
        "bob"
    };
    let other = if name == "alice" { "bob" } else { "alice" };
    assert!(!body.to_string().contains(&format!("MEMORY-PROBE-{other}")));
    let has_tool = |id: &str| {
        messages
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == id)
    };
    if user.contains("BROWSER-PROBE") {
        let completed = has_tool("browser-command");
        let delta = if completed {
            assert!(body.to_string().contains("browser-operation-ok"));
            json!({"role":"assistant","content":"BROWSER-COMPLETE: 已完成批准的命令。"})
        } else {
            json!({"role":"assistant","tool_calls":[{
                "index":0,"id":"browser-command","type":"function","function":{
                    "name":"environment_command",
                    "arguments":json!({"argv":["sh","-c","printf 'browser-operation-ok\\n'"]}).to_string()
                }
            }]})
        };
        return model_response(delta, completed);
    }
    let live_context = messages
        .iter()
        .filter(|message| message["role"] == "system" || message["role"] == "developer")
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let is_child = live_context.contains("CHILD-EXECUTOR");
    let completed = if is_child {
        has_tool("child-command")
    } else {
        has_tool("native-child")
    };
    if is_child || has_tool("native-command") {
        assert!(
            !live_context.contains("MEMORY-PROBE-"),
            "revoked Memory must not return from the checkpoint"
        );
        assert!(!live_context.contains("MEMORY-LEARNED-"));
    } else {
        assert!(
            live_context.contains(&format!("MEMORY-PROBE-{name}")),
            "{live_context}"
        );
    }
    let delta = if is_child && !completed {
        json!({"role":"assistant","tool_calls":[{
            "index":0,"id":"child-command","type":"function","function":{
                "name":"environment_command","arguments":json!({"argv":["sh","-c",
                    "test \"$(cat /workspace/native-once)\" = once; printf child > /workspace/native-once; printf 'child-workspace-ok\\n'"
                ]}).to_string()
            }
        }]})
    } else if is_child {
        json!({"role":"assistant","content":format!("CHILD-VERIFIED-{name}")})
    } else if completed {
        assert!(body.to_string().contains(&format!("CHILD-VERIFIED-{name}")));
        json!({"role":"assistant","content":"Completed the approved operation."})
    } else if has_tool("native-command") {
        json!({"role":"assistant","tool_calls":[{
            "index":0,"id":"native-child","type":"function","function":{
                "name":"task","arguments":json!({
                    "agent":"workspace-helper","objective":format!("Inspect inherited workspace for {name}"),
                    "deliverable":"A verified workspace result","instructions":format!("Verify inherited parent files for {name} in the child workspace."),
                    "success_evidence":"Read the inherited file and change only the child's copy."
                }).to_string()
            }
        }]})
    } else if !has_tool("native-memory-read") {
        json!({"role":"assistant","tool_calls":[{
            "index":0,"id":"native-memory-read","type":"function","function":{
                "name":"memory_read","arguments":json!({"target":"project","limit":4}).to_string()
            }
        }]})
    } else if !has_tool("native-memory-update") {
        json!({"role":"assistant","tool_calls":[{
            "index":0,"id":"native-memory-update","type":"function","function":{
                "name":"memory_update","arguments":json!({
                    "target":"project","action":"add","content":format!("MEMORY-LEARNED-{name}"),
                    "reason":"Explicit private Memory consent","expected_revision":2,"confidence":1.0
                }).to_string()
            }
        }]})
    } else {
        assert!(
            live_context.contains(&format!("MEMORY-LEARNED-{name}")),
            "a committed Memory update must refresh the next request"
        );
        json!({"role":"assistant","tool_calls":[{
            "index":0,"id":"native-command","type":"function","function":{
                "name":"environment_command",
                "arguments":json!({"argv":["sh","-c",
                    "test ! -S /var/run/docker.sock && printf 'once\\n' >> /workspace/native-once && cat /workspace/native-once"
                ]}).to_string(),
            }
        }]})
    };
    model_response(delta, completed)
}
fn model_response(delta: Value, completed: bool) -> Response {
    let frames = [
        json!({"id":"native-response","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"native-response","object":"chat.completion.chunk","model":"model","choices":[{"index":0,"delta":{},"finish_reason":if completed {"stop"} else {"tool_calls"}}],
            "usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}),
    ];
    let mut output = frames
        .into_iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect::<String>();
    output.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], output).into_response()
}
fn address() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}
fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
fn verifier(issuer: &Issuer, fixture: &Fixture, user: bool) -> VerifierConfig {
    VerifierConfig::Jwt {
        config:serde_json::from_value(json!({
            "authority":{"issuer":issuer.origin},
            "claims":{"tenantId":"native-enterprise","audience":if user {"native-api"} else {"native-state"},
                "allowedClients":if user {vec!["web"]} else {vec!["worker","gateway"]},
                "requiredScopes":if user {vec!["agent"]} else {vec!["service"]},
                "principalKind":if user {"user"} else {"workload"},
                "actorClaim":{"claim":"actor","value":if user {"user"} else {"workload"}}},
            "profile":{"type":"rfc9068"},
        })).unwrap(),
        root_certificate:Some(fixture.root_certificate.clone()),
    }
}
async fn command(root: &Path, name: &str, service: ServiceRole) -> tokio::process::Command {
    let file = root.join(format!("{name}.json"));
    write(
        &file,
        serde_json::to_vec(&ServiceConfig {
            state_directory: root.join(format!("{name}-state")),
            service,
        })
        .unwrap(),
    );
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_zuno-enterprise"));
    command
        .arg("--config")
        .arg(file)
        .env("NO_PROXY", "localhost,127.0.0.1")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command.stderr(Stdio::from(
        std::fs::File::create(root.join(format!("{name}.log"))).unwrap(),
    ));
    command
}
async fn run_once(root: &Path, name: &str, service: ServiceRole) -> Value {
    let mut command = command(root, name, service).await;
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "role {name} failed: {}",
        std::fs::read_to_string(root.join(format!("{name}.log"))).unwrap()
    );
    if output.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
async fn spawn(root: &Path, name: &str, service: ServiceRole) -> tokio::process::Child {
    command(root, name, service)
        .await
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

#[tokio::test]
#[ignore = "requires scripts/check_enterprise_docker.py with isolated PostgreSQL and rootless Docker"]
async fn independent_control_gateway_and_two_workers_complete_isolated_approved_jobs() {
    let fixture: Fixture = serde_json::from_slice(
        &std::fs::read(std::env::var("ZUNO_POSTGRES_TEST_CONFIG").unwrap()).unwrap(),
    )
    .unwrap();
    let socket = PathBuf::from(
        std::env::var("ZUNO_ROOTLESS_DOCKER_SOCKET").expect("required rootless daemon"),
    );
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let issuer_address = address();
    let issuer = Arc::new(Issuer {
        origin: format!("https://{issuer_address}"),
        key: KeyPair::generate(KeySize::Rsa2048).unwrap(),
        model_requests: AtomicUsize::new(0),
        codes: Default::default(),
    });
    let routes = Router::new()
        .route("/.well-known/openid-configuration", get(metadata))
        .route("/jwks", get(keys))
        .route("/authorize", get(browser::authorize))
        .route("/token", post(browser::exchange))
        .route("/v1/chat/completions", post(model))
        .with_state(issuer.clone());
    let issuer_stopped = InterruptSignal::new();
    let issuer_task = {
        let options = fixture.tls(issuer_address);
        let stopped = issuer_stopped.clone();
        tokio::spawn(async move {
            zuno_enterprise::serve_tls(&options, routes, stopped)
                .await
                .unwrap()
        })
    };
    let user_verifier = verifier(&issuer, &fixture, true);
    let service_verifier = verifier(&issuer, &fixture, false);
    let mut identities = BTreeMap::new();
    let mut tokens = BTreeMap::new();
    for (name, client, user) in [
        ("alice", "web", true),
        ("bob", "web", true),
        ("worker", "worker", false),
        ("gateway", "gateway", false),
    ] {
        let token = issuer.token(name, client, user);
        let path = root.join(format!("{name}.token"));
        write(&path, &token);
        let identity = run_once(
            root,
            &format!("identity-{name}"),
            ServiceRole::Identity(IdentityConfig {
                verifier: if user {
                    user_verifier.clone()
                } else {
                    service_verifier.clone()
                },
                access_token_file: path,
            }),
        )
        .await;
        identities.insert(name, identity);
        tokens.insert(name, token);
    }
    let tenant = TenantId::new("native-enterprise").unwrap();
    let owner = |name: &str| PrincipalKey {
        tenant_id: tenant.clone(),
        principal_id: PrincipalId::new(identities[name]["principalId"].as_str().unwrap()).unwrap(),
    };
    let web = ClientId::new(identities["alice"]["clientId"].as_str().unwrap()).unwrap();
    let cluster = fixture
        .options(&fixture.admin_url, "postgres")
        .connect()
        .await
        .unwrap();
    raw_sql("CREATE DATABASE zuno_process_fixture OWNER zuno_preview_migrator")
        .execute(&cluster)
        .await
        .unwrap();
    let migration_url = root.join("migration.url");
    let runtime_url = root.join("runtime.url");
    write(
        &migration_url,
        &fixture
            .options(&fixture.migration_url, "zuno_process_fixture")
            .url,
    );
    write(
        &runtime_url,
        &fixture
            .options(&fixture.runtime_url, "zuno_process_fixture")
            .url,
    );
    run_once(
        root,
        "migrate",
        ServiceRole::Migrate(MigrationConfig {
            database: DatabaseConfig {
                url_file: migration_url,
                root_certificate: Some(fixture.root_certificate.clone()),
                max_connections: 4,
            },
            runtime_role: fixture.runtime_role.clone(),
            bootstrap: Some(BootstrapConfig {
                administrator: owner("alice"),
                policy: zuno_permission::enterprise::OrganizationPolicy {
                    tenant_id: tenant.clone(),
                    revision: std::num::NonZeroU64::MIN,
                    allowed_apps: [web.clone()].into(),
                    approval_apps: [web.clone()].into(),
                    auto_read_apps: [web].into(),
                    approval_lifetime_seconds: 300,
                },
            }),
        }),
    )
    .await;
    let admin = fixture
        .options(&fixture.admin_url, "zuno_process_fixture")
        .connect()
        .await
        .unwrap();
    query("INSERT INTO zuno_enterprise_preview.organization_member(tenant_id,principal_id,role,active) VALUES($1,$2,'member',true)")
        .bind(tenant.as_str()).bind(owner("bob").principal_id.as_str()).execute(&admin).await.unwrap();
    let control_address = address();
    let gateway_address = address();
    let control_url = format!("https://{control_address}/");
    let browser_assets = std::env::var_os("ZUNO_ENTERPRISE_WEB_DIST").map(PathBuf::from);
    let browser_config = browser_assets
        .as_ref()
        .map(|_| browser::config(root, &fixture, &issuer, &control_url));
    let mut definition:Definition=serde_json::from_value(json!({
        "id":"native","version":1,"workspace":{"id":"workspace","title":"Workspace"},
        "agent":{"name":"build","systemPrompt":"Complete the user's task through approved tools.","maxSteps":8},
        "model":{"providerId":"fixture","modelId":"model","transport":"openai-compatible","surface":"chat",
            "baseUrl":format!("{}/v1",issuer.origin),"credential":"model","contextTokens":16000,"maxOutputTokens":512},
        "budget":{"tokens":100000,"toolCalls":8,"durationSeconds":300},
        "environment":{"gatewayId":"native","endpoint":format!("https://{gateway_address}/"),
            "image":"public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce",
            "memoryBytes":67108864,"pidsLimit":32,"cpuMillis":500},
    })).unwrap();
    let mut child_definition = definition.clone();
    child_definition.id = zuno_types::identity::ConfigurationId::new("native-child").unwrap();
    child_definition.agent.name = "workspace-helper".to_owned();
    child_definition.agent.system_prompt="CHILD-EXECUTOR: inspect inherited files and keep edits inside the assigned child workspace.".to_owned();
    let child_definition_file = root.join("child-definition.json");
    write(
        &child_definition_file,
        serde_json::to_vec(&child_definition).unwrap(),
    );
    let reference = tokio::process::Command::new(env!("CARGO_BIN_EXE_zuno-enterprise"))
        .arg("--definition-ref")
        .arg(&child_definition_file)
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(reference.status.success());
    let child_reference = serde_json::from_slice(&reference.stdout).unwrap();
    definition.delegation = Some(DelegationDefinition {
        targets: vec![child_reference],
        maximum_depth: 2,
        maximum_children: 4,
    });
    let definition_file = root.join("definition.json");
    write(&definition_file, serde_json::to_vec(&definition).unwrap());
    let job_key = root.join("job.key");
    write(&job_key, [4u8; 32]);
    let gateway_key = root.join("gateway.key");
    write(&gateway_key, [5u8; 32]);
    let subject = |name: &str| zuno_identity::worker::WorkerSubject {
        tenant_id: tenant.clone(),
        principal_id: owner(name).principal_id,
        client_id: ClientId::new(identities[name]["clientId"].as_str().unwrap()).unwrap(),
    };
    let mut children = Vec::new();
    children.push(
        spawn(
            root,
            "control",
            ServiceRole::ControlPlane(Box::new(ControlConfig {
                web_assets_directory: browser_assets,
                memory: Default::default(),
                tenant_id: tenant.clone(),
                tls: fixture.tls(control_address),
                database: DatabaseConfig {
                    url_file: runtime_url,
                    root_certificate: Some(fixture.root_certificate.clone()),
                    max_connections: 16,
                },
                user_identity: user_verifier,
                service_identity: service_verifier,
                workers: [subject("worker")].into(),
                gateways: vec![GatewaySubject {
                    subject: subject("gateway"),
                    gateway_id: GatewayId::new("native").unwrap(),
                }],
                job_keys: KeyFiles {
                    active: "current".to_owned(),
                    keys: vec![KeyFile {
                        id: "current".to_owned(),
                        path: job_key,
                    }],
                },
                gateway_keys: KeyFiles {
                    active: "current".to_owned(),
                    keys: vec![KeyFile {
                        id: "current".to_owned(),
                        path: gateway_key,
                    }],
                },
                definitions: vec![definition_file.clone(), child_definition_file.clone()],
                active_definitions: vec![DefinitionKey {
                    id: definition.id.clone(),
                    version: 1,
                }],
                browser: browser_config,
                lease_millis: 30000,
            })),
        )
        .await,
    );
    let state = |name: &str| StateClientConfig {
        endpoint: control_url.clone(),
        access_token_file: root.join(format!("{name}.token")),
        root_certificate: Some(fixture.root_certificate.clone()),
    };
    children.push(
        spawn(
            root,
            "gateway",
            ServiceRole::Gateway(GatewayConfig {
                id: GatewayId::new("native").unwrap(),
                tls: fixture.tls(gateway_address),
                state: state("gateway"),
                docker_socket: socket,
                delivery_millis: 100,
            }),
        )
        .await,
    );
    write(&root.join("model.key"), "fixture-model-key");
    for name in ["worker-a", "worker-b"] {
        children.push(
            spawn(
                root,
                name,
                ServiceRole::Worker(WorkerConfig {
                    live_millis: Some(100),
                    instance_prefix: name.to_owned(),
                    state: state("worker"),
                    definitions: vec![definition_file.clone(), child_definition_file.clone()],
                    credentials: [(
                        "model".to_owned(),
                        ModelCredential {
                            api_key_file: Some(root.join("model.key")),
                            root_certificate: Some(fixture.root_certificate.clone()),
                        },
                    )]
                    .into(),
                    slots: 1,
                    poll_millis: 100,
                    renew_millis: 100,
                    drain_seconds: 5,
                }),
            )
            .await,
        );
    }
    let http = reqwest::Client::builder()
        .add_root_certificate(
            reqwest::Certificate::from_pem(&std::fs::read(&fixture.root_certificate).unwrap())
                .unwrap(),
        )
        .build()
        .unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(response) = http
            .get(format!("{control_url}api/v1/workspaces"))
            .bearer_auth(&tokens["alice"])
            .send()
            .await
            && response.status().is_success()
        {
            break;
        }
        for (index, child) in children.iter_mut().enumerate() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "service {index} exited; logs: {}",
                std::fs::read_to_string(
                    root.join(
                        ["control.log", "gateway.log", "worker-a.log", "worker-b.log"][index]
                    )
                )
                .unwrap()
            );
        }
        assert!(
            tokio::time::Instant::now() < until,
            "control plane did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut jobs = Vec::new();
    for name in ["alice", "bob"] {
        let session: Value = http
            .post(format!("{control_url}api/v1/sessions"))
            .bearer_auth(&tokens[name])
            .json(&json!({"requestId":"session","workspaceId":"workspace","title":"Native task"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let memory_url = format!("{control_url}api/v1/workspaces/workspace/memory");
        let proposal:Value = http.post(&memory_url).bearer_auth(&tokens[name])
            .json(&json!({"requestId":"seed-memory","command":{"kind":"propose","change":{
                "scope":"project","action":"add","content":format!("MEMORY-PROBE-{name}"),
                "oldText":null,"reason":"User-owned private convention","expectedRevision":null,"confidence":1.0
            }}})).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let candidate = proposal["result"]["Ok"]["candidate"]["id"]
            .as_str()
            .unwrap();
        for (id, command) in [
            (
                "apply-memory",
                json!({"kind":"apply","candidateId":candidate,"expectedState":proposal["result"]["Ok"]["stateDigest"]}),
            ),
            (
                "memory-consent",
                json!({"kind":"set_policy","sessionId":null,"expectedRevision":0,"useMemories":true,"generatePrivate":true}),
            ),
        ] {
            let response: Value = http
                .post(&memory_url)
                .bearer_auth(&tokens[name])
                .json(&json!({"requestId":id,"command":command}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(response["result"]["Ok"].is_object(), "{response}");
        }
        let job: Value = http
            .post(format!(
                "{control_url}api/v1/sessions/{}/turns",
                session["id"].as_str().unwrap()
            ))
            .bearer_auth(&tokens[name])
            .json(&json!({"requestId":"turn","expectedInputVersion":"0","text":format!("Run once for {name}")}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        jobs.push((name, job["id"].as_str().unwrap().to_owned()));
    }
    let mut approved = std::collections::BTreeSet::new();
    let mut memory_disabled = std::collections::BTreeSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let mut completed = 0;
        for (name, id) in &jobs {
            let job: Value = http
                .get(format!("{control_url}api/v1/jobs/{id}"))
                .bearer_auth(&tokens[name])
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            if job["phase"] == "completed" {
                completed += 1;
                continue;
            }
            if !matches!(job["phase"].as_str(), Some("ready" | "running" | "waiting")) {
                let events:Vec<Value>=query_scalar(
                    "SELECT data FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND session_id=$2
                     AND type IN ('runtime.driver.advance','runtime.operation.completed','runtime.job.finished')
                     ORDER BY sequence DESC LIMIT 6",
                ).bind(tenant.as_str()).bind(job["sessionId"].as_str().unwrap()).fetch_all(&admin).await.unwrap();
                let operations:Vec<Value>=query_scalar(
                    "SELECT to_jsonb(o) FROM zuno_enterprise_preview.gateway_operation o WHERE tenant_id=$1 AND job_id=$2",
                ).bind(tenant.as_str()).bind(id).fetch_all(&admin).await.unwrap();
                let logs = ["control.log", "gateway.log", "worker-a.log", "worker-b.log"]
                    .into_iter()
                    .map(|name| {
                        format!(
                            "{name}: {}",
                            std::fs::read_to_string(root.join(name)).unwrap()
                        )
                    })
                    .collect::<Vec<_>>();
                let ledger =
                    rusqlite::Connection::open(root.join("gateway-state/gateway.sqlite")).unwrap();
                let states = ledger
                    .prepare("SELECT data FROM operation")
                    .unwrap()
                    .query_map([], |row| row.get::<_, String>(0))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                let mut docker_states = Vec::new();
                for state in &states {
                    let state: Value = serde_json::from_str(state).unwrap();
                    if let Some(container) = state.get("container").and_then(Value::as_str) {
                        let mut command = tokio::process::Command::new("docker");
                        command
                            .args([
                                "--host",
                                &format!(
                                    "unix://{}",
                                    std::env::var("ZUNO_ROOTLESS_DOCKER_SOCKET").unwrap()
                                ),
                                "inspect",
                                "--format",
                                "{{json .State}}",
                                container,
                            ])
                            .kill_on_drop(true);
                        let inspected =
                            tokio::time::timeout(Duration::from_secs(5), command.output()).await;
                        docker_states.push(match inspected {
                            Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout).into_owned(),
                            _ => "bounded Docker inspection unavailable".to_owned(),
                        });
                    }
                }
                panic!(
                    "unexpected Job state {job}; events={events:?}; operations={operations:?}; ledger={states:?}; docker={docker_states:?}; logs={logs:?}"
                );
            }
            let mut waiting_jobs = vec![job.clone()];
            let mut cursor = 0;
            while cursor < waiting_jobs.len() {
                let current = waiting_jobs[cursor].clone();
                cursor += 1;
                for wait in current["waits"].as_array().unwrap() {
                    if wait["target"]["kind"] == "child" {
                        let child_id = wait["target"]["job_id"].as_str().unwrap();
                        let child: Value = http
                            .get(format!("{control_url}api/v1/jobs/{child_id}"))
                            .bearer_auth(&tokens[name])
                            .send()
                            .await
                            .unwrap()
                            .error_for_status()
                            .unwrap()
                            .json()
                            .await
                            .unwrap();
                        assert!(
                            matches!(
                                child["phase"].as_str(),
                                Some("ready" | "running" | "waiting" | "completed")
                            ),
                            "child failed: {child}"
                        );
                        waiting_jobs.push(child);
                        assert!(waiting_jobs.len() <= 16, "bounded child fixture");
                    }
                    if wait["target"]["kind"] == "approval" {
                        let id = wait["target"]["approval_id"].as_str().unwrap();
                        if approved.insert(id.to_owned()) {
                            if memory_disabled.insert(*name) {
                                let disabled:Value = http.post(format!("{control_url}api/v1/workspaces/workspace/memory"))
                            .bearer_auth(&tokens[name]).json(&json!({
                                "requestId":"disable-memory","command":{"kind":"set_policy","sessionId":null,
                                    "expectedRevision":1,"useMemories":false,"generatePrivate":false}
                            })).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
                                assert!(disabled["result"]["Ok"].is_object(), "{disabled}");
                            }
                            http.post(format!("{control_url}api/v1/approvals/{id}/answer"))
                            .bearer_auth(&tokens[name])
                            .json(&json!({"requestId":format!("approve-{id}"),"answer":"approve"}))
                            .send()
                            .await
                            .unwrap()
                            .error_for_status()
                            .unwrap();
                        }
                    }
                }
            }
        }
        if completed == 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "independent runtime did not complete both Jobs"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(approved.len(), 4);
    assert_eq!(issuer.model_requests.load(Ordering::SeqCst), 14);
    let attempts:i64=query_scalar("SELECT count(DISTINCT worker_id) FROM zuno_enterprise_preview.runtime_attempt WHERE tenant_id=$1")
        .bind(tenant.as_str()).fetch_one(&admin).await.unwrap();
    assert_eq!(attempts, 2, "both independent Workers must participate");
    let operations:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.gateway_operation WHERE tenant_id=$1 AND completion IS NOT NULL")
        .bind(tenant.as_str()).fetch_one(&admin).await.unwrap();
    assert_eq!(
        operations, 4,
        "one admitted execution per logical parent or child command"
    );
    if std::env::var_os("ZUNO_ENTERPRISE_WEB_DIST").is_some() {
        browser::verify(root, &control_url).await;
        assert_eq!(issuer.model_requests.load(Ordering::SeqCst), 16);
        let browser_operations:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.gateway_operation WHERE tenant_id=$1 AND completion IS NOT NULL")
            .bind(tenant.as_str()).fetch_one(&admin).await.unwrap();
        assert_eq!(
            browser_operations, 5,
            "one explicitly approved browser command"
        );
    }
    for child in &mut children {
        assert!(
            tokio::process::Command::new("kill")
                .arg("-TERM")
                .arg(child.id().unwrap().to_string())
                .status()
                .await
                .unwrap()
                .success()
        );
    }
    for child in &mut children {
        assert!(
            tokio::time::timeout(Duration::from_secs(35), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success(),
            "service did not drain successfully on SIGTERM"
        );
    }
    issuer_stopped.fire();
    issuer_task.await.unwrap();
}
