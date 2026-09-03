use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tower::ServiceExt;
use zuno_db::Pool;
use zuno_db::artifact_gc::ArtifactGcPaths;
use zuno_db::event_log::SessionEventLog;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox};
use zuno_db::session::SessionCreate;
use zuno_engine::interrupt::{HardInterruptReason, HardInterruptRequest, HardInterruptSource};
use zuno_engine::r#loop::{TurnEvent, TurnEventSender};
use zuno_engine::status::{AbortDisposition, SessionStatus};
use zuno_llm::event::RequestContentBlock;
use zuno_paths::DbLocation;
use zuno_permission::ReplyKind;
use zuno_pty::{CreateInput, PtyId, TicketScope};
use zuno_server::api::{self, ApiState};
use zuno_server::{
    Delivery, EventService, NewEvent, PermissionRequest, QuestionDecision, QuestionRequest,
    RequestBroker, ServerBuilder, ServerConfig, ServerServices, SessionCompactExecution,
    SessionMutationExecutor, SessionMutationFuture, SessionPromptExecution,
};

fn api_app(state: ApiState) -> Router {
    ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_routes(api::router(state))
        .router()
}

fn api_app_with_services(state: ApiState) -> (Router, ServerServices) {
    let services = ServerServices::new(64);
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(state))
        .router();
    (app, services)
}

fn api_cancel() -> HardInterruptRequest {
    HardInterruptRequest::new(HardInterruptSource::Api, HardInterruptReason::UserCancel)
}

#[cfg(windows)]
fn native_pty_command(script: &str) -> (String, Vec<String>) {
    (
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_owned()),
        vec![
            "/D".to_owned(),
            "/S".to_owned(),
            "/C".to_owned(),
            script.to_owned(),
        ],
    )
}

#[cfg(not(windows))]
fn native_pty_command(script: &str) -> (String, Vec<String>) {
    ("sh".to_owned(), vec!["-c".to_owned(), script.to_owned()])
}

#[derive(Debug, Default)]
struct BlockingMutationExecutor {
    prompt_started: Arc<AtomicBool>,
    prompt_started_notify: Arc<Notify>,
    prompts: Mutex<Vec<SessionPromptExecution>>,
    compact_calls: AtomicUsize,
}

impl BlockingMutationExecutor {
    async fn wait_until_prompt_started(&self) {
        loop {
            let mut notified = std::pin::pin!(self.prompt_started_notify.notified());
            notified.as_mut().enable();
            if self.prompt_started.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn prompts(&self) -> Vec<SessionPromptExecution> {
        self.prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn wait_until_prompt_count(&self, count: usize) {
        loop {
            let mut notified = std::pin::pin!(self.prompt_started_notify.notified());
            notified.as_mut().enable();
            if self.prompts().len() >= count {
                return;
            }
            notified.await;
        }
    }
}

impl SessionMutationExecutor for BlockingMutationExecutor {
    fn prompt(
        &self,
        request: SessionPromptExecution,
        guard: zuno_engine::status::SessionRunGuard,
        _events: TurnEventSender,
    ) -> SessionMutationFuture {
        self.prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
        let started = Arc::clone(&self.prompt_started);
        let started_notify = Arc::clone(&self.prompt_started_notify);
        Box::pin(async move {
            started.store(true, Ordering::SeqCst);
            started_notify.notify_waiters();
            guard.interrupt_signal().notified().await;
            Ok(())
        })
    }

    fn compact(
        &self,
        _request: SessionCompactExecution,
        _guard: zuno_engine::status::SessionRunGuard,
        _events: TurnEventSender,
    ) -> SessionMutationFuture {
        self.compact_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug, Default)]
struct InterruptEventExecutor {
    started: Arc<Notify>,
}

impl SessionMutationExecutor for InterruptEventExecutor {
    fn prompt(
        &self,
        _request: SessionPromptExecution,
        guard: zuno_engine::status::SessionRunGuard,
        events: TurnEventSender,
    ) -> SessionMutationFuture {
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.notify_one();
            guard.interrupt_signal().notified().await;
            events
                .publish(TurnEvent::TurnInterrupted {
                    assistant_message_id: None,
                    steps: 0,
                    request: guard.interrupt_request(),
                })
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn compact(
        &self,
        _request: SessionCompactExecution,
        _guard: zuno_engine::status::SessionRunGuard,
        _events: TurnEventSender,
    ) -> SessionMutationFuture {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug, Default)]
struct BlockingCompactMutationExecutor {
    compact_started: AtomicBool,
    compact_started_notify: Notify,
    compact_release: Arc<Notify>,
}

impl BlockingCompactMutationExecutor {
    async fn wait_until_compact_started(&self) {
        loop {
            let mut notified = std::pin::pin!(self.compact_started_notify.notified());
            notified.as_mut().enable();
            if self.compact_started.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn release_compact(&self) {
        self.compact_release.notify_one();
    }
}

impl SessionMutationExecutor for BlockingCompactMutationExecutor {
    fn prompt(
        &self,
        _request: SessionPromptExecution,
        _guard: zuno_engine::status::SessionRunGuard,
        _events: TurnEventSender,
    ) -> SessionMutationFuture {
        Box::pin(async { Err("prompt is not expected in this test".to_owned()) })
    }

    fn compact(
        &self,
        _request: SessionCompactExecution,
        guard: zuno_engine::status::SessionRunGuard,
        _events: TurnEventSender,
    ) -> SessionMutationFuture {
        self.compact_started.store(true, Ordering::SeqCst);
        self.compact_started_notify.notify_waiters();
        let compact_release = self.compact_release.clone();
        Box::pin(async move {
            compact_release.notified().await;
            drop(guard);
            Ok(())
        })
    }
}

fn request(method: Method, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    let (builder, bytes) = match body {
        Some(value) => (
            builder.header("content-type", "application/json"),
            serde_json::to_vec(&value).expect("test JSON serializes"),
        ),
        None => (builder, Vec::new()),
    };
    builder
        .body(Body::from(bytes))
        .expect("test request is valid")
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("response body is bounded and readable");
    serde_json::from_slice(&bytes).expect("response is JSON")
}

async fn read_http_head(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16 * 1024, "HTTP response headers are bounded");
        let mut byte = [0_u8; 1];
        stream
            .read_exact(&mut byte)
            .await
            .expect("HTTP response head remains readable");
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).expect("HTTP response head is UTF-8")
}

async fn write_masked_text_frame(stream: &mut TcpStream, text: &str) {
    let payload = text.as_bytes();
    assert!(
        payload.len() <= 125,
        "test input fits one small WebSocket frame"
    );
    let mask = [0x12_u8, 0x34, 0x56, 0x78];
    let mut frame = Vec::with_capacity(payload.len() + 6);
    frame.extend_from_slice(&[0x81, 0x80 | payload.len() as u8]);
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    stream
        .write_all(&frame)
        .await
        .expect("WebSocket input frame writes");
}

async fn read_server_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut head = [0_u8; 2];
    stream
        .read_exact(&mut head)
        .await
        .expect("WebSocket frame header reads");
    assert_eq!(head[1] & 0x80, 0, "server frames must not be masked");
    let mut length = u64::from(head[1] & 0x7f);
    if length == 126 {
        let mut extended = [0_u8; 2];
        stream
            .read_exact(&mut extended)
            .await
            .expect("16-bit WebSocket length reads");
        length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0_u8; 8];
        stream
            .read_exact(&mut extended)
            .await
            .expect("64-bit WebSocket length reads");
        length = u64::from_be_bytes(extended);
    }
    assert!(
        length <= 64 * 1024,
        "server frame stays at the protocol cap"
    );
    let mut payload = vec![0_u8; length as usize];
    stream
        .read_exact(&mut payload)
        .await
        .expect("WebSocket frame payload reads");
    (head[0] & 0x0f, payload)
}

struct ReadApiFixture {
    _temp: TempDir,
    state: ApiState,
    events: EventService,
    pool: Arc<Pool>,
}

struct MutationApiFixture {
    _temp: TempDir,
    state: ApiState,
    pool: Arc<Pool>,
}

impl MutationApiFixture {
    fn new(session_id: &str) -> Self {
        let temp = tempfile::tempdir().expect("temporary mutation fixture directory");
        let location = DbLocation::File(temp.path().join("zuno.db"));
        let state_pool = Pool::open(&location).expect("open mutation state pool");
        let pool = Arc::new(Pool::open(&location).expect("open mutation inspection pool"));
        let state = ApiState::from_pool(
            state_pool,
            "/repo",
            ArtifactGcPaths::from_data_root(temp.path()),
        )
        .expect("initialize mutation API state");
        state
            .sessions()
            .create(&SessionCreate::new(
                session_id, session_id, "global", "/repo", "/repo", "mutation", "test",
            ))
            .expect("fixture session inserts");
        Self {
            _temp: temp,
            state,
            pool,
        }
    }
}

impl ReadApiFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary API fixture directory");
        let location = DbLocation::File(temp.path().join("opencode.db"));
        let state_pool = Pool::open(&location).expect("open API fixture pool");
        let pool = Arc::new(Pool::open(&location).expect("open event fixture pool"));
        let events = EventService::new(Arc::clone(&pool), 64);
        let state = ApiState::from_pool(
            state_pool,
            "/repo",
            ArtifactGcPaths::from_data_root(temp.path()),
        )
        .expect("initialize API fixture state")
        .with_events(events.clone());
        Self {
            _temp: temp,
            state,
            events,
            pool,
        }
    }

    fn seed_session_messages(&self, count: i64, compaction_seq: i64) {
        self.state
            .sessions()
            .create(&SessionCreate::new(
                "ses_reads",
                "ses_reads",
                "global",
                "/repo",
                "/repo",
                "reads",
                "test",
            ))
            .expect("fixture session inserts");
        let connection = self.pool.get().expect("fixture database connection");
        for seq in 0..count {
            let kind = if seq == compaction_seq {
                "compaction"
            } else {
                "user"
            };
            let data = if seq == compaction_seq {
                json!({
                    "reason": "auto",
                    "summary": "summary",
                    "recent": "recent",
                    "time": {"created": seq}
                })
            } else {
                json!({
                    "text": format!("message-{seq}"),
                    "files": [],
                    "agents": [],
                    "time": {"created": seq}
                })
            };
            connection
                .execute(
                    "INSERT INTO session_message \
                     (id, session_id, type, seq, time_created, time_updated, data) \
                     VALUES (?1, 'ses_reads', ?2, ?3, ?3, ?3, ?4)",
                    rusqlite::params![format!("msg_{seq:04}"), kind, seq, data.to_string()],
                )
                .expect("fixture projected message inserts");
            let message = zuno_db::message::MessageRecord::from_json(json!({
                "id": format!("msg_{seq:04}"),
                "sessionID": "ses_reads",
                "role": "user",
                "time": {"created": seq},
                "agent": "test",
                "model": {"providerID": "test", "modelID": "test"},
            }))
            .expect("canonical fixture message decodes");
            let part_data = if seq == compaction_seq {
                json!({
                    "id": format!("prt_{seq:04}"),
                    "sessionID": "ses_reads",
                    "messageID": format!("msg_{seq:04}"),
                    "type": "compaction",
                    "reason": "auto",
                    "summary": "summary",
                })
            } else {
                json!({
                    "id": format!("prt_{seq:04}"),
                    "sessionID": "ses_reads",
                    "messageID": format!("msg_{seq:04}"),
                    "type": "text",
                    "text": format!("message-{seq}"),
                })
            };
            let part = zuno_db::message::PartRecord::from_json(part_data, seq)
                .expect("canonical fixture part decodes");
            let store = zuno_db::message::MessageStore::new(&connection);
            store
                .put_message_at(&message, seq)
                .expect("canonical fixture message inserts");
            store
                .put_part_at(&part, seq)
                .expect("canonical fixture part inserts");
        }
    }
}

fn fixture_operations(document: &Value) -> BTreeSet<(String, String)> {
    let mut operations = BTreeSet::new();
    let paths = document["paths"].as_object().expect("fixture paths object");
    for (path, item) in paths {
        if !path.starts_with("/api/") {
            continue;
        }
        let methods = item.as_object().expect("fixture path item");
        for method in methods.keys() {
            if matches!(method.as_str(), "get" | "post" | "put" | "patch" | "delete") {
                operations.insert((path.clone(), method.clone()));
            }
        }
    }
    operations
}

#[test]
fn api_openapi_contains_only_registered_zuno_operations() {
    let generated = api::openapi();
    let actual = fixture_operations(&generated);
    assert_eq!(actual.len(), 51, "the registered Zuno API surface changed");
    for operation in [
        ("/api/integration/{integrationID}/connect/key", "post"),
        ("/api/integration/{integrationID}/connect/oauth", "post"),
        ("/api/integration/attempt/{attemptID}", "get"),
        ("/api/integration/attempt/{attemptID}/complete", "post"),
        ("/api/integration/attempt/{attemptID}", "delete"),
        ("/api/credential/{credentialID}", "patch"),
        ("/api/credential/{credentialID}", "delete"),
        ("/api/session/{sessionID}/permission", "post"),
        ("/api/session/{sessionID}/permission/{requestID}", "get"),
        ("/api/session/{sessionID}/message/{messageID}", "get"),
    ] {
        assert!(
            !actual.contains(&(operation.0.to_owned(), operation.1.to_owned())),
            "unimplemented operation is still advertised: {operation:?}"
        );
    }
}

#[test]
fn api_openapi_presents_zunos_identity() {
    assert_eq!(api::openapi()["info"]["title"], "Zuno API");
}

#[test]
fn api_openapi_binds_every_body_with_an_existing_rust_schema() {
    let document = api::openapi();
    let request_ref = |path: &str, method: &str| {
        document["paths"][path][method]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"]
            .as_str()
    };
    let response_ref = |path: &str, method: &str| {
        document["paths"][path][method]["responses"]["200"]["content"]["application/json"]["schema"]
            ["$ref"]
            .as_str()
    };

    assert_eq!(
        request_ref("/api/session", "post"),
        Some("#/components/schemas/SessionCreate")
    );
    assert_eq!(
        request_ref("/api/session/prune", "post"),
        Some("#/components/schemas/SessionPruneMutation")
    );
    assert_eq!(
        response_ref("/api/session", "get"),
        Some("#/components/schemas/SessionListResponse")
    );
    assert_eq!(
        response_ref("/api/session", "post"),
        Some("#/components/schemas/SessionResponse")
    );
    assert_eq!(
        response_ref("/api/session/active", "get"),
        Some("#/components/schemas/SessionActiveResponse")
    );
    assert_eq!(
        response_ref("/api/session/{sessionID}", "get"),
        Some("#/components/schemas/SessionResponse")
    );
    assert_eq!(
        response_ref("/api/permission/request", "get"),
        Some("#/components/schemas/PermissionRequestListResponse")
    );
    assert_eq!(
        response_ref("/api/session/{sessionID}/permission", "get"),
        Some("#/components/schemas/SessionPermissionResponse")
    );
    assert_eq!(
        request_ref(
            "/api/session/{sessionID}/permission/{requestID}/reply",
            "post"
        ),
        Some("#/components/schemas/PermissionReply")
    );
    assert_eq!(
        response_ref("/api/question/request", "get"),
        Some("#/components/schemas/QuestionRequestListResponse")
    );
    assert_eq!(
        response_ref("/api/session/{sessionID}/question", "get"),
        Some("#/components/schemas/SessionQuestionResponse")
    );
    assert_eq!(
        request_ref(
            "/api/session/{sessionID}/question/{requestID}/reply",
            "post"
        ),
        Some("#/components/schemas/QuestionReply")
    );
}

#[tokio::test]
async fn api_doc_aliases_serve_the_generated_document() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let app = api_app(state);
    for path in ["/doc", "/openapi.json", "/api/doc"] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path, None))
            .await
            .expect("document route responds");
        assert_eq!(response.status(), StatusCode::OK, "alias {path}");
        let document = response_json(response).await;
        assert_eq!(document["openapi"], "3.1.0");
    }
}

#[tokio::test]
async fn api_session_response_validates_against_its_published_openapi_binding() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    state
        .sessions()
        .create(&SessionCreate::new(
            "ses_schema",
            "schema-slug",
            "global",
            "/repo",
            "/repo",
            "schema validation probe",
            "test",
        ))
        .expect("schema fixture session inserts");
    let app = api_app(state);

    let document = app
        .clone()
        .oneshot(request(Method::GET, "/doc", None))
        .await
        .expect("the build serves its own OpenAPI document");
    assert_eq!(document.status(), StatusCode::OK);
    let document = response_json(document).await;
    let schema_ref = document["paths"]["/api/session/{sessionID}"]["get"]["responses"]["200"]
        ["content"]["application/json"]["schema"]["$ref"]
        .as_str()
        .expect("GET /api/session/{sessionID} publishes a JSON response schema reference");
    let schema_name = schema_ref
        .strip_prefix("#/components/schemas/")
        .expect("the response schema reference resolves inside the published document");
    let schema = &document["components"]["schemas"][schema_name];
    assert!(
        schema.is_object(),
        "the bound response component `{schema_name}` exists"
    );
    let validator = jsonschema::validator_for(schema)
        .unwrap_or_else(|error| panic!("published `{schema_name}` schema compiles: {error}"));

    let response = app
        .oneshot(request(Method::GET, "/api/session/ses_schema", None))
        .await
        .expect("bound session route responds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    let errors = validator
        .iter_errors(&body)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "GET /api/session/ses_schema returned a body rejected by its own published schema: {errors:?}; body={body}"
    );
}

#[tokio::test]
async fn api_session_learning_returns_the_shared_durable_projection_shape() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    state
        .sessions()
        .create(&SessionCreate::new(
            "ses_learning",
            "learning-slug",
            "global",
            "/repo",
            "/repo",
            "learning projection",
            "test",
        ))
        .expect("learning fixture session inserts");

    let response = api_app(state)
        .oneshot(request(
            Method::GET,
            "/api/session/ses_learning/learning",
            None,
        ))
        .await
        .expect("learning projection route responds");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({
            "data": {
                "feedback": [],
                "experiences": [],
                "patterns": [],
                "skillCandidates": [],
            }
        })
    );
}

#[tokio::test]
async fn api_session_list_rejects_directory_and_project_together() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let response = api_app(state)
        .oneshot(request(
            Method::GET,
            "/api/session?directory=%2Frepo&project=global",
            None,
        ))
        .await
        .expect("session route responds");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn api_session_list_applies_subpath_as_a_literal_tree_prefix() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    for (id, directory) in [
        ("ses_pkg", "/repo/pkg"),
        ("ses_child", "/repo/pkg/child"),
        ("ses_neighbour", "/repo/pkgx"),
    ] {
        state
            .sessions()
            .create(&SessionCreate::new(
                id, id, "global", "/repo", directory, id, "test",
            ))
            .expect("fixture session inserts");
    }

    let response = api_app(state)
        .oneshot(request(
            Method::GET,
            "/api/session?project=global&subpath=pkg",
            None,
        ))
        .await
        .expect("session route responds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    let ids = body["data"]
        .as_array()
        .expect("session data array")
        .iter()
        .map(|session| session["id"].as_str().expect("session id"))
        .collect::<BTreeSet<_>>();
    assert_eq!(ids, BTreeSet::from(["ses_child", "ses_pkg"]));
}

#[tokio::test]
async fn api_session_list_defaults_to_updated_then_id_desc_and_created_opts_out() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    for (id, created, updated) in [
        ("ses_a", 10_i64, 100_i64),
        ("ses_b", 20_i64, 50_i64),
        ("ses_c", 5_i64, 100_i64),
    ] {
        state
            .sessions()
            .create(&SessionCreate::new(id, id, "global", "/repo", "/repo", id, "test").at(created))
            .expect("fixture session inserts");
        state
            .sessions()
            .touch_at(id, updated)
            .expect("fixture updated time changes");
    }
    let app = api_app(state);

    let default_response = app
        .clone()
        .oneshot(request(Method::GET, "/api/session?project=global", None))
        .await
        .expect("default list responds");
    let default_body = response_json(default_response).await;
    let default_ids = default_body["data"]
        .as_array()
        .expect("session data array")
        .iter()
        .map(|session| session["id"].as_str().expect("session id"))
        .collect::<Vec<_>>();
    assert_eq!(default_ids, ["ses_c", "ses_a", "ses_b"]);

    let created_response = app
        .oneshot(request(
            Method::GET,
            "/api/session?project=global&sort=created",
            None,
        ))
        .await
        .expect("created list responds");
    let created_body = response_json(created_response).await;
    let created_ids = created_body["data"]
        .as_array()
        .expect("session data array")
        .iter()
        .map(|session| session["id"].as_str().expect("session id"))
        .collect::<Vec<_>>();
    assert_eq!(created_ids, ["ses_b", "ses_a", "ses_c"]);
}

#[tokio::test]
async fn api_session_active_reports_only_process_local_running_sessions_in_stable_order() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let (app, services) = api_app_with_services(state);
    let _z_guard = services.runs.begin_turn("ses_z").expect("start z turn");
    let _a_guard = services.runs.begin_turn("ses_a").expect("start a turn");

    let response = app
        .oneshot(request(Method::GET, "/api/session/active", None))
        .await
        .expect("active-session route responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({
            "data": {
                "ses_a": {"type": "running"},
                "ses_z": {"type": "running"}
            }
        })
    );
}

#[tokio::test]
async fn api_session_message_and_context_are_projected_and_bounded_by_their_contracts() {
    let fixture = ReadApiFixture::new();
    fixture.seed_session_messages(120, 70);
    let app = api_app(fixture.state);

    let messages = app
        .clone()
        .oneshot(request(Method::GET, "/api/session/ses_reads/message", None))
        .await
        .expect("message list responds");
    assert_eq!(messages.status(), StatusCode::OK);
    let messages = response_json(messages).await;
    assert_eq!(messages["data"].as_array().expect("message data").len(), 50);
    assert_eq!(messages["data"][0]["info"]["id"], "msg_0119");
    assert_eq!(messages["data"][0]["parts"][0]["text"], "message-119");
    assert!(messages["cursor"]["previous"].is_string());
    assert!(messages["cursor"]["next"].is_string());

    let context = app
        .oneshot(request(Method::GET, "/api/session/ses_reads/context", None))
        .await
        .expect("context responds");
    assert_eq!(context.status(), StatusCode::OK);
    let context = response_json(context).await;
    assert_eq!(context["data"].as_array().expect("context data").len(), 50);
    assert_eq!(context["data"][0]["id"], "msg_0070");
    assert_eq!(context["data"][0]["type"], "compaction");
}

#[tokio::test]
async fn api_session_history_is_a_finite_default_page_with_an_exclusive_cursor() {
    let fixture = ReadApiFixture::new();
    fixture.seed_session_messages(1, 0);
    for ordinal in 0..75 {
        fixture
            .events
            .publish(
                "ses_reads",
                NewEvent::new(
                    "test.event",
                    serde_json::Map::from_iter([("ordinal".to_owned(), json!(ordinal))]),
                )
                .expect("fixture event"),
            )
            .await
            .expect("fixture event publishes");
    }
    let app = api_app(fixture.state);

    let first = app
        .clone()
        .oneshot(request(Method::GET, "/api/session/ses_reads/history", None))
        .await
        .expect("history responds");
    assert_eq!(first.status(), StatusCode::OK);
    let first = response_json(first).await;
    assert_eq!(first["data"].as_array().expect("history data").len(), 50);
    assert_eq!(first["data"][0]["durable"]["seq"], 0);
    assert_eq!(first["data"][49]["durable"]["seq"], 49);
    assert_eq!(first["hasMore"], true);

    let second = app
        .oneshot(request(
            Method::GET,
            "/api/session/ses_reads/history?after=49&limit=100",
            None,
        ))
        .await
        .expect("next history page responds");
    assert_eq!(second.status(), StatusCode::OK);
    let second = response_json(second).await;
    assert_eq!(second["data"].as_array().expect("history data").len(), 25);
    assert_eq!(second["data"][0]["durable"]["seq"], 50);
    assert_eq!(second["hasMore"], false);
}

#[tokio::test]
async fn api_empty_session_message_and_history_stay_empty() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    state
        .sessions()
        .create(&SessionCreate::new(
            "ses_empty",
            "ses_empty",
            "global",
            "/repo",
            "/repo",
            "empty",
            "test",
        ))
        .expect("fixture session inserts");
    let app = api_app(state);

    let messages = app
        .clone()
        .oneshot(request(Method::GET, "/api/session/ses_empty/message", None))
        .await
        .expect("empty message read responds");
    assert_eq!(messages.status(), StatusCode::OK);
    assert_eq!(response_json(messages).await["data"], json!([]));

    let history = app
        .oneshot(request(Method::GET, "/api/session/ses_empty/history", None))
        .await
        .expect("empty history read responds");
    assert_eq!(history.status(), StatusCode::OK);
    assert_eq!(response_json(history).await["data"], json!([]));
}

#[tokio::test]
async fn api_session_read_routes_return_not_found_for_an_unknown_session() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let app = api_app(state);
    for path in [
        "/api/session/ses_missing/context",
        "/api/session/ses_missing/history",
        "/api/session/ses_missing/message",
        "/api/session/ses_missing/question",
    ] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path, None))
            .await
            .expect("session read responds");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(response_json(response).await["error"]["code"], "not_found");
    }
}

#[tokio::test]
async fn api_permission_and_question_read_routes_match_the_empty_process_state() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    state
        .sessions()
        .create(&SessionCreate::new(
            "ses_empty",
            "ses_empty",
            "global",
            "/repo",
            "/repo",
            "empty",
            "test",
        ))
        .expect("fixture session inserts");
    let app = api_app(state);

    for path in [
        "/api/permission/request",
        "/api/question/request",
        "/api/permission/saved",
        "/api/session/ses_empty/question",
    ] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path, None))
            .await
            .expect("read route responds");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response_json(response).await["data"], json!([]), "{path}");
    }

    let removed = app
        .oneshot(request(
            Method::DELETE,
            "/api/permission/saved/per_missing",
            None,
        ))
        .await
        .expect("saved permission delete responds");
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn api_reply_routes_validate_bodies_before_rejecting_cross_session_requests() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let requests = RequestBroker::default();
    let services = ServerServices::new(64).with_requests(requests.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services)
        .with_routes(api::router(state))
        .router();
    let mut answer = tokio::spawn({
        let requests = requests.clone();
        async move {
            requests
                .ask_permission(PermissionRequest {
                    id: "per_owner".to_owned(),
                    session_id: "ses_owner".to_owned(),
                    action: "shell".to_owned(),
                    resources: vec!["pwd".to_owned()],
                    save: Vec::new(),
                    metadata: serde_json::Map::new(),
                    source: None,
                })
                .await
        }
    });
    while requests.permissions(None).is_empty() {
        tokio::task::yield_now().await;
    }

    let malformed_permission = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_other/permission/per_owner/reply",
            Some(json!("malformed for PermissionReplyBody")),
        ))
        .await
        .expect("malformed cross-session permission reply responds");
    assert_eq!(malformed_permission.status(), StatusCode::BAD_REQUEST);
    assert_eq!(requests.permissions(None).len(), 1);

    let cross_session_permission = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_other/permission/per_owner/reply",
            Some(json!({"reply": "once"})),
        ))
        .await
        .expect("valid cross-session permission reply responds");
    assert_eq!(cross_session_permission.status(), StatusCode::NOT_FOUND);
    assert_eq!(requests.permissions(None).len(), 1);

    let owner_permission = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_owner/permission/per_owner/reply",
            Some(json!({"reply": "once"})),
        ))
        .await
        .expect("owner reply responds");
    assert_eq!(owner_permission.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), &mut answer)
            .await
            .expect("permission asker resumes")
            .expect("permission asker task does not panic"),
        ReplyKind::Once
    );

    let mut question_answer = tokio::spawn({
        let requests = requests.clone();
        async move {
            requests
                .ask_question(QuestionRequest {
                    id: "que_owner".to_owned(),
                    session_id: "ses_owner".to_owned(),
                    questions: vec![json!({"question": "Continue?"})],
                    tool: None,
                })
                .await
        }
    });
    while requests.questions(None).is_empty() {
        tokio::task::yield_now().await;
    }

    let malformed_question = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_other/question/que_owner/reply",
            Some(json!("malformed for QuestionReplyBody")),
        ))
        .await
        .expect("malformed cross-session question reply responds");
    assert_eq!(malformed_question.status(), StatusCode::BAD_REQUEST);
    assert_eq!(requests.questions(None).len(), 1);

    let cross_session_question = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_other/question/que_owner/reply",
            Some(json!({"answers": [["yes"]]})),
        ))
        .await
        .expect("valid cross-session question reply responds");
    assert_eq!(cross_session_question.status(), StatusCode::NOT_FOUND);
    assert_eq!(requests.questions(None).len(), 1);

    let owner_question = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_owner/question/que_owner/reply",
            Some(json!({"answers": [["yes"]]})),
        ))
        .await
        .expect("owner question reply responds");
    assert_eq!(owner_question.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), &mut question_answer)
            .await
            .expect("question asker resumes")
            .expect("question asker task does not panic"),
        QuestionDecision::Answered(vec![vec!["yes".to_owned()]])
    );

    for (path, body) in [
        (
            "/api/session/ses_owner/permission/request_matrix/reply",
            Some(json!({"reply": "once"})),
        ),
        (
            "/api/session/ses_owner/question/request_matrix/reply",
            Some(json!({"answers": [["yes"]]})),
        ),
        (
            "/api/session/ses_owner/question/request_matrix/reject",
            None,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request(Method::POST, path, body))
            .await
            .expect("invalid request ID responds");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
    }

    for (path, body) in [
        (
            "/api/session/ses_owner/permission/per_missing/reply",
            Some(json!({"reply": "once"})),
        ),
        (
            "/api/session/ses_owner/question/que_missing/reply",
            Some(json!({"answers": [["yes"]]})),
        ),
        ("/api/session/ses_owner/question/que_missing/reject", None),
    ] {
        let response = app
            .clone()
            .oneshot(request(Method::POST, path, body))
            .await
            .expect("missing request responds");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn malformed_owned_question_reply_rejects_and_removes_the_request() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let requests = RequestBroker::default();
    let services = ServerServices::new(64).with_requests(requests.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services)
        .with_routes(api::router(state))
        .router();
    let mut answer = tokio::spawn({
        let requests = requests.clone();
        async move {
            requests
                .ask_question(QuestionRequest {
                    id: "que_malformed_owned".to_owned(),
                    session_id: "ses_owner".to_owned(),
                    questions: vec![json!({"question": "Continue?"})],
                    tool: None,
                })
                .await
        }
    });
    while requests.questions(None).is_empty() {
        tokio::task::yield_now().await;
    }

    let malformed = app
        .oneshot(request(
            Method::POST,
            "/api/session/ses_owner/question/que_malformed_owned/reply",
            Some(json!("malformed for QuestionReplyBody")),
        ))
        .await
        .expect("malformed owned question reply responds");

    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), &mut answer)
            .await
            .expect("owned malformed cleanup must release the question asker")
            .expect("question asker task does not panic"),
        QuestionDecision::Failed
    );
    assert!(
        requests.questions(None).is_empty(),
        "owned malformed cleanup must remove the rejected question"
    );
}

#[tokio::test(start_paused = true)]
async fn permission_without_an_observer_is_rejected_by_the_deadline() {
    let requests = RequestBroker::default();
    let mut answer = tokio::spawn({
        let requests = requests.clone();
        async move {
            requests
                .ask_permission(PermissionRequest {
                    id: "per_unobserved".to_owned(),
                    session_id: "ses_unobserved".to_owned(),
                    action: "shell".to_owned(),
                    resources: vec!["touch must-not-run".to_owned()],
                    save: Vec::new(),
                    metadata: serde_json::Map::new(),
                    source: None,
                })
                .await
        }
    });
    while requests.permissions(None).is_empty() {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
    tokio::task::yield_now().await;

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), &mut answer)
            .await
            .expect("an unobserved permission request must have a finite deadline")
            .expect("permission asker task does not panic"),
        ReplyKind::Reject,
        "the deadline must fail closed rather than authorize the request"
    );
    assert!(
        requests.permissions(None).is_empty(),
        "the deadline must remove the stale permission request"
    );
}

#[tokio::test(start_paused = true)]
async fn question_without_an_observer_is_rejected_by_the_deadline() {
    let requests = RequestBroker::default();
    let mut answer = tokio::spawn({
        let requests = requests.clone();
        async move {
            requests
                .ask_question(QuestionRequest {
                    id: "que_unobserved".to_owned(),
                    session_id: "ses_unobserved".to_owned(),
                    questions: vec![json!({"question": "Continue?"})],
                    tool: None,
                })
                .await
        }
    });
    while requests.questions(None).is_empty() {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
    tokio::task::yield_now().await;

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), &mut answer)
            .await
            .expect("an unobserved question request must have a finite deadline")
            .expect("question asker task does not panic"),
        QuestionDecision::Expired,
        "the deadline must fail closed rather than invent an answer"
    );
    assert!(
        requests.questions(None).is_empty(),
        "the deadline must remove the stale question request"
    );
}

#[tokio::test]
async fn api_prompt_wait_and_interrupt_share_one_live_turn_signal() {
    let fixture = MutationApiFixture::new("ses_mutation");
    let executor = Arc::new(BlockingMutationExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(fixture.state))
        .router();

    let response = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_mutation/prompt",
            Some(json!({
                "id": "msg_http",
                "prompt": {"text": "hello", "files": [], "agents": []},
                "delivery": "steer"
            })),
        ))
        .await
        .expect("prompt responds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["id"], "msg_http");
    assert_eq!(body["data"]["sessionID"], "ses_mutation");
    assert_eq!(body["data"]["admittedSeq"], 0);
    assert_eq!(body["data"]["prompt"]["files"], json!([]));
    assert_eq!(body["data"]["prompt"]["agents"], json!([]));
    executor.wait_until_prompt_started().await;
    assert_eq!(services.runs.status("ses_mutation"), SessionStatus::Busy);

    let mut wait_task = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(request(
                Method::POST,
                "/api/session/ses_mutation/wait",
                None,
            ))
            .await
            .expect("wait responds")
        }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut wait_task)
            .await
            .is_err(),
        "wait must remain suspended while the turn is active"
    );

    let conflict = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_mutation/agent",
            Some(json!({"agent": "explorer"})),
        ))
        .await
        .expect("busy mutation responds");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    let interrupted = app
        .oneshot(request(
            Method::POST,
            "/api/session/ses_mutation/interrupt",
            None,
        ))
        .await
        .expect("interrupt responds");
    assert_eq!(interrupted.status(), StatusCode::NO_CONTENT);
    let waited = tokio::time::timeout(Duration::from_secs(1), &mut wait_task)
        .await
        .expect("wait wakes after interrupt")
        .expect("wait task does not panic");
    assert_eq!(waited.status(), StatusCode::NO_CONTENT);
    assert_eq!(services.runs.status("ses_mutation"), SessionStatus::Idle);
    assert_eq!(
        executor.prompts(),
        vec![SessionPromptExecution {
            session_id: "ses_mutation".to_owned(),
            directory: "/repo".into(),
            message_id: "msg_http".to_owned(),
            prompt: "hello".to_owned(),
            content: Vec::new(),
            agent: None,
            model: None,
        }]
    );
}

#[tokio::test]
async fn api_image_prompt_persists_only_an_attachment_reference() {
    let fixture = MutationApiFixture::new("ses_image");
    let executor = Arc::new(BlockingMutationExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(fixture.state))
        .router();
    let source = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DAAAAEAQEARwbK3gAAAABJRU5ErkJggg==";

    let response = app
        .oneshot(request(
            Method::POST,
            "/api/session/ses_image/prompt",
            Some(json!({
                "id": "msg_image",
                "prompt": {
                    "text": "inspect the image",
                    "files": [{
                        "filename": "pixel.png",
                        "mimeType": "image/png",
                        "data": source
                    }],
                    "agents": []
                },
                "delivery": "steer"
            })),
        ))
        .await
        .expect("image prompt responds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["prompt"]["files"][0]["type"], "image");
    assert!(
        body["data"]["prompt"]["files"][0].get("data").is_none(),
        "the HTTP response must not echo durable base64"
    );
    let reference = serde_json::from_value::<zuno_attachment::ImageAttachmentRef>(
        body["data"]["prompt"]["files"][0]["attachment"].clone(),
    )
    .expect("response contains a typed attachment reference");

    executor.wait_until_prompt_started().await;
    let requests = executor.prompts();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].content,
        vec![
            RequestContentBlock::Text {
                text: "inspect the image".to_owned(),
            },
            RequestContentBlock::ImageAttachment {
                reference: reference.clone(),
            },
        ]
    );

    let stored = SessionInbox::new(Arc::clone(&fixture.pool))
        .promote_id("ses_image", "msg_image")
        .expect("durable image input can be inspected");
    assert!(
        stored.is_none(),
        "the active driver already promoted the admitted image"
    );
    let connection = fixture.pool.get().expect("inspection connection");
    let persisted: String = connection
        .query_row(
            "SELECT prompt FROM session_input WHERE session_id = ?1 AND id = ?2",
            rusqlite::params!["ses_image", "msg_image"],
            |row| row.get(0),
        )
        .expect("persisted prompt reads");
    assert!(persisted.contains(&reference.id.to_string()));
    assert!(!persisted.contains(source));
    assert!(!persisted.contains("\"data\""));

    assert_eq!(
        services.runs.abort("ses_image", api_cancel()),
        AbortDisposition::Active
    );
}

#[tokio::test]
async fn api_interrupt_source_reaches_the_durable_turn_event() {
    let fixture = MutationApiFixture::new("ses_interrupt_event");
    let executor = Arc::new(InterruptEventExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let state = fixture
        .state
        .clone()
        .with_events(EventService::new(Arc::clone(&fixture.pool), 64));
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(state))
        .router();

    let response = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_interrupt_event/prompt",
            Some(json!({
                "id": "msg_interrupt_event",
                "prompt": {"text": "wait for cancellation", "files": [], "agents": []}
            })),
        ))
        .await
        .expect("prompt responds");
    assert_eq!(response.status(), StatusCode::OK);
    executor.started.notified().await;

    let interrupted = app
        .oneshot(request(
            Method::POST,
            "/api/session/ses_interrupt_event/interrupt",
            None,
        ))
        .await
        .expect("interrupt responds");
    assert_eq!(interrupted.status(), StatusCode::NO_CONTENT);
    services.runs.wait_until_idle("ses_interrupt_event").await;

    let event_log = SessionEventLog::new(fixture.pool);
    let properties = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let events = event_log
                .read_after("ses_interrupt_event", None)
                .expect("durable session events");
            if let Some(interruption) = events
                .iter()
                .find(|event| event.event_type == "turn.interrupted")
            {
                return interruption.properties.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("typed turn interruption becomes durable");
    assert_eq!(properties["source"], "api");
    assert_eq!(properties["reason"], "user_cancel");
}

#[tokio::test]
async fn api_prompt_defaults_to_queue_and_resume_false_does_not_start_a_turn() {
    let fixture = MutationApiFixture::new("ses_deferred");
    let executor = Arc::new(BlockingMutationExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(fixture.state))
        .router();

    let response = app
        .oneshot(request(
            Method::POST,
            "/api/session/ses_deferred/prompt",
            Some(json!({
                "id": "msg_deferred",
                "prompt": {"text": "later", "files": [], "agents": []},
                "resume": false
            })),
        ))
        .await
        .expect("deferred prompt responds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["admittedSeq"], 0);
    assert_eq!(body["data"]["delivery"], "queue");
    assert_eq!(services.runs.status("ses_deferred"), SessionStatus::Idle);
    assert!(executor.prompts().is_empty());
    let pending = SessionInbox::new(Arc::clone(&fixture.pool))
        .pending("ses_deferred")
        .expect("pending input reads");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "msg_deferred");
    let events = SessionEventLog::new(fixture.pool)
        .read_after("ses_deferred", None)
        .expect("admission event reads");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "session.input.admitted");
}

#[tokio::test]
async fn api_busy_steer_is_durable_and_runs_after_the_active_prompt() {
    let fixture = MutationApiFixture::new("ses_queued");
    let executor = Arc::new(BlockingMutationExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(fixture.state))
        .router();

    let first = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_queued/prompt",
            Some(json!({
                "id": "msg_first",
                "prompt": {"text": "first", "files": [], "agents": []},
                "delivery": "steer"
            })),
        ))
        .await
        .expect("first prompt responds");
    assert_eq!(first.status(), StatusCode::OK);
    executor.wait_until_prompt_count(1).await;

    let second = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_queued/prompt",
            Some(json!({
                "id": "msg_second",
                "prompt": {"text": "second", "files": [], "agents": []},
                "delivery": "steer"
            })),
        ))
        .await
        .expect("busy steer responds");
    assert_eq!(second.status(), StatusCode::OK);
    let body = response_json(second).await;
    assert_eq!(body["data"]["admittedSeq"], 2);
    assert_eq!(
        SessionInbox::new(Arc::clone(&fixture.pool))
            .pending("ses_queued")
            .expect("queued input reads")
            .len(),
        1
    );

    assert_eq!(
        services.runs.abort("ses_queued", api_cancel()),
        AbortDisposition::Active
    );
    executor.wait_until_prompt_count(2).await;
    assert_eq!(
        executor
            .prompts()
            .into_iter()
            .map(|request| request.message_id)
            .collect::<Vec<_>>(),
        ["msg_first", "msg_second"]
    );
    assert!(
        SessionInbox::new(fixture.pool)
            .pending("ses_queued")
            .expect("drained inbox reads")
            .is_empty()
    );
    assert_eq!(
        services.runs.abort("ses_queued", api_cancel()),
        AbortDisposition::Active
    );
    services.runs.wait_until_idle("ses_queued").await;
}

#[tokio::test]
async fn api_prompt_driver_runs_a_durable_subagent_report_before_later_user_input() {
    let fixture = MutationApiFixture::new("ses_report");
    SessionInbox::new(Arc::clone(&fixture.pool))
        .admit(NewSessionInput::new(
            "input_report",
            "ses_report",
            json!({
                "kind": "subagentReport",
                "jobID": "job_1",
                "childSessionID": "ses_child",
                "status": "completed",
                "text": "background result"
            }),
            InputDelivery::Queue,
            1,
        ))
        .expect("admit background report");
    let executor = Arc::new(BlockingMutationExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(fixture.state))
        .router();

    let response = app
        .oneshot(request(
            Method::POST,
            "/api/session/ses_report/prompt",
            Some(json!({
                "id": "msg_user",
                "prompt": {"text": "user input", "files": [], "agents": []},
                "delivery": "queue"
            })),
        ))
        .await
        .expect("user prompt responds");
    assert_eq!(response.status(), StatusCode::OK);

    executor.wait_until_prompt_count(1).await;
    assert_eq!(executor.prompts()[0].message_id, "input_report");
    assert_eq!(executor.prompts()[0].prompt, "background result");
    assert_eq!(
        services.runs.abort("ses_report", api_cancel()),
        AbortDisposition::Active
    );
    executor.wait_until_prompt_count(2).await;
    assert_eq!(
        executor
            .prompts()
            .into_iter()
            .map(|request| (request.message_id, request.prompt))
            .collect::<Vec<_>>(),
        [
            ("input_report".to_owned(), "background result".to_owned()),
            ("msg_user".to_owned(), "user input".to_owned())
        ]
    );
    assert_eq!(
        services.runs.abort("ses_report", api_cancel()),
        AbortDisposition::Active
    );
    services.runs.wait_until_idle("ses_report").await;
}

#[tokio::test]
async fn api_prompt_driver_skips_a_malformed_durable_input_without_stranding_the_queue() {
    let fixture = MutationApiFixture::new("ses_malformed");
    SessionInbox::new(Arc::clone(&fixture.pool))
        .admit(NewSessionInput::new(
            "input_malformed",
            "ses_malformed",
            json!({"kind": "unknown"}),
            InputDelivery::Queue,
            1,
        ))
        .expect("admit malformed input");
    let executor = Arc::new(BlockingMutationExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(fixture.state))
        .router();

    let response = app
        .oneshot(request(
            Method::POST,
            "/api/session/ses_malformed/prompt",
            Some(json!({
                "id": "msg_user",
                "prompt": {"text": "user input", "files": [], "agents": []},
                "delivery": "queue"
            })),
        ))
        .await
        .expect("user prompt responds");
    assert_eq!(response.status(), StatusCode::OK);

    executor.wait_until_prompt_count(1).await;
    assert_eq!(executor.prompts()[0].message_id, "msg_user");
    assert_eq!(executor.prompts()[0].prompt, "user input");
    assert_eq!(
        services.runs.abort("ses_malformed", api_cancel()),
        AbortDisposition::Active
    );
    services.runs.wait_until_idle("ses_malformed").await;
}

#[tokio::test]
async fn api_compact_holds_the_admission_guard_until_the_executor_returns() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    state
        .sessions()
        .create(&SessionCreate::new(
            "ses_compact_guard",
            "ses_compact_guard",
            "global",
            "/repo",
            "/repo",
            "compact guard",
            "test",
        ))
        .expect("fixture session inserts");
    let executor = Arc::new(BlockingCompactMutationExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(state))
        .router();

    let mut compact_task = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(request(
                Method::POST,
                "/api/session/ses_compact_guard/compact",
                None,
            ))
            .await
            .expect("compact responds")
        }
    });
    executor.wait_until_compact_started().await;
    assert_eq!(
        services.runs.status("ses_compact_guard"),
        SessionStatus::Busy,
        "the HTTP admission guard must remain live inside the compact executor"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut compact_task)
            .await
            .is_err(),
        "the HTTP request must wait for compact and its post-compact handoff"
    );

    executor.release_compact();
    let response = tokio::time::timeout(Duration::from_secs(1), &mut compact_task)
        .await
        .expect("compact finishes after release")
        .expect("compact task does not panic");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        services.runs.status("ses_compact_guard"),
        SessionStatus::Idle,
        "the guard must be released when the executor and idle handoff finish"
    );
}

#[tokio::test]
async fn api_agent_model_compact_and_revert_mutations_are_guarded_and_persisted() {
    let fixture = ReadApiFixture::new();
    fixture.seed_session_messages(3, -1);
    fixture
        .events
        .publish(
            "ses_reads",
            NewEvent::new(
                "session.created",
                serde_json::Map::from_iter([("sessionID".to_owned(), json!("ses_reads"))]),
            )
            .expect("created event is valid"),
        )
        .await
        .expect("created event inserts");
    let executor = Arc::new(BlockingMutationExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services)
        .with_routes(api::router(fixture.state.clone()))
        .router();

    for (path, body) in [
        ("/api/session/ses_reads/agent", json!({"agent": "explorer"})),
        (
            "/api/session/ses_reads/model",
            json!({"model": {"providerID": "provider", "id": "model", "variant": "fast"}}),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request(Method::POST, path, Some(body)))
            .await
            .expect("session switch responds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
    }
    let session = fixture
        .state
        .sessions()
        .get("ses_reads")
        .expect("session reads");
    assert_eq!(session.agent.as_deref(), Some("explorer"));
    assert_eq!(
        session.model.as_deref(),
        Some(r#"{"id":"model","providerID":"provider","variant":"fast"}"#)
    );

    let context = app
        .clone()
        .oneshot(request(Method::GET, "/api/session/ses_reads/context", None))
        .await
        .expect("context responds after session switches");
    assert_eq!(context.status(), StatusCode::OK);
    let context = response_json(context).await;
    let switched = context["data"]
        .as_array()
        .expect("context data is an array")
        .iter()
        .filter(|message| {
            matches!(
                message["type"].as_str(),
                Some("agent-switched" | "model-switched")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        switched.len(),
        2,
        "both successful switch operations must append their projected messages: {context}"
    );
    assert_eq!(switched[0]["type"], "agent-switched");
    assert_eq!(switched[0]["agent"], "explorer");
    assert!(
        switched[0]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("msg_")),
        "agent switch uses a native Zuno message ID: {}",
        switched[0]
    );
    assert!(switched[0]["time"]["created"].is_i64());
    assert_eq!(switched[1]["type"], "model-switched");
    assert_eq!(
        switched[1]["model"],
        json!({"providerID": "provider", "id": "model", "variant": "fast"})
    );
    assert!(
        switched[1]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("msg_")),
        "model switch uses a native Zuno message ID: {}",
        switched[1]
    );
    assert!(switched[1]["time"]["created"].is_i64());

    let messages = app
        .clone()
        .oneshot(request(Method::GET, "/api/session/ses_reads/message", None))
        .await
        .expect("messages respond after session switches");
    assert_eq!(messages.status(), StatusCode::OK);
    let messages = response_json(messages).await;
    let message_data = messages["data"]
        .as_array()
        .expect("message data is an array");
    assert!(
        message_data.iter().any(|message| {
            message["type"] == "agent-switched" && message["agent"] == "explorer"
        }),
        "agent control message remains visible beside canonical messages: {messages}"
    );
    assert!(
        message_data.iter().any(|message| {
            message["type"] == "model-switched"
                && message["model"]
                    == json!({"providerID": "provider", "id": "model", "variant": "fast"})
        }),
        "model control message remains visible beside canonical messages: {messages}"
    );
    assert!(
        message_data
            .iter()
            .any(|message| message["info"]["id"] == "msg_0002"),
        "canonical messages remain visible beside control messages: {messages}"
    );

    let history = app
        .clone()
        .oneshot(request(Method::GET, "/api/session/ses_reads/history", None))
        .await
        .expect("history responds after session switches");
    assert_eq!(history.status(), StatusCode::OK);
    let history = response_json(history).await;
    let events = history["data"]
        .as_array()
        .expect("history data is an array");
    assert_eq!(events.len(), 2, "both switch events are durable: {history}");
    assert_eq!(events[0]["type"], "session.next.agent.switched");
    assert_eq!(events[0]["data"]["sessionID"], "ses_reads");
    assert_eq!(events[0]["data"]["agent"], "explorer");
    assert!(events[0]["data"]["timestamp"].is_i64());
    assert!(
        events[0]["data"]["messageID"]
            .as_str()
            .is_some_and(|id| id.starts_with("msg_"))
    );
    assert_eq!(events[1]["type"], "session.next.model.switched");
    assert_eq!(events[1]["data"]["sessionID"], "ses_reads");
    assert_eq!(
        events[1]["data"]["model"],
        json!({"providerID": "provider", "id": "model", "variant": "fast"})
    );
    assert!(events[1]["data"]["timestamp"].is_i64());
    assert!(
        events[1]["data"]["messageID"]
            .as_str()
            .is_some_and(|id| id.starts_with("msg_"))
    );

    let compact = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_reads/compact",
            None,
        ))
        .await
        .expect("compact responds");
    assert_eq!(compact.status(), StatusCode::NO_CONTENT);
    assert_eq!(executor.compact_calls.load(Ordering::SeqCst), 1);

    let unstaged = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_reads/revert/commit",
            None,
        ))
        .await
        .expect("unstaged commit responds");
    assert_eq!(unstaged.status(), StatusCode::NO_CONTENT);

    let staged = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_reads/revert/stage",
            Some(json!({"messageID": "msg_0001", "files": false})),
        ))
        .await
        .expect("revert stage responds");
    assert_eq!(staged.status(), StatusCode::OK);
    assert_eq!(response_json(staged).await["data"]["messageID"], "msg_0001");
    let cleared = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_reads/revert/clear",
            None,
        ))
        .await
        .expect("revert clear responds");
    assert_eq!(cleared.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        fixture
            .state
            .sessions()
            .get("ses_reads")
            .expect("session")
            .revert,
        None
    );

    let staged = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_reads/revert/stage",
            Some(json!({"messageID": "msg_0001", "files": false})),
        ))
        .await
        .expect("revert restage responds");
    assert_eq!(staged.status(), StatusCode::OK);
    let committed = app
        .oneshot(request(
            Method::POST,
            "/api/session/ses_reads/revert/commit",
            None,
        ))
        .await
        .expect("revert commit responds");
    assert_eq!(committed.status(), StatusCode::NO_CONTENT);
    let connection = fixture.pool.get().expect("fixture database connection");
    let remaining: i64 = connection
        .query_row(
            "SELECT count(*) FROM session_message WHERE session_id = 'ses_reads'",
            [],
            |row| row.get(0),
        )
        .expect("count projected messages");
    assert_eq!(
        remaining, 2,
        "commit removes only rows after the staged boundary"
    );
}

#[tokio::test]
async fn api_pty_list_and_create_use_the_real_pty_service() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let app = api_app(state);
    let empty = app
        .clone()
        .oneshot(request(Method::GET, "/api/pty", None))
        .await
        .expect("PTY list responds");
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(response_json(empty).await["data"], json!([]));

    let script = if cfg!(windows) { "exit /B 0" } else { "exit 0" };
    let (command, args) = native_pty_command(script);
    let created = app
        .oneshot(request(
            Method::POST,
            "/api/pty",
            Some(json!({"command": &command, "args": &args})),
        ))
        .await
        .expect("PTY create responds");
    assert_eq!(created.status(), StatusCode::OK);
    let body = response_json(created).await;
    assert_eq!(body["data"]["command"], command);
}

#[tokio::test]
async fn api_pty_connect_requires_a_single_use_unexpired_ticket_without_echoing_it() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let script = if cfg!(windows) {
        "ping -n 31 127.0.0.1 >NUL"
    } else {
        "sleep 30"
    };
    let (command, args) = native_pty_command(script);
    let info = state
        .pty()
        .create(CreateInput {
            command: Some(command),
            args: Some(args),
            ..CreateInput::default()
        })
        .expect("fixture PTY starts");
    let pty_id = info.id.as_str().to_owned();
    let app = api_app(state.clone());

    let missing_header = app
        .clone()
        .oneshot(request(
            Method::POST,
            &format!("/api/pty/{pty_id}/connect-token"),
            None,
        ))
        .await
        .expect("connect-token responds");
    assert_eq!(missing_header.status(), StatusCode::FORBIDDEN);

    let mut mint = request(
        Method::POST,
        &format!("/api/pty/{pty_id}/connect-token"),
        None,
    );
    mint.headers_mut()
        .insert("x-zuno-ticket", "1".parse().expect("valid header"));
    let minted = app
        .clone()
        .oneshot(mint)
        .await
        .expect("connect-token responds");
    assert_eq!(minted.status(), StatusCode::OK);
    let minted = response_json(minted).await;
    let ticket = minted["data"]["ticket"]
        .as_str()
        .expect("ticket response")
        .to_owned();
    assert_eq!(minted["data"]["expires_in"], 60);

    for uri in [
        format!("/api/pty/{pty_id}/connect"),
        format!("/api/pty/{pty_id}/connect?ticket=definitely-wrong"),
    ] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, &uri, None))
            .await
            .expect("connect rejection responds");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
        let body = response_json(response).await;
        assert!(!body.to_string().contains(&ticket));
        assert!(!body.to_string().contains("definitely-wrong"));
    }

    let first = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/pty/{pty_id}/connect?ticket={ticket}"),
            None,
        ))
        .await
        .expect("valid ticket reaches the upgrade boundary");
    assert_eq!(first.status(), StatusCode::BAD_REQUEST);
    assert!(!response_json(first).await.to_string().contains(&ticket));

    let replay = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/pty/{pty_id}/connect?ticket={ticket}"),
            None,
        ))
        .await
        .expect("ticket replay responds");
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);
    assert!(!response_json(replay).await.to_string().contains(&ticket));

    // Two things made the old expiry assertion pass for the wrong reason, and both
    // are fixed here.
    //
    // 1. It issued the stale ticket with `TicketScope::for_session`, whose
    //    `directory` is `None`, while `pty::connect` redeems with `Some("/repo")`.
    //    The route answered 403 on the scope mismatch, so it stayed green with
    //    expiry enforcement removed entirely. The probe below redeems a *fresh*
    //    ticket carrying the hand-built scope first, so the scope is proven
    //    redeemable before the stale one is trusted to fail for expiry.
    // 2. `prune_expired` sweeps from the FRONT only, which is correct because
    //    production always issues at `Instant::now()` and the queue is therefore
    //    ordered oldest-first. A back-dated ticket pushed behind a fresher one
    //    escapes that sweep. So this block runs last, when the earlier tickets have
    //    all been consumed and the stale ticket is the queue's only — hence
    //    oldest — entry, which is the ordering production guarantees.
    let scope = || TicketScope {
        pty_id: PtyId::from_raw(&pty_id),
        directory: Some("/repo".to_owned()),
        workspace_id: None,
    };
    assert!(
        state.pty().tickets().is_empty(),
        "the stale ticket must be the only entry, or the front-only prune will not reach it and \
         this block stops testing expiry"
    );
    let probe = state.pty().tickets().issue(scope());
    let accepted = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/pty/{pty_id}/connect?ticket={}", probe.ticket),
            None,
        ))
        .await
        .expect("scope probe responds");
    assert_eq!(
        accepted.status(),
        StatusCode::BAD_REQUEST,
        "an unexpired ticket carrying the route's own scope must be redeemed and fail later, at \
         the WebSocket upgrade. A 403 here means the scope below does not match production and \
         the expiry assertion would pass for the wrong reason."
    );

    let expired = state
        .pty()
        .tickets()
        .issue_at(scope(), Instant::now() - Duration::from_secs(61));
    let expired_response = app
        .oneshot(request(
            Method::GET,
            &format!("/api/pty/{pty_id}/connect?ticket={}", expired.ticket),
            None,
        ))
        .await
        .expect("expired connect rejection responds");
    assert_eq!(
        expired_response.status(),
        StatusCode::FORBIDDEN,
        "a ticket older than the 60s TTL must be refused. Its scope matches the route exactly — \
         proven by the probe above — so expiry is the only thing left that can reject it, and \
         removing `prune_expired` turns this into a 400."
    );
    assert!(
        !response_json(expired_response)
            .await
            .to_string()
            .contains(&expired.ticket)
    );
}

#[cfg(unix)]
#[test]
fn api_state_wires_the_configured_terminal_shell() {
    let directory = TempDir::new().expect("temporary terminal workspace");
    let state = ApiState::memory(directory.path().to_string_lossy())
        .expect("in-memory API state initializes")
        .with_configured_shell(Some("/bin/sh".to_owned()));

    let info = state
        .pty()
        .create(CreateInput::default())
        .expect("configured terminal starts");

    assert!(
        info.command.ends_with("/sh"),
        "configured terminal used {}",
        info.command
    );
    state.pty().remove(&info.id).expect("terminal removed");
}

#[tokio::test]
async fn api_pty_websocket_ticket_streams_real_terminal_input_and_output() {
    let directory = tempfile::tempdir().expect("temporary PTY directory");
    let state = ApiState::memory(directory.path().to_string_lossy())
        .expect("in-memory API state initializes");
    #[cfg(unix)]
    let (command, args, input) = (
        "/bin/sh".to_owned(),
        vec!["-i".to_owned()],
        "printf 'WS-READY\\n'\n",
    );
    #[cfg(windows)]
    let (command, args, input) = (
        std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_owned()),
        vec!["/D".to_owned(), "/Q".to_owned()],
        "echo WS-READY\r\n",
    );
    let info = state
        .pty()
        .create(CreateInput {
            command: Some(command),
            args: Some(args),
            ..CreateInput::default()
        })
        .expect("interactive fixture PTY starts");
    let pty_id = info.id.as_str().to_owned();
    let server = ServerBuilder::new(
        ServerConfig::default().with_default_directory(directory.path().to_string_lossy()),
    )
    .with_routes(api::router(state))
    .bind()
    .await
    .expect("test server binds");
    let address = server.local_addr();
    let task = tokio::spawn(server.serve());

    let token: Value = reqwest::Client::new()
        .post(format!("http://{address}/api/pty/{pty_id}/connect-token"))
        .header("x-zuno-ticket", "1")
        .send()
        .await
        .expect("ticket request sends")
        .error_for_status()
        .expect("ticket request succeeds")
        .json()
        .await
        .expect("ticket response is JSON");
    let ticket = token["data"]["ticket"]
        .as_str()
        .expect("ticket response contains credential");

    let mut socket = TcpStream::connect(address)
        .await
        .expect("WebSocket TCP connection opens");
    socket
        .write_all(
            format!(
                "GET /api/pty/{pty_id}/connect?ticket={ticket} HTTP/1.1\r\n\
                 Host: {address}\r\n\
                 Connection: Upgrade\r\n\
                 Upgrade: websocket\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("WebSocket handshake writes");
    let head = read_http_head(&mut socket).await;
    assert!(
        head.starts_with("HTTP/1.1 101 "),
        "upgrade response: {head}"
    );

    write_masked_text_frame(&mut socket, input).await;
    let output = tokio::time::timeout(Duration::from_secs(5), async {
        let mut output = Vec::new();
        loop {
            let (opcode, payload) = read_server_frame(&mut socket).await;
            if opcode == 8 {
                panic!("PTY WebSocket closed before terminal output arrived");
            }
            if matches!(opcode, 1 | 2) && payload.first() != Some(&0) {
                output.extend_from_slice(&payload);
            }
            if String::from_utf8_lossy(&output).contains("WS-READY") {
                break output;
            }
        }
    })
    .await
    .expect("PTY output arrives before timeout");
    assert!(String::from_utf8_lossy(&output).contains("WS-READY"));

    task.abort();
}

#[tokio::test]
async fn api_maintenance_preview_is_inert_and_emits_ordered_progress() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    state
        .sessions()
        .create(
            &SessionCreate::new(
                "ses_old", "ses_old", "global", "/repo", "/repo", "old", "test",
            )
            .at(1),
        )
        .expect("old fixture session inserts");
    let (app, services) = api_app_with_services(state.clone());
    let mut progress = services.maintenance_events.subscribe();

    let response = app
        .oneshot(request(
            Method::GET,
            "/api/session/prune?olderThan=90&project=global",
            None,
        ))
        .await
        .expect("maintenance preview responds");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("preview body is readable");
    let body: Value = serde_json::from_slice(&bytes).expect("preview is JSON");
    assert_eq!(body["action"], "preview");
    assert_eq!(body["selected_session_ids"], json!(["ses_old"]));
    assert_eq!(body["changed_sessions"], 0);
    assert!(
        state
            .sessions()
            .find("ses_old")
            .expect("read session")
            .is_some()
    );

    let mut phases = Vec::new();
    for _ in 0..5 {
        let delivery = progress.recv().await.expect("progress stream remains open");
        let Delivery::Event(event) = delivery else {
            panic!("progress must not lag");
        };
        phases.push(event.phase);
    }
    assert_eq!(
        phases,
        [
            zuno_db::session_prune::ProgressPhase::Selecting,
            zuno_db::session_prune::ProgressPhase::Selected,
            zuno_db::session_prune::ProgressPhase::Database,
            zuno_db::session_prune::ProgressPhase::Artifacts,
            zuno_db::session_prune::ProgressPhase::Completed,
        ]
    );
}

#[tokio::test]
async fn api_maintenance_mutation_requires_apply_true() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    state
        .sessions()
        .create(
            &SessionCreate::new(
                "ses_old", "ses_old", "global", "/repo", "/repo", "old", "test",
            )
            .at(1),
        )
        .expect("old fixture session inserts");

    for body in [
        json!({
            "olderThan": 90,
            "project": "global",
            "action": "delete"
        }),
        json!({
            "olderThan": 90,
            "project": "global",
            "action": "delete",
            "apply": false
        }),
    ] {
        let response = api_app(state.clone())
            .oneshot(request(Method::POST, "/api/session/prune", Some(body)))
            .await
            .expect("maintenance mutation responds");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "invalid_request"
        );
        assert!(
            state
                .sessions()
                .find("ses_old")
                .expect("read session")
                .is_some()
        );
    }
}

#[tokio::test]
async fn api_maintenance_archive_mutates_only_with_explicit_apply() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    state
        .sessions()
        .create(
            &SessionCreate::new(
                "ses_old", "ses_old", "global", "/repo", "/repo", "old", "test",
            )
            .at(1),
        )
        .expect("old fixture session inserts");

    let response = api_app(state.clone())
        .oneshot(request(
            Method::POST,
            "/api/session/prune",
            Some(json!({
                "olderThan": 90,
                "project": "global",
                "action": "archive",
                "apply": true
            })),
        ))
        .await
        .expect("maintenance archive responds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(body["action"]["archive"]["at_ms"].is_number());
    assert_eq!(body["changed_sessions"], 1);
    assert!(
        state
            .sessions()
            .find("ses_old")
            .expect("read session")
            .expect("session remains after archive")
            .time_archived
            .is_some()
    );
}

#[test]
fn api_maintenance_openapi_registers_preview_and_mutation() {
    let document = api::openapi();
    assert!(document["paths"]["/api/session/prune"]["get"].is_object());
    assert!(document["paths"]["/api/session/prune"]["post"].is_object());
    assert!(document["components"]["schemas"]["SessionPruneMutation"].is_object());
    assert!(document["components"]["schemas"]["SessionActiveResponse"].is_object());
}

// ---------------------------------------------------------------------------
// Filesystem sandbox
//
// These are the tests that make `/api/fs/*` safe to expose. The server binds
// loopback and may be unauthenticated, so a containment defect here is an
// arbitrary-file-read primitive, not a cosmetic bug. Each case below is a distinct
// attack *shape* rather than a restatement of the same one, because the two
// containment stages fail differently: `../` and an absolute path are caught
// lexically, a symlink is only caught after `canonicalize`, and `%2e%2e` tests
// whether decoding happens before or after the check.
// ---------------------------------------------------------------------------

/// An environment isolated from the host's home and XDG directories.
///
/// `Layout::resolve` falls back to the real home when the injected environment
/// says nothing, so an "empty" environment still reads the developer's own
/// `auth.json` and their stored credentials change which providers resolve as
/// available. Every catalogue test pins these five keys for that reason.
fn isolated_env(root: &std::path::Path) -> zuno_paths::Env {
    zuno_paths::Env::empty()
        .with("HOME", root.join("home").to_string_lossy().into_owned())
        .with(
            "XDG_DATA_HOME",
            root.join("data").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_CONFIG_HOME",
            root.join("config").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_CACHE_HOME",
            root.join("cache").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_STATE_HOME",
            root.join("state").to_string_lossy().into_owned(),
        )
}

/// A worktree with one readable file, one nested directory, and a symlink that
/// points at a secret outside the root.
fn fs_fixture() -> (tempfile::TempDir, String) {
    let root = tempfile::tempdir().expect("filesystem fixture root");
    let outside = root.path().join("outside");
    let inside = root.path().join("inside");
    std::fs::create_dir_all(&outside).expect("create the out-of-root directory");
    std::fs::create_dir_all(inside.join("nested")).expect("create the in-root directory");
    std::fs::write(outside.join("secret.txt"), b"OUT-OF-ROOT-SECRET")
        .expect("write the out-of-root secret");
    std::fs::write(inside.join("visible.txt"), b"in-root\n").expect("write the in-root file");
    std::fs::write(inside.join("nested").join("deep.txt"), b"deep\n").expect("write nested file");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("secret.txt"), inside.join("escape-link.txt"))
        .expect("create the escaping symlink");
    let directory = inside.to_string_lossy().into_owned();
    (root, directory)
}

async fn fs_body(state: ApiState, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = api_app(state)
        .oneshot(request(Method::GET, uri, None))
        .await
        .expect("filesystem endpoint responds");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body is bounded");
    (status, bytes.to_vec())
}

#[tokio::test]
async fn api_fs_read_refuses_every_shape_of_escape_from_the_session_directory() {
    let (_root, directory) = fs_fixture();
    let escapes = [
        ("../outside/secret.txt", "a relative parent traversal"),
        (
            "nested/../../outside/secret.txt",
            "a traversal that only escapes after folding",
        ),
        ("%2e%2e/outside/secret.txt", "a percent-encoded traversal"),
        (
            "%2E%2E%2Foutside%2Fsecret.txt",
            "a fully percent-encoded traversal",
        ),
        (
            "etc/../../../../etc/hostname",
            "a deep traversal to a real file",
        ),
        #[cfg(unix)]
        ("escape-link.txt", "a symlink pointing out of the root"),
    ];
    for (path, shape) in escapes {
        let state = ApiState::memory(directory.clone()).expect("API state");
        let (status, body) = fs_body(state, &format!("/api/fs/read/{path}")).await;
        let text = String::from_utf8_lossy(&body);
        assert!(
            matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
            "{shape} (`{path}`) must be refused, got {status}: {text}"
        );
        assert!(
            !text.contains("OUT-OF-ROOT-SECRET"),
            "{shape} (`{path}`) leaked a file outside the session directory"
        );
    }
}

#[tokio::test]
async fn api_fs_read_names_the_violation_when_a_path_escapes() {
    let (_root, directory) = fs_fixture();
    let state = ApiState::memory(directory).expect("API state");
    let (status, body) = fs_body(state, "/api/fs/read/../outside/secret.txt").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let json: Value = serde_json::from_slice(&body).expect("refusal is JSON");
    assert_eq!(json["error"]["code"], "path_escaped_root");
    assert_eq!(
        json["error"]["message"],
        "the requested path leaves the session directory"
    );
}

#[tokio::test]
async fn api_fs_read_serves_a_file_inside_the_session_directory() {
    let (_root, directory) = fs_fixture();
    let state = ApiState::memory(directory).expect("API state");
    let (status, body) = fs_body(state, "/api/fs/read/visible.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"in-root\n");
}

#[tokio::test]
async fn api_fs_read_folds_a_traversal_that_stays_inside_the_root() {
    let (_root, directory) = fs_fixture();
    let state = ApiState::memory(directory).expect("API state");
    let (status, body) = fs_body(state, "/api/fs/read/nested/../visible.txt").await;
    assert_eq!(status, StatusCode::OK, "a contained `..` is not an escape");
    assert_eq!(body, b"in-root\n");
}

#[tokio::test]
async fn api_fs_list_refuses_to_leave_the_session_directory() {
    let (_root, directory) = fs_fixture();
    for path in ["..", "/etc", "/", "nested/../.."] {
        let state = ApiState::memory(directory.clone()).expect("API state");
        let (status, body) = fs_body(state, &format!("/api/fs/list?path={path}")).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "listing `{path}` must be refused: {}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[tokio::test]
async fn api_fs_list_orders_directories_before_files_and_marks_them() {
    let (_root, directory) = fs_fixture();
    let state = ApiState::memory(directory).expect("API state");
    let (status, body) = fs_body(state, "/api/fs/list").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("listing is JSON");
    assert_eq!(
        json["data"],
        json!([
            {"path": "nested/", "type": "directory"},
            {"path": "visible.txt", "type": "file"}
        ]),
        "a symlink is dropped, a directory carries its separator, and directories sort first"
    );
    assert!(json["location"]["directory"].is_string());
}

#[tokio::test]
async fn api_fs_find_rejects_a_missing_query_the_way_the_oracle_does() {
    let (_root, directory) = fs_fixture();
    let state = ApiState::memory(directory).expect("API state");
    let (status, body) = fs_body(state, "/api/fs/find").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let json: Value = serde_json::from_slice(&body).expect("refusal is JSON");
    assert_eq!(json["_tag"], "InvalidRequestError");
    assert_eq!(json["kind"], "Query");
    assert_eq!(json["message"], "Missing key\n  at [\"query\"]");
}

#[tokio::test]
async fn api_fs_find_stays_inside_the_session_directory_and_honours_its_limit() {
    let (_root, directory) = fs_fixture();
    let state = ApiState::memory(directory.clone()).expect("API state");
    let (status, body) = fs_body(state, "/api/fs/find?query=deep").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("results are JSON");
    assert_eq!(
        json["data"],
        json!([{"path": "nested/deep.txt", "type": "file"}])
    );

    let state = ApiState::memory(directory.clone()).expect("API state");
    let (_, body) = fs_body(state, "/api/fs/find?query=secret").await;
    let json: Value = serde_json::from_slice(&body).expect("results are JSON");
    assert_eq!(
        json["data"],
        json!([]),
        "find must not reach a file outside the session directory"
    );

    let state = ApiState::memory(directory.clone()).expect("API state");
    let (_, body) = fs_body(state, "/api/fs/find?query=t&limit=1").await;
    let json: Value = serde_json::from_slice(&body).expect("results are JSON");
    assert_eq!(
        json["data"].as_array().map(Vec::len),
        Some(1),
        "limit must bound the result set"
    );

    let state = ApiState::memory(directory).expect("API state");
    let (status, _) = fs_body(state, "/api/fs/find?query=t&limit=0").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a non-positive limit is a request error, not a silent default"
    );
}

// ---------------------------------------------------------------------------
// Catalogue operations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_catalogue_operations_answer_in_the_location_envelope() {
    let (root, directory) = fs_fixture();
    let env = isolated_env(root.path());
    for path in [
        "/api/agent",
        "/api/command",
        "/api/skill",
        "/api/reference",
        "/api/model",
        "/api/provider",
        "/api/integration",
    ] {
        let state = ApiState::memory(directory.clone())
            .expect("API state")
            .with_env(env.clone());
        let (status, body) = fs_body(state, path).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path} must answer: {}",
            String::from_utf8_lossy(&body)
        );
        let json: Value = serde_json::from_slice(&body).expect("catalogue body is JSON");
        assert!(
            json["data"].is_array(),
            "{path} must answer with a data array, not {}",
            json["data"]
        );
        assert!(
            json["location"]["project"]["id"].is_string(),
            "{path} must carry the location envelope every SDK caller reads"
        );
    }
}

#[tokio::test]
async fn api_init_command_is_written_for_future_zuno_sessions() {
    let (root, directory) = fs_fixture();
    let state = ApiState::memory(directory)
        .expect("API state")
        .with_env(isolated_env(root.path()));
    let (status, body) = fs_body(state, "/api/command").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("command body is JSON");
    let init = json["data"]
        .as_array()
        .expect("commands are an array")
        .iter()
        .find(|entry| entry["name"] == "init")
        .expect("init command is present");
    let template = init["template"].as_str().expect("init template is text");
    assert!(template.contains("future Zuno sessions"), "{template}");
    assert!(!template.contains("OpenCode"), "{template}");
}

#[tokio::test]
async fn api_init_deep_command_is_native_project_scoped_and_hierarchical() {
    let (root, directory) = fs_fixture();
    let state = ApiState::memory(directory.clone())
        .expect("API state")
        .with_env(isolated_env(root.path()));
    let (status, body) = fs_body(state, "/api/command").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("command body is JSON");
    let init_deep = json["data"]
        .as_array()
        .expect("commands are an array")
        .iter()
        .find(|entry| entry["name"] == "init-deep")
        .expect("init-deep command is present");
    let template = init_deep["template"]
        .as_str()
        .expect("init-deep template is text");

    assert!(template.contains("future Zuno sessions"), "{template}");
    assert!(
        !template.contains("OpenCode") && !template.contains("oh-my") && !template.contains("OMO"),
        "{template}"
    );
    assert!(!template.contains("${path}"), "{template}");
    let project_directory = json["location"]["project"]["directory"]
        .as_str()
        .expect("location carries the project directory");
    assert!(
        template.contains(&format!("repository rooted at `{project_directory}`")),
        "{template}"
    );
    for required in [
        "Use CodeGraph first",
        "Generate or update the root `AGENTS.md`",
        "real responsibility, build, language, or deployment boundary",
        "only rules that are new relative to its parent `AGENTS.md`",
        "Do not repeat parent rules",
    ] {
        assert!(
            template.contains(required),
            "missing {required:?}: {template}"
        );
    }
}

#[tokio::test]
async fn api_user_review_command_keeps_the_users_template() {
    let (root, directory) = fs_fixture();
    let command_directory = std::path::Path::new(&directory).join(".zuno/command");
    std::fs::create_dir_all(&command_directory).expect("create project command directory");
    std::fs::write(
        command_directory.join("review.md"),
        "USER-OWNED REVIEW $ARGUMENTS\n",
    )
    .expect("write user-owned review command");
    let state = ApiState::memory(directory)
        .expect("API state")
        .with_env(isolated_env(root.path()));

    let (status, body) = fs_body(state, "/api/command").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("command body is JSON");
    let review = json["data"]
        .as_array()
        .expect("commands are an array")
        .iter()
        .find(|entry| entry["name"] == "review")
        .expect("the user-owned review command is present");

    assert_eq!(review["template"], "USER-OWNED REVIEW $ARGUMENTS");
}

#[tokio::test]
async fn api_user_init_command_can_override_the_builtin_template() {
    let (root, directory) = fs_fixture();
    let command_directory = std::path::Path::new(&directory).join(".zuno/command");
    std::fs::create_dir_all(&command_directory).expect("create project command directory");
    std::fs::write(
        command_directory.join("init.md"),
        "USER-OWNED INIT $ARGUMENTS\n",
    )
    .expect("write user-owned init command");
    let state = ApiState::memory(directory)
        .expect("API state")
        .with_env(isolated_env(root.path()));

    let (status, body) = fs_body(state, "/api/command").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("command body is JSON");
    let init = json["data"]
        .as_array()
        .expect("commands are an array")
        .iter()
        .find(|entry| entry["name"] == "init")
        .expect("the user-owned init command is present");

    assert_eq!(init["template"], "USER-OWNED INIT $ARGUMENTS");
}

#[tokio::test]
async fn api_agent_roster_is_the_resolved_native_set() {
    let (root, directory) = fs_fixture();
    let state = ApiState::memory(directory)
        .expect("API state")
        .with_env(isolated_env(root.path()));
    let (status, body) = fs_body(state, "/api/agent").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("agent body is JSON");
    let agents = json["data"].as_array().expect("agents are an array");
    let names = agents
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(
        names.contains("build") && names.contains("plan"),
        "the native roster must reach the API, got {names:?}"
    );
    let build = agents
        .iter()
        .find(|entry| entry["id"] == "build")
        .expect("build is present");
    assert!(build["system"].is_string(), "the system prompt is exposed");
    assert_eq!(build["mode"], "primary");
    assert_eq!(build["hidden"], false);
    assert!(build["request"]["headers"].is_object());
    let deep = agents
        .iter()
        .find(|entry| entry["id"] == "deep")
        .expect("deep is present");
    assert_eq!(
        deep["mode"], "all",
        "deep must be both directly selectable and delegable"
    );
    let plan = agents
        .iter()
        .find(|entry| entry["id"] == "plan")
        .expect("plan is present");
    let resources = plan["permissions"]
        .as_array()
        .expect("permissions are an array")
        .iter()
        .filter_map(|rule| rule["resource"].as_str())
        .collect::<Vec<_>>();
    assert!(resources.contains(&".zuno/plans/*.md"));
    assert!(!resources.contains(&".opencode/plans/*.md"));
}

#[tokio::test]
async fn api_skill_reports_every_first_party_location_and_description() {
    let (root, directory) = fs_fixture();
    let state = ApiState::memory(directory)
        .expect("API state")
        .with_env(isolated_env(root.path()));
    let (status, body) = fs_body(state, "/api/skill").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("skill body is JSON");
    let skills = json["data"].as_array().expect("skills are an array");
    for name in [
        "customize-zuno",
        "deepwork",
        "codemap",
        "verification-planning",
        "reflect",
        "worktree",
        "git-workflow",
    ] {
        let builtin = skills
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("first-party Skill {name} is registered"));
        assert_eq!(
            builtin["location"],
            format!("/builtin/{name}.md"),
            "the API projects the stable embedded source into a native location"
        );
        assert!(
            builtin["description"]
                .as_str()
                .is_some_and(|description| !description.trim().is_empty()),
            "{name} must carry a model-facing description"
        );
        assert_eq!(
            builtin["slash"],
            Value::Bool(true),
            "an unambiguous first-party Skill must be invokable as `/{name}` in every client"
        );
    }

    let customize = skills
        .iter()
        .find(|entry| entry["name"] == "customize-zuno")
        .expect("customize-zuno is registered");
    let description = customize["description"]
        .as_str()
        .expect("description")
        .to_ascii_lowercase();
    for surface in [
        "configuration",
        "agents",
        "workflows",
        "skills",
        "mcp servers",
    ] {
        assert!(
            description.contains(surface),
            "the description omits native configuration surface {surface:?}: {description}"
        );
    }
    assert!(!description.contains("opencode"));
}

#[tokio::test]
async fn api_provider_reports_upstreams_tagged_not_found_body() {
    let (root, directory) = fs_fixture();
    let state = ApiState::memory(directory)
        .expect("API state")
        .with_env(isolated_env(root.path()));
    let (status, body) = fs_body(state, "/api/provider/definitely-absent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let json: Value = serde_json::from_slice(&body).expect("refusal is JSON");
    assert_eq!(json["_tag"], "ProviderNotFoundError");
    assert_eq!(json["providerID"], "definitely-absent");
    assert_eq!(json["message"], "Provider not found: definitely-absent");
}

#[tokio::test]
async fn api_integration_answers_200_with_a_null_data_for_an_unknown_id() {
    let (root, directory) = fs_fixture();
    let state = ApiState::memory(directory)
        .expect("API state")
        .with_env(isolated_env(root.path()));
    let (status, body) = fs_body(state, "/api/integration/definitely-absent").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an unknown integration is a success with no value upstream, not a 404"
    );
    let json: Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(
        json["data"],
        Value::Null,
        "1.18.12 answers `data: null`, measured live against the released binary: {json}"
    );
}

#[tokio::test]
async fn api_catalogue_projects_a_pinned_models_document_onto_the_v2_shape() {
    let (root, directory) = fs_fixture();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../zuno-llm/tests/fixtures/models-dev-pinned.json")
        .canonicalize()
        .expect("the pinned catalogue fixture exists");
    let env = isolated_env(root.path())
        .with("ZUNO_MODELS_PATH", fixture.to_string_lossy().into_owned())
        .with("ZUNO_DISABLE_MODELS_FETCH", "1")
        .with("DEEPSEEK_API_KEY", "probe-key");
    let state = ApiState::memory(directory)
        .expect("API state")
        .with_env(env);

    let (status, body) = fs_body(state.clone(), "/api/provider").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("provider body is JSON");
    assert_eq!(
        json["data"],
        json!([{
            "id": "deepseek",
            "name": "DeepSeek",
            "api": {
                "type": "aisdk",
                "package": "@ai-sdk/openai-compatible",
                "url": "https://api.deepseek.com"
            },
            "request": {"headers": {}, "body": {}}
        }]),
        "only the provider the environment authenticates is available"
    );

    let (_, body) = fs_body(state.clone(), "/api/model").await;
    let json: Value = serde_json::from_slice(&body).expect("model body is JSON");
    let models = json["data"].as_array().expect("models are an array");
    assert_eq!(models.len(), 2);
    assert_eq!(
        models[0],
        json!({
            "id": "deepseek-chat",
            "providerID": "deepseek",
            "family": "deepseek",
            "name": "DeepSeek Chat",
            "api": {
                "id": "deepseek-chat",
                "type": "aisdk",
                "package": "@ai-sdk/openai-compatible",
                "url": "https://api.deepseek.com"
            },
            "capabilities": {"tools": true, "input": ["text"], "output": ["text"]},
            "request": {"headers": {}, "body": {}},
            "variants": [],
            "time": {"released": 1_764_547_200_000_i64},
            "cost": [{"input": 0.14, "output": 0.28, "cache": {"read": 0.0028, "write": 0}}],
            "status": "active",
            "enabled": true,
            "limit": {"context": 1_000_000, "output": 384_000}
        }),
        "the provider's api and request are folded into the model, as `projectModel` does"
    );

    let (_, body) = fs_body(state, "/api/integration").await;
    let json: Value = serde_json::from_slice(&body).expect("integration body is JSON");
    let integrations = json["data"].as_array().expect("integrations are an array");
    let names = integrations
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "AnyAPI",
            "DeepSeek",
            "Groq",
            "Impossibl",
            "Inceptron",
            "Mistral",
            "openai",
            "Zhipu AI"
        ],
        "integrations contain only catalogue providers and native authentication methods"
    );
    let deepseek = integrations
        .iter()
        .find(|entry| entry["id"] == "deepseek")
        .expect("deepseek is registered");
    assert_eq!(
        deepseek["connections"],
        json!([{"type": "env", "name": "DEEPSEEK_API_KEY"}]),
        "an environment variable that is set is a live connection"
    );
    assert_eq!(
        integrations
            .iter()
            .find(|entry| entry["id"] == "groq")
            .expect("groq is registered")["connections"],
        json!([]),
        "an unset variable is not a connection"
    );
    let openai = integrations
        .iter()
        .find(|entry| entry["id"] == "openai")
        .expect("native OpenAI authentication is registered");
    let methods = openai["methods"].as_array().expect("OpenAI methods");
    for method_id in ["chatgpt-browser", "chatgpt-device"] {
        assert!(
            methods.iter().any(|method| method["id"] == method_id),
            "OpenAI must advertise {method_id}: {methods:?}"
        );
    }
    assert!(
        methods.iter().any(|method| method["type"] == "key"),
        "OpenAI must also accept API keys: {methods:?}"
    );
}

/// Holds the compaction lease until released and records every prompt it is
/// asked to run, so a test can prove that input admitted *during* a compaction
/// is still driven once the lease is released.
#[derive(Debug, Default)]
struct CompactionLeaseExecutor {
    compact_started: AtomicBool,
    compact_started_notify: Notify,
    compact_release: Arc<Notify>,
    prompts: Mutex<Vec<SessionPromptExecution>>,
    prompt_notify: Arc<Notify>,
}

impl CompactionLeaseExecutor {
    async fn wait_until_compact_started(&self) {
        loop {
            let mut notified = std::pin::pin!(self.compact_started_notify.notified());
            notified.as_mut().enable();
            if self.compact_started.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn prompts(&self) -> Vec<SessionPromptExecution> {
        self.prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn wait_until_prompt_count(&self, count: usize) {
        loop {
            let mut notified = std::pin::pin!(self.prompt_notify.notified());
            notified.as_mut().enable();
            if self.prompts().len() >= count {
                return;
            }
            notified.await;
        }
    }
}

impl SessionMutationExecutor for CompactionLeaseExecutor {
    fn prompt(
        &self,
        request: SessionPromptExecution,
        guard: zuno_engine::status::SessionRunGuard,
        _events: TurnEventSender,
    ) -> SessionMutationFuture {
        self.prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
        let notify = Arc::clone(&self.prompt_notify);
        Box::pin(async move {
            drop(guard);
            notify.notify_waiters();
            Ok(())
        })
    }

    fn compact(
        &self,
        _request: SessionCompactExecution,
        guard: zuno_engine::status::SessionRunGuard,
        _events: TurnEventSender,
    ) -> SessionMutationFuture {
        self.compact_started.store(true, Ordering::SeqCst);
        self.compact_started_notify.notify_waiters();
        let release = Arc::clone(&self.compact_release);
        Box::pin(async move {
            release.notified().await;
            drop(guard);
            Ok(())
        })
    }
}

#[tokio::test]
async fn api_interrupt_on_an_idle_session_does_not_arm_the_next_turn() {
    // Given: a session with no live turn.
    let fixture = MutationApiFixture::new("ses_idle_interrupt");
    let (app, services) = api_app_with_services(fixture.state.clone());
    assert_eq!(
        services.runs.status("ses_idle_interrupt"),
        SessionStatus::Idle
    );

    // When: a client cancels while nothing is running.
    let response = app
        .oneshot(request(
            Method::POST,
            "/api/session/ses_idle_interrupt/interrupt",
            None,
        ))
        .await
        .expect("interrupt responds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Then: the next ordinary turn starts clean instead of being born interrupted.
    let guard = services
        .runs
        .begin_turn("ses_idle_interrupt")
        .expect("the idle session accepts a new turn");
    assert_eq!(
        guard.interrupt_request(),
        None,
        "an interrupt with no live turn must not leave a marker that cancels the next turn"
    );
    assert!(
        !guard.interrupt_signal().is_set(),
        "the new turn's cancellation signal must start unset"
    );
}

#[tokio::test]
async fn api_prompt_queued_during_compaction_runs_after_the_lease_is_released() {
    // Given: a compaction that holds the session's live-turn lease.
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    state
        .sessions()
        .create(&SessionCreate::new(
            "ses_compact_queue",
            "ses_compact_queue",
            "global",
            "/repo",
            "/repo",
            "compact queue",
            "test",
        ))
        .expect("fixture session inserts");
    let executor = Arc::new(CompactionLeaseExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_services(services.clone())
        .with_routes(api::router(state))
        .router();
    let mut compact_task = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(request(
                Method::POST,
                "/api/session/ses_compact_queue/compact",
                None,
            ))
            .await
            .expect("compact responds")
        }
    });
    executor.wait_until_compact_started().await;
    assert_eq!(
        services.runs.status("ses_compact_queue"),
        SessionStatus::Busy
    );

    // When: a prompt is admitted while the compaction owns the lease.
    let admitted = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/session/ses_compact_queue/prompt",
            Some(json!({
                "id": "msg_after_compaction",
                "prompt": {"text": "continue after compaction", "files": [], "agents": []}
            })),
        ))
        .await
        .expect("prompt responds");
    assert_eq!(admitted.status(), StatusCode::OK);
    assert!(
        executor.prompts().is_empty(),
        "the prompt must wait for the compaction lease"
    );

    // And: the compaction finishes and releases the lease.
    executor.compact_release.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(1), &mut compact_task)
        .await
        .expect("compact finishes after release")
        .expect("compact task does not panic");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Then: the durable input is driven without waiting for another external event.
    tokio::time::timeout(Duration::from_secs(1), executor.wait_until_prompt_count(1))
        .await
        .expect("the queued prompt must run once the compaction lease is released");
    assert_eq!(
        executor
            .prompts()
            .iter()
            .map(|prompt| prompt.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["msg_after_compaction"]
    );
    services.runs.wait_until_idle("ses_compact_queue").await;
}
