use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use oc_db::Pool;
use oc_db::artifact_gc::ArtifactGcPaths;
use oc_db::session::SessionCreate;
use oc_paths::DbLocation;
use oc_pty::{CreateInput, PtyId, TicketScope};
use oc_server::api::{self, ApiState};
use oc_server::{Delivery, EventService, NewEvent, ServerBuilder, ServerConfig, ServerServices};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::ServiceExt;

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
fn api_openapi_contains_every_owned_oracle_operation() {
    let oracle: Value = serde_json::from_str(include_str!(
        "../../../.omo/fixtures/oracle-openapi-1.18.12.json"
    ))
    .expect("checked-in oracle OpenAPI parses");
    let generated = api::openapi();
    let expected = fixture_operations(&oracle);
    assert_eq!(
        expected.len(),
        58,
        "the measured task-owned surface changed"
    );

    let actual = fixture_operations(&generated);
    let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "generated OpenAPI is missing {missing:?}"
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
    assert_eq!(messages["data"][0]["id"], "msg_0119");
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
async fn api_unbacked_endpoint_is_an_explicit_gap_not_a_501_compatibility_claim() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let response = api_app(state)
        .oneshot(request(Method::GET, "/api/integration", None))
        .await
        .expect("registered endpoint responds");
    let status = response.status();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "backend_unavailable");
    assert_eq!(
        body["error"]["message"],
        "backend unavailable for GET /api/integration"
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

    let created = app
        .oneshot(request(
            Method::POST,
            "/api/pty",
            Some(json!({"command":"sh","args":["-c","exit 0"]})),
        ))
        .await
        .expect("PTY create responds");
    assert_eq!(created.status(), StatusCode::OK);
    let body = response_json(created).await;
    assert_eq!(body["data"]["command"], "sh");
}

#[tokio::test]
async fn api_pty_connect_requires_a_single_use_unexpired_ticket_without_echoing_it() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let info = state
        .pty()
        .create(CreateInput {
            command: Some("sh".to_owned()),
            args: Some(vec!["-c".to_owned(), "sleep 30".to_owned()]),
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
        .insert("x-opencode-ticket", "1".parse().expect("valid header"));
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

    let expired = state.pty().tickets().issue_at(
        TicketScope::for_session(PtyId::from_raw(&pty_id)),
        Instant::now() - Duration::from_secs(61),
    );
    let expired_response = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/pty/{pty_id}/connect?ticket={}", expired.ticket),
            None,
        ))
        .await
        .expect("expired connect rejection responds");
    assert_eq!(expired_response.status(), StatusCode::FORBIDDEN);
    assert!(
        !response_json(expired_response)
            .await
            .to_string()
            .contains(&expired.ticket)
    );

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
        .oneshot(request(
            Method::GET,
            &format!("/api/pty/{pty_id}/connect?ticket={ticket}"),
            None,
        ))
        .await
        .expect("ticket replay responds");
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);
    assert!(!response_json(replay).await.to_string().contains(&ticket));
}

#[tokio::test]
async fn api_pty_websocket_ticket_streams_real_terminal_input_and_output() {
    let directory = tempfile::tempdir().expect("temporary PTY directory");
    let state = ApiState::memory(directory.path().to_string_lossy())
        .expect("in-memory API state initializes");
    let info = state
        .pty()
        .create(CreateInput {
            command: Some("/bin/sh".to_owned()),
            args: Some(vec!["-i".to_owned()]),
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
        .header("x-opencode-ticket", "1")
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

    write_masked_text_frame(&mut socket, "printf 'WS-READY\\n'\n").await;
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
            oc_db::session_prune::ProgressPhase::Selecting,
            oc_db::session_prune::ProgressPhase::Selected,
            oc_db::session_prune::ProgressPhase::Database,
            oc_db::session_prune::ProgressPhase::Artifacts,
            oc_db::session_prune::ProgressPhase::Completed,
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
