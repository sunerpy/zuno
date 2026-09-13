use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use zuno_application::{ApplicationError, mcp::*, runtime::ExecutionLease};
use zuno_environment::mcp::McpExecutor;
use zuno_types::{activity::ActivityName, identity::*};

fn admission(owner: &str, id: &str) -> McpAdmission {
    McpAdmission {
        gateway_id: GatewayId::new("gateway").unwrap(),
        lease: ExecutionLease {
            owner: PrincipalKey {
                tenant_id: TenantId::new("tenant").unwrap(),
                principal_id: PrincipalId::new(owner).unwrap(),
            },
            job_id: JobId::new(format!("job-{owner}")).unwrap(),
            session_id: SessionId::new(format!("session-{owner}")).unwrap(),
            attempt_id: ExecutionAttemptId::new("attempt").unwrap(),
            worker: WorkerInstanceId::new("worker").unwrap(),
            epoch: 1,
            checkpoint_version: 0,
            expires_at_ms: i64::MAX,
        },
        operation: McpOperation {
            id: OperationId::new(id).unwrap(),
            invocation_id: InvocationId::new(id).unwrap(),
            environment_id: EnvironmentId::new("environment").unwrap(),
            binding: McpToolBinding {
                endpoint: "https://mcp.example/tool".to_owned(),
                connection: ActivityName::new("target").unwrap(),
                server: ActivityName::new("service").unwrap(),
                tool: ActivityName::new("apply").unwrap(),
                revision: 1,
                definition: json!({"name":"apply","inputSchema":{"type":"object"}}),
            },
            arguments: json!({"value":owner}),
        },
    }
}
struct Authority(AtomicBool);
#[async_trait]
impl McpOperationAuthority for Authority {
    async fn check_admitted_mcp(&self, admission: &McpAdmission) -> Result<(), ApplicationError> {
        self.authorize_mcp(admission).await
    }
    async fn authorize_mcp(&self, _: &McpAdmission) -> Result<(), ApplicationError> {
        if self.0.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(ApplicationError::Forbidden)
        }
    }
}
#[derive(Default)]
struct Provider {
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    hang: AtomicBool,
}
struct Call {
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    hang: bool,
}
#[async_trait]
impl McpConnectionProvider for Provider {
    async fn prepare(
        &self,
        _: &PrincipalKey,
        _: &McpToolBinding,
    ) -> Result<Box<dyn PreparedMcpCall>, McpFailure> {
        Ok(Box::new(Call {
            calls: self.calls.clone(),
            entered: self.entered.clone(),
            release: self.release.clone(),
            hang: self.hang.load(Ordering::SeqCst),
        }))
    }
}
#[async_trait]
impl PreparedMcpCall for Call {
    async fn call(self: Box<Self>, arguments: &Value) -> Result<Value, McpFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.hang {
            self.release.notified().await;
        }
        Ok(json!({"content":[{"type":"text","text":arguments["value"]}],"isError":false}))
    }
}
#[derive(Default)]
struct Sink {
    lost: AtomicBool,
    receipts: Mutex<Vec<McpCompletion>>,
}
#[async_trait]
impl McpCompletionSink for Sink {
    async fn publish_mcp(&self, completion: &McpCompletion) -> Result<(), ApplicationError> {
        completion.validate()?;
        self.receipts.lock().unwrap().push(completion.clone());
        if self.lost.swap(false, Ordering::SeqCst) {
            Err(ApplicationError::Unavailable)
        } else {
            Ok(())
        }
    }
}
async fn until(executor: &McpExecutor, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            executor.advance().unwrap();
            if condition() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn mcp_lost_delivery_never_repeats_call_and_owners_remain_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let authority = Arc::new(Authority(AtomicBool::new(false)));
    let provider = Arc::new(Provider::default());
    let sink = Arc::new(Sink::default());
    sink.lost.store(true, Ordering::SeqCst);
    let executor = McpExecutor::open(
        &dir.path().join("mcp.sqlite"),
        authority.clone(),
        provider.clone(),
        sink.clone(),
        2,
    )
    .unwrap();
    let a = admission("alice", "same");
    let b = admission("bob", "same");
    assert!(executor.submit(&a).await.is_err());
    authority.0.store(true, Ordering::SeqCst);
    executor.submit(&a).await.unwrap();
    executor.submit(&b).await.unwrap();
    until(&executor, || sink.receipts.lock().unwrap().len() >= 3).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    for value in [&a, &b] {
        let receipt = executor.submit(value).await.unwrap();
        assert_eq!(
            receipt.result.unwrap()["content"][0]["text"],
            value.operation.arguments["value"]
        );
    }
    let mut changed = a.clone();
    changed.operation.arguments = json!({"value":"different"});
    assert!(executor.submit(&changed).await.is_err());
    let mut wrong_job = a.lease.clone();
    wrong_job.job_id = JobId::new("other-job").unwrap();
    assert!(
        executor
            .receipt_for(&wrong_job, &a.operation.environment_id, &a.operation.id)
            .is_err()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn mcp_restart_preserves_uncertainty_and_never_replays_a_running_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sqlite");
    let authority = Arc::new(Authority(AtomicBool::new(true)));
    let provider = Arc::new(Provider::default());
    provider.hang.store(true, Ordering::SeqCst);
    let sink = Arc::new(Sink::default());
    let a = admission("alice", "running");
    let executor =
        McpExecutor::open(&path, authority.clone(), provider.clone(), sink.clone(), 1).unwrap();
    executor.submit(&a).await.unwrap();
    executor.advance().unwrap();
    tokio::time::timeout(Duration::from_secs(2), provider.entered.notified())
        .await
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    executor.drain(Duration::from_millis(1)).await;
    drop(executor);
    provider.hang.store(false, Ordering::SeqCst);
    let executor = McpExecutor::open(&path, authority, provider.clone(), sink.clone(), 1).unwrap();
    assert_eq!(
        executor
            .receipt(&a.lease.owner, &a.operation.id)
            .unwrap()
            .state,
        McpOperationState::Uncertain
    );
    until(&executor, || !sink.receipts.lock().unwrap().is_empty()).await;
    executor.submit(&a).await.unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        sink.receipts.lock().unwrap()[0].receipt.failure,
        Some(McpFailure::LostOutcome)
    );
}
#[tokio::test]
async fn mcp_cancellation_before_submit_is_a_tombstone_and_late_success_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let authority = Arc::new(Authority(AtomicBool::new(true)));
    let provider = Arc::new(Provider::default());
    let sink = Arc::new(Sink::default());
    let executor = McpExecutor::open(
        &dir.path().join("mcp.sqlite"),
        authority,
        provider.clone(),
        sink.clone(),
        1,
    )
    .unwrap();
    let a = admission("alice", "cancel-before");
    executor.cancel(&a).unwrap();
    executor.submit(&a).await.unwrap();
    until(&executor, || !sink.receipts.lock().unwrap().is_empty()).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        executor
            .receipt(&a.lease.owner, &a.operation.id)
            .unwrap()
            .state,
        McpOperationState::Cancelled
    );
    let b = admission("alice", "cancel-during");
    provider.hang.store(true, Ordering::SeqCst);
    executor.submit(&b).await.unwrap();
    until(&executor, || provider.calls.load(Ordering::SeqCst) == 1).await;
    executor.cancel(&b).unwrap();
    provider.release.notify_one();
    until(&executor, || {
        sink.receipts
            .lock()
            .unwrap()
            .iter()
            .any(|value| value.receipt.id == b.operation.id)
    })
    .await;
    let receipt = executor.receipt(&b.lease.owner, &b.operation.id).unwrap();
    assert_eq!(receipt.state, McpOperationState::Succeeded);
    assert!(receipt.cancellation_requested);
}
#[tokio::test]
async fn mcp_revocation_before_external_call_stops_queued_execution() {
    let dir = tempfile::tempdir().unwrap();
    let authority = Arc::new(Authority(AtomicBool::new(true)));
    let provider = Arc::new(Provider::default());
    let sink = Arc::new(Sink::default());
    let executor = McpExecutor::open(
        &dir.path().join("mcp.sqlite"),
        authority.clone(),
        provider.clone(),
        sink.clone(),
        1,
    )
    .unwrap();
    let a = admission("alice", "revoked");
    executor.submit(&a).await.unwrap();
    authority.0.store(false, Ordering::SeqCst);
    until(&executor, || !sink.receipts.lock().unwrap().is_empty()).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        executor
            .receipt(&a.lease.owner, &a.operation.id)
            .unwrap()
            .failure,
        Some(McpFailure::AuthorizationRevoked)
    );
}
