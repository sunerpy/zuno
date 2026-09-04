use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zuno_mcp::{
    MAX_CONSECUTIVE_UNDECODABLE_FRAMES, RemoteClient, RemoteConnect, RemoteError, RemoteTransport,
};

use crate::remote_support::{initialize_result, remote_config, sse_event};

/// A legacy-SSE server whose whole event stream is scripted up front.
///
/// The streamable-HTTP attempt is refused with 405 so `connect` falls back to the
/// legacy transport, which is the path under test.
async fn legacy_server(stream: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
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
    server
}

fn initialize_event() -> String {
    sse_event(
        "message",
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": initialize_result("2024-11-05", "sse-server"),
        })
        .to_string(),
    )
}

/// A `message` event whose data is not JSON carries no id, so it can have answered
/// no request. The reader used to fail every in-flight call on it — here, the
/// handshake that the very next event answers.
#[tokio::test]
async fn legacy_sse_stray_non_json_event_does_not_fail_the_pending_request() {
    let stream = format!(
        "{}{}{}",
        sse_event("endpoint", "/messages"),
        // What a proxy error page or a misrouted keep-alive looks like once it
        // reaches the client as a message event.
        sse_event("message", "<html>gateway timeout</html>"),
        initialize_event(),
    );
    let server = legacy_server(stream).await;

    let outcome = RemoteClient::connect("legacy", &remote_config(format!("{}/mcp", server.uri())))
        .await
        .expect("an event that belongs to no id must not fail the request in flight");
    let RemoteConnect::Connected(client) = outcome else {
        panic!("an unauthenticated server must connect")
    };
    assert_eq!(client.transport(), RemoteTransport::Sse);
    assert_eq!(client.initialization().server_info.name, "sse-server");
    client.close().await;
}

/// The other half of the rule: a stream that never carries a decodable event is not
/// noisy, it is not MCP, and the caller must learn that as a protocol failure rather
/// than by waiting out its deadline.
#[tokio::test]
async fn legacy_sse_run_of_non_json_events_fails_the_pending_request() {
    // One event past the shared undecodable-frame bound. Taken from the production
    // constant, because a test that restates the number keeps passing after the bound
    // moves and then proves nothing about the build it ran against.
    let noise = sse_event("message", "<html>gateway timeout</html>")
        .repeat(MAX_CONSECUTIVE_UNDECODABLE_FRAMES + 1);
    let stream = format!(
        "{}{noise}{}",
        sse_event("endpoint", "/messages"),
        initialize_event(),
    );
    let server = legacy_server(stream).await;
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    // Short enough that a bound that never trips shows up as a deadline instead of
    // a slow pass.
    config.timeout = NonZeroU32::new(5_000);

    let error = RemoteClient::connect("legacy", &config)
        .await
        .expect_err("a stream with no decodable event must fail the handshake");
    let RemoteError::Fallback { sse, .. } = &error else {
        panic!("both transports were attempted, so the failure is a pair: {error:?}")
    };
    assert!(
        matches!(sse.as_ref(), RemoteError::Protocol { transport, .. }
            if *transport == RemoteTransport::Sse),
        "a peer that never frames a JSON-RPC message is a protocol violation: {sse:?}"
    );
    assert!(
        !error.is_timeout(),
        "the bound must report the violation, not leave the call waiting: {error:?}"
    );
}

/// A stream that framed one JSON-RPC message has proven it speaks the protocol, and no
/// quantity of later noise may end it.
///
/// The run bound used to apply to every stream, so a legacy server that emits a
/// progress notification and then a burst of proxy noise lost its reader mid-handshake
/// — with the response to the call in flight already scripted behind the noise.
#[tokio::test]
async fn legacy_sse_noise_after_a_decodable_event_never_ends_the_stream() {
    let progress = sse_event(
        "message",
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": { "level": "info", "data": "starting" },
        })
        .to_string(),
    );
    let noise = sse_event("message", "<html>gateway timeout</html>")
        .repeat(MAX_CONSECUTIVE_UNDECODABLE_FRAMES * 2);
    let stream = format!(
        "{}{progress}{noise}{}",
        sse_event("endpoint", "/messages"),
        initialize_event(),
    );
    let server = legacy_server(stream).await;
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    // Long enough that a reader ended by the noise shows up as the handshake failing,
    // not as this test hanging.
    config.timeout = NonZeroU32::new(5_000);

    let outcome = RemoteClient::connect("legacy", &config)
        .await
        .expect("noise on a stream that already framed JSON-RPC must not end it");
    let RemoteConnect::Connected(client) = outcome else {
        panic!("an unauthenticated server must connect")
    };
    assert_eq!(client.transport(), RemoteTransport::Sse);
    assert_eq!(client.initialization().server_info.name, "sse-server");
    client.close().await;
}

/// The legacy reader is the only thing that can deliver a response, so once the event
/// stream ends the connection is finished — however happily `POST /messages` keeps
/// returning 202.
///
/// Without the recorded exit the POST succeeds, the call waits out the whole per-server
/// deadline, and the failure comes back as a retryable timeout against a connection
/// that can never answer anything again.
#[tokio::test]
async fn legacy_sse_call_after_the_stream_ended_fails_at_once_not_at_its_deadline() {
    let stream = format!(
        "{}{}",
        sse_event("endpoint", "/messages"),
        initialize_event()
    );
    let server = legacy_server(stream).await;
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    config.timeout = NonZeroU32::new(5_000);

    let outcome = RemoteClient::connect("legacy", &config)
        .await
        .expect("the scripted handshake succeeds");
    let RemoteConnect::Connected(client) = outcome else {
        panic!("an unauthenticated server must connect")
    };
    // The body is fully scripted, so the reader reaches its end on its own; give it the
    // scheduler turn in which to notice.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let started = Instant::now();
    let error = client
        .list_tools()
        .await
        .expect_err("a call after the event stream ended can never be answered");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(1),
        "a call against a finished reader must fail at once, not at its deadline: {waited:?}"
    );
    assert!(
        matches!(&error, RemoteError::Protocol { transport, message, .. }
            if *transport == RemoteTransport::Sse && message.contains("connection is unusable")),
        "nothing was written, so the refusal is definite and names why: {error:?}"
    );
    assert!(
        !error.is_timeout(),
        "a finished stream is not a slow one: {error:?}"
    );
    client.close().await;
}

/// The legacy handshake holds back every event that arrives before the `endpoint`
/// event, so a peer that delays that event turns the queue into an unbounded buffer of
/// its own output.
///
/// The per-event cap does not cover this: each of these events is a few hundred bytes,
/// well under it, and only their number is a problem. This one is not on the reviewer's
/// list — it is the same out-of-memory class as the OAuth bodies, found while sweeping
/// the crate for the rest of that class, and a wiremock body can only show the refusal,
/// not the exhaustion an endless stream would cause.
#[tokio::test]
async fn legacy_sse_events_before_the_endpoint_event_are_bounded() {
    let notification = sse_event(
        "message",
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": { "level": "info", "data": "x".repeat(512) },
        })
        .to_string(),
    );
    // Past the one-mebibyte pre-endpoint bound, with the endpoint event only after it,
    // so a client that defers everything gets there and one that refuses does not.
    let mut stream = notification.repeat(2048);
    assert!(stream.len() > 1024 * 1024);
    stream.push_str(&sse_event("endpoint", "/messages"));
    stream.push_str(&initialize_event());
    let server = legacy_server(stream).await;
    let mut config = remote_config(format!("{}/mcp", server.uri()));
    config.timeout = NonZeroU32::new(5_000);

    let error = RemoteClient::connect("legacy", &config)
        .await
        .expect_err("a peer may not queue unbounded output inside the handshake");
    let RemoteError::Fallback { sse, .. } = &error else {
        panic!("both transports were attempted, so the failure is a pair: {error:?}")
    };
    let RemoteError::Protocol { message, .. } = sse.as_ref() else {
        panic!("a peer that will not finish its handshake is a protocol fault: {sse:?}")
    };
    assert!(
        message.contains("before its endpoint event") && message.contains("1048576"),
        "the refusal must name the bound it enforced: {message}"
    );
}
