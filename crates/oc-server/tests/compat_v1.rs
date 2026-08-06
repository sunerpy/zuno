use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use oc_paths::DbLocation;
use oc_server::api::{self, ApiState};
use oc_server::compat_v1::{V1_DIAGNOSTICS_PATH, V1Method};
use oc_server::{
    CompatV1State, DEFAULT_EVENT_SUBSCRIBER_CAPACITY, EventService, ServerBuilder, ServerConfig,
    Toast, ToastForwarder, V1_PREFIXES, V1_SURFACE, compat_v1_router, events_router,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const ORACLE: &str = include_str!("../../../.omo/fixtures/oracle-openapi-1.18.12.json");

fn compat_app(state: CompatV1State) -> Router {
    ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_routes(compat_v1_router(state))
        .router()
}

/// The router every other surface is merged into, so a shadowing regression in
/// the v1 catch-all fails here rather than in someone's hands-on QA.
fn assembled_app(state: CompatV1State) -> Router {
    let pool =
        Arc::new(oc_db::Pool::open(&DbLocation::Memory).expect("in-memory event database opens"));
    let events = EventService::new(pool, DEFAULT_EVENT_SUBSCRIBER_CAPACITY);
    let api_state = ApiState::memory("/repo").expect("in-memory API state initializes");
    ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
        .with_routes(
            api::router(api_state)
                .merge(events_router(events))
                .merge(compat_v1_router(state)),
        )
        .router()
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

#[tokio::test]
async fn compat_v1_seam_route_names_its_sdk_method_and_callers() {
    let app = compat_app(CompatV1State::new());
    let response = app
        .oneshot(request(
            Method::POST,
            "/session/ses_fixture/abort",
            Some(json!({})),
        ))
        .await
        .expect("the abort seam responds");
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "not_implemented");
    assert_eq!(body["error"]["sdkMethod"], "client.session.abort");
    assert_eq!(body["error"]["route"], "POST /session/{sessionID}/abort");
    let callers = body["error"]["callers"]
        .as_array()
        .expect("the seam names its callers");
    assert!(
        callers
            .iter()
            .any(|caller| caller.as_str() == Some("opencode-antigravity-auth@1.6.0")),
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
    assert_eq!(body["v1Surface"]["implementedRoutes"], 20);
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
