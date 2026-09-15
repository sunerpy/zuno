use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::RpcError;
use super::frame::id_key;
use super::lock;

pub(super) enum Outbound {
    Frame {
        value: Value,
        sent: oneshot::Sender<Result<(), String>>,
    },
    Close {
        closed: oneshot::Sender<()>,
    },
}

type Pending = HashMap<String, oneshot::Sender<Result<Value, RpcError>>>;

#[derive(Default)]
pub(super) struct PendingState {
    closed: bool,
    waiters: Pending,
}

#[derive(Default)]
pub(super) struct DeferredState {
    response_sent: bool,
    notifications: Vec<(String, Value)>,
}

#[derive(Clone)]
pub struct ClientConnection {
    pub(super) output: mpsc::Sender<Outbound>,
    pub(super) pending: Arc<Mutex<PendingState>>,
    pub(super) next_id: Arc<AtomicU64>,
    pub(super) deferred: Option<Arc<Mutex<DeferredState>>>,
    pub(super) scoped_requests: Option<Arc<Mutex<HashMap<String, Value>>>>,
}

struct PendingRequestGuard {
    pending: Arc<Mutex<PendingState>>,
    scoped_requests: Option<Arc<Mutex<HashMap<String, Value>>>>,
    pending_id: String,
    completed: bool,
}

impl PendingRequestGuard {
    fn complete(&mut self) {
        self.completed = true;
        if let Some(scoped) = &self.scoped_requests {
            lock(scoped).remove(&self.pending_id);
        }
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if !self.completed {
            lock(&self.pending).waiters.remove(&self.pending_id);
        }
    }
}

impl ClientConnection {
    /// Clone for outbound RPCs supervised by the session rather than one prompt.
    ///
    /// Only prompt-owned pending-request tracking is cleared. The connection's
    /// writer, ID sequence, disconnect state, and after-response notification
    /// ordering remain shared. Completing or cancelling the originating prompt
    /// cannot cancel requests made through this clone; disconnect still does.
    ///
    /// The host owns session shutdown and must drop/abort its supervised request
    /// futures when the session closes.
    #[must_use]
    pub fn session_scoped(&self) -> Self {
        Self {
            scoped_requests: None,
            ..self.clone()
        }
    }

    pub async fn session_update(&self, session_id: &str, update: Value) -> Result<(), RpcError> {
        self.notify(
            "session/update",
            json!({ "sessionId": session_id, "update": update }),
        )
        .await
    }

    pub fn session_update_after_response(
        &self,
        session_id: &str,
        update: Value,
    ) -> Result<(), RpcError> {
        let deferred = self.deferred.as_ref().ok_or_else(|| {
            RpcError::internal("deferred ACP updates require a request-scoped client connection")
        })?;
        let mut deferred = lock(deferred);
        if deferred.response_sent {
            return Err(RpcError::internal(
                "ACP response was already sent before the deferred update was registered",
            ));
        }
        deferred.notifications.push((
            "session/update".to_owned(),
            json!({ "sessionId": session_id, "update": update }),
        ));
        Ok(())
    }

    pub async fn request_permission(&self, params: Value) -> Result<Value, RpcError> {
        self.request("session/request_permission", params).await
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = format!("acp-agent-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let pending_id = format!("s:{id}");
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut pending = lock(&self.pending);
            if pending.closed {
                return Err(RpcError::internal("ACP connection is closed"));
            }
            pending.waiters.insert(pending_id.clone(), response_tx);
        }
        if let Some(scoped) = &self.scoped_requests {
            lock(scoped).insert(pending_id.clone(), Value::String(id.clone()));
        }
        let mut guard = PendingRequestGuard {
            pending: Arc::clone(&self.pending),
            scoped_requests: self.scoped_requests.as_ref().map(Arc::clone),
            pending_id: pending_id.clone(),
            completed: false,
        };
        if let Err(error) = self
            .send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .await
        {
            lock(&self.pending).waiters.remove(&pending_id);
            guard.complete();
            return Err(error);
        }
        let result = response_rx
            .await
            .map_err(|_| RpcError::internal("ACP connection closed before client response"))?;
        guard.complete();
        result
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    pub(super) async fn response(
        &self,
        id: Value,
        result: Result<Value, RpcError>,
    ) -> Result<(), RpcError> {
        let value = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": error.value() })
            }
        };
        self.send(value).await
    }

    async fn send(&self, value: Value) -> Result<(), RpcError> {
        let (sent_tx, sent_rx) = oneshot::channel();
        self.output
            .send(Outbound::Frame {
                value,
                sent: sent_tx,
            })
            .await
            .map_err(|_| RpcError::internal("ACP writer is closed"))?;
        sent_rx
            .await
            .map_err(|_| RpcError::internal("ACP writer stopped"))?
            .map_err(RpcError::internal)
    }

    pub(super) async fn close_output(&self) -> Result<(), RpcError> {
        let (closed_tx, closed_rx) = oneshot::channel();
        self.output
            .send(Outbound::Close { closed: closed_tx })
            .await
            .map_err(|_| RpcError::internal("ACP writer is closed"))?;
        closed_rx
            .await
            .map_err(|_| RpcError::internal("ACP writer stopped before closing"))
    }

    pub(super) fn request_scoped(&self) -> Self {
        Self {
            output: self.output.clone(),
            pending: Arc::clone(&self.pending),
            next_id: Arc::clone(&self.next_id),
            deferred: Some(Arc::new(Mutex::new(DeferredState::default()))),
            scoped_requests: Some(Arc::new(Mutex::new(HashMap::new()))),
        }
    }

    pub(super) async fn cancel_scoped_requests(&self) -> Result<(), RpcError> {
        let Some(scoped) = &self.scoped_requests else {
            return Ok(());
        };
        let requests = lock(scoped).drain().collect::<Vec<_>>();
        let mut failure = None;
        for (pending_id, request_id) in requests {
            if let Some(waiter) = lock(&self.pending).waiters.remove(&pending_id) {
                let _ignored =
                    waiter.send(Err(RpcError::cancelled("parent ACP request was cancelled")));
            }
            if let Err(error) = self
                .notify(
                    "$/cancel_request",
                    json!({
                        "requestId": request_id,
                    }),
                )
                .await
                && failure.is_none()
            {
                failure = Some(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    pub(super) async fn flush_after_response(&self) -> Result<(), RpcError> {
        let Some(deferred) = &self.deferred else {
            return Ok(());
        };
        let notifications = {
            let mut deferred = lock(deferred);
            deferred.response_sent = true;
            std::mem::take(&mut deferred.notifications)
        };
        for (method, params) in notifications {
            self.notify(&method, params).await?;
        }
        Ok(())
    }

    pub(super) fn resolve_response(&self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(id_key) else {
            return;
        };
        let Some(waiter) = lock(&self.pending).waiters.remove(&id) else {
            return;
        };
        let response = if let Some(result) = frame.get("result") {
            Ok(result.clone())
        } else if let Some(error) = frame.get("error") {
            Err(RpcError {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("client request failed")
                    .to_owned(),
                data: error.get("data").cloned(),
            })
        } else {
            Err(RpcError::invalid_request("response has no result or error"))
        };
        let _ignored = waiter.send(response);
    }

    pub(super) fn close_pending(&self, error: RpcError) {
        let waiters = {
            let mut pending = lock(&self.pending);
            pending.closed = true;
            pending
                .waiters
                .drain()
                .map(|(_, waiter)| waiter)
                .collect::<Vec<_>>()
        };
        for waiter in waiters {
            let _ignored = waiter.send(Err(error.clone()));
        }
    }
}

impl std::fmt::Debug for ClientConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending = lock(&self.pending);
        formatter
            .debug_struct("ClientConnection")
            .field("pending", &pending.waiters.len())
            .field("closed", &pending.closed)
            .field("request_scoped", &self.deferred.is_some())
            .field(
                "scoped_requests",
                &self
                    .scoped_requests
                    .as_ref()
                    .map(|requests| lock(requests).len()),
            )
            .finish_non_exhaustive()
    }
}
