use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zuno_mcp::{RemoteClient, RemoteConnect, RemoteError, RemoteTransport};

use crate::remote_support::{initialize_result, remote_config, sse_event};

#[tokio::test]
async fn remote_streamable_http_accepts_plain_json_and_negotiates_the_server_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": initialize_result("2025-03-26", "json-server"),
                })),
        )
        .mount(&server)
        .await;

    let outcome = RemoteClient::connect("json", &remote_config(format!("{}/mcp", server.uri())))
        .await
        .expect("streamable HTTP connects");
    let RemoteConnect::Connected(client) = outcome else {
        panic!("an unauthenticated server must connect")
    };
    assert_eq!(client.transport(), RemoteTransport::StreamableHttp);
    assert_eq!(client.initialization().protocol_version, "2025-03-26");
    assert_eq!(client.initialization().server_info.name, "json-server");
    let requests = server.received_requests().await.expect("request journal");
    let initialize: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("initialize request JSON");
    assert_eq!(initialize["params"]["clientInfo"]["name"], "zuno");
}

/// The non-SSE branch is selected by a `Content-Type` header alone, so an unbounded
/// read there is one header away from bypassing the SSE event cap — and an OOM kill
/// takes the whole agent down, not just the MCP call.
#[tokio::test]
async fn remote_streamable_http_refuses_a_body_past_the_message_bound() {
    // The production constant, not a copy of its value: a test that restates the
    // bound keeps passing after the bound moves, and then proves nothing about the
    // build it ran against.
    const BOUND: usize = zuno_mcp::MAX_FRAME_BYTES;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_bytes(vec![b'x'; BOUND + 1]),
        )
        .mount(&server)
        .await;
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    // No fallback: the streamable branch is what is under test, and a second
    // multi-megabyte attempt proves nothing further.
    config.streamable_http_only = true;

    let error = RemoteClient::connect("oversized", &config)
        .await
        .expect_err("a body past the message bound must be refused");
    let RemoteError::Protocol { message, .. } = &error else {
        panic!("an over-long body is a framing violation, not a transport hiccup: {error:?}")
    };
    assert!(
        message.contains(&BOUND.to_string()) && message.contains("past the"),
        "the refusal must name the bound it enforced, not a JSON parse position: {message}"
    );
}

/// The reviewer's input: a `tools/call` answered with a reply that stops mid-JSON.
///
/// `{"jsonrpc":"2.0","id":2,"result":{"content":[{"typ` arrives whole, with a 200 and a
/// JSON content type, and does not parse. The request had already reached the server,
/// so the tool may well have run — reporting that as a definite permanent failure is
/// the direction that invites a duplicate side effect, so it reports the uncertain
/// class instead. A read-only method mutated nothing, so it keeps the permanent framing
/// error that names the fault.
#[tokio::test]
async fn remote_an_unreadable_reply_to_a_side_effecting_call_is_uncertain_not_a_definite_failure() {
    const TRUNCATED: &str = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"typ"#;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains(r#""method":"initialize""#))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": initialize_result("2025-03-26", "truncating-server"),
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    for outstanding in [r#""method":"tools/call""#, r#""method":"resources/read""#] {
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(outstanding))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(TRUNCATED),
            )
            .mount(&server)
            .await;
    }

    let outcome = RemoteClient::connect(
        "truncating",
        &remote_config(format!("{}/mcp", server.uri())),
    )
    .await
    .expect("the handshake itself is well-formed");
    let RemoteConnect::Connected(client) = outcome else {
        panic!("an unauthenticated server must connect")
    };

    let error = client
        .call_tool("shoot", serde_json::Map::new())
        .await
        .expect_err("an unparseable reply cannot be reported as a successful call");
    assert!(
        matches!(&error, RemoteError::Timeout { .. }) && error.is_timeout(),
        "a call that may have taken effect and was answered unreadably has an unknown \
         outcome, not a definite permanent failure: {error:?}"
    );

    let error = client
        .read_resource("mcp://doc")
        .await
        .expect_err("an unparseable reply cannot be reported as resource contents");
    let RemoteError::Protocol { message, .. } = &error else {
        panic!(
            "a read-only method mutated nothing, so the framing fault is the whole diagnosis: {error:?}"
        )
    };
    assert!(
        message.contains("response body was not JSON"),
        "the read-only refusal must still name the fault: {message}"
    );
}

#[tokio::test]
async fn remote_falls_back_from_streamable_http_to_legacy_sse_in_that_order() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(405))
        .expect(1)
        .mount(&server)
        .await;
    let stream = format!(
        "{}{}",
        sse_event("endpoint", "/messages"),
        sse_event(
            "message",
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": initialize_result("2024-11-05", "sse-server"),
            })
            .to_string(),
        )
    );
    Mock::given(method("GET"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(stream),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    let outcome = RemoteClient::connect("legacy", &remote_config(format!("{}/mcp", server.uri())))
        .await
        .expect("legacy SSE connects after fallback");
    let RemoteConnect::Connected(client) = outcome else {
        panic!("legacy SSE must connect")
    };
    assert_eq!(client.transport(), RemoteTransport::Sse);
    let requests = server.received_requests().await.expect("request journal");
    let observed: Vec<_> = requests
        .iter()
        .take(3)
        .map(|request| (request.method.as_str(), request.url.path()))
        .collect();
    assert_eq!(
        observed,
        vec![("POST", "/mcp"), ("GET", "/mcp"), ("POST", "/messages")]
    );
}

#[tokio::test]
async fn remote_static_headers_are_sent_on_protocol_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": initialize_result("2024-11-05", "header-server"),
                })),
        )
        .mount(&server)
        .await;
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    config.headers = Some(BTreeMap::from([(
        "X-Static-Probe".to_owned(),
        "present".to_owned(),
    )]));
    let outcome = RemoteClient::connect("headers", &config)
        .await
        .expect("header server connects");
    assert!(matches!(outcome, RemoteConnect::Connected(_)));
    let requests = server.received_requests().await.expect("request journal");
    assert!(requests.iter().all(|request| {
        request
            .headers
            .get("x-static-probe")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "present")
    }));
}

#[tokio::test]
async fn remote_timeout_bounds_both_transport_attempts() {
    let server = MockServer::start().await;
    Mock::given(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .insert_header("content-type", "application/json")
                .set_body_json(json!({})),
        )
        .mount(&server)
        .await;
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    config.timeout = NonZeroU32::new(40);
    let started = Instant::now();
    let error = RemoteClient::connect("timeout", &config)
        .await
        .expect_err("both attempts must observe the configured deadline");
    assert!(error.is_timeout(), "unexpected error: {error:?}");
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn remote_oauth_false_turns_a_401_into_an_error_without_discovery() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    config.oauth = Some(zuno_config::schema::mcp::McpOauth::Disabled(
        serde_json::from_value(serde_json::Value::Bool(false)).expect("literal false"),
    ));
    let error = RemoteClient::connect("disabled", &config)
        .await
        .expect_err("explicit oauth:false must suppress automatic OAuth");
    assert!(matches!(error, RemoteError::OAuthDisabled { .. }));
    let requests = server.received_requests().await.expect("request journal");
    assert_eq!(requests.len(), 1);
}
