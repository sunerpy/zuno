use std::collections::VecDeque;

use axum::extract::{Query, State};
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

/// Builds the upstream-compatible `/event` route.
pub fn events_router(service: EventService) -> Router {
    Router::new()
        .route("/event", get(stream_events))
        .with_state(service)
}

#[derive(Debug, Deserialize)]
struct EventQuery {
    #[serde(rename = "sessionID")]
    session_id: String,
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
    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(service.heartbeat_interval)
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
    Ok(response)
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
