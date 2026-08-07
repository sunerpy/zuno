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
    let mut expected = fixture_operations(&oracle);
    expected.remove(&("/api/event".to_owned(), "get".to_owned()));
    expected.remove(&(
        "/api/session/{sessionID}/event".to_owned(),
        "get".to_owned(),
    ));
    assert_eq!(
        expected.len(),
        56,
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
async fn api_unbacked_endpoint_is_explicit_instead_of_fabricating_data() {
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let response = api_app(state)
        .oneshot(request(Method::GET, "/api/integration", None))
        .await
        .expect("registered endpoint responds");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "not_implemented");
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
