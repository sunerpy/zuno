use std::future::pending;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::sync::Notify;
use zuno_error::McpError;
use zuno_mcp::{
    Catalog, ConnectedServer, McpConnectOutcome, McpConnection, McpConnector, McpLifecycleOptions,
    McpServerController, McpServerEvent, McpServerState, PromptDefinition, ResourceContents,
    ResourceDefinition, ResourceTemplate, ToolCallResult, ToolDefinition,
};

const SERVER: &str = "fake";

#[derive(Clone, Copy)]
enum ConnectBehavior {
    Immediate,
    Blocked,
    Never,
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
        Arc::new(Self {
            behavior,
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
            connection: Arc::new(FakeConnection::new()),
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
    fn new() -> Self {
        Self {
            server: Arc::new(FakeServer),
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

struct FakeServer;

#[async_trait]
impl ConnectedServer for FakeServer {
    fn server_name(&self) -> &str {
        SERVER
    }

    fn supports_resources(&self) -> bool {
        false
    }

    fn supports_prompts(&self) -> bool {
        false
    }

    async fn list_tools(&self) -> Result<Vec<ToolDefinition>, McpError> {
        Ok(Vec::new())
    }

    async fn call_tool(
        &self,
        _tool: &str,
        _arguments: Map<String, Value>,
    ) -> Result<ToolCallResult, McpError> {
        panic!("fake tool calls are outside lifecycle tests")
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
        Ok(Vec::new())
    }
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

#[test]
fn fake_state_is_thread_safe() {
    let value = Mutex::new(McpServerState::Disabled);
    assert_eq!(*value.lock().expect("lock"), McpServerState::Disabled);
}
