use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
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
}

struct Outbound {
    value: Value,
    sent: oneshot::Sender<Result<(), String>>,
}

type Pending = HashMap<String, oneshot::Sender<Result<Value, RpcError>>>;

#[derive(Clone)]
pub struct ClientConnection {
    output: mpsc::UnboundedSender<Outbound>,
    pending: Arc<Mutex<Pending>>,
    next_id: Arc<AtomicU64>,
}

impl ClientConnection {
    pub async fn session_update(&self, session_id: &str, update: Value) -> Result<(), RpcError> {
        self.notify(
            "session/update",
            json!({ "sessionId": session_id, "update": update }),
        )
        .await
    }

    pub async fn request_permission(&self, params: Value) -> Result<Value, RpcError> {
        self.request("session/request_permission", params).await
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = format!("acp-agent-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let pending_id = format!("s:{id}");
        let (response_tx, response_rx) = oneshot::channel();
        lock(&self.pending).insert(pending_id.clone(), response_tx);
        if let Err(error) = self
            .send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .await
        {
            lock(&self.pending).remove(&pending_id);
            return Err(error);
        }
        response_rx
            .await
            .map_err(|_| RpcError::internal("ACP connection closed before client response"))?
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
            .send(Outbound {
                value,
                sent: sent_tx,
            })
            .map_err(|_| RpcError::internal("ACP writer is closed"))?;
        sent_rx
            .await
            .map_err(|_| RpcError::internal("ACP writer stopped"))?
            .map_err(RpcError::internal)
    }

    fn resolve_response(&self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(id_key) else {
            return;
        };
        let Some(waiter) = lock(&self.pending).remove(&id) else {
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
}

impl std::fmt::Debug for ClientConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientConnection")
            .field("pending", &lock(&self.pending).len())
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
    let (output_tx, output_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_frames(output, output_rx));
    let client = ClientConnection {
        output: output_tx,
        pending: Arc::new(Mutex::new(HashMap::new())),
        next_id: Arc::new(AtomicU64::new(1)),
    };
    let agent = Arc::new(agent);
    let mut reader = BufReader::new(input);
    let mut buffer = Vec::new();
    let mut requests = JoinSet::new();

    loop {
        buffer.clear();
        let bytes = reader.read_until(b'\n', &mut buffer).await?;
        if bytes == 0 {
            break;
        }
        let frame = match serde_json::from_slice::<Value>(&buffer) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("ACP parse error: {error}");
                client
                    .response(Value::Null, Err(RpcError::new(-32700, "Parse error")))
                    .await
                    .map_err(|_| ServeError::WriterClosed)?;
                continue;
            }
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
        if let Some(id) = frame.get("id").cloned() {
            if id_key(&id).is_none() {
                client
                    .response(
                        Value::Null,
                        Err(RpcError::invalid_request("invalid request id")),
                    )
                    .await
                    .map_err(|_| ServeError::WriterClosed)?;
                continue;
            }
            let agent = Arc::clone(&agent);
            let client = client.clone();
            let method = method.to_owned();
            requests.spawn(async move {
                let result = agent.request(&method, params, client.clone()).await;
                if let Err(error) = client.response(id, result).await {
                    eprintln!("ACP response failed: {error}");
                }
            });
        } else {
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

    while requests.join_next().await.is_some() {}
    drop(agent);
    drop(client);
    writer.await??;
    Ok(())
}

async fn write_frames<W>(
    mut output: W,
    mut frames: mpsc::UnboundedReceiver<Outbound>,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = frames.recv().await {
        let result = async {
            let mut encoded = serde_json::to_vec(&frame.value).map_err(std::io::Error::other)?;
            encoded.push(b'\n');
            output.write_all(&encoded).await?;
            output.flush().await
        }
        .await;
        let completion = result.as_ref().map(|_| ()).map_err(ToString::to_string);
        let _ignored = frame.sent.send(completion);
        result?;
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
