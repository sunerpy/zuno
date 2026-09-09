//! Runtime lifecycle control for configured MCP servers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, watch};
use tokio::time::Instant;
use zuno_config::schema::mcp::McpServerConfig;

use crate::{
    Catalog, ConnectedServer, PromptDefinition, RemoteClient, RemoteConnect, ResourceContents,
    ResourceDefinition, ResourceTemplate, ServerStatus, StdioClient, ToolCallResult,
    ToolDefinition,
};
use zuno_error::McpError;

const EVENT_CAPACITY: usize = 64;

/// Format of a persisted MCP tool directory.
pub const MCP_TOOL_DIRECTORY_VERSION: u32 = 1;

/// Secret-free digest of every input that selects one server connection.
///
/// The digest includes the configured name, runtime workspace and the complete
/// server configuration. Headers and OAuth secrets affect the digest but are never
/// stored in the identity itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConnectionIdentity {
    /// Configured server name.
    pub server: String,
    /// URL-safe SHA-256 over the complete connection inputs.
    pub sha256: String,
}

impl McpConnectionIdentity {
    /// Computes the exact connection identity without retaining configuration secrets.
    #[must_use]
    pub fn from_config(
        server: impl Into<String>,
        workspace: impl AsRef<Path>,
        config: &McpServerConfig,
    ) -> Self {
        let server = server.into();
        let mut digest = Sha256::new();
        digest_field(&mut digest, b"zuno-mcp-connection-v1");
        digest_field(&mut digest, server.as_bytes());
        digest_path(&mut digest, workspace.as_ref());
        let encoded = serde_json::to_vec(config).expect("MCP configuration is serializable");
        digest_field(&mut digest, &encoded);
        let streamable_http_only = matches!(
            config,
            McpServerConfig::Remote(remote) if remote.streamable_http_only
        );
        digest_field(&mut digest, &[u8::from(streamable_http_only)]);
        Self {
            server,
            sha256: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.finalize()),
        }
    }
}

/// Frozen tool schemas discovered from one exact MCP connection identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDirectory {
    /// Cache format. Unknown formats are ignored and rediscovered eagerly.
    pub version: u32,
    /// Exact server connection that produced these schemas.
    pub identity: McpConnectionIdentity,
    /// Server-local tool definitions in discovery order.
    pub tools: Vec<ToolDefinition>,
}

impl McpToolDirectory {
    /// Builds the current cache format.
    #[must_use]
    pub fn new(identity: McpConnectionIdentity, tools: Vec<ToolDefinition>) -> Self {
        Self {
            version: MCP_TOOL_DIRECTORY_VERSION,
            identity,
            tools,
        }
    }
}

/// Bounds applied by the lifecycle layer around transport work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpLifecycleOptions {
    /// Maximum time for connection, handshake, and initial discovery.
    pub connect_timeout: Duration,
    /// Maximum time to wait for an established transport to close.
    pub close_timeout: Duration,
}

impl Default for McpLifecycleOptions {
    fn default() -> Self {
        Self {
            connect_timeout: crate::DEFAULT_REQUEST_TIMEOUT,
            close_timeout: crate::DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

/// Observable runtime state of one configured server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerState {
    /// No connection is present and none is desired.
    Disabled,
    /// A transport is connecting and completing initial discovery.
    Connecting,
    /// The handshake and initial discovery completed.
    Connected,
    /// An in-flight connection is being cancelled or a live one is closing.
    Disconnecting,
    /// Connection, discovery, or shutdown failed.
    Failed {
        /// Human-readable failure detail suitable for the MCP picker.
        error: String,
    },
    /// OAuth must be completed outside this control point before retrying.
    NeedsAuth,
    /// The authorization server requires a pre-registered client.
    NeedsClientRegistration {
        /// Registration failure detail.
        error: String,
    },
}

impl McpServerState {
    /// Stable catalog status, when this lifecycle state has one.
    #[must_use]
    pub fn catalog_status(&self) -> Option<ServerStatus> {
        match self {
            Self::Disabled => Some(ServerStatus::Disabled),
            Self::Connected => Some(ServerStatus::Connected),
            Self::Failed { error } => Some(ServerStatus::Failed {
                error: error.clone(),
            }),
            Self::NeedsAuth => Some(ServerStatus::NeedsAuth),
            Self::NeedsClientRegistration { error } => {
                Some(ServerStatus::NeedsClientRegistration {
                    error: error.clone(),
                })
            }
            Self::Connecting | Self::Disconnecting => None,
        }
    }
}

/// Current lifecycle facts for one server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSnapshot {
    /// Configured server name.
    pub server: String,
    /// Current runtime state.
    pub state: McpServerState,
    /// Latest requested target. This differs from `state` during transitions.
    pub desired_enabled: bool,
}

/// A bounded lifecycle notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerEvent {
    /// One server changed state or target.
    StateChanged {
        /// Complete replacement snapshot; lagged receivers should re-read all snapshots.
        snapshot: McpServerSnapshot,
    },
}

/// Errors rejected before a lifecycle operation can begin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpLifecycleError {
    /// The requested name was not registered with this controller.
    #[error("MCP server {server:?} is not configured")]
    UnknownServer {
        /// Unknown configured name.
        server: String,
    },
    /// A tool directory was produced by another connection identity.
    #[error("cached MCP tool directory for {server:?} does not match the configured connection")]
    ToolDirectoryIdentityMismatch {
        /// Configured server name.
        server: String,
    },
    /// A newer or corrupt tool-directory format cannot be trusted.
    #[error("cached MCP tool directory for {server:?} has unsupported version {version}")]
    ToolDirectoryVersion {
        /// Configured server name.
        server: String,
        /// Unrecognized cache format.
        version: u32,
    },
    /// A prepared runtime cannot adopt a connection into a busy or live slot.
    #[error("MCP server {server:?} is not idle for connection reuse")]
    ReuseSlotBusy {
        /// Configured server name.
        server: String,
    },
}

/// Transport-neutral result of a connection attempt.
pub enum McpConnectOutcome {
    /// A usable transport completed its MCP handshake.
    Connected(Arc<dyn McpConnection>),
    /// OAuth interaction is required before another attempt can connect.
    NeedsAuth,
    /// Dynamic registration is unavailable or was rejected.
    NeedsClientRegistration {
        /// Registration failure detail.
        error: String,
    },
}

/// One established transport owned by the lifecycle controller.
///
/// Implementations must make dropping the last handle cancellation-safe: a
/// connection future can be dropped when an enable operation is cancelled.
#[async_trait]
pub trait McpConnection: Send + Sync + 'static {
    /// Server interface installed into the merged catalog.
    fn server(&self) -> Arc<dyn ConnectedServer>;

    /// Stops transport tasks and any child process. Repeated calls must be harmless.
    async fn close(&self);
}

/// Fakeable transport selection and connection seam.
#[async_trait]
pub trait McpConnector: Send + Sync + 'static {
    /// Connects the named configured server.
    async fn connect(&self, server: &str) -> Result<McpConnectOutcome, String>;

    /// Optional per-server override for the lifecycle connection bound.
    fn connect_timeout(&self, _server: &str) -> Option<Duration> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationKind {
    Connect,
    Disconnect,
}

struct Operation {
    generation: u64,
    kind: OperationKind,
    cancel: watch::Sender<bool>,
}

struct ServerSlot {
    identity: Option<McpConnectionIdentity>,
    state: McpServerState,
    desired_enabled: bool,
    generation: u64,
    operation: Option<Operation>,
    connection: Option<Arc<dyn McpConnection>>,
    changed: Arc<Notify>,
}

impl ServerSlot {
    fn snapshot(&self, server: &str) -> McpServerSnapshot {
        McpServerSnapshot {
            server: server.to_owned(),
            state: self.state.clone(),
            desired_enabled: self.desired_enabled,
        }
    }
}

struct Inner {
    catalog: Catalog,
    connector: Arc<dyn McpConnector>,
    options: McpLifecycleOptions,
    servers: Mutex<BTreeMap<String, ServerSlot>>,
    events: broadcast::Sender<McpServerEvent>,
}

/// One live server that can move between two session snapshots without reconnecting.
///
/// The handle is process-local and never crosses sessions unless the caller uses the
/// same [`McpRuntimeManager`] instance. No global stdio pool exists.
#[derive(Clone)]
pub struct McpReusableServer {
    identity: McpConnectionIdentity,
    connection: Arc<dyn McpConnection>,
    server: Arc<dyn ConnectedServer>,
    tools: Vec<ToolDefinition>,
    prompts: Vec<PromptDefinition>,
}

impl std::fmt::Debug for McpReusableServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpReusableServer")
            .field("identity", &self.identity)
            .field("tools", &self.tools.len())
            .field("prompts", &self.prompts.len())
            .finish_non_exhaustive()
    }
}

impl McpReusableServer {
    /// Exact connection identity carried by this live transport.
    #[must_use]
    pub const fn identity(&self) -> &McpConnectionIdentity {
        &self.identity
    }

    /// Configured server name.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.identity.server
    }
}

/// Single runtime control point for enabling and disabling MCP servers.
///
/// Clones share state. Concurrent same-target requests join one operation;
/// disabling a connecting server cancels that connection future. Every
/// transport operation and shutdown wait is bounded.
#[derive(Clone)]
pub struct McpServerController {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for McpServerController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpServerController")
            .field("servers", &lock(&self.inner.servers).len())
            .finish()
    }
}

impl McpServerController {
    /// Builds a production controller that selects stdio or remote transport
    /// from each server's resolved configuration.
    #[must_use]
    pub fn from_config(
        catalog: Catalog,
        workspace: impl AsRef<Path>,
        configs: BTreeMap<String, McpServerConfig>,
        options: McpLifecycleOptions,
    ) -> Self {
        let workspace = workspace.as_ref().to_owned();
        let names: Vec<String> = configs.keys().cloned().collect();
        let identities = configs
            .iter()
            .map(|(server, config)| {
                (
                    server.clone(),
                    McpConnectionIdentity::from_config(server, &workspace, config),
                )
            })
            .collect();
        let connector = Arc::new(ConfiguredConnector { workspace, configs });
        Self::with_connector_and_identities(catalog, names, identities, connector, options)
    }

    /// Builds a controller around a fake or alternate connector.
    #[must_use]
    pub fn with_connector<I, S, C>(
        catalog: Catalog,
        servers: I,
        connector: Arc<C>,
        options: McpLifecycleOptions,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        C: McpConnector,
    {
        Self::with_connector_and_identities(catalog, servers, BTreeMap::new(), connector, options)
    }

    /// Builds a controller around an alternate connector and exact identities.
    ///
    /// This is the test and embedding seam for cached discovery and atomic runtime
    /// replacement. A server without an identity remains eager-only.
    #[must_use]
    pub fn with_connector_and_identities<I, S, C>(
        catalog: Catalog,
        servers: I,
        identities: BTreeMap<String, McpConnectionIdentity>,
        connector: Arc<C>,
        options: McpLifecycleOptions,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        C: McpConnector,
    {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let servers = servers
            .into_iter()
            .map(Into::into)
            .map(|server| {
                catalog.unavailable(server.clone(), ServerStatus::Disabled);
                let identity = identities.get(&server).cloned();
                (
                    server,
                    ServerSlot {
                        identity,
                        state: McpServerState::Disabled,
                        desired_enabled: false,
                        generation: 0,
                        operation: None,
                        connection: None,
                        changed: Arc::new(Notify::new()),
                    },
                )
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                catalog,
                connector,
                options,
                servers: Mutex::new(servers),
                events,
            }),
        }
    }

    /// Shared merged catalog updated by lifecycle transitions.
    #[must_use]
    pub fn catalog(&self) -> Catalog {
        self.inner.catalog.clone()
    }

    /// Receives lifecycle changes from this point forward.
    ///
    /// The channel retains at most 64 events. On `Lagged`, re-read
    /// [`Self::snapshots`] rather than replaying stale transitions.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<McpServerEvent> {
        self.inner.events.subscribe()
    }

    /// Stable name-ordered snapshot of every configured server.
    #[must_use]
    pub fn snapshots(&self) -> Vec<McpServerSnapshot> {
        lock(&self.inner.servers)
            .iter()
            .map(|(server, slot)| slot.snapshot(server))
            .collect()
    }

    /// Snapshot for one configured server.
    pub fn snapshot(&self, server: &str) -> Result<McpServerSnapshot, McpLifecycleError> {
        lock(&self.inner.servers)
            .get(server)
            .map(|slot| slot.snapshot(server))
            .ok_or_else(|| McpLifecycleError::UnknownServer {
                server: server.to_owned(),
            })
    }

    /// Stable configured names for this session runtime.
    #[must_use]
    pub fn server_names(&self) -> Vec<String> {
        lock(&self.inner.servers).keys().cloned().collect()
    }

    /// Exact connection identity for one configured server.
    pub fn connection_identity(
        &self,
        server: &str,
    ) -> Result<Option<McpConnectionIdentity>, McpLifecycleError> {
        lock(&self.inner.servers)
            .get(server)
            .map(|slot| slot.identity.clone())
            .ok_or_else(|| McpLifecycleError::UnknownServer {
                server: server.to_owned(),
            })
    }

    /// Installs a validated frozen tool directory without starting a transport.
    ///
    /// The returned schemas are served by a lazy proxy. Its first real tool call
    /// joins [`Self::enable`], so concurrent calls produce one connection attempt.
    pub fn install_cached_directory(
        &self,
        directory: McpToolDirectory,
    ) -> Result<McpServerSnapshot, McpLifecycleError> {
        let server = directory.identity.server.clone();
        if directory.version != MCP_TOOL_DIRECTORY_VERSION {
            return Err(McpLifecycleError::ToolDirectoryVersion {
                server,
                version: directory.version,
            });
        }
        let lazy: Arc<dyn ConnectedServer> = Arc::new(LazyConnectedServer {
            server: server.clone(),
            tools: directory.tools.clone(),
            controller: Arc::downgrade(&self.inner),
        });
        let snapshot = {
            let mut servers = lock(&self.inner.servers);
            let slot =
                servers
                    .get_mut(&server)
                    .ok_or_else(|| McpLifecycleError::UnknownServer {
                        server: server.clone(),
                    })?;
            if slot.identity.as_ref() != Some(&directory.identity) {
                return Err(McpLifecycleError::ToolDirectoryIdentityMismatch { server });
            }
            if slot.operation.is_some() || slot.connection.is_some() || slot.desired_enabled {
                return Err(McpLifecycleError::ReuseSlotBusy { server });
            }
            self.inner.catalog.cached(lazy, directory.tools);
            slot.snapshot(&server)
        };
        self.publish(snapshot.clone());
        Ok(snapshot)
    }

    /// Current valid tool directory, whether cached or discovered live.
    #[must_use]
    pub fn tool_directory(&self, server: &str) -> Option<McpToolDirectory> {
        let identity = self.connection_identity(server).ok().flatten()?;
        if let Some(tools) = self.inner.catalog.cached_tools(server) {
            return Some(McpToolDirectory::new(identity, tools));
        }
        self.inner
            .catalog
            .connected_snapshot(server)
            .map(|snapshot| McpToolDirectory::new(identity, snapshot.tools))
    }

    /// All valid server tool directories, in configured-name order.
    #[must_use]
    pub fn tool_directories(&self) -> BTreeMap<String, McpToolDirectory> {
        self.server_names()
            .into_iter()
            .filter_map(|server| self.tool_directory(&server).map(|cache| (server, cache)))
            .collect()
    }

    /// Exports a live connection for exact-identity reuse by a prepared runtime.
    #[must_use]
    pub fn reusable_server(&self, server: &str) -> Option<McpReusableServer> {
        let servers = lock(&self.inner.servers);
        let slot = servers.get(server)?;
        if slot.operation.is_some() || !matches!(slot.state, McpServerState::Connected) {
            return None;
        }
        let identity = slot.identity.clone()?;
        let connection = slot.connection.as_ref().map(Arc::clone)?;
        let connected = self.inner.catalog.connected_snapshot(server)?;
        Some(McpReusableServer {
            identity,
            connection,
            server: connected.server,
            tools: connected.tools,
            prompts: connected.prompts,
        })
    }

    fn same_runtime(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn connected_server(&self, server: &str) -> Option<Arc<dyn ConnectedServer>> {
        lock(&self.inner.servers)
            .get(server)
            .filter(|slot| matches!(slot.state, McpServerState::Connected))
            .and_then(|slot| slot.connection.as_ref())
            .map(|connection| connection.server())
    }

    /// Enables one server, joining an existing enable operation when present.
    pub async fn enable(&self, server: &str) -> Result<McpServerSnapshot, McpLifecycleError> {
        self.set_enabled(server, true).await
    }

    /// Disables one server, cancelling an in-flight connection when present.
    pub async fn disable(&self, server: &str) -> Result<McpServerSnapshot, McpLifecycleError> {
        self.set_enabled(server, false).await
    }

    /// Stops every transport owned by this session controller.
    ///
    /// Cached-only servers are already disabled and spawn nothing. Live local
    /// servers continue through their existing process-group or Job Object close
    /// path; this method adds no alternate child-process lifecycle.
    pub async fn shutdown(&self) {
        let mut servers = self.server_names();
        servers.reverse();
        for server in servers {
            let _snapshot = self.disable(&server).await;
        }
    }

    /// Drives one server toward the requested target.
    pub async fn set_enabled(
        &self,
        server: &str,
        enabled: bool,
    ) -> Result<McpServerSnapshot, McpLifecycleError> {
        let mut generation = self.request_target(server, enabled)?;
        loop {
            let Some(current) = generation else {
                return self.snapshot(server);
            };
            self.wait_for_operation(server, current).await?;
            let snapshot = self.snapshot(server)?;
            if snapshot.desired_enabled != enabled || target_reached(&snapshot.state, enabled) {
                return Ok(snapshot);
            }
            generation = self.reconcile(server)?;
        }
    }

    fn request_target(
        &self,
        server: &str,
        enabled: bool,
    ) -> Result<Option<u64>, McpLifecycleError> {
        let (generation, event, should_spawn) = {
            let mut servers = lock(&self.inner.servers);
            let slot = servers
                .get_mut(server)
                .ok_or_else(|| McpLifecycleError::UnknownServer {
                    server: server.to_owned(),
                })?;
            slot.desired_enabled = enabled;
            let mut event = None;
            if let Some(operation) = &slot.operation {
                if operation.kind == OperationKind::Connect && !enabled {
                    let _replaced = operation.cancel.send(true);
                    if slot.state != McpServerState::Disconnecting {
                        slot.state = McpServerState::Disconnecting;
                        event = Some(slot.snapshot(server));
                    }
                }
                (Some(operation.generation), event, false)
            } else if target_reached(&slot.state, enabled) {
                (None, event, false)
            } else {
                let generation = start_operation(slot, enabled);
                event = Some(slot.snapshot(server));
                (Some(generation), event, true)
            }
        };
        if let Some(snapshot) = event {
            self.publish(snapshot);
        }
        if should_spawn && let Some(generation) = generation {
            self.spawn_operation(server.to_owned(), generation);
        }
        Ok(generation)
    }

    fn reconcile(&self, server: &str) -> Result<Option<u64>, McpLifecycleError> {
        let (generation, snapshot) = {
            let mut servers = lock(&self.inner.servers);
            let slot = servers
                .get_mut(server)
                .ok_or_else(|| McpLifecycleError::UnknownServer {
                    server: server.to_owned(),
                })?;
            if let Some(operation) = &slot.operation {
                (Some(operation.generation), None)
            } else if target_reached(&slot.state, slot.desired_enabled) {
                (None, None)
            } else {
                let generation = start_operation(slot, slot.desired_enabled);
                (Some(generation), Some(slot.snapshot(server)))
            }
        };
        if let Some(snapshot) = snapshot {
            self.publish(snapshot);
        }
        if let Some(generation) = generation {
            self.spawn_operation(server.to_owned(), generation);
        }
        Ok(generation)
    }

    fn spawn_operation(&self, server: String, generation: u64) {
        let Some((kind, cancel)) = self.operation_receiver(&server, generation) else {
            return;
        };
        let controller = self.clone();
        tokio::spawn(async move {
            match kind {
                OperationKind::Connect => {
                    controller.run_connect(server, generation, cancel).await;
                }
                OperationKind::Disconnect => {
                    controller.run_disconnect(server, generation).await;
                }
            }
        });
    }

    fn operation_receiver(
        &self,
        server: &str,
        generation: u64,
    ) -> Option<(OperationKind, watch::Receiver<bool>)> {
        let servers = lock(&self.inner.servers);
        let operation = servers.get(server)?.operation.as_ref()?;
        (operation.generation == generation).then(|| (operation.kind, operation.cancel.subscribe()))
    }

    async fn run_connect(
        &self,
        server: String,
        generation: u64,
        mut cancel: watch::Receiver<bool>,
    ) {
        let timeout = self
            .inner
            .connector
            .connect_timeout(&server)
            .unwrap_or(self.inner.options.connect_timeout);
        let started = Instant::now();
        let connection = self.inner.connector.connect(&server);
        let result = tokio::select! {
            biased;
            changed = cancel.changed() => {
                let _closed = changed;
                ConnectResult::Cancelled
            }
            result = tokio::time::timeout(timeout, connection) => match result {
                Ok(result) => ConnectResult::Completed(result),
                Err(_) => ConnectResult::TimedOut(timeout),
            },
        };

        match result {
            ConnectResult::Completed(Ok(McpConnectOutcome::Connected(connection))) => {
                self.run_discovery(
                    server,
                    generation,
                    connection,
                    cancel,
                    timeout.saturating_sub(started.elapsed()),
                )
                .await;
            }
            ConnectResult::Completed(Ok(McpConnectOutcome::NeedsAuth)) => {
                self.finish_state(&server, generation, McpServerState::NeedsAuth);
            }
            ConnectResult::Completed(Ok(McpConnectOutcome::NeedsClientRegistration { error })) => {
                self.finish_state(
                    &server,
                    generation,
                    McpServerState::NeedsClientRegistration { error },
                );
            }
            ConnectResult::Completed(Err(error)) => {
                self.finish_state(&server, generation, McpServerState::Failed { error });
            }
            ConnectResult::TimedOut(elapsed) => {
                self.finish_state(
                    &server,
                    generation,
                    McpServerState::Failed {
                        error: format!("connection timed out after {elapsed:?}"),
                    },
                );
            }
            ConnectResult::Cancelled => {
                self.finish_state(&server, generation, McpServerState::Disabled);
            }
        }
    }

    async fn run_discovery(
        &self,
        server: String,
        generation: u64,
        connection: Arc<dyn McpConnection>,
        mut cancel: watch::Receiver<bool>,
        timeout: Duration,
    ) {
        let activation = activate(Arc::clone(&connection), &server);
        let result = tokio::select! {
            biased;
            changed = cancel.changed() => {
                let _closed = changed;
                ActivationResult::Cancelled
            }
            result = tokio::time::timeout(timeout, activation) => match result {
                Ok(result) => ActivationResult::Completed(result),
                Err(_) => ActivationResult::TimedOut(timeout),
            },
        };
        match result {
            ActivationResult::Completed(Ok(Activated {
                server: connected,
                tools,
                prompts,
            })) => {
                if self.should_install(&server, generation) {
                    self.finish_connected(
                        &server, generation, connection, connected, tools, prompts,
                    );
                } else {
                    self.close_connection(connection).await;
                    self.finish_state(&server, generation, McpServerState::Disabled);
                }
            }
            ActivationResult::Completed(Err(error)) => {
                self.close_connection(connection).await;
                self.finish_state(&server, generation, McpServerState::Failed { error });
            }
            ActivationResult::TimedOut(elapsed) => {
                self.close_connection(connection).await;
                self.finish_state(
                    &server,
                    generation,
                    McpServerState::Failed {
                        error: format!("initial discovery timed out after {elapsed:?}"),
                    },
                );
            }
            ActivationResult::Cancelled => {
                self.close_connection(connection).await;
                self.finish_state(&server, generation, McpServerState::Disabled);
            }
        }
    }

    async fn run_disconnect(&self, server: String, generation: u64) {
        let connection = {
            let mut servers = lock(&self.inner.servers);
            servers
                .get_mut(&server)
                .and_then(|slot| slot.connection.take())
        };
        if let Some(connection) = connection {
            let timeout = self.inner.options.close_timeout;
            match tokio::time::timeout(timeout, connection.close()).await {
                Ok(()) => self.finish_state(&server, generation, McpServerState::Disabled),
                Err(_) => self.finish_state(
                    &server,
                    generation,
                    McpServerState::Failed {
                        error: format!("shutdown timed out after {timeout:?}"),
                    },
                ),
            }
        } else {
            self.finish_state(&server, generation, McpServerState::Disabled);
        }
    }

    async fn close_connection(&self, connection: Arc<dyn McpConnection>) {
        let timeout = self.inner.options.close_timeout;
        let _bounded = tokio::time::timeout(timeout, connection.close()).await;
    }

    fn should_install(&self, server: &str, generation: u64) -> bool {
        lock(&self.inner.servers).get(server).is_some_and(|slot| {
            slot.desired_enabled
                && slot
                    .operation
                    .as_ref()
                    .is_some_and(|operation| operation.generation == generation)
        })
    }

    fn finish_connected(
        &self,
        server: &str,
        generation: u64,
        connection: Arc<dyn McpConnection>,
        connected: Arc<dyn ConnectedServer>,
        tools: Vec<ToolDefinition>,
        prompts: Vec<PromptDefinition>,
    ) {
        let snapshot = {
            let mut servers = lock(&self.inner.servers);
            let Some(slot) = servers.get_mut(server) else {
                return;
            };
            if !operation_matches(slot, generation) || !slot.desired_enabled {
                return;
            }
            self.inner
                .catalog
                .connected_with_prompts(connected, tools, prompts);
            slot.connection = Some(connection);
            slot.state = McpServerState::Connected;
            slot.operation = None;
            let snapshot = slot.snapshot(server);
            slot.changed.notify_waiters();
            snapshot
        };
        self.publish(snapshot);
    }

    fn finish_state(&self, server: &str, generation: u64, mut state: McpServerState) {
        let snapshot = {
            let mut servers = lock(&self.inner.servers);
            let Some(slot) = servers.get_mut(server) else {
                return;
            };
            if !operation_matches(slot, generation) {
                return;
            }
            if !slot.desired_enabled {
                state = McpServerState::Disabled;
            }
            if let Some(status) = state.catalog_status()
                && !status.is_connected()
            {
                self.inner.catalog.unavailable(server, status);
            }
            slot.state = state;
            slot.operation = None;
            let snapshot = slot.snapshot(server);
            slot.changed.notify_waiters();
            snapshot
        };
        self.publish(snapshot);
    }

    async fn wait_for_operation(
        &self,
        server: &str,
        generation: u64,
    ) -> Result<(), McpLifecycleError> {
        loop {
            let notified = {
                let servers = lock(&self.inner.servers);
                let slot = servers
                    .get(server)
                    .ok_or_else(|| McpLifecycleError::UnknownServer {
                        server: server.to_owned(),
                    })?;
                let notified = Arc::clone(&slot.changed).notified_owned();
                if !operation_matches(slot, generation) {
                    return Ok(());
                }
                notified
            };
            notified.await;
        }
    }

    fn publish(&self, snapshot: McpServerSnapshot) {
        let _receivers = self
            .inner
            .events
            .send(McpServerEvent::StateChanged { snapshot });
    }
}

struct LazyConnectedServer {
    server: String,
    tools: Vec<ToolDefinition>,
    controller: std::sync::Weak<Inner>,
}

impl LazyConnectedServer {
    fn controller(&self) -> Result<McpServerController, McpError> {
        self.controller
            .upgrade()
            .map(|inner| McpServerController { inner })
            .ok_or_else(|| McpError::Connect {
                server: self.server.clone(),
                source: Box::new(std::io::Error::other(
                    "owning MCP session runtime is no longer available",
                )),
            })
    }

    async fn live_server(&self) -> Result<Arc<dyn ConnectedServer>, McpError> {
        let controller = self.controller()?;
        if let Some(server) = controller.connected_server(&self.server) {
            return Ok(server);
        }
        let snapshot =
            controller
                .enable(&self.server)
                .await
                .map_err(|error| McpError::Connect {
                    server: self.server.clone(),
                    source: Box::new(error),
                })?;
        if !matches!(snapshot.state, McpServerState::Connected) {
            return Err(McpError::Connect {
                server: self.server.clone(),
                source: Box::new(std::io::Error::other(format!(
                    "lazy MCP activation ended in state {:?}",
                    snapshot.state
                ))),
            });
        }
        controller
            .connected_server(&self.server)
            .ok_or_else(|| McpError::Connect {
                server: self.server.clone(),
                source: Box::new(std::io::Error::other(
                    "lazy MCP activation published no live server handle",
                )),
            })
    }
}

#[async_trait]
impl ConnectedServer for LazyConnectedServer {
    fn server_name(&self) -> &str {
        &self.server
    }

    fn supports_resources(&self) -> bool {
        false
    }

    fn supports_prompts(&self) -> bool {
        false
    }

    async fn list_tools(&self) -> Result<Vec<ToolDefinition>, McpError> {
        if let Ok(controller) = self.controller()
            && let Some(server) = controller.connected_server(&self.server)
        {
            return server.list_tools().await;
        }
        Ok(self.tools.clone())
    }

    async fn call_tool(
        &self,
        tool: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<ToolCallResult, McpError> {
        self.live_server().await?.call_tool(tool, arguments).await
    }

    async fn list_resources(&self) -> Result<Vec<ResourceDefinition>, McpError> {
        self.live_server().await?.list_resources().await
    }

    async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        self.live_server().await?.list_resource_templates().await
    }

    async fn read_resource(&self, uri: &str) -> Result<ResourceContents, McpError> {
        self.live_server().await?.read_resource(uri).await
    }

    async fn list_prompts(&self) -> Result<Vec<PromptDefinition>, McpError> {
        self.live_server().await?.list_prompts().await
    }
}

fn transfer_reused(
    source: &McpServerController,
    target: &McpServerController,
    reusable: &[McpReusableServer],
) -> Result<(), McpLifecycleError> {
    let source_address = Arc::as_ptr(&source.inner).cast::<()>() as usize;
    let target_address = Arc::as_ptr(&target.inner).cast::<()>() as usize;
    let snapshots = if source_address < target_address {
        let mut source_servers = lock(&source.inner.servers);
        let mut target_servers = lock(&target.inner.servers);
        transfer_reused_locked(target, &mut source_servers, &mut target_servers, reusable)?
    } else {
        let mut target_servers = lock(&target.inner.servers);
        let mut source_servers = lock(&source.inner.servers);
        transfer_reused_locked(target, &mut source_servers, &mut target_servers, reusable)?
    };
    for snapshot in snapshots {
        target.publish(snapshot);
    }
    Ok(())
}

fn transfer_reused_locked(
    target: &McpServerController,
    source_servers: &mut BTreeMap<String, ServerSlot>,
    target_servers: &mut BTreeMap<String, ServerSlot>,
    reusable: &[McpReusableServer],
) -> Result<Vec<McpServerSnapshot>, McpLifecycleError> {
    for reusable in reusable {
        let server = reusable.server_name();
        let source_slot =
            source_servers
                .get(server)
                .ok_or_else(|| McpLifecycleError::UnknownServer {
                    server: server.to_owned(),
                })?;
        let source_matches = source_slot.operation.is_none()
            && matches!(source_slot.state, McpServerState::Connected)
            && source_slot.identity.as_ref() == Some(reusable.identity())
            && source_slot
                .connection
                .as_ref()
                .is_some_and(|connection| Arc::ptr_eq(connection, &reusable.connection));
        if !source_matches {
            return Err(McpLifecycleError::ReuseSlotBusy {
                server: server.to_owned(),
            });
        }
        let target_slot =
            target_servers
                .get(server)
                .ok_or_else(|| McpLifecycleError::UnknownServer {
                    server: server.to_owned(),
                })?;
        if target_slot.identity.as_ref() != Some(reusable.identity()) {
            return Err(McpLifecycleError::ToolDirectoryIdentityMismatch {
                server: server.to_owned(),
            });
        }
        if target_slot.operation.is_some()
            || target_slot.connection.is_some()
            || target_slot.desired_enabled
        {
            return Err(McpLifecycleError::ReuseSlotBusy {
                server: server.to_owned(),
            });
        }
    }

    let mut snapshots = Vec::with_capacity(reusable.len());
    for reusable in reusable {
        let server = reusable.server_name();
        target.inner.catalog.connected_with_prompts(
            Arc::clone(&reusable.server),
            reusable.tools.clone(),
            reusable.prompts.clone(),
        );
        let target_slot = target_servers
            .get_mut(server)
            .expect("validated reusable MCP target remains configured");
        target_slot.connection = Some(Arc::clone(&reusable.connection));
        target_slot.state = McpServerState::Connected;
        target_slot.desired_enabled = true;
        snapshots.push(target_slot.snapshot(server));

        let source_slot = source_servers
            .get_mut(server)
            .expect("validated reusable MCP source remains configured");
        let connection = source_slot
            .connection
            .take()
            .expect("validated reusable MCP source remains connected");
        debug_assert!(Arc::ptr_eq(&connection, &reusable.connection));
    }
    Ok(snapshots)
}

struct RuntimeManagerState {
    revision: u64,
    current: Option<McpServerController>,
}

/// Session-local owner of one published MCP controller snapshot.
///
/// Each ACP session creates its own manager. It deliberately has no global registry
/// and cannot share stdio processes across sessions.
#[derive(Clone)]
pub struct McpRuntimeManager {
    inner: Arc<AsyncMutex<RuntimeManagerState>>,
}

impl Default for McpRuntimeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for McpRuntimeManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpRuntimeManager")
            .finish_non_exhaustive()
    }
}

impl McpRuntimeManager {
    /// Empty session manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AsyncMutex::new(RuntimeManagerState {
                revision: 0,
                current: None,
            })),
        }
    }

    /// Session manager whose first published snapshot already exists.
    #[must_use]
    pub fn with_current(current: McpServerController) -> Self {
        Self {
            inner: Arc::new(AsyncMutex::new(RuntimeManagerState {
                revision: 1,
                current: Some(current),
            })),
        }
    }

    /// Current controller snapshot and its publication revision.
    pub async fn current(&self) -> (u64, Option<McpServerController>) {
        let state = self.inner.lock().await;
        (state.revision, state.current.clone())
    }

    /// Prepares a candidate and identifies exact-identity connections it may reuse.
    ///
    /// The candidate remains unpublished. Callers connect every required/eager name
    /// except [`PreparedMcpRuntime::reused_server_names`], then publish. Any failure
    /// before publication leaves the current snapshot untouched.
    pub async fn prepare(&self, candidate: Option<McpServerController>) -> PreparedMcpRuntime {
        let state = self.inner.lock().await;
        let mut reused = Vec::new();
        if let (Some(current), Some(candidate)) = (&state.current, &candidate) {
            for server in candidate.server_names() {
                // Reuse is only valid before this candidate starts its own transport.
                // Required ACP servers are eager and therefore connected by the time
                // the host reaches publication; optional cached servers remain disabled
                // and can adopt the exact live connection atomically.
                if !candidate
                    .snapshot(&server)
                    .is_ok_and(|snapshot| matches!(snapshot.state, McpServerState::Disabled))
                {
                    continue;
                }
                let Ok(Some(identity)) = candidate.connection_identity(&server) else {
                    continue;
                };
                let Some(reusable) = current.reusable_server(&server) else {
                    continue;
                };
                if reusable.identity() == &identity {
                    reused.push(reusable);
                }
            }
        }
        PreparedMcpRuntime {
            base_revision: state.revision,
            candidate,
            reused,
        }
    }

    /// Atomically publishes one prepared snapshot.
    ///
    /// All fallible validation runs before either controller is mutated. A stale or
    /// invalid candidate is returned in the error so its newly opened transports can
    /// be shut down while the old snapshot remains authoritative.
    pub async fn publish(
        &self,
        prepared: PreparedMcpRuntime,
    ) -> Result<McpRuntimePublication, McpRuntimePublishError> {
        let mut state = self.inner.lock().await;
        if state.revision != prepared.base_revision {
            return Err(McpRuntimePublishError {
                expected_revision: prepared.base_revision,
                actual_revision: state.revision,
                reason: "publication revision changed while the runtime was prepared".to_owned(),
                candidate: prepared.candidate,
            });
        }
        if !prepared.reused.is_empty() && prepared.candidate.is_none() {
            return Err(McpRuntimePublishError {
                expected_revision: prepared.base_revision,
                actual_revision: state.revision,
                reason: "a cleared runtime cannot adopt reusable servers".to_owned(),
                candidate: prepared.candidate,
            });
        }
        if let (Some(current), Some(candidate)) = (&state.current, &prepared.candidate) {
            if current.same_runtime(candidate) {
                state.revision = state.revision.wrapping_add(1);
                return Ok(McpRuntimePublication {
                    revision: state.revision,
                    current: state.current.clone(),
                    retired: None,
                });
            }
            if let Err(error) = transfer_reused(current, candidate, &prepared.reused) {
                return Err(McpRuntimePublishError {
                    expected_revision: prepared.base_revision,
                    actual_revision: state.revision,
                    reason: error.to_string(),
                    candidate: prepared.candidate,
                });
            }
        }

        let retired = std::mem::replace(&mut state.current, prepared.candidate);
        state.revision = state.revision.wrapping_add(1);
        Ok(McpRuntimePublication {
            revision: state.revision,
            current: state.current.clone(),
            retired,
        })
    }
}

/// Candidate controller plus exact live servers that startup should skip.
pub struct PreparedMcpRuntime {
    base_revision: u64,
    candidate: Option<McpServerController>,
    reused: Vec<McpReusableServer>,
}

impl PreparedMcpRuntime {
    /// Revision this preparation observed.
    #[must_use]
    pub const fn base_revision(&self) -> u64 {
        self.base_revision
    }

    /// Candidate controller used for eager startup before publication.
    #[must_use]
    pub fn candidate(&self) -> Option<McpServerController> {
        self.candidate.clone()
    }

    /// Names whose exact live connection will be transferred during publication.
    #[must_use]
    pub fn reused_server_names(&self) -> BTreeSet<String> {
        self.reused
            .iter()
            .map(|reusable| reusable.server_name().to_owned())
            .collect()
    }
}

/// Successful atomic publication. Retire the old snapshot after consumers switch.
#[derive(Debug)]
pub struct McpRuntimePublication {
    revision: u64,
    current: Option<McpServerController>,
    retired: Option<McpServerController>,
}

impl McpRuntimePublication {
    /// New publication revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Newly authoritative controller.
    #[must_use]
    pub fn current(&self) -> Option<McpServerController> {
        self.current.clone()
    }

    /// Stops the retired snapshot. Reused connections were relinquished first and
    /// remain owned by the new snapshot.
    pub async fn shutdown_retired(&self) {
        if let Some(retired) = &self.retired {
            retired.shutdown().await;
        }
    }
}

/// Publication rejection that preserves both the current snapshot and candidate.
#[derive(Debug, thiserror::Error)]
#[error(
    "MCP runtime publication failed at revision {actual_revision} (prepared from {expected_revision}): {reason}"
)]
pub struct McpRuntimePublishError {
    expected_revision: u64,
    actual_revision: u64,
    reason: String,
    candidate: Option<McpServerController>,
}

impl McpRuntimePublishError {
    /// Candidate that was not published and may be shut down independently.
    #[must_use]
    pub fn candidate(&self) -> Option<McpServerController> {
        self.candidate.clone()
    }
}

fn start_operation(slot: &mut ServerSlot, enabled: bool) -> u64 {
    slot.generation = slot.generation.wrapping_add(1);
    let generation = slot.generation;
    let (cancel, _receiver) = watch::channel(false);
    let kind = if enabled {
        slot.state = McpServerState::Connecting;
        OperationKind::Connect
    } else {
        slot.state = McpServerState::Disconnecting;
        OperationKind::Disconnect
    };
    slot.operation = Some(Operation {
        generation,
        kind,
        cancel,
    });
    generation
}

fn operation_matches(slot: &ServerSlot, generation: u64) -> bool {
    slot.operation
        .as_ref()
        .is_some_and(|operation| operation.generation == generation)
}

fn target_reached(state: &McpServerState, enabled: bool) -> bool {
    if enabled {
        matches!(
            state,
            McpServerState::Connected
                | McpServerState::Failed { .. }
                | McpServerState::NeedsAuth
                | McpServerState::NeedsClientRegistration { .. }
        )
    } else {
        matches!(
            state,
            McpServerState::Disabled | McpServerState::Failed { .. }
        )
    }
}

struct Activated {
    server: Arc<dyn ConnectedServer>,
    tools: Vec<ToolDefinition>,
    prompts: Vec<PromptDefinition>,
}

enum ConnectResult {
    Completed(Result<McpConnectOutcome, String>),
    TimedOut(Duration),
    Cancelled,
}

enum ActivationResult {
    Completed(Result<Activated, String>),
    TimedOut(Duration),
    Cancelled,
}

async fn activate(
    connection: Arc<dyn McpConnection>,
    expected_server: &str,
) -> Result<Activated, String> {
    let server = connection.server();
    if server.server_name() != expected_server {
        return Err(format!(
            "connector returned server {:?} for configured server {expected_server:?}",
            server.server_name()
        ));
    }
    let tools = server
        .list_tools()
        .await
        .map_err(|error| format!("initial tools/list failed: {error}"))?;
    let prompts = if server.supports_prompts() {
        server
            .list_prompts()
            .await
            .map_err(|error| format!("initial prompts/list failed: {error}"))?
    } else {
        Vec::new()
    };
    Ok(Activated {
        server,
        tools,
        prompts,
    })
}

fn digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(value);
}

#[cfg(unix)]
fn digest_path(digest: &mut Sha256, path: &Path) {
    use std::os::unix::ffi::OsStrExt as _;

    digest_field(digest, path.as_os_str().as_bytes());
}

#[cfg(windows)]
fn digest_path(digest: &mut Sha256, path: &Path) {
    use std::os::windows::ffi::OsStrExt as _;

    let bytes = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    digest_field(digest, &bytes);
}

#[cfg(not(any(unix, windows)))]
fn digest_path(digest: &mut Sha256, path: &Path) {
    digest_field(digest, path.to_string_lossy().as_bytes());
}

struct ConfiguredConnector {
    workspace: PathBuf,
    configs: BTreeMap<String, McpServerConfig>,
}

#[async_trait]
impl McpConnector for ConfiguredConnector {
    async fn connect(&self, server: &str) -> Result<McpConnectOutcome, String> {
        let config = self
            .configs
            .get(server)
            .ok_or_else(|| format!("MCP server {server:?} is not configured"))?;
        match config {
            McpServerConfig::Local(config) => {
                let client = StdioClient::connect(server, &self.workspace, config)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(McpConnectOutcome::Connected(Arc::new(StdioConnection {
                    client: Arc::new(client),
                })))
            }
            McpServerConfig::Remote(config) => match RemoteClient::connect(server, config).await {
                Ok(RemoteConnect::Connected(client)) => {
                    Ok(McpConnectOutcome::Connected(Arc::new(RemoteConnection {
                        client: Arc::new(client),
                    })))
                }
                Ok(RemoteConnect::AuthorizationRequired(_request)) => {
                    Ok(McpConnectOutcome::NeedsAuth)
                }
                Err(error) if error.needs_client_registration() => {
                    Ok(McpConnectOutcome::NeedsClientRegistration {
                        error: error.to_string(),
                    })
                }
                Err(error) => Err(error.to_string()),
            },
            McpServerConfig::Toggle(_) => Err(format!(
                "MCP server {server:?} has only an enabled toggle and no transport configuration"
            )),
        }
    }

    fn connect_timeout(&self, server: &str) -> Option<Duration> {
        let millis = match self.configs.get(server)? {
            McpServerConfig::Local(config) => config.timeout?,
            McpServerConfig::Remote(config) => config.timeout?,
            McpServerConfig::Toggle(_) => return None,
        };
        Some(Duration::from_millis(u64::from(millis.get())))
    }
}

struct StdioConnection {
    client: Arc<StdioClient>,
}

#[async_trait]
impl McpConnection for StdioConnection {
    fn server(&self) -> Arc<dyn ConnectedServer> {
        self.client.clone()
    }

    async fn close(&self) {
        self.client.close().await;
    }
}

struct RemoteConnection {
    client: Arc<RemoteClient>,
}

#[async_trait]
impl McpConnection for RemoteConnection {
    fn server(&self) -> Arc<dyn ConnectedServer> {
        self.client.clone()
    }

    async fn close(&self) {
        self.client.close().await;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
