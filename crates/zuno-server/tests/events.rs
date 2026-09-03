use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, BodyDataStream, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use futures::StreamExt;
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use zuno_db::Pool;
use zuno_db::artifact_gc::ArtifactGcPaths;
use zuno_db::session::SessionCreate;
use zuno_engine::r#loop::TurnEvent;
use zuno_llm::sse::SseParser;
use zuno_paths::DbLocation;
use zuno_server::{
    EventCursor, EventFanout, EventService, EventStreamError, NewEvent, QuestionDecision,
    QuestionRequest, RequestBroker, ServerBuilder, ServerConfig, ServerServices, events_router,
};

use zuno_server::api::{self, ApiState};

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

/// Insert the project and session rows a session-scoped stream requires.
///
/// The session route answers `404` for a session the database has never seen, so
/// every fixture that opens `/api/session/{id}/event` needs the row first.
fn create_session(pool: &Pool, session_id: &str) {
    {
        let mut connection = pool.get().expect("pooled connection");
        zuno_db::migration::apply(&mut connection).expect("schema applies");
        connection
            .execute(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('global', '/repo', '0', '0', '[]') ON CONFLICT (id) DO NOTHING",
                (),
            )
            .expect("global project row inserts");
    }
    zuno_db::session::Store::new(pool)
        .create(&SessionCreate::new(
            session_id, session_id, "global", "/repo", "/repo", "events", "test",
        ))
        .expect("fixture session inserts");
}

/// One file-backed database shared by the API state and the event service, which is
/// how `zuno serve` wires them: two pools, one durable store.
fn shared_fixture(capacity: usize) -> (TempDir, ApiState, EventService) {
    let temp = tempfile::tempdir().expect("temporary event fixture directory");
    let location = DbLocation::File(temp.path().join("zuno.db"));
    let state_pool = Pool::open(&location).expect("open API pool");
    let events_pool = Arc::new(Pool::open(&location).expect("open event pool"));
    let state = ApiState::from_pool(
        state_pool,
        "/repo",
        ArtifactGcPaths::from_data_root(temp.path()),
    )
    .expect("initialize API state");
    let events = EventService::new(events_pool, capacity);
    (temp, state, events)
}

fn event_app(events: EventService) -> axum::Router {
    ServerBuilder::new(ServerConfig::default())
        .with_routes(events_router(events))
        .router()
}

fn api_event_app(state: ApiState, events: EventService) -> axum::Router {
    ServerBuilder::new(ServerConfig::default())
        .with_routes(api::router(state.with_events(events.clone())).merge(events_router(events)))
        .router()
}

async fn open_stream(
    app: &axum::Router,
    session_id: &str,
    cursor: Option<&EventCursor>,
) -> BodyDataStream {
    let uri = format!("/api/session/{session_id}/event");
    open_stream_at(app, &uri, cursor).await
}

async fn open_stream_at(
    app: &axum::Router,
    uri: &str,
    cursor: Option<&EventCursor>,
) -> BodyDataStream {
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

#[tokio::test]
async fn unscoped_pre_release_event_route_is_not_mounted() {
    let (_pool, events) = event_service(8);
    let response = event_app(events)
        .oneshot(
            Request::builder()
                .uri("/event?sessionID=ses_retired")
                .body(Body::empty())
                .expect("the retired route request is valid"),
        )
        .await
        .expect("the server returns its normal fallback");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn global_event_stream_emits_connected_then_published_events() {
    // Given: a client on the global event path.
    let (_pool, events) = event_service(8);
    let app = event_app(events.clone());
    let mut stream = open_stream_at(&app, "/api/event", None).await;

    // Then: connection establishment is immediately observable.
    let (_, connected) = decode_frame(&next_frame(&mut stream).await);
    assert_eq!(connected["type"], "server.connected");
    assert_eq!(connected["data"], json!({}));

    // When: a durable event is published for any session.
    events
        .publish("ses_global", event(7))
        .await
        .expect("publish the global fixture event");
    let (_, published) = decode_frame(&next_frame(&mut stream).await);

    // Then: the global stream carries asynchronous events, not only a route or heartbeat.
    assert_eq!(published["type"], "test.event");
    assert_eq!(published["data"]["ordinal"], json!(7));
}

#[tokio::test]
async fn creating_a_session_is_observable_on_the_global_stream() {
    // Given: one global subscriber sharing the API's event service.
    let (_pool, events) = event_service(8);
    let state = ApiState::memory("/repo").expect("in-memory API state initializes");
    let app = api_event_app(state.clone(), events);
    let mut stream = open_stream_at(&app, "/api/event", None).await;
    let (_, connected) = decode_frame(&next_frame(&mut stream).await);
    assert_eq!(connected["type"], "server.connected");

    // When: a client creates a session through the public API.
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"ses_observable"}"#))
                .expect("session request is valid"),
        )
        .await
        .expect("session create responds");

    // Then: the row exists and the asynchronous event is delivered with its payload.
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        state
            .sessions()
            .find("ses_observable")
            .expect("read created session")
            .is_some()
    );
    let (_, created) = decode_frame(&next_frame(&mut stream).await);
    assert_eq!(created["type"], "session.created");
    assert_eq!(created["data"]["sessionID"], "ses_observable");
    assert_eq!(created["data"]["info"]["id"], "ses_observable");
}

#[tokio::test]
async fn session_event_stream_replays_after_last_event_id() {
    // Given: two committed events in one session and one event in another session.
    let (pool, events) = event_service(8);
    create_session(&pool, "ses_target");
    events
        .publish("ses_target", event(0))
        .await
        .expect("publish the sequence-zero event");
    events
        .publish("ses_target", event(1))
        .await
        .expect("publish the sequence-one event");
    events
        .publish("ses_other", event(99))
        .await
        .expect("publish the other session event");
    let app = event_app(events);

    // When: the session route resumes after aggregate sequence zero.
    let cursor = "ses_target:0"
        .parse::<EventCursor>()
        .expect("the fixture cursor is valid");
    let mut stream = open_stream(&app, "ses_target", Some(&cursor)).await;
    let (cursor, replayed) = decode_frame(&next_frame(&mut stream).await);

    // Then: the SSE id and durable body identify the same session-scoped event.
    assert_eq!(
        cursor.as_ref().map(ToString::to_string).as_deref(),
        Some("ses_target:1")
    );
    assert_eq!(replayed["type"], "test.event");
    assert_eq!(replayed["durable"]["aggregateID"], "ses_target");
    assert_eq!(replayed["durable"]["seq"], json!(1));
    assert_eq!(replayed["durable"]["version"], json!(1));
    assert_eq!(replayed["data"]["ordinal"], json!(1));
}

#[tokio::test]
async fn session_sse_never_outpaces_the_history_route() {
    // Given: a persisted session with its public SSE and history routes sharing one event service.
    let (_temp, state, events) = shared_fixture(8);
    state
        .sessions()
        .create(&SessionCreate::new(
            "ses_order",
            "ses_order",
            "global",
            "/repo",
            "/repo",
            "order",
            "test",
        ))
        .expect("fixture session inserts");
    let app = api_event_app(state, events.clone());
    let mut stream = open_stream_at(&app, "/api/session/ses_order/event", None).await;

    // When: an engine event crosses the production projection into the session SSE stream.
    let local = EventFanout::with_capacity(8);
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let forwarder = tokio::spawn({
        let events = events.clone();
        async move {
            events
                .forward_engine_events("ses_order", &local, receiver)
                .await;
        }
    });
    sender
        .send(TurnEvent::TurnStarted {
            session_id: "ses_order".to_owned(),
        })
        .await
        .expect("engine event enters the projection");
    drop(sender);
    let (_, observed) = decode_frame(&next_frame(&mut stream).await);
    let observed_sequence = observed["durable"]["seq"]
        .as_i64()
        .expect("session SSE carries a durable sequence");

    // Then: the same sequence is already committed when the client can observe it live.
    let history = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/session/ses_order/history")
                .body(Body::empty())
                .expect("history request is valid"),
        )
        .await
        .expect("history route responds");
    assert_eq!(history.status(), StatusCode::OK);
    let bytes = to_bytes(history.into_body(), 1024 * 1024)
        .await
        .expect("history response is bounded and readable");
    let history: Value = serde_json::from_slice(&bytes).expect("history response is JSON");
    assert!(
        history["data"].as_array().is_some_and(|events| {
            events.iter().any(|event| {
                event["durable"]["seq"] == observed_sequence && event["type"] == observed["type"]
            })
        }),
        "history must already contain SSE sequence {observed_sequence}: {history}"
    );

    forwarder.await.expect("event forwarder does not panic");
}

#[tokio::test]
async fn dropping_the_only_session_observer_rejects_a_question() {
    let (pool, events) = event_service(8);
    create_session(&pool, "ses_observed");
    let requests = RequestBroker::default();
    let services = ServerServices::new(8).with_requests(requests.clone());
    let app = ServerBuilder::new(ServerConfig::default())
        .with_services(services)
        .with_routes(events_router(events))
        .router();
    let observer = open_stream(&app, "ses_observed", None).await;
    let mut answer = tokio::spawn({
        let requests = requests.clone();
        async move {
            requests
                .ask_question(QuestionRequest {
                    id: "que_observed".to_owned(),
                    session_id: "ses_observed".to_owned(),
                    questions: vec![json!({"question": "Continue?"})],
                    tool: None,
                })
                .await
        }
    });
    while requests.questions(None).is_empty() {
        tokio::task::yield_now().await;
    }

    drop(observer);

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), &mut answer)
            .await
            .expect("dropping the only observer must release the question asker")
            .expect("question asker task does not panic"),
        QuestionDecision::Cancelled
    );
    assert!(
        requests.questions(None).is_empty(),
        "observer-zero cleanup must remove the rejected question"
    );
}

#[tokio::test]
async fn session_event_stream_replays_creation_at_sequence_zero() {
    // Given: a creation event at sequence zero and another public event after it.
    let (pool, events) = event_service(8);
    create_session(&pool, "ses_target");
    events
        .publish(
            "ses_target",
            NewEvent::new("session.created", Map::new()).expect("created event type"),
        )
        .await
        .expect("publish creation event");
    events
        .publish(
            "ses_target",
            NewEvent::new("session.next.agent.switched", Map::new()).expect("durable event type"),
        )
        .await
        .expect("publish durable event");

    // When: a fresh session-event subscription replays from the beginning.
    let app = event_app(events);
    let mut stream = open_stream_at(&app, "/api/session/ses_target/event", None).await;
    let (_, replayed) = decode_frame(&next_frame(&mut stream).await);

    // Then: replay starts at the first committed event without a hidden offset.
    assert_eq!(replayed["type"], "session.created");
    assert_eq!(replayed["durable"]["seq"], json!(0));
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
        .as_ref()
        .expect("the server event stays below the SSE frame cap")
        .deserialize("zuno-server", "event-stream")
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
    let (pool, events) = event_service(64);
    create_session(&pool, "ses_stream");
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
        observed.push(payload["data"]["ordinal"].as_u64());
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
        observed.push(payload["data"]["ordinal"].as_u64());
    }

    // Then: replay plus live delivery is the exact sequence, without gaps or duplicates.
    assert_eq!(observed, (0..TOTAL).map(Some).collect::<Vec<Option<u64>>>());
}

#[tokio::test]
async fn events_two_concurrent_subscribers_receive_the_same_live_event() {
    // Given: two active streams for the same session.
    let (pool, events) = event_service(8);
    create_session(&pool, "ses_shared");
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
    assert_eq!(first_payload["data"]["ordinal"], json!(7));
}

#[tokio::test]
async fn events_slow_subscriber_gets_a_diagnostic_then_disconnects() {
    // Given: a subscriber whose two-slot queue is not being polled.
    let (pool, events) = event_service(2);
    create_session(&pool, "ses_slow");
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
    assert_eq!(first.1["data"]["ordinal"], json!(0));
    assert_eq!(second.1["data"]["ordinal"], json!(1));
    assert!(diagnostic_cursor.is_none());
    assert_eq!(diagnostic["type"], "server.stream.lagged");
    assert_eq!(diagnostic["data"]["dropped"], json!(8));
    assert_eq!(diagnostic["data"]["action"], "reconnect");
    assert!(stalled.next().await.is_none());
}

#[tokio::test]
async fn events_idle_stream_emits_a_heartbeat_comment() {
    // Given: an idle stream with a short heartbeat interval.
    let (pool, events) = event_service(8);
    create_session(&pool, "ses_idle");
    let app = event_app(events.with_heartbeat_interval(Duration::from_millis(10)));
    let mut stream = open_stream(&app, "ses_idle", None).await;

    // When: no application event arrives before the keepalive deadline.
    let frame = tokio::time::timeout(Duration::from_secs(1), next_frame(&mut stream))
        .await
        .expect("the heartbeat arrives before the test timeout");

    // Then: an SSE comment keeps intermediaries alive without advancing the cursor.
    assert_eq!(frame, ": heartbeat\n\n");
}

#[tokio::test]
async fn session_event_stream_rejects_an_unknown_session() {
    // Given: an event service whose database has never seen the session.
    let (_pool, events) = event_service(8);
    let app = event_app(events.clone());

    // When: a client opens the session-scoped stream for it.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/session/ses_missing/event")
                .body(Body::empty())
                .expect("the stream request is valid"),
        )
        .await
        .expect("the event route responds");

    // Then: the request is refused instead of opening a stream that can never produce.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("error body is bounded");
    let body: Value = serde_json::from_slice(&bytes).expect("error body is JSON");
    assert_eq!(body["error"]["code"], "not_found");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("ses_missing")),
        "the error names the session: {body}"
    );
    assert!(
        format!("{events:?}").contains("sessions: 0"),
        "a refused stream must not leave a per-session fan-out behind: {events:?}"
    );
}

#[tokio::test]
async fn session_fanout_is_released_when_the_last_subscriber_disconnects() {
    // Given: two live subscribers on one session.
    let (pool, events) = event_service(8);
    create_session(&pool, "ses_release");
    let app = event_app(events.clone());
    let first = open_stream(&app, "ses_release", None).await;
    let second = open_stream(&app, "ses_release", None).await;
    assert!(
        format!("{events:?}").contains("sessions: 1"),
        "both subscribers share one fan-out: {events:?}"
    );

    // When: the subscribers disconnect one after the other.
    drop(first);
    assert!(
        format!("{events:?}").contains("sessions: 1"),
        "the fan-out must survive while a subscriber remains: {events:?}"
    );
    drop(second);

    // Then: the per-session fan-out is released with its last subscriber.
    assert!(
        format!("{events:?}").contains("sessions: 0"),
        "the fan-out must be released with its last subscriber: {events:?}"
    );

    // And: publishing to a session nobody observes does not allocate one.
    events
        .publish("ses_release", event(1))
        .await
        .expect("publish without subscribers");
    assert!(
        format!("{events:?}").contains("sessions: 0"),
        "publishing must not allocate a fan-out for an unobserved session: {events:?}"
    );
}
