use super::*;
use zuno_types::identity::{
    ExecutionAttemptId, InvocationId, JobId, PrincipalId, SessionId, TenantId, WorkerInstanceId,
};

struct Authority;
#[async_trait]
impl OperationAuthority for Authority {
    async fn authorize(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        _operation: &CommandOperation,
    ) -> Result<(), ApplicationError> {
        if lease.owner != environment.owner || lease.session_id != environment.spec.session_id {
            return Err(ApplicationError::Forbidden);
        }
        Ok(())
    }
}
struct Sink;
#[async_trait]
impl OperationCompletionSink for Sink {
    async fn publish(&self, completion: &OperationCompletion) -> Result<(), ApplicationError> {
        completion.validate()
    }
}

#[tokio::test]
#[ignore = "requires scripts/check_enterprise_docker.py and its isolated rootless daemon"]
async fn receipt_observation_cannot_invalidate_an_inflight_container_start() {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let socket = std::env::var("ZUNO_ROOTLESS_DOCKER_SOCKET").unwrap();
    let gateway = Arc::new(
        DockerGateway::connect(
            Path::new(&socket),
            &directory.path().join("gateway.sqlite"),
            Arc::new(Authority),
        )
        .await
        .unwrap(),
    );
    let owner = PrincipalKey {
        tenant_id: TenantId::new("start-race").unwrap(),
        principal_id: PrincipalId::new("owner").unwrap(),
    };
    let spec=EnvironmentSpec {
        id:EnvironmentId::new("environment").unwrap(),session_id:SessionId::new("session").unwrap(),
        image:"public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce".to_owned(),
        memory_bytes:67108864,pids_limit:32,cpu_millis:500,
    };
    let environment = gateway.acquire(&owner, spec.clone()).await.unwrap();
    let lease = ExecutionLease {
        owner: owner.clone(),
        job_id: JobId::new("job").unwrap(),
        session_id: spec.session_id.clone(),
        attempt_id: ExecutionAttemptId::new("attempt").unwrap(),
        worker: WorkerInstanceId::new("worker").unwrap(),
        epoch: 1,
        checkpoint_version: 0,
        expires_at_ms: i64::MAX,
    };
    let request = CommandOperation {
        id: OperationId::new("command").unwrap(),
        invocation_id: InvocationId::new("invoke").unwrap(),
        environment_id: spec.id.clone(),
        expected_revision: 1,
        argv: vec!["sh".to_owned(), "-c".to_owned(), "printf once".to_owned()],
    };
    // Hold exactly the submit critical section while Docker still reports
    // "created" after durable admission but before the start response.
    let control = gateway.control(&owner, &spec.id).unwrap();
    let guard = control.lock().await;
    let operation = gateway
        .ledger
        .admit(
            &lease,
            &request,
            DockerGateway::container(&owner, &request.id),
        )
        .unwrap();
    gateway
        .prepare_container(&operation, &environment)
        .await
        .unwrap();
    assert!(gateway.ledger.begin_start(&owner, &request.id).unwrap());
    let inspector = {
        let gateway = gateway.clone();
        let owner = owner.clone();
        let id = request.id.clone();
        tokio::spawn(async move { gateway.inspect(&owner, &id).await })
    };
    let delivered = gateway.deliver_completions(&Sink, 128).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let prematurely_observed = inspector.is_finished();
    let phase = gateway
        .ledger
        .operation(&owner, &request.id)
        .unwrap()
        .receipt
        .phase;
    gateway
        .docker
        .json(
            Method::POST,
            &format!("/containers/{}/start", operation.container),
            None,
        )
        .await
        .unwrap();
    drop(guard);
    let _ = inspector.await.unwrap().unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if gateway.inspect(&owner, &request.id).await.unwrap().phase == OperationPhase::Completed {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    gateway.deliver_completions(&Sink, 128).await.unwrap();
    let current = gateway.get(&owner, &spec.id).await.unwrap();
    gateway
        .release(&owner, &spec.id, current.revision)
        .await
        .unwrap();
    assert_eq!(delivered, 0);
    assert_eq!(
        phase,
        OperationPhase::Starting,
        "a live start is not an uncertain abandoned operation"
    );
    assert!(
        !prematurely_observed,
        "inspection must wait for the active submitter's observation boundary"
    );
}
