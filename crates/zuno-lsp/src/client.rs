//! JSON-RPC 2.0 over LSP's `Content-Length` framed stdio transport.
//!
//! One reader task owns stdout and demultiplexes responses by request id. This is
//! deliberately not a request/write/read lockstep client: language servers publish
//! diagnostics and issue reverse requests while ordinary requests are in flight.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, oneshot, watch};
use url::Url;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Capacity the framing buffer keeps once a message has been drained off it.
///
/// Protects against: one large server message — a whole-project diagnostics push,
/// a long completion list — pinning up to [`MAX_MESSAGE_BYTES`] resident for the
/// rest of the process. This buffer outlives any single stream: one framer per
/// language server, alive for as long as the server is. 64 MiB is 5.47% of M1's
/// 1,198,872 KiB W-real median in `docs/perf-methodology.md`, and it is the
/// largest single reusable buffer in the workspace.
///
/// Calibrated separately from `zuno_llm::buffer::STEADY_STATE_CAPACITY_BYTES`
/// rather than imported, because `zuno-lsp` has no business depending on the
/// model-provider crate. Both land on 64 KiB for the same reason: it clears
/// [`MAX_HEADER_BYTES`], so an ordinary request/response pair never reallocates.
/// Holding it costs 65,536 bytes per language server, 0.0053% of that median.
const STEADY_STATE_BUFFER_BYTES: usize = 64 * 1024;

type BoxWriter = Pin<Box<dyn AsyncWrite + Send>>;

/// A zero-based position in an LSP document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Zero-based line.
    pub line: u32,
    /// Zero-based UTF-16 character offset.
    pub character: u32,
}

/// A half-open range in an LSP document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    /// First included position.
    pub start: Position,
    /// First excluded position.
    pub end: Position,
}

/// A diagnostic published or returned by a language server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    /// Location the diagnostic applies to.
    pub range: Range,
    /// Optional LSP severity (`1` is error, `2` warning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<u32>,
    /// Server-defined numeric or string code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    /// Producer, such as `typescript` or `rustc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Human-readable explanation.
    pub message: String,
    /// Additional fields introduced by newer protocol versions or servers.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Failures at the transport and JSON-RPC layer.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Reading or writing the framed stream failed.
    #[error("LSP transport {operation} failed")]
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// OS error.
        #[source]
        source: io::Error,
    },
    /// A frame or JSON-RPC payload was malformed.
    #[error("LSP protocol message is invalid")]
    Protocol {
        /// Parse failure.
        #[source]
        source: serde_json::Error,
    },
    /// A frame header was syntactically invalid or exceeded a safety bound.
    #[error("invalid LSP frame header: {reason}")]
    Framing {
        /// Static classification of the framing defect.
        reason: &'static str,
    },
    /// The peer returned a JSON-RPC error object.
    #[error("LSP request failed with code {code}: {message}")]
    Remote {
        /// JSON-RPC error code.
        code: i64,
        /// Peer-provided text.
        message: String,
        /// Optional peer-provided detail.
        data: Option<Value>,
    },
    /// The reader reached EOF before the operation completed.
    #[error("language server connection closed")]
    Closed,
    /// The peer did not answer within the operation's budget.
    #[error("language server did not respond within {elapsed:?}")]
    Timeout {
        /// Timeout budget.
        elapsed: Duration,
    },
    /// A filesystem path could not be represented as a `file:` URI.
    #[error("path cannot be represented as a file URI: {path}")]
    InvalidFileUri {
        /// Offending path.
        path: PathBuf,
    },
}

#[derive(Debug, Clone)]
enum ResponseFailure {
    Remote {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    Closed,
}

impl From<ResponseFailure> for ClientError {
    fn from(value: ResponseFailure) -> Self {
        match value {
            ResponseFailure::Remote {
                code,
                message,
                data,
            } => Self::Remote {
                code,
                message,
                data,
            },
            ResponseFailure::Closed => Self::Closed,
        }
    }
}

#[derive(Debug, Clone)]
struct DocumentState {
    version: i64,
    text: String,
}

struct Inner {
    server_id: String,
    root: PathBuf,
    initialization: Value,
    writer: Mutex<BoxWriter>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, ResponseFailure>>>>,
    diagnostics: Mutex<BTreeMap<PathBuf, Vec<Diagnostic>>>,
    diagnostic_epochs: Mutex<BTreeMap<PathBuf, u64>>,
    diagnostic_changed: Notify,
    documents: Mutex<BTreeMap<PathBuf, DocumentState>>,
    closed: watch::Sender<bool>,
    /// The task that owns the server's stdout, until its owner takes it to settle it.
    ///
    /// A `std` mutex: it is only ever locked to move the handle out, never across an await.
    reader: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// A connected, initialized language-server client.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("server_id", &self.inner.server_id)
            .field("root", &self.inner.root)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Start the reader, perform `initialize`, then send `initialized`.
    pub async fn connect<R, W>(
        server_id: impl Into<String>,
        root: impl Into<PathBuf>,
        process_id: Option<u32>,
        reader: R,
        writer: W,
        initialization: Value,
    ) -> Result<Self, ClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (closed, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            server_id: server_id.into(),
            root: root.into(),
            initialization,
            writer: Mutex::new(Box::pin(writer)),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(BTreeMap::new()),
            diagnostic_epochs: Mutex::new(BTreeMap::new()),
            diagnostic_changed: Notify::new(),
            documents: Mutex::new(BTreeMap::new()),
            closed,
            reader: std::sync::Mutex::new(None),
        });
        let client = Self {
            inner: Arc::clone(&inner),
        };
        let reader = tokio::spawn(async move {
            if let Err(error) = read_loop(reader, Arc::clone(&inner)).await {
                tracing::warn!(server = %inner.server_id, %error, "language server reader stopped");
            }
            close_pending(&inner).await;
        });
        *lock_reader(&client.inner) = Some(reader);

        if let Err(error) = client.handshake(process_id).await {
            // No caller ever sees this client, so nobody could take the reader and settle it.
            // Left alone it would run until the server's stdout reaches EOF, which a server
            // that leaked a helper holding that pipe never delivers.
            if let Some(reader) = client.take_reader() {
                reader.abort();
            }
            return Err(error);
        }
        Ok(client)
    }

    /// The `initialize` round trip and the notifications that follow it.
    async fn handshake(&self, process_id: Option<u32>) -> Result<(), ClientError> {
        let client = self;
        let root_uri = file_uri(&client.inner.root)?;
        let initialized = client
            .request_with_timeout(
                "initialize",
                json!({
                    "rootUri": root_uri,
                    "processId": process_id,
                    "workspaceFolders": [{ "name": "workspace", "uri": root_uri }],
                    "initializationOptions": client.inner.initialization,
                    "capabilities": {
                        "window": { "workDoneProgress": true },
                        "workspace": {
                            "configuration": true,
                            "workspaceFolders": true,
                            "didChangeWatchedFiles": { "dynamicRegistration": true },
                            "diagnostics": { "refreshSupport": false }
                        },
                        "textDocument": {
                            "synchronization": { "didOpen": true, "didClose": true, "didChange": true },
                            "diagnostic": { "dynamicRegistration": true, "relatedDocumentSupport": true },
                            "publishDiagnostics": { "versionSupport": false }
                        }
                    }
                }),
                INITIALIZE_TIMEOUT,
            )
            .await?;
        let _capabilities = initialized.get("capabilities");
        client.notify("initialized", json!({})).await?;
        if !client.inner.initialization.is_null() {
            client
                .notify(
                    "workspace/didChangeConfiguration",
                    json!({ "settings": client.inner.initialization }),
                )
                .await?;
        }
        Ok(())
    }

    /// Hands the reader task to whoever owns the server's lifecycle, exactly once.
    ///
    /// The reader returns only when the server's stdout reaches EOF, and EOF needs every process
    /// holding that pipe to have closed it. The owner that reaps the server settles the reader
    /// under its own ceiling and aborts it if the pipe is still held; a second call, or a call
    /// after a failed handshake, gets `None`.
    #[must_use]
    pub fn take_reader(&self) -> Option<tokio::task::JoinHandle<()>> {
        lock_reader(&self.inner).take()
    }

    /// Registry id of the connected server.
    #[must_use]
    pub fn server_id(&self) -> &str {
        &self.inner.server_id
    }

    /// Workspace root used during initialization.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Subscribe to transport closure. The current value is true after EOF.
    #[must_use]
    pub fn closed(&self) -> watch::Receiver<bool> {
        self.inner.closed.subscribe()
    }

    /// Send an ordinary JSON-RPC request with the default timeout.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT)
            .await
    }

    /// Send a JSON-RPC request with an explicit timeout.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        elapsed: Duration,
    ) -> Result<Value, ClientError> {
        if *self.inner.closed.borrow() {
            return Err(ClientError::Closed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, sender);
        if let Err(error) = send_message(
            &self.inner,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        )
        .await
        {
            self.inner.pending.lock().await.remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(elapsed, receiver).await {
            Ok(Ok(result)) => result.map_err(ClientError::from),
            Ok(Err(_)) => Err(ClientError::Closed),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(ClientError::Timeout { elapsed })
            }
        }
    }

    /// Send a JSON-RPC notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), ClientError> {
        send_message(
            &self.inner,
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        )
        .await
    }

    /// Open a file, or send a full-content change when it is already open.
    /// Returns the new document version and the diagnostic epoch observed before
    /// the notification, which can be passed to [`Self::wait_for_diagnostics`].
    pub async fn open_or_change(&self, path: &Path) -> Result<(i64, u64), ClientError> {
        let path = absolute(path).await?;
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|source| ClientError::Io {
                operation: "read document",
                source,
            })?;
        let uri = file_uri(&path)?;
        let before = self.diagnostic_epoch(&path).await;
        let mut documents = self.inner.documents.lock().await;
        let version = if let Some(document) = documents.get_mut(&path) {
            document.version += 1;
            document.text.clone_from(&text);
            let version = document.version;
            drop(documents);
            self.notify(
                "workspace/didChangeWatchedFiles",
                json!({ "changes": [{ "uri": uri, "type": 2 }] }),
            )
            .await?;
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }]
                }),
            )
            .await?;
            version
        } else {
            documents.insert(
                path.clone(),
                DocumentState {
                    version: 0,
                    text: text.clone(),
                },
            );
            drop(documents);
            self.notify(
                "workspace/didChangeWatchedFiles",
                json!({ "changes": [{ "uri": uri, "type": 1 }] }),
            )
            .await?;
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id(&path),
                        "version": 0,
                        "text": text
                    }
                }),
            )
            .await?;
            0
        };
        Ok((version, before))
    }

    /// Close a previously opened file. Closing an unopened path is a no-op.
    pub async fn close_document(&self, path: &Path) -> Result<(), ClientError> {
        let path = absolute(path).await?;
        if self.inner.documents.lock().await.remove(&path).is_some() {
            self.notify(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": file_uri(&path)? } }),
            )
            .await?;
        }
        Ok(())
    }

    /// Wait until the server publishes a diagnostic set newer than `after_epoch`.
    /// An empty publication counts: it is the authoritative "no diagnostics" result.
    pub async fn wait_for_diagnostics(
        &self,
        path: &Path,
        after_epoch: u64,
    ) -> Result<Vec<Diagnostic>, ClientError> {
        let path = absolute(path).await?;
        let wait = async {
            loop {
                let notified = self.inner.diagnostic_changed.notified();
                if self.diagnostic_epoch(&path).await > after_epoch {
                    return Ok(self.diagnostics_for(&path).await);
                }
                if *self.inner.closed.borrow() {
                    return Err(ClientError::Closed);
                }
                notified.await;
            }
        };
        tokio::time::timeout(DIAGNOSTICS_TIMEOUT, wait)
            .await
            .map_err(|_| ClientError::Timeout {
                elapsed: DIAGNOSTICS_TIMEOUT,
            })?
    }

    /// Snapshot diagnostics for one path.
    pub async fn diagnostics_for(&self, path: &Path) -> Vec<Diagnostic> {
        self.inner
            .diagnostics
            .lock()
            .await
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    /// Snapshot every path for which this server has published diagnostics.
    pub async fn diagnostics(&self) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        self.inner.diagnostics.lock().await.clone()
    }

    /// Ask the server to shut down cleanly, then send the `exit` notification.
    pub async fn shutdown(&self) {
        let _result = self
            .request_with_timeout("shutdown", Value::Null, Duration::from_secs(2))
            .await;
        let _result = self.notify("exit", Value::Null).await;
    }

    async fn diagnostic_epoch(&self, path: &Path) -> u64 {
        self.inner
            .diagnostic_epochs
            .lock()
            .await
            .get(path)
            .copied()
            .unwrap_or(0)
    }
}

async fn read_loop<R>(mut reader: R, inner: Arc<Inner>) -> Result<(), ClientError>
where
    R: AsyncRead + Unpin,
{
    let mut framer = Framer::default();
    let mut chunk = [0_u8; 8192];
    loop {
        while let Some(message) = framer.next_message()? {
            dispatch_message(&inner, message).await?;
        }
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|source| ClientError::Io {
                operation: "read",
                source,
            })?;
        if read == 0 {
            return Ok(());
        }
        framer.push(&chunk[..read]);
    }
}

async fn dispatch_message(inner: &Arc<Inner>, message: Value) -> Result<(), ClientError> {
    if let Some(id) = message.get("id").and_then(Value::as_u64)
        && message.get("method").is_none()
    {
        if let Some(sender) = inner.pending.lock().await.remove(&id) {
            let result = if let Some(error) = message.get("error") {
                Err(ResponseFailure::Remote {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(-32_603),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("language server returned an error")
                        .to_owned(),
                    data: error.get("data").cloned(),
                })
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            let _result = sender.send(result);
        }
        return Ok(());
    }

    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(());
    };
    if method == "textDocument/publishDiagnostics" {
        publish_diagnostics(inner, message.get("params")).await?;
        return Ok(());
    }
    if let Some(id) = message.get("id").cloned() {
        let result = reverse_request_result(inner, method, message.get("params"));
        send_message(
            inner,
            &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        )
        .await?;
    }
    Ok(())
}

fn reverse_request_result(inner: &Inner, method: &str, params: Option<&Value>) -> Value {
    match method {
        "workspace/configuration" => {
            let items = params
                .and_then(|value| value.get("items"))
                .and_then(Value::as_array);
            Value::Array(
                items
                    .into_iter()
                    .flatten()
                    .map(|item| {
                        item.get("section").and_then(Value::as_str).map_or_else(
                            || inner.initialization.clone(),
                            |section| configuration_value(&inner.initialization, section),
                        )
                    })
                    .collect(),
            )
        }
        "workspace/workspaceFolders" => file_uri(&inner.root).map_or(
            Value::Null,
            |uri| json!([{ "name": "workspace", "uri": uri }]),
        ),
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "workspace/diagnostic/refresh" => Value::Null,
        _ => Value::Null,
    }
}

fn configuration_value(settings: &Value, section: &str) -> Value {
    let mut value = settings;
    for key in section.split('.') {
        let Some(next) = value.get(key) else {
            return Value::Null;
        };
        value = next;
    }
    value.clone()
}

async fn publish_diagnostics(inner: &Inner, params: Option<&Value>) -> Result<(), ClientError> {
    let params = params.cloned().unwrap_or(Value::Null);
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(path) = path_from_file_uri(uri) else {
        return Ok(());
    };
    let diagnostics = serde_json::from_value::<Vec<Diagnostic>>(
        params
            .get("diagnostics")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
    )
    .map_err(|source| ClientError::Protocol { source })?;
    inner
        .diagnostics
        .lock()
        .await
        .insert(path.clone(), diagnostics);
    let mut epochs = inner.diagnostic_epochs.lock().await;
    let epoch = epochs.entry(path).or_default();
    *epoch = epoch.saturating_add(1);
    drop(epochs);
    inner.diagnostic_changed.notify_waiters();
    Ok(())
}

fn lock_reader(inner: &Inner) -> std::sync::MutexGuard<'_, Option<tokio::task::JoinHandle<()>>> {
    inner
        .reader
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn close_pending(inner: &Inner) {
    let _result = inner.closed.send(true);
    let pending = std::mem::take(&mut *inner.pending.lock().await);
    for sender in pending.into_values() {
        let _result = sender.send(Err(ResponseFailure::Closed));
    }
    inner.diagnostic_changed.notify_waiters();
}

async fn send_message(inner: &Inner, message: &Value) -> Result<(), ClientError> {
    let body = serde_json::to_vec(message).map_err(|source| ClientError::Protocol { source })?;
    let mut writer = inner.writer.lock().await;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await
        .map_err(|source| ClientError::Io {
            operation: "write header",
            source,
        })?;
    writer
        .write_all(&body)
        .await
        .map_err(|source| ClientError::Io {
            operation: "write body",
            source,
        })?;
    writer.flush().await.map_err(|source| ClientError::Io {
        operation: "flush",
        source,
    })
}

#[derive(Debug, Default)]
struct Framer {
    buffer: Vec<u8>,
    body_len: Option<usize>,
}

impl Framer {
    fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    fn next_message(&mut self) -> Result<Option<Value>, ClientError> {
        if self.body_len.is_none() {
            let Some(header_end) = find_subsequence(&self.buffer, b"\r\n\r\n") else {
                if self.buffer.len() > MAX_HEADER_BYTES {
                    return Err(ClientError::Framing {
                        reason: "header exceeds 16 KiB",
                    });
                }
                return Ok(None);
            };
            let header = std::str::from_utf8(&self.buffer[..header_end]).map_err(|_| {
                ClientError::Framing {
                    reason: "header is not UTF-8",
                }
            })?;
            let length = header
                .split("\r\n")
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("Content-Length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .ok_or(ClientError::Framing {
                    reason: "missing or invalid Content-Length",
                })?;
            if length > MAX_MESSAGE_BYTES {
                return Err(ClientError::Framing {
                    reason: "message exceeds 64 MiB",
                });
            }
            self.buffer.drain(..header_end + 4);
            self.body_len = Some(length);
        }

        let length = self.body_len.unwrap_or_default();
        if self.buffer.len() < length {
            return Ok(None);
        }
        let body: Vec<u8> = self.buffer.drain(..length).collect();
        self.body_len = None;
        if self.buffer.capacity() > STEADY_STATE_BUFFER_BYTES {
            self.buffer.shrink_to(STEADY_STATE_BUFFER_BYTES);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|source| ClientError::Protocol { source })
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn file_uri(path: &Path) -> Result<String, ClientError> {
    Url::from_file_path(path)
        .map(String::from)
        .map_err(|()| ClientError::InvalidFileUri {
            path: path.to_path_buf(),
        })
}

fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    Url::parse(uri).ok()?.to_file_path().ok()
}

async fn absolute(path: &Path) -> Result<PathBuf, ClientError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = tokio::task::spawn_blocking(std::env::current_dir)
        .await
        .map_err(|_| ClientError::Framing {
            reason: "current-directory task was cancelled",
        })?
        .map_err(|source| ClientError::Io {
            operation: "resolve current directory",
            source,
        })?;
    Ok(cwd.join(path))
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => "rust",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("jsx") => "javascriptreact",
        Some("go") => "go",
        Some("py" | "pyi") => "python",
        Some("java") => "java",
        Some("kt" | "kts") => "kotlin",
        Some("c") => "c",
        Some("cpp" | "cc" | "cxx" | "c++") => "cpp",
        Some("cs" | "csx") => "csharp",
        Some("rb" | "rake" | "gemspec" | "ru") => "ruby",
        Some("sh" | "bash" | "zsh" | "ksh") => "shellscript",
        Some("yaml" | "yml") => "yaml",
        Some("lua") => "lua",
        Some("dart") => "dart",
        Some("swift") => "swift",
        Some("ex" | "exs") => "elixir",
        Some("zig" | "zon") => "zig",
        Some("vue") => "vue",
        Some("svelte") => "svelte",
        Some("astro") => "astro",
        Some("tf") => "terraform",
        Some("tfvars") => "terraform-vars",
        Some("nix") => "nix",
        Some("typ" | "typc") => "typst",
        Some("tex") => "latex",
        Some("bib") => "bibtex",
        Some("json" | "jsonc") => "json",
        Some("css") => "css",
        Some("html" | "htm") => "html",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{DuplexStream, duplex};

    fn framed(value: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).expect("test JSON serializes");
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend(body);
        frame
    }

    async fn read_frame(stream: &mut DuplexStream) -> Value {
        let mut framer = Framer::default();
        let mut chunk = [0_u8; 1];
        loop {
            if let Some(value) = framer.next_message().expect("valid client frame") {
                return value;
            }
            let count = stream.read(&mut chunk).await.expect("read client frame");
            assert_ne!(count, 0, "client closed before writing a frame");
            framer.push(&chunk[..count]);
        }
    }

    /// The framer outlives every message, so its capacity is a process-lifetime cost.
    #[test]
    fn a_large_server_message_does_not_strand_its_buffer_for_the_process() {
        let payload = "d".repeat(4 * 1024 * 1024);
        let large = json!({"jsonrpc":"2.0","method":"diagnostics","params":{"text":payload}});
        let mut framer = Framer::default();
        framer.push(&framed(&large));

        let delivered = framer
            .next_message()
            .expect("a 4 MiB message is under the 64 MiB cap")
            .expect("the message itself must still be delivered");
        assert_eq!(delivered["method"], "diagnostics");

        assert!(
            framer.buffer.capacity() <= STEADY_STATE_BUFFER_BYTES,
            "the framer holds {} bytes of capacity with {} live bytes, against a \
             {STEADY_STATE_BUFFER_BYTES}-byte floor",
            framer.buffer.capacity(),
            framer.buffer.len()
        );
    }

    #[test]
    fn ordinary_traffic_after_a_large_message_never_reallocates() {
        let payload = "e".repeat(4 * 1024 * 1024);
        let mut framer = Framer::default();
        framer.push(&framed(
            &json!({"jsonrpc":"2.0","method":"big","params":{"text":payload}}),
        ));
        let _ = framer.next_message().expect("the large frame parses");
        let settled = framer.buffer.capacity();

        for id in 0..500 {
            framer.push(&framed(
                &json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}}),
            ));
            assert!(
                framer
                    .next_message()
                    .expect("a small frame parses")
                    .is_some()
            );
        }

        assert_eq!(
            framer.buffer.capacity(),
            settled,
            "steady-state framing reallocated after the large message"
        );
    }

    #[test]
    fn framing_accepts_split_headers_and_multiple_messages() {
        let first = json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        let second = json!({"jsonrpc":"2.0","method":"tick","params":{}});
        let mut bytes = framed(&first);
        bytes.extend(framed(&second));
        let split = 11;
        let mut framer = Framer::default();
        framer.push(&bytes[..split]);
        assert!(
            framer
                .next_message()
                .expect("partial frame is valid")
                .is_none()
        );
        framer.push(&bytes[split..]);
        assert_eq!(framer.next_message().expect("first frame"), Some(first));
        assert_eq!(framer.next_message().expect("second frame"), Some(second));
        assert!(framer.next_message().expect("buffer is empty").is_none());
    }

    #[tokio::test]
    async fn responses_are_demultiplexed_when_the_server_replies_out_of_order() {
        let (client_read, mut server_write) = duplex(4096);
        let (mut server_read, client_write) = duplex(4096);
        let server = tokio::spawn(async move {
            let initialize = read_frame(&mut server_read).await;
            let initialize_id = initialize["id"].as_u64().expect("initialize id");
            server_write
                .write_all(&framed(&json!({
                    "jsonrpc":"2.0", "id":initialize_id, "result":{"capabilities":{}}
                })))
                .await
                .expect("initialize response");
            let _initialized = read_frame(&mut server_read).await;
            let one = read_frame(&mut server_read).await;
            let two = read_frame(&mut server_read).await;
            server_write
                .write_all(&framed(&json!({
                    "jsonrpc":"2.0", "id":two["id"], "result":"second"
                })))
                .await
                .expect("second response");
            server_write
                .write_all(&framed(&json!({
                    "jsonrpc":"2.0", "id":one["id"], "result":"first"
                })))
                .await
                .expect("first response");
        });
        let client = Client::connect(
            "test",
            std::env::temp_dir(),
            None,
            client_read,
            client_write,
            Value::Null,
        )
        .await
        .expect("connect client");
        let (one, two) = tokio::join!(
            client.request("one", json!({})),
            client.request("two", json!({}))
        );
        assert_eq!(one.expect("first result"), json!("first"));
        assert_eq!(two.expect("second result"), json!("second"));
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn diagnostics_notifications_are_not_mistaken_for_responses() {
        let directory = tempfile::tempdir().expect("temp directory");
        let source = directory.path().join("main.ts");
        tokio::fs::write(&source, "const n: number = 'x';\n")
            .await
            .expect("fixture");
        let uri = file_uri(&source).expect("file URI");
        let (client_read, mut server_write) = duplex(8192);
        let (mut server_read, client_write) = duplex(8192);
        let server = tokio::spawn(async move {
            let initialize = read_frame(&mut server_read).await;
            server_write
                .write_all(&framed(&json!({
                    "jsonrpc":"2.0", "id":initialize["id"], "result":{"capabilities":{}}
                })))
                .await
                .expect("initialize response");
            let _initialized = read_frame(&mut server_read).await;
            let _watch = read_frame(&mut server_read).await;
            let _open = read_frame(&mut server_read).await;
            server_write
                .write_all(&framed(&json!({
                    "jsonrpc":"2.0",
                    "method":"textDocument/publishDiagnostics",
                    "params":{
                        "uri":uri,
                        "diagnostics":[{
                            "range":{"start":{"line":0,"character":6},"end":{"line":0,"character":7}},
                            "severity":1,
                            "message":"not a number",
                            "source":"test"
                        }]
                    }
                })))
                .await
                .expect("diagnostics");
        });
        let client = Client::connect(
            "test",
            directory.path(),
            None,
            client_read,
            client_write,
            Value::Null,
        )
        .await
        .expect("connect client");
        let (_version, epoch) = client.open_or_change(&source).await.expect("open file");
        let diagnostics = client
            .wait_for_diagnostics(&source, epoch)
            .await
            .expect("published diagnostics");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "not a number");
        server.await.expect("server task");
    }
}
