//! The resident host: one long-lived JavaScript process holding callable handles.
//!
//! # Why this is not a serialization bridge
//!
//! An auth hook's substance is closures. `validate?: (value) => string | undefined`
//! on a text prompt, the `loader` that builds provider options, the `authorize`
//! that starts an OAuth flow, and the `callback` that `authorize`'s result carries
//! (`packages/plugin/src/index.ts:88-208`). Marshalling the hook object once would
//! deliver its labels and drop everything that does work. Measured on the two real
//! plugins: kiro@0.20.6's auth hook alone carries **11** closures — one `loader`,
//! four `authorize`, and six prompt `validate` — and antigravity@1.6.0's carries
//! two.
//!
//! So the process stays alive. Every function crossing the boundary is retained in
//! the shim's handle registry and replaced with `{"$fn": id}`; Rust holds the id and
//! round-trips a `call` frame whenever the value is needed. [`JsHandle`] is that id.
//! A bridge that serialized once could not implement [`JsHandle::call`] at all,
//! which is why the acceptance test for this module invokes a real prompt validator
//! from Rust and observes its return value.
//!
//! # Why the protocol runs over a loopback socket and not stdio
//!
//! stdin belongs to the terminal. `opencode-antigravity-auth` prompts through
//! `node:readline/promises` (`dist/src/plugin/cli.js:1`), and a plugin that has to
//! read a device code cannot have a JSON protocol on its stdin — that is exactly
//! the deadlock `oc_engine::terminal_lease` exists to prevent, one layer down. So
//! fd 0 and fd 1 stay inherited and the protocol gets its own loopback connection.
//!
//! The listener is bound **once** and kept for the host's whole life, including
//! across restarts. Binding port 0, learning the port, dropping the listener and
//! re-binding is a real race — a sibling steals the port between the two calls —
//! and this project has already paid for that flake once.
//!
//! # Bounded, because everything here is
//!
//! A `bun` child inside a project whose point is bounded memory gets the same
//! discipline as the rest of it: a memory ceiling sampled from the OS and enforced
//! by restart, a per-hook deadline, a handle-count ceiling, and a bounded restart
//! budget after which the plugin is disabled with a diagnostic rather than
//! respawned forever. See [`JsHostLimits`].
//!
//! # Why the host owns a runtime thread
//!
//! `crate::AuthTextValidator` is **synchronous** — the oracle's `validate` returns
//! `string | undefined`, not a promise — but calling into the child is inherently
//! async. Blocking the caller's runtime on a task scheduled by that same runtime
//! deadlocks on a current-thread executor, which is what `#[tokio::test]` gives you
//! by default. So the transport runs on a runtime this host owns, and a synchronous
//! validator blocks its own thread on a channel while the transport makes progress
//! elsewhere. The cost is one extra thread per host; the alternative is a validator
//! that cannot be called from a test.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use oc_engine::terminal_lease::{LeaseReason, TerminalLease, TerminalLeaseGuard};
use oc_tool::{PermissionAsk, ToolContext};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::process::{Child, Command};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio::task::JoinHandle;

use crate::js::runtime::JsRuntime;
use crate::js::spec::JsPluginSpec;
use crate::{PluginDiagnostic, PluginDiagnosticKind};

/// The embedded shim. Written to disk at spawn time; never bundles a runtime.
pub const SHIM_SOURCE: &str = include_str!("shim.mjs");

/// The protocol string both halves must agree on.
pub const JS_PROTOCOL_VERSION: &str = "js-compat-1";

/// How long to wait for the child to connect back before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds applied to the resident child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsHostLimits {
    /// Resident-set ceiling in bytes. Exceeding it restarts the child.
    ///
    /// 512 MiB: measured baseline for the two real plugins is 84-94 MiB RSS
    /// immediately after init (kiro pulls `libsql`), so the ceiling is roughly 5x
    /// headroom — high enough that normal operation never trips it and low enough
    /// that a leak is caught long before the machine notices.
    pub memory_ceiling: u64,
    /// Deadline for one hook or handle invocation.
    pub hook_timeout: Duration,
    /// Deadline for `init`, which imports the plugin and runs its factory.
    ///
    /// Larger than [`Self::hook_timeout`] because init is where a cold `libsql`
    /// load and a module graph of tens of thousands of lines is paid for.
    pub init_timeout: Duration,
    /// Restarts permitted inside [`Self::restart_window`] before disabling.
    pub max_restarts: u32,
    /// The window the restart budget is counted over.
    pub restart_window: Duration,
    /// Handle-count ceiling. A plugin past it is leaking callables.
    pub max_handles: usize,
}

impl Default for JsHostLimits {
    fn default() -> Self {
        Self {
            memory_ceiling: 512 * 1024 * 1024,
            hook_timeout: Duration::from_secs(30),
            init_timeout: Duration::from_secs(60),
            max_restarts: 3,
            restart_window: Duration::from_secs(300),
            max_handles: 4_096,
        }
    }
}

/// The host values a plugin factory receives (`packages/plugin/src/index.ts:56-66`).
///
/// Held as strings and paths rather than as the engine's own types because these
/// cross into JSON verbatim; a caller assembles them from `ResolvedProject` and the
/// running server's address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsPluginInput {
    /// `project`, serialized as the SDK's `Project` shape.
    pub project: Value,
    /// `directory`.
    pub directory: PathBuf,
    /// `worktree`.
    pub worktree: PathBuf,
    /// `serverUrl`, which the shim turns into a real `URL`.
    pub server_url: String,
    /// Options from a `[name, options]` config spec.
    pub options: Option<Value>,
    /// An explicit `@opencode-ai/sdk` directory, when the caller knows better than
    /// the walk up from the entry point.
    pub sdk_module: Option<PathBuf>,
    /// A loopback port reserved for a plugin that starts its own `node:http`
    /// listener.
    ///
    /// `opencode-antigravity-auth` does (`dist/src/plugin/server.js:1`). Handing it
    /// a port the host has already bound and released is the coordination: the host
    /// knows the number is free of its own allocations, and the plugin reads it from
    /// `OPENCODE_PLUGIN_LOOPBACK_PORT` rather than picking one that might collide
    /// with the host's protocol socket.
    pub loopback_port: Option<u16>,
}

impl JsPluginInput {
    /// The minimum a plugin factory needs, with `project` derived from `worktree`.
    #[must_use]
    pub fn new(
        directory: impl Into<PathBuf>,
        worktree: impl Into<PathBuf>,
        server_url: impl Into<String>,
    ) -> Self {
        let directory = directory.into();
        let worktree = worktree.into();
        Self {
            project: json!({
                "id": "local",
                "worktree": worktree.display().to_string(),
                "vcs": "git",
                "time": { "created": 0, "initialized": 0 },
            }),
            directory,
            worktree,
            server_url: server_url.into(),
            options: None,
            sdk_module: None,
            loopback_port: None,
        }
    }

    /// Attach the options from a `[name, options]` config spec.
    #[must_use]
    pub fn with_options(mut self, options: Value) -> Self {
        self.options = Some(options);
        self
    }

    /// Point the shim at a specific `@opencode-ai/sdk` install.
    #[must_use]
    pub fn with_sdk_module(mut self, path: impl Into<PathBuf>) -> Self {
        self.sdk_module = Some(path.into());
        self
    }
}

/// What `init` reported: the plugin's identity, hooks, and resource descriptors.
#[derive(Debug, Clone)]
pub struct JsInitReport {
    /// `PluginModule.id`, when the package declares one.
    pub id: Option<String>,
    /// The exports whose factories ran, in the order they ran.
    pub exports: Vec<String>,
    /// Which `@opencode-ai/sdk` the client was built from.
    pub sdk: Option<String>,
    /// `bun` or `node`, as the child reports itself.
    pub runtime: String,
    /// Hook property names present on the merged returned objects.
    pub hooks: Vec<String>,
    /// One descriptor per registered `auth` hook.
    pub auth: Vec<Value>,
    /// One descriptor per registered `provider` hook.
    pub provider: Vec<Value>,
    /// One descriptor per registered tool.
    pub tools: Vec<Value>,
    /// `experimental_workspace.register` calls made during init.
    pub workspace: Vec<Value>,
    /// Hook name to the handles implementing it, in registration order.
    pub callbacks: HashMap<String, Vec<u64>>,
}

/// A live reference to a function retained inside the child.
///
/// The `generation` is what makes a restart safe: a handle minted before a restart
/// names a closure the new process never created, and calling it must fail loudly
/// rather than hit whatever id was reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsHandle {
    id: u64,
    arity: u32,
    generation: u64,
}

impl JsHandle {
    /// The child-side registry id.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// The JavaScript function's declared parameter count.
    #[must_use]
    pub const fn arity(&self) -> u32 {
        self.arity
    }

    /// Read a handle out of an encoded descriptor field, if it is one.
    #[must_use]
    pub fn from_value(value: &Value, generation: u64) -> Option<Self> {
        let id = value.get("$fn")?.as_u64()?;
        let arity = value
            .get("$arity")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .try_into()
            .unwrap_or(u32::MAX);
        Some(Self {
            id,
            arity,
            generation,
        })
    }
}

/// A failure of the resident host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsHostError {
    #[error("plugin `{plugin}` could not be started: {detail}")]
    Spawn { plugin: String, detail: String },
    #[error("plugin `{plugin}` did not connect back within {} ms", timeout.as_millis())]
    Connect { plugin: String, timeout: Duration },
    #[error("plugin `{plugin}` sent an unusable handshake: {detail}")]
    Handshake { plugin: String, detail: String },
    #[error("plugin `{plugin}` did not answer `{method}` within {} ms", timeout.as_millis())]
    Timeout {
        plugin: String,
        method: String,
        timeout: Duration,
    },
    #[error("plugin `{plugin}` is disabled: {detail}")]
    Disabled { plugin: String, detail: String },
    #[error("plugin `{plugin}` failed `{method}`: {detail}")]
    Remote {
        plugin: String,
        method: String,
        detail: String,
    },
    #[error(
        "plugin `{plugin}` handle {handle} belongs to generation {handle_generation}, \
         but the host has restarted and is now on generation {current_generation}"
    )]
    StaleHandle {
        plugin: String,
        handle: u64,
        handle_generation: u64,
        current_generation: u64,
    },
    #[error("plugin `{plugin}` protocol failure: {detail}")]
    Protocol { plugin: String, detail: String },
}

impl JsHostError {
    /// The diagnostic class this failure records.
    #[must_use]
    pub const fn kind(&self) -> PluginDiagnosticKind {
        match self {
            Self::Spawn { .. } | Self::Connect { .. } | Self::Handshake { .. } => {
                PluginDiagnosticKind::FailedToLoad
            }
            Self::Timeout { .. } => PluginDiagnosticKind::TimedOut,
            Self::Disabled { .. } => PluginDiagnosticKind::Crashed,
            Self::Remote { .. } | Self::StaleHandle { .. } | Self::Protocol { .. } => {
                PluginDiagnosticKind::Protocol
            }
        }
    }
}

/// The dedicated runtime the transport lives on. See the module note.
struct Executor {
    runtime: Option<Runtime>,
}

impl Executor {
    fn new(name: &str) -> io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name(format!("oc-js-{name}"))
            .enable_all()
            .build()?;
        Ok(Self {
            runtime: Some(runtime),
        })
    }

    fn handle(&self) -> Handle {
        self.runtime
            .as_ref()
            .map_or_else(Handle::current, |runtime| runtime.handle().clone())
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // Dropping a `Runtime` from inside an async context panics, and a host is
        // very likely dropped from one. `shutdown_background` is the escape hatch.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Everything needed to spawn the child again after it dies.
struct BootPlan {
    runtime: JsRuntime,
    shim: PathBuf,
    entry: PathBuf,
    spec: String,
    kind: &'static str,
    input: JsPluginInput,
}

/// One live child and the writer half of its connection.
struct Session {
    writer: OwnedWriteHalf,
    child: Child,
    pid: Option<u32>,
    reader: JoinHandle<()>,
    stderr: Option<JoinHandle<()>>,
}

type Waiters = Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

struct HostInner {
    plugin: String,
    limits: JsHostLimits,
    boot: BootPlan,
    listener: TcpListener,
    token: String,
    executor: Handle,
    session: AsyncMutex<Option<Session>>,
    waiters: Waiters,
    next_id: AtomicU64,
    generation: AtomicU64,
    enabled: AtomicBool,
    closing: AtomicBool,
    diagnostics: Mutex<Vec<PluginDiagnostic>>,
    restarts: Mutex<VecDeque<Instant>>,
    report: Mutex<Option<Arc<JsInitReport>>>,
    terminal: Option<Arc<dyn TerminalLease>>,
    lease: Mutex<Option<TerminalLeaseGuard>>,
    handle_peak: AtomicU64,
    tool_contexts: Mutex<HashMap<u64, (String, ToolContext)>>,
    next_tool_context: AtomicU64,
    host_functions: Mutex<HashMap<u64, Value>>,
    next_host_function: AtomicU64,
    _temp: Option<Arc<tempfile::TempDir>>,
}

/// A resident JavaScript compat host for one plugin.
///
/// Cloning shares the child: an auth hook's `validate` closure has to reach the
/// process for as long as the hook is alive, so the handle must be shareable.
#[derive(Clone)]
pub struct JsHost {
    inner: Arc<HostInner>,
    // Field order matters: `inner`'s tasks live on this executor, so the executor
    // must outlive them. Rust drops fields in declaration order, so `inner` first.
    _executor: Arc<Executor>,
}

impl std::fmt::Debug for JsHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JsHost")
            .field("plugin", &self.inner.plugin)
            .field("generation", &self.generation())
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

/// How a host is configured before it is started.
pub struct JsHostBuilder {
    plugin: String,
    runtime: JsRuntime,
    entry: PathBuf,
    spec: String,
    kind: &'static str,
    input: JsPluginInput,
    limits: JsHostLimits,
    terminal: Option<Arc<dyn TerminalLease>>,
    shim: Option<PathBuf>,
}

impl JsHostBuilder {
    /// A host for one plugin's entry point.
    #[must_use]
    pub fn new(
        plugin: impl Into<String>,
        runtime: JsRuntime,
        spec: &JsPluginSpec,
        entry: impl Into<PathBuf>,
        input: JsPluginInput,
    ) -> Self {
        Self {
            plugin: plugin.into(),
            runtime,
            entry: entry.into(),
            spec: spec.spec().to_owned(),
            kind: spec.kind().as_str(),
            input,
            limits: JsHostLimits::default(),
            terminal: None,
            shim: None,
        }
    }

    /// A resident host for one config-directory tool module.
    #[must_use]
    pub fn config_tool(
        runtime: JsRuntime,
        entry: impl Into<PathBuf>,
        input: JsPluginInput,
    ) -> Self {
        let entry = entry.into();
        let label = entry.display().to_string();
        Self {
            plugin: label.clone(),
            runtime,
            entry,
            spec: format!("file:{label}"),
            kind: "config-tool",
            input,
            limits: JsHostLimits::default(),
            terminal: None,
            shim: None,
        }
    }

    /// Override the bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: JsHostLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Route the child's interactive prompts through a terminal lease.
    ///
    /// Without one, a prompt is granted immediately: a headless server has no TUI
    /// to contend with and refusing would break `opencode run`. With one, the TUI
    /// steps aside for the duration and the handoff is observable.
    #[must_use]
    pub fn with_terminal_lease(mut self, terminal: Arc<dyn TerminalLease>) -> Self {
        self.terminal = Some(terminal);
        self
    }

    /// Use a shim already on disk instead of writing the embedded one.
    #[must_use]
    pub fn with_shim(mut self, shim: impl Into<PathBuf>) -> Self {
        self.shim = Some(shim.into());
        self
    }

    /// Spawn the child, accept its connection, and run `init`.
    ///
    /// # Errors
    /// Returns [`JsHostError`] for a failed spawn, a child that never connects, a
    /// bad handshake, an `init` timeout, or an `init` the plugin itself rejected.
    pub async fn start(self) -> Result<JsHost, JsHostError> {
        let plugin = self.plugin.clone();
        let executor =
            Arc::new(
                Executor::new(&sanitize(&plugin)).map_err(|error| JsHostError::Spawn {
                    plugin: plugin.clone(),
                    detail: format!("could not create a runtime for the plugin host: {error}"),
                })?,
            );
        let handle = executor.handle();

        let (shim, temp) = match self.shim {
            Some(path) => (path, None),
            None => {
                let directory = tempfile::Builder::new()
                    .prefix("oc-js-shim-")
                    .tempdir()
                    .map_err(|error| JsHostError::Spawn {
                        plugin: plugin.clone(),
                        detail: format!("could not create a shim directory: {error}"),
                    })?;
                let path = directory.path().join("shim.mjs");
                std::fs::write(&path, SHIM_SOURCE).map_err(|error| JsHostError::Spawn {
                    plugin: plugin.clone(),
                    detail: format!("could not write the shim: {error}"),
                })?;
                (path, Some(Arc::new(directory)))
            }
        };

        // Bound once, kept forever. See the module note about the port race.
        let listener = bind_loopback(&handle, &plugin).await?;
        let token = mint_token();

        let inner = Arc::new(HostInner {
            plugin: plugin.clone(),
            limits: self.limits,
            boot: BootPlan {
                runtime: self.runtime,
                shim,
                entry: self.entry,
                spec: self.spec,
                kind: self.kind,
                input: self.input,
            },
            listener,
            token,
            executor: handle,
            session: AsyncMutex::new(None),
            waiters: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            generation: AtomicU64::new(1),
            enabled: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            diagnostics: Mutex::new(Vec::new()),
            restarts: Mutex::new(VecDeque::new()),
            report: Mutex::new(None),
            terminal: self.terminal,
            lease: Mutex::new(None),
            handle_peak: AtomicU64::new(0),
            tool_contexts: Mutex::new(HashMap::new()),
            next_tool_context: AtomicU64::new(1),
            host_functions: Mutex::new(HashMap::new()),
            next_host_function: AtomicU64::new(1),
            _temp: temp,
        });

        let host = JsHost {
            inner: Arc::clone(&inner),
            _executor: executor,
        };
        let boot = Arc::clone(&inner);
        inner
            .executor
            .spawn(async move { boot.boot().await })
            .await
            .map_err(|error| JsHostError::Spawn {
                plugin,
                detail: format!("the host executor stopped during startup: {error}"),
            })??;
        let monitor = Arc::downgrade(&inner);
        inner
            .executor
            .spawn(async move { supervision_loop(monitor).await });
        Ok(host)
    }
}

impl JsHost {
    /// The plugin name used in every diagnostic.
    #[must_use]
    pub fn plugin(&self) -> &str {
        &self.inner.plugin
    }

    /// The current generation. Increments on every restart.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::SeqCst)
    }

    /// Whether the host is still usable.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::SeqCst)
    }

    /// The latest `init` report.
    #[must_use]
    pub fn report(&self) -> Option<Arc<JsInitReport>> {
        lock(&self.inner.report).clone()
    }

    /// Contained failures recorded so far.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<PluginDiagnostic> {
        lock(&self.inner.diagnostics).clone()
    }

    pub(crate) async fn disable(
        &self,
        plugin: impl Into<String>,
        kind: PluginDiagnosticKind,
        hook: impl Into<String>,
        message: impl Into<String>,
    ) {
        let plugin = plugin.into();
        let hook = hook.into();
        let message = message.into();
        {
            let mut diagnostics = lock(&self.inner.diagnostics);
            if let Some(existing) = diagnostics.iter_mut().rev().find(|diagnostic| {
                diagnostic.hook.as_deref() == Some("call") && diagnostic.message == message
            }) {
                existing.plugin.clone_from(&plugin);
                existing.hook = Some(hook.clone());
                existing.kind = kind;
            } else if !diagnostics.iter().any(|diagnostic| {
                diagnostic.plugin == plugin
                    && diagnostic.hook.as_deref() == Some(hook.as_str())
                    && diagnostic.message == message
            }) {
                diagnostics.push(PluginDiagnostic {
                    plugin: plugin.clone(),
                    hook: Some(hook.clone()),
                    kind,
                    message: message.clone(),
                });
            }
        }
        tracing::warn!(
            plugin = %plugin,
            hook = %hook,
            %message,
            "disabled JavaScript plugin after hook failure"
        );
        self.inner.shutdown().await;
    }

    /// How many restarts have happened inside the current window.
    #[must_use]
    pub fn restart_count(&self) -> usize {
        lock(&self.inner.restarts).len()
    }

    /// The largest handle count the child has reported.
    #[must_use]
    pub fn handle_peak(&self) -> u64 {
        self.inner.handle_peak.load(Ordering::SeqCst)
    }

    /// The child's resident set in bytes, when the platform can report it.
    ///
    /// Linux only: read from `/proc/<pid>/status`, which is in kB and therefore
    /// exact regardless of page size. Elsewhere the runtime's own heap flag is the
    /// only bound, which is stated rather than hidden.
    pub async fn resident_bytes(&self) -> Option<u64> {
        let pid = self
            .inner
            .session
            .lock()
            .await
            .as_ref()
            .and_then(|s| s.pid)?;
        resident_bytes(pid)
    }

    /// Decode a handle out of a descriptor field.
    #[must_use]
    pub fn handle(&self, value: &Value) -> Option<JsHandle> {
        JsHandle::from_value(value, self.generation())
    }

    /// Mint a handle for a callback id `init` reported.
    #[must_use]
    pub fn callback_handle(&self, id: u64, arity: u32) -> JsHandle {
        JsHandle {
            id,
            arity,
            generation: self.generation(),
        }
    }

    /// Invoke a retained function inside the child and await its result.
    ///
    /// # Errors
    /// Returns [`JsHostError`] when the handle predates a restart, the deadline
    /// passes, the child is gone, or the function itself threw.
    pub async fn call(
        &self,
        handle: &JsHandle,
        arguments: Vec<Value>,
    ) -> Result<Value, JsHostError> {
        let inner = Arc::clone(&self.inner);
        let executor = inner.executor.clone();
        let id = handle.id;
        let arity = handle.arity;
        executor
            .spawn(async move { inner.call_stable(id, arity, arguments).await })
            .await
            .map_err(|error| JsHostError::Disabled {
                plugin: self.inner.plugin.clone(),
                detail: format!("the host executor stopped during a callback: {error}"),
            })?
    }

    /// Invoke a retained function and return both its value and mutated arguments.
    ///
    /// JavaScript auth loaders mutate the provider object in place. Returning only
    /// the callback value would silently discard that half of their contract.
    pub async fn call_mutating(
        &self,
        handle: &JsHandle,
        arguments: Vec<Value>,
    ) -> Result<(Value, Vec<Value>), JsHostError> {
        let inner = Arc::clone(&self.inner);
        let executor = inner.executor.clone();
        let id = handle.id;
        let arity = handle.arity;
        let frame = executor
            .spawn(async move { inner.call_stable_frame(id, arity, arguments).await })
            .await
            .map_err(|error| JsHostError::Disabled {
                plugin: self.inner.plugin.clone(),
                detail: format!("the host executor stopped during a callback: {error}"),
            })??;
        let value = frame.get("value").cloned().unwrap_or(Value::Null);
        let arguments = frame
            .get("args")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok((value, arguments))
    }

    pub(crate) fn constant_function(&self, value: Value) -> JsHostFunction {
        let id = self.inner.next_host_function.fetch_add(1, Ordering::SeqCst);
        lock(&self.inner.host_functions).insert(id, value);
        JsHostFunction {
            host: self.clone(),
            id,
        }
    }

    /// Invoke a config-directory tool retained by this host.
    ///
    /// The execution context stays resident in Rust. JavaScript receives the public
    /// coordinates and a context id; `context.ask(...)` round-trips through that id
    /// so permission decisions are never reimplemented in the compat process.
    pub async fn call_tool(
        &self,
        tool: &str,
        index: usize,
        arguments: Value,
        context: ToolContext,
        directory: &std::path::Path,
        worktree: &std::path::Path,
    ) -> Result<Value, JsHostError> {
        let inner = Arc::clone(&self.inner);
        let executor = inner.executor.clone();
        let tool = tool.to_owned();
        let directory = directory.to_path_buf();
        let worktree = worktree.to_path_buf();
        executor
            .spawn(async move {
                if !inner.enabled.load(Ordering::SeqCst) {
                    if inner.closing.load(Ordering::SeqCst) {
                        return Err(JsHostError::Disabled {
                            plugin: inner.plugin.clone(),
                            detail: "the host is permanently disabled".to_owned(),
                        });
                    }
                    inner.restart().await?;
                }
                let handle_id = lock(&inner.report)
                    .as_ref()
                    .and_then(|report| report.tools.get(index))
                    .and_then(|descriptor| descriptor.get("execute"))
                    .and_then(|execute| {
                        JsHandle::from_value(execute, inner.generation.load(Ordering::SeqCst))
                    })
                    .map(|handle| handle.id)
                    .ok_or_else(|| JsHostError::Protocol {
                        plugin: inner.plugin.clone(),
                        detail: format!("config tool `{tool}` has no executable handle"),
                    })?;
                let context_id = inner.next_tool_context.fetch_add(1, Ordering::SeqCst);
                let context_value = json!({
                    "sessionID": context.session_id,
                    "messageID": context.message_id,
                    "callID": context.call_id,
                    "agent": context.agent,
                    "depth": context.depth,
                    "directory": directory,
                    "worktree": worktree,
                    "aborted": context.is_interrupted(),
                });
                lock(&inner.tool_contexts).insert(context_id, (tool, context));
                let result = inner
                    .request(
                        "tool.call",
                        json!({
                            "handle": handle_id,
                            "args": arguments,
                            "contextID": context_id,
                            "context": context_value,
                        }),
                    )
                    .await;
                lock(&inner.tool_contexts).remove(&context_id);
                result
            })
            .await
            .map_err(|error| JsHostError::Disabled {
                plugin: self.inner.plugin.clone(),
                detail: format!("the host executor stopped during a tool call: {error}"),
            })?
    }

    /// Invoke a retained function while this plugin owns the terminal.
    pub async fn call_with_terminal(
        &self,
        handle: &JsHandle,
        arguments: Vec<Value>,
        purpose: &str,
    ) -> Result<Value, JsHostError> {
        let grant = self.inner.grant_terminal(purpose).await;
        if grant.get("granted").and_then(Value::as_bool) != Some(true) {
            return Err(JsHostError::Disabled {
                plugin: self.inner.plugin.clone(),
                detail: grant
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("the terminal lease was refused")
                    .to_owned(),
            });
        }
        let managed = grant.get("managed").and_then(Value::as_bool) == Some(true);
        let result = self.call(handle, arguments).await;
        if managed {
            self.inner.release_terminal();
        }
        result
    }

    /// Invoke a retained function from synchronous code.
    ///
    /// This is what `crate::AuthTextValidator` needs: the oracle's `validate` is
    /// synchronous. It blocks the calling thread, never the transport — see the
    /// module note on why the host owns a runtime.
    ///
    /// # Errors
    /// Returns [`JsHostError`] for the same reasons as [`Self::call`], plus a
    /// deadline for the blocking wait itself.
    pub fn call_blocking(
        &self,
        handle: &JsHandle,
        arguments: Vec<Value>,
    ) -> Result<Value, JsHostError> {
        let inner = Arc::clone(&self.inner);
        let executor = inner.executor.clone();
        let timeout = inner.limits.hook_timeout;
        let plugin = inner.plugin.clone();
        let id = handle.id;
        let arity = handle.arity;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        executor.spawn(async move {
            let outcome = inner.call_stable(id, arity, arguments).await;
            let _sent = sender.send(outcome);
        });
        // A margin past the request deadline, so the transport's own timeout wins
        // and reports the precise cause; this bound only covers a lost executor.
        receiver
            .recv_timeout(timeout + Duration::from_secs(2))
            .unwrap_or(Err(JsHostError::Timeout {
                plugin,
                method: "call".to_owned(),
                timeout,
            }))
    }

    /// Invoke one of a hook's callbacks by name and index.
    ///
    /// # Errors
    /// Returns [`JsHostError`] when the hook has no such callback, or for any
    /// reason [`Self::call`] would.
    pub async fn call_hook(
        &self,
        hook: &str,
        index: usize,
        arguments: Vec<Value>,
    ) -> Result<Value, JsHostError> {
        let inner = Arc::clone(&self.inner);
        let executor = inner.executor.clone();
        let hook = hook.to_owned();
        executor
            .spawn(async move {
                if !inner.enabled.load(Ordering::SeqCst) {
                    if inner.closing.load(Ordering::SeqCst) {
                        return Err(JsHostError::Disabled {
                            plugin: inner.plugin.clone(),
                            detail: "the host is permanently disabled".to_owned(),
                        });
                    }
                    inner.restart().await?;
                }
                let id = {
                    let report = lock(&inner.report);
                    let report = report.as_ref().ok_or_else(|| JsHostError::Disabled {
                        plugin: inner.plugin.clone(),
                        detail: "the host has not completed init".to_owned(),
                    })?;
                    report
                        .callbacks
                        .get(&hook)
                        .and_then(|ids| ids.get(index))
                        .copied()
                        .ok_or_else(|| JsHostError::Protocol {
                            plugin: inner.plugin.clone(),
                            detail: format!(
                                "plugin registered no callback {index} for hook `{hook}`"
                            ),
                        })?
                };
                let handle = JsHandle {
                    id,
                    arity: 0,
                    generation: inner.generation.load(Ordering::SeqCst),
                };
                inner.call_frame(&handle, arguments).await
            })
            .await
            .map_err(|error| JsHostError::Disabled {
                plugin: self.inner.plugin.clone(),
                detail: format!("the host executor stopped during a hook: {error}"),
            })?
    }

    /// Ask the child for its own accounting, and fold it into the peak.
    ///
    /// # Errors
    /// Returns [`JsHostError`] when the child cannot answer.
    pub async fn stats(&self) -> Result<Value, JsHostError> {
        let value = self.inner.request("stats", json!({})).await?;
        if let Some(handles) = value.get("handles").and_then(Value::as_u64) {
            self.inner.handle_peak.fetch_max(handles, Ordering::SeqCst);
        }
        Ok(value)
    }

    /// Enforce the memory ceiling and the handle ceiling now.
    ///
    /// Returns the breach that was acted on, if any. The child is restarted rather
    /// than merely reported: a ceiling nobody enforces is documentation.
    pub async fn enforce_limits(&self) -> Option<LimitBreach> {
        self.inner.enforce_limits().await
    }

    /// Stop the child and release anything it held.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

pub(crate) struct JsHostFunction {
    host: JsHost,
    id: u64,
}

impl JsHostFunction {
    pub(crate) fn argument(&self) -> Value {
        json!({ "$hostFn": self.id })
    }
}

impl Drop for JsHostFunction {
    fn drop(&mut self) {
        lock(&self.host.inner.host_functions).remove(&self.id);
    }
}

/// A bound the child exceeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitBreach {
    /// Resident set past [`JsHostLimits::memory_ceiling`].
    Memory {
        /// Observed resident bytes.
        observed: u64,
        /// The configured ceiling.
        ceiling: u64,
    },
    /// Retained callables past [`JsHostLimits::max_handles`].
    Handles {
        /// Observed retained handles.
        observed: u64,
        /// The configured ceiling.
        ceiling: usize,
    },
}

impl std::fmt::Display for LimitBreach {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory { observed, ceiling } => write!(
                formatter,
                "resident set {observed} bytes exceeded the {ceiling}-byte plugin host ceiling"
            ),
            Self::Handles { observed, ceiling } => write!(
                formatter,
                "{observed} retained callables exceeded the {ceiling}-handle ceiling"
            ),
        }
    }
}

impl HostInner {
    async fn enforce_limits(self: &Arc<Self>) -> Option<LimitBreach> {
        let breach = self.detect_breach().await?;
        self.record(
            PluginDiagnosticKind::Crashed,
            None,
            breach.to_string(),
            false,
        );
        if self.restart().await.is_err() {
            self.enabled.store(false, Ordering::SeqCst);
        }
        Some(breach)
    }

    async fn call_stable(
        self: &Arc<Self>,
        id: u64,
        arity: u32,
        arguments: Vec<Value>,
    ) -> Result<Value, JsHostError> {
        let frame = self.call_stable_frame(id, arity, arguments).await?;
        Ok(frame.get("value").cloned().unwrap_or(Value::Null))
    }

    async fn call_stable_frame(
        self: &Arc<Self>,
        id: u64,
        arity: u32,
        arguments: Vec<Value>,
    ) -> Result<Value, JsHostError> {
        if !self.enabled.load(Ordering::SeqCst) {
            if self.closing.load(Ordering::SeqCst) {
                return Err(JsHostError::Disabled {
                    plugin: self.plugin.clone(),
                    detail: "the host is permanently disabled".to_owned(),
                });
            }
            self.restart().await?;
        }
        let handle = JsHandle {
            id,
            arity,
            generation: self.generation.load(Ordering::SeqCst),
        };
        self.call_frame(&handle, arguments).await
    }

    async fn boot(self: &Arc<Self>) -> Result<(), JsHostError> {
        let port = self
            .listener
            .local_addr()
            .map(|address| address.port())
            .map_err(|error| JsHostError::Spawn {
                plugin: self.plugin.clone(),
                detail: format!("the protocol listener has no address: {error}"),
            })?;

        let mut arguments = self
            .boot
            .runtime
            .kind()
            .memory_flags(self.limits.memory_ceiling);
        arguments.push(self.boot.shim.as_os_str().to_os_string());
        let (program, arguments) = oc_process::guarded_argv(self.boot.runtime.program(), arguments);
        let mut command = Command::new(program);
        command
            .args(arguments)
            // fd 0 and 1 stay inherited so a `readline` prompt reaches the user;
            // the shim rebinds `console` to stderr so protocol-adjacent chatter
            // cannot land on a terminal the TUI is about to redraw.
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("OC_JS_HOST_PORT", port.to_string())
            .env("OC_JS_HOST_TOKEN", &self.token)
            .env("OPENCODE_PLUGIN_NAME", &self.plugin)
            .env("OPENCODE_PLUGIN_PROTOCOL_VERSION", JS_PROTOCOL_VERSION);
        if let Some(reserved) = self.boot.input.loopback_port {
            command.env("OPENCODE_PLUGIN_LOOPBACK_PORT", reserved.to_string());
        }
        if let Some(parent) = self.boot.entry.parent() {
            command.current_dir(parent);
        }

        let mut child = command.spawn().map_err(|error| JsHostError::Spawn {
            plugin: self.plugin.clone(),
            detail: format!("could not spawn `{}`: {error}", self.boot.runtime),
        })?;
        let pid = child.id();
        let stderr = child.stderr.take().map(|stderr| {
            let plugin = self.plugin.clone();
            self.executor.spawn(drain_stderr(plugin, stderr))
        });

        let stream = match tokio::time::timeout(CONNECT_TIMEOUT, self.listener.accept()).await {
            Ok(Ok((stream, _))) => stream,
            Ok(Err(error)) => {
                let _killed = child.kill().await;
                tracing::debug!(
                    plugin = %self.plugin,
                    %error,
                    "accepting the plugin protocol connection failed"
                );
                return Err(JsHostError::Connect {
                    plugin: self.plugin.clone(),
                    timeout: CONNECT_TIMEOUT,
                });
            }
            Err(_) => {
                let _killed = child.kill().await;
                return Err(JsHostError::Connect {
                    plugin: self.plugin.clone(),
                    timeout: CONNECT_TIMEOUT,
                });
            }
        };
        let _ = stream.set_nodelay(true);
        let (read, writer) = stream.into_split();

        let (hello_sender, hello_receiver) = oneshot::channel();
        let reader = self
            .executor
            .spawn(read_loop(Arc::downgrade(self), read, Some(hello_sender)));

        *self.session.lock().await = Some(Session {
            writer,
            child,
            pid,
            reader,
            stderr,
        });

        match tokio::time::timeout(CONNECT_TIMEOUT, hello_receiver).await {
            Ok(Ok(token)) if token == self.token => {}
            Ok(Ok(token)) => {
                self.shutdown().await;
                return Err(JsHostError::Handshake {
                    plugin: self.plugin.clone(),
                    detail: format!(
                        "the connecting process presented an unexpected token \
                         ({} bytes); refusing it",
                        token.len()
                    ),
                });
            }
            Ok(Err(_)) | Err(_) => {
                self.shutdown().await;
                return Err(JsHostError::Handshake {
                    plugin: self.plugin.clone(),
                    detail: "the child connected but never sent `hello`".to_owned(),
                });
            }
        }

        let report = self.initialize().await?;
        *lock(&self.report) = Some(Arc::new(report));
        Ok(())
    }

    async fn initialize(self: &Arc<Self>) -> Result<JsInitReport, JsHostError> {
        let input = &self.boot.input;
        let params = json!({
            "entry": self.boot.entry,
            "spec": self.boot.spec,
            "kind": self.boot.kind,
            "options": input.options,
            "project": input.project,
            "directory": input.directory,
            "worktree": input.worktree,
            "serverUrl": input.server_url,
            "sdkModule": input.sdk_module,
        });
        let value = self
            .request_with_timeout("init", params, self.limits.init_timeout)
            .await?;
        let mut callbacks = HashMap::new();
        if let Some(map) = value.get("callbacks").and_then(Value::as_object) {
            for (hook, ids) in map {
                let ids = ids
                    .as_array()
                    .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default();
                callbacks.insert(hook.clone(), ids);
            }
        }
        Ok(JsInitReport {
            id: value.get("id").and_then(Value::as_str).map(str::to_owned),
            exports: string_list(value.get("exports")),
            sdk: value.get("sdk").and_then(Value::as_str).map(str::to_owned),
            runtime: value
                .get("runtime")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            hooks: string_list(value.get("hooks")),
            auth: value_list(value.get("auth")),
            provider: value_list(value.get("provider")),
            tools: value_list(value.get("tools")),
            workspace: value_list(value.get("workspace")),
            callbacks,
        })
    }

    async fn call_frame(
        &self,
        handle: &JsHandle,
        arguments: Vec<Value>,
    ) -> Result<Value, JsHostError> {
        let current = self.generation.load(Ordering::SeqCst);
        if handle.generation != current {
            return Err(JsHostError::StaleHandle {
                plugin: self.plugin.clone(),
                handle: handle.id,
                handle_generation: handle.generation,
                current_generation: current,
            });
        }
        self.request("call", json!({ "handle": handle.id, "args": arguments }))
            .await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, JsHostError> {
        self.request_with_timeout(method, params, self.limits.hook_timeout)
            .await
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, JsHostError> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(JsHostError::Disabled {
                plugin: self.plugin.clone(),
                detail: "an earlier failure disabled this plugin".to_owned(),
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        lock(&self.waiters).insert(id, sender);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let result = async {
            self.write(&frame).await?;
            match tokio::time::timeout(timeout, receiver).await {
                Ok(Ok(Ok(value))) => Ok(value),
                Ok(Ok(Err(detail))) => Err(JsHostError::Remote {
                    plugin: self.plugin.clone(),
                    method: method.to_owned(),
                    detail,
                }),
                Ok(Err(_)) => Err(JsHostError::Disabled {
                    plugin: self.plugin.clone(),
                    detail: "the child closed while a request was in flight".to_owned(),
                }),
                Err(_) => Err(JsHostError::Timeout {
                    plugin: self.plugin.clone(),
                    method: method.to_owned(),
                    timeout,
                }),
            }
        }
        .await;
        lock(&self.waiters).remove(&id);
        if let Err(error) = &result {
            let fatal = matches!(
                error,
                JsHostError::Timeout { .. } | JsHostError::Disabled { .. }
            );
            if fatal {
                self.record(
                    error.kind(),
                    Some(method.to_owned()),
                    error.to_string(),
                    true,
                );
            }
        }
        result
    }

    async fn write(&self, frame: &Value) -> Result<(), JsHostError> {
        let mut bytes = serde_json::to_vec(frame).map_err(|error| JsHostError::Protocol {
            plugin: self.plugin.clone(),
            detail: format!("could not encode a frame: {error}"),
        })?;
        bytes.push(b'\n');
        let mut session = self.session.lock().await;
        let session = session.as_mut().ok_or_else(|| JsHostError::Disabled {
            plugin: self.plugin.clone(),
            detail: "the child is not running".to_owned(),
        })?;
        session
            .writer
            .write_all(&bytes)
            .await
            .map_err(|error| JsHostError::Protocol {
                plugin: self.plugin.clone(),
                detail: format!("could not write a frame: {error}"),
            })?;
        session
            .writer
            .flush()
            .await
            .map_err(|error| JsHostError::Protocol {
                plugin: self.plugin.clone(),
                detail: format!("could not flush a frame: {error}"),
            })
    }

    async fn detect_breach(&self) -> Option<LimitBreach> {
        if let Ok(stats) = self.request("stats", json!({})).await
            && let Some(handles) = stats.get("handles").and_then(Value::as_u64)
        {
            self.handle_peak.fetch_max(handles, Ordering::SeqCst);
            if handles as usize > self.limits.max_handles {
                return Some(LimitBreach::Handles {
                    observed: handles,
                    ceiling: self.limits.max_handles,
                });
            }
        }
        let pid = self.session.lock().await.as_ref().and_then(|s| s.pid)?;
        let observed = resident_bytes(pid)?;
        (observed > self.limits.memory_ceiling).then_some(LimitBreach::Memory {
            observed,
            ceiling: self.limits.memory_ceiling,
        })
    }

    async fn restart(self: &Arc<Self>) -> Result<(), JsHostError> {
        let now = Instant::now();
        {
            let mut restarts = lock(&self.restarts);
            while restarts
                .front()
                .is_some_and(|at| now.duration_since(*at) > self.limits.restart_window)
            {
                restarts.pop_front();
            }
            if restarts.len() >= self.limits.max_restarts as usize {
                let detail = format!(
                    "restarted {} times within {} s; not restarting again",
                    restarts.len(),
                    self.limits.restart_window.as_secs()
                );
                drop(restarts);
                self.record(PluginDiagnosticKind::Crashed, None, detail.clone(), true);
                return Err(JsHostError::Disabled {
                    plugin: self.plugin.clone(),
                    detail,
                });
            }
            restarts.push_back(now);
        }
        self.stop_session().await;
        // Every handle the old process minted is now meaningless.
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.enabled.store(true, Ordering::SeqCst);
        self.boot().await
    }

    async fn stop_session(&self) {
        fail_waiters(&self.waiters, "the child was stopped");
        let session = self.session.lock().await.take();
        if let Some(mut session) = session {
            let _closed = session.writer.shutdown().await;
            let _killed = session.child.kill().await;
            session.reader.abort();
            if let Some(stderr) = session.stderr {
                stderr.abort();
            }
        }
        if let Some(guard) = lock(&self.lease).take() {
            guard.release();
        }
    }

    async fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        if !self.enabled.swap(false, Ordering::SeqCst) {
            self.stop_session().await;
            return;
        }
        // Best effort: a child that answers gets to exit cleanly, and one that does
        // not is killed. Either way the lease is returned.
        let _asked = tokio::time::timeout(
            Duration::from_millis(500),
            self.request_with_timeout("shutdown", json!({}), Duration::from_millis(400)),
        )
        .await;
        self.stop_session().await;
    }

    fn record(
        &self,
        kind: PluginDiagnosticKind,
        hook: Option<String>,
        message: String,
        disable: bool,
    ) {
        tracing::warn!(plugin = %self.plugin, ?hook, %message, "javascript plugin host failure");
        lock(&self.diagnostics).push(PluginDiagnostic {
            plugin: self.plugin.clone(),
            hook,
            kind,
            message,
        });
        if disable {
            self.enabled.store(false, Ordering::SeqCst);
        }
    }

    async fn grant_terminal(&self, purpose: &str) -> Value {
        let Some(terminal) = &self.terminal else {
            // No owner is registered, so nobody is holding the TTY and there is
            // nothing to step aside. Refusing here would break headless runs.
            return json!({ "granted": true, "managed": false });
        };
        if lock(&self.lease).is_some() {
            return json!({ "granted": true, "managed": false });
        }
        let actor = lock(&self.report)
            .as_ref()
            .and_then(|report| report.id.clone())
            .unwrap_or_else(|| self.plugin.clone());
        match terminal
            .acquire(LeaseReason::new(actor, purpose.to_owned()))
            .await
        {
            Ok(guard) => {
                *lock(&self.lease) = Some(guard);
                json!({ "granted": true, "managed": true })
            }
            Err(error) => json!({ "granted": false, "detail": error.to_string() }),
        }
    }

    fn release_terminal(&self) {
        if let Some(guard) = lock(&self.lease).take() {
            guard.release();
        }
    }
}

async fn supervision_loop(host: Weak<HostInner>) {
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let Some(host) = host.upgrade() else { return };
        if host.closing.load(Ordering::SeqCst) {
            return;
        }
        if host.enabled.load(Ordering::SeqCst) {
            let _breach = host.enforce_limits().await;
        }
    }
}

async fn read_loop(
    host: Weak<HostInner>,
    reader: tokio::net::tcp::OwnedReadHalf,
    mut hello: Option<oneshot::Sender<String>>,
) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                if let Some(host) = host.upgrade() {
                    host.record(
                        PluginDiagnosticKind::Crashed,
                        None,
                        format!("could not read the plugin socket: {error}"),
                        true,
                    );
                    fail_waiters(&host.waiters, "the plugin socket failed");
                }
                return;
            }
        }
        let frame: Value = match serde_json::from_str(line.trim_end_matches(['\r', '\n'])) {
            Ok(frame) => frame,
            Err(error) => {
                if let Some(host) = host.upgrade() {
                    host.record(
                        PluginDiagnosticKind::Protocol,
                        None,
                        format!("plugin emitted a malformed frame: {error}"),
                        true,
                    );
                    fail_waiters(&host.waiters, "the plugin emitted a malformed frame");
                }
                return;
            }
        };
        let Some(host) = host.upgrade() else { return };
        route(&host, frame, &mut hello);
    }
    if let Some(host) = host.upgrade() {
        fail_waiters(&host.waiters, "the plugin closed its socket");
    }
}

fn route(host: &Arc<HostInner>, frame: Value, hello: &mut Option<oneshot::Sender<String>>) {
    let method = frame.get("method").and_then(Value::as_str);
    let id = frame.get("id").and_then(Value::as_u64);

    if method.is_none()
        && let Some(id) = id
    {
        let outcome = if let Some(error) = frame.get("error") {
            Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the plugin reported an unspecified error")
                .to_owned())
        } else {
            Ok(frame.get("result").cloned().unwrap_or(Value::Null))
        };
        if let Some(sender) = lock(&host.waiters).remove(&id) {
            let _sent = sender.send(outcome);
        }
        return;
    }

    let Some(method) = method else { return };
    match method {
        "hello" => {
            if let Some(sender) = hello.take() {
                let token = frame
                    .get("params")
                    .and_then(|params| params.get("token"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let _sent = sender.send(token);
            }
        }
        "terminal.acquire" => {
            let purpose = frame
                .get("params")
                .and_then(|params| params.get("purpose"))
                .and_then(Value::as_str)
                .unwrap_or("interactive prompt")
                .to_owned();
            let host = Arc::clone(host);
            host.executor.clone().spawn(async move {
                let result = host.grant_terminal(&purpose).await;
                if let Some(id) = id {
                    let _written = host
                        .write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                        .await;
                }
            });
        }
        "terminal.release" => {
            host.release_terminal();
            if let Some(id) = id {
                let host = Arc::clone(host);
                host.executor.clone().spawn(async move {
                    let _written = host
                        .write(&json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null }))
                        .await;
                });
            }
        }
        "tool.ask" => {
            let context_id = frame
                .get("params")
                .and_then(|params| params.get("context"))
                .and_then(Value::as_u64);
            let input = frame
                .get("params")
                .and_then(|params| params.get("input"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let context = context_id
                .and_then(|context_id| lock(&host.tool_contexts).get(&context_id).cloned());
            let host = Arc::clone(host);
            host.executor.clone().spawn(async move {
                let result = match context {
                    Some((tool, context)) => {
                        let ask = permission_ask(&input);
                        context
                            .ask(&tool, ask)
                            .await
                            .map(|()| Value::Null)
                            .map_err(|error| error.to_string())
                    }
                    None => Err("tool execution context is no longer available".to_owned()),
                };
                if let Some(id) = id {
                    let frame = match result {
                        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                        Err(message) => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32000, "message": message },
                        }),
                    };
                    let _written = host.write(&frame).await;
                }
            });
        }
        "host.call" => {
            let target = frame
                .get("params")
                .and_then(|params| params.get("handle"))
                .and_then(Value::as_u64);
            let value = target.and_then(|target| lock(&host.host_functions).get(&target).cloned());
            let host = Arc::clone(host);
            host.executor.clone().spawn(async move {
                if let Some(id) = id {
                    let frame = match value {
                        Some(value) => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "value": value },
                        }),
                        None => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32000, "message": "host callback is unavailable" },
                        }),
                    };
                    let _written = host.write(&frame).await;
                }
            });
        }
        other => {
            tracing::debug!(plugin = %host.plugin, method = other, "ignored plugin-initiated frame");
            if let Some(id) = id {
                let host = Arc::clone(host);
                let message = format!("host does not implement `{other}`");
                host.executor.clone().spawn(async move {
                    let _written = host
                        .write(&json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": message },
                        }))
                        .await;
                });
            }
        }
    }
}

fn permission_ask(input: &Value) -> PermissionAsk {
    let strings = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    PermissionAsk {
        permission: input
            .get("permission")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        patterns: strings("patterns"),
        metadata: input
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        always: strings("always"),
    }
}

fn fail_waiters(waiters: &Waiters, detail: &str) {
    let senders: Vec<_> = lock(waiters).drain().map(|(_, sender)| sender).collect();
    for sender in senders {
        let _sent = sender.send(Err(detail.to_owned()));
    }
}

async fn drain_stderr(plugin: String, stderr: tokio::process::ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => tracing::debug!(
                %plugin,
                message = line.trim_end_matches(['\r', '\n']),
                "javascript plugin stderr"
            ),
        }
    }
}

async fn bind_loopback(executor: &Handle, plugin: &str) -> Result<TcpListener, JsHostError> {
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
    let plugin = plugin.to_owned();
    executor
        .spawn(async move { TcpListener::bind(address).await })
        .await
        .map_err(|error| JsHostError::Spawn {
            plugin: plugin.clone(),
            detail: format!("the host executor could not bind a listener: {error}"),
        })?
        .map_err(|error| JsHostError::Spawn {
            plugin,
            detail: format!("could not bind a loopback listener: {error}"),
        })
}

/// A per-host secret so only the child this host spawned may connect.
///
/// The listener is on loopback, which any local process can reach; without a token
/// another user's process could speak the protocol and drive a plugin's auth hook.
/// Derived from the OS random source through `TempDir`'s own name generator, which
/// is already a dependency and already used for exactly this.
fn mint_token() -> String {
    let mut token = String::with_capacity(48);
    for _ in 0..3 {
        let entropy = tempfile::Builder::new()
            .prefix("t")
            .tempdir()
            .ok()
            .and_then(|dir| {
                dir.path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("{:?}", Instant::now()));
        token.push_str(entropy.trim_start_matches('t'));
    }
    token.retain(|c| c.is_ascii_alphanumeric());
    token
}

/// The child's resident set, in bytes, from `/proc/<pid>/status`.
///
/// `VmRSS` is reported in kB, so this is exact regardless of the kernel's page
/// size — which `statm`'s page counts are not, on an aarch64 kernel with 16 KiB
/// pages.
#[cfg(target_os = "linux")]
fn resident_bytes(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kilobytes: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kilobytes * 1024);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes(_pid: u32) -> Option<u64> {
    // No procfs. The runtime's own heap flag is the only bound here, and saying so
    // is better than reporting a number this host did not measure.
    None
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn value_list(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(12)
        .collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
