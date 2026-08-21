//! MCP over a child's standard input and output.
//!
//! Every wire message is one UTF-8 JSON value followed by `\n`. The single reader
//! owns stdout and dispatches responses through an id-indexed waiter map; requests
//! never read stdout themselves. That separation is load-bearing: MCP servers may
//! send notifications at any point, and treating the next line as the current
//! request's response permanently desynchronizes the connection.
//!
//! The framing was validated against `codegraph serve --mcp` 0.42.9 rather than a
//! fixture authored with this client. The live test at the bottom initializes the
//! real server, lists its tools, and calls `codegraph_status`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use zuno_config::schema::mcp::McpLocal;
use zuno_error::McpError;

use crate::catalog::{PromptDefinition, ResourceContents, ResourceDefinition, ResourceTemplate};
use crate::protocol::{
    ExchangeError, Pending, ReaderFailure, decode_error, decode_response, fail_pending, lock,
    route_message,
};

/// Protocol version proven against the real server used by the live test.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Runtime fallback used by the TypeScript client (`mcp/index.ts:38,359`).
///
/// The config schema's prose still says five seconds (`config/mcp.ts:20-22`), but
/// the executable path has used this 30-second constant since before connection.
/// Runtime behavior wins over stale schema documentation.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const MAX_LIST_PAGES: usize = 1_000;
const NOTIFICATION_CAPACITY: usize = 64;
const TOOLS_CHANGED_CAPACITY: usize = 16;
const TASK_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
const PROCESS_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

type DynReader = Pin<Box<dyn AsyncRead + Send>>;
type DynWriter = Pin<Box<dyn AsyncWrite + Send>>;

/// Server implementation metadata returned by `initialize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplementationInfo {
    /// Stable implementation name.
    pub name: String,
    /// Server version string.
    pub version: String,
    /// Fields added by later protocol revisions remain available to callers.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The server's successful `initialize` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// Protocol revision selected by the server.
    pub protocol_version: String,
    /// Capability object, retained without narrowing so newer servers stay usable.
    pub capabilities: Value,
    /// Server identity.
    pub server_info: ImplementationInfo,
    /// Optional server-supplied instructions. `codegraph` sends a long one, which
    /// is why the live test also guards against assuming a fixed initialize shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Fields introduced after this client was built.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One tool advertised by an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    /// Name passed back to `tools/call` without opencode namespacing.
    pub name: String,
    /// Human-readable purpose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for call arguments.
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    /// Optional JSON Schema for structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Annotations and future fields are preserved for todo 46's registry adapter.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Successful or tool-level-error payload returned by `tools/call`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallResult {
    /// Content blocks returned by the tool.
    #[serde(default)]
    pub content: Vec<Value>,
    /// Machine-readable output when the tool declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    /// A JSON-RPC success may still carry a tool-level error.
    #[serde(default)]
    pub is_error: bool,
    /// Future result fields remain available to the registry adapter.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A server-initiated notification (a JSON-RPC message with no `id`).
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// Notification method, such as `notifications/tools/list_changed`.
    pub method: String,
    /// Params, or JSON null when the server omitted them.
    pub params: Value,
}

/// A refreshed tool snapshot caused by `notifications/tools/list_changed`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolsChanged {
    /// The complete post-refresh tool list, including all cursor pages.
    pub tools: Vec<ToolDefinition>,
}

/// A connected MCP stdio client.
///
/// Clones share one child, one request-id sequence, one waiter map, and one tool
/// cache. Dropping the final clone signals the supervisor, which kills and reaps
/// the child. Call [`Self::close`] when shutdown ordering itself matters.
#[derive(Clone)]
pub struct StdioClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for StdioClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StdioClient")
            .field("server", &self.inner.server)
            .field("timeout", &self.inner.timeout)
            .field("closed", &self.inner.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

struct Inner {
    server: String,
    timeout: Duration,
    next_id: AtomicU64,
    writer: tokio::sync::Mutex<Option<DynWriter>>,
    pending: Pending,
    notifications: broadcast::Sender<Notification>,
    tools_changed: broadcast::Sender<ToolsChanged>,
    tools: RwLock<Vec<ToolDefinition>>,
    initialization: OnceLock<InitializeResult>,
    process: Option<ProcessControl>,
    tasks: Mutex<BackgroundTasks>,
    closed: AtomicBool,
}

#[derive(Default)]
struct BackgroundTasks {
    reader: Option<JoinHandle<()>>,
    refresh: Option<JoinHandle<()>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        fail_pending(&self.pending, ReaderFailure::Closed);
        if let Some(process) = &self.process {
            process.signal();
        }
        let mut tasks = lock(&self.tasks);
        if let Some(task) = tasks.reader.take() {
            task.abort();
        }
        if let Some(task) = tasks.refresh.take() {
            task.abort();
        }
    }
}

impl StdioClient {
    /// Spawns a configured local server, initializes it, and sends
    /// `notifications/initialized`.
    ///
    /// `cwd` is resolved lexically from `workspace`, matching
    /// `mcp/index.ts:344-356`. The child inherits the process environment;
    /// `BUN_BE_BUN=1` is then applied for an `opencode` command, and configured
    /// `environment` entries are applied last, so user configuration wins.
    ///
    /// # Errors
    ///
    /// [`McpError::Connect`] for invalid process configuration or spawn failure,
    /// [`McpError::Handshake`] for an initialize failure,
    /// [`McpError::Protocol`] for malformed JSON, and [`McpError::Timeout`] when
    /// the configured per-server deadline expires.
    pub async fn connect(
        server: impl Into<String>,
        workspace: impl AsRef<Path>,
        config: &McpLocal,
    ) -> Result<Self, McpError> {
        let server = server.into();
        let timeout = config.timeout.map_or(DEFAULT_REQUEST_TIMEOUT, |value| {
            Duration::from_millis(u64::from(value.get()))
        });
        let mut command =
            build_command(workspace.as_ref(), config).map_err(|source| McpError::Connect {
                server: server.clone(),
                source: Box::new(source),
            })?;
        let command_name = config
            .command
            .first()
            .cloned()
            .unwrap_or_else(|| "<empty>".to_owned());
        let mut child = command.spawn().map_err(|source| McpError::Connect {
            server: server.clone(),
            source: Box::new(io::Error::new(
                source.kind(),
                format!("could not spawn {command_name}: {source}"),
            )),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Connect {
            server: server.clone(),
            source: Box::new(io::Error::other("spawned MCP child has no stdin")),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Connect {
            server: server.clone(),
            source: Box::new(io::Error::other("spawned MCP child has no stdout")),
        })?;
        if let Some(stderr) = child.stderr.take() {
            spawn_stderr_reader(server.clone(), stderr);
        }
        let process = ProcessControl::new(server.clone(), child);
        let client = Self::from_io(server, stdout, stdin, timeout, Some(process));

        let initialized = match client.initialize().await {
            Ok(initialized) => initialized,
            Err(error) => {
                client.close().await;
                return Err(error);
            }
        };
        let _already_initialized = client.inner.initialization.set(initialized);
        if let Err(error) = client.send_initialized().await {
            client.close().await;
            return Err(error);
        }
        Ok(client)
    }

    /// Server name used in errors, logs, and tool namespacing.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.inner.server
    }

    /// Deadline applied independently to every request.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.inner.timeout
    }

    /// Initialize payload retained for capability and instruction consumers.
    #[must_use]
    pub fn initialization(&self) -> Option<&InitializeResult> {
        self.inner.initialization.get()
    }

    /// Receives server notifications from this point forward.
    #[must_use]
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.inner.notifications.subscribe()
    }

    /// Receives complete tool snapshots after server-triggered refreshes.
    #[must_use]
    pub fn subscribe_tools_changed(&self) -> broadcast::Receiver<ToolsChanged> {
        self.inner.tools_changed.subscribe()
    }

    /// The last complete tool snapshot fetched from the server.
    pub async fn cached_tools(&self) -> Vec<ToolDefinition> {
        self.inner.tools.read().await.clone()
    }

    /// Lists every tool, following `nextCursor` pages and replacing the cache only
    /// after the complete list succeeds.
    ///
    /// # Errors
    ///
    /// A transport, protocol, JSON-RPC, pagination, or timeout failure classified
    /// as [`McpError`].
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>, McpError> {
        let tools = self.fetch_tools().await?;
        *self.inner.tools.write().await = tools.clone();
        Ok(tools)
    }

    /// Calls one server tool. `is_error` remains payload data so todo 46 can turn
    /// tool-level failure into its own `ToolError` without parsing rendered text.
    ///
    /// # Errors
    ///
    /// A transport, protocol, JSON-RPC, result-decode, or timeout failure.
    pub async fn call_tool(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<ToolCallResult, McpError> {
        let value = self
            .request(
                "tools/call",
                json!({
                    "name": tool,
                    "arguments": arguments,
                }),
            )
            .await
            .map_err(|error| self.map_tool_error(tool, error))?;
        serde_json::from_value(value).map_err(|source| McpError::ToolCall {
            server: self.inner.server.clone(),
            tool: tool.to_owned(),
            source: Box::new(source),
        })
    }

    /// Lists every resource, following `nextCursor` pages.
    ///
    /// # Errors
    ///
    /// A transport, protocol, JSON-RPC, pagination, or timeout failure.
    pub async fn list_resources(&self) -> Result<Vec<ResourceDefinition>, McpError> {
        self.fetch_list("resources/list", "resources").await
    }

    /// Lists every resource template, following `nextCursor` pages.
    ///
    /// # Errors
    ///
    /// A transport, protocol, JSON-RPC, pagination, or timeout failure.
    pub async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        self.fetch_list("resources/templates/list", "resourceTemplates")
            .await
    }

    /// Lists every prompt, following `nextCursor` pages.
    ///
    /// # Errors
    ///
    /// A transport, protocol, JSON-RPC, pagination, or timeout failure.
    pub async fn list_prompts(&self) -> Result<Vec<PromptDefinition>, McpError> {
        self.fetch_list("prompts/list", "prompts").await
    }

    /// Reads one resource by its MCP URI.
    ///
    /// # Errors
    ///
    /// A transport, protocol, JSON-RPC, result-decode, or timeout failure.
    pub async fn read_resource(&self, uri: &str) -> Result<ResourceContents, McpError> {
        let value = self
            .request("resources/read", json!({ "uri": uri }))
            .await
            .map_err(|error| self.map_list_error(error))?;
        serde_json::from_value(value)
            .map_err(ExchangeError::DecodeResult)
            .map_err(|error| self.map_list_error(error))
    }

    /// Closes stdin, kills and reaps the child, and stops background tasks.
    /// Calling it more than once is harmless.
    pub async fn close(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }

        if let Some(mut writer) = self.inner.writer.lock().await.take() {
            let _result = writer.shutdown().await;
        }
        fail_pending(&self.inner.pending, ReaderFailure::Closed);

        if let Some(process) = &self.inner.process {
            process.close().await;
        }

        let (reader, refresh) = {
            let mut tasks = lock(&self.inner.tasks);
            (tasks.reader.take(), tasks.refresh.take())
        };
        if let Some(task) = refresh {
            task.abort();
            let _result = task.await;
        }
        if let Some(task) = reader {
            finish_task(task, TASK_SHUTDOWN_GRACE).await;
        }
    }

    fn from_io<R, W>(
        server: String,
        reader: R,
        writer: W,
        timeout: Duration,
        process: Option<ProcessControl>,
    ) -> Self
    where
        R: AsyncRead + Send + 'static,
        W: AsyncWrite + Send + 'static,
    {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (notifications, _) = broadcast::channel(NOTIFICATION_CAPACITY);
        let (tools_changed, _) = broadcast::channel(TOOLS_CHANGED_CAPACITY);
        let (refresh, refresh_receiver) = mpsc::channel(1);
        let inner = Arc::new(Inner {
            server: server.clone(),
            timeout,
            next_id: AtomicU64::new(1),
            writer: tokio::sync::Mutex::new(Some(Box::pin(writer))),
            pending: Arc::clone(&pending),
            notifications: notifications.clone(),
            tools_changed,
            tools: RwLock::new(Vec::new()),
            initialization: OnceLock::new(),
            process,
            tasks: Mutex::new(BackgroundTasks::default()),
            closed: AtomicBool::new(false),
        });

        let reader_task = tokio::spawn(read_loop(
            server,
            Box::pin(reader),
            pending,
            notifications,
            refresh,
        ));
        let refresh_task = tokio::spawn(refresh_loop(Arc::downgrade(&inner), refresh_receiver));
        {
            let mut tasks = lock(&inner.tasks);
            tasks.reader = Some(reader_task);
            tasks.refresh = Some(refresh_task);
        }

        Self { inner }
    }

    async fn initialize(&self) -> Result<InitializeResult, McpError> {
        let value = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "opencode",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await
            .map_err(|error| self.map_handshake_error(error))?;
        serde_json::from_value(value).map_err(|source| McpError::Handshake {
            server: self.inner.server.clone(),
            source: Box::new(source),
        })
    }

    async fn send_initialized(&self) -> Result<(), McpError> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        match tokio::time::timeout(self.inner.timeout, self.write_value(&message)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(self.map_handshake_error(error)),
            Err(_) => Err(McpError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            }),
        }
    }

    async fn fetch_tools(&self) -> Result<Vec<ToolDefinition>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();

        for _ in 0..MAX_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
            let value = self
                .request("tools/list", params)
                .await
                .map_err(|error| self.map_list_error(error))?;
            let page: ListToolsResult = serde_json::from_value(value)
                .map_err(ExchangeError::DecodeResult)
                .map_err(|error| self.map_list_error(error))?;
            tools.extend(page.tools);
            let Some(next) = page.next_cursor else {
                return Ok(tools);
            };
            if !seen.insert(next.clone()) {
                return Err(self.map_list_error(ExchangeError::Invalid(format!(
                    "MCP tools/list returned duplicate cursor {next:?}"
                ))));
            }
            cursor = Some(next);
        }

        Err(self.map_list_error(ExchangeError::Invalid(format!(
            "MCP tools/list exceeded {MAX_LIST_PAGES} pages"
        ))))
    }

    /// `tools/list` deliberately keeps its own loop: it also swaps the tool cache,
    /// and folding it in here would couple that cache to every pure read.
    async fn fetch_list<T>(&self, method: &str, key: &str) -> Result<Vec<T>, McpError>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();

        for _ in 0..MAX_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
            let value = self
                .request(method, params)
                .await
                .map_err(|error| self.map_list_error(error))?;
            let page = value
                .get(key)
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let page: Vec<T> = serde_json::from_value(page)
                .map_err(ExchangeError::DecodeResult)
                .map_err(|error| self.map_list_error(error))?;
            items.extend(page);
            let Some(next) = value
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                return Ok(items);
            };
            if !seen.insert(next.clone()) {
                return Err(self.map_list_error(ExchangeError::Invalid(format!(
                    "MCP {method} returned duplicate cursor {next:?}"
                ))));
            }
            cursor = Some(next);
        }

        Err(self.map_list_error(ExchangeError::Invalid(format!(
            "MCP {method} exceeded {MAX_LIST_PAGES} pages"
        ))))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ExchangeError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(ExchangeError::Closed);
        }

        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let (sender, receiver) = oneshot::channel();
        lock(&self.inner.pending).insert(id, sender);

        let exchange = async {
            self.write_value(&request).await?;
            let message = receiver.await.map_err(|_| ExchangeError::Closed)??;
            decode_response(method, message)
        };

        let result = match tokio::time::timeout(self.inner.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(ExchangeError::Timeout),
        };
        lock(&self.inner.pending).remove(&id);
        result
    }

    async fn write_value(&self, message: &Value) -> Result<(), ExchangeError> {
        let mut bytes = serde_json::to_vec(message).map_err(ExchangeError::Encode)?;
        bytes.push(b'\n');
        let mut writer = self.inner.writer.lock().await;
        let writer = writer.as_mut().ok_or(ExchangeError::Closed)?;
        writer
            .write_all(&bytes)
            .await
            .map_err(ExchangeError::Write)?;
        writer.flush().await.map_err(ExchangeError::Write)
    }

    fn map_handshake_error(&self, error: ExchangeError) -> McpError {
        match error {
            ExchangeError::Timeout => McpError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            },
            ExchangeError::FrameDecode { line } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: decode_error(&line),
            },
            error => McpError::Handshake {
                server: self.inner.server.clone(),
                source: Box::new(error),
            },
        }
    }

    fn map_list_error(&self, error: ExchangeError) -> McpError {
        match error {
            ExchangeError::Timeout => McpError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            },
            ExchangeError::FrameDecode { line } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: decode_error(&line),
            },
            ExchangeError::DecodeResult(source) => McpError::Protocol {
                server: self.inner.server.clone(),
                source,
            },
            error => McpError::Connect {
                server: self.inner.server.clone(),
                source: Box::new(error),
            },
        }
    }

    fn map_tool_error(&self, tool: &str, error: ExchangeError) -> McpError {
        match error {
            ExchangeError::Timeout => McpError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            },
            ExchangeError::FrameDecode { line } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: decode_error(&line),
            },
            error => McpError::ToolCall {
                server: self.inner.server.clone(),
                tool: tool.to_owned(),
                source: Box::new(error),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListToolsResult {
    tools: Vec<ToolDefinition>,
    #[serde(default)]
    next_cursor: Option<String>,
}

struct ProcessControl {
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ProcessControl {
    fn new(server: String, mut child: Child) -> Self {
        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = tokio::select! {
                status = child.wait() => status.map(|status| Some(status.code())),
                _ = &mut shutdown_receiver => child.kill().await.map(|()| None),
            };
            match result {
                Ok(Some(code)) => tracing::debug!(%server, ?code, "MCP child exited"),
                Ok(None) => tracing::debug!(%server, "MCP child was stopped and reaped"),
                Err(error) => tracing::warn!(%server, %error, "could not reap MCP child"),
            }
        });
        Self {
            shutdown: Mutex::new(Some(shutdown)),
            task: Mutex::new(Some(task)),
        }
    }

    fn signal(&self) {
        if let Some(shutdown) = lock(&self.shutdown).take() {
            let _result = shutdown.send(());
        }
    }

    async fn close(&self) {
        self.signal();
        let task = lock(&self.task).take();
        if let Some(task) = task {
            finish_task(task, PROCESS_SHUTDOWN_GRACE).await;
        }
    }
}

impl Drop for ProcessControl {
    fn drop(&mut self) {
        if let Some(shutdown) = lock(&self.shutdown).take() {
            let _result = shutdown.send(());
        }
        // Detach rather than abort: the supervisor owns the child and must reach
        // `kill().await` to reap it. Aborting would make kill-on-drop signal the
        // process but give nobody a chance to collect its exit status.
        let _detached = lock(&self.task).take();
    }
}

async fn read_loop(
    server: String,
    reader: DynReader,
    pending: Pending,
    notifications: broadcast::Sender<Notification>,
    refresh: mpsc::Sender<()>,
) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // Every other arm of this match already reports itself; end-of-stream
                // was the one server death that happened in silence. It is split by
                // whether calls were outstanding because both a crash and an ordinary
                // shutdown arrive here: a stream closing with nothing in flight is what
                // stopping a server looks like, and reporting that at a level demanding
                // attention would fire on every clean exit.
                let in_flight = fail_pending(&pending, ReaderFailure::Closed);
                if in_flight == 0 {
                    tracing::debug!(%server, "MCP server closed its output stream");
                } else {
                    tracing::warn!(
                        %server,
                        in_flight,
                        "MCP server closed its output stream while calls were in flight; \
                         they were failed"
                    );
                }
                break;
            }
            Ok(_) => {
                let frame = line.trim_end_matches(['\r', '\n']);
                if frame.is_empty() {
                    tracing::warn!(%server, "MCP server emitted an empty stdout line");
                    continue;
                }
                match serde_json::from_str::<Value>(frame) {
                    Ok(message) => {
                        route_message(&server, &pending, &notifications, &refresh, message);
                    }
                    Err(error) => {
                        tracing::warn!(
                            %server,
                            line = error.line(),
                            column = error.column(),
                            "MCP server emitted malformed JSON"
                        );
                        fail_pending(
                            &pending,
                            ReaderFailure::Decode {
                                line: Arc::from(frame),
                            },
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%server, %error, "could not read MCP stdout");
                fail_pending(
                    &pending,
                    ReaderFailure::Io {
                        kind: error.kind(),
                        message: Arc::from(error.to_string()),
                    },
                );
                break;
            }
        }
    }
}

async fn refresh_loop(inner: Weak<Inner>, mut refresh: mpsc::Receiver<()>) {
    while refresh.recv().await.is_some() {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if inner.closed.load(Ordering::SeqCst) {
            return;
        }
        let client = StdioClient { inner };
        match client.list_tools().await {
            Ok(tools) => {
                let _receivers = client.inner.tools_changed.send(ToolsChanged { tools });
            }
            Err(error) => {
                tracing::warn!(
                    server = %client.inner.server,
                    %error,
                    "could not refresh MCP tools after list_changed"
                );
            }
        }
    }
}

fn build_command(workspace: &Path, config: &McpLocal) -> io::Result<Command> {
    let (program, arguments) = config.command.split_first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "local MCP command must contain a program",
        )
    })?;
    let cwd = resolve_cwd(workspace, config.cwd.as_deref())?;
    let (guarded_program, guarded_arguments) = zuno_process::guarded_argv(program, arguments);
    let mut command = Command::new(guarded_program);
    command
        .args(guarded_arguments)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // `Command` inherits process env unless `env_clear` is called. Apply the two
    // later layers in oracle order (`mcp/index.ts:352-356`).
    if program == "opencode" {
        command.env("BUN_BE_BUN", "1");
    }
    if let Some(environment) = &config.environment {
        command.envs(environment);
    }
    Ok(command)
}

fn resolve_cwd(workspace: &Path, configured: Option<&str>) -> io::Result<PathBuf> {
    let base = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace)
    };
    let path = configured.map_or(base.clone(), |configured| {
        let configured = Path::new(configured);
        if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            base.join(configured)
        }
    });
    Ok(normalize_lexically(&path))
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    let _removed = normalized.pop();
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn spawn_stderr_reader(server: String, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => return,
                Ok(_) => {
                    tracing::debug!(
                        %server,
                        message = line.trim_end_matches(['\r', '\n']),
                        "MCP server stderr"
                    );
                }
                Err(error) => {
                    tracing::debug!(%server, %error, "could not drain MCP server stderr");
                    return;
                }
            }
        }
    });
}

async fn finish_task(mut task: JoinHandle<()>, grace: Duration) {
    if tokio::time::timeout(grace, &mut task).await.is_err() {
        task.abort();
        let _result = task.await;
    }
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// Namespaces one MCP tool exactly as the TypeScript registry does
/// (`mcp/catalog.ts:117-119`).
///
/// JavaScript's global regex sees UTF-16 code units, so an astral character becomes
/// two underscores rather than one. Iterating `encode_utf16` preserves that edge.
#[must_use]
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{}_{}", sanitize(server), sanitize(tool))
}

fn sanitize(value: &str) -> String {
    value
        .encode_utf16()
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' => {
                char::from(byte)
            }
            Ok(_) | Err(_) => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::num::NonZeroU32;

    use tokio::io::{DuplexStream, ReadHalf, WriteHalf, duplex, split};
    use zuno_config::schema::mcp::LocalKind;

    use super::*;

    const LIVE_TIMEOUT_MS: u32 = 30_000;

    fn memory_client(timeout: Duration) -> (StdioClient, DuplexStream) {
        let (client, server) = duplex(256 * 1024);
        let (reader, writer) = split(client);
        (
            StdioClient::from_io("fake".to_owned(), reader, writer, timeout, None),
            server,
        )
    }

    async fn read_message(reader: &mut BufReader<ReadHalf<DuplexStream>>) -> Value {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .expect("read fake-server request");
        assert!(read > 0, "client closed before fake server read a request");
        assert!(
            line.ends_with('\n'),
            "MCP stdio must terminate each JSON value with one newline: {line:?}"
        );
        serde_json::from_str(&line).expect("client emitted one JSON value")
    }

    async fn send_message(writer: &mut WriteHalf<DuplexStream>, message: &Value) {
        let mut bytes = serde_json::to_vec(message).expect("serialize fake-server response");
        bytes.push(b'\n');
        writer
            .write_all(&bytes)
            .await
            .expect("write fake-server response");
    }

    #[tokio::test]
    async fn stdio_interleaved_notification_does_not_consume_the_pending_response() {
        let (client, server) = memory_client(Duration::from_secs(1));
        let mut notifications = client.subscribe_notifications();
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let request = read_message(&mut server_reader).await;
            let id = request["id"].clone();
            send_message(
                &mut server_writer,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": { "progress": 1 },
                }),
            )
            .await;
            send_message(
                &mut server_writer,
                &json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } }),
            )
            .await;
        });

        let response = client
            .request("probe", json!({}))
            .await
            .expect("the id-matched response must survive an interleaved notification");
        assert_eq!(response, json!({ "ok": true }));
        let notification = tokio::time::timeout(Duration::from_secs(1), notifications.recv())
            .await
            .expect("notification deadline")
            .expect("notification channel remains open");
        assert_eq!(notification.method, "notifications/progress");
        assert_eq!(notification.params, json!({ "progress": 1 }));
        server.await.expect("fake server completed");
    }

    #[tokio::test]
    async fn stdio_unknown_response_id_is_ignored_without_desynchronizing_the_waiter() {
        let (client, server) = memory_client(Duration::from_secs(1));
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let request = read_message(&mut server_reader).await;
            let id = request["id"].clone();
            send_message(
                &mut server_writer,
                &json!({ "jsonrpc": "2.0", "id": 999_999, "result": "stray" }),
            )
            .await;
            send_message(
                &mut server_writer,
                &json!({ "jsonrpc": "2.0", "id": id, "result": "matched" }),
            )
            .await;
        });

        let response = client
            .request("probe", json!({}))
            .await
            .expect("unknown ids are logged, not assigned to another request");
        assert_eq!(response, Value::String("matched".to_owned()));
        server.await.expect("fake server completed");
    }

    #[tokio::test]
    async fn stdio_reader_keeps_a_multi_kilobyte_json_value_on_one_frame() {
        let (client, server) = memory_client(Duration::from_secs(1));
        let payload = "λ".repeat(32 * 1024);
        let expected = payload.clone();
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let request = read_message(&mut server_reader).await;
            send_message(
                &mut server_writer,
                &json!({ "jsonrpc": "2.0", "id": request["id"], "result": payload }),
            )
            .await;
        });

        let response = client
            .request("large", json!({}))
            .await
            .expect("line-oriented reading must not impose a small frame limit");
        assert_eq!(response, Value::String(expected));
        server.await.expect("fake server completed");
    }

    #[tokio::test(start_paused = true)]
    async fn stdio_request_honors_the_per_server_timeout() {
        let deadline = Duration::from_millis(50);
        let (client, server) = memory_client(deadline);
        let (server_reader, _server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let (seen_sender, seen_receiver) = oneshot::channel();
        let server = tokio::spawn(async move {
            let _request = read_message(&mut server_reader).await;
            let _receiver = seen_sender.send(());
            std::future::pending::<()>().await;
        });
        let caller = tokio::spawn({
            let client = client.clone();
            async move { client.list_tools().await }
        });
        seen_receiver.await.expect("fake server saw the request");
        tokio::time::advance(deadline + Duration::from_millis(1)).await;

        let error = caller
            .await
            .expect("request task completed")
            .expect_err("request must time out");
        match error {
            McpError::Timeout { elapsed, .. } => assert_eq!(elapsed, deadline),
            other => panic!("expected the public MCP timeout, got {other:?}"),
        }
        assert!(
            lock(&client.inner.pending).is_empty(),
            "a timed-out request must remove its waiter"
        );
        server.abort();
    }

    #[tokio::test]
    async fn stdio_tools_changed_notification_refreshes_the_cached_list() {
        let (client, server) = memory_client(Duration::from_secs(1));
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let (change_sender, change_receiver) = oneshot::channel();
        let server = tokio::spawn(async move {
            let first = read_message(&mut server_reader).await;
            send_message(
                &mut server_writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": first["id"],
                    "result": { "tools": [{ "name": "before", "inputSchema": {} }] },
                }),
            )
            .await;
            change_receiver.await.expect("test requested list change");
            send_message(
                &mut server_writer,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                }),
            )
            .await;
            let second = read_message(&mut server_reader).await;
            assert_eq!(second["method"], "tools/list");
            send_message(
                &mut server_writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": second["id"],
                    "result": { "tools": [{ "name": "after", "inputSchema": {} }] },
                }),
            )
            .await;
        });

        let initial = client.list_tools().await.expect("initial tool list");
        assert_eq!(initial[0].name, "before");
        let mut changes = client.subscribe_tools_changed();
        change_sender.send(()).expect("signal fake server");
        let changed = tokio::time::timeout(Duration::from_secs(1), changes.recv())
            .await
            .expect("tools-changed refresh deadline")
            .expect("tools-changed channel remains open");
        assert_eq!(changed.tools[0].name, "after");
        assert_eq!(client.cached_tools().await[0].name, "after");
        server.await.expect("fake server completed");
    }

    #[test]
    fn stdio_tool_namespacing_matches_the_javascript_utf16_sanitizer() {
        assert_eq!(tool_name("my server", "read:file"), "my_server_read_file");
        assert_eq!(tool_name("a💡b", "café"), "a__b_caf_");
        assert_eq!(tool_name("a-b", "c_d"), "a-b_c_d");
    }

    #[test]
    fn stdio_command_resolves_cwd_and_applies_configured_environment_last() {
        let workspace = Path::new("/workspace/project");
        let config = McpLocal {
            kind: LocalKind::Local,
            command: vec!["opencode".to_owned(), "mcp".to_owned()],
            cwd: Some("nested/../server".to_owned()),
            environment: Some(BTreeMap::from([
                ("BUN_BE_BUN".to_owned(), "configured".to_owned()),
                ("MCP_TEST_VALUE".to_owned(), "present".to_owned()),
            ])),
            enabled: None,
            timeout: None,
        };
        let command = build_command(workspace, &config).expect("valid command");
        let command = command.as_std();
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("/workspace/project/server"))
        );
        let environments: BTreeMap<_, _> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            environments.get("BUN_BE_BUN"),
            Some(&Some("configured".to_owned()))
        );
        assert_eq!(
            environments.get("MCP_TEST_VALUE"),
            Some(&Some("present".to_owned()))
        );
        assert!(
            command.get_envs().all(|(key, _)| key != OsStr::new("PATH")),
            "PATH must be inherited from the process, not replaced by a snapshot"
        );
    }

    #[test]
    fn stdio_runtime_default_timeout_is_the_executable_oracles_value() {
        assert_eq!(DEFAULT_REQUEST_TIMEOUT, Duration::from_millis(30_000));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_live_codegraph_handshake_lists_and_calls_a_real_tool() {
        let Some(binary) = codegraph_binary() else {
            eprintln!(
                "SKIP stdio live codegraph test: no codegraph binary on PATH or at the known install path"
            );
            return;
        };
        let project = tempfile::tempdir().expect("create isolated codegraph project");
        let initialized = Command::new(&binary)
            .arg("init")
            .arg(project.path())
            .env("CODEGRAPH_NO_DAEMON", "1")
            .env("CODEGRAPH_NO_WATCH", "1")
            .output()
            .await
            .expect("run real codegraph init");
        assert!(
            initialized.status.success(),
            "codegraph init failed with {}\nstdout:\n{}\nstderr:\n{}",
            initialized.status,
            String::from_utf8_lossy(&initialized.stdout),
            String::from_utf8_lossy(&initialized.stderr),
        );

        let config = McpLocal {
            kind: LocalKind::Local,
            command: vec![
                binary.to_string_lossy().into_owned(),
                "serve".to_owned(),
                "--mcp".to_owned(),
                "--path".to_owned(),
                ".".to_owned(),
            ],
            cwd: Some(".".to_owned()),
            environment: Some(BTreeMap::from([
                ("CODEGRAPH_NO_DAEMON".to_owned(), "1".to_owned()),
                ("CODEGRAPH_NO_WATCH".to_owned(), "1".to_owned()),
            ])),
            enabled: None,
            timeout: NonZeroU32::new(LIVE_TIMEOUT_MS),
        };
        let client = StdioClient::connect("codegraph", project.path(), &config)
            .await
            .expect("real codegraph initialize handshake");
        let initialize = client
            .initialization()
            .expect("connect retains initialize result");
        assert_eq!(initialize.protocol_version, PROTOCOL_VERSION);
        assert_eq!(initialize.server_info.name, "codegraph");
        assert!(initialize.instructions.is_some());

        let tools = client
            .list_tools()
            .await
            .expect("real codegraph tools/list");
        assert!(!tools.is_empty(), "real server must advertise tools");
        assert!(
            tools.iter().any(|tool| tool.name == "codegraph_search"),
            "live list must contain the tool selected for the call"
        );
        let mut arguments = Map::new();
        arguments.insert(
            "query".to_owned(),
            Value::String("task45-no-symbol".to_owned()),
        );
        let result = client
            .call_tool("codegraph_search", arguments)
            .await
            .expect("real codegraph tools/call response");
        assert!(
            !result.is_error,
            "codegraph_search must be a non-error call"
        );
        assert!(!result.content.is_empty());

        eprintln!(
            "LIVE MCP initialize: {}",
            json!({ "jsonrpc": "2.0", "id": 1, "result": initialize })
        );
        eprintln!(
            "LIVE MCP tools/list: {}",
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": tools } })
        );
        eprintln!(
            "LIVE MCP tools/call: {}",
            json!({ "jsonrpc": "2.0", "id": 3, "result": result })
        );
        client.close().await;
    }

    fn codegraph_binary() -> Option<PathBuf> {
        const KNOWN: &str = "/config/.local/share/mise/shims/codegraph";
        std::env::var_os("CODEGRAPH_TEST_BINARY")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .or_else(|| Path::new(KNOWN).is_file().then(|| PathBuf::from(KNOWN)))
            .or_else(|| {
                std::env::var_os("PATH").and_then(|path| {
                    std::env::split_paths(&path)
                        .map(|directory| directory.join("codegraph"))
                        .find(|candidate| candidate.is_file())
                })
            })
    }
}
