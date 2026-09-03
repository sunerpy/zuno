use std::collections::VecDeque;

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::stream;
use serde_json::{Value, json};

use crate::request_broker::SessionRequestObserver;
use crate::{Delivery, ServerServices};

use super::{
    EventCursor, EventService, EventStreamError, LiveSessionSubscription, SessionSubscription,
    StreamEvent,
};

/// Builds Zuno's global and session-scoped SSE operations.
pub fn events_router(service: EventService) -> Router {
    Router::new()
        .route("/api/event", get(stream_global_events))
        .route("/api/session/{sessionID}/event", get(stream_session_events))
        .with_state(service)
}

async fn stream_global_events(State(service): State<EventService>) -> Response {
    let connection = GlobalStream {
        connected: false,
        live: service.subscribe_global(),
    };
    let stream = stream::unfold(connection, |mut connection| async move {
        connection.next_sse().await.map(|event| (event, connection))
    });
    sse_response(stream, service.heartbeat_interval)
}

async fn stream_session_events(
    State(service): State<EventService>,
    Extension(services): Extension<ServerServices>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, EventStreamError> {
    let cursor = cursor_from_headers(&headers)?;
    let subscription = service.subscribe(&session_id, cursor.as_ref()).await?;
    let observer = services.requests.observe_session(&session_id);
    let connection = SessionStream::new(subscription, observer);
    let stream = stream::unfold(connection, |mut connection| async move {
        connection.next_sse().await.map(|event| (event, connection))
    });
    Ok(sse_response(stream, service.heartbeat_interval))
}

fn cursor_from_headers(headers: &HeaderMap) -> Result<Option<EventCursor>, EventStreamError> {
    Ok(match headers.get("last-event-id") {
        Some(value) => Some(
            value
                .to_str()
                .map_err(|_| EventStreamError::InvalidCursor {
                    value: "<non-UTF-8>".to_owned(),
                })?
                .parse::<EventCursor>()?,
        ),
        None => None,
    })
}

fn sse_response<S>(stream: S, heartbeat_interval: std::time::Duration) -> Response
where
    S: futures::Stream<Item = Result<SseEvent, EventStreamError>> + Send + 'static,
{
    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(heartbeat_interval)
            .text("heartbeat"),
    );
    let mut response = sse.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

struct GlobalStream {
    connected: bool,
    live: crate::EventSubscription<StreamEvent>,
}

impl GlobalStream {
    async fn next_sse(&mut self) -> Option<Result<SseEvent, EventStreamError>> {
        if !self.connected {
            self.connected = true;
            return Some(encode_connected());
        }
        match self.live.recv().await? {
            Delivery::Event(event) => Some(encode_event(&event)),
            Delivery::Lagged { dropped } => Some(encode_global_lagged(dropped)),
        }
    }
}

struct SessionStream {
    session_id: String,
    replay: VecDeque<StreamEvent>,
    boundary: i64,
    live: LiveSessionSubscription,
    last_cursor: Option<EventCursor>,
    finished: bool,
    _observer: SessionRequestObserver,
}

impl SessionStream {
    fn new(subscription: SessionSubscription, observer: SessionRequestObserver) -> Self {
        Self {
            session_id: subscription.session_id,
            replay: subscription.events.into(),
            boundary: subscription.boundary,
            live: subscription.live,
            last_cursor: subscription.cursor,
            finished: false,
            _observer: observer,
        }
    }

    async fn next_sse(&mut self) -> Option<Result<SseEvent, EventStreamError>> {
        if self.finished {
            return None;
        }
        if let Some(event) = self.replay.pop_front() {
            self.last_cursor = Some(event.cursor.clone());
            return Some(encode_event(&event));
        }
        loop {
            match self.live.recv().await? {
                Delivery::Event(event) if event.sequence() <= self.boundary => continue,
                Delivery::Event(event) => {
                    self.last_cursor = Some(event.cursor.clone());
                    return Some(encode_event(&event));
                }
                Delivery::Lagged { dropped } => {
                    self.finished = true;
                    return Some(encode_lagged(
                        &self.session_id,
                        dropped,
                        self.last_cursor.as_ref(),
                    ));
                }
            }
        }
    }
}

fn encode_event(event: &StreamEvent) -> Result<SseEvent, EventStreamError> {
    let data = serde_json::to_string(&json!({
        "id": event.id,
        "type": event.event_type,
        "durable": {
            "aggregateID": event.cursor.session_id,
            "seq": event.cursor.sequence,
            "version": event.version
        },
        "data": event.properties
    }))?;
    Ok(SseEvent::default()
        .event("message")
        .id(event.cursor.to_string())
        .data(data))
}

fn encode_connected() -> Result<SseEvent, EventStreamError> {
    encode_data(json!({
        "id": format!("evt_{}", uuid::Uuid::now_v7().simple()),
        "type": "server.connected",
        "data": {}
    }))
}

fn encode_global_lagged(dropped: u64) -> Result<SseEvent, EventStreamError> {
    encode_data(json!({
        "id": format!("evt_{}", uuid::Uuid::now_v7().simple()),
        "type": "server.stream.lagged",
        "data": {"dropped": dropped, "action": "reconnect"}
    }))
}

fn encode_data(value: Value) -> Result<SseEvent, EventStreamError> {
    Ok(SseEvent::default().data(serde_json::to_string(&value)?))
}

fn encode_lagged(
    session_id: &str,
    dropped: u64,
    last_cursor: Option<&EventCursor>,
) -> Result<SseEvent, EventStreamError> {
    let data = json!({
        "id": format!("evt_{}", uuid::Uuid::now_v7().simple()),
        "type": "server.stream.lagged",
        "data": {
            "sessionID": session_id,
            "dropped": dropped,
            "lastCursor": last_cursor.map(ToString::to_string),
            "action": "reconnect"
        }
    });
    Ok(SseEvent::default()
        .event("message")
        .data(serde_json::to_string(&data)?))
}

impl IntoResponse for EventStreamError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::InvalidCursor { .. }
            | Self::CursorSessionMismatch { .. }
            | Self::InvalidSessionId { .. }
            | Self::InvalidEventType { .. } => (
                StatusCode::BAD_REQUEST,
                "invalid_event_request",
                self.to_string(),
            ),
            Self::SessionNotFound { .. } => (StatusCode::NOT_FOUND, "not_found", self.to_string()),
            Self::Database(_) | Self::Worker { .. } | Self::Encode(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "event_stream_failed",
                "event stream failed".to_owned(),
            ),
        };
        (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}
