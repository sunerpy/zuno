use std::collections::BTreeMap;
use std::future::pending;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::Notify;
use zuno_config::schema::mcp::{LocalKind, McpLocal, McpRemote, McpServerConfig, RemoteKind};
use zuno_error::McpError;
use zuno_mcp::{
    Catalog, ConnectedServer, McpConnectOutcome, McpConnection, McpConnectionIdentity,
    McpConnector, McpLifecycleError, McpLifecycleOptions, McpRuntimeManager, McpServerController,
    McpServerEvent, McpServerState, McpToolDirectory, PromptDefinition, ResourceContents,
    ResourceDefinition, ResourceTemplate, ToolCallResult, ToolDefinition,
};
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

const SERVER: &str = "fake";

#[derive(Clone, Copy)]
enum ConnectBehavior {
    Immediate,
    Blocked,
    Never,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiscoveryBehavior {
    Immediate,
    FailTools,
    BlockTools,
    FailPrompts,
}

struct FakeConnector {
    behavior: ConnectBehavior,
    calls: AtomicUsize,
    started: Notify,
    release: Notify,
    cancelled: Arc<AtomicBool>,
    connection: Arc<FakeConnection>,
}

impl FakeConnector {
    fn new(behavior: ConnectBehavior) -> Arc<Self> {
        Self::with_discovery(behavior, DiscoveryBehavior::Immediate)
    }

    fn with_discovery(behavior: ConnectBehavior, discovery: DiscoveryBehavior) -> Arc<Self> {
        Arc::new(Self {
            behavior,
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
            connection: Arc::new(FakeConnection::new(discovery)),
        })
    }

    async fn wait_until_started(&self) {
        while self.calls.load(Ordering::SeqCst) == 0 {
            self.started.notified().await;
        }
    }
}

struct CancelProbe {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for CancelProbe {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl McpConnector for FakeConnector {
    async fn connect(&self, server: &str) -> Result<McpConnectOutcome, String> {
        assert_eq!(server, SERVER);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_waiters();
        let mut probe = CancelProbe {
            cancelled: Arc::clone(&self.cancelled),
            armed: true,
        };
        match self.behavior {
            ConnectBehavior::Immediate => {}
            ConnectBehavior::Blocked => self.release.notified().await,
            ConnectBehavior::Never => pending::<()>().await,
        }
        probe.armed = false;
        Ok(McpConnectOutcome::Connected(self.connection.clone()))
    }
}

struct FakeConnection {
    server: Arc<FakeServer>,
    child_alive: AtomicBool,
    close_calls: AtomicUsize,
}

impl FakeConnection {
    fn new(discovery: DiscoveryBehavior) -> Self {
        Self {
            server: Arc::new(FakeServer {
                discovery,
                calls: AtomicUsize::new(0),
                tool_calls: AtomicUsize::new(0),
                started: Notify::new(),
                cancelled: Arc::new(AtomicBool::new(false)),
            }),
            child_alive: AtomicBool::new(true),
            close_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl McpConnection for FakeConnection {
    fn server(&self) -> Arc<dyn ConnectedServer> {
        self.server.clone()
    }

    async fn close(&self) {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        self.child_alive.store(false, Ordering::SeqCst);
    }
}

struct FakeServer {
    discovery: DiscoveryBehavior,
    calls: AtomicUsize,
    tool_calls: AtomicUsize,
    started: Notify,
    cancelled: Arc<AtomicBool>,
}

impl FakeServer {
    async fn wait_until_started(&self) {
        while self.calls.load(Ordering::SeqCst) == 0 {
            self.started.notified().await;
        }
    }
}

#[async_trait]
impl ConnectedServer for FakeServer {
    fn server_name(&self) -> &str {
        SERVER
    }

    fn supports_resources(&self) -> bool {
        false
    }

    fn supports_prompts(&self) -> bool {
        self.discovery == DiscoveryBehavior::FailPrompts
    }

    async fn list_tools(&self) -> Result<Vec<ToolDefinition>, McpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_waiters();
        match self.discovery {
            DiscoveryBehavior::FailTools => Err(discovery_error("tools/list failed")),
            DiscoveryBehavior::BlockTools => {
                let _probe = CancelProbe {
                    cancelled: Arc::clone(&self.cancelled),
                    armed: true,
                };
                pending::<Result<Vec<ToolDefinition>, McpError>>().await
            }
            DiscoveryBehavior::Immediate | DiscoveryBehavior::FailPrompts => {
                Ok(vec![tool_definition()])
            }
        }
    }

    async fn call_tool(
        &self,
        tool: &str,
        _arguments: Map<String, Value>,
    ) -> Result<ToolCallResult, McpError> {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolCallResult {
            content: vec![json!({"type": "text", "text": format!("called {tool}")})],
            structured_content: None,
            is_error: false,
            extra: Map::new(),
        })
    }

    async fn list_resources(&self) -> Result<Vec<ResourceDefinition>, McpError> {
        Ok(Vec::new())
    }

    async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        Ok(Vec::new())
    }

    async fn read_resource(&self, _uri: &str) -> Result<ResourceContents, McpError> {
        panic!("fake resource reads are outside lifecycle tests")
    }

    async fn list_prompts(&self) -> Result<Vec<PromptDefinition>, McpError> {
        if self.discovery == DiscoveryBehavior::FailPrompts {
            Err(discovery_error("prompts/list failed"))
        } else {
            Ok(Vec::new())
        }
    }
}

fn discovery_error(message: &str) -> McpError {
    McpError::Handshake {
        server: SERVER.to_owned(),
        source: Box::new(std::io::Error::other(message.to_owned())),
    }
}

fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "echo".to_owned(),
        description: Some("Echo a value".to_owned()),
        input_schema: json!({
            "type": "object",
            "properties": {"value": {"type": "string"}}
        }),
        output_schema: None,
        extra: Map::new(),
    }
}

fn connection_identity(command: &str) -> McpConnectionIdentity {
    let config = McpServerConfig::Local(McpLocal {
        kind: LocalKind::Local,
        command: vec![command.to_owned()],
        cwd: None,
        environment: None,
        enabled: Some(true),
        timeout: None,
    });
    McpConnectionIdentity::from_config(SERVER, "/workspace", &config)
}

#[test]
fn connection_identity_includes_internal_transport_policy() {
    let mut remote = McpRemote {
        kind: RemoteKind::Remote,
        url: "https://mcp.example.test".to_owned(),
        enabled: Some(true),
        headers: None,
        oauth: None,
        timeout: None,
        streamable_http_only: false,
    };
    let negotiated = McpConnectionIdentity::from_config(
        SERVER,
        "/workspace",
        &McpServerConfig::Remote(remote.clone()),
    );
    remote.streamable_http_only = true;
    let streamable_only =
        McpConnectionIdentity::from_config(SERVER, "/workspace", &McpServerConfig::Remote(remote));

    assert_ne!(negotiated, streamable_only);
}

fn controller_with_identity(
    connector: Arc<FakeConnector>,
    identity: McpConnectionIdentity,
) -> McpServerController {
    McpServerController::with_connector_and_identities(
        Catalog::new([SERVER]),
        [SERVER],
        BTreeMap::from([(SERVER.to_owned(), identity)]),
        connector,
        McpLifecycleOptions {
            connect_timeout: Duration::from_secs(1),
            close_timeout: Duration::from_secs(1),
        },
    )
}

fn tool_context(call: &str) -> ToolContext {
    ToolContext::new(
        "session",
        "message",
        call,
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn controller(connector: Arc<FakeConnector>, timeout: Duration) -> McpServerController {
    McpServerController::with_connector(
        Catalog::new([SERVER]),
        [SERVER],
        connector,
        McpLifecycleOptions {
            connect_timeout: timeout,
            close_timeout: timeout,
        },
    )
}

#[tokio::test]
async fn enable_connects_once_updates_catalog_and_publishes_state() {
    let connector = FakeConnector::new(ConnectBehavior::Immediate);
    let controller = controller(connector.clone(), Duration::from_secs(1));
    let mut events = controller.subscribe();

    let snapshot = controller.enable(SERVER).await.expect("enable");

    assert_eq!(snapshot.state, McpServerState::Connected);
    assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    assert_eq!(controller.catalog().connected_servers(), vec![SERVER]);
    assert!(matches!(
        events.recv().await.expect("connecting event"),
        McpServerEvent::StateChanged { snapshot }
            if snapshot.state == McpServerState::Connecting
    ));
    assert!(matches!(
        events.recv().await.expect("connected event"),
        McpServerEvent::StateChanged { snapshot }
            if snapshot.state == McpServerState::Connected
    ));
}

#[tokio::test]
async fn disable_closes_connection_and_updates_catalog() {
    let connector = FakeConnector::new(ConnectBehavior::Immediate);
    let controller = controller(connector.clone(), Duration::from_secs(1));
    controller.enable(SERVER).await.expect("enable");

    let snapshot = controller.disable(SERVER).await.expect("disable");

    assert_eq!(snapshot.state, McpServerState::Disabled);
    assert_eq!(connector.connection.close_calls.load(Ordering::SeqCst), 1);
    assert!(controller.catalog().connected_servers().is_empty());
    assert_eq!(
        controller.catalog().diagnostics()[0].status,
        zuno_mcp::ServerStatus::Disabled
    );
}

#[tokio::test]
async fn enable_while_connecting_joins_the_single_connect_attempt() {
    let connector = FakeConnector::new(ConnectBehavior::Blocked);
    let controller = controller(connector.clone(), Duration::from_secs(1));
    let first = tokio::spawn({
        let controller = controller.clone();
        async move { controller.enable(SERVER).await }
    });
    connector.wait_until_started().await;
    let second = tokio::spawn({
        let controller = controller.clone();
        async move { controller.enable(SERVER).await }
    });
    tokio::task::yield_now().await;

    assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    connector.release.notify_waiters();

    assert_eq!(
        first
            .await
            .expect("first task")
            .expect("first enable")
            .state,
        McpServerState::Connected
    );
    assert_eq!(
        second
            .await
            .expect("second task")
            .expect("second enable")
            .state,
        McpServerState::Connected
    );
    assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disable_while_connecting_cancels_the_attempt_without_installing_it() {
    let connector = FakeConnector::new(ConnectBehavior::Blocked);
    let controller = controller(connector.clone(), Duration::from_secs(1));
    let enabling = tokio::spawn({
        let controller = controller.clone();
        async move { controller.enable(SERVER).await }
    });
    connector.wait_until_started().await;

    let disabled = controller.disable(SERVER).await.expect("disable");
    let enable_result = enabling.await.expect("enable task").expect("enable result");

    assert_eq!(disabled.state, McpServerState::Disabled);
    assert_eq!(enable_result.state, McpServerState::Disabled);
    assert!(connector.cancelled.load(Ordering::SeqCst));
    assert!(controller.catalog().connected_servers().is_empty());
}

#[tokio::test(start_paused = true)]
async fn never_responding_connect_is_cancelled_at_the_bound() {
    let connector = FakeConnector::new(ConnectBehavior::Never);
    let controller = controller(connector.clone(), Duration::from_secs(5));
    let enabling = tokio::spawn({
        let controller = controller.clone();
        async move { controller.enable(SERVER).await }
    });
    connector.wait_until_started().await;

    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    assert!(
        enabling.is_finished(),
        "connect exceeded its configured bound"
    );
    let snapshot = enabling.await.expect("enable task").expect("enable result");

    assert!(matches!(
        snapshot.state,
        McpServerState::Failed { ref error } if error.contains("timed out")
    ));
    assert!(connector.cancelled.load(Ordering::SeqCst));
    assert!(controller.catalog().connected_servers().is_empty());
}

#[tokio::test]
async fn disabling_a_local_connection_terminates_its_child() {
    let connector = FakeConnector::new(ConnectBehavior::Immediate);
    let controller = controller(connector.clone(), Duration::from_secs(1));
    controller.enable(SERVER).await.expect("enable");
    assert!(connector.connection.child_alive.load(Ordering::SeqCst));

    controller.disable(SERVER).await.expect("disable");

    assert!(!connector.connection.child_alive.load(Ordering::SeqCst));
    assert_eq!(connector.connection.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn initial_tools_failure_closes_the_connected_transport() {
    let connector =
        FakeConnector::with_discovery(ConnectBehavior::Immediate, DiscoveryBehavior::FailTools);
    let controller = controller(connector.clone(), Duration::from_secs(1));

    let snapshot = controller.enable(SERVER).await.expect("enable result");

    assert!(matches!(
        snapshot.state,
        McpServerState::Failed { ref error } if error.contains("initial tools/list failed")
    ));
    assert_eq!(connector.connection.close_calls.load(Ordering::SeqCst), 1);
    assert!(!connector.connection.child_alive.load(Ordering::SeqCst));
}

#[tokio::test]
async fn initial_prompts_failure_closes_the_connected_transport() {
    let connector =
        FakeConnector::with_discovery(ConnectBehavior::Immediate, DiscoveryBehavior::FailPrompts);
    let controller = controller(connector.clone(), Duration::from_secs(1));

    let snapshot = controller.enable(SERVER).await.expect("enable result");

    assert!(matches!(
        snapshot.state,
        McpServerState::Failed { ref error } if error.contains("initial prompts/list failed")
    ));
    assert_eq!(connector.connection.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disable_during_initial_discovery_closes_the_connected_transport() {
    let connector =
        FakeConnector::with_discovery(ConnectBehavior::Immediate, DiscoveryBehavior::BlockTools);
    let controller = controller(connector.clone(), Duration::from_secs(1));
    let enabling = tokio::spawn({
        let controller = controller.clone();
        async move { controller.enable(SERVER).await }
    });
    connector.connection.server.wait_until_started().await;

    let disabled = controller.disable(SERVER).await.expect("disable");
    let enabled = enabling.await.expect("enable task").expect("enable result");

    assert_eq!(disabled.state, McpServerState::Disabled);
    assert_eq!(enabled.state, McpServerState::Disabled);
    assert!(connector.connection.server.cancelled.load(Ordering::SeqCst));
    assert_eq!(connector.connection.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn initial_discovery_timeout_closes_the_connected_transport() {
    let connector =
        FakeConnector::with_discovery(ConnectBehavior::Immediate, DiscoveryBehavior::BlockTools);
    let controller = controller(connector.clone(), Duration::from_secs(5));
    let enabling = tokio::spawn({
        let controller = controller.clone();
        async move { controller.enable(SERVER).await }
    });
    connector.connection.server.wait_until_started().await;

    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::task::yield_now().await;
    let snapshot = enabling.await.expect("enable task").expect("enable result");

    assert!(matches!(
        snapshot.state,
        McpServerState::Failed { ref error } if error.contains("initial discovery timed out")
    ));
    assert!(connector.connection.server.cancelled.load(Ordering::SeqCst));
    assert_eq!(connector.connection.close_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lagged_lifecycle_subscriber_recovers_latest_state_from_snapshots() {
    let connector = FakeConnector::new(ConnectBehavior::Immediate);
    let controller = controller(connector, Duration::from_secs(1));
    let mut stalled = controller.subscribe();

    for _ in 0..17 {
        controller.enable(SERVER).await.expect("enable");
        controller.disable(SERVER).await.expect("disable");
    }

    assert!(matches!(
        stalled.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) if skipped >= 4
    ));
    assert_eq!(
        controller.snapshots(),
        vec![zuno_mcp::McpServerSnapshot {
            server: SERVER.to_owned(),
            state: McpServerState::Disabled,
            desired_enabled: false,
        }]
    );
}

#[tokio::test]
async fn cached_tool_directory_is_visible_without_starting_the_server() {
    let connector = FakeConnector::new(ConnectBehavior::Immediate);
    let identity = connection_identity("server-v1");
    let controller = controller_with_identity(connector.clone(), identity.clone());

    controller
        .install_cached_directory(McpToolDirectory::new(identity, vec![tool_definition()]))
        .expect("matching directory");

    assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
    assert!(controller.catalog().connected_servers().is_empty());
    assert_eq!(controller.catalog().tool_ids(), vec!["fake_echo"]);
    assert!(matches!(
        controller.catalog().diagnostics()[0].status,
        zuno_mcp::ServerStatus::Cached
    ));
}

#[tokio::test]
async fn concurrent_first_cached_tool_calls_share_one_connection_attempt() {
    let connector = FakeConnector::new(ConnectBehavior::Immediate);
    let identity = connection_identity("server-v1");
    let controller = controller_with_identity(connector.clone(), identity.clone());
    controller
        .install_cached_directory(McpToolDirectory::new(identity, vec![tool_definition()]))
        .expect("matching directory");
    let tool = controller
        .catalog()
        .tools()
        .into_iter()
        .find(|tool| tool.id() == "fake_echo")
        .expect("cached tool proxy");

    let calls = (0..8)
        .map(|index| {
            let tool = Arc::clone(&tool);
            tokio::spawn(async move {
                tool.invoke(
                    json!({"value": index.to_string()}),
                    tool_context(&format!("call-{index}")),
                )
                .await
            })
        })
        .collect::<Vec<_>>();
    for call in calls {
        call.await.expect("tool task").expect("tool result");
    }

    assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        connector
            .connection
            .server
            .tool_calls
            .load(Ordering::SeqCst),
        8
    );
    assert_eq!(controller.catalog().connected_servers(), vec![SERVER]);
}

#[tokio::test]
async fn changed_connection_identity_rejects_cache_and_requires_eager_discovery() {
    let connector = FakeConnector::new(ConnectBehavior::Immediate);
    let controller = controller_with_identity(connector.clone(), connection_identity("server-v2"));

    let error = controller
        .install_cached_directory(McpToolDirectory::new(
            connection_identity("server-v1"),
            vec![tool_definition()],
        ))
        .expect_err("stale identity");

    assert!(matches!(
        error,
        McpLifecycleError::ToolDirectoryIdentityMismatch { .. }
    ));
    assert!(controller.catalog().tool_ids().is_empty());
    controller.enable(SERVER).await.expect("eager discovery");
    assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    assert_eq!(controller.catalog().tool_ids(), vec!["fake_echo"]);
}

#[tokio::test]
async fn runtime_manager_publishes_reuse_then_retired_shutdown_keeps_connection_alive() {
    let identity = connection_identity("server-v1");
    let old_connector = FakeConnector::new(ConnectBehavior::Immediate);
    let old = controller_with_identity(old_connector.clone(), identity.clone());
    old.enable(SERVER).await.expect("old runtime connect");
    let manager = McpRuntimeManager::with_current(old);

    let candidate_connector = FakeConnector::new(ConnectBehavior::Immediate);
    let candidate = controller_with_identity(candidate_connector.clone(), identity);
    let prepared = manager.prepare(Some(candidate)).await;
    assert_eq!(
        prepared.reused_server_names(),
        std::iter::once(SERVER.to_owned()).collect()
    );

    let publication = manager.publish(prepared).await.expect("publish");
    publication.shutdown_retired().await;

    assert_eq!(candidate_connector.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        old_connector.connection.close_calls.load(Ordering::SeqCst),
        0,
        "retired snapshot relinquishes the connection before shutdown"
    );
    let current = publication.current().expect("current runtime");
    assert_eq!(current.catalog().connected_servers(), vec![SERVER]);
    current.shutdown().await;
    assert_eq!(
        old_connector.connection.close_calls.load(Ordering::SeqCst),
        1,
        "the new snapshot owns the final close"
    );
}

#[tokio::test]
async fn runtime_manager_rejects_reuse_when_source_changes_after_prepare() {
    let identity = connection_identity("server-v1");
    let old_connector = FakeConnector::new(ConnectBehavior::Immediate);
    let old = controller_with_identity(old_connector.clone(), identity.clone());
    old.enable(SERVER).await.expect("old runtime connect");
    let manager = McpRuntimeManager::with_current(old.clone());
    let candidate =
        controller_with_identity(FakeConnector::new(ConnectBehavior::Immediate), identity);
    let prepared = manager.prepare(Some(candidate)).await;
    assert_eq!(prepared.reused_server_names().len(), 1);

    old.disable(SERVER).await.expect("concurrent disable");
    let error = manager
        .publish(prepared)
        .await
        .expect_err("changed source must reject publication");

    let candidate = error.candidate().expect("unpublished candidate");
    assert!(candidate.catalog().connected_servers().is_empty());
    let (_, current) = manager.current().await;
    assert_eq!(
        current
            .expect("old runtime remains current")
            .snapshot(SERVER)
            .expect("old server")
            .state,
        McpServerState::Disabled
    );
    assert_eq!(
        old_connector.connection.close_calls.load(Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn stale_runtime_publication_rolls_back_without_replacing_current_snapshot() {
    let old_connector = FakeConnector::new(ConnectBehavior::Immediate);
    let old = controller_with_identity(old_connector, connection_identity("server-v1"));
    old.enable(SERVER).await.expect("old runtime connect");
    let manager = McpRuntimeManager::with_current(old);

    let stale = manager
        .prepare(Some(controller_with_identity(
            FakeConnector::new(ConnectBehavior::Immediate),
            connection_identity("server-v1"),
        )))
        .await;
    let replacement_connector = FakeConnector::new(ConnectBehavior::Immediate);
    let replacement = controller_with_identity(
        replacement_connector.clone(),
        connection_identity("server-v2"),
    );
    replacement
        .enable(SERVER)
        .await
        .expect("replacement connect");
    let replacement = manager.prepare(Some(replacement)).await;
    let publication = manager
        .publish(replacement)
        .await
        .expect("replacement publish");

    let error = manager.publish(stale).await.expect_err("stale publication");
    assert!(error.candidate().is_some());
    let (_, current) = manager.current().await;
    assert_eq!(
        current
            .expect("current runtime")
            .connection_identity(SERVER)
            .expect("configured server"),
        Some(connection_identity("server-v2"))
    );
    assert_eq!(replacement_connector.calls.load(Ordering::SeqCst), 1);
    publication.shutdown_retired().await;
}

#[test]
fn fake_state_is_thread_safe() {
    let value = Mutex::new(McpServerState::Disabled);
    assert_eq!(*value.lock().expect("lock"), McpServerState::Disabled);
}
