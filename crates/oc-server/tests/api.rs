use std::collections::BTreeSet;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use oc_db::session::SessionCreate;
use oc_server::api::{self, ApiState};
use oc_server::{Delivery, ServerBuilder, ServerConfig, ServerServices};
use serde_json::{Value, json};
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
async fn api_unbacked_endpoint_is_an_explicit_gap_not_a_501_compatibility_claim() {
    // `/api/integration` used to stand in here and is now backed, so the guarantee
    // is asserted against an operation that is still unbacked (todo 128 owns the
    // permission group). The property under test is unchanged: a registered route
    // with no backend answers an explicit 503 naming the gap, never a 501.
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let response = api_app(state)
        .oneshot(request(Method::GET, "/api/permission/saved", None))
        .await
        .expect("registered endpoint responds");
    let status = response.status();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "backend_unavailable");
    assert_eq!(
        body["error"]["message"],
        "backend unavailable for GET /api/permission/saved"
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
fn isolated_env(root: &std::path::Path) -> oc_paths::Env {
    oc_paths::Env::empty()
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
}

#[tokio::test]
async fn api_skill_reports_the_v2_builtin_location_and_description() {
    let (root, directory) = fs_fixture();
    let state = ApiState::memory(directory)
        .expect("API state")
        .with_env(isolated_env(root.path()));
    let (status, body) = fs_body(state, "/api/skill").await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&body).expect("skill body is JSON");
    let builtin = json["data"]
        .as_array()
        .expect("skills are an array")
        .iter()
        .find(|entry| entry["name"] == "customize-opencode")
        .expect("the built-in skill is registered");
    assert_eq!(
        builtin["location"], "/builtin/customize-opencode.md",
        "the V2 surface reports the plugin's absolute location, not the v1 `<built-in>` sentinel"
    );
    assert!(
        builtin["description"]
            .as_str()
            .expect("description is a string")
            .contains("commands, skills, plugins"),
        "the V2 description lists `commands`, which the v1 copy does not"
    );
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
        .join("../oc-llm/tests/fixtures/models-dev-pinned.json")
        .canonicalize()
        .expect("the pinned catalogue fixture exists");
    let env = isolated_env(root.path())
        .with(
            "OPENCODE_MODELS_PATH",
            fixture.to_string_lossy().into_owned(),
        )
        .with("OPENCODE_DISABLE_MODELS_FETCH", "1")
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
            "OpenCode",
            "Zhipu AI"
        ],
        "integrations sort case-insensitively, which puts `openai` before `OpenCode`"
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
}
