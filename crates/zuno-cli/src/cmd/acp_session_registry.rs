//! Stable ACP session ownership and resource-bearing runtime capacity.
//!
//! An ACP session is durable state first and a live [`super::acp::AcpSession`] runtime
//! second. Keeping the `Arc<AcpSession>` stable lets concurrent `load`, `resume`, prompt,
//! and configuration requests share one activation gate instead of racing through
//! remove/open/insert and leaking whichever runtime lost the final map write.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore, TryAcquireError};

use super::acp::AcpSession;

/// Process-owned registry for durable ACP sessions and their active runtimes.
pub(super) struct AcpSessionRegistry {
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
    open_gates: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    open_slots: Arc<Semaphore>,
    active_slots: Arc<Semaphore>,
    max_open_sessions: usize,
    max_active_runtimes: usize,
    idle_timeout: Duration,
    activation_wait_timeout: Duration,
}

impl AcpSessionRegistry {
    pub(super) fn new(config: zuno_config::ResolvedAcpRuntimeConfig) -> Self {
        let max_open_sessions =
            usize::try_from(config.max_open_sessions).expect("u32 ACP capacity fits usize");
        let max_active_runtimes =
            usize::try_from(config.max_active_runtimes).expect("u32 ACP capacity fits usize");
        Self {
            sessions: Mutex::new(HashMap::new()),
            open_gates: Mutex::new(HashMap::new()),
            open_slots: Arc::new(Semaphore::new(max_open_sessions)),
            active_slots: Arc::new(Semaphore::new(max_active_runtimes)),
            max_open_sessions,
            max_active_runtimes,
            idle_timeout: config.idle_timeout(),
            activation_wait_timeout: config.activation_wait_timeout(),
        }
    }

    /// Serialize open/load/resume transitions for one durable session id.
    pub(super) async fn open_guard(&self, session_id: &str) -> OwnedMutexGuard<()> {
        let gate = {
            let mut gates = self.open_gates.lock().await;
            gates.retain(|_, gate| gate.strong_count() > 0);
            match gates.get(session_id).and_then(Weak::upgrade) {
                Some(gate) => gate,
                None => {
                    let gate = Arc::new(Mutex::new(()));
                    gates.insert(session_id.to_owned(), Arc::downgrade(&gate));
                    gate
                }
            }
        };
        gate.lock_owned().await
    }

    pub(super) async fn get(&self, session_id: &str) -> Option<Arc<AcpSession>> {
        let session = self.sessions.lock().await.get(session_id).cloned();
        if let Some(session) = session.as_ref() {
            session.touch();
        }
        session
    }

    /// Publish one newly opened durable session without replacing an existing owner.
    pub(super) async fn insert_new(&self, session: Arc<AcpSession>) -> bool {
        let mut sessions = self.sessions.lock().await;
        match sessions.entry(session.id().to_owned()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(session);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    /// Remove exactly `session`, preserving a newer owner if one somehow exists.
    pub(super) async fn remove_if(
        &self,
        session_id: &str,
        session: &Arc<AcpSession>,
    ) -> Option<Arc<AcpSession>> {
        let mut sessions = self.sessions.lock().await;
        if sessions
            .get(session_id)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            sessions.remove(session_id)
        } else {
            None
        }
    }

    pub(super) async fn drain(&self) -> Vec<Arc<AcpSession>> {
        std::mem::take(&mut *self.sessions.lock().await)
            .into_values()
            .collect()
    }

    pub(super) fn reserve_open(&self) -> Result<OwnedSemaphorePermit, zuno_acp::RpcError> {
        Arc::clone(&self.open_slots)
            .try_acquire_owned()
            .map_err(|_| {
                zuno_acp::RpcError::invalid_params(format!(
                    "this ACP connection already has {} open sessions; close an inactive \
                     session before opening another",
                    self.max_open_sessions
                ))
            })
    }

    /// Reserve one resource-bearing runtime, sleeping an eligible LRU session first.
    pub(super) async fn reserve_active(
        &self,
        exclude_session_id: Option<String>,
    ) -> Result<OwnedSemaphorePermit, zuno_acp::RpcError> {
        match Arc::clone(&self.active_slots).try_acquire_owned() {
            Ok(permit) => return Ok(permit),
            Err(TryAcquireError::Closed) => {
                return Err(zuno_acp::RpcError::internal(
                    "ACP runtime capacity is shutting down",
                ));
            }
            Err(TryAcquireError::NoPermits) => {}
        }
        if self.sleep_lru(exclude_session_id.as_deref()).await {
            match Arc::clone(&self.active_slots).try_acquire_owned() {
                Ok(permit) => return Ok(permit),
                Err(TryAcquireError::Closed) => {
                    return Err(zuno_acp::RpcError::internal(
                        "ACP runtime capacity is shutting down",
                    ));
                }
                Err(TryAcquireError::NoPermits) => {}
            }
        }
        let waited = tokio::time::timeout(
            self.activation_wait_timeout,
            Arc::clone(&self.active_slots).acquire_owned(),
        )
        .await;
        match waited {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_closed)) => Err(zuno_acp::RpcError::internal(
                "ACP runtime capacity is shutting down",
            )),
            Err(_elapsed) => Err(zuno_acp::RpcError::session_busy(format!(
                "all {} ACP runtimes are active and none became safely sleepable within {} ms",
                self.max_active_runtimes,
                self.activation_wait_timeout.as_millis()
            ))
            .with_data(serde_json::json!({
                "kind": "acp_runtime_capacity",
                "retryable": true,
                "maxActiveRuntimes": self.max_active_runtimes,
                "waitedMs": self.activation_wait_timeout.as_millis(),
            }))),
        }
    }

    /// Sleep every runtime that has been idle beyond the configured threshold.
    pub(super) async fn sleep_idle(&self) {
        let sessions = self
            .sessions
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in sessions {
            if session.idle_for() < self.idle_timeout {
                continue;
            }
            if let Err(error) = session.try_sleep().await {
                tracing::debug!(
                    session_id = session.id(),
                    %error,
                    "idle ACP runtime was not sleepable"
                );
            }
        }
    }

    pub(super) fn reaper_interval(&self) -> Duration {
        self.idle_timeout
            .min(Duration::from_secs(60))
            .max(Duration::from_millis(10))
    }

    async fn sleep_lru(&self, exclude_session_id: Option<&str>) -> bool {
        let mut sessions = self
            .sessions
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.last_used_tick());
        for session in sessions {
            if exclude_session_id.is_some_and(|excluded| excluded == session.id()) {
                continue;
            }
            match session.try_sleep().await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(error) => tracing::debug!(
                    session_id = session.id(),
                    %error,
                    "LRU ACP runtime could not sleep"
                ),
            }
        }
        false
    }
}
