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
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use zuno_config::schema::mcp::McpLocal;
use zuno_error::McpError;

use crate::catalog::{PromptDefinition, ResourceContents, ResourceDefinition, ResourceTemplate};
use crate::protocol::{
    ExchangeError, Pending, ReaderFailure, ReaderState, decode_response, fail_pending, lock,
    may_have_side_effects, no_json_rpc_frames_error, not_json_rpc_error, oversized_frame_error,
    reader_failure_label, route_message,
};

/// Protocol version proven against the real server used by the live test.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Runtime fallback used by the TypeScript client (`mcp/index.ts:38,359`).
///
/// The config schema's prose still says five seconds (`config/mcp.ts:20-22`), but
/// the executable path has used this 30-second constant since before connection.
/// Runtime behavior wins over stale schema documentation.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest single JSON-RPC message this client will accumulate before it stops reading.
///
/// Named for stdio because that is where the bound is enforced per frame, but the
/// streamable-HTTP body reader shares it: both buffer one whole JSON-RPC message, so
/// they have the same thing to be wrong about and no reason to disagree by transport.
///
/// One MCP message is one newline-terminated JSON value, so a peer that never emits a
/// newline is asking this client to grow one allocation until the process dies. The
/// bound therefore has to sit far above any response a server can plausibly mean to
/// send. The largest is a `resources/read`: a blob up to
/// [`crate::MAX_RESOURCE_BLOB_BYTES`] (10 MiB) arrives base64-encoded, which inflates
/// it to roughly 13.4 MiB before the JSON envelope, and a `tools/list` page is three
/// orders of magnitude smaller than that. 64 MiB leaves more than four times the
/// largest attachment-eligible payload, so nothing legitimate is rejected; past it,
/// "one JSON value" has stopped being a credible reading of the byte stream.
///
/// The precedent for bounding a record at all is
/// `zuno-search/src/ripgrep.rs`'s `MAX_RECORD_BYTES`; only the number differs,
/// because a ripgrep record is one match line and an MCP frame can carry a blob.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Largest stderr line kept for one `tracing` record.
///
/// Deliberately far smaller than [`MAX_FRAME_BYTES`], and enforced differently: see
/// [`spawn_stderr_reader`]. Server stderr has no pending caller to fail and no
/// framing contract to violate, so an over-long line is truncated rather than fatal.
/// 8 KiB is generous for a diagnostic line and bounds the allocation.
const MAX_STDERR_LINE_BYTES: usize = 8 * 1024;

const MAX_LIST_PAGES: usize = 1_000;
const NOTIFICATION_CAPACITY: usize = 64;
const TOOLS_CHANGED_CAPACITY: usize = 16;
const TASK_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
const PROCESS_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const PROCESS_TERM_GRACE: Duration = Duration::from_millis(250);

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
/// cache. The configured command is the direct process-group leader; no Zuno helper
/// process is inserted in front of it. Dropping the final clone kills the complete
/// group and leaves its task to reap the direct child. Call [`Self::close`] when
/// shutdown ordering matters.
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
    /// What [`read_loop`] has learned about the child's stdout, including why it
    /// stopped. A request consults it instead of writing to a stream nothing is
    /// listening to: see [`StdioClient::request`].
    reader: Arc<ReaderState>,
}

#[derive(Default)]
struct BackgroundTasks {
    reader: Option<JoinHandle<()>>,
    refresh: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
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
        if let Some(task) = tasks.stderr.take() {
            task.abort();
        }
    }
}

impl StdioClient {
    /// Spawns a configured local server, initializes it, and sends
    /// `notifications/initialized`.
    ///
    /// `cwd` is resolved lexically from `workspace`. The child inherits the
    /// process environment, then configured `environment` entries are applied.
    ///
    /// # Errors
    ///
    /// [`McpError::Connect`] for invalid process configuration or spawn failure,
    /// [`McpError::Handshake`] for an initialize failure, and [`McpError::Protocol`]
    /// when the child's stdout is not JSON-RPC at all — a run of undecodable frames
    /// past [`crate::MAX_CONSECUTIVE_UNDECODABLE_FRAMES`], a stream that ends or
    /// times out having never framed one decodable message, or a frame past
    /// [`MAX_FRAME_BYTES`]. A single undecodable line is logged and skipped, because
    /// a frame with no JSON-RPC id cannot be charged to any call; it therefore no
    /// longer fails the handshake by itself, and a server that emits nothing but such
    /// lines is reported once its deadline or its stream ends. A blank line counts as
    /// one of those frames — a stream of nothing but `\n` is reported as
    /// [`McpError::Protocol`] naming the count, not as a bare deadline.
    /// [`McpError::Timeout`] when the configured per-server deadline expires with the
    /// stream otherwise healthy.
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
        let process_group = match child.id().map(zuno_process::DirectProcessGroup::register) {
            Some(Ok(process_group)) => process_group,
            Some(Err(source)) => {
                let _kill = child.start_kill();
                let _status = child.wait().await;
                return Err(McpError::Connect {
                    server,
                    source: Box::new(source),
                });
            }
            None => {
                let _kill = child.start_kill();
                let _status = child.wait().await;
                return Err(McpError::Connect {
                    server,
                    source: Box::new(io::Error::other("spawned MCP child exposed no process id")),
                });
            }
        };
        let stdin = child.stdin.take().ok_or_else(|| McpError::Connect {
            server: server.clone(),
            source: Box::new(io::Error::other("spawned MCP child has no stdin")),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Connect {
            server: server.clone(),
            source: Box::new(io::Error::other("spawned MCP child has no stdout")),
        })?;
        let stderr = child
            .stderr
            .take()
            .map(|stderr| spawn_stderr_reader(server.clone(), stderr));
        let process = ProcessControl::new(server.clone(), child, process_group);
        let client = Self::from_io(
            server,
            stdout,
            stdin,
            timeout,
            Some(process),
            MAX_FRAME_BYTES,
        );
        if let Some(stderr) = stderr {
            lock(&client.inner.tasks).stderr = Some(stderr);
        }

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

        let (reader, refresh, stderr) = {
            let mut tasks = lock(&self.inner.tasks);
            (
                tasks.reader.take(),
                tasks.refresh.take(),
                tasks.stderr.take(),
            )
        };
        if let Some(task) = refresh {
            task.abort();
            let _result = task.await;
        }
        if let Some(task) = reader {
            finish_task(task, TASK_SHUTDOWN_GRACE).await;
        }
        if let Some(task) = stderr {
            finish_task(task, TASK_SHUTDOWN_GRACE).await;
        }
    }

    /// `max_frame_bytes` is a parameter rather than a direct read of
    /// [`MAX_FRAME_BYTES`] so a test can prove the bound is enforced without moving
    /// 64 MiB through a pipe. Every production path passes the constant.
    fn from_io<R, W>(
        server: String,
        reader: R,
        writer: W,
        timeout: Duration,
        process: Option<ProcessControl>,
        max_frame_bytes: usize,
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
            reader: Arc::new(ReaderState::default()),
        });

        let reader_task = tokio::spawn(read_loop(
            server,
            Box::pin(reader),
            pending,
            notifications,
            refresh,
            max_frame_bytes,
            Arc::clone(&inner.reader),
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
                        "name": crate::CLIENT_NAME,
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
        // The reader is the only thing that can deliver a response, so once it has
        // stopped this request can never be answered, however writable the child's
        // stdin still is. Refusing *before* the write is what keeps the refusal
        // honest: nothing reached the server, so a definite failure is not a claim
        // about a side effect. Without this check the write succeeds, the call waits
        // out the whole deadline, and the failure comes back as a retryable timeout
        // against a permanently deaf connection — for as many attempts as the
        // harness is willing to make.
        if let Some(failure) = self.inner.reader.exit() {
            return Err(ExchangeError::from(failure));
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
            let message = receiver
                .await
                .map_err(|_| ExchangeError::Closed)?
                .map_err(|failure| self.reader_failure_error(method, failure))?;
            decode_response(method, message)
        };

        let result = match tokio::time::timeout(self.inner.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(self.deadline_error()),
        };
        lock(&self.inner.pending).remove(&id);
        result
    }

    /// Reports a reader that stopped under this call as the class the peer justified.
    ///
    /// This request was already written, so for a method that may have run a side
    /// effect the honest report is that the outcome is *unknown*, not that the call
    /// definitely failed — a `tools/call` the server executed before its stdout
    /// stopped being readable has still executed.
    /// [`ExchangeError::Uncertain`] carries that, and reaches the model as the
    /// instruction to inspect authoritative state rather than replay the call, because
    /// [`zuno_tool::Tool::replay_policy`] for an MCP proxy is
    /// [`zuno_tool::ToolReplayPolicy::Never`].
    ///
    /// Read-only methods keep the definite failure, which is what names the fault:
    /// nothing was mutated, so there is nothing to be uncertain about.
    fn reader_failure_error(&self, method: &str, failure: ReaderFailure) -> ExchangeError {
        let failure_label = reader_failure_label(&failure);
        let error = ExchangeError::from(failure);
        if !may_have_side_effects(method) {
            return error;
        }
        let reason = error.to_string();
        tracing::warn!(
            server = %self.inner.server,
            method,
            // The class is safe to render; the sentence is not, because
            // `ExchangeError::NotJsonRpc` embeds a bounded excerpt of the peer's own
            // stdout and a field name the redaction policy does not know reaches the
            // plaintext log and the `logs.sqlite` record verbatim. `stream_output` ends
            // in a payload word, so policy scrubs it.
            failure = failure_label,
            stream_output = %reason,
            "MCP stdout reader stopped while a call that may have taken effect was \
             outstanding; its outcome is unknown"
        );
        ExchangeError::Uncertain {
            reason: Arc::from(reason),
        }
    }

    /// The deadline this call spent, named by what the stream had produced by then.
    ///
    /// A peer that answered nothing is what a deadline is for, and stays a retryable
    /// timeout. A peer that has written output on this connection but never once
    /// framed a JSON-RPC message is a different fault: the configured command is not
    /// an MCP server, which no number of retries fixes, so the deadline reports the
    /// framing violation and the frames that prove it. That predicate needs a frame
    /// that failed to parse *and* no frame that ever parsed, so a working server
    /// cannot reach it by being slow — and before the undecodable-frame bound existed
    /// this case failed permanently on the very first stray line, so naming it here is
    /// strictly narrower than the behavior that shipped.
    fn deadline_error(&self) -> ExchangeError {
        self.inner
            .reader
            .not_json_rpc()
            .map_or(ExchangeError::Timeout, ExchangeError::from)
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
            // A deadline and a lost response around a possible side effect are the
            // same class here: the outcome is unknown, and `elapsed` is the deadline
            // this client configured rather than a measurement, exactly as the remote
            // transport reports it.
            ExchangeError::Timeout | ExchangeError::Uncertain { .. } => McpError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            },
            ExchangeError::NotJsonRpc { count, excerpt } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: not_json_rpc_error(count, &excerpt),
            },
            ExchangeError::NoJsonRpcFrames { count } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: no_json_rpc_frames_error(count),
            },
            ExchangeError::FrameTooLarge { bytes, limit } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: oversized_frame_error(bytes, limit),
            },
            error => McpError::Handshake {
                server: self.inner.server.clone(),
                source: Box::new(error),
            },
        }
    }

    fn map_list_error(&self, error: ExchangeError) -> McpError {
        match error {
            // A deadline and a lost response around a possible side effect are the
            // same class here: the outcome is unknown, and `elapsed` is the deadline
            // this client configured rather than a measurement, exactly as the remote
            // transport reports it.
            ExchangeError::Timeout | ExchangeError::Uncertain { .. } => McpError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            },
            // A connection this client has already seen end. `Closed` is the reader
            // reaching end-of-stream, the client being closed, or a waiter whose
            // answer can no longer arrive; `Read` is the reader stopping on its own
            // I/O error. Every one of those is recorded before it is reported (see
            // [`Self::request`]), nothing reconnects a stdio client, and the identical
            // exchange can therefore never succeed on this client — so this is not the
            // retryable `Connect` the catch-all below reports for a server still coming
            // up. `call_tool` already reports the same refusal as the permanent
            // `ToolCall`; listing reported it as retryable, and a caller honoring that
            // could only repeat the refusal. `Handshake` is this crate's permanent class
            // for a boxed cause — "the transport came up and the exchange is unusable"
            // — for the same reason the remote transport uses it
            // (`catalog::remote_error`): `Protocol` contracts for a real decode
            // position, and this failure has none.
            error @ (ExchangeError::Closed | ExchangeError::Read(_)) => McpError::Handshake {
                server: self.inner.server.clone(),
                source: Box::new(error),
            },
            ExchangeError::NotJsonRpc { count, excerpt } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: not_json_rpc_error(count, &excerpt),
            },
            ExchangeError::NoJsonRpcFrames { count } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: no_json_rpc_frames_error(count),
            },
            ExchangeError::FrameTooLarge { bytes, limit } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: oversized_frame_error(bytes, limit),
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
            // A deadline and a lost response around a possible side effect are the
            // same class here: the outcome is unknown, and `elapsed` is the deadline
            // this client configured rather than a measurement, exactly as the remote
            // transport reports it.
            ExchangeError::Timeout | ExchangeError::Uncertain { .. } => McpError::Timeout {
                server: self.inner.server.clone(),
                elapsed: self.inner.timeout,
            },
            ExchangeError::NotJsonRpc { count, excerpt } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: not_json_rpc_error(count, &excerpt),
            },
            ExchangeError::NoJsonRpcFrames { count } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: no_json_rpc_frames_error(count),
            },
            ExchangeError::FrameTooLarge { bytes, limit } => McpError::Protocol {
                server: self.inner.server.clone(),
                source: oversized_frame_error(bytes, limit),
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
    process_group: zuno_process::DirectProcessGroup,
}

impl ProcessControl {
    fn new(
        server: String,
        mut child: Child,
        process_group: zuno_process::DirectProcessGroup,
    ) -> Self {
        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        let task_group = process_group;
        let task = tokio::spawn(async move {
            let result = tokio::select! {
                status = child.wait() => match status {
                    Ok(status) => task_group
                        .force_kill()
                        .map(|()| Some(status.code())),
                    Err(error) => Err(error),
                },
                _ = &mut shutdown_receiver => {
                    stop_direct_child(&mut child, &task_group).await.map(|()| None)
                }
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
            process_group,
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
            finish_process_task(task, PROCESS_SHUTDOWN_GRACE).await;
        }
    }
}

async fn stop_direct_child(
    child: &mut Child,
    process_group: &zuno_process::DirectProcessGroup,
) -> io::Result<()> {
    process_group.request_termination()?;
    match tokio::time::timeout(PROCESS_TERM_GRACE, child.wait()).await {
        Ok(status) => {
            let _status = status?;
        }
        Err(_) => {
            process_group.force_kill()?;
            let _status = child.wait().await?;
        }
    }
    process_group.force_kill()
}

impl Drop for ProcessControl {
    fn drop(&mut self) {
        // The Tokio runtime may be tearing down at the same time as the final client.
        // Signal the complete group synchronously before relying on the detached task to
        // reap the direct child.
        let _killed = self.process_group.force_kill();
        if let Some(shutdown) = lock(&self.shutdown).take() {
            let _result = shutdown.send(());
        }
        // Detach rather than abort: the task owns the direct child and its process-group
        // registration until the complete tree has been terminated and reaped.
        let _detached = lock(&self.task).take();
    }
}

/// What one bounded frame read produced.
enum Frame {
    /// `buffer` holds one frame, terminator included when the peer sent one.
    Line,
    /// The stream ended with nothing buffered.
    Eof,
    /// The peer reached `bytes` without a terminator and passed the bound.
    ///
    /// `buffer` holds the prefix accepted before the bound, and the reader is left
    /// at the byte that broke it.
    TooLarge { bytes: usize },
}

/// One step of [`read_frame`], separated so the borrow of the reader's buffer ends
/// before the matching `consume`.
enum FrameStep {
    Consumed { found: bool, take: usize },
    TooLarge { bytes: usize },
}

/// Reads one newline-terminated frame into `buffer`, refusing to grow past `limit`.
///
/// `tokio::io::AsyncBufReadExt` has no bounded `read_line`: it appends to a `String`
/// until a newline arrives or the process dies, which is exactly the defect this
/// replaces. `fill_buf`/`consume` gives the same framing with the length checked
/// before each chunk is copied, so a peer that never terminates a frame is stopped at
/// `limit` bytes rather than at the memory limit.
///
/// The terminator is left in `buffer`, and a trailing frame that ends at
/// end-of-stream is reported once as [`Frame::Line`] followed by [`Frame::Eof`] on the
/// next call — the same sequence `read_line` produced, so callers keep their behavior.
/// UTF-8 is deliberately *not* validated here: framing is byte-oriented, and the one
/// caller that needs a `str` validates once.
async fn read_frame<R>(reader: &mut R, buffer: &mut Vec<u8>, limit: usize) -> io::Result<Frame>
where
    R: AsyncBufRead + Unpin,
{
    buffer.clear();
    loop {
        let step = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(if buffer.is_empty() {
                    Frame::Eof
                } else {
                    Frame::Line
                });
            }
            let (found, take) = match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => (true, index + 1),
                None => (false, available.len()),
            };
            let bytes = buffer.len() + take;
            if bytes > limit {
                FrameStep::TooLarge { bytes }
            } else {
                buffer.extend_from_slice(&available[..take]);
                FrameStep::Consumed { found, take }
            }
        };
        match step {
            FrameStep::TooLarge { bytes } => return Ok(Frame::TooLarge { bytes }),
            FrameStep::Consumed { found, take } => {
                reader.consume(take);
                if found {
                    return Ok(Frame::Line);
                }
            }
        }
    }
}

/// Discards bytes through the next newline, so a drain can resume at the next line.
///
/// Only sound where the byte stream carries no framing contract, which means stderr
/// and not stdout: see [`spawn_stderr_reader`].
async fn skip_to_newline<R>(reader: &mut R) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let (found, take) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(());
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => (true, index + 1),
                None => (false, available.len()),
            }
        };
        reader.consume(take);
        if found {
            return Ok(());
        }
    }
}

/// Reads framed JSON-RPC messages until the stream stops being one.
///
/// Every exit records itself in `state` before it fails the calls that were in flight,
/// so a request issued afterwards learns immediately that nothing can answer it. That
/// matters most for the exits this loop takes while the child is still alive and its
/// stdin still writable: without the record, every later call writes successfully and
/// waits out its whole deadline against a reader that is gone.
async fn read_loop(
    server: String,
    reader: DynReader,
    pending: Pending,
    notifications: broadcast::Sender<Notification>,
    refresh: mpsc::Sender<()>,
    max_frame_bytes: usize,
    state: Arc<ReaderState>,
) {
    let mut reader = BufReader::new(reader);
    let mut frame = Vec::new();
    loop {
        match read_frame(&mut reader, &mut frame, max_frame_bytes).await {
            Ok(Frame::Eof) => {
                // Every other arm of this match already reports itself; end-of-stream
                // was the one server death that happened in silence. It is split by
                // whether calls were outstanding because both a crash and an ordinary
                // shutdown arrive here: a stream closing with nothing in flight is what
                // stopping a server looks like, and reporting that at a level demanding
                // attention would fire on every clean exit.
                // A stream that ends having produced output but never one decodable
                // frame is the misconfiguration case ending fast rather than at a
                // deadline: a command that printed its usage and exited, an HTTP-only
                // server that logged a startup banner and died. Reporting that as a
                // bare close would name the wrong cause and drop the only evidence.
                let failure = state.not_json_rpc().unwrap_or(ReaderFailure::Closed);
                state.note_exit(failure.clone());
                let in_flight = fail_pending(&pending, failure.clone());
                if let ReaderFailure::Undecodable { excerpt, count } = &failure {
                    tracing::warn!(
                        %server,
                        in_flight,
                        undecodable = count,
                        // `stdout`, not `excerpt`: these are the peer's own bytes, and
                        // `zuno_observability`'s redaction policy classifies field
                        // *names*. A name it does not know is written verbatim to the
                        // plaintext log and to the `logs.sqlite` record; a name ending in
                        // a payload word is scrubbed. The excerpt still reaches the
                        // operator through the returned `McpError::Protocol`, which is
                        // answering the person who ran the command rather than filling a
                        // durable log.
                        stdout = %excerpt,
                        "MCP server closed its output stream having never sent a JSON-RPC frame"
                    );
                } else if in_flight == 0 {
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
            Ok(Frame::TooLarge { bytes }) => {
                // A peer past the frame bound has left the byte stream at an unknown
                // offset: the bytes read are a prefix of a value whose end was never
                // announced, so the next newline cannot be trusted to begin a message
                // rather than sit inside one. Resynchronising would route a fragment as
                // a frame, so this ends the connection the way end-of-stream does —
                // every outstanding call is failed with a typed reason, and the reader
                // stops instead of guessing.
                let failure = ReaderFailure::FrameTooLarge {
                    bytes,
                    limit: max_frame_bytes,
                };
                state.note_exit(failure.clone());
                let in_flight = fail_pending(&pending, failure);
                tracing::warn!(
                    %server,
                    bytes,
                    limit = max_frame_bytes,
                    in_flight,
                    "MCP server exceeded the stdio frame bound without a newline; \
                     the connection was ended because the stream cannot be resynchronized"
                );
                break;
            }
            Ok(Frame::Line) => {
                // `read_line` rejected invalid UTF-8 with an `InvalidData` read error
                // and stopped. Framing is byte-oriented now, so that check lives here,
                // at the one point that needs a `str`, and keeps the same outcome.
                let text = match std::str::from_utf8(&frame) {
                    Ok(text) => text,
                    Err(error) => {
                        tracing::warn!(%server, %error, "MCP server emitted non-UTF-8 stdout");
                        let failure = ReaderFailure::Io {
                            kind: io::ErrorKind::InvalidData,
                            message: Arc::from(error.to_string()),
                        };
                        state.note_exit(failure.clone());
                        fail_pending(&pending, failure);
                        break;
                    }
                };
                let text = text.trim_end_matches(['\r', '\n']);
                // A blank line goes through the same counter as a malformed one. It used
                // to `continue` above this machinery, which is what let a peer writing
                // nothing but `\n` produce one unthrottled `warn` per line while the run
                // stayed at zero, so the call reported a bare retryable deadline that
                // named nothing. See [`ReaderState::note_undecodable`].
                let outcome = if text.is_empty() {
                    Err(None)
                } else {
                    serde_json::from_str::<Value>(text).map_err(Some)
                };
                match outcome {
                    Ok(message) => {
                        state.note_decoded();
                        route_message(&server, &pending, &notifications, &refresh, message);
                    }
                    Err(error) => {
                        // Scoped to the frame, not the connection: see
                        // [`crate::MAX_CONSECUTIVE_UNDECODABLE_FRAMES`] for why a line
                        // with no id must not be charged to calls it cannot belong to,
                        // and why a run of them on a stream that never framed JSON-RPC
                        // still ends it.
                        let run = state.note_undecodable(text);
                        let what = if error.is_some() {
                            "MCP server emitted malformed JSON"
                        } else {
                            "MCP server emitted an empty stdout line"
                        };
                        if run.loud {
                            tracing::warn!(
                                %server,
                                line = error.as_ref().map_or(0, serde_json::Error::line),
                                column = error.as_ref().map_or(0, serde_json::Error::column),
                                undecodable = run.count,
                                // Zuno's own words for which shape this line was, in a
                                // field of its own so the event's text stays one literal
                                // and the peer's bytes stay out of both.
                                what,
                                "MCP server emitted a stdout line that is not a JSON-RPC message"
                            );
                        } else {
                            tracing::debug!(
                                %server,
                                undecodable = run.count,
                                what,
                                "MCP server emitted a stdout line that is not a JSON-RPC message"
                            );
                        }
                        if let Some(failure) = run.violation {
                            state.note_exit(failure.clone());
                            let in_flight = fail_pending(&pending, failure);
                            tracing::warn!(
                                %server,
                                undecodable = run.count,
                                in_flight,
                                "MCP server emitted no decodable frame within the undecodable-frame \
                                 bound; the connection was ended"
                            );
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%server, %error, "could not read MCP stdout");
                let failure = ReaderFailure::Io {
                    kind: error.kind(),
                    message: Arc::from(error.to_string()),
                };
                state.note_exit(failure.clone());
                fail_pending(&pending, failure);
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
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    // `Command` inherits process env unless `env_clear` is called.
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

/// Drains a child's stderr into `tracing`, bounded per line.
///
/// # Why this truncates where the stdout reader disconnects
///
/// Both readers used an unbounded `read_line`, but the two streams answer to
/// different contracts. A stdout frame is protocol: exceeding
/// [`MAX_FRAME_BYTES`] means the client can no longer say where one message ends, so
/// [`read_loop`] fails every pending call and stops. Stderr is diagnostics: it has no
/// pending caller to fail, no framing to resynchronize, and ending the drain would be
/// actively harmful, because a full stderr pipe blocks the child. So an over-long
/// line is logged truncated, marked `truncated = true`, and its remainder discarded
/// through the next newline, after which draining continues.
///
/// # Why the line is recorded as `stderr` and never as `message`
///
/// The value is a verbatim line from a process this client only knows from user
/// configuration: a stack trace, a startup banner, an echoed environment variable.
/// `zuno_observability`'s redaction policy classifies field *names*, and `message` is
/// the one name it deliberately lets through — it is the field `tracing` uses for an
/// event's own text, so redacting it would blank every log line in every sink.
/// `DefaultVisitor` also prints that field with no `name=` prefix. Recording this line
/// as `message` therefore wrote peer bytes into the plaintext log, into `--print-logs`
/// stderr, and into the `message` column of `logs.sqlite`, rendered as if they were
/// Zuno's own sentence — measured as
/// `DEBUG …: MCP server stderr server=probe-mcp Traceback: API_KEY=sk-…`.
///
/// `stderr` is a name that policy classifies as a payload, so the value is replaced
/// with the redaction placeholder in every sink while `server`, `bytes`, `limit`, and
/// `truncated` stay readable. Do not rename it back, and do not give a subprocess
/// stream any other name policy does not know.
fn spawn_stderr_reader<R>(server: String, stderr: R) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            match read_frame(&mut reader, &mut line, MAX_STDERR_LINE_BYTES).await {
                Ok(Frame::Eof) => return,
                Ok(Frame::Line) => {
                    let text = String::from_utf8_lossy(&line);
                    tracing::debug!(
                        %server,
                        stderr = text.trim_end_matches(['\r', '\n']),
                        "MCP server stderr"
                    );
                }
                Ok(Frame::TooLarge { bytes }) => {
                    let text = String::from_utf8_lossy(&line);
                    tracing::debug!(
                        %server,
                        bytes,
                        limit = MAX_STDERR_LINE_BYTES,
                        truncated = true,
                        stderr = %text,
                        "MCP server stderr line exceeded its bound and was truncated"
                    );
                    if skip_to_newline(&mut reader).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    tracing::debug!(%server, %error, "could not drain MCP server stderr");
                    return;
                }
            }
        }
    })
}

async fn finish_task(mut task: JoinHandle<()>, grace: Duration) {
    if tokio::time::timeout(grace, &mut task).await.is_err() {
        task.abort();
        let _result = task.await;
    }
}

async fn finish_process_task(mut task: JoinHandle<()>, grace: Duration) {
    if tokio::time::timeout(grace, &mut task).await.is_err() {
        tracing::warn!(
            ?grace,
            "MCP process-group cleanup exceeded its grace; reaper remains detached"
        );
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

    use tokio::io::{AsyncReadExt as _, DuplexStream, ReadHalf, WriteHalf, duplex, split};
    use zuno_config::schema::mcp::LocalKind;

    use crate::MAX_CONSECUTIVE_UNDECODABLE_FRAMES;

    use super::*;

    const LIVE_TIMEOUT_MS: u32 = 30_000;

    fn memory_client(timeout: Duration) -> (StdioClient, DuplexStream) {
        bounded_memory_client(timeout, MAX_FRAME_BYTES)
    }

    /// A memory client whose reader enforces `max_frame_bytes`.
    ///
    /// Exists so the frame bound can be proven at a few kilobytes instead of moving
    /// [`MAX_FRAME_BYTES`] through a duplex pipe; the production bound itself is
    /// pinned by `stdio_frame_bound_cannot_reject_a_maximum_resource_blob`.
    fn bounded_memory_client(
        timeout: Duration,
        max_frame_bytes: usize,
    ) -> (StdioClient, DuplexStream) {
        let (client, server) = duplex(256 * 1024);
        let (reader, writer) = split(client);
        (
            StdioClient::from_io(
                "fake".to_owned(),
                reader,
                writer,
                timeout,
                None,
                max_frame_bytes,
            ),
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
    async fn stderr_drain_returns_an_owned_task_that_can_be_joined() {
        let (mut writer, reader) = duplex(256);
        let task = spawn_stderr_reader("fake".to_owned(), reader);
        writer
            .write_all(b"diagnostic\n")
            .await
            .expect("write stderr fixture");
        drop(writer);

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("stderr drain reaches EOF")
            .expect("stderr task does not panic");
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

    /// A stray non-JSON line carries no id, so it can have answered no call. The
    /// reader used to fail every in-flight request on it, which reported unrelated
    /// tool calls as permanently failed and then dropped their real responses.
    #[tokio::test]
    async fn stdio_stray_non_json_line_does_not_fail_concurrent_requests() {
        let (client, server) = memory_client(Duration::from_secs(5));
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let first = read_message(&mut server_reader).await;
            let second = read_message(&mut server_reader).await;
            // What a wrapper script's banner or a stray `print` in a handler looks
            // like on stdout while calls are outstanding.
            server_writer
                .write_all(b"Debugger attached.\n")
                .await
                .expect("write the stray non-JSON line");
            // Answered out of order so the assertions cannot pass by arrival order.
            for request in [second, first] {
                send_message(
                    &mut server_writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": request["method"],
                    }),
                )
                .await;
            }
        });

        let alpha = tokio::spawn({
            let client = client.clone();
            async move { client.request("alpha", json!({})).await }
        });
        let beta = tokio::spawn({
            let client = client.clone();
            async move { client.request("beta", json!({})).await }
        });
        let alpha = alpha
            .await
            .expect("alpha task completed")
            .expect("a line that belongs to no id must not fail an unrelated call");
        let beta = beta
            .await
            .expect("beta task completed")
            .expect("a line that belongs to no id must not fail an unrelated call");
        assert_eq!(alpha, Value::String("alpha".to_owned()));
        assert_eq!(beta, Value::String("beta".to_owned()));
        server.await.expect("fake server completed");
    }

    /// A peer that has framed one JSON-RPC message has proven it speaks the protocol,
    /// and no quantity of later noise may take that connection down.
    ///
    /// The reviewer's input: a working server writes junk to stdout while a
    /// `tools/call` is outstanding. The undecodable-frame bound used to end the
    /// reader here — silently, while the child was alive and its stdin writable — so
    /// the call in flight was reported as a definite permanent failure and every
    /// later call waited out its whole deadline. Twice the bound is written so a
    /// counter that only resets on a decodable frame cannot pass this by accident.
    #[tokio::test]
    async fn stdio_junk_after_a_decodable_frame_never_ends_the_connection() {
        let (client, server) = memory_client(Duration::from_secs(5));
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let first = read_message(&mut server_reader).await;
            send_message(
                &mut server_writer,
                &json!({ "jsonrpc": "2.0", "id": first["id"], "result": { "tools": [] } }),
            )
            .await;
            let call = read_message(&mut server_reader).await;
            for _ in 0..MAX_CONSECUTIVE_UNDECODABLE_FRAMES * 2 {
                server_writer
                    .write_all(b"[debug] still working\n")
                    .await
                    .expect("write a stray non-JSON line");
            }
            send_message(
                &mut server_writer,
                &json!({
                    "jsonrpc": "2.0",
                    "id": call["id"],
                    "result": { "content": [], "isError": false },
                }),
            )
            .await;
        });

        client
            .list_tools()
            .await
            .expect("the server answers a list");
        let result = client
            .call_tool("write_file", Map::new())
            .await
            .expect("noise on a stream that already framed JSON-RPC must not fail a call");
        assert!(!result.is_error);
        server.await.expect("fake server completed");
    }

    /// A reader that has stopped cannot answer anything, however writable stdin is.
    ///
    /// Without the recorded exit the next call writes successfully, waits out the
    /// whole per-server deadline, and comes back as a retryable timeout against a
    /// permanently deaf connection — so the harness retries it, one deadline at a
    /// time, forever.
    #[tokio::test]
    async fn stdio_a_stopped_reader_fails_the_next_call_at_once_not_at_its_deadline() {
        let deadline = Duration::from_secs(5);
        let (client, server) = memory_client(deadline);
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let _request = read_message(&mut server_reader).await;
            for _ in 0..MAX_CONSECUTIVE_UNDECODABLE_FRAMES {
                server_writer
                    .write_all(b"usage: some-cli [options]\n")
                    .await
                    .expect("write an undecodable line");
            }
            // Still alive and still reading: exactly the state in which a request
            // writes fine and is never answered.
            let _second = read_message(&mut server_reader).await;
            std::future::pending::<()>().await;
        });

        let first = client
            .list_tools()
            .await
            .expect_err("a stream with no decodable frame must fail the pending call");
        assert!(!first.is_retryable(), "{first:?}");

        let started = std::time::Instant::now();
        let second = client
            .list_tools()
            .await
            .expect_err("a call after the reader stopped cannot be answered");
        let waited = started.elapsed();
        assert!(
            waited < deadline / 5,
            "a call against a stopped reader must fail at once, not at its deadline: {waited:?}"
        );
        assert!(
            matches!(&second, McpError::Protocol { server, .. } if server == "fake"),
            "the refusal must name the framing violation that stopped the reader: {second:?}"
        );
        assert!(
            !second.is_retryable(),
            "nothing was written, and no retry can restart a reader: {second:?}"
        );
        server.abort();
    }

    /// The most common stdio misconfiguration: an HTTP-only server pointed at as a
    /// command. It prints one banner line and then never speaks JSON-RPC.
    ///
    /// One line is under the undecodable-frame bound, so the call reaches its
    /// deadline — but a deadline reported as a bare timeout is retryable and names
    /// nothing, and the configured command will not become an MCP server on the next
    /// attempt. The evidence has to survive to the failure.
    #[tokio::test]
    async fn stdio_one_stray_line_then_silence_names_the_line_not_a_bare_deadline() {
        const BANNER: &[u8] =
            b"INFO:     Uvicorn running on http://0.0.0.0:8000 (Press CTRL+C to quit)\n";
        let (client, server) = memory_client(Duration::from_millis(300));
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let _request = read_message(&mut server_reader).await;
            server_writer
                .write_all(BANNER)
                .await
                .expect("write the startup banner");
            std::future::pending::<()>().await;
        });

        let error = client
            .list_tools()
            .await
            .expect_err("a peer that never frames JSON-RPC must not report success");
        assert!(
            matches!(&error, McpError::Protocol { server, .. } if server == "fake"),
            "a command that is not an MCP server is a permanent configuration fault: {error:?}"
        );
        assert!(!error.is_retryable(), "{error:?}");
        let described = zuno_error::source::describe(&error);
        assert!(
            described.contains("Uvicorn"),
            "the failure must carry the output that proves it: {described}"
        );
        server.abort();
    }

    /// The same misconfiguration when the command exits instead of hanging: a CLI
    /// that printed its usage and left. Reported as a close, the only evidence — the
    /// text it printed — is dropped, and `list` even reports it as retryable.
    #[tokio::test]
    async fn stdio_one_stray_line_then_exit_names_the_line_not_a_bare_close() {
        let (client, server) = memory_client(Duration::from_secs(5));
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let _request = read_message(&mut server_reader).await;
            server_writer
                .write_all(b"usage: some-cli [options]\n")
                .await
                .expect("write the usage line");
            drop(server_writer);
        });

        let error = client
            .list_tools()
            .await
            .expect_err("a stream that never framed JSON-RPC must not look like a clean close");
        assert!(
            matches!(&error, McpError::Protocol { server, .. } if server == "fake"),
            "a command that printed prose and exited is not an MCP server: {error:?}"
        );
        assert!(!error.is_retryable(), "{error:?}");
        let described = zuno_error::source::describe(&error);
        assert!(
            described.contains("usage: some-cli"),
            "the failure must carry the output that proves it: {described}"
        );
        server.await.expect("fake server completed");
    }

    /// The other direction of the same predicate: silence is not a framing violation.
    ///
    /// A server that has said nothing at all may simply be slow, and a slow server
    /// that comes back on the next attempt must not be reported as permanently
    /// misconfigured. This is what stops the previous two tests from being satisfied
    /// by "every deadline is a protocol error".
    #[tokio::test]
    async fn stdio_a_silent_server_still_reports_a_retryable_deadline() {
        let (client, server) = memory_client(Duration::from_millis(300));
        let (server_reader, _server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let _request = read_message(&mut server_reader).await;
            std::future::pending::<()>().await;
        });

        let error = client
            .list_tools()
            .await
            .expect_err("a server that answers nothing must not report success");
        assert!(
            matches!(&error, McpError::Timeout { server, .. } if server == "fake"),
            "silence is a deadline, not a framing violation: {error:?}"
        );
        assert!(error.is_retryable(), "{error:?}");
        server.abort();
    }

    /// And the third direction: a server that answered once and then went quiet keeps
    /// its retryable deadline even though earlier output on the stream was not JSON.
    ///
    /// Guards the `has this stream ever framed JSON-RPC` half of the predicate. Drop
    /// it and a chatty working server becomes permanently broken the first time one
    /// call is slow.
    #[tokio::test]
    async fn stdio_a_server_that_answered_once_still_reports_a_retryable_deadline() {
        let (client, server) = memory_client(Duration::from_millis(300));
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let first = read_message(&mut server_reader).await;
            server_writer
                .write_all(b"Debugger attached.\n")
                .await
                .expect("write a stray non-JSON line");
            send_message(
                &mut server_writer,
                &json!({ "jsonrpc": "2.0", "id": first["id"], "result": { "tools": [] } }),
            )
            .await;
            let _second = read_message(&mut server_reader).await;
            std::future::pending::<()>().await;
        });

        client
            .list_tools()
            .await
            .expect("the server answers a list");
        let error = client
            .list_tools()
            .await
            .expect_err("the second call is never answered");
        assert!(
            matches!(&error, McpError::Timeout { server, .. } if server == "fake"),
            "a peer that has framed JSON-RPC is slow, not misconfigured: {error:?}"
        );
        assert!(error.is_retryable(), "{error:?}");
        server.abort();
    }

    /// A `tools/call` whose response is lost has an unknown outcome, not a failed one.
    ///
    /// The server may have created the pull request, sent the message, or written the
    /// row before its stdout stopped being readable. Reported as
    /// [`McpError::ToolCall`] this reaches the model as a definite failure, which is
    /// the at-most-once rule inverted: the model is told to try again.
    #[tokio::test]
    async fn stdio_a_lost_tool_call_response_is_uncertain_not_a_definite_failure() {
        let (client, server) = memory_client(Duration::from_secs(5));
        let (server_reader, server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let _call = read_message(&mut server_reader).await;
            // The side effect has landed by now; only the answer is lost. Both halves
            // go, because a `duplex` reports EOF when the whole stream is dropped and
            // dropping the write half alone would leave this a plain deadline — which
            // reports the same class for a different reason and would let this test
            // pass with the distinction reverted.
            drop(server_writer);
            drop(server_reader);
        });

        let started = std::time::Instant::now();
        let error = client
            .call_tool("create_pull_request", Map::new())
            .await
            .expect_err("a lost response is not a result");
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(1),
            "the reader saw the close, so this is not the deadline path: {waited:?}"
        );
        assert!(
            matches!(&error, McpError::Timeout { server, .. } if server == "fake"),
            "a call that may have taken effect must reach the caller as uncertain, \
             not as a definite failure: {error:?}"
        );
        server.await.expect("fake server completed");
    }

    /// The other half of the rule: a peer that emits nothing decodable is not noisy,
    /// it is not speaking JSON-RPC, and every call must not have to wait out its own
    /// deadline to learn that.
    #[tokio::test]
    async fn stdio_run_of_undecodable_lines_ends_the_connection() {
        // Far longer than the test needs, so a failure here is the bound and not a
        // deadline.
        let (client, server) = memory_client(Duration::from_secs(60));
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let _request = read_message(&mut server_reader).await;
            for _ in 0..MAX_CONSECUTIVE_UNDECODABLE_FRAMES {
                server_writer
                    .write_all(b"usage: some-cli [options]\n")
                    .await
                    .expect("write an undecodable line");
            }
            std::future::pending::<()>().await;
        });

        let error = client
            .list_tools()
            .await
            .expect_err("a stream with no decodable frame must fail the pending call");
        assert!(
            matches!(&error, McpError::Protocol { server, .. } if server == "fake"),
            "a peer that never frames a JSON-RPC message is a protocol violation: {error:?}"
        );
        assert!(
            !error.is_retryable(),
            "retrying a peer that is not speaking JSON-RPC repeats the violation"
        );
        server.abort();
    }

    /// The reviewer's input: a server whose stdout is nothing but blank lines.
    ///
    /// A blank line used to `continue` before the undecodable counter, which had two
    /// consequences: every line reached an unlatched `warn` (one log record per line,
    /// forever), and the run stayed at zero, so a peer that framed nothing but `\n` was
    /// reported as a bare retryable deadline that named no cause. It is the same
    /// framing violation as any other non-JSON frame and is reported as one, with a
    /// deadline long enough that a failure here cannot be the timeout.
    #[tokio::test]
    async fn stdio_a_stream_of_nothing_but_blank_lines_ends_the_connection() {
        let (client, server) = memory_client(Duration::from_secs(60));
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let _request = read_message(&mut server_reader).await;
            for _ in 0..MAX_CONSECUTIVE_UNDECODABLE_FRAMES {
                server_writer
                    .write_all(b"\n")
                    .await
                    .expect("write a blank stdout line");
            }
            std::future::pending::<()>().await;
        });

        let error = client
            .list_tools()
            .await
            .expect_err("a stream of blank lines must fail the pending call, not wait it out");
        let McpError::Protocol {
            server: name,
            source,
        } = &error
        else {
            panic!("a peer that never frames a JSON-RPC message is a protocol violation: {error:?}")
        };
        assert_eq!(name, "fake");
        let named = source.to_string();
        assert!(
            named.contains(&format!("{MAX_CONSECUTIVE_UNDECODABLE_FRAMES} frame(s)"))
                && named.ends_with("the last was empty"),
            "the refusal must name the run, not quote an empty excerpt: {named}"
        );
        assert!(
            !error.is_retryable(),
            "retrying a peer that is not speaking JSON-RPC repeats the violation"
        );
        server.abort();
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

    /// The bound is only safe if it cannot refuse a response a server means to send.
    #[test]
    fn stdio_frame_bound_cannot_reject_a_maximum_resource_blob() {
        // A `resources/read` blob arrives base64-encoded: four output bytes per three
        // input bytes, before the JSON envelope and any second content item.
        let largest_blob = crate::MAX_RESOURCE_BLOB_BYTES;
        let base64 = largest_blob.div_ceil(3) * 4;
        assert!(
            MAX_FRAME_BYTES > base64 * 4,
            "the frame bound ({MAX_FRAME_BYTES}) must leave several times the largest \
             attachment-eligible payload ({base64} base64 bytes) so no legitimate \
             response is rejected"
        );
        // Both bounds are constants, so their ordering is a compile-time invariant.
        const {
            assert!(
                MAX_STDERR_LINE_BYTES < MAX_FRAME_BYTES,
                "a diagnostic line needs a far tighter bound than a protocol frame"
            )
        };
    }

    #[tokio::test]
    async fn stdio_unterminated_frame_past_the_bound_fails_the_pending_caller() {
        const LIMIT: usize = 4 * 1024;

        let (client, server) = bounded_memory_client(Duration::from_secs(5), LIMIT);
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let _request = read_message(&mut server_reader).await;
            // A hostile or wedged peer: bytes forever, never a newline.
            server_writer
                .write_all(&vec![b'x'; LIMIT * 2])
                .await
                .expect("write the oversized frame");
            std::future::pending::<()>().await;
        });

        let error = client
            .list_tools()
            .await
            .expect_err("an unterminated frame past the bound must fail the pending call");
        let McpError::Protocol {
            server: named,
            source,
        } = &error
        else {
            panic!(
                "an over-long frame is a framing violation, which must block rather than \
                 look retryable: {error:?}"
            );
        };
        assert_eq!(named, "fake");
        assert!(
            source.to_string().contains("without a newline"),
            "the typed cause must say why the frame was refused: {source}"
        );
        assert!(
            !error.is_retryable(),
            "retrying a peer that cannot frame a message repeats the violation"
        );
        server.abort();
    }

    #[tokio::test]
    async fn stdio_frame_just_under_the_bound_still_round_trips() {
        const LIMIT: usize = 4 * 1024;

        let (client, server) = bounded_memory_client(Duration::from_secs(5), LIMIT);
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let server = tokio::spawn(async move {
            let request = read_message(&mut server_reader).await;
            // Grow the payload until one more byte would exceed the bound, so this
            // asserts the boundary rather than a comfortable margin.
            let mut payload = String::new();
            loop {
                let candidate = json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": format!("{payload}x"),
                });
                let framed = serde_json::to_vec(&candidate)
                    .expect("serialize fake-server response")
                    .len()
                    + 1;
                if framed > LIMIT {
                    break;
                }
                payload.push('x');
            }
            let response = json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": payload.clone(),
            });
            let framed = serde_json::to_vec(&response)
                .expect("serialize fake-server response")
                .len()
                + 1;
            assert!(
                framed <= LIMIT,
                "fixture must sit under the bound: {framed}"
            );
            assert!(
                framed > LIMIT - 8,
                "fixture must sit just under the bound, not far below it: {framed}"
            );
            send_message(&mut server_writer, &response).await;
            payload
        });

        let response = client
            .request("large", json!({}))
            .await
            .expect("a frame under the bound must still be delivered whole");
        let payload = server.await.expect("fake server completed");
        assert_eq!(response, Value::String(payload));
    }

    #[tokio::test]
    async fn stderr_drain_truncates_an_over_long_line_and_keeps_draining() {
        let (mut writer, reader) = duplex(4 * MAX_STDERR_LINE_BYTES);
        let task = spawn_stderr_reader("fake".to_owned(), reader);
        let writes = tokio::spawn(async move {
            writer
                .write_all(&vec![b'y'; MAX_STDERR_LINE_BYTES * 2])
                .await
                .expect("write the over-long stderr line");
            writer
                .write_all(b"\nstill draining\n")
                .await
                .expect("write the line after the truncated one");
            drop(writer);
        });

        // Reaching end-of-stream is the assertion: an unbounded reader would have
        // swallowed the whole line, and a reader that gave up on the bound the way
        // stdout does would never see the line that follows it.
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the stderr drain must survive an over-long line and reach EOF")
            .expect("stderr task does not panic");
        writes.await.expect("stderr writer completed");
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
        let fixture = tempfile::tempdir().expect("workspace fixture");
        let workspace = fixture.path().join("project");
        let config = McpLocal {
            kind: LocalKind::Local,
            command: vec!["opencode".to_owned(), "mcp".to_owned()],
            cwd: Some("nested/../server".to_owned()),
            environment: Some(BTreeMap::from([(
                "MCP_TEST_VALUE".to_owned(),
                "present".to_owned(),
            )])),
            enabled: None,
            timeout: None,
        };
        let command = build_command(&workspace, &config).expect("valid command");
        let command = command.as_std();
        assert_eq!(
            command.get_current_dir(),
            Some(workspace.join("server").as_path())
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
        assert!(!environments.contains_key("BUN_BE_BUN"));
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

    /// Every field of every event the stderr drain emits for a `probe-mcp*` server, by
    /// name, so a test can ask *where* the peer's bytes landed rather than only whether
    /// they were logged.
    ///
    /// Installed once, process-wide, through [`captured_events`]. A thread-scoped
    /// `tracing::subscriber::set_default` is not enough in this binary: it would be the
    /// only registered dispatcher, so the first thread to hit a drain callsite decides
    /// its cached interest from *its own* default — `NoSubscriber` on every test thread
    /// but this one — and the callsite is then skipped here without `enabled` ever
    /// being asked. A global default is consulted for every callsite on every thread,
    /// and the `server` name keeps one test's events apart from another's.
    #[derive(Default)]
    struct CapturedEvents {
        events: Mutex<Vec<Vec<(&'static str, String)>>>,
    }

    impl CapturedEvents {
        fn events_for(&self, server: &str) -> Vec<Vec<(&'static str, String)>> {
            lock(&self.events)
                .iter()
                .filter(|fields| {
                    fields
                        .iter()
                        .any(|(name, value)| *name == "server" && value == server)
                })
                .cloned()
                .collect()
        }
    }

    fn captured_events() -> Arc<CapturedEvents> {
        static CAPTURED: OnceLock<Arc<CapturedEvents>> = OnceLock::new();
        Arc::clone(CAPTURED.get_or_init(|| {
            let captured = Arc::new(CapturedEvents::default());
            tracing::subscriber::set_global_default(Capture(Arc::clone(&captured)))
                .expect("this test binary installs exactly one tracing subscriber");
            captured
        }))
    }

    struct Capture(Arc<CapturedEvents>);

    struct FieldRecorder(Vec<(&'static str, String)>);

    impl tracing::field::Visit for FieldRecorder {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push((field.name(), format!("{value:?}")));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push((field.name(), value.to_owned()));
        }
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields = FieldRecorder(Vec::new());
            event.record(&mut fields);
            if fields
                .0
                .iter()
                .any(|(name, value)| *name == "server" && value.starts_with("probe-mcp"))
            {
                lock(&self.0.events).push(fields.0);
            }
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// The peer's stderr bytes reach the log under exactly one field name, `stderr`,
    /// which `zuno_observability`'s redaction policy classifies as a payload; they never
    /// ride in the event's own text and never under a name the policy does not know.
    ///
    /// Measured before this pin: the same line, recorded as `message`, rendered through
    /// the shipped text sink as `DEBUG …: MCP server stderr server=probe-mcp
    /// Traceback: API_KEY=sk-live-abc123` — verbatim in the plaintext log, on
    /// `--print-logs` stderr, and in the `message` column of `logs.sqlite`, because
    /// `message` is the one field name the policy leaves readable. Both callsites in
    /// [`spawn_stderr_reader`] are driven here: the ordinary line and the over-long line
    /// that is truncated at [`MAX_STDERR_LINE_BYTES`]. The observability side of the
    /// same contract — that a field named `stderr` carrying this exact value is
    /// `[redacted]` in every sink — is pinned in
    /// `crates/zuno-observability/tests/stdout_purity.rs`.
    #[tokio::test]
    async fn stderr_drain_names_the_peer_line_only_by_a_field_the_log_policy_redacts() {
        const SECRET_LINE: &str = "Traceback: API_KEY=sk-live-abc123";
        const SECRET: &str = "sk-live-abc123";
        const SERVER: &str = "probe-mcp-stderr-drain";
        let captured = captured_events();

        let (mut writer, reader) = duplex(4 * MAX_STDERR_LINE_BYTES);
        let task = spawn_stderr_reader(SERVER.to_owned(), reader);
        writer
            .write_all(format!("{SECRET_LINE}\n").as_bytes())
            .await
            .expect("write the peer's stderr line");
        let mut over_long = SECRET_LINE.as_bytes().to_vec();
        over_long.extend(std::iter::repeat_n(b'y', MAX_STDERR_LINE_BYTES * 2));
        over_long.push(b'\n');
        writer
            .write_all(&over_long)
            .await
            .expect("write the over-long stderr line");
        drop(writer);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("stderr drain reaches EOF")
            .expect("stderr task does not panic");

        let events = captured.events_for(SERVER);
        let carrying = events
            .iter()
            .filter(|fields| fields.iter().any(|(_, value)| value.contains(SECRET)))
            .count();
        assert_eq!(
            carrying, 2,
            "both the plain and the truncated line must reach the subscriber, or the \
             field-name assertion below is vacuous: {events:?}"
        );
        for fields in &events {
            for (name, value) in fields {
                if value.contains(SECRET) {
                    assert_eq!(
                        *name, "stderr",
                        "peer stderr bytes reached the log under field {name:?}, which \
                         zuno_observability does not redact: {value:?}"
                    );
                }
            }
            let text = fields
                .iter()
                .filter(|(name, _)| *name == "message")
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>();
            assert!(
                !text.is_empty() && text.iter().all(|value| !value.contains(SECRET)),
                "the event text must be Zuno's own literal, not the peer's line: {text:?}"
            );
        }
    }

    /// A reader that has stopped can never answer, so a request refused before the write
    /// is a permanent failure on every path — `list_tools` no less than `call_tool`.
    ///
    /// Measured before this pin, against a server that closed its stdout after the
    /// handshake and kept reading stdin: `call_tool` returned
    /// `ToolCall { source: Closed }` (`Recovery::Fail`) while `list_tools` returned
    /// `Connect { source: Closed }` (`Recovery::Retry`), and nothing reconnects a stdio
    /// client, so the retry could only repeat the refusal. The refusal is honest either
    /// way — nothing was written, as the fake server's stdin proves — but the recovery
    /// it carried was not.
    #[tokio::test]
    async fn stdio_a_dead_reader_refuses_list_and_call_with_the_same_permanent_recovery() {
        let (client, server) = memory_client(Duration::from_secs(5));
        let (server_reader, mut server_writer) = split(server);
        // The server's stdout ends; its stdin stays open and readable.
        server_writer
            .shutdown()
            .await
            .expect("close the fake server's stdout");
        tokio::time::timeout(Duration::from_secs(5), async {
            while client.inner.reader.exit().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reader records why it stopped");

        let call = client
            .call_tool("t", Map::new())
            .await
            .expect_err("a call against a dead reader is refused");
        let list = client
            .list_tools()
            .await
            .expect_err("a list against a dead reader is refused");

        for (path, error) in [("call_tool", &call), ("list_tools", &list)] {
            assert!(
                !error.is_retryable(),
                "{path} reported a retryable failure against a reader that can never \
                 answer: {error:?} -> {:?}",
                error.recovery()
            );
            assert_eq!(error.server(), "fake");
            let source = std::error::Error::source(error)
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                source.contains("MCP connection closed"),
                "{path} must still name the closed connection as its cause: {error:?}"
            );
        }
        assert!(
            matches!(call, McpError::ToolCall { ref tool, .. } if tool == "t"),
            "call_tool keeps naming the tool: {call:?}"
        );
        assert!(
            matches!(list, McpError::Handshake { .. }),
            "list_tools reports the dead transport in this crate's permanent class: {list:?}"
        );

        // Nothing reached the server: the refusal preceded the write.
        drop(client);
        let mut unread = Vec::new();
        let mut server_reader = BufReader::new(server_reader);
        tokio::time::timeout(
            Duration::from_secs(5),
            server_reader.read_to_end(&mut unread),
        )
        .await
        .expect("the fake server's stdin reaches EOF once the client is gone")
        .expect("read the fake server's stdin");
        assert!(
            unread.is_empty(),
            "a refusal against a dead reader must not write the request: {:?}",
            String::from_utf8_lossy(&unread)
        );
    }
}
