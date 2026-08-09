use std::collections::VecDeque;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::Delivery;

use super::{EventCursor, EventService, EventStreamError, SessionSubscription, StreamEvent};

/// Builds the legacy event route and both upstream `/api` SSE operations.
pub fn events_router(service: EventService) -> Router {
    Router::new()
        .route("/event", get(stream_events))
        .route("/api/event", get(stream_global_events))
        .route("/api/session/{sessionID}/event", get(stream_session_events))
        .with_state(service)
}

#[derive(Debug, Deserialize)]
struct EventQuery {
    #[serde(rename = "sessionID")]
    session_id: String,
}

#[derive(Debug, Default, Deserialize)]
struct SessionEventQuery {
    after: Option<i64>,
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
    Path(session_id): Path<String>,
    Query(query): Query<SessionEventQuery>,
) -> Result<Response, EventStreamError> {
    if query.after.is_some_and(|after| after < 0) {
        return Err(EventStreamError::InvalidCursor {
            value: query.after.expect("checked as some").to_string(),
        });
    }
    let cursor = Some(EventCursor {
        session_id: session_id.clone(),
        sequence: query.after.unwrap_or(0),
    });
    let subscription = service.subscribe(&session_id, cursor.as_ref()).await?;
    let connection = SessionStream::from(subscription);
    let stream = stream::unfold(connection, |mut connection| async move {
        connection
            .next_upstream_sse()
            .await
            .map(|event| (event, connection))
    });
    Ok(sse_response(stream, service.heartbeat_interval))
}

async fn stream_events(
    State(service): State<EventService>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> Result<Response, EventStreamError> {
    let cursor = match headers.get("last-event-id") {
        Some(value) => Some(
            value
                .to_str()
                .map_err(|_| EventStreamError::InvalidCursor {
                    value: "<non-UTF-8>".to_owned(),
                })?
                .parse::<EventCursor>()?,
        ),
        None => None,
    };
    let subscription = service
        .subscribe(&query.session_id, cursor.as_ref())
        .await?;
    let connection = SessionStream::from(subscription);
    let stream = stream::unfold(connection, |mut connection| async move {
        connection.next_sse().await.map(|event| (event, connection))
    });
    Ok(sse_response(stream, service.heartbeat_interval))
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
            Delivery::Event(event) => Some(encode_upstream_event(&event)),
            Delivery::Lagged { dropped } => Some(encode_global_lagged(dropped)),
        }
    }
}

struct SessionStream {
    session_id: String,
    replay: VecDeque<StreamEvent>,
    boundary: i64,
    live: crate::EventSubscription<StreamEvent>,
    last_cursor: Option<EventCursor>,
    finished: bool,
}

impl From<SessionSubscription> for SessionStream {
    fn from(subscription: SessionSubscription) -> Self {
        Self {
            session_id: subscription.session_id,
            replay: subscription.events.into(),
            boundary: subscription.boundary,
            live: subscription.live,
            last_cursor: subscription.cursor,
            finished: false,
        }
    }
}

impl SessionStream {
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

    async fn next_upstream_sse(&mut self) -> Option<Result<SseEvent, EventStreamError>> {
        if self.finished {
            return None;
        }
        if let Some(event) = self.replay.pop_front() {
            self.last_cursor = Some(event.cursor.clone());
            return Some(encode_upstream_event(&event));
        }
        loop {
            match self.live.recv().await? {
                Delivery::Event(event) if event.sequence() <= self.boundary => continue,
                Delivery::Event(event) => {
                    self.last_cursor = Some(event.cursor.clone());
                    return Some(encode_upstream_event(&event));
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

#[derive(Serialize)]
struct WireEvent<'event> {
    id: &'event str,
    #[serde(rename = "type")]
    event_type: &'event str,
    properties: &'event Map<String, Value>,
}

fn encode_event(event: &StreamEvent) -> Result<SseEvent, EventStreamError> {
    let data = serde_json::to_string(&WireEvent {
        id: &event.id,
        event_type: &event.event_type,
        properties: &event.properties,
    })?;
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

fn encode_upstream_event(event: &StreamEvent) -> Result<SseEvent, EventStreamError> {
    encode_data(json!({
        "id": event.id,
        "type": event.event_type,
        "durable": {
            "aggregateID": event.cursor.session_id,
            "seq": event.cursor.sequence,
            "version": event.version
        },
        "data": event.properties
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
        "properties": {
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
