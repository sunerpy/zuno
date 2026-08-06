use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, BodyDataStream};
use axum::http::{Request, StatusCode, header};
use futures::StreamExt;
use oc_db::Pool;
use oc_llm::sse::SseParser;
use oc_paths::DbLocation;
use oc_server::{
    EventCursor, EventService, EventStreamError, NewEvent, ServerBuilder, ServerConfig,
    events_router,
};
use serde_json::{Map, Value, json};
use tower::ServiceExt;

fn event(ordinal: u64) -> NewEvent {
    let mut properties = Map::new();
    properties.insert("ordinal".to_owned(), json!(ordinal));
    NewEvent::new("test.event", properties).expect("the fixture event type is valid")
}

fn event_service(capacity: usize) -> (Arc<Pool>, EventService) {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open in-memory event database"));
    let events = EventService::new(Arc::clone(&pool), capacity);
    (pool, events)
}

fn event_app(events: EventService) -> axum::Router {
    ServerBuilder::new(ServerConfig::default())
        .with_routes(events_router(events))
        .router()
}

async fn open_stream(
    app: &axum::Router,
    session_id: &str,
    cursor: Option<&EventCursor>,
) -> BodyDataStream {
    let uri = format!("/event?sessionID={session_id}");
    let mut request = Request::builder().uri(uri);
    if let Some(cursor) = cursor {
        request = request.header("last-event-id", cursor.to_string());
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::empty())
                .expect("the stream request is valid"),
        )
        .await
        .expect("the event route responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    response.into_body().into_data_stream()
}

async fn next_frame(stream: &mut BodyDataStream) -> String {
    let bytes = stream
        .next()
        .await
        .expect("the stream remains open")
        .expect("the SSE frame is readable");
    String::from_utf8(bytes.to_vec()).expect("axum emits UTF-8 SSE frames")
}

fn decode_frame(frame: &str) -> (Option<EventCursor>, Value) {
    let cursor = frame
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .map(|raw| raw.parse().expect("the server emits a valid cursor"));
    let mut parser = SseParser::new();
    let parsed = parser.push(frame.as_bytes());
    assert_eq!(parsed.len(), 1, "axum must emit one complete event frame");
    let payload = parsed[0]
        .deserialize("oc-server", "event-stream")
        .expect("the SSE data is valid JSON");
    (cursor, payload)
}

#[tokio::test]
async fn events_replay_strictly_after_a_session_cursor() {
    // Given: two durable events in one session and an independent event in another.
    let (pool, events) = event_service(8);
    let first = events
        .publish("ses_alpha", event(0))
        .await
        .expect("publish the first event");
    let second = events
        .publish("ses_alpha", event(1))
        .await
        .expect("publish the second event");
    let other = events
        .publish("ses_beta", event(100))
        .await
        .expect("publish the other session event");

    // When: a fresh service process replays after the first event's cursor.
    let restarted = EventService::new(pool, 8);
    let replayed = restarted
        .replay("ses_alpha", Some(first.cursor()))
        .await
        .expect("replay the session tail");

    // Then: sequences are per-session and the cursor itself is not repeated.
    assert_eq!(first.sequence(), 0);
    assert_eq!(second.sequence(), 1);
    assert_eq!(other.sequence(), 0);
    assert_eq!(first.cursor().to_string(), "ses_alpha:0");
    assert_eq!(
        replayed
            .iter()
            .map(|event| event.properties()["ordinal"].clone())
            .collect::<Vec<Value>>(),
        vec![json!(1)]
    );
}

#[tokio::test]
async fn events_reject_a_cursor_from_another_session() {
    // Given: a valid cursor minted for one session.
    let (_pool, events) = event_service(8);
    let cursor = events
        .publish("ses_alpha", event(0))
        .await
        .expect("publish the fixture event")
        .cursor()
        .clone();

    // When: another session attempts to use that cursor.
    let error = events
        .replay("ses_beta", Some(&cursor))
        .await
        .expect_err("cross-session cursors must be rejected");

    // Then: the typed error preserves both session identities.
    assert!(matches!(
        error,
        EventStreamError::CursorSessionMismatch { expected, actual }
            if expected == "ses_beta" && actual == "ses_alpha"
    ));
}

#[test]
fn events_reject_a_malformed_cursor_at_the_boundary() {
    // Given: an SSE Last-Event-ID without a numeric sequence suffix.
    let raw = "ses_alpha:not-a-sequence";

    // When: the boundary parser receives it.
    let error = raw
        .parse::<EventCursor>()
        .expect_err("a malformed cursor must not enter the event service");

    // Then: callers receive a classified input error.
    assert!(matches!(
        error,
        EventStreamError::InvalidCursor { value } if value == raw
    ));
}

#[tokio::test]
async fn events_initialize_an_empty_in_memory_pool_before_first_publish() {
    // Given: the same empty pooled in-memory database used by the server process.
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open empty in-memory pool"));
    let events = EventService::new(pool, 8);

    // When: the first event is published without a separate migration connection.
    let stored = events
        .publish("ses_fresh", event(0))
        .await
        .expect("the event service initializes its own pool");

    // Then: the newly created durable stream starts at sequence zero.
    assert_eq!(stored.sequence(), 0);
}

#[tokio::test]
async fn events_reconnect_delivers_exactly_the_one_thousand_published_events() {
    // Given: one connected client and a deterministic non-boundary disconnect point.
    const TOTAL: u64 = 1_000;
    const DISCONNECT_AT: u64 = 437;
    let (_pool, events) = event_service(64);
    let app = event_app(events.clone());
    let mut first_connection = open_stream(&app, "ses_stream", None).await;
    let mut observed = Vec::with_capacity(TOTAL as usize);
    let mut cursor = None;

    for ordinal in 0..DISCONNECT_AT {
        events
            .publish("ses_stream", event(ordinal))
            .await
            .expect("publish while the first client is connected");
        let (next_cursor, payload) = decode_frame(&next_frame(&mut first_connection).await);
        cursor = next_cursor;
        observed.push(payload["properties"]["ordinal"].as_u64());
    }
    drop(first_connection);
    for ordinal in DISCONNECT_AT..TOTAL {
        events
            .publish("ses_stream", event(ordinal))
            .await
            .expect("publish while the client is disconnected");
    }

    // When: the client reconnects with the last cursor it observed.
    let cursor = cursor.expect("the first connection observed a cursor");
    let mut resumed = open_stream(&app, "ses_stream", Some(&cursor)).await;
    for _ in DISCONNECT_AT..TOTAL {
        let (_cursor, payload) = decode_frame(&next_frame(&mut resumed).await);
        observed.push(payload["properties"]["ordinal"].as_u64());
    }

    // Then: replay plus live delivery is the exact sequence, without gaps or duplicates.
    assert_eq!(observed, (0..TOTAL).map(Some).collect::<Vec<Option<u64>>>());
}

#[tokio::test]
async fn events_two_concurrent_subscribers_receive_the_same_live_event() {
    // Given: two active streams for the same session.
    let (_pool, events) = event_service(8);
    let app = event_app(events.clone());
    let mut first = open_stream(&app, "ses_shared", None).await;
    let mut second = open_stream(&app, "ses_shared", None).await;

    // When: one event is committed and published.
    events
        .publish("ses_shared", event(7))
        .await
        .expect("publish the shared event");
    let (_, first_payload) = decode_frame(&next_frame(&mut first).await);
    let (_, second_payload) = decode_frame(&next_frame(&mut second).await);

    // Then: both independently bounded subscribers observe it.
    assert_eq!(first_payload, second_payload);
    assert_eq!(first_payload["properties"]["ordinal"], json!(7));
}

#[tokio::test]
async fn events_slow_subscriber_gets_a_diagnostic_then_disconnects() {
    // Given: a subscriber whose two-slot queue is not being polled.
    let (_pool, events) = event_service(2);
    let app = event_app(events.clone());
    let mut stalled = open_stream(&app, "ses_slow", None).await;
    for ordinal in 0..10 {
        events
            .publish("ses_slow", event(ordinal))
            .await
            .expect("persist every event despite subscriber pressure");
    }

    // When: the client resumes reading after overflowing its queue.
    let first = decode_frame(&next_frame(&mut stalled).await);
    let second = decode_frame(&next_frame(&mut stalled).await);
    let (diagnostic_cursor, diagnostic) = decode_frame(&next_frame(&mut stalled).await);

    // Then: retained events stay ordered, loss is explicit, and the stream terminates.
    assert_eq!(first.1["properties"]["ordinal"], json!(0));
    assert_eq!(second.1["properties"]["ordinal"], json!(1));
    assert!(diagnostic_cursor.is_none());
    assert_eq!(diagnostic["type"], "server.stream.lagged");
    assert_eq!(diagnostic["properties"]["dropped"], json!(8));
    assert_eq!(diagnostic["properties"]["action"], "reconnect");
    assert!(stalled.next().await.is_none());
}

#[tokio::test]
async fn events_idle_stream_emits_a_heartbeat_comment() {
    // Given: an idle stream with a short heartbeat interval.
    let (_pool, events) = event_service(8);
    let app = event_app(events.with_heartbeat_interval(Duration::from_millis(10)));
    let mut stream = open_stream(&app, "ses_idle", None).await;

    // When: no application event arrives before the keepalive deadline.
    let frame = tokio::time::timeout(Duration::from_secs(1), next_frame(&mut stream))
        .await
        .expect("the heartbeat arrives before the test timeout");

    // Then: an SSE comment keeps intermediaries alive without advancing the cursor.
    assert_eq!(frame, ": heartbeat\n\n");
}
