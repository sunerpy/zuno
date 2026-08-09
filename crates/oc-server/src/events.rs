//! Durable event storage and per-session bounded live delivery.

mod route;
mod store;
mod types;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use oc_db::Pool;

use crate::{EventFanout, EventSubscription};
use store::{Snapshot, Store};

pub use route::events_router;
pub use types::{EventCursor, EventStreamError, NewEvent, StreamEvent};

const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Durable event storage plus per-session bounded live fan-out.
#[derive(Clone)]
pub struct EventService {
    store: Arc<Store>,
    fanouts: Arc<Mutex<HashMap<String, EventFanout<StreamEvent>>>>,
    global: EventFanout<StreamEvent>,
    heartbeat_interval: Duration,
}

impl EventService {
    /// Creates a service over an initialized `opencode.db` pool.
    #[must_use]
    pub fn new(pool: Arc<Pool>, subscriber_capacity: usize) -> Self {
        Self {
            store: Arc::new(Store::new(pool, subscriber_capacity.max(1))),
            fanouts: Arc::new(Mutex::new(HashMap::new())),
            global: EventFanout::with_capacity(subscriber_capacity),
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
        }
    }

    /// Overrides the ten-second SSE keepalive cadence.
    #[must_use]
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// Commits one event before offering it to live subscribers.
    pub async fn publish(
        &self,
        session_id: &str,
        event: NewEvent,
    ) -> Result<StreamEvent, EventStreamError> {
        let session_id = types::validate_session_id(session_id)?.to_owned();
        let store = Arc::clone(&self.store);
        let stored = tokio::task::spawn_blocking({
            let session_id = session_id.clone();
            move || store.append(&session_id, event)
        })
        .await
        .map_err(|source| EventStreamError::Worker { source })??;
        self.fanout(&session_id).publish(stored.clone());
        self.global.publish(stored.clone());
        Ok(stored)
    }

    /// Reads committed events strictly after an optional cursor.
    pub async fn replay(
        &self,
        session_id: &str,
        cursor: Option<&EventCursor>,
    ) -> Result<Vec<StreamEvent>, EventStreamError> {
        let session_id = types::validate_session_id(session_id)?.to_owned();
        let after = types::checked_sequence(&session_id, cursor)?;
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.replay(&session_id, after))
            .await
            .map_err(|source| EventStreamError::Worker { source })?
    }

    async fn subscribe(
        &self,
        session_id: &str,
        cursor: Option<&EventCursor>,
    ) -> Result<SessionSubscription, EventStreamError> {
        let session_id = types::validate_session_id(session_id)?.to_owned();
        let after = types::checked_sequence(&session_id, cursor)?;
        let live = self.fanout(&session_id).subscribe();
        let store = Arc::clone(&self.store);
        let snapshot_session = session_id.clone();
        let Snapshot { events, boundary } =
            tokio::task::spawn_blocking(move || store.snapshot(&snapshot_session, after))
                .await
                .map_err(|source| EventStreamError::Worker { source })??;
        Ok(SessionSubscription {
            session_id,
            events,
            boundary,
            live,
            cursor: cursor.cloned(),
        })
    }

    fn fanout(&self, session_id: &str) -> EventFanout<StreamEvent> {
        self.lock_fanouts()
            .entry(session_id.to_owned())
            .or_insert_with(|| EventFanout::with_capacity(self.store.subscriber_capacity()))
            .clone()
    }

    fn subscribe_global(&self) -> EventSubscription<StreamEvent> {
        self.global.subscribe()
    }

    fn lock_fanouts(&self) -> MutexGuard<'_, HashMap<String, EventFanout<StreamEvent>>> {
        self.fanouts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for EventService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventService")
            .field("sessions", &self.lock_fanouts().len())
            .field("heartbeat_interval", &self.heartbeat_interval)
            .finish_non_exhaustive()
    }
}

struct SessionSubscription {
    session_id: String,
    events: Vec<StreamEvent>,
    boundary: i64,
    live: EventSubscription<StreamEvent>,
    cursor: Option<EventCursor>,
}
