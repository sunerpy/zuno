//! Runtime lifecycle control for configured MCP servers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Notify, broadcast, watch};
use tokio::time::Instant;
use zuno_config::schema::mcp::McpServerConfig;

use crate::{
    Catalog, ConnectedServer, PromptDefinition, RemoteClient, RemoteConnect, ServerStatus,
    StdioClient, ToolDefinition,
};

const EVENT_CAPACITY: usize = 64;

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
        let names: Vec<String> = configs.keys().cloned().collect();
        let connector = Arc::new(ConfiguredConnector {
            workspace: workspace.as_ref().to_owned(),
            configs,
        });
        Self::with_connector(catalog, names, connector, options)
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
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let servers = servers
            .into_iter()
            .map(Into::into)
            .map(|server| {
                catalog.unavailable(server.clone(), ServerStatus::Disabled);
                (
                    server,
                    ServerSlot {
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

    /// Enables one server, joining an existing enable operation when present.
    pub async fn enable(&self, server: &str) -> Result<McpServerSnapshot, McpLifecycleError> {
        self.set_enabled(server, true).await
    }

    /// Disables one server, cancelling an in-flight connection when present.
    pub async fn disable(&self, server: &str) -> Result<McpServerSnapshot, McpLifecycleError> {
        self.set_enabled(server, false).await
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
