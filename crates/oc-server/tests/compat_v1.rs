use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use oc_db::Pool;
use oc_db::artifact_gc::ArtifactGcPaths;
use oc_db::message::{MessageRecord, MessageStore, PartRecord, now_millis};
use oc_db::session::SessionCreate;
use oc_engine::interrupt::InterruptSignal;
use oc_engine::r#loop::TurnEventSender;
use oc_paths::DbLocation;
use oc_server::api::{self, ApiState};
use oc_server::compat_v1::{TOAST_PATH, V1_DIAGNOSTICS_PATH, V1Method, v1_coverage};
use oc_server::{
    CompatV1State, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventService, ServerBuilder, ServerConfig,
    ServerServices, SessionCompactExecution, SessionMutationExecutor, SessionMutationFuture,
    SessionPromptExecution, Toast, ToastForwarder, V1_PREFIXES, V1_SURFACE, compat_v1_router,
    events_router,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const ORACLE: &str = include_str!("../../../.omo/fixtures/oracle-openapi-1.18.12.json");

fn compat_app(state: CompatV1State) -> Router {
    let api_state = ApiState::memory("/repo").expect("in-memory API state initializes");
    seed_session(&api_state);
    ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_routes(compat_v1_router(state, api_state))
        .router()
}

/// The router every other surface is merged into, so a shadowing regression in
/// the v1 catch-all fails here rather than in someone's hands-on QA.
fn assembled_app(state: CompatV1State) -> Router {
    let pool =
        Arc::new(oc_db::Pool::open(&DbLocation::Memory).expect("in-memory event database opens"));
    let events = EventService::new(pool, DEFAULT_EVENT_SUBSCRIBER_CAPACITY);
    let api_state = ApiState::memory("/repo").expect("in-memory API state initializes");
    seed_session(&api_state);
    ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_routes(
            api::router(api_state.clone())
                .merge(events_router(events))
                .merge(compat_v1_router(state, api_state)),
        )
        .router()
}

fn seed_session(state: &ApiState) {
    let directory = state.directory().to_owned();
    state
        .sessions()
        .create(&SessionCreate::new(
            "ses_fixture",
            "ses_fixture",
            "global",
            &directory,
            &directory,
            "fixture",
            "test",
        ))
        .expect("fixture session inserts");
}

#[derive(Debug)]
struct CompletingMutationExecutor {
    database: PathBuf,
    prompts: Mutex<Vec<SessionPromptExecution>>,
    compact_calls: AtomicUsize,
}

impl CompletingMutationExecutor {
    fn new(database: PathBuf) -> Self {
        Self {
            database,
            prompts: Mutex::new(Vec::new()),
            compact_calls: AtomicUsize::new(0),
        }
    }

    fn prompts(&self) -> Vec<SessionPromptExecution> {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl SessionMutationExecutor for CompletingMutationExecutor {
    fn prompt(
        &self,
        request: SessionPromptExecution,
        _interrupt: InterruptSignal,
        _events: TurnEventSender,
    ) -> SessionMutationFuture {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        let database = self.database.clone();
        Box::pin(async move {
            let pool =
                Pool::open(&DbLocation::File(database)).map_err(|error| error.to_string())?;
            let connection = pool.get().map_err(|error| error.to_string())?;
            let created = now_millis();
            let assistant_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
            let part_id = format!("prt_{}", uuid::Uuid::new_v4().simple());
            let model = request.model.unwrap_or(oc_server::SessionModelSelection {
                provider_id: "test".to_owned(),
                model_id: "test".to_owned(),
            });
            let directory = request.directory.to_string_lossy().into_owned();
            let message = MessageRecord::from_json(json!({
                "id": assistant_id,
                "sessionID": request.session_id,
                "role": "assistant",
                "time": {"created": created, "completed": created},
                "parentID": request.message_id,
                "modelID": model.model_id,
                "providerID": model.provider_id,
                "mode": request.agent.unwrap_or_else(|| "build".to_owned()),
                "path": {"cwd": directory, "root": directory},
                "cost": 0,
                "tokens": {
                    "input": 1,
                    "output": 1,
                    "reasoning": 0,
                    "cache": {"read": 0, "write": 0}
                },
                "finish": "stop"
            }))
            .map_err(|error| error.to_string())?;
            let part = PartRecord::from_json(
                json!({
                    "id": part_id,
                    "sessionID": message.session_id,
                    "messageID": message.id,
                    "type": "text",
                    "text": format!("completed: {}", request.prompt)
                }),
                created,
            )
            .map_err(|error| error.to_string())?;
            let store = MessageStore::new(&connection);
            store
                .put_message_at(&message, created)
                .map_err(|error| error.to_string())?;
            store
                .put_part_at(&part, created)
                .map_err(|error| error.to_string())?;
            Ok(())
        })
    }

    fn compact(
        &self,
        _request: SessionCompactExecution,
        _interrupt: InterruptSignal,
    ) -> SessionMutationFuture {
        self.compact_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

struct AdapterFixture {
    _temp: TempDir,
    app: Router,
    services: ServerServices,
    executor: Arc<CompletingMutationExecutor>,
}

fn adapter_fixture() -> AdapterFixture {
    let temp = tempfile::tempdir().expect("temporary adapter directory");
    let directory = temp.path().join("repo");
    std::fs::create_dir(&directory).expect("adapter repository directory creates");
    let database = temp.path().join("opencode.db");
    let location = DbLocation::File(database.clone());
    let models = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../oc-llm/tests/fixtures/models-dev-pinned.json")
        .canonicalize()
        .expect("the pinned catalogue fixture exists");
    let env = oc_paths::Env::empty()
        .with("HOME", temp.path().to_string_lossy().into_owned())
        .with(
            "OPENCODE_TEST_HOME",
            temp.path().to_string_lossy().into_owned(),
        )
        .with(
            "OPENCODE_MODELS_PATH",
            models.to_string_lossy().into_owned(),
        )
        .with("OPENCODE_DISABLE_MODELS_FETCH", "1")
        .with("DEEPSEEK_API_KEY", "probe-key");
    let state = ApiState::from_pool(
        Pool::open(&location).expect("adapter database opens"),
        directory.to_string_lossy().into_owned(),
        ArtifactGcPaths::from_data_root(temp.path()),
    )
    .expect("adapter API state initializes")
    .with_env(env);
    seed_session(&state);
    let executor = Arc::new(CompletingMutationExecutor::new(database));
    let services = ServerServices::new(DEFAULT_EVENT_SUBSCRIBER_CAPACITY)
        .with_mutations(Arc::clone(&executor) as Arc<_>);
    let app = ServerBuilder::new(
        ServerConfig::default().with_default_directory(directory.to_string_lossy()),
    )
    .with_services(services.clone())
    .with_routes(api::router(state.clone()).merge(compat_v1_router(CompatV1State::new(), state)))
    .router();
    AdapterFixture {
        _temp: temp,
        app,
        services,
        executor,
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

fn concrete_uri(path: &str) -> String {
    path.replace("{providerID}", "anthropic")
        .replace("{sessionID}", "ses_fixture")
}

/// Finds a citation of numbered plan work: `todo`/`todos` followed by a number.
///
/// A plain substring test for "todo" cannot be used, and finding that out is half
/// the point: `client.session.todo` is a real SDK method this surface serves, so it
/// appears in these bodies legitimately. What must never appear is a reference to
/// numbered plan work — the form that was true when written and silently stopped
/// being true once those todos closed.
fn plan_todo_citation(text: &str) -> Option<String> {
    let lowered = text.to_lowercase();
    let mut search = 0;
    while let Some(offset) = lowered[search..].find("todo") {
        let word_start = search + offset;
        let mut cursor = word_start + "todo".len();
        if lowered[cursor..].starts_with('s') {
            cursor += 1;
        }
        let digits: String = lowered[cursor..]
            .trim_start_matches([' ', '\t'])
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return Some(format!("{} {digits}", &lowered[word_start..cursor]));
        }
        search = cursor;
    }
    None
}

#[test]
fn compat_v1_router_derived_coverage_counts_only_registered_backends() {
    let coverage = v1_coverage();
    assert_eq!(coverage.measured, 20);
    assert_eq!(coverage.served, 11);
    assert_eq!(coverage.unbacked, 9);
    assert_eq!(coverage.redirected, 0);
}

#[tokio::test]
async fn compat_v1_backed_sdk_routes_return_expected_catalog_and_session_shapes() {
    let fixture = adapter_fixture();
    let app = fixture.app;

    let agents = app
        .clone()
        .oneshot(request(Method::GET, "/agent", None))
        .await
        .expect("agent adapter responds");
    assert_eq!(agents.status(), StatusCode::OK);
    let agents = response_json(agents).await;
    let agents = agents
        .as_array()
        .expect("the SDK receives a bare agent array");
    let build = agents
        .iter()
        .find(|agent| agent["name"] == "build")
        .expect("the resolved build agent is projected");
    assert_eq!(build["mode"], "primary");
    assert_eq!(build["builtIn"], true);
    assert!(build["permission"].is_object());
    assert!(build["tools"].is_object());
    assert!(build["options"].is_object());

    let providers = app
        .clone()
        .oneshot(request(Method::GET, "/provider", None))
        .await
        .expect("provider adapter responds");
    assert_eq!(providers.status(), StatusCode::OK);
    let providers = response_json(providers).await;
    assert!(providers["all"].is_array());
    assert!(providers["default"].is_object());
    assert!(providers["connected"].is_array());
    let provider = providers["all"]
        .as_array()
        .and_then(|all| all.first())
        .expect("the pinned catalogue exposes a provider");
    assert!(provider["id"].is_string());
    assert!(provider["models"].is_object());

    let sessions = app
        .clone()
        .oneshot(request(Method::GET, "/session", None))
        .await
        .expect("session list adapter responds");
    assert_eq!(sessions.status(), StatusCode::OK);
    let sessions = response_json(sessions).await;
    let sessions = sessions
        .as_array()
        .expect("the SDK receives a bare session array");
    assert!(
        sessions
            .iter()
            .any(|session| session["id"] == "ses_fixture")
    );
    assert!(sessions.iter().all(|session| session.get("data").is_none()));

    let created = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/session",
            Some(json!({
                "parentID": "ses_fixture",
                "title": "child from SDK",
                "agent": "explore",
                "model": {"providerID": "deepseek", "modelID": "deepseek-chat"}
            })),
        ))
        .await
        .expect("session create adapter responds");
    assert_eq!(created.status(), StatusCode::OK);
    let created = response_json(created).await;
    let child_id = created["id"]
        .as_str()
        .expect("the SDK receives a bare created session")
        .to_owned();
    assert_eq!(created["parentID"], "ses_fixture");
    assert_eq!(created["title"], "child from SDK");
    assert!(created.get("data").is_none());

    let fetched = app
        .clone()
        .oneshot(request(Method::GET, &format!("/session/{child_id}"), None))
        .await
        .expect("session get adapter responds");
    assert_eq!(fetched.status(), StatusCode::OK);
    let fetched = response_json(fetched).await;
    assert_eq!(fetched["id"], child_id);
    assert_eq!(fetched["projectID"], "global");

    let messages = app
        .clone()
        .oneshot(request(
            Method::GET,
            "/session/ses_fixture/message?limit=25",
            None,
        ))
        .await
        .expect("message list adapter responds");
    assert_eq!(messages.status(), StatusCode::OK);
    assert_eq!(response_json(messages).await, json!([]));

    let aborted = app
        .clone()
        .oneshot(request(Method::POST, "/session/ses_fixture/abort", None))
        .await
        .expect("abort adapter responds");
    assert_eq!(aborted.status(), StatusCode::OK);
    assert_eq!(response_json(aborted).await, json!(true));

    let summarized = app
        .oneshot(request(
            Method::POST,
            "/session/ses_fixture/summarize",
            Some(json!({"providerID": "deepseek", "modelID": "deepseek-chat"})),
        ))
        .await
        .expect("summarize adapter responds");
    assert_eq!(summarized.status(), StatusCode::OK);
    assert_eq!(response_json(summarized).await, json!(true));
    assert_eq!(fixture.executor.compact_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn compat_v1_backed_sdk_prompt_routes_preserve_sync_and_async_contracts() {
    let fixture = adapter_fixture();
    let app = fixture.app;

    let asynchronous = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/session/ses_fixture/prompt_async",
            Some(json!({
                "messageID": "msg_async_sdk",
                "agent": "explore",
                "model": {"providerID": "deepseek", "modelID": "deepseek-chat"},
                "parts": [{"type": "text", "text": "async request"}]
            })),
        ))
        .await
        .expect("async prompt adapter responds");
    assert_eq!(asynchronous.status(), StatusCode::NO_CONTENT);
    fixture.services.runs.wait_until_idle("ses_fixture").await;

    let synchronous = app
        .oneshot(request(
            Method::POST,
            "/session/ses_fixture/message",
            Some(json!({
                "messageID": "msg_sync_sdk",
                "agent": "build",
                "model": {"providerID": "deepseek", "modelID": "deepseek-chat"},
                "parts": [
                    {"type": "text", "text": "sync request"},
                    {"type": "file", "mime": "text/plain", "url": "data:text/plain,fixture"},
                    {"type": "agent", "name": "explore"}
                ]
            })),
        ))
        .await
        .expect("sync prompt adapter responds");
    assert_eq!(synchronous.status(), StatusCode::OK);
    let synchronous = response_json(synchronous).await;
    assert_eq!(synchronous["info"]["role"], "assistant");
    assert_eq!(synchronous["info"]["parentID"], "msg_sync_sdk");
    assert_eq!(synchronous["info"]["providerID"], "deepseek");
    assert_eq!(synchronous["info"]["modelID"], "deepseek-chat");
    assert_eq!(synchronous["parts"][0]["text"], "completed: sync request");

    let prompts = fixture.executor.prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0].message_id, "msg_async_sdk");
    assert_eq!(prompts[0].prompt, "async request");
    assert_eq!(prompts[0].agent.as_deref(), Some("explore"));
    assert_eq!(prompts[1].message_id, "msg_sync_sdk");
    assert_eq!(prompts[1].prompt, "sync request");
    assert_eq!(prompts[1].agent.as_deref(), Some("build"));
}

#[test]
fn compat_v1_every_route_has_a_recorded_callsite() {
    assert_eq!(
        V1_SURFACE.len(),
        20,
        "the measured plugin surface changed; re-run the capture in docs/v1-surface-capture.md"
    );
    for route in V1_SURFACE {
        assert!(
            !route.callsites.is_empty(),
            "{} {} is served with no recorded callsite, which is scope creep",
            route.method,
            route.path
        );
        assert!(
            !route.plugins.is_empty(),
            "{} {} names no calling plugin",
            route.method,
            route.path
        );
        for callsite in route.callsites {
            assert!(
                callsite.contains(':'),
                "callsite `{callsite}` for {} {} is not a file:line citation",
                route.method,
                route.path
            );
        }
    }
}

#[test]
fn compat_v1_every_route_exists_in_the_oracle_document() {
    let oracle: Value = serde_json::from_str(ORACLE).expect("checked-in oracle OpenAPI parses");
    let paths = oracle["paths"].as_object().expect("fixture paths object");
    assert!(
        paths.len() >= 160,
        "scanned only {} oracle paths; the fixture is not the document this asserts against",
        paths.len()
    );
    for route in V1_SURFACE {
        let item = paths
            .get(route.path)
            .unwrap_or_else(|| panic!("the oracle does not declare `{}`", route.path));
        assert!(
            item.get(route.method.as_openapi_key()).is_some(),
            "the oracle declares `{}` but not with {}",
            route.path,
            route.method
        );
        assert!(
            !route.path.starts_with("/api"),
            "{} is not a pre-/api path",
            route.path
        );
    }
}

#[test]
fn compat_v1_prefix_set_is_the_oracle_surface_minus_the_event_stream() {
    let oracle: Value = serde_json::from_str(ORACLE).expect("checked-in oracle OpenAPI parses");
    let paths = oracle["paths"].as_object().expect("fixture paths object");
    let mut expected = BTreeSet::new();
    let mut scanned = 0_usize;
    for path in paths.keys() {
        if path.starts_with("/api") {
            continue;
        }
        scanned += 1;
        let segment = path
            .split('/')
            .nth(1)
            .expect("an absolute path has a first segment");
        expected.insert(segment.to_owned());
    }
    assert!(
        scanned >= 100,
        "scanned only {scanned} pre-/api oracle paths; this assertion would pass vacuously"
    );
    assert!(
        expected.remove("event"),
        "the oracle no longer serves /event; the exclusion needs revisiting"
    );
    let actual = V1_PREFIXES
        .iter()
        .map(|prefix| (*prefix).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected, "V1_PREFIXES drifted from the oracle");
    assert!(
        !actual.contains("api"),
        "the accounting prefix set must never cover /api"
    );
}

#[test]
fn compat_v1_capture_document_records_every_served_route() {
    let capture = include_str!("../../../docs/v1-surface-capture.md");
    assert!(
        capture.len() > 4_000,
        "the capture artifact is {} bytes; it is not the document this asserts against",
        capture.len()
    );
    for route in V1_SURFACE {
        assert!(
            capture.contains(route.path),
            "docs/v1-surface-capture.md does not record `{}`",
            route.path
        );
        assert!(
            capture.contains(route.sdk_method),
            "docs/v1-surface-capture.md does not record `{}`",
            route.sdk_method
        );
    }
}

#[tokio::test]
async fn compat_v1_every_measured_route_is_reachable_and_never_answers_404() {
    let app = compat_app(CompatV1State::new());
    for route in V1_SURFACE {
        let method = match route.method {
            V1Method::Get => Method::GET,
            V1Method::Post => Method::POST,
            V1Method::Put => Method::PUT,
            V1Method::Patch => Method::PATCH,
        };
        let body = if route.path == "/tui/show-toast" {
            Some(json!({"message": "reachability", "variant": "info"}))
        } else {
            Some(json!({}))
        };
        let response = app
            .clone()
            .oneshot(request(method, &concrete_uri(route.path), body))
            .await
            .expect("a registered route responds");
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "measured route {} {} answered 404; the catch-all is shadowing it",
            route.method,
            route.path
        );
        assert_ne!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "measured route {} {} is registered under a different verb",
            route.method,
            route.path
        );
    }
}

/// The declared status of every route, against what the router really answers.
///
/// This is the executable half of the `v1-surface-unbacked` known gap: the gap's
/// text is rendered from [`v1_coverage`], which counts registered backends, so a
/// route whose declared `backing` drifts from its real behaviour would publish a false
/// coverage number in both the `501` body and `docs/compatibility-matrix.md`.
/// Driving each route closes that: gaining a backend without editing the table
/// fails here, and so does claiming one that does not exist.
#[tokio::test]
async fn compat_v1_declared_backing_matches_what_the_router_answers() {
    let app = compat_app(CompatV1State::new());
    let coverage = v1_coverage();
    assert_eq!(
        coverage.measured,
        V1_SURFACE.len(),
        "the coverage summary counts a different table than the one that is served"
    );
    assert_eq!(
        coverage.served + coverage.unbacked,
        coverage.measured,
        "every measured route is either served or unbacked"
    );

    let mut served = 0;
    let mut unbacked = 0;
    let mut redirected = 0;
    for route in V1_SURFACE {
        let method = match route.method {
            V1Method::Get => Method::GET,
            V1Method::Post => Method::POST,
            V1Method::Put => Method::PUT,
            V1Method::Patch => Method::PATCH,
        };
        let body = if route.path == TOAST_PATH {
            Some(json!({"message": "backing probe", "variant": "info"}))
        } else {
            Some(json!({}))
        };
        let response = app
            .clone()
            .oneshot(request(method, &concrete_uri(route.path), body))
            .await
            .expect("a registered route responds");
        let answered_501 = response.status() == StatusCode::NOT_IMPLEMENTED;
        assert_eq!(
            route.backing.is_served(),
            !answered_501,
            "{} {} declares backing `{}` but the router answered {}. Update the `backing` field \
             in V1_SURFACE in the same commit that changes the behaviour: the known gap \
             `v1-surface-unbacked` and the compatibility matrix both render their counts from it, \
             so leaving it stale publishes a coverage number the server contradicts.",
            route.method,
            route.path,
            route.backing,
            response.status(),
        );
        if route.backing.is_served() {
            served += 1;
        } else {
            unbacked += 1;
            if route.api_alternative.is_some() {
                redirected += 1;
            }
        }
    }

    assert_eq!(
        (served, unbacked, redirected),
        (coverage.served, coverage.unbacked, coverage.redirected),
        "the coverage summary disagrees with what driving every route observed"
    );
}

/// The `501` must tell a caller something that is true when they read it.
///
/// The wave-10 review found the hint reading "its backend lands in todos 57-62"
/// after all 161 implementation todos had closed, so it pointed a plugin author at
/// finished work. A todo number is a poor thing to publish in an error body at all:
/// it goes stale silently and means nothing outside this repository. This asserts
/// the body carries no such reference and names the served `/api` route instead.
#[tokio::test]
async fn compat_v1_seam_hint_names_a_live_alternative_and_never_a_plan_todo() {
    let app = compat_app(CompatV1State::new());
    for route in V1_SURFACE.iter().filter(|route| !route.backing.is_served()) {
        let method = match route.method {
            V1Method::Get => Method::GET,
            V1Method::Post => Method::POST,
            V1Method::Put => Method::PUT,
            V1Method::Patch => Method::PATCH,
        };
        let response = app
            .clone()
            .oneshot(request(
                method,
                &concrete_uri(route.path),
                Some(json!({"probe": "hint"})),
            ))
            .await
            .expect("an unbacked route responds");
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = response_json(response).await;
        let rendered = body.to_string();
        let hint = body["error"]["hint"]
            .as_str()
            .expect("every 501 carries a hint")
            .to_owned();

        if let Some(citation) = plan_todo_citation(&rendered) {
            panic!(
                "the 501 for {} {} cites plan work (`{citation}`), which is exactly the reference \
                 that went stale when todos 57-62 closed. Tell the caller what to call instead, \
                 not which todo owns the backend: {rendered}",
                route.method, route.path,
            );
        }
        let lowered = rendered.to_lowercase();
        assert!(
            !lowered.contains("lands in"),
            "the 501 for {} {} still promises future work: {rendered}",
            route.method,
            route.path,
        );

        match route.api_alternative {
            Some(alternative) => {
                assert!(
                    hint.contains(alternative),
                    "{} {} records `{alternative}` as its served /api equivalent, but its hint \
                     does not name it, so the caller is not told what works: {hint}",
                    route.method,
                    route.path,
                );
                assert_eq!(
                    body["error"]["apiAlternative"], alternative,
                    "the structured field and the prose hint must name the same route"
                );
            }
            None => {
                assert!(
                    body["error"]["apiAlternative"].is_null(),
                    "{} {} declares no /api alternative but the body advertises one",
                    route.method,
                    route.path,
                );
                assert!(
                    hint.contains("no served /api equivalent"),
                    "{} {} has no alternative, so its hint must say so plainly rather than imply \
                     one exists: {hint}",
                    route.method,
                    route.path,
                );
            }
        }
        assert_eq!(body["error"]["backing"], route.backing.as_str());
        assert!(
            body["error"]["surfaceCoverage"]
                .as_str()
                .is_some_and(|line| line.contains(&v1_coverage().unbacked.to_string())),
            "the 501 should carry the surface's real coverage, counted at response time: \
             {rendered}"
        );
    }
}

#[tokio::test]
async fn compat_v1_seam_route_names_its_sdk_method_and_callers() {
    let app = compat_app(CompatV1State::new());
    let response = app
        .oneshot(request(Method::GET, "/session/status", None))
        .await
        .expect("the status seam responds");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "not_implemented");
    assert_eq!(body["error"]["sdkMethod"], "client.session.status");
    assert_eq!(body["error"]["route"], "GET /session/status");
    let callers = body["error"]["callers"]
        .as_array()
        .expect("the seam names its callers");
    assert!(
        callers
            .iter()
            .any(|caller| caller.as_str() == Some("@sunerpy/oh-my-openagent@4.21.0")),
        "the seam must name the plugin that needs the backend: {callers:?}"
    );
}

#[tokio::test]
async fn compat_v1_auth_set_seam_never_echoes_the_credential_it_was_sent() {
    let app = compat_app(CompatV1State::new());
    let response = app
        .oneshot(request(
            Method::PUT,
            "/auth/anthropic",
            Some(json!({"type": "api", "key": "sk-do-not-echo-me"})),
        ))
        .await
        .expect("the auth seam responds");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body = serde_json::to_string(&response_json(response).await).expect("body re-serializes");
    assert!(
        !body.contains("sk-do-not-echo-me"),
        "the credential leaked into the response body: {body}"
    );
}

#[tokio::test]
async fn compat_v1_show_toast_records_the_toast_and_answers_true_with_no_tui() {
    let state = CompatV1State::new();
    assert!(!state.toast_display_attached());
    let app = compat_app(state.clone());
    let response = app
        .oneshot(request(
            Method::POST,
            "/tui/show-toast",
            Some(json!({"title": "Kiro", "message": "signed in", "variant": "success"})),
        ))
        .await
        .expect("the toast route responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await, json!(true));

    let retained = state.retained_toasts();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].message, "signed in");
    assert_eq!(retained[0].variant, "success");
    assert_eq!(retained[0].title.as_deref(), Some("Kiro"));
    assert_eq!(state.accepted_toasts(), 1);
}

#[tokio::test]
async fn compat_v1_show_toast_is_lenient_about_variant_and_strict_about_message() {
    let state = CompatV1State::new();
    let app = compat_app(state.clone());

    let lenient = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/tui/show-toast",
            Some(json!({"message": "no variant supplied", "extra": "ignored"})),
        ))
        .await
        .expect("the toast route responds");
    assert_eq!(
        lenient.status(),
        StatusCode::OK,
        "a variant-less toast must not fail; three of three plugins depend on this route"
    );
    assert_eq!(state.retained_toasts()[0].variant, "info");

    let unusable = app
        .oneshot(request(
            Method::POST,
            "/tui/show-toast",
            Some(json!({"variant": "info"})),
        ))
        .await
        .expect("the toast route responds");
    assert_eq!(unusable.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(unusable).await["error"]["code"],
        "invalid_request"
    );
    assert_eq!(
        state.accepted_toasts(),
        1,
        "the unusable toast was recorded"
    );
}

#[derive(Debug, Default)]
struct RecordingTui {
    shown: Mutex<Vec<String>>,
}

impl ToastForwarder for RecordingTui {
    fn show(&self, toast: &Toast) {
        self.shown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(toast.message.clone());
    }
}

#[tokio::test]
async fn compat_v1_attached_display_receives_the_toast_without_a_route_change() {
    let tui = Arc::new(RecordingTui::default());
    let state = CompatV1State::new().with_toast_forwarder(Arc::clone(&tui) as Arc<_>);
    assert!(state.toast_display_attached());
    let response = compat_app(state.clone())
        .oneshot(request(
            Method::POST,
            "/tui/show-toast",
            Some(json!({"message": "forwarded", "variant": "info"})),
        ))
        .await
        .expect("the toast route responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        *tui.shown.lock().unwrap_or_else(PoisonError::into_inner),
        vec!["forwarded".to_owned()]
    );
    assert_eq!(
        state.accepted_toasts(),
        1,
        "recording continues after a display attaches, or diagnostics go blind"
    );
}

#[tokio::test]
async fn compat_v1_unimplemented_path_returns_an_actionable_404_and_bumps_the_counter() {
    let state = CompatV1State::new();
    let app = compat_app(state.clone());
    assert_eq!(state.unknown_routes().total(), 0);

    let response = app
        .clone()
        .oneshot(request(Method::GET, "/session/ses_fixture/diff", None))
        .await
        .expect("the accounting catch-all responds");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "unimplemented_v1_route");
    assert_eq!(body["error"]["path"], "/session/ses_fixture/diff");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("/session/ses_fixture/diff")),
        "the 404 body must name the path: {body}"
    );
    let action = body["error"]["action"].as_str().expect("an action string");
    assert!(
        action.contains("docs/v1-surface-capture.md") && action.contains("V1_SURFACE"),
        "the 404 must tell the operator to re-run the capture: {action}"
    );
    assert_eq!(body["error"]["diagnostics"], V1_DIAGNOSTICS_PATH);
    assert_eq!(body["error"]["unaccountedRequests"], 1);

    assert_eq!(state.unknown_routes().total(), 1);
    assert_eq!(
        state
            .unknown_routes()
            .count_for("/session/ses_fixture/diff"),
        1
    );

    for _ in 0..3 {
        let repeat = app
            .clone()
            .oneshot(request(Method::GET, "/session/ses_fixture/diff", None))
            .await
            .expect("the accounting catch-all responds");
        assert_eq!(repeat.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(state.unknown_routes().total(), 4);
    assert_eq!(
        state
            .unknown_routes()
            .count_for("/session/ses_fixture/diff"),
        4
    );
}

#[tokio::test]
async fn compat_v1_accounting_covers_every_prefix_including_bare_and_nested_paths() {
    let state = CompatV1State::new();
    let app = compat_app(state.clone());
    let implemented = V1_SURFACE
        .iter()
        .map(|route| route.path)
        .collect::<BTreeSet<_>>();

    let mut expected = 0_u64;
    for prefix in V1_PREFIXES {
        let bare = format!("/{prefix}");
        if !implemented.contains(bare.as_str()) {
            let response = app
                .clone()
                .oneshot(request(Method::GET, &bare, None))
                .await
                .expect("the accounting catch-all responds");
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "bare prefix {bare} is unaccounted for"
            );
            expected += 1;
        }
        // Three trailing segments, so no measured template can absorb the probe —
        // `/auth/{providerID}` and `/session/{sessionID}/message` are shorter, and
        // `/provider/{providerID}/oauth/authorize` needs two literal segments.
        let nested = format!("/{prefix}/definitely/not/measured");
        let response = app
            .clone()
            .oneshot(request(Method::GET, &nested, None))
            .await
            .expect("the accounting catch-all responds");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "nested path {nested} is unaccounted for"
        );
        expected += 1;
    }
    assert_eq!(
        state.unknown_routes().total(),
        expected,
        "some prefix answered without being counted"
    );
    assert!(expected >= 43, "only {expected} accounted paths exercised");
}

#[tokio::test]
async fn compat_v1_unmeasured_verb_on_a_measured_path_is_accounted_too() {
    let state = CompatV1State::new();
    let response = compat_app(state.clone())
        .oneshot(request(Method::DELETE, "/auth/anthropic", None))
        .await
        .expect("the method accounting responds");
    assert_eq!(
        response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "the path is measured, so 404 would misreport it"
    );
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "unimplemented_v1_operation");
    assert_eq!(body["error"]["path"], "/auth/anthropic");
    assert_eq!(body["error"]["method"], "DELETE");
    assert!(
        body["error"]["action"]
            .as_str()
            .is_some_and(|action| action.contains("docs/v1-surface-capture.md")),
        "an unmeasured verb must also point at the capture: {body}"
    );
    assert_eq!(state.unknown_routes().total(), 1);
    assert_eq!(
        state.unknown_routes().count_for("DELETE /auth/anthropic"),
        1,
        "an unmeasured operation is keyed by verb and path: {:?}",
        state.unknown_routes().breakdown()
    );
}

#[tokio::test]
async fn compat_v1_diagnostics_surface_the_counter_and_the_toast_sink() {
    let state = CompatV1State::new();
    let app = compat_app(state.clone());
    app.clone()
        .oneshot(request(Method::GET, "/file/content", None))
        .await
        .expect("the accounting catch-all responds");
    app.clone()
        .oneshot(request(
            Method::POST,
            "/tui/show-toast",
            Some(json!({"message": "diagnostic", "variant": "warning"})),
        ))
        .await
        .expect("the toast route responds");

    let response = app
        .oneshot(request(Method::GET, V1_DIAGNOSTICS_PATH, None))
        .await
        .expect("diagnostics respond");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    let coverage = v1_coverage();
    assert_eq!(body["v1Surface"]["registeredRoutes"], 20);
    assert_eq!(body["v1Surface"]["servedRoutes"], coverage.served);
    assert_eq!(body["v1Surface"]["unbackedRoutes"], coverage.unbacked);
    assert_eq!(
        body["v1Surface"]["unbackedWithApiAlternative"],
        coverage.redirected
    );
    assert_eq!(
        body["v1Surface"]["coverage"],
        coverage.summary(),
        "diagnostics must publish the surface's real coverage, not just its route count"
    );
    assert_eq!(body["unknownRoutes"]["total"], 1);
    assert_eq!(body["unknownRoutes"]["paths"]["/file/content"], 1);
    assert_eq!(body["unknownRoutes"]["overflowedSightings"], 0);
    assert_eq!(body["toasts"]["accepted"], 1);
    assert_eq!(body["toasts"]["displayAttached"], false);
    assert_eq!(body["toasts"]["latest"]["message"], "diagnostic");
    assert_eq!(body["toasts"]["latest"]["variant"], "warning");
}

#[tokio::test]
async fn compat_v1_unknown_path_breakdown_is_bounded_while_the_total_stays_exact() {
    let state = CompatV1State::new();
    let app = compat_app(state.clone());
    for index in 0..200_u32 {
        app.clone()
            .oneshot(request(Method::GET, &format!("/file/scan-{index}"), None))
            .await
            .expect("the accounting catch-all responds");
    }
    assert_eq!(
        state.unknown_routes().total(),
        200,
        "the total must stay exact even when the breakdown is capped"
    );
    let breakdown = state.unknown_routes().breakdown();
    assert_eq!(
        breakdown.len(),
        64,
        "the per-path breakdown must be bounded against a path scanner"
    );
    assert_eq!(state.unknown_routes().overflowed(), 200 - 64);
}

#[tokio::test]
async fn compat_v1_catch_all_does_not_shadow_the_api_surface_or_the_event_stream() {
    let state = CompatV1State::new();
    let app = assembled_app(state.clone());

    let health = app
        .clone()
        .oneshot(request(Method::GET, "/api/health", None))
        .await
        .expect("the API health route responds");
    assert_eq!(
        health.status(),
        StatusCode::OK,
        "the v1 catch-all shadowed /api/health"
    );
    assert_eq!(response_json(health).await, json!({"healthy": true}));

    for path in ["/doc", "/openapi.json", "/api/doc"] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path, None))
            .await
            .expect("the document alias responds");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "alias {path} was shadowed"
        );
    }

    let api_session = app
        .clone()
        .oneshot(request(Method::GET, "/api/session?project=global", None))
        .await
        .expect("the API session route responds");
    assert_eq!(
        api_session.status(),
        StatusCode::OK,
        "the v1 /session routes shadowed /api/session"
    );

    let core_health = app
        .clone()
        .oneshot(request(Method::GET, "/health", None))
        .await
        .expect("the core health route responds");
    assert_eq!(core_health.status(), StatusCode::OK);

    let stream = app
        .clone()
        .oneshot(request(Method::GET, "/event?sessionID=ses_fixture", None))
        .await
        .expect("the event stream responds");
    assert_eq!(
        stream.status(),
        StatusCode::OK,
        "the v1 catch-all shadowed the /event stream"
    );

    assert_eq!(
        state.unknown_routes().total(),
        0,
        "an /api, /doc, /health or /event request was counted as an unknown v1 route"
    );
}

#[tokio::test]
async fn compat_v1_installed_auth_plugin_lifecycles_are_answered_without_a_single_404() {
    let state = CompatV1State::new();
    let app = assembled_app(state.clone());

    // The ordered call set each installed auth plugin issues, from the capture in
    // docs/v1-surface-capture.md. Every one must reach a registered route.
    let antigravity = [
        (Method::PUT, "/auth/anthropic", Some(json!({"type": "api"}))),
        (
            Method::POST,
            "/session/ses_fixture/message",
            Some(json!({"parts": []})),
        ),
        (Method::GET, "/session/ses_fixture/message", None),
        (Method::POST, "/session/ses_fixture/abort", Some(json!({}))),
        (
            Method::POST,
            "/log",
            Some(json!({"service": "antigravity", "level": "info", "message": "hi"})),
        ),
        (
            Method::POST,
            "/tui/show-toast",
            Some(json!({"message": "antigravity ready", "variant": "success"})),
        ),
    ];
    let kiro = [
        (
            Method::POST,
            "/provider/kiro-auth/oauth/authorize",
            Some(json!({"method": 0})),
        ),
        (
            Method::POST,
            "/provider/kiro-auth/oauth/callback",
            Some(json!({"method": 0})),
        ),
        (
            Method::POST,
            "/tui/show-toast",
            Some(json!({"message": "kiro ready", "variant": "info"})),
        ),
    ];

    for (method, path, body) in antigravity.into_iter().chain(kiro) {
        let response = app
            .clone()
            .oneshot(request(method.clone(), path, body))
            .await
            .expect("a plugin call reaches a registered route");
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {path} is a measured plugin call and answered 404"
        );
        assert_ne!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path} is registered under the wrong verb"
        );
        assert_ne!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{method} {path} failed instead of answering definitively"
        );
    }

    assert_eq!(
        state.unknown_routes().total(),
        0,
        "an installed plugin's call set hit an unmeasured route: {:?}",
        state.unknown_routes().breakdown()
    );
    assert_eq!(
        state.accepted_toasts(),
        2,
        "both auth plugins' toasts must be delivered to the sink"
    );
}
