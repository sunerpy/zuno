use std::fmt;
use std::str::FromStr;

use serde_json::{Map, Value};
use zuno_error::DbError;

/// A replay position tied to the session that minted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventCursor {
    pub(super) session_id: String,
    pub(super) sequence: i64,
}

impl fmt::Display for EventCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.session_id, self.sequence)
    }
}

impl FromStr for EventCursor {
    type Err = EventStreamError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parsed = value.rsplit_once(':').and_then(|(session_id, sequence)| {
            sequence
                .parse::<i64>()
                .ok()
                .filter(|sequence| *sequence >= 0)
                .map(|sequence| (session_id, sequence))
        });
        let Some((session_id, sequence)) = parsed else {
            return Err(EventStreamError::InvalidCursor {
                value: value.to_owned(),
            });
        };
        validate_session_id(session_id).map_err(|_| EventStreamError::InvalidCursor {
            value: value.to_owned(),
        })?;
        Ok(Self {
            session_id: session_id.to_owned(),
            sequence,
        })
    }
}

/// A typed event ready to be committed to one session's stream.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEvent {
    pub(super) event_type: String,
    pub(super) properties: Map<String, Value>,
}

impl NewEvent {
    /// Parses an event type once before storage or SSE framing.
    pub fn new(
        event_type: impl Into<String>,
        properties: Map<String, Value>,
    ) -> Result<Self, EventStreamError> {
        let event_type = event_type.into();
        if event_type.is_empty() || event_type.contains(['\r', '\n']) {
            return Err(EventStreamError::InvalidEventType { value: event_type });
        }
        Ok(Self {
            event_type,
            properties,
        })
    }
}

/// One committed event with its stable SSE cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamEvent {
    pub(super) cursor: EventCursor,
    pub(super) id: String,
    pub(super) event_type: String,
    pub(super) version: u32,
    pub(super) properties: Map<String, Value>,
}

impl StreamEvent {
    /// Returns the cursor to send as `Last-Event-ID` after reconnection.
    #[must_use]
    pub const fn cursor(&self) -> &EventCursor {
        &self.cursor
    }

    /// Returns this event's monotonic sequence within its session.
    #[must_use]
    pub const fn sequence(&self) -> i64 {
        self.cursor.sequence
    }

    /// Returns the upstream-compatible event properties object.
    #[must_use]
    pub const fn properties(&self) -> &Map<String, Value> {
        &self.properties
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }
}

pub(super) fn checked_sequence(
    session_id: &str,
    cursor: Option<&EventCursor>,
) -> Result<Option<i64>, EventStreamError> {
    match cursor {
        Some(cursor) if cursor.session_id != session_id => {
            Err(EventStreamError::CursorSessionMismatch {
                expected: session_id.to_owned(),
                actual: cursor.session_id.clone(),
            })
        }
        Some(cursor) => Ok(Some(cursor.sequence)),
        None => Ok(None),
    }
}

pub(super) fn validate_session_id(session_id: &str) -> Result<&str, EventStreamError> {
    if session_id.is_empty() || session_id.contains(['\r', '\n']) {
        return Err(EventStreamError::InvalidSessionId {
            value: session_id.to_owned(),
        });
    }
    Ok(session_id)
}

/// Classified failures at the storage and HTTP boundaries.
#[derive(Debug, thiserror::Error)]
pub enum EventStreamError {
    #[error("invalid event cursor `{value}`")]
    InvalidCursor { value: String },
    #[error("event cursor belongs to `{actual}`, not `{expected}`")]
    CursorSessionMismatch { expected: String, actual: String },
    #[error("invalid session id `{value}`")]
    InvalidSessionId { value: String },
    #[error("invalid event type `{value}`")]
    InvalidEventType { value: String },
    #[error("event database operation failed")]
    Database(#[from] DbError),
    #[error("event storage worker stopped")]
    Worker {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("event payload encoding failed")]
    Encode(#[from] serde_json::Error),
}
