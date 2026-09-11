use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::{Body, BodyDataStream, to_bytes};
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use zuno_db::Pool;
use zuno_db::artifact_gc::ArtifactGcPaths;
use zuno_db::context_usage::ContextUsageStore;
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::session::{SessionCreate, Store};
use zuno_paths::DbLocation;
use zuno_server::api::{self, ApiState};
use zuno_server::{EventService, ServerBuilder, ServerConfig, events_router};
use zuno_server::{
    ServerServices, SessionCompactExecution, SessionMutationExecutor, SessionMutationFuture,
    SessionPromptExecution, SessionReportExecution,
};
use zuno_types::context_usage::{
    ContextRequestIdentity, ContextTokenAccounting, ContextUsageCounters, ContextUsageSource,
    ContextUsageTracker,
};

fn fixture() -> (TempDir, ApiState, Arc<Pool>, EventService) {
    let temporary = tempfile::tempdir().unwrap();
    let location = DbLocation::File(temporary.path().join("context.sqlite"));
    let state = ApiState::from_pool(
        Pool::open(&location).unwrap(),
        "/repo",
        ArtifactGcPaths::from_data_root(temporary.path()),
    )
    .unwrap();
    let pool = Arc::new(Pool::open(&location).unwrap());
    Store::new(&pool)
        .create(&SessionCreate::new(
            "ses_context",
            "context",
            "global",
            "/repo",
            "/repo",
            "Context",
            "test",
        ))
        .unwrap();
    let events = EventService::new(Arc::clone(&pool), 64);
    (temporary, state.with_events(events.clone()), pool, events)
}

fn request(sequence: u64, source: ContextUsageSource) -> ContextRequestIdentity {
    ContextRequestIdentity {
        request_id: format!("synthetic-request-{sequence}"),
        request_sequence: sequence,
        attempt: 1,
        context_epoch: 0,
        provider_id: "synthetic-provider".to_owned(),
        model_id: "synthetic-model".to_owned(),
        source,
        turn_id: Some("synthetic-turn".to_owned()),
        time_started: i64::try_from(sequence).unwrap(),
        request_context_tokens: None,
        history_prefix: None,
    }
}

fn tracker(session_id: &str, source: ContextUsageSource) -> ContextUsageTracker {
    let mut tracker = ContextUsageTracker::for_source(session_id, source);
    let first = request(1, source);
    tracker.start_request(first.clone(), Some(70_000), Some(0), Some(200_000), 1);
    tracker.observe_usage(
        &first,
        ContextUsageCounters {
            input_tokens: Some(125_350),
            output_tokens: Some(40),
            reasoning_tokens: Some(10),
            accounting: ContextTokenAccounting::CacheInsideInput,
            ..ContextUsageCounters::default()
        },
        2,
    );
    tracker.commit_request(&first, 3);
    tracker
}

fn app(state: ApiState, events: EventService) -> axum::Router {
    ServerBuilder::new(ServerConfig::default())
        .with_routes(api::router(state).merge(events_router(events)))
        .router()
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, value)
}

async fn sse_frame(stream: &mut BodyDataStream) -> (Option<String>, Value) {
    let frame = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("bounded context frame")
        .unwrap()
        .unwrap();
    let frame = std::str::from_utf8(&frame).unwrap();
    let cursor = frame
        .lines()
        .find_map(|line| line.strip_prefix("id:"))
        .map(|id| id.trim().to_owned());
    let data = frame
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .unwrap();
    (cursor, serde_json::from_str(data.trim()).unwrap())
}

#[tokio::test]
async fn session_detail_exposes_unknown_context_without_inventing_zero() {
    let state = ApiState::memory("/repo").expect("isolated API state");
    state
        .sessions()
        .create(&SessionCreate::new(
            "ses_context",
            "context",
            "global",
            "/repo",
            "/repo",
            "Context fixture",
            "test",
        ))
        .expect("synthetic session");
    let app = ServerBuilder::new(ServerConfig::default())
        .with_routes(api::router(state))
        .router();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/session/ses_context")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        body["data"]["contextUsage"]["freshness"], "unknown",
        "session detail must carry an explicit canonical context state"
    );
    assert!(body["data"]["contextUsage"]["usedTokens"].is_null());
}

#[tokio::test]
async fn session_detail_and_list_read_the_same_canonical_snapshot() {
    let (_temporary, state, pool, events) = fixture();
    let mut usage = tracker("ses_context", ContextUsageSource::Main);
    usage.start_request(
        request(2, ContextUsageSource::Main),
        Some(73_948),
        Some(2_500),
        Some(200_000),
        4,
    );
    ContextUsageStore::new(pool).save(None, &usage).unwrap();
    let app = app(state, events);
    let (status, detail) = get(&app, "/api/session/ses_context").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail["data"]["contextUsage"],
        serde_json::to_value(usage.snapshot()).unwrap()
    );
    assert_eq!(detail["data"]["contextUsage"]["usedTokens"], 127_890);
    assert_eq!(detail["data"]["contextUsage"]["freshness"], "estimated");
    let (_, list) = get(&app, "/api/session").await;
    let row = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "ses_context")
        .unwrap();
    assert_eq!(row["contextUsage"], detail["data"]["contextUsage"]);
}

#[tokio::test]
async fn legacy_resume_uses_real_message_usage_and_returned_tail_without_writing_state() {
    let (_temporary, state, pool, events) = fixture();
    {
        let connection = pool.get().unwrap();
        let transaction = zuno_db::open::immediate_transaction(&connection).unwrap();
        let messages = MessageStore::new(&transaction);
        let assistant = MessageRecord::from_json(json!({
            "id": "assistant",
            "sessionID": "ses_context",
            "role": "assistant",
            "providerID": "synthetic-provider",
            "modelID": "synthetic-model",
            "time": {"created": 1},
            "tokens": {
                "input": 149_501, "output": 5, "reasoning": 4,
                "cache": {"read": 0, "write": 0},
                "accounting": "cache-inside-input",
            },
        }))
        .unwrap();
        messages.put_message(&assistant).unwrap();
        let user = MessageRecord::from_json(json!({
            "id": "next-user",
            "sessionID": "ses_context",
            "role": "user",
            "time": {"created": 2},
        }))
        .unwrap();
        messages.put_message(&user).unwrap();
        messages.put_part(&PartRecord::from_json(json!({
            "id": "next-user-text", "messageID": "next-user", "sessionID": "ses_context",
            "type": "text", "text": "Continue with the returned content.",
        }), 2).unwrap()).unwrap();
        zuno_db::session::reconcile_usage(
            &transaction,
            "ses_context",
            None,
            zuno_db::session::MessageUsage::from_data(&assistant.data),
            Some(200_000),
        )
        .unwrap();
        transaction.commit().unwrap();
    }
    let app = app(state, events);
    let (status, response) = get(&app, "/api/session/ses_context").await;
    assert_eq!(status, StatusCode::OK);
    let context = &response["data"]["contextUsage"];
    assert!(context["usedTokens"].as_u64().unwrap() > 149_510);
    assert!(context["usedTokens"].as_u64().unwrap() < 149_610);
    assert_eq!(context["lastConfirmed"]["usage"]["outputTokens"], 9);
    assert_eq!(context["freshness"], "estimated");
    assert_eq!(context["revision"], 0);
    assert!(
        ContextUsageStore::new(pool)
            .load("ses_context")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn auxiliary_rows_and_child_events_cannot_replace_the_parent_context() {
    let (_temporary, state, pool, events) = fixture();
    let store = ContextUsageStore::new(Arc::clone(&pool));
    let main = tracker("ses_context", ContextUsageSource::Main);
    store.save(None, &main).unwrap();
    store
        .save(None, &tracker("ses_context", ContextUsageSource::Learning))
        .unwrap();
    let mut child = SessionCreate::new(
        "ses_child",
        "child",
        "global",
        "/repo",
        "/repo",
        "Child",
        "test",
    );
    child.parent_id = Some("ses_context".to_owned());
    Store::new(&pool).create(&child).unwrap();
    let child_usage = tracker("ses_child", ContextUsageSource::Child);
    store.save(None, &child_usage).unwrap();
    let app = app(state, events.clone());
    let (_, parent) = get(&app, "/api/session/ses_context").await;
    let (_, child) = get(&app, "/api/session/ses_child").await;
    assert_eq!(
        parent["data"]["contextUsage"],
        serde_json::to_value(main.snapshot()).unwrap()
    );
    assert_eq!(child["data"]["contextUsage"]["source"], "child");
    assert!(
        events
            .publish_context_usage(
                tracker("ses_context", ContextUsageSource::Child)
                    .snapshot()
                    .clone(),
            )
            .await
            .is_err()
    );
    assert!(
        events
            .publish_context_usage(
                tracker("ses_context", ContextUsageSource::Learning)
                    .snapshot()
                    .clone(),
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn corrupt_canonical_state_fails_closed_instead_of_falling_back_to_history() {
    let (_temporary, state, pool, events) = fixture();
    ContextUsageStore::new(Arc::clone(&pool))
        .save(None, &tracker("ses_context", ContextUsageSource::Main))
        .unwrap();
    pool.get()
        .unwrap()
        .execute(
            "UPDATE session_context_usage SET state_json = \
         json_set(state_json, '$.snapshot.usedTokens', 73948)",
            [],
        )
        .unwrap();
    let (status, _) = get(&app(state, events), "/api/session/ses_context").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let stored: i64 = pool
        .get()
        .unwrap()
        .query_row(
            "SELECT json_extract(state_json, '$.snapshot.usedTokens') FROM session_context_usage",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, 73_948);
}

#[tokio::test]
async fn sse_bootstrap_and_reconnect_restore_state_without_inventing_event_cursors() {
    let (_temporary, state, pool, events) = fixture();
    let store = ContextUsageStore::new(pool);
    let mut usage = tracker("ses_context", ContextUsageSource::Main);
    store.save(None, &usage).unwrap();
    let app = app(state, events.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/session/ses_context/event")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let (cursor, snapshot) = sse_frame(&mut stream).await;
    assert_eq!(cursor, None);
    assert_eq!(snapshot["type"], "session.context.snapshot");
    assert_eq!(
        snapshot["data"]["snapshot"],
        serde_json::to_value(usage.snapshot()).unwrap()
    );

    let previous = usage.snapshot().revision;
    usage.start_request(
        request(2, ContextUsageSource::Main),
        Some(73_948),
        Some(3_000),
        Some(200_000),
        4,
    );
    store.save(Some(previous), &usage).unwrap();
    let committed = events
        .publish_context_usage(usage.snapshot().clone())
        .await
        .unwrap();
    let (cursor, live) = sse_frame(&mut stream).await;
    assert_eq!(cursor, Some(committed.cursor().to_string()));
    assert_eq!(live["type"], "session.context.usage");
    assert_eq!(live["data"]["snapshot"]["usedTokens"], 128_390);
    drop(stream);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/session/ses_context/event")
                .header("last-event-id", cursor.unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut resumed = response.into_body().into_data_stream();
    let (cursor, replayed) = sse_frame(&mut resumed).await;
    assert_eq!(cursor, None);
    assert_eq!(
        replayed["data"]["snapshot"],
        serde_json::to_value(usage.snapshot()).unwrap()
    );
    let durable = events.replay("ses_context", None).await.unwrap();
    assert_eq!(
        durable.len(),
        1,
        "bootstrap must not append a fabricated event"
    );
}

#[derive(Debug, Default)]
struct DeferredOnlyExecutor {
    calls: AtomicUsize,
}

impl SessionMutationExecutor for DeferredOnlyExecutor {
    fn prompt(
        &self,
        _request: SessionPromptExecution,
        _guard: zuno_engine::status::SessionRunGuard,
        _events: zuno_engine::r#loop::TurnEventSender,
    ) -> SessionMutationFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn reports(
        &self,
        _request: SessionReportExecution,
        _guard: zuno_engine::status::SessionRunGuard,
        _events: zuno_engine::r#loop::TurnEventSender,
    ) -> SessionMutationFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn compact(
        &self,
        _request: SessionCompactExecution,
        _guard: zuno_engine::status::SessionRunGuard,
        _events: zuno_engine::r#loop::TurnEventSender,
    ) -> SessionMutationFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn deferred_prompt_returns_the_stored_receipt_without_claiming_model_application() {
    let (_temporary, state, pool, _events) = fixture();
    let executor = Arc::new(DeferredOnlyExecutor::default());
    let services = ServerServices::new(64).with_mutations(executor.clone());
    let app = ServerBuilder::new(ServerConfig::default())
        .with_services(services)
        .with_routes(api::router(state))
        .router();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/session/ses_context/prompt")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "id": "synthetic-input",
                        "prompt": {"text": "Wait for explicit execution."},
                        "resume": false,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let receipt = zuno_db::input_receipt::InputReceiptStore::new(pool)
        .get("ses_context", "synthetic-input")
        .unwrap()
        .unwrap();
    assert_eq!(
        response["data"]["receipt"],
        serde_json::to_value(receipt).unwrap()
    );
    assert_eq!(response["data"]["receipt"]["state"], "admitted");
    assert!(response["data"]["receipt"].get("appliedAt").is_none());
    assert!(response["data"]["receipt"].get("completedAt").is_none());
    tokio::task::yield_now().await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_context_event_cannot_claim_unpersisted_usage() {
    let (_temporary, _state, _pool, events) = fixture();
    assert!(
        events
            .publish_context_usage(
                tracker("ses_context", ContextUsageSource::Main)
                    .snapshot()
                    .clone(),
            )
            .await
            .is_err()
    );
    assert!(events.replay("ses_context", None).await.unwrap().is_empty());
}
