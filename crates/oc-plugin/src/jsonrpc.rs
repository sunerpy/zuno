//! Out-of-process plugins over newline-delimited JSON-RPC.
//!
//! One stdout reader classifies frames before touching the id-indexed waiter map.
//! Notifications and server requests therefore cannot consume a pending response.
//! Process startup is parallel, while the returned plugin vector stays in config
//! order (`packages/opencode/src/plugin/loader.ts:203-235`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::join_all;
use oc_db::message::{MessageRecord, PartRecord};
use oc_error::{BoxSource, ToolError};
use oc_llm::event::Message;
use oc_plugin_sdk::{
    HookCall, HookResult, HostInfo, InitializeParams, InitializeResult, PROTOCOL_VERSION, ToolCall,
    ToolContext as WireToolContext, ToolDefinition as WireToolDefinition,
};
use oc_tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio::task::JoinHandle;

use crate::js::projection::{
    HookModelBoundary, JsModelArrival, JsModelProjection, SdkGeneration, chat_context_value,
    model_value, plugin_model, provider_source, provider_value,
};
use crate::{
    ChatHeadersOutput, ChatParamsOutput, ChatSystemTransformOutput, CompactionAutocontinueOutput,
    HookInvocation, HookName, MessageWithParts, PermissionStatus, Plugin, PluginManifest,
    PluginTools, ProviderSmallModelOutput, SessionCompactingOutput, ShellEnvOutput,
    TextCompleteOutput, ToolExecuteBeforeOutput,
};

/// Production deadline applied independently to initialization, hooks, and tools.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

const TASK_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
const PROCESS_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

type DynReader = Pin<Box<dyn AsyncRead + Send>>;
type DynWriter = Pin<Box<dyn AsyncWrite + Send>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<PendingResult>>>>;
type PendingResult = Result<Value, ReaderFailure>;

/// Process declaration for one configured plugin.
#[derive(Debug, Clone)]
pub struct PluginProcessSpec {
    name: String,
    program: PathBuf,
    arguments: Vec<OsString>,
    cwd: Option<PathBuf>,
    environment: BTreeMap<OsString, OsString>,
    timeout: Duration,
}

impl PluginProcessSpec {
    /// Name the config entry independently from the manifest it may never return.
    #[must_use]
    pub fn new(name: impl Into<String>, program: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            arguments: Vec::new(),
            cwd: None,
            environment: BTreeMap::new(),
            timeout: DEFAULT_HOOK_TIMEOUT,
        }
    }

    #[must_use]
    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    #[must_use]
    pub fn current_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.cwd = Some(directory.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    /// Inject a short deterministic deadline without changing production policy.
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Why a process was skipped or permanently disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginDiagnosticKind {
    FailedToLoad,
    Crashed,
    TimedOut,
    Protocol,
}

/// A contained failure suitable for status output and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDiagnostic {
    pub plugin: String,
    pub hook: Option<String>,
    pub kind: PluginDiagnosticKind,
    pub message: String,
}

impl PluginDiagnostic {
    fn failed_to_load(plugin: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            hook: None,
            kind: PluginDiagnosticKind::FailedToLoad,
            message: message.into(),
        }
    }
}

struct Lifecycle {
    plugin: Mutex<String>,
    enabled: AtomicBool,
    closing: AtomicBool,
    diagnostics: Mutex<Vec<PluginDiagnostic>>,
}

impl Lifecycle {
    fn new(plugin: String) -> Self {
        Self {
            plugin: Mutex::new(plugin),
            enabled: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            diagnostics: Mutex::new(Vec::new()),
        }
    }

    fn rename(&self, plugin: &str) {
        *lock(&self.plugin) = plugin.to_owned();
    }

    fn record(&self, kind: PluginDiagnosticKind, hook: Option<String>, message: impl Into<String>) {
        if self.enabled.swap(false, Ordering::SeqCst) {
            let diagnostic = PluginDiagnostic {
                plugin: lock(&self.plugin).clone(),
                hook,
                kind,
                message: message.into(),
            };
            tracing::warn!(
                plugin = %diagnostic.plugin,
                hook = ?diagnostic.hook,
                message = %diagnostic.message,
                "disabled out-of-process plugin"
            );
            lock(&self.diagnostics).push(diagnostic);
        }
    }
}

struct Shutdown {
    sender: Mutex<Option<oneshot::Sender<()>>>,
}

impl Shutdown {
    fn signal(&self) {
        if let Some(sender) = lock(&self.sender).take() {
            let _sent = sender.send(());
        }
    }
}

struct Transport {
    timeout: Duration,
    next_id: AtomicU64,
    writer: AsyncMutex<Option<DynWriter>>,
    pending: Pending,
    lifecycle: Arc<Lifecycle>,
    shutdown: Arc<Shutdown>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
    process_task: Mutex<Option<JoinHandle<()>>>,
}

impl Transport {
    async fn spawn(spec: &PluginProcessSpec) -> io::Result<Arc<Self>> {
        let (program, arguments) = oc_process::guarded_argv(&spec.program, &spec.arguments);
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("ZUNO_PLUGIN_NAME", &spec.name)
            .env("ZUNO_PLUGIN_PROTOCOL_VERSION", PROTOCOL_VERSION)
            .envs(&spec.environment);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("spawned plugin has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("spawned plugin has no stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            drain_stderr(spec.name.clone(), stderr);
        }

        let lifecycle = Arc::new(Lifecycle::new(spec.name.clone()));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let shutdown = Arc::new(Shutdown {
            sender: Mutex::new(Some(shutdown_sender)),
        });
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let process_task = tokio::spawn(supervise_process(
            spec.name.clone(),
            child,
            shutdown_receiver,
            Arc::clone(&lifecycle),
        ));
        let reader_task = tokio::spawn(read_loop(
            spec.name.clone(),
            Box::pin(stdout),
            Arc::clone(&pending),
            Arc::clone(&lifecycle),
            Arc::clone(&shutdown),
        ));
        Ok(Arc::new(Self {
            timeout: spec.timeout,
            next_id: AtomicU64::new(1),
            writer: AsyncMutex::new(Some(Box::pin(stdin))),
            pending,
            lifecycle,
            shutdown,
            reader_task: Mutex::new(Some(reader_task)),
            process_task: Mutex::new(Some(process_task)),
        }))
    }

    fn rename(&self, plugin: &str) {
        self.lifecycle.rename(plugin);
    }

    fn is_enabled(&self) -> bool {
        self.lifecycle.enabled.load(Ordering::SeqCst)
    }

    fn diagnostics(&self) -> Vec<PluginDiagnostic> {
        lock(&self.lifecycle.diagnostics).clone()
    }

    fn disable(
        &self,
        kind: PluginDiagnosticKind,
        hook: Option<String>,
        message: impl Into<String>,
    ) {
        self.lifecycle.record(kind, hook, message);
        self.shutdown.signal();
        fail_pending(&self.pending, ReaderFailure::Closed);
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, TransportError> {
        if !self.is_enabled() {
            return Err(TransportError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id, sender);
        let exchange = async {
            self.write(&message).await?;
            let response = receiver.await.map_err(|_| TransportError::Closed)??;
            decode_response(response)
        };
        let result = match tokio::time::timeout(self.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(TransportError::Timeout),
        };
        lock(&self.pending).remove(&id);
        result
    }

    async fn write(&self, message: &Value) -> Result<(), TransportError> {
        let mut bytes = serde_json::to_vec(message).map_err(TransportError::Json)?;
        bytes.push(b'\n');
        let mut writer = self.writer.lock().await;
        let writer = writer.as_mut().ok_or(TransportError::Closed)?;
        writer.write_all(&bytes).await.map_err(TransportError::Io)?;
        writer.flush().await.map_err(TransportError::Io)
    }

    async fn close(&self) {
        if self.lifecycle.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        self.lifecycle.enabled.store(false, Ordering::SeqCst);
        if let Some(mut writer) = self.writer.lock().await.take() {
            let _closed = writer.shutdown().await;
        }
        fail_pending(&self.pending, ReaderFailure::Closed);
        self.shutdown.signal();
        let process_task = lock(&self.process_task).take();
        if let Some(task) = process_task {
            finish_task(task, PROCESS_SHUTDOWN_GRACE).await;
        }
        let reader_task = lock(&self.reader_task).take();
        if let Some(task) = reader_task {
            finish_task(task, TASK_SHUTDOWN_GRACE).await;
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.lifecycle.closing.store(true, Ordering::SeqCst);
        self.shutdown.signal();
        fail_pending(&self.pending, ReaderFailure::Closed);
        if let Some(task) = lock(&self.reader_task).take() {
            task.abort();
        }
        let _reaper = lock(&self.process_task).take();
    }
}

async fn supervise_process(
    plugin: String,
    mut child: Child,
    mut shutdown: oneshot::Receiver<()>,
    lifecycle: Arc<Lifecycle>,
) {
    let result = tokio::select! {
        status = child.wait() => status.map(|status| Some(status.code())),
        _ = &mut shutdown => child.kill().await.map(|()| None),
    };
    match result {
        Ok(Some(code)) if !lifecycle.closing.load(Ordering::SeqCst) => lifecycle.record(
            PluginDiagnosticKind::Crashed,
            None,
            format!("plugin process exited with code {code:?}"),
        ),
        Ok(Some(code)) => tracing::debug!(%plugin, ?code, "plugin process exited"),
        Ok(None) => tracing::debug!(%plugin, "plugin process was stopped and reaped"),
        Err(error) => lifecycle.record(
            PluginDiagnosticKind::Crashed,
            None,
            format!("could not reap plugin process: {error}"),
        ),
    }
}

async fn read_loop(
    plugin: String,
    reader: DynReader,
    pending: Pending,
    lifecycle: Arc<Lifecycle>,
    shutdown: Arc<Shutdown>,
) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                fail_pending(&pending, ReaderFailure::Closed);
                if !lifecycle.closing.load(Ordering::SeqCst) {
                    lifecycle.record(PluginDiagnosticKind::Crashed, None, "plugin closed stdout");
                }
                return;
            }
            Ok(_) => {
                let frame = line.trim_end_matches(['\r', '\n']);
                let message = match serde_json::from_str::<Value>(frame) {
                    Ok(message) => message,
                    Err(error) => {
                        fail_pending(
                            &pending,
                            ReaderFailure::Protocol(Arc::from(error.to_string())),
                        );
                        lifecycle.record(
                            PluginDiagnosticKind::Protocol,
                            None,
                            format!("plugin emitted malformed JSON: {error}"),
                        );
                        shutdown.signal();
                        return;
                    }
                };
                route_message(&plugin, &pending, message);
            }
            Err(error) => {
                fail_pending(
                    &pending,
                    ReaderFailure::Protocol(Arc::from(error.to_string())),
                );
                lifecycle.record(
                    PluginDiagnosticKind::Crashed,
                    None,
                    format!("could not read plugin stdout: {error}"),
                );
                shutdown.signal();
                return;
            }
        }
    }
}

fn route_message(plugin: &str, pending: &Pending, message: Value) {
    let is_response = message.get("id").and_then(Value::as_u64).is_some()
        && (message.get("result").is_some() || message.get("error").is_some());
    if is_response {
        let id = message.get("id").and_then(Value::as_u64);
        if let Some(id) = id {
            if let Some(sender) = lock(pending).remove(&id) {
                let _sent = sender.send(Ok(message));
            } else {
                tracing::debug!(%plugin, id, "ignored response with unknown plugin request id");
            }
        }
    } else if let Some(method) = message.get("method").and_then(Value::as_str) {
        tracing::debug!(%plugin, %method, "ignored plugin notification or server request");
    } else {
        tracing::debug!(%plugin, "ignored unclassified plugin frame");
    }
}

fn decode_response(message: Value) -> Result<Value, TransportError> {
    if let Some(result) = message.get("result") {
        return Ok(result.clone());
    }
    if let Some(error) = message.get("error") {
        return Err(TransportError::Remote {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(-32_000),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("plugin returned an unspecified JSON-RPC error")
                .to_owned(),
        });
    }
    Err(TransportError::Protocol(
        "plugin response contains neither result nor error".to_owned(),
    ))
}

fn fail_pending(pending: &Pending, failure: ReaderFailure) {
    let waiters = {
        let mut pending = lock(pending);
        pending
            .drain()
            .map(|(_, sender)| sender)
            .collect::<Vec<_>>()
    };
    for waiter in waiters {
        let _sent = waiter.send(Err(failure.clone()));
    }
}

#[derive(Debug, Clone)]
enum ReaderFailure {
    Closed,
    Protocol(Arc<str>),
}

impl From<ReaderFailure> for TransportError {
    fn from(failure: ReaderFailure) -> Self {
        match failure {
            ReaderFailure::Closed => Self::Closed,
            ReaderFailure::Protocol(message) => Self::Protocol(message.to_string()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum TransportError {
    #[error("plugin request timed out")]
    Timeout,
    #[error("plugin connection is closed")]
    Closed,
    #[error("plugin transport I/O failed: {0}")]
    Io(io::Error),
    #[error("plugin JSON encoding failed: {0}")]
    Json(serde_json::Error),
    #[error("plugin protocol failed: {0}")]
    Protocol(String),
    #[error("plugin returned JSON-RPC error {code}: {message}")]
    Remote { code: i64, message: String },
}

fn drain_stderr(plugin: String, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => return,
                Ok(_) => tracing::debug!(
                    %plugin,
                    message = line.trim_end_matches(['\r', '\n']),
                    "plugin stderr"
                ),
                Err(error) => {
                    tracing::debug!(%plugin, %error, "could not drain plugin stderr");
                    return;
                }
            }
        }
    });
}

async fn finish_task(mut task: JoinHandle<()>, grace: Duration) {
    if tokio::time::timeout(grace, &mut task).await.is_err() {
        task.abort();
        let _joined = task.await;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Results of parallel resolution, retained in configuration order.
pub struct PluginLoad {
    plugins: Vec<Arc<JsonRpcPlugin>>,
    startup_diagnostics: Vec<PluginDiagnostic>,
}

impl PluginLoad {
    #[must_use]
    pub fn plugins(&self) -> &[Arc<JsonRpcPlugin>] {
        &self.plugins
    }

    /// Reuse the existing sequential bus as the sole dispatch-order authority.
    #[must_use]
    pub fn hook_bus(&self) -> crate::HookBus {
        crate::HookBus::new(
            self.plugins
                .iter()
                .cloned()
                .map(|plugin| plugin as Arc<dyn Plugin>)
                .collect(),
        )
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<PluginDiagnostic> {
        let mut diagnostics = self.startup_diagnostics.clone();
        for plugin in &self.plugins {
            diagnostics.extend(plugin.diagnostics());
        }
        diagnostics
    }

    /// Reject shadowing before plugin tools cross into the shared registry.
    pub fn validate_tool_names<'a>(
        &self,
        reserved: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), PluginToolConflict> {
        validate_tool_names(
            self.plugins.iter().flat_map(|plugin| {
                plugin
                    .inner
                    .tools
                    .iter()
                    .map(|(name, _)| (name.as_str(), None))
            }),
            reserved,
        )
    }

    pub async fn shutdown(&self) {
        join_all(self.plugins.iter().map(|plugin| plugin.shutdown())).await;
    }
}

/// A plugin tool would make registry lookup order decide which implementation runs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginToolConflict {
    #[error("plugin tool `{name}` conflicts with a reserved tool name")]
    Reserved { name: String },
    #[error("duplicate plugin tool name `{name}`")]
    Duplicate { name: String },
    #[error(
        "duplicate plugin tool name `{name}` from `{first}` and `{second}`",
        first = first.display(),
        second = second.display()
    )]
    DuplicateSources {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
}

pub(crate) fn validate_tool_names<'a, 'b, 'c>(
    tools: impl IntoIterator<Item = (&'a str, Option<&'b std::path::Path>)>,
    reserved: impl IntoIterator<Item = &'c str>,
) -> Result<(), PluginToolConflict> {
    let reserved = reserved.into_iter().collect::<BTreeSet<_>>();
    let mut extension_names = BTreeMap::new();
    for (name, origin) in tools {
        if reserved.contains(name) {
            return Err(PluginToolConflict::Reserved {
                name: name.to_owned(),
            });
        }
        if let Some(first) = extension_names.insert(name, origin) {
            return match (first, origin) {
                (Some(first), Some(second)) => Err(PluginToolConflict::DuplicateSources {
                    name: name.to_owned(),
                    first: first.to_path_buf(),
                    second: second.to_path_buf(),
                }),
                _ => Err(PluginToolConflict::Duplicate {
                    name: name.to_owned(),
                }),
            };
        }
    }
    Ok(())
}

/// Spawn all configured processes concurrently and register successes in input order.
pub async fn load_plugins_ordered(specs: Vec<PluginProcessSpec>) -> PluginLoad {
    let attempts = join_all(specs.into_iter().map(JsonRpcPlugin::spawn)).await;
    let mut plugins = Vec::new();
    let mut startup_diagnostics = Vec::new();
    for attempt in attempts {
        match attempt {
            Ok(plugin) => plugins.push(plugin),
            Err(diagnostic) => startup_diagnostics.push(diagnostic),
        }
    }
    PluginLoad {
        plugins,
        startup_diagnostics,
    }
}

/// A plugin process adapted to todo 57's resident [`Plugin`] trait.
pub struct JsonRpcPlugin {
    inner: Arc<PluginInner>,
}

struct PluginInner {
    manifest: PluginManifest,
    tools: Vec<(String, Arc<dyn Tool>)>,
    transport: Arc<Transport>,
}

impl JsonRpcPlugin {
    async fn spawn(spec: PluginProcessSpec) -> Result<Arc<Self>, PluginDiagnostic> {
        let configured_name = spec.name.clone();
        let transport = Transport::spawn(&spec).await.map_err(|error| {
            PluginDiagnostic::failed_to_load(&configured_name, error.to_string())
        })?;
        let params = serde_json::to_value(InitializeParams {
            protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
            host: HostInfo {
                name: "zuno".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        })
        .map_err(|error| PluginDiagnostic::failed_to_load(&configured_name, error.to_string()))?;
        let initialized = match transport.request("plugin.initialize", params).await {
            Ok(value) => serde_json::from_value::<InitializeResult>(value).map_err(|error| {
                PluginDiagnostic::failed_to_load(
                    &configured_name,
                    format!("invalid initialize result: {error}"),
                )
            }),
            Err(error) => Err(PluginDiagnostic::failed_to_load(
                &configured_name,
                error.to_string(),
            )),
        };
        let initialized = match initialized {
            Ok(initialized) => initialized,
            Err(diagnostic) => {
                transport.close().await;
                return Err(diagnostic);
            }
        };
        if initialized.protocol_version != PROTOCOL_VERSION {
            transport.close().await;
            return Err(PluginDiagnostic::failed_to_load(
                &configured_name,
                format!(
                    "plugin selected protocol {}, host offered {PROTOCOL_VERSION}",
                    initialized.protocol_version
                ),
            ));
        }

        let hooks = initialized
            .plugin
            .hooks
            .iter()
            .map(|name| HookName::from_str(name))
            .collect::<Result<Vec<_>, _>>();
        let hooks = match hooks {
            Ok(hooks) => hooks,
            Err(error) => {
                transport.close().await;
                return Err(PluginDiagnostic::failed_to_load(
                    &configured_name,
                    error.to_string(),
                ));
            }
        };
        let manifest = match PluginManifest::new(&initialized.plugin.id, hooks) {
            Ok(manifest) => manifest,
            Err(error) => {
                transport.close().await;
                return Err(PluginDiagnostic::failed_to_load(
                    &configured_name,
                    error.to_string(),
                ));
            }
        };
        transport.rename(manifest.id());

        let mut seen_tools = BTreeSet::new();
        let mut tools: Vec<(String, Arc<dyn Tool>)> = Vec::new();
        for definition in initialized.plugin.tools {
            if definition.id.trim().is_empty() || !seen_tools.insert(definition.id.clone()) {
                transport.close().await;
                return Err(PluginDiagnostic::failed_to_load(
                    manifest.id(),
                    format!(
                        "plugin returned an empty or duplicate tool id `{}`",
                        definition.id
                    ),
                ));
            }
            let id = definition.id.clone();
            tools.push((
                id,
                Arc::new(RemoteTool {
                    definition,
                    transport: Arc::clone(&transport),
                }) as Arc<dyn Tool>,
            ));
        }
        if !tools.is_empty() && !manifest.supports(HookName::Tool) {
            transport.close().await;
            return Err(PluginDiagnostic::failed_to_load(
                manifest.id(),
                "plugin returned tools without declaring the `tool` resource",
            ));
        }

        Ok(Arc::new(Self {
            inner: Arc::new(PluginInner {
                manifest,
                tools,
                transport,
            }),
        }))
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.inner.transport.is_enabled()
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<PluginDiagnostic> {
        self.inner.transport.diagnostics()
    }

    pub async fn shutdown(&self) {
        self.inner.transport.close().await;
    }

    async fn dispatch_remote(&self, hook: &mut HookInvocation<'_>) {
        if !self.is_enabled() {
            return;
        }
        let name = hook.name().to_string();
        let call = match encode_hook(hook) {
            Ok(call) => call,
            Err(error) => {
                self.inner.transport.disable(
                    PluginDiagnosticKind::Protocol,
                    Some(name),
                    error.to_string(),
                );
                return;
            }
        };
        let params = match serde_json::to_value(call) {
            Ok(params) => params,
            Err(error) => {
                self.inner.transport.disable(
                    PluginDiagnosticKind::Protocol,
                    Some(name),
                    error.to_string(),
                );
                return;
            }
        };
        match self.inner.transport.request("hook.call", params).await {
            Ok(value) => {
                let result = serde_json::from_value::<HookResult>(value)
                    .map_err(HookCodecError::Json)
                    .and_then(|result| apply_hook_output(hook, result.output));
                if let Err(error) = result {
                    self.inner.transport.disable(
                        PluginDiagnosticKind::Protocol,
                        Some(name),
                        error.to_string(),
                    );
                }
            }
            Err(TransportError::Timeout) => self.inner.transport.disable(
                PluginDiagnosticKind::TimedOut,
                Some(name),
                format!("hook exceeded {:?}", self.inner.transport.timeout),
            ),
            Err(error) => self.inner.transport.disable(
                PluginDiagnosticKind::Crashed,
                Some(name),
                error.to_string(),
            ),
        }
    }
}

#[async_trait]
impl Plugin for JsonRpcPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.inner.manifest
    }

    fn tools(&self) -> PluginTools {
        if !self.is_enabled() {
            return PluginTools::new();
        }
        self.inner.tools.iter().cloned().collect()
    }

    async fn call(&self, hook: &mut HookInvocation<'_>) -> Result<(), BoxSource> {
        self.dispatch_remote(hook).await;
        Ok(())
    }
}

struct RemoteTool {
    definition: WireToolDefinition,
    transport: Arc<Transport>,
}

#[async_trait]
impl Tool for RemoteTool {
    fn id(&self) -> &str {
        &self.definition.id
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn raw_parameters_schema(&self) -> Value {
        self.definition.parameters.clone()
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if !self.transport.is_enabled() {
            return Err(self.failure("plugin is disabled"));
        }
        let params = serde_json::to_value(ToolCall {
            tool: self.definition.id.clone(),
            arguments: args,
            context: WireToolContext {
                session_id: ctx.session_id,
                message_id: ctx.message_id,
                call_id: ctx.call_id,
                agent: ctx.agent,
                depth: ctx.depth,
            },
        })
        .map_err(|error| self.failure(error.to_string()))?;
        match self.transport.request("tool.call", params).await {
            Ok(value) => serde_json::from_value(value).map_err(|error| {
                self.transport.disable(
                    PluginDiagnosticKind::Protocol,
                    Some(format!("tool:{}", self.definition.id)),
                    error.to_string(),
                );
                self.failure(error.to_string())
            }),
            Err(TransportError::Timeout) => {
                self.transport.disable(
                    PluginDiagnosticKind::TimedOut,
                    Some(format!("tool:{}", self.definition.id)),
                    format!("tool exceeded {:?}", self.transport.timeout),
                );
                Err(ToolError::Timeout {
                    tool: self.definition.id.clone(),
                    elapsed: self.transport.timeout,
                })
            }
            Err(error) => {
                self.transport.disable(
                    PluginDiagnosticKind::Crashed,
                    Some(format!("tool:{}", self.definition.id)),
                    error.to_string(),
                );
                Err(self.failure(error.to_string()))
            }
        }
    }
}

impl RemoteTool {
    fn failure(&self, message: impl Into<String>) -> ToolError {
        ToolError::Failed {
            tool: self.definition.id.clone(),
            source: Box::new(RemoteToolFailure(message.into())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("remote plugin tool failed: {0}")]
struct RemoteToolFailure(String);

pub(crate) fn encode_hook(hook: &HookInvocation<'_>) -> Result<HookCall, HookCodecError> {
    let model_boundary = HookModelBoundary::classify(hook);
    let model_projection = JsModelArrival::Hook(model_boundary).projection();
    let (input, output) = match hook {
        HookInvocation::Dispose => (Value::Null, Value::Null),
        HookInvocation::Event { event } => (json!({ "event": event_value(event)? }), Value::Null),
        HookInvocation::Config { config } => (Value::Null, serde_json::to_value(&**config)?),
        HookInvocation::Tool { .. }
        | HookInvocation::Auth { .. }
        | HookInvocation::Provider { .. } => {
            return Err(HookCodecError::Invalid(
                "resource hooks are negotiated during initialization".to_owned(),
            ));
        }
        HookInvocation::ChatMessage { input, output } => (
            {
                debug_assert_eq!(model_projection, JsModelProjection::ModelSelection);
                json!({
                "sessionID": input.session_id,
                "agent": input.agent,
                "model": input.model.map(|model| json!({
                    "providerID": model.provider_id,
                    "modelID": model.model_id,
                })),
                "messageID": input.message_id,
                "variant": input.variant,
                })
            },
            json!({
                "message": output.message.to_json(),
                "parts": encode_parts(&output.parts)?,
            }),
        ),
        HookInvocation::ChatParams { input, output } => (
            {
                debug_assert_eq!(model_projection, JsModelProjection::LegacySdk);
                chat_context_value(input).into_json()
            },
            json!({
                "temperature": output.temperature,
                "topP": output.top_p,
                "topK": output.top_k,
                "maxOutputTokens": output.max_output_tokens,
                "options": output.options,
            }),
        ),
        HookInvocation::ChatHeaders { input, output } => (
            {
                debug_assert_eq!(model_projection, JsModelProjection::LegacySdk);
                chat_context_value(input).into_json()
            },
            json!({ "headers": output.headers }),
        ),
        HookInvocation::PermissionAsk { input, output } => (
            serde_json::to_value(input.request)?,
            json!({ "status": permission_status(output.status) }),
        ),
        HookInvocation::CommandExecuteBefore { input, output } => (
            json!({
                "command": input.command,
                "sessionID": input.session_id,
                "arguments": input.arguments,
            }),
            json!({ "parts": encode_parts(&output.parts)? }),
        ),
        HookInvocation::ToolExecuteBefore { input, output } => (
            json!({
                "tool": input.tool,
                "sessionID": input.session_id,
                "callID": input.call_id,
            }),
            json!({ "args": output.args }),
        ),
        HookInvocation::ShellEnv { input, output } => (
            json!({
                "cwd": input.cwd,
                "sessionID": input.session_id,
                "callID": input.call_id,
            }),
            json!({ "env": output.env }),
        ),
        HookInvocation::ToolExecuteAfter { input, output } => (
            json!({
                "tool": input.tool,
                "sessionID": input.session_id,
                "callID": input.call_id,
                "args": input.args,
            }),
            serde_json::to_value(&**output)?,
        ),
        HookInvocation::ChatMessagesTransform { output } => (
            json!({}),
            json!({
                "messages": output.messages.iter().map(message_with_parts_value).collect::<Result<Vec<_>, _>>()?,
            }),
        ),
        HookInvocation::ChatSystemTransform { input, output } => (
            {
                debug_assert_eq!(model_projection, JsModelProjection::LegacySdk);
                json!({
                "sessionID": input.session_id,
                "model": model_value(input.model, SdkGeneration::Legacy).into_json(),
                })
            },
            json!({ "system": output.system }),
        ),
        HookInvocation::ProviderSmallModel { input, output } => (
            {
                debug_assert_eq!(model_projection, JsModelProjection::V2Sdk);
                json!({
                    "provider": provider_value(
                        input.provider,
                        SdkGeneration::V2,
                        provider_source(input.provider),
                        None,
                    ).into_json(),
                })
            },
            json!({
                "model": output.model.as_ref().map(|model| {
                    model_value(model, SdkGeneration::V2).into_json()
                }),
            }),
        ),
        HookInvocation::SessionCompacting { input, output } => (
            json!({ "sessionID": input.session_id }),
            json!({ "context": output.context, "prompt": output.prompt }),
        ),
        HookInvocation::CompactionAutocontinue { input, output } => {
            debug_assert_eq!(model_projection, JsModelProjection::LegacySdk);
            let mut value = chat_context_value(input.context).into_json();
            let object = value.as_object_mut().ok_or_else(|| {
                HookCodecError::Invalid("chat context must encode as an object".to_owned())
            })?;
            object.insert("overflow".to_owned(), Value::Bool(input.overflow));
            (value, json!({ "enabled": output.enabled }))
        }
        HookInvocation::TextComplete { input, output } => (
            json!({
                "sessionID": input.session_id,
                "messageID": input.message_id,
                "partID": input.part_id,
            }),
            json!({ "text": output.text }),
        ),
        HookInvocation::ToolDefinition { input, output } => (
            json!({ "toolID": input.tool_id }),
            json!({
                "id": output.id,
                "description": output.description,
                "parameters": output.parameters,
            }),
        ),
    };
    Ok(HookCall {
        hook: hook.name().as_str().to_owned(),
        input,
        output,
    })
}

pub(crate) fn apply_hook_output(
    hook: &mut HookInvocation<'_>,
    value: Value,
) -> Result<(), HookCodecError> {
    match hook {
        HookInvocation::Dispose | HookInvocation::Event { .. } => Ok(()),
        HookInvocation::Config { config } => {
            **config = decode(value)?;
            Ok(())
        }
        HookInvocation::Tool { .. }
        | HookInvocation::Auth { .. }
        | HookInvocation::Provider { .. } => Err(HookCodecError::Invalid(
            "resource hooks cannot return callback output".to_owned(),
        )),
        HookInvocation::ChatMessage { output, .. } => {
            let output_value: WireChatMessageOutput = decode(value)?;
            output.message = MessageRecord::from_json(output_value.message)
                .map_err(|error| HookCodecError::Invalid(error.to_string()))?;
            output.parts = decode_parts(output_value.parts)?;
            Ok(())
        }
        HookInvocation::ChatParams { output, .. } => {
            let remote: WireChatParamsOutput = decode(value)?;
            **output = ChatParamsOutput {
                temperature: remote.temperature,
                top_p: remote.top_p,
                top_k: remote.top_k,
                max_output_tokens: remote.max_output_tokens,
                options: remote.options,
            };
            Ok(())
        }
        HookInvocation::ChatHeaders { output, .. } => {
            let remote: WireHeadersOutput = decode(value)?;
            **output = ChatHeadersOutput {
                headers: remote.headers,
            };
            Ok(())
        }
        HookInvocation::PermissionAsk { output, .. } => {
            let remote: WirePermissionOutput = decode(value)?;
            output.status = match remote.status.as_str() {
                "ask" => PermissionStatus::Ask,
                "deny" => PermissionStatus::Deny,
                "allow" => PermissionStatus::Allow,
                other => {
                    return Err(HookCodecError::Invalid(format!(
                        "unknown permission status `{other}`"
                    )));
                }
            };
            Ok(())
        }
        HookInvocation::CommandExecuteBefore { output, .. } => {
            let remote: WirePartsOutput = decode(value)?;
            output.parts = decode_parts(remote.parts)?;
            Ok(())
        }
        HookInvocation::ToolExecuteBefore { output, .. } => {
            let remote: WireArgsOutput = decode(value)?;
            **output = ToolExecuteBeforeOutput { args: remote.args };
            Ok(())
        }
        HookInvocation::ShellEnv { output, .. } => {
            let remote: WireEnvOutput = decode(value)?;
            **output = ShellEnvOutput { env: remote.env };
            Ok(())
        }
        HookInvocation::ToolExecuteAfter { output, .. } => {
            **output = decode(value)?;
            Ok(())
        }
        HookInvocation::ChatMessagesTransform { output } => {
            let remote: WireMessagesOutput = decode(value)?;
            output.messages = remote
                .messages
                .into_iter()
                .map(|message| {
                    Ok(MessageWithParts {
                        info: message.info,
                        parts: decode_parts(message.parts)?,
                    })
                })
                .collect::<Result<Vec<_>, HookCodecError>>()?;
            Ok(())
        }
        HookInvocation::ChatSystemTransform { output, .. } => {
            let remote: WireSystemOutput = decode(value)?;
            **output = ChatSystemTransformOutput {
                system: remote.system,
            };
            Ok(())
        }
        HookInvocation::ProviderSmallModel { output, .. } => {
            let remote: WireSmallModelOutput = decode(value)?;
            **output = ProviderSmallModelOutput {
                model: remote
                    .model
                    .map(|model| plugin_model(model, SdkGeneration::V2))
                    .transpose()?,
            };
            Ok(())
        }
        HookInvocation::SessionCompacting { output, .. } => {
            let remote: WireCompactingOutput = decode(value)?;
            **output = SessionCompactingOutput {
                context: remote.context,
                prompt: remote.prompt,
            };
            Ok(())
        }
        HookInvocation::CompactionAutocontinue { output, .. } => {
            let remote: WireAutocontinueOutput = decode(value)?;
            **output = CompactionAutocontinueOutput {
                enabled: remote.enabled,
            };
            Ok(())
        }
        HookInvocation::TextComplete { output, .. } => {
            let remote: WireTextOutput = decode(value)?;
            **output = TextCompleteOutput { text: remote.text };
            Ok(())
        }
        HookInvocation::ToolDefinition { output, .. } => {
            let remote: WireToolDefinition = decode(value)?;
            **output = ToolDefinition {
                id: remote.id,
                description: remote.description,
                parameters: remote.parameters,
            };
            Ok(())
        }
    }
}

fn event_value(event: &oc_engine::r#loop::TurnEvent) -> Result<Value, HookCodecError> {
    use oc_engine::r#loop::TurnEvent;
    let value = match event {
        TurnEvent::TurnStarted { session_id } => {
            json!({ "type": "turn.started", "sessionID": session_id })
        }
        TurnEvent::HistoryRepaired {
            repaired_tool_results,
        } => json!({ "type": "history.repaired", "repairedToolResults": repaired_tool_results }),
        TurnEvent::AgentResolved { step, agent } => {
            json!({ "type": "agent.resolved", "step": step, "agent": agent })
        }
        TurnEvent::ModelResolved {
            step,
            provider_id,
            model_id,
        } => {
            json!({ "type": "model.resolved", "step": step, "providerID": provider_id, "modelID": model_id })
        }
        TurnEvent::AssistantMessageCreated { step, message_id } => {
            json!({ "type": "assistant.message.created", "step": step, "messageID": message_id })
        }
        TurnEvent::ToolSnapshotLocked {
            step,
            tool_ids,
            rebuilt_for_late_mcp,
        } => {
            json!({ "type": "tool.snapshot.locked", "step": step, "toolIDs": tool_ids, "rebuiltForLateMcp": rebuilt_for_late_mcp })
        }
        TurnEvent::ProviderRequestStarted {
            step,
            message_count,
        } => {
            json!({ "type": "provider.request.started", "step": step, "messageCount": message_count })
        }
        TurnEvent::Provider { step, event } => {
            json!({ "type": "provider", "step": step, "event": stream_event_value(event) })
        }
        TurnEvent::AssistantCheckpointed {
            step,
            message_id,
            interrupted,
        } => {
            json!({ "type": "assistant.checkpointed", "step": step, "messageID": message_id, "interrupted": interrupted })
        }
        TurnEvent::ToolDispatchStarted {
            step,
            call_id,
            name,
        } => {
            json!({ "type": "tool.dispatch.started", "step": step, "callID": call_id, "name": name })
        }
        TurnEvent::ToolDispatchCompleted {
            step,
            call_id,
            name,
            title,
            output,
            is_error,
        } => {
            json!({ "type": "tool.dispatch.completed", "step": step, "callID": call_id, "name": name, "title": title, "output": output, "isError": is_error })
        }
        TurnEvent::ToolResultAppended {
            step,
            call_id,
            is_error,
        } => {
            json!({ "type": "tool.result.appended", "step": step, "callID": call_id, "isError": is_error })
        }
        TurnEvent::StepCompleted {
            step,
            finish_reason,
        } => json!({ "type": "step.completed", "step": step, "finishReason": finish_reason }),
        TurnEvent::TurnCompleted {
            assistant_message_id,
            steps,
        } => {
            json!({ "type": "turn.completed", "assistantMessageID": assistant_message_id, "steps": steps })
        }
        TurnEvent::TurnInterrupted {
            assistant_message_id,
            steps,
        } => {
            json!({ "type": "turn.interrupted", "assistantMessageID": assistant_message_id, "steps": steps })
        }
    };
    Ok(value)
}

fn stream_event_value(event: &oc_llm::event::StreamEvent) -> Value {
    use oc_llm::event::{ConnectionPhase, StreamEvent};
    match event {
        StreamEvent::TextDelta(text) => json!({ "type": "text.delta", "text": text }),
        StreamEvent::ToolUseStart { id, name } => {
            json!({ "type": "tool.use.start", "id": id, "name": name })
        }
        StreamEvent::ToolInputDelta(delta) => json!({ "type": "tool.input.delta", "delta": delta }),
        StreamEvent::ToolUseEnd => json!({ "type": "tool.use.end" }),
        StreamEvent::ToolUseSignature(signature) => {
            json!({ "type": "tool.use.signature", "signature": signature })
        }
        StreamEvent::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            json!({ "type": "tool.result", "toolUseID": tool_use_id, "content": content, "isError": is_error })
        }
        StreamEvent::GeneratedImage {
            id,
            path,
            metadata_path,
            output_format,
            revised_prompt,
        } => {
            json!({ "type": "generated.image", "id": id, "path": path, "metadataPath": metadata_path, "outputFormat": output_format, "revisedPrompt": revised_prompt })
        }
        StreamEvent::ReasoningStart => json!({ "type": "reasoning.start" }),
        StreamEvent::ReasoningDelta(text) => json!({ "type": "reasoning.delta", "text": text }),
        StreamEvent::ReasoningSignatureDelta(signature) => {
            json!({ "type": "reasoning.signature.delta", "signature": signature })
        }
        StreamEvent::ProviderReasoningItem {
            id,
            summary,
            encrypted_content,
            status,
        } => {
            json!({ "type": "provider.reasoning.item", "id": id, "summary": summary, "encryptedContent": encrypted_content, "status": status })
        }
        StreamEvent::ReasoningEnd => json!({ "type": "reasoning.end" }),
        StreamEvent::ReasoningDone { duration_secs } => {
            json!({ "type": "reasoning.done", "durationSecs": duration_secs })
        }
        StreamEvent::MessageEnd { stop_reason } => {
            json!({ "type": "message.end", "stopReason": stop_reason })
        }
        StreamEvent::RetryRollback { attempt, max } => {
            json!({ "type": "retry.rollback", "attempt": attempt, "max": max })
        }
        StreamEvent::TokenUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
        } => {
            json!({ "type": "token.usage", "inputTokens": input_tokens, "outputTokens": output_tokens, "cacheReadInputTokens": cache_read_input_tokens, "cacheWriteInputTokens": cache_write_input_tokens })
        }
        StreamEvent::ConnectionType { connection } => {
            json!({ "type": "connection.type", "connection": connection })
        }
        StreamEvent::ConnectionPhase { phase } => match phase {
            ConnectionPhase::Authenticating => {
                json!({ "type": "connection.phase", "phase": "authenticating" })
            }
            ConnectionPhase::Connecting => {
                json!({ "type": "connection.phase", "phase": "connecting" })
            }
            ConnectionPhase::SendingRequest => {
                json!({ "type": "connection.phase", "phase": "sending-request" })
            }
            ConnectionPhase::WaitingForResponse => {
                json!({ "type": "connection.phase", "phase": "waiting-for-response" })
            }
            ConnectionPhase::Streaming => {
                json!({ "type": "connection.phase", "phase": "streaming" })
            }
            ConnectionPhase::Retrying { attempt, max } => {
                json!({ "type": "connection.phase", "phase": "retrying", "attempt": attempt, "max": max })
            }
        },
        StreamEvent::StatusDetail { detail } => {
            json!({ "type": "status.detail", "detail": detail })
        }
        StreamEvent::Error {
            message,
            retry_after,
        } => {
            json!({ "type": "error", "message": message, "retryAfterMs": retry_after.map(|value| value.as_millis()) })
        }
        StreamEvent::SessionId(id) => json!({ "type": "session.id", "id": id }),
        StreamEvent::Compaction {
            trigger,
            pre_tokens,
            openai_encrypted_content,
        } => {
            json!({ "type": "compaction", "trigger": trigger, "preTokens": pre_tokens, "openaiEncryptedContent": openai_encrypted_content })
        }
        StreamEvent::UpstreamProvider { provider } => {
            json!({ "type": "upstream.provider", "provider": provider })
        }
        StreamEvent::NativeToolCall {
            request_id,
            tool_name,
            input,
        } => {
            json!({ "type": "native.tool.call", "requestID": request_id, "toolName": tool_name, "input": input })
        }
    }
}

fn permission_status(status: PermissionStatus) -> &'static str {
    match status {
        PermissionStatus::Ask => "ask",
        PermissionStatus::Deny => "deny",
        PermissionStatus::Allow => "allow",
    }
}

const TIME_CREATED: &str = "$ocTimeCreated";
const TIME_UPDATED: &str = "$ocTimeUpdated";

fn encode_parts(parts: &[PartRecord]) -> Result<Vec<Value>, HookCodecError> {
    parts
        .iter()
        .map(|part| {
            let mut value = part.to_json();
            let object = value.as_object_mut().ok_or_else(|| {
                HookCodecError::Invalid("part must encode as an object".to_owned())
            })?;
            object.insert(TIME_CREATED.to_owned(), Value::from(part.time_created));
            object.insert(TIME_UPDATED.to_owned(), Value::from(part.time_updated));
            Ok(value)
        })
        .collect()
}

fn decode_parts(values: Vec<Value>) -> Result<Vec<PartRecord>, HookCodecError> {
    values
        .into_iter()
        .map(|mut value| {
            let object = value.as_object_mut().ok_or_else(|| {
                HookCodecError::Invalid("plugin returned a non-object part".to_owned())
            })?;
            let time_created = object
                .remove(TIME_CREATED)
                .and_then(|value| value.as_i64())
                .ok_or_else(|| HookCodecError::Invalid("part lost its creation time".to_owned()))?;
            let time_updated = object
                .remove(TIME_UPDATED)
                .and_then(|value| value.as_i64())
                .ok_or_else(|| HookCodecError::Invalid("part lost its update time".to_owned()))?;
            let mut part = PartRecord::from_json(value, time_created)
                .map_err(|error| HookCodecError::Invalid(error.to_string()))?;
            part.time_updated = time_updated;
            Ok(part)
        })
        .collect()
}

fn message_with_parts_value(message: &MessageWithParts) -> Result<Value, HookCodecError> {
    Ok(json!({
        "info": message.info,
        "parts": encode_parts(&message.parts)?,
    }))
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, HookCodecError> {
    serde_json::from_value(value).map_err(HookCodecError::Json)
}

#[derive(Deserialize)]
struct WireChatMessageOutput {
    message: Value,
    parts: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireChatParamsOutput {
    temperature: f64,
    top_p: f64,
    top_k: f64,
    max_output_tokens: Option<u64>,
    options: serde_json::Map<String, Value>,
}

#[derive(Deserialize)]
struct WireHeadersOutput {
    headers: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct WirePermissionOutput {
    status: String,
}

#[derive(Deserialize)]
struct WirePartsOutput {
    parts: Vec<Value>,
}

#[derive(Deserialize)]
struct WireArgsOutput {
    args: Value,
}

#[derive(Deserialize)]
struct WireEnvOutput {
    env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct WireMessagesOutput {
    messages: Vec<WireMessageWithParts>,
}

#[derive(Deserialize)]
struct WireMessageWithParts {
    info: Message,
    parts: Vec<Value>,
}

#[derive(Deserialize)]
struct WireSystemOutput {
    system: Vec<String>,
}

#[derive(Deserialize)]
struct WireSmallModelOutput {
    model: Option<Value>,
}

#[derive(Deserialize)]
struct WireCompactingOutput {
    context: Vec<String>,
    prompt: Option<String>,
}

#[derive(Deserialize)]
struct WireAutocontinueOutput {
    enabled: bool,
}

#[derive(Deserialize)]
struct WireTextOutput {
    text: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum HookCodecError {
    #[error("plugin hook JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("plugin hook output is invalid: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn jsonrpc_notification_does_not_consume_an_id_matched_response() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = oneshot::channel();
        lock(&pending).insert(7, sender);

        route_message(
            "fixture",
            &pending,
            json!({ "jsonrpc": "2.0", "method": "plugin.progress", "params": {} }),
        );
        assert!(lock(&pending).contains_key(&7));
        route_message(
            "fixture",
            &pending,
            json!({ "jsonrpc": "2.0", "id": 7, "result": { "ok": true } }),
        );

        let response = receiver
            .await
            .expect("matched sender remains live")
            .expect("response is successful");
        assert_eq!(response["result"], json!({ "ok": true }));
    }
}
