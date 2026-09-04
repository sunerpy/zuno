//! Language-server process pool, lifecycle supervision, and request fan-out.

use crate::client::{Client, ClientError, Diagnostic, Position};
use crate::registry::{RegistryError, ServerRegistry, ServerSpec};
use futures::stream::{self, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use url::Url;
use zuno_process::ShutdownCeilings;

const CLIENT_READY_TIMEOUT: Duration = Duration::from_secs(50);
/// Ceiling on one blocking process-control call, which on Windows is a `taskkill /f /t` tree walk.
const PROCESS_CONTROL_LIMIT: Duration = Duration::from_secs(2);
/// Ceiling on collecting a language server after its tree was asked to stop.
const CHILD_REAP_LIMIT: Duration = Duration::from_secs(2);
/// Ceiling on the two reader tasks that hold a language server's stdout and stderr.
const PIPE_DRAIN_LIMIT: Duration = Duration::from_secs(1);
/// The ceilings every language-server settlement runs under.
///
/// Constants, and only constants: nothing a language server sends, nothing the model asks for,
/// and nothing in configuration reaches them, so a server cannot widen the bound on its own
/// shutdown. Every exit of the supervisor — the server exiting on its own, a requested restart,
/// manager shutdown, and a failed handshake — goes through [`settle`] with these.
const SHUTDOWN_CEILINGS: ShutdownCeilings = ShutdownCeilings {
    process_control: PROCESS_CONTROL_LIMIT,
    reap: CHILD_REAP_LIMIT,
    drain: PIPE_DRAIN_LIMIT,
};

/// Bounded restart behavior for a crashed language server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Delay before the first restart.
    pub initial_delay: Duration,
    /// Maximum delay between attempts.
    pub maximum_delay: Duration,
    /// Number of consecutive failures before supervision stops.
    pub maximum_restarts: u32,
    /// Runtime after which an earlier failure streak is forgotten.
    pub stable_after: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(100),
            maximum_delay: Duration::from_secs(2),
            maximum_restarts: 5,
            stable_after: Duration::from_secs(30),
        }
    }
}

impl RestartPolicy {
    fn delay(self, failure: u32) -> Duration {
        let exponent = failure.saturating_sub(1).min(31);
        self.initial_delay
            .saturating_mul(1_u32 << exponent)
            .min(self.maximum_delay)
    }
}

/// Observable state of one server/root pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    Starting,
    Connected,
    Degraded,
    Stopped,
}

/// Snapshot returned by [`Manager::status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub id: String,
    pub root: PathBuf,
    pub state: ServerState,
    pub process_id: Option<u32>,
    pub consecutive_failures: u32,
}

/// Lifecycle event emitted on the manager's bounded broadcast channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerEvent {
    Connected {
        id: String,
        root: PathBuf,
        process_id: u32,
    },
    Degraded {
        id: String,
        root: PathBuf,
        process_id: Option<u32>,
        attempt: u32,
    },
    Restarted {
        id: String,
        root: PathBuf,
        process_id: u32,
        attempt: u32,
    },
    Stopped {
        id: String,
        root: PathBuf,
    },
}

/// Process-pool failures surfaced to callers without terminating the agent.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("language server {server_id} could not be spawned with {command}")]
    Spawn {
        server_id: String,
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("language server {server_id} did not provide piped {stream}")]
    MissingPipe {
        server_id: String,
        stream: &'static str,
    },
    #[error("language server {server_id} failed during initialization")]
    Initialize {
        server_id: String,
        #[source]
        source: ClientError,
    },
    #[error("language server {server_id} is unavailable")]
    Unavailable { server_id: String },
    #[error("no language server is available for {path}")]
    NoServer { path: PathBuf },
    #[error("LSP request through {server_id} failed")]
    Request {
        server_id: String,
        #[source]
        source: ClientError,
    },
    #[error("path cannot be represented as a file URI: {path}")]
    InvalidFileUri { path: PathBuf },
}

#[derive(Debug)]
enum SupervisorCommand {
    Terminate,
    Shutdown,
}

#[derive(Debug)]
struct RuntimeState {
    status: ServerStatus,
    client: Option<Client>,
}

struct ManagedServer {
    spec: ServerSpec,
    root: PathBuf,
    state: Mutex<RuntimeState>,
    changed: watch::Sender<u64>,
    command: mpsc::Sender<SupervisorCommand>,
}

impl std::fmt::Debug for ManagedServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedServer")
            .field("spec", &self.spec)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

struct ManagerInner {
    workspace: PathBuf,
    registry: Arc<ServerRegistry>,
    policy: RestartPolicy,
    request_concurrency: NonZeroUsize,
    request_slots: Arc<Semaphore>,
    servers: RwLock<BTreeMap<String, Arc<ManagedServer>>>,
    events: broadcast::Sender<ManagerEvent>,
}

/// Lazily started language-server pool for one workspace.
#[derive(Clone)]
pub struct Manager {
    inner: Arc<ManagerInner>,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Manager")
            .field("workspace", &self.inner.workspace)
            .field("policy", &self.inner.policy)
            .finish_non_exhaustive()
    }
}

impl Manager {
    /// Create a lazy pool. No process starts until a file is touched.
    #[must_use]
    pub fn new(
        workspace: impl Into<PathBuf>,
        registry: Arc<ServerRegistry>,
        policy: RestartPolicy,
        request_concurrency: NonZeroUsize,
    ) -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(ManagerInner {
                workspace: workspace.into(),
                registry,
                policy,
                request_concurrency,
                request_slots: Arc::new(Semaphore::new(request_concurrency.get())),
                servers: RwLock::new(BTreeMap::new()),
                events,
            }),
        }
    }

    /// Global cap shared by server startup and request fan-out.
    #[must_use]
    pub fn request_concurrency(&self) -> NonZeroUsize {
        self.inner.request_concurrency
    }

    /// Receive subsequent lifecycle events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ManagerEvent> {
        self.inner.events.subscribe()
    }

    /// Return status in stable server-id/root order.
    pub async fn status(&self) -> Vec<ServerStatus> {
        let servers: Vec<_> = self.inner.servers.read().await.values().cloned().collect();
        let mut statuses = Vec::with_capacity(servers.len());
        for server in servers {
            statuses.push(server.state.lock().await.status.clone());
        }
        statuses
    }

    /// Whether at least one configured definition can handle `file`.
    #[must_use]
    pub fn has_server(&self, file: &Path) -> bool {
        self.inner
            .registry
            .matching(file, &self.inner.workspace)
            .next()
            .is_some()
    }

    /// Open or refresh `file` on every matching server.
    pub async fn touch_file(&self, file: &Path) -> Result<(), ManagerError> {
        let clients = self.clients_for(file).await?;
        let results = stream::iter(clients.into_iter().map(|client| {
            let manager = self.clone();
            let file = file.to_path_buf();
            async move {
                let server_id = client.server_id().to_owned();
                manager
                    .with_request_slot(client.open_or_change(&file))
                    .await
                    .map(|_| ())
                    .map_err(|source| ManagerError::Request { server_id, source })
            }
        }))
        .buffered(self.request_concurrency().get())
        .collect::<Vec<_>>()
        .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    /// Open a file and wait for fresh diagnostics from every matching server.
    pub async fn diagnostics(&self, file: &Path) -> Result<Vec<Diagnostic>, ManagerError> {
        let clients = self.clients_for(file).await?;
        let results = stream::iter(clients.into_iter().map(|client| {
            let manager = self.clone();
            let file = file.to_path_buf();
            async move {
                let server_id = client.server_id().to_owned();
                manager
                    .with_request_slot(async {
                        let (_version, epoch) =
                            client.open_or_change(&file).await.map_err(|source| {
                                ManagerError::Request {
                                    server_id: server_id.clone(),
                                    source,
                                }
                            })?;
                        match client.wait_for_diagnostics(&file, epoch).await {
                            Ok(items) => Ok(items),
                            Err(ClientError::Timeout { .. }) => {
                                Ok(client.diagnostics_for(&file).await)
                            }
                            Err(source) => Err(ManagerError::Request {
                                server_id: server_id.clone(),
                                source,
                            }),
                        }
                    })
                    .await
            }
        }))
        .buffered(self.request_concurrency().get())
        .collect::<Vec<_>>()
        .await;
        let mut diagnostics = Vec::new();
        for result in results {
            diagnostics.extend(result?);
        }
        deduplicate_diagnostics(&mut diagnostics);
        Ok(diagnostics)
    }

    /// Close `file` on all live matching clients.
    pub async fn close_file(&self, file: &Path) -> Result<(), ManagerError> {
        let clients = self.connected_clients_for(file).await;
        let results = stream::iter(clients.into_iter().map(|client| {
            let manager = self.clone();
            let file = file.to_path_buf();
            async move {
                let server_id = client.server_id().to_owned();
                manager
                    .with_request_slot(client.close_document(&file))
                    .await
                    .map_err(|source| ManagerError::Request { server_id, source })
            }
        }))
        .buffered(self.request_concurrency().get())
        .collect::<Vec<_>>()
        .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    /// Send a position-based request to every matching client and flatten results.
    pub async fn position_request(
        &self,
        file: &Path,
        position: Position,
        method: &str,
        extra: Value,
    ) -> Result<Vec<Value>, ManagerError> {
        let clients = self.clients_for(file).await?;
        let uri = file_uri(file)?;
        let results = stream::iter(clients.into_iter().map(|client| {
            let manager = self.clone();
            let uri = uri.clone();
            let extra = extra.clone();
            let method = method.to_owned();
            async move {
                let server_id = client.server_id().to_owned();
                let mut params = json!({
                    "textDocument": { "uri": uri },
                    "position": position
                });
                if let (Some(target), Some(source)) = (params.as_object_mut(), extra.as_object()) {
                    target.extend(source.clone());
                }
                manager
                    .with_request_slot(client.request(&method, params))
                    .await
                    .map_err(|source| ManagerError::Request { server_id, source })
            }
        }))
        .buffered(self.request_concurrency().get())
        .collect::<Vec<_>>()
        .await;
        let mut output = Vec::new();
        for result in results {
            flatten_result(result?, &mut output);
        }
        Ok(output)
    }

    /// Request symbols from every matching document client.
    pub async fn document_symbols(&self, file: &Path) -> Result<Vec<Value>, ManagerError> {
        self.document_request(file, "textDocument/documentSymbol")
            .await
    }

    /// Request workspace symbols from every already-started client.
    pub async fn workspace_symbols(&self, query: &str) -> Result<Vec<Value>, ManagerError> {
        let clients = self.connected_clients().await;
        let results = stream::iter(clients.into_iter().map(|client| {
            let manager = self.clone();
            let query = query.to_owned();
            async move {
                let server_id = client.server_id().to_owned();
                manager
                    .with_request_slot(
                        client.request("workspace/symbol", json!({ "query": query })),
                    )
                    .await
                    .map_err(|source| ManagerError::Request { server_id, source })
            }
        }))
        .buffered(self.request_concurrency().get())
        .collect::<Vec<_>>()
        .await;
        let mut output = Vec::new();
        for result in results {
            flatten_result(result?, &mut output);
        }
        output.retain(|symbol| {
            symbol
                .get("kind")
                .and_then(Value::as_u64)
                .is_some_and(|kind| matches!(kind, 5 | 6 | 10 | 11 | 12 | 13 | 14 | 23))
        });
        output.truncate(10);
        Ok(output)
    }

    /// Prepare a call-hierarchy item and query one direction on each client.
    pub async fn call_hierarchy(
        &self,
        file: &Path,
        position: Position,
        direction: &str,
    ) -> Result<Vec<Value>, ManagerError> {
        let clients = self.clients_for(file).await?;
        let uri = file_uri(file)?;
        let results = stream::iter(clients.into_iter().map(|client| {
            let manager = self.clone();
            let uri = uri.clone();
            let direction = direction.to_owned();
            async move {
                let server_id = client.server_id().to_owned();
                manager
                    .with_request_slot(async {
                        let prepared = client
                            .request(
                                "textDocument/prepareCallHierarchy",
                                json!({
                                    "textDocument": { "uri": uri },
                                    "position": position
                                }),
                            )
                            .await
                            .map_err(|source| ManagerError::Request {
                                server_id: server_id.clone(),
                                source,
                            })?;
                        let Some(item) =
                            prepared.as_array().and_then(|items| items.first()).cloned()
                        else {
                            return Ok(Value::Null);
                        };
                        client
                            .request(&direction, json!({ "item": item }))
                            .await
                            .map_err(|source| ManagerError::Request {
                                server_id: server_id.clone(),
                                source,
                            })
                    })
                    .await
            }
        }))
        .buffered(self.request_concurrency().get())
        .collect::<Vec<_>>()
        .await;
        let mut output = Vec::new();
        for result in results {
            flatten_result(result?, &mut output);
        }
        Ok(output)
    }

    /// Kill a live child to exercise or trigger normal restart supervision.
    pub async fn terminate(&self, server_id: &str, root: &Path) -> bool {
        let key = server_key(server_id, root);
        let server = self.inner.servers.read().await.get(&key).cloned();
        match server {
            Some(server) => server
                .command
                .send(SupervisorCommand::Terminate)
                .await
                .is_ok(),
            None => false,
        }
    }

    /// Stop every supervisor, kill and reap every child, and leave no live client.
    pub async fn shutdown(&self) {
        let servers: Vec<_> = self.inner.servers.read().await.values().cloned().collect();
        for server in &servers {
            let _result = server.command.send(SupervisorCommand::Shutdown).await;
        }
        for server in servers {
            let mut changed = server.changed.subscribe();
            let wait = async {
                loop {
                    if server.state.lock().await.status.state == ServerState::Stopped {
                        break;
                    }
                    if changed.changed().await.is_err() {
                        break;
                    }
                }
            };
            let _result = tokio::time::timeout(Duration::from_secs(5), wait).await;
        }
    }

    async fn document_request(
        &self,
        file: &Path,
        method: &str,
    ) -> Result<Vec<Value>, ManagerError> {
        let clients = self.clients_for(file).await?;
        let uri = file_uri(file)?;
        let results = stream::iter(clients.into_iter().map(|client| {
            let manager = self.clone();
            let uri = uri.clone();
            let method = method.to_owned();
            async move {
                let server_id = client.server_id().to_owned();
                manager
                    .with_request_slot(
                        client.request(&method, json!({ "textDocument": { "uri": uri } })),
                    )
                    .await
                    .map_err(|source| ManagerError::Request { server_id, source })
            }
        }))
        .buffered(self.request_concurrency().get())
        .collect::<Vec<_>>()
        .await;
        let mut output = Vec::new();
        for result in results {
            flatten_result(result?, &mut output);
        }
        Ok(output)
    }

    async fn clients_for(&self, file: &Path) -> Result<Vec<Client>, ManagerError> {
        let matches: Vec<_> = self
            .inner
            .registry
            .matching(file, &self.inner.workspace)
            .map(|(spec, root)| (spec.clone(), root))
            .collect();
        if matches.is_empty() {
            return Err(ManagerError::NoServer {
                path: file.to_path_buf(),
            });
        }
        let results = stream::iter(matches.into_iter().map(|(spec, root)| {
            let manager = self.clone();
            async move {
                manager
                    .with_request_slot(async {
                        let server = manager.ensure_supervisor(spec, root).await;
                        wait_for_client(&server).await
                    })
                    .await
            }
        }))
        .buffered(self.request_concurrency().get())
        .collect::<Vec<_>>()
        .await;
        let mut clients = Vec::with_capacity(results.len());
        for result in results {
            clients.push(result?);
        }
        Ok(clients)
    }

    async fn connected_clients_for(&self, file: &Path) -> Vec<Client> {
        let keys: Vec<_> = self
            .inner
            .registry
            .matching(file, &self.inner.workspace)
            .map(|(spec, root)| server_key(&spec.id, &root))
            .collect();
        let map = self.inner.servers.read().await;
        let servers: Vec<_> = keys
            .iter()
            .filter_map(|key| map.get(key).cloned())
            .collect();
        drop(map);
        let mut clients = Vec::new();
        for server in servers {
            if let Some(client) = server.state.lock().await.client.clone() {
                clients.push(client);
            }
        }
        clients
    }

    async fn connected_clients(&self) -> Vec<Client> {
        let servers: Vec<_> = self.inner.servers.read().await.values().cloned().collect();
        let mut clients = Vec::new();
        for server in servers {
            if let Some(client) = server.state.lock().await.client.clone() {
                clients.push(client);
            }
        }
        clients
    }

    async fn ensure_supervisor(&self, spec: ServerSpec, root: PathBuf) -> Arc<ManagedServer> {
        let key = server_key(&spec.id, &root);
        if let Some(server) = self.inner.servers.read().await.get(&key).cloned() {
            return server;
        }
        let mut servers = self.inner.servers.write().await;
        if let Some(server) = servers.get(&key).cloned() {
            return server;
        }
        let (changed, _) = watch::channel(0_u64);
        let (command, receiver) = mpsc::channel(4);
        let server = Arc::new(ManagedServer {
            spec,
            root,
            state: Mutex::new(RuntimeState {
                status: ServerStatus {
                    id: key_id(&key),
                    root: key_root(&key),
                    state: ServerState::Starting,
                    process_id: None,
                    consecutive_failures: 0,
                },
                client: None,
            }),
            changed,
            command,
        });
        servers.insert(key, Arc::clone(&server));
        drop(servers);
        tokio::spawn(supervise(
            Arc::clone(&self.inner),
            Arc::clone(&server),
            receiver,
        ));
        server
    }

    async fn with_request_slot<F, T>(&self, future: F) -> T
    where
        F: Future<Output = T>,
    {
        let _permit = self
            .inner
            .request_slots
            .acquire()
            .await
            .expect("LSP request semaphore is never closed");
        future.await
    }
}

async fn supervise(
    manager: Arc<ManagerInner>,
    server: Arc<ManagedServer>,
    mut commands: mpsc::Receiver<SupervisorCommand>,
) {
    let mut failures = 0_u32;
    let mut started_once = false;
    loop {
        let launched = launch(&manager, &server).await;
        let LaunchedServer {
            mut child,
            client,
            process_id,
            stderr_reader,
        } = match launched {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(server = %server.spec.id, %error, "language server launch failed");
                failures = failures.saturating_add(1);
                publish_degraded(&manager, &server, None, failures).await;
                if failures > manager.policy.maximum_restarts {
                    publish_stopped(&manager, &server).await;
                    return;
                }
                if wait_backoff_or_shutdown(&manager, &server, &mut commands, failures).await {
                    return;
                }
                continue;
            }
        };

        let started_at = Instant::now();
        publish_connected(
            &manager,
            &server,
            client.clone(),
            process_id,
            failures,
            started_once,
        )
        .await;
        started_once = true;

        let readers = |client: &Client| {
            stderr_reader
                .into_iter()
                .chain(client.take_reader())
                .collect::<Vec<_>>()
        };
        let should_restart = tokio::select! {
            status = child.wait() => {
                if let Err(error) = status {
                    tracing::warn!(server = %server.spec.id, %error, "language server wait failed");
                }
                // The child is already reaped; this settles the two readers, which a helper
                // the server leaked can otherwise hold open for the rest of the session.
                settle(&server.spec.id, &mut child, readers(&client)).await;
                true
            }
            command = commands.recv() => {
                match command {
                    Some(SupervisorCommand::Terminate) => {
                        settle(&server.spec.id, &mut child, readers(&client)).await;
                        true
                    }
                    Some(SupervisorCommand::Shutdown) | None => {
                        client.shutdown().await;
                        settle(&server.spec.id, &mut child, readers(&client)).await;
                        publish_stopped(&manager, &server).await;
                        return;
                    }
                }
            }
        };
        if !should_restart {
            publish_stopped(&manager, &server).await;
            return;
        }

        if started_at.elapsed() >= manager.policy.stable_after {
            failures = 0;
        }
        failures = failures.saturating_add(1);
        publish_degraded(&manager, &server, Some(process_id), failures).await;
        if failures > manager.policy.maximum_restarts {
            publish_stopped(&manager, &server).await;
            return;
        }
        if wait_backoff_or_shutdown(&manager, &server, &mut commands, failures).await {
            return;
        }
    }
}

/// One language server that completed its handshake, with every handle its owner must settle.
struct LaunchedServer {
    child: Child,
    client: Client,
    process_id: u32,
    /// The task draining the server's stderr, if the server provided that pipe.
    stderr_reader: Option<JoinHandle<()>>,
}

async fn launch(
    manager: &ManagerInner,
    server: &ManagedServer,
) -> Result<LaunchedServer, ManagerError> {
    let argv = manager.registry.launch_command(&server.spec).await?;
    let executable = argv.first().ok_or_else(|| RegistryError::EmptyCommand {
        server_id: server.spec.id.clone(),
    })?;
    let (program, arguments) = zuno_process::guarded_argv(executable, &argv[1..]);
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(&server.root)
        .envs(&server.spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|source| ManagerError::Spawn {
        server_id: server.spec.id.clone(),
        command: argv.join(" "),
        source,
    })?;
    let process_id = child.id().unwrap_or(0);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ManagerError::MissingPipe {
            server_id: server.spec.id.clone(),
            stream: "stdout",
        })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ManagerError::MissingPipe {
            server_id: server.spec.id.clone(),
            stream: "stdin",
        })?;
    let stderr_reader = child.stderr.take().map(|stderr| {
        let server_id = server.spec.id.clone();
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            if stderr
                .take(64 * 1024)
                .read_to_end(&mut buffer)
                .await
                .is_ok()
                && !buffer.is_empty()
            {
                tracing::debug!(server = %server_id, bytes = buffer.len(), "language server stderr");
            }
        })
    });
    match Client::connect(
        server.spec.id.clone(),
        server.root.clone(),
        Some(std::process::id()),
        stdout,
        stdin,
        server.spec.initialization.clone(),
    )
    .await
    {
        Ok(client) => Ok(LaunchedServer {
            child,
            client,
            process_id,
            stderr_reader,
        }),
        Err(source) => {
            // A failed handshake already aborted the client's own reader; the stderr reader is
            // this function's to settle.
            settle(
                &server.spec.id,
                &mut child,
                stderr_reader.into_iter().collect(),
            )
            .await;
            Err(ManagerError::Initialize {
                server_id: server.spec.id.clone(),
                source,
            })
        }
    }
}

async fn wait_for_client(server: &ManagedServer) -> Result<Client, ManagerError> {
    let mut changed = server.changed.subscribe();
    let server_id = server.spec.id.clone();
    let wait = async {
        loop {
            let state = server.state.lock().await;
            if let Some(client) = state.client.clone() {
                return Ok(client);
            }
            if state.status.state == ServerState::Stopped {
                return Err(ManagerError::Unavailable {
                    server_id: server_id.clone(),
                });
            }
            drop(state);
            if changed.changed().await.is_err() {
                return Err(ManagerError::Unavailable {
                    server_id: server_id.clone(),
                });
            }
        }
    };
    tokio::time::timeout(CLIENT_READY_TIMEOUT, wait)
        .await
        .map_err(|_| ManagerError::Unavailable { server_id })?
}

async fn publish_connected(
    manager: &ManagerInner,
    server: &ManagedServer,
    client: Client,
    process_id: u32,
    failures: u32,
    restarted: bool,
) {
    let mut state = server.state.lock().await;
    let event = if restarted {
        ManagerEvent::Restarted {
            id: server.spec.id.clone(),
            root: server.root.clone(),
            process_id,
            attempt: failures,
        }
    } else {
        ManagerEvent::Connected {
            id: server.spec.id.clone(),
            root: server.root.clone(),
            process_id,
        }
    };
    let _result = manager.events.send(event);
    state.status.state = ServerState::Connected;
    state.status.process_id = Some(process_id);
    state.status.consecutive_failures = failures;
    state.client = Some(client);
    signal_change(server);
}

async fn publish_degraded(
    manager: &ManagerInner,
    server: &ManagedServer,
    process_id: Option<u32>,
    attempt: u32,
) {
    let mut state = server.state.lock().await;
    let _result = manager.events.send(ManagerEvent::Degraded {
        id: server.spec.id.clone(),
        root: server.root.clone(),
        process_id,
        attempt,
    });
    state.status.state = ServerState::Degraded;
    state.status.process_id = None;
    state.status.consecutive_failures = attempt;
    state.client = None;
    signal_change(server);
}

async fn publish_stopped(manager: &ManagerInner, server: &ManagedServer) {
    let mut state = server.state.lock().await;
    let _result = manager.events.send(ManagerEvent::Stopped {
        id: server.spec.id.clone(),
        root: server.root.clone(),
    });
    state.status.state = ServerState::Stopped;
    state.status.process_id = None;
    state.client = None;
    signal_change(server);
}

fn signal_change(server: &ManagedServer) {
    let next = server.changed.borrow().saturating_add(1);
    let _result = server.changed.send(next);
}

async fn wait_backoff_or_shutdown(
    manager: &ManagerInner,
    server: &ManagedServer,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    failure: u32,
) -> bool {
    tokio::select! {
        () = tokio::time::sleep(manager.policy.delay(failure)) => false,
        command = commands.recv() => {
            match command {
                Some(SupervisorCommand::Shutdown) | None => {
                    publish_stopped(manager, server).await;
                    true
                }
                Some(SupervisorCommand::Terminate) => false,
            }
        }
    }
}

/// Stops one language server's tree, collects it, and settles its readers, each under its
/// ceiling.
///
/// This is the only way a supervised child leaves. `zuno_process::shutdown_contained_child`
/// runs the process-control call off the runtime worker — on Windows it is a `taskkill /f /t`
/// tree walk that would otherwise freeze the current-thread session runtime — bounds the reap,
/// and aborts a reader that is still holding a pipe at the drain ceiling rather than dropping
/// it, which would leave the task alive for as long as whatever the server leaked keeps the
/// pipe open. An unsettled outcome is logged, never promoted to a clean stop.
async fn settle(server_id: &str, child: &mut Child, readers: Vec<JoinHandle<()>>) {
    let outcome = zuno_process::shutdown_contained_child(child, readers, SHUTDOWN_CEILINGS).await;
    if outcome.is_settled() {
        tracing::debug!(server = %server_id, %outcome, "language server settled");
    } else {
        tracing::warn!(server = %server_id, %outcome, "language server did not settle cleanly");
    }
}

fn server_key(id: &str, root: &Path) -> String {
    format!("{id}\0{}", root.to_string_lossy())
}

fn key_id(key: &str) -> String {
    key.split_once('\0')
        .map_or_else(|| key.to_owned(), |(id, _)| id.to_owned())
}

fn key_root(key: &str) -> PathBuf {
    key.split_once('\0')
        .map_or_else(PathBuf::new, |(_, root)| PathBuf::from(root))
}

fn file_uri(path: &Path) -> Result<String, ManagerError> {
    Url::from_file_path(path)
        .map(String::from)
        .map_err(|()| ManagerError::InvalidFileUri {
            path: path.to_path_buf(),
        })
}

fn flatten_result(result: Value, output: &mut Vec<Value>) {
    match result {
        Value::Array(items) => output.extend(items),
        Value::Null => {}
        item => output.push(item),
    }
}

fn deduplicate_diagnostics(diagnostics: &mut Vec<Diagnostic>) {
    let mut seen = std::collections::BTreeSet::new();
    diagnostics.retain(|diagnostic| {
        serde_json::to_string(&(
            diagnostic.code.as_ref(),
            diagnostic.severity,
            &diagnostic.message,
            diagnostic.source.as_deref(),
            diagnostic.range,
        ))
        .map_or(true, |key| seen.insert(key))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use zuno_catalog::lsp_config::ResolvedLsp;
    use zuno_config::schema::lsp::LspConfig;

    fn test_python() -> Option<PathBuf> {
        for candidate in ["python3", "python"] {
            let Ok(paths) = which::which_all(candidate) else {
                continue;
            };
            for path in paths {
                let usable = std::process::Command::new(&path)
                    .args(["-c", "import json, pathlib, sys, time"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if usable {
                    return Some(path);
                }
            }
        }
        None
    }

    fn write_server(path: &Path) {
        fs::write(
            path,
            r#"import json, sys
def read():
    size = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b'\r\n', b'\n'):
            break
        if line.lower().startswith(b'content-length:'):
            size = int(line.split(b':', 1)[1].strip())
    return json.loads(sys.stdin.buffer.read(size))
def send(value):
    body = json.dumps(value, separators=(',', ':')).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    sys.stdout.buffer.flush()
while True:
    message = read()
    if message is None:
        break
    if message.get('method') == 'initialize':
        send({'jsonrpc':'2.0','id':message['id'],'result':{'capabilities':{}}})
    elif 'id' in message:
        send({'jsonrpc':'2.0','id':message['id'],'result':None})
"#,
        )
        .expect("write test language server");
    }

    fn write_barrier_server(path: &Path) {
        fs::write(
            path,
            r#"import json, pathlib, sys, time
server_id = sys.argv[1]
barrier = pathlib.Path(sys.argv[2])
def read():
    size = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b'\r\n', b'\n'):
            break
        if line.lower().startswith(b'content-length:'):
            size = int(line.split(b':', 1)[1].strip())
    return json.loads(sys.stdin.buffer.read(size))
def send(value):
    body = json.dumps(value, separators=(',', ':')).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    sys.stdout.buffer.flush()
def wait_for(name):
    while not (barrier / name).exists():
        time.sleep(0.005)
while True:
    message = read()
    if message is None:
        break
    method = message.get('method')
    if method == 'initialize':
        (barrier / (server_id + '.initialize')).touch()
        wait_for('release.initialize')
        send({'jsonrpc':'2.0','id':message['id'],'result':{'capabilities':{}}})
    elif method == 'workspace/symbol':
        (barrier / (server_id + '.request')).touch()
        wait_for('release.request')
        send({'jsonrpc':'2.0','id':message['id'],'result':[{
            'name': server_id, 'kind': 12,
            'location': {'uri':'file:///fixture','range':{
                'start':{'line':0,'character':0},'end':{'line':0,'character':1}
            }}
        }]})
    elif 'id' in message:
        send({'jsonrpc':'2.0','id':message['id'],'result':None})
"#,
        )
        .expect("write barrier language server");
    }

    fn barrier_manager(
        temp: &tempfile::TempDir,
        concurrency: usize,
        python: &Path,
    ) -> (Manager, PathBuf, PathBuf) {
        let script = temp.path().join("barrier_server.py");
        let barrier = temp.path().join("barrier");
        let source = temp.path().join("file.mine");
        fs::create_dir_all(&barrier).expect("create barrier directory");
        fs::write(&source, "content\n").expect("write source file");
        write_barrier_server(&script);
        let config: LspConfig = serde_json::from_value(json!({
            "one": {
                "command": [
                    python.to_string_lossy(),
                    script.to_string_lossy(),
                    "one",
                    barrier.to_string_lossy()
                ],
                "extensions": [".mine"]
            },
            "two": {
                "command": [
                    python.to_string_lossy(),
                    script.to_string_lossy(),
                    "two",
                    barrier.to_string_lossy()
                ],
                "extensions": [".mine"]
            }
        }))
        .expect("custom LSP config");
        let registry = Arc::new(ServerRegistry::offline(&ResolvedLsp::resolve(Some(
            &config,
        ))));
        (
            Manager::new(
                temp.path(),
                registry,
                RestartPolicy::default(),
                NonZeroUsize::new(concurrency).expect("non-zero concurrency"),
            ),
            source,
            barrier,
        )
    }

    async fn wait_for_marker_count(barrier: &Path, suffix: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let observed = fs::read_dir(barrier)
                    .expect("read barrier directory")
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.ends_with(suffix))
                    })
                    .count();
                if observed >= count {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("only part of the `{suffix}` barrier was reached"));
    }

    fn marker_count(barrier: &Path, suffix: &str) -> usize {
        fs::read_dir(barrier)
            .expect("read barrier directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(suffix))
            })
            .count()
    }

    #[tokio::test]
    async fn different_servers_start_and_answer_requests_concurrently_in_stable_order() {
        let Some(python) = test_python() else {
            return;
        };
        let temp = tempfile::tempdir().expect("temporary workspace");
        let (manager, source, barrier) = barrier_manager(&temp, 2, &python);

        let starting = {
            let manager = manager.clone();
            let source = source.clone();
            tokio::spawn(async move { manager.touch_file(&source).await })
        };
        wait_for_marker_count(&barrier, ".initialize", 2).await;
        fs::write(barrier.join("release.initialize"), []).expect("release initialization");
        starting
            .await
            .expect("startup task")
            .expect("both servers start");

        let requesting = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.workspace_symbols("needle").await })
        };
        wait_for_marker_count(&barrier, ".request", 2).await;
        fs::write(barrier.join("release.request"), []).expect("release requests");
        let symbols = requesting
            .await
            .expect("request task")
            .expect("both requests complete");

        assert_eq!(
            symbols
                .iter()
                .filter_map(|symbol| symbol["name"].as_str())
                .collect::<Vec<_>>(),
            ["one", "two"],
            "concurrent fan-out must preserve registry order"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn concurrency_one_restores_serial_lsp_startup_and_requests() {
        let Some(python) = test_python() else {
            return;
        };
        let temp = tempfile::tempdir().expect("temporary workspace");
        let (manager, source, barrier) = barrier_manager(&temp, 1, &python);

        let starting = {
            let manager = manager.clone();
            let source = source.clone();
            tokio::spawn(async move { manager.touch_file(&source).await })
        };
        wait_for_marker_count(&barrier, ".initialize", 1).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            marker_count(&barrier, ".initialize"),
            1,
            "the second server started while the only slot was occupied"
        );
        fs::write(barrier.join("release.initialize"), []).expect("release initialization");
        starting
            .await
            .expect("startup task")
            .expect("serial startup completes");

        let requesting = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.workspace_symbols("needle").await })
        };
        wait_for_marker_count(&barrier, ".request", 1).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            marker_count(&barrier, ".request"),
            1,
            "the second request started while the only slot was occupied"
        );
        fs::write(barrier.join("release.request"), []).expect("release requests");
        let symbols = requesting
            .await
            .expect("request task")
            .expect("serial requests complete");
        assert_eq!(
            symbols
                .iter()
                .filter_map(|symbol| symbol["name"].as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn a_killed_server_publishes_degraded_before_restarting() {
        let Some(python) = test_python() else {
            return;
        };
        let temp = tempfile::tempdir().expect("temporary workspace");
        let script = temp.path().join("server.py");
        let source = temp.path().join("file.mine");
        write_server(&script);
        fs::write(&source, "content\n").expect("write source file");
        let config: LspConfig = serde_json::from_value(json!({
            "test": {
                "command": [python.to_string_lossy(), script.to_string_lossy(), "--stdio"],
                "extensions": [".mine"]
            }
        }))
        .expect("custom LSP config");
        let registry = Arc::new(ServerRegistry::offline(&ResolvedLsp::resolve(Some(
            &config,
        ))));
        let manager = Manager::new(
            temp.path(),
            registry,
            RestartPolicy {
                initial_delay: Duration::from_millis(10),
                maximum_delay: Duration::from_millis(20),
                maximum_restarts: 2,
                stable_after: Duration::from_secs(10),
            },
            NonZeroUsize::new(4).expect("non-zero"),
        );
        let mut events = manager.subscribe();
        manager
            .touch_file(&source)
            .await
            .expect("start test server");
        let first_pid = match events.recv().await.expect("connected event") {
            ManagerEvent::Connected { process_id, .. } => process_id,
            other => panic!("expected connected event, got {other:?}"),
        };
        assert!(manager.terminate("test", temp.path()).await);
        match events.recv().await.expect("degraded event") {
            ManagerEvent::Degraded {
                process_id: Some(process_id),
                attempt: 1,
                ..
            } => assert_eq!(process_id, first_pid),
            other => panic!("expected degraded event, got {other:?}"),
        }
        assert_eq!(manager.status().await[0].state, ServerState::Degraded);
        let second_pid = match events.recv().await.expect("restart event") {
            ManagerEvent::Restarted { process_id, .. } => process_id,
            other => panic!("expected restart event, got {other:?}"),
        };
        assert_ne!(first_pid, second_pid);
        assert_eq!(manager.status().await[0].state, ServerState::Connected);
        manager.shutdown().await;
        assert_eq!(manager.status().await[0].state, ServerState::Stopped);
    }

    /// Tasks alive on this test's runtime, where the supervisor spawns both pipe readers.
    ///
    /// A reader returns only at EOF, and EOF needs every writer to have closed the pipe. One
    /// whose handle was dropped instead of aborted is still alive after the supervisor has
    /// published `Stopped`, still holding the pipe read end. The runtime already counts it, so
    /// no production hook is needed to see it.
    ///
    /// Used only by the Unix wedged-server test; on Windows there is no `mkfifo` fixture
    /// yet, so the helper is gated with its caller instead of tripping `dead_code`.
    #[cfg(unix)]
    fn alive_tasks() -> usize {
        tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks()
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// A language server that answers `initialize`, detaches a helper into its own session
    /// that inherits the server's stdout and stderr, then stops reading and never exits.
    ///
    /// The helper is what keeps both pipes open after the server's own group is reaped: it
    /// is outside that group, so the group kill never reaches it, and a reader that is only
    /// dropped keeps waiting for an EOF that arrives when the helper dies and not before.
    #[cfg(unix)]
    fn write_wedged_server(path: &Path) {
        fs::write(
            path,
            r#"import json, os, signal, subprocess, sys, time
pid_file = sys.argv[1]
def read():
    size = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b'\r\n', b'\n'):
            break
        if line.lower().startswith(b'content-length:'):
            size = int(line.split(b':', 1)[1].strip())
    return json.loads(sys.stdin.buffer.read(size))
def send(value):
    body = json.dumps(value, separators=(',', ':')).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
    sys.stdout.buffer.flush()
while True:
    message = read()
    if message is None:
        break
    if message.get('method') == 'initialize':
        send({'jsonrpc':'2.0','id':message['id'],'result':{'capabilities':{}}})
        helper = subprocess.Popen(['sleep', '600'], stdin=subprocess.DEVNULL, start_new_session=True)
        with open(pid_file + '.tmp', 'w') as handle:
            handle.write(str(helper.pid))
        os.replace(pid_file + '.tmp', pid_file)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        while True:
            time.sleep(3600)
"#,
        )
        .expect("write wedged language server");
    }

    /// Cancelling a server that stopped reading and never exits must settle the whole
    /// launch: the child reaped and both pipe readers gone, within fixed ceilings.
    ///
    /// The fixture's escaped helper holds stdout and stderr open past the group kill, so a
    /// reader that was dropped rather than aborted stays alive for the helper's whole life
    /// and the runtime's task count never returns to where it started. An unbounded
    /// `child.wait()` would show up here as the shutdown never returning at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_a_wedged_server_leaves_no_reader_holding_its_pipes() {
        let Some(python) = test_python() else {
            return;
        };
        let temp = tempfile::tempdir().expect("temporary workspace");
        let script = temp.path().join("wedged_server.py");
        let source = temp.path().join("file.mine");
        let pid_path = temp.path().join("helper.pid");
        write_wedged_server(&script);
        fs::write(&source, "content\n").expect("write source file");
        let config: LspConfig = serde_json::from_value(json!({
            "wedged": {
                "command": [
                    python.to_string_lossy(),
                    script.to_string_lossy(),
                    pid_path.to_string_lossy()
                ],
                "extensions": [".mine"]
            }
        }))
        .expect("custom LSP config");
        let registry = Arc::new(ServerRegistry::offline(&ResolvedLsp::resolve(Some(
            &config,
        ))));
        let manager = Manager::new(
            temp.path(),
            registry,
            RestartPolicy::default(),
            NonZeroUsize::new(4).expect("non-zero"),
        );

        let baseline = alive_tasks();
        manager
            .touch_file(&source)
            .await
            .expect("the wedged server still completes initialization");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !pid_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the fixture records its helper's pid");
        let helper = fs::read_to_string(&pid_path)
            .expect("helper pid")
            .trim()
            .parse::<u32>()
            .expect("numeric helper pid");
        assert!(
            alive_tasks() > baseline,
            "the supervisor and its readers must be running before the cancellation"
        );

        let started = std::time::Instant::now();
        manager.shutdown().await;
        let settled = tokio::time::timeout(
            SHUTDOWN_CEILINGS.reap + SHUTDOWN_CEILINGS.drain + Duration::from_secs(5),
            async {
                while alive_tasks() != baseline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            },
        )
        .await;
        let elapsed = started.elapsed();
        let helper_alive = process_exists(helper);
        let _killed = std::process::Command::new("kill")
            .args(["-9", &helper.to_string()])
            .status();

        assert!(
            helper_alive,
            "the escaped helper must outlive the cancellation, otherwise both pipes would \
             reach EOF on their own and a leaked reader would exit without being observed"
        );
        assert_eq!(manager.status().await[0].state, ServerState::Stopped);
        assert!(
            settled.is_ok(),
            "{} task(s) above the baseline were still alive {elapsed:?} after shutdown: a \
             pipe reader was dropped instead of aborted, or the reap had no ceiling",
            alive_tasks().saturating_sub(baseline)
        );
    }
}
