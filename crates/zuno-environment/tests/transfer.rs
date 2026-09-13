//! Provider fault tests. Enterprise service authentication is covered separately
//! by the real control-plane/peer-gateway process fixture.
use std::{path::Path, sync::Arc, time::Duration};
use zuno_application::{
    ApplicationError, child::ChildWorkspaceAssignment, environment::*, runtime::ExecutionLease,
    workspace_transfer::*,
};
use zuno_environment::DockerGateway;
use zuno_types::identity::*;

struct Authority;
#[async_trait::async_trait]
impl OperationAuthority for Authority {
    async fn authorize(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        _: &CommandOperation,
    ) -> Result<(), ApplicationError> {
        if lease.owner != environment.owner || lease.session_id != environment.spec.session_id {
            return Err(ApplicationError::Forbidden);
        }
        Ok(())
    }
}
struct Sink;
#[async_trait::async_trait]
impl OperationCompletionSink for Sink {
    async fn publish(&self, value: &OperationCompletion) -> Result<(), ApplicationError> {
        value.validate()
    }
}
fn spec(name: &str) -> EnvironmentSpec {
    EnvironmentSpec {
        id: EnvironmentId::new(name).unwrap(),
        session_id: SessionId::new(name).unwrap(),
        image: "public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce".to_owned(),
        memory_bytes: 67108864, pids_limit: 32, cpu_millis: 500,
    }
}
fn lease(owner: &PrincipalKey, spec: &EnvironmentSpec) -> ExecutionLease {
    ExecutionLease {
        owner: owner.clone(),
        job_id: JobId::new(spec.id.as_str()).unwrap(),
        session_id: spec.session_id.clone(),
        attempt_id: ExecutionAttemptId::new("attempt").unwrap(),
        worker: WorkerInstanceId::new("worker").unwrap(),
        epoch: 1,
        checkpoint_version: 0,
        expires_at_ms: i64::MAX,
    }
}
async fn command(gateway: &DockerGateway, lease: &ExecutionLease, id: &str, script: &str) {
    let environment = gateway
        .get(
            &lease.owner,
            &EnvironmentId::new(lease.session_id.as_str()).unwrap(),
        )
        .await
        .unwrap();
    let operation = CommandOperation {
        id: OperationId::new(id).unwrap(),
        invocation_id: InvocationId::new(id).unwrap(),
        environment_id: environment.spec.id,
        expected_revision: environment.revision,
        argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
    };
    gateway.submit(lease, operation.clone()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let receipt = gateway.inspect(&lease.owner, &operation.id).await.unwrap();
        if receipt.phase == OperationPhase::Completed {
            assert_eq!(receipt.exit_code, Some(0), "{script}");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    gateway.deliver_completions(&Sink, 16).await.unwrap();
}

#[tokio::test]
#[ignore = "requires scripts/check_enterprise_docker.py and its isolated rootless daemon"]
async fn snapshot_retry_after_truncation_and_gateway_restart_never_overwrites_a_workspace() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let directory = tempfile::tempdir().unwrap();
    let source_dir = directory.path().join("source");
    let target_dir = directory.path().join("target");
    for path in [&source_dir, &target_dir] {
        std::fs::create_dir(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let socket = std::env::var("ZUNO_ROOTLESS_DOCKER_SOCKET").unwrap();
    let source = DockerGateway::connect(
        Path::new(&socket),
        &source_dir.join("gateway.sqlite"),
        Arc::new(Authority),
    )
    .await
    .unwrap();
    let target = Arc::new(
        DockerGateway::connect(
            Path::new(&socket),
            &target_dir.join("gateway.sqlite"),
            Arc::new(Authority),
        )
        .await
        .unwrap(),
    );
    let owner = PrincipalKey {
        tenant_id: TenantId::new("snapshot-faults").unwrap(),
        principal_id: PrincipalId::new("alice").unwrap(),
    };
    let source_spec = spec("snapshot-source");
    let target_spec = spec("snapshot-child");
    let source_lease = lease(&owner, &source_spec);
    let target_lease = lease(&owner, &target_spec);
    source.acquire(&owner, source_spec.clone()).await.unwrap();
    command(&source, &source_lease, "seed",
        "printf original > /workspace/file; chmod 600 /workspace/file; chmod 750 /workspace; ln /workspace/file /workspace/hard; ln -s file /workspace/link").await;
    let assigned = SnapshotTransferAssignment {
        request: SnapshotTransferRequest {
            lease: source_lease.clone(),
            purpose: SnapshotTransferPurpose::ChildWorkspace {
                child_job_id: target_lease.job_id.clone(),
            },
        },
        source: zuno_application::environment::wire::GatewayAssignment {
            gateway_id: GatewayId::new("source").unwrap(),
            endpoint: "https://source.example/".to_owned(),
            environment: source_spec.clone(),
        },
        target_gateway_id: GatewayId::new("target").unwrap(),
        existing_source: true,
    };
    let (snapshot, file) = source.export_snapshot(&assigned).await.unwrap();
    assert!(
        target
            .receive_snapshot(&assigned, &snapshot, file.take(snapshot.bytes - 1))
            .await
            .is_err()
    );
    assert!(!target.has_snapshot(&owner, &snapshot).await.unwrap());

    let (mut writer, reader) = tokio::io::duplex(16);
    let cancelled = {
        let target = target.clone();
        let assigned = assigned.clone();
        let snapshot = snapshot.clone();
        tokio::spawn(async move { target.receive_snapshot(&assigned, &snapshot, reader).await })
    };
    writer.write_all(&[0u8; 64]).await.unwrap();
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    assert!(!target.has_snapshot(&owner, &snapshot).await.unwrap());

    // Advancing the source after a lost transfer response does not change the
    // stable fork snapshot or replace the bytes offered to the child.
    command(
        &source,
        &source_lease,
        "source-change",
        "printf changed > /workspace/file",
    )
    .await;
    let (retried, file) = source.export_snapshot(&assigned).await.unwrap();
    assert_eq!(retried, snapshot);
    target
        .receive_snapshot(&assigned, &snapshot, file)
        .await
        .unwrap();
    let (_, repeated) = source.export_snapshot(&assigned).await.unwrap();
    target
        .receive_snapshot(&assigned, &snapshot, repeated)
        .await
        .unwrap();
    assert!(target.has_snapshot(&owner, &snapshot).await.unwrap());
    let mut foreign = owner.clone();
    foreign.principal_id = PrincipalId::new("bob").unwrap();
    assert!(!target.has_snapshot(&foreign, &snapshot).await.unwrap());
    assert!(
        target
            .receive_snapshot(
                &assigned,
                &snapshot,
                std::io::Cursor::new(vec![0u8; snapshot.bytes as usize])
            )
            .await
            .is_err()
    );
    assert!(target.has_snapshot(&owner, &snapshot).await.unwrap());
    let child = ChildWorkspaceAssignment {
        child_job_id: target_lease.job_id.clone(),
        gateway_id: assigned.target_gateway_id.clone(),
        parent_gateway_id: Some(assigned.source.gateway_id.clone()),
        parent: source_spec.clone(),
        target: target_spec.clone(),
        resume: false,
    };
    let first = target
        .fork_transferred_workspace(&owner, &child, &snapshot)
        .await
        .unwrap();
    drop(target);
    let target = DockerGateway::connect(
        Path::new(&socket),
        &target_dir.join("gateway.sqlite"),
        Arc::new(Authority),
    )
    .await
    .unwrap();
    assert_eq!(
        target
            .fork_transferred_workspace(&owner, &child, &snapshot)
            .await
            .unwrap(),
        first,
        "lost acknowledgement and restart must return the original publication"
    );
    command(&target, &target_lease, "verify-child",
        "test \"$(cat /workspace/file)\" = original; test \"$(stat -c %a /workspace)\" = 750; test \"$(stat -c %a /workspace/file)\" = 600; test \"$(stat -c %i /workspace/file)\" = \"$(stat -c %i /workspace/hard)\"; test \"$(readlink /workspace/link)\" = file; printf child > /workspace/file").await;
    assert!(matches!(
        target
            .fork_transferred_workspace(&owner, &child, &snapshot)
            .await,
        Err(ApplicationError::Conflict)
    ));
    command(
        &target,
        &target_lease,
        "verify-preserved",
        "test \"$(cat /workspace/file)\" = child",
    )
    .await;
    for (gateway, spec) in [(&source, &source_spec), (&target, &target_spec)] {
        let environment = gateway.get(&owner, &spec.id).await.unwrap();
        gateway
            .release(&owner, &spec.id, environment.revision)
            .await
            .unwrap();
    }
}
