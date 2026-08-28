use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ACP request failed ({code}): {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(-32600, message)
    }

    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("Method not found: {method}"))
    }

    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(-32800, message)
    }

    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn value(&self) -> Value {
        let mut error = json!({
            "code": self.code,
            "message": self.message,
        });
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        error
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("ACP transport I/O failed")]
    Io(#[from] std::io::Error),
    #[error("ACP writer task failed")]
    WriterTask(#[from] tokio::task::JoinError),
    #[error("ACP writer stopped before a frame was sent")]
    WriterClosed,
}

#[async_trait]
pub trait Agent: Send + Sync + 'static {
    async fn request(
        &self,
        method: &str,
        params: Value,
        client: ClientConnection,
    ) -> Result<Value, RpcError>;

    async fn notification(
        &self,
        method: &str,
        params: Value,
        client: ClientConnection,
    ) -> Result<(), RpcError>;

    /// Notify the Agent before an in-flight client request future is dropped.
    ///
    /// The transport owns JSON-RPC request ids, while the Agent owns session and
    /// process-tree cancellation. Passing the original method and params lets the
    /// Agent abort that durable operation without teaching the transport about
    /// product-specific session fields.
    async fn request_cancelled(&self, _method: &str, _params: &Value) {}
}

enum Outbound {
    Frame {
        value: Value,
        sent: oneshot::Sender<Result<(), String>>,
    },
    Close {
        closed: oneshot::Sender<()>,
    },
}

/// Maximum number of encoded ACP frames waiting for the stdout writer.
///
/// The queue is lossless: once full, request tasks wait while the reader keeps
/// accepting client responses and the writer keeps draining frames. This bounds
/// memory without allowing a slow editor to make the adapter allocate forever.
pub const OUTBOUND_FRAME_CHANNEL_CAPACITY: usize = 64;

/// Maximum encoded JSON bytes accepted for one newline-delimited ACP frame.
pub const MAX_INBOUND_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Grace for already accepted requests to publish a ready response at clean EOF.
///
/// Editors commonly close stdin immediately after writing their final request.
/// A short drain keeps deterministic validation and lifecycle responses from
/// being replaced by cancellation, while truly blocked requests are still
/// cancelled promptly afterwards.
const EOF_REQUEST_DRAIN_GRACE: Duration = Duration::from_millis(25);

const UNINITIALIZED: u8 = 0;
const INITIALIZING: u8 = 1;
const INITIALIZED: u8 = 2;

type Pending = HashMap<String, oneshot::Sender<Result<Value, RpcError>>>;

struct InFlightRequest {
    cancel: oneshot::Sender<()>,
    method: String,
    params: Value,
    response_ready: Arc<AtomicBool>,
}

type InFlight = HashMap<String, InFlightRequest>;

#[derive(Default)]
struct PendingState {
    closed: bool,
    waiters: Pending,
}

#[derive(Default)]
struct DeferredState {
    response_sent: bool,
    notifications: Vec<(String, Value)>,
}

#[derive(Clone)]
pub struct ClientConnection {
    output: mpsc::Sender<Outbound>,
    pending: Arc<Mutex<PendingState>>,
    next_id: Arc<AtomicU64>,
    deferred: Option<Arc<Mutex<DeferredState>>>,
    scoped_requests: Option<Arc<Mutex<HashMap<String, Value>>>>,
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

    async fn response(&self, id: Value, result: Result<Value, RpcError>) -> Result<(), RpcError> {
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

    async fn close_output(&self) -> Result<(), RpcError> {
        let (closed_tx, closed_rx) = oneshot::channel();
        self.output
            .send(Outbound::Close { closed: closed_tx })
            .await
            .map_err(|_| RpcError::internal("ACP writer is closed"))?;
        closed_rx
            .await
            .map_err(|_| RpcError::internal("ACP writer stopped before closing"))
    }

    fn request_scoped(&self) -> Self {
        Self {
            output: self.output.clone(),
            pending: Arc::clone(&self.pending),
            next_id: Arc::clone(&self.next_id),
            deferred: Some(Arc::new(Mutex::new(DeferredState::default()))),
            scoped_requests: Some(Arc::new(Mutex::new(HashMap::new()))),
        }
    }

    async fn cancel_scoped_requests(&self) -> Result<(), RpcError> {
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

    async fn flush_after_response(&self) -> Result<(), RpcError> {
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

    fn resolve_response(&self, frame: &Value) {
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

    fn cancel_pending(&self, request_id: &Value) {
        let Some(request_id) = id_key(request_id) else {
            return;
        };
        let waiter = lock(&self.pending).waiters.remove(&request_id);
        if let Some(waiter) = waiter {
            let _ignored = waiter.send(Err(RpcError::cancelled("request cancelled")));
        }
    }

    fn close_pending(&self, error: RpcError) {
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

pub async fn serve_stdio<A>(agent: A) -> Result<(), ServeError>
where
    A: Agent,
{
    serve(agent, tokio::io::stdin(), tokio::io::stdout()).await
}

async fn serve<A, R, W>(agent: A, input: R, output: W) -> Result<(), ServeError>
where
    A: Agent,
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (output_tx, output_rx) = mpsc::channel(OUTBOUND_FRAME_CHANNEL_CAPACITY);
    let writer = tokio::spawn(write_frames(output, output_rx));
    let client = ClientConnection {
        output: output_tx,
        pending: Arc::new(Mutex::new(PendingState::default())),
        next_id: Arc::new(AtomicU64::new(1)),
        deferred: None,
        scoped_requests: None,
    };
    let agent = Arc::new(agent);
    let initialized = Arc::new(AtomicU8::new(UNINITIALIZED));
    let in_flight = Arc::new(Mutex::new(InFlight::new()));
    let mut reader = BufReader::new(input);
    let mut requests = JoinSet::new();
    let mut clean_eof = false;

    let loop_result = async {
        loop {
            let frame = match read_frame(&mut reader, MAX_INBOUND_FRAME_BYTES).await? {
                FrameRead::Eof => {
                    clean_eof = true;
                    break;
                }
                FrameRead::Oversized => {
                    client
                        .response(
                            Value::Null,
                            Err(RpcError::invalid_request(format!(
                                "ACP frame exceeds the {MAX_INBOUND_FRAME_BYTES}-byte limit"
                            ))),
                        )
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                    continue;
                }
                FrameRead::Frame(buffer) => match serde_json::from_slice::<Value>(&buffer) {
                    Ok(frame) => frame,
                    Err(error) => {
                        eprintln!("ACP parse error: {error}");
                        client
                            .response(Value::Null, Err(RpcError::new(-32700, "Parse error")))
                            .await
                            .map_err(|_| ServeError::WriterClosed)?;
                        continue;
                    }
                },
            };
            if frame.get("method").is_none() {
                client.resolve_response(&frame);
                continue;
            }
            let Some(method) = frame.get("method").and_then(Value::as_str) else {
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                client
                    .response(
                        id,
                        Err(RpcError::invalid_request("method must be a string")),
                    )
                    .await
                    .map_err(|_| ServeError::WriterClosed)?;
                continue;
            };
            if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                let id = frame.get("id").cloned().unwrap_or(Value::Null);
                client
                    .response(id, Err(RpcError::invalid_request("jsonrpc must be 2.0")))
                    .await
                    .map_err(|_| ServeError::WriterClosed)?;
                continue;
            }
            let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));

            if method == "$/cancel_request" {
                if let Some(request_id) = params.get("requestId") {
                    if let Some(request) = take_in_flight(&in_flight, request_id) {
                        agent
                            .request_cancelled(&request.method, &request.params)
                            .await;
                        let _ignored = request.cancel.send(());
                    }
                    client.cancel_pending(request_id);
                }
                continue;
            }

            if method.starts_with("session/") && initialized.load(Ordering::Acquire) != INITIALIZED
            {
                if let Some(id) = frame.get("id").cloned() {
                    client
                        .response(
                            id,
                            Err(RpcError::invalid_request(
                                "initialize must complete before session methods",
                            )),
                        )
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                }
                continue;
            }

            if let Some(id) = frame.get("id").cloned() {
                let Some(request_key) = id_key(&id) else {
                    client
                        .response(
                            Value::Null,
                            Err(RpcError::invalid_request("invalid request id")),
                        )
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                    continue;
                };
                if method == "initialize"
                    && initialized
                        .compare_exchange(
                            UNINITIALIZED,
                            INITIALIZING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                {
                    client
                        .response(
                            id,
                            Err(RpcError::invalid_request(
                                "initialize is already in progress or complete",
                            )),
                        )
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                    continue;
                }

                let request_method = method.to_owned();
                let (cancel_tx, cancel_rx) = oneshot::channel();
                let response_ready = Arc::new(AtomicBool::new(false));
                let duplicate = {
                    let mut active = lock(&in_flight);
                    if active.contains_key(&request_key) {
                        true
                    } else {
                        active.insert(
                            request_key.clone(),
                            InFlightRequest {
                                cancel: cancel_tx,
                                method: request_method.clone(),
                                params: params.clone(),
                                response_ready: Arc::clone(&response_ready),
                            },
                        );
                        false
                    }
                };
                if duplicate {
                    if method == "initialize" {
                        initialized.store(UNINITIALIZED, Ordering::Release);
                    }
                    client
                        .response(id, Err(RpcError::invalid_request("duplicate request id")))
                        .await
                        .map_err(|_| ServeError::WriterClosed)?;
                    continue;
                }

                let agent = Arc::clone(&agent);
                let client = client.clone();
                let initialized = Arc::clone(&initialized);
                let in_flight = Arc::clone(&in_flight);
                let method = request_method;
                requests.spawn(async move {
                    let request_client = client.request_scoped();
                    let result = tokio::select! {
                        result = agent.request(&method, params, request_client.clone()) => result,
                        _ = cancel_rx => {
                            if let Err(error) = request_client.cancel_scoped_requests().await {
                                eprintln!("ACP child request cancellation failed: {error}");
                            }
                            Err(RpcError::cancelled("request cancelled"))
                        },
                    };
                    response_ready.store(true, Ordering::Release);
                    if let Err(error) = request_client.cancel_scoped_requests().await {
                        eprintln!("ACP child request cleanup failed: {error}");
                    }
                    if method == "initialize" {
                        initialized.store(
                            if result.is_ok() {
                                INITIALIZED
                            } else {
                                UNINITIALIZED
                            },
                            Ordering::Release,
                        );
                    }
                    lock(&in_flight).remove(&request_key);
                    let succeeded = result.is_ok();
                    if let Err(error) = client.response(id, result).await {
                        eprintln!("ACP response failed: {error}");
                    } else if succeeded
                        && let Err(error) = request_client.flush_after_response().await
                    {
                        eprintln!("ACP deferred notification failed: {error}");
                    }
                });
            } else {
                if method == "initialize" {
                    continue;
                }
                let agent = Arc::clone(&agent);
                let client = client.clone();
                let method = method.to_owned();
                requests.spawn(async move {
                    if let Err(error) = agent.notification(&method, params, client).await {
                        eprintln!("ACP notification failed: {error}");
                    }
                });
            }
        }
        Ok::<(), ServeError>(())
    }
    .await;

    if clean_eof && loop_result.is_ok() {
        drain_accepted_requests_at_eof(&mut requests).await;
    }
    let cancellations = {
        let mut active = lock(&in_flight);
        let keys = active
            .iter()
            .filter(|(_, request)| {
                !clean_eof
                    || loop_result.is_err()
                    || !request.response_ready.load(Ordering::Acquire)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| active.remove(&key))
            .collect::<Vec<_>>()
    };
    for request in cancellations {
        agent
            .request_cancelled(&request.method, &request.params)
            .await;
        let _ignored = request.cancel.send(());
    }
    client.close_pending(RpcError::internal("ACP connection closed"));
    while requests.join_next().await.is_some() {}
    let close_output = client.close_output().await;
    drop(agent);
    drop(client);
    let writer_result = writer.await?;
    loop_result?;
    close_output.map_err(|_| ServeError::WriterClosed)?;
    writer_result?;
    Ok(())
}

async fn drain_accepted_requests_at_eof(requests: &mut JoinSet<()>) {
    let deadline = tokio::time::Instant::now() + EOF_REQUEST_DRAIN_GRACE;
    while !requests.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, requests.join_next()).await {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}

enum FrameRead {
    Eof,
    Frame(Vec<u8>),
    Oversized,
}

async fn read_frame<R>(reader: &mut R, limit: usize) -> Result<FrameRead, std::io::Error>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if oversized {
                FrameRead::Oversized
            } else if frame.is_empty() {
                FrameRead::Eof
            } else {
                FrameRead::Frame(frame)
            });
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if !oversized {
                if frame.len().saturating_add(newline) > limit {
                    oversized = true;
                } else {
                    frame.extend_from_slice(&available[..newline]);
                }
            }
            reader.consume(newline + 1);
            return Ok(if oversized {
                FrameRead::Oversized
            } else {
                FrameRead::Frame(frame)
            });
        }

        let available_len = available.len();
        if !oversized {
            if frame.len().saturating_add(available_len) > limit {
                oversized = true;
            } else {
                frame.extend_from_slice(available);
            }
        }
        reader.consume(available_len);
    }
}

fn take_in_flight(in_flight: &Mutex<InFlight>, request_id: &Value) -> Option<InFlightRequest> {
    let request_key = id_key(request_id)?;
    lock(in_flight).remove(&request_key)
}

async fn write_frames<W>(
    mut output: W,
    mut frames: mpsc::Receiver<Outbound>,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = frames.recv().await {
        match frame {
            Outbound::Frame { value, sent } => {
                let result = async {
                    let mut encoded = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
                    encoded.push(b'\n');
                    output.write_all(&encoded).await?;
                    output.flush().await
                }
                .await;
                let completion = result.as_ref().map(|_| ()).map_err(ToString::to_string);
                let _ignored = sent.send(completion);
                result?;
            }
            Outbound::Close { closed } => {
                let _ignored = closed.send(());
                break;
            }
        }
    }
    Ok(())
}

fn id_key(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".to_owned()),
        Value::String(value) => Some(format!("s:{value}")),
        Value::Number(value) if value.as_i64().is_some() || value.as_u64().is_some() => {
            Some(format!("n:{value}"))
        }
        Value::Bool(_) | Value::Array(_) | Value::Object(_) | Value::Number(_) => None,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingAgent {
        requests: Arc<Mutex<Vec<String>>>,
        notifications: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Agent for RecordingAgent {
        async fn request(
            &self,
            method: &str,
            _params: Value,
            _client: ClientConnection,
        ) -> Result<Value, RpcError> {
            lock(&self.requests).push(method.to_owned());
            Ok(json!({}))
        }

        async fn notification(
            &self,
            method: &str,
            _params: Value,
            _client: ClientConnection,
        ) -> Result<(), RpcError> {
            lock(&self.notifications).push(method.to_owned());
            Ok(())
        }
    }

    #[derive(Debug)]
    struct DeferredUpdateAgent;

    #[async_trait]
    impl Agent for DeferredUpdateAgent {
        async fn request(
            &self,
            _method: &str,
            _params: Value,
            client: ClientConnection,
        ) -> Result<Value, RpcError> {
            client.session_update_after_response(
                "ses_deferred",
                json!({
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": [],
                }),
            )?;
            Ok(json!({"ready": true}))
        }

        async fn notification(
            &self,
            method: &str,
            _params: Value,
            _client: ClientConnection,
        ) -> Result<(), RpcError> {
            Err(RpcError::method_not_found(method))
        }
    }

    #[tokio::test]
    async fn deferred_session_updates_are_written_after_the_request_response() {
        let frames = run_to_eof(
            DeferredUpdateAgent,
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n".to_vec(),
            2,
        )
        .await;

        assert_eq!(frames[0]["id"], 1);
        assert_eq!(frames[0]["result"]["ready"], true);
        assert_eq!(frames[1]["method"], "session/update");
        assert_eq!(frames[1]["params"]["sessionId"], "ses_deferred");
        assert_eq!(
            frames[1]["params"]["update"]["sessionUpdate"],
            "available_commands_update"
        );
    }

    #[derive(Debug, Default)]
    struct RetainingAgent {
        client: Mutex<Option<ClientConnection>>,
    }

    #[async_trait]
    impl Agent for RetainingAgent {
        async fn request(
            &self,
            _method: &str,
            _params: Value,
            client: ClientConnection,
        ) -> Result<Value, RpcError> {
            *lock(&self.client) = Some(client);
            Ok(json!({}))
        }

        async fn notification(
            &self,
            method: &str,
            _params: Value,
            _client: ClientConnection,
        ) -> Result<(), RpcError> {
            Err(RpcError::method_not_found(method))
        }
    }

    #[tokio::test]
    async fn eof_closes_the_writer_when_the_agent_retains_a_client_connection() {
        let frames = timeout(
            Duration::from_secs(1),
            run_to_eof(
                RetainingAgent::default(),
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n"
                    .to_vec(),
                1,
            ),
        )
        .await
        .expect("the retained client connection must not keep stdout open");
        assert_eq!(frames[0]["id"], 1);
    }

    async fn run_to_eof<A: Agent>(agent: A, input: Vec<u8>, expected_frames: usize) -> Vec<Value> {
        let capacity = input.len().max(1024);
        let (mut input_writer, input_reader) = tokio::io::duplex(capacity);
        let (output_writer, output_reader) = tokio::io::duplex(64 * 1024);
        let mut output = BufReader::new(output_reader);
        let server = tokio::spawn(serve(agent, input_reader, output_writer));
        input_writer
            .write_all(&input)
            .await
            .expect("write ACP input");

        let mut frames = Vec::with_capacity(expected_frames);
        for _ in 0..expected_frames {
            let mut line = String::new();
            timeout(Duration::from_secs(1), output.read_line(&mut line))
                .await
                .expect("ACP response arrives before EOF")
                .expect("read ACP response");
            assert!(
                !line.is_empty(),
                "ACP output closed before expected response"
            );
            frames.push(serde_json::from_str(&line).expect("ACP output is NDJSON"));
        }

        input_writer.shutdown().await.expect("close ACP input");
        let mut remainder = String::new();
        output
            .read_to_string(&mut remainder)
            .await
            .expect("read ACP output");
        server
            .await
            .expect("server task joins")
            .expect("server exits cleanly");
        frames.extend(
            remainder
                .lines()
                .map(|line| serde_json::from_str(line).expect("ACP output is NDJSON")),
        );
        frames
    }

    #[tokio::test]
    async fn session_methods_are_rejected_until_initialize_succeeds() {
        let agent = RecordingAgent::default();
        let requests = Arc::clone(&agent.requests);
        let notifications = Arc::clone(&agent.notifications);
        let frames = run_to_eof(
            agent,
            concat!(
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session/new\",\"params\":{}}\n",
                "{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\",\"params\":{}}\n",
            )
            .as_bytes()
            .to_vec(),
            1,
        )
        .await;

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["id"], 1);
        assert_eq!(frames[0]["error"]["code"], -32600);
        assert!(
            frames[0]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("initialize"))
        );
        assert!(lock(&requests).is_empty());
        assert!(lock(&notifications).is_empty());
    }

    #[tokio::test]
    async fn oversized_frames_are_bounded_rejected_and_the_stream_resynchronizes() {
        let mut input = vec![b' '; MAX_INBOUND_FRAME_BYTES + 1];
        input.push(b'\n');
        input.extend_from_slice(
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{}}\n",
        );
        let frames = run_to_eof(RecordingAgent::default(), input, 2).await;

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], Value::Null);
        assert_eq!(frames[0]["error"]["code"], -32600);
        assert!(
            frames[0]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("frame") && message.contains("limit"))
        );
        assert_eq!(frames[1]["id"], 2);
        assert!(frames[1].get("result").is_some());
    }

    #[derive(Debug)]
    struct BlockingAgent {
        started: Arc<Notify>,
        dropped: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
    }

    struct DropSignal(Arc<AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, AtomicOrdering::SeqCst);
        }
    }

    #[async_trait]
    impl Agent for BlockingAgent {
        async fn request(
            &self,
            method: &str,
            _params: Value,
            _client: ClientConnection,
        ) -> Result<Value, RpcError> {
            if method == "initialize" {
                return Ok(json!({}));
            }
            let _drop = DropSignal(Arc::clone(&self.dropped));
            self.started.notify_one();
            std::future::pending::<()>().await;
            unreachable!("the pending request is cancelled by the transport")
        }

        async fn notification(
            &self,
            method: &str,
            _params: Value,
            _client: ClientConnection,
        ) -> Result<(), RpcError> {
            Err(RpcError::method_not_found(method))
        }

        async fn request_cancelled(&self, method: &str, params: &Value) {
            if method == "session/prompt" && params["sessionId"] == "ses_cancel" {
                self.cancelled.store(true, AtomicOrdering::SeqCst);
            }
        }
    }

    #[tokio::test]
    async fn cancel_request_aborts_the_matching_in_flight_request() {
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let agent = BlockingAgent {
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
            cancelled: Arc::clone(&cancelled),
        };
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (output_writer, output_reader) = tokio::io::duplex(4096);
        let mut output = BufReader::new(output_reader);
        let server = tokio::spawn(serve(agent, input_reader, output_writer));

        input_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .expect("write initialize");
        let mut line = String::new();
        output
            .read_line(&mut line)
            .await
            .expect("read initialize response");
        assert_eq!(
            serde_json::from_str::<Value>(&line).expect("initialize response")["id"],
            1
        );

        input_writer
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"ses_cancel\"}}\n",
            )
            .await
            .expect("write prompt");
        started.notified().await;
        input_writer
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"$/cancel_request\",\"params\":{\"requestId\":2}}\n",
            )
            .await
            .expect("write cancellation");

        line.clear();
        timeout(Duration::from_secs(1), output.read_line(&mut line))
            .await
            .expect("cancelled request responds promptly")
            .expect("read cancellation response");
        let response: Value = serde_json::from_str(&line).expect("cancellation response");
        assert_eq!(response["id"], 2);
        assert_eq!(response["error"]["code"], -32800);
        assert!(cancelled.load(AtomicOrdering::SeqCst));
        assert!(dropped.load(AtomicOrdering::SeqCst));

        input_writer.shutdown().await.expect("close ACP input");
        timeout(Duration::from_secs(1), server)
            .await
            .expect("server exits")
            .expect("server task joins")
            .expect("server exits cleanly");
    }

    #[tokio::test]
    async fn clean_eof_aborts_in_flight_requests_before_joining() {
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let agent = BlockingAgent {
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
            cancelled: Arc::clone(&cancelled),
        };
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (output_writer, output_reader) = tokio::io::duplex(4096);
        let mut output = BufReader::new(output_reader);
        let server = tokio::spawn(serve(agent, input_reader, output_writer));

        input_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .expect("write initialize");
        let mut line = String::new();
        output
            .read_line(&mut line)
            .await
            .expect("read initialize response");

        input_writer
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{\"sessionId\":\"ses_cancel\"}}\n",
            )
            .await
            .expect("write prompt");
        started.notified().await;
        input_writer.shutdown().await.expect("close ACP input");

        timeout(Duration::from_secs(1), server)
            .await
            .expect("clean EOF cancels active requests")
            .expect("server task joins")
            .expect("server exits cleanly");
        assert!(cancelled.load(AtomicOrdering::SeqCst));
        assert!(dropped.load(AtomicOrdering::SeqCst));
    }

    #[derive(Debug)]
    struct YieldingInvalidParamsAgent;

    #[async_trait]
    impl Agent for YieldingInvalidParamsAgent {
        async fn request(
            &self,
            _method: &str,
            _params: Value,
            _client: ClientConnection,
        ) -> Result<Value, RpcError> {
            tokio::task::yield_now().await;
            Err(RpcError::invalid_params("invalid fixture parameters"))
        }

        async fn notification(
            &self,
            method: &str,
            _params: Value,
            _client: ClientConnection,
        ) -> Result<(), RpcError> {
            Err(RpcError::method_not_found(method))
        }
    }

    #[tokio::test]
    async fn clean_eof_drains_an_already_accepted_ready_response_before_cancelling() {
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (output_writer, output_reader) = tokio::io::duplex(4096);
        let mut output = BufReader::new(output_reader);
        let server = tokio::spawn(serve(
            YieldingInvalidParamsAgent,
            input_reader,
            output_writer,
        ));

        input_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .expect("write final request");
        input_writer.shutdown().await.expect("close ACP input");

        let mut line = String::new();
        timeout(Duration::from_secs(1), output.read_line(&mut line))
            .await
            .expect("ready response survives clean EOF")
            .expect("read ready response");
        let response: Value = serde_json::from_str(&line).expect("ACP response is JSON");
        assert_eq!(response["id"], 1);
        assert_eq!(response["error"]["code"], -32602);

        timeout(Duration::from_secs(1), server)
            .await
            .expect("server exits after draining ready response")
            .expect("server task joins")
            .expect("server exits cleanly");
    }

    #[derive(Debug)]
    struct ClientRequestAgent {
        request_started: Arc<Notify>,
    }

    #[async_trait]
    impl Agent for ClientRequestAgent {
        async fn request(
            &self,
            method: &str,
            _params: Value,
            client: ClientConnection,
        ) -> Result<Value, RpcError> {
            if method == "initialize" {
                return Ok(json!({}));
            }
            self.request_started.notify_one();
            client.request("client/check", json!({})).await
        }

        async fn notification(
            &self,
            method: &str,
            _params: Value,
            _client: ClientConnection,
        ) -> Result<(), RpcError> {
            Err(RpcError::method_not_found(method))
        }
    }

    #[tokio::test]
    async fn eof_fails_all_pending_client_requests_instead_of_hanging() {
        let request_started = Arc::new(Notify::new());
        let agent = ClientRequestAgent {
            request_started: Arc::clone(&request_started),
        };
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (output_writer, output_reader) = tokio::io::duplex(4096);
        let mut output = BufReader::new(output_reader);
        let server = tokio::spawn(serve(agent, input_reader, output_writer));

        input_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .expect("write initialize");
        let mut line = String::new();
        output
            .read_line(&mut line)
            .await
            .expect("read initialize response");

        input_writer
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{}}\n",
            )
            .await
            .expect("write prompt");
        request_started.notified().await;
        line.clear();
        output
            .read_line(&mut line)
            .await
            .expect("read agent-to-client request");
        let client_request: Value =
            serde_json::from_str(&line).expect("agent-to-client request is JSON");
        assert_eq!(client_request["method"], "client/check");

        input_writer.shutdown().await.expect("close ACP input");
        timeout(Duration::from_secs(1), server)
            .await
            .expect("EOF clears pending client requests")
            .expect("server task joins")
            .expect("server exits cleanly");
    }

    #[tokio::test]
    async fn cancel_request_cancels_agent_to_client_request() {
        let request_started = Arc::new(Notify::new());
        let agent = ClientRequestAgent {
            request_started: Arc::clone(&request_started),
        };
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (output_writer, output_reader) = tokio::io::duplex(4096);
        let mut output = BufReader::new(output_reader);
        let server = tokio::spawn(serve(agent, input_reader, output_writer));

        input_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .expect("write initialize");
        let mut line = String::new();
        output
            .read_line(&mut line)
            .await
            .expect("read initialize response");

        input_writer
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{}}\n",
            )
            .await
            .expect("write prompt");
        request_started.notified().await;
        line.clear();
        output
            .read_line(&mut line)
            .await
            .expect("read agent-to-client request");
        let client_request: Value =
            serde_json::from_str(&line).expect("agent-to-client request is JSON");
        let request_id = client_request["id"].clone();
        assert!(request_id.as_str().is_some());

        let cancellation = json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": { "requestId": request_id },
        });
        let mut cancellation = serde_json::to_vec(&cancellation).expect("encode cancellation");
        cancellation.push(b'\n');
        input_writer
            .write_all(&cancellation)
            .await
            .expect("write cancellation");

        line.clear();
        timeout(Duration::from_secs(1), output.read_line(&mut line))
            .await
            .expect("agent-to-client cancellation responds promptly")
            .expect("read prompt cancellation response");
        let response: Value = serde_json::from_str(&line).expect("cancellation response");
        assert_eq!(response["id"], 2);
        assert_eq!(response["error"]["code"], -32800);

        input_writer.shutdown().await.expect("close ACP input");
        timeout(Duration::from_secs(1), server)
            .await
            .expect("server exits")
            .expect("server task joins")
            .expect("server exits cleanly");
    }

    #[tokio::test]
    async fn cancelling_parent_prompt_cancels_its_pending_agent_to_client_request() {
        let request_started = Arc::new(Notify::new());
        let agent = ClientRequestAgent {
            request_started: Arc::clone(&request_started),
        };
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        let (output_writer, output_reader) = tokio::io::duplex(4096);
        let mut output = BufReader::new(output_reader);
        let server = tokio::spawn(serve(agent, input_reader, output_writer));

        input_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .expect("write initialize");
        let mut line = String::new();
        output
            .read_line(&mut line)
            .await
            .expect("read initialize response");

        input_writer
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/prompt\",\"params\":{}}\n",
            )
            .await
            .expect("write prompt");
        request_started.notified().await;
        line.clear();
        output
            .read_line(&mut line)
            .await
            .expect("read agent-to-client request");
        let client_request: Value =
            serde_json::from_str(&line).expect("agent-to-client request is JSON");
        let child_id = client_request["id"].clone();

        input_writer
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"$/cancel_request\",\"params\":{\"requestId\":2}}\n",
            )
            .await
            .expect("cancel parent prompt");

        let mut frames = Vec::new();
        while frames.len() < 2 {
            line.clear();
            timeout(Duration::from_secs(1), output.read_line(&mut line))
                .await
                .expect("cancellation frames arrive")
                .expect("read cancellation frame");
            frames.push(serde_json::from_str::<Value>(&line).expect("cancellation frame is JSON"));
        }
        assert!(frames.iter().any(|frame| {
            frame["method"] == "$/cancel_request" && frame["params"]["requestId"] == child_id
        }));
        assert!(
            frames
                .iter()
                .any(|frame| frame["id"] == 2 && frame["error"]["code"] == -32800)
        );

        input_writer.shutdown().await.expect("close ACP input");
        timeout(Duration::from_secs(1), server)
            .await
            .expect("server exits")
            .expect("server task joins")
            .expect("server exits cleanly");
    }
}
