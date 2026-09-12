#![cfg(target_os = "linux")]
use async_trait::async_trait;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use zuno_application::{
    ApplicationError, environment::*, runtime::ExecutionLease, workspace_merge::*,
};
use zuno_environment::DockerGateway;
use zuno_types::identity::*;

struct Authority(AtomicBool);
struct MergeSink {
    lose_first: AtomicBool,
    received: std::sync::Mutex<Vec<WorkspaceMergeCompletion>>,
}
#[async_trait]
impl WorkspaceMergeCompletionSink for MergeSink {
    async fn publish_merge(
        &self,
        completion: &WorkspaceMergeCompletion,
    ) -> Result<(), ApplicationError> {
        completion.validate()?;
        self.received.lock().unwrap().push(completion.clone());
        if self.lose_first.swap(false, Ordering::SeqCst) {
            Err(ApplicationError::Unavailable)
        } else {
            Ok(())
        }
    }
}
#[async_trait]
impl OperationAuthority for Authority {
    async fn authorize(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        _: &CommandOperation,
    ) -> Result<(), ApplicationError> {
        if lease.owner == environment.owner && lease.session_id == environment.spec.session_id {
            Ok(())
        } else {
            Err(ApplicationError::Forbidden)
        }
    }
}
#[async_trait]
impl WorkspaceMergeAuthority for Authority {
    async fn authorize_merge(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        _: &WorkspaceMergeOperation,
    ) -> Result<(), ApplicationError> {
        if self.0.load(Ordering::SeqCst)
            && lease.owner == environment.owner
            && lease.session_id == environment.spec.session_id
        {
            Ok(())
        } else {
            Err(ApplicationError::Forbidden)
        }
    }
}
async fn command(
    gateway: &DockerGateway,
    lease: &ExecutionLease,
    environment: &EnvironmentId,
    id: &str,
    script: &str,
) {
    let current = gateway.get(&lease.owner, environment).await.unwrap();
    let operation = CommandOperation {
        id: OperationId::new(id).unwrap(),
        invocation_id: InvocationId::new(id).unwrap(),
        environment_id: environment.clone(),
        expected_revision: current.revision,
        argv: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
    };
    gateway.submit(lease, operation.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let receipt = gateway.inspect(&lease.owner, &operation.id).await.unwrap();
            match receipt.phase {
                OperationPhase::Completed => {
                    if receipt.exit_code != Some(0) {
                        let output = gateway
                            .output(&lease.owner, &operation.id, OutputCursor::default(), 65536)
                            .await
                            .unwrap();
                        panic!(
                            "{id} failed with {:?}: {}",
                            receipt.exit_code,
                            String::from_utf8_lossy(
                                &output
                                    .chunks
                                    .into_iter()
                                    .flat_map(|chunk| chunk.bytes)
                                    .collect::<Vec<_>>()
                            )
                        );
                    }
                    break;
                }
                OperationPhase::Cancelled | OperationPhase::Uncertain => {
                    panic!("unexpected {id} receipt: {receipt:?}")
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "requires scripts/check_enterprise_docker.py and its isolated rootless daemon"]
async fn approved_copy_on_write_merge_preserves_parent_edits_and_recovers_atomic_publication() {
    let root = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = std::env::var("ZUNO_ROOTLESS_DOCKER_SOCKET").unwrap();
    let ledger = root.path().join("gateway.sqlite");
    let authority = Arc::new(Authority(AtomicBool::new(false)));
    let gateway = DockerGateway::connect(Path::new(&socket), &ledger, authority.clone())
        .await
        .unwrap();
    let owner = PrincipalKey {
        tenant_id: TenantId::new("native-merge").unwrap(),
        principal_id: PrincipalId::new("owner").unwrap(),
    };
    let spec=EnvironmentSpec {
        id:EnvironmentId::new("parent").unwrap(),session_id:SessionId::new("parent").unwrap(),
        image:"public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce".to_owned(),
        memory_bytes:134217728,pids_limit:64,cpu_millis:1000,
    };
    gateway.acquire(&owner, spec.clone()).await.unwrap();
    let lease = ExecutionLease {
        owner: owner.clone(),
        job_id: JobId::new("parent-job").unwrap(),
        session_id: spec.session_id.clone(),
        attempt_id: ExecutionAttemptId::new("attempt").unwrap(),
        worker: WorkerInstanceId::new("worker").unwrap(),
        epoch: 1,
        checkpoint_version: 0,
        expires_at_ms: i64::MAX,
    };
    command(
        &gateway,
        &lease,
        &spec.id,
        "seed",
        "set -eu; printf base > /workspace/one; printf base > /workspace/two; chmod 750 /workspace",
    )
    .await;
    let base = gateway.snapshot(&owner, &spec.id, 2).await.unwrap();
    let mut child = spec.clone();
    child.id = EnvironmentId::new("child").unwrap();
    child.session_id = SessionId::new("child").unwrap();
    gateway.fork(&owner, &base, child.clone()).await.unwrap();
    let child_lease = ExecutionLease {
        job_id: JobId::new("child-job").unwrap(),
        session_id: child.session_id.clone(),
        ..lease.clone()
    };
    command(&gateway,&child_lease,&child.id,"child-edit","set -eu; printf child > /workspace/two; printf '\\000\\377' > /workspace/binary; ln /workspace/binary /workspace/alias; chmod 640 /workspace/binary").await;
    command(
        &gateway,
        &lease,
        &spec.id,
        "parent-edit",
        "printf parent > /workspace/one",
    )
    .await;
    let operation = gateway
        .preview_workspace_merge(
            &lease,
            WorkspaceMergePreviewRequest {
                id: OperationId::new("merge").unwrap(),
                invocation_id: InvocationId::new("merge-call").unwrap(),
                child_job_id: child_lease.job_id.clone(),
                environment_id: spec.id.clone(),
                source_id: child.id.clone(),
                base: base.clone(),
            },
        )
        .await
        .unwrap();
    for (role, snapshot) in [
        ("base", &operation.base),
        ("parent", &operation.parent),
        ("child", &operation.child),
    ] {
        let path = ledger.with_extension("snapshots").join(format!(
            "{}.tar",
            zuno_orchestration::sha256_json(&serde_json::json!([owner, snapshot.id]))
        ));
        let tree = zuno_environment::workspace_merge::SnapshotTree::read(
            &path,
            &snapshot.sha256,
            snapshot.bytes,
        )
        .unwrap();
        assert!(
            matches!(
                tree.entries().get(&WorkspacePath::root()),
                Some(WorkspaceEntry::Directory { mode: 0o750, .. })
            ),
            "{role} snapshot changed workspace-root metadata: {:?}",
            tree.entries().get(&WorkspacePath::root())
        );
    }
    assert!(
        operation
            .plan
            .changes
            .iter()
            .all(|change| change.choice != MergeChoice::Conflict)
    );
    assert!(matches!(
        gateway
            .merge_workspace(&lease, &operation, authority.as_ref())
            .await,
        Err(ApplicationError::Forbidden)
    ));
    assert_eq!(gateway.get(&owner, &spec.id).await.unwrap().revision, 3);
    authority.0.store(true, Ordering::SeqCst);
    let injection = rusqlite::Connection::open(&ledger).unwrap();
    injection.execute_batch("CREATE TRIGGER refuse_merge BEFORE INSERT ON environment_volume BEGIN SELECT RAISE(ABORT,'injected publication failure'); END;").unwrap();
    assert!(
        gateway
            .merge_workspace(&lease, &operation, authority.as_ref())
            .await
            .is_err()
    );
    assert_eq!(gateway.get(&owner, &spec.id).await.unwrap().revision, 3);
    let before = gateway.get(&owner, &spec.id).await.unwrap();
    assert_eq!(before.revision, operation.expected_revision);
    injection
        .execute_batch("DROP TRIGGER refuse_merge")
        .unwrap();
    drop(injection);
    drop(gateway);
    let gateway = DockerGateway::connect(Path::new(&socket), &ledger, authority.clone())
        .await
        .unwrap();
    let receipt = gateway
        .merge_workspace(&lease, &operation, authority.as_ref())
        .await
        .unwrap();
    assert_eq!(receipt.state, WorkspaceMergeState::Committed);
    assert_eq!(receipt.revision, 4);
    command(&gateway,&lease,&spec.id,"verify-merged","set -eux; cat /workspace/one /workspace/two; stat -c '%a %i %n' /workspace /workspace/alias /workspace/binary; test \"$(cat /workspace/one)\" = parent; test \"$(cat /workspace/two)\" = child; test \"$(stat -c %a /workspace)\" = 750; test \"$(stat -c %a /workspace/alias)\" = 640; test \"$(stat -c %i /workspace/alias)\" = \"$(stat -c %i /workspace/binary)\"; test \"$(wc -c < /workspace/binary)\" = 2").await;
    assert_eq!(
        gateway
            .merge_workspace(&lease, &operation, authority.as_ref())
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        gateway.get(&owner, &spec.id).await.unwrap().revision,
        5,
        "receipt replay must not switch the volume or revision twice"
    );
    let foreign = PrincipalKey {
        principal_id: PrincipalId::new("foreign").unwrap(),
        ..owner.clone()
    };
    assert!(
        gateway
            .inspect_workspace_merge(&foreign, &operation.id)
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires scripts/check_enterprise_docker.py and its isolated rootless daemon"]
async fn background_merge_recovers_admission_and_lost_completion_ack_without_republication() {
    let root = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = std::env::var("ZUNO_ROOTLESS_DOCKER_SOCKET").unwrap();
    let ledger = root.path().join("gateway.sqlite");
    let authority = Arc::new(Authority(AtomicBool::new(true)));
    let gateway = DockerGateway::connect(Path::new(&socket), &ledger, authority.clone())
        .await
        .unwrap();
    let owner = PrincipalKey {
        tenant_id: TenantId::new("background-merge").unwrap(),
        principal_id: PrincipalId::new("owner").unwrap(),
    };
    let spec=EnvironmentSpec {
        id:EnvironmentId::new("parent").unwrap(),session_id:SessionId::new("parent").unwrap(),
        image:"public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce".to_owned(),
        memory_bytes:134217728,pids_limit:64,cpu_millis:1000,
    };
    gateway.acquire(&owner, spec.clone()).await.unwrap();
    let lease = ExecutionLease {
        owner: owner.clone(),
        job_id: JobId::new("parent-job").unwrap(),
        session_id: spec.session_id.clone(),
        attempt_id: ExecutionAttemptId::new("attempt").unwrap(),
        worker: WorkerInstanceId::new("worker").unwrap(),
        epoch: 1,
        checkpoint_version: 0,
        expires_at_ms: i64::MAX,
    };
    command(
        &gateway,
        &lease,
        &spec.id,
        "seed",
        "printf base > /workspace/file",
    )
    .await;
    let base = gateway.snapshot(&owner, &spec.id, 2).await.unwrap();
    let mut child = spec.clone();
    child.id = EnvironmentId::new("child").unwrap();
    child.session_id = SessionId::new("child").unwrap();
    gateway.fork(&owner, &base, child.clone()).await.unwrap();
    let child_lease = ExecutionLease {
        job_id: JobId::new("child-job").unwrap(),
        session_id: child.session_id.clone(),
        ..lease.clone()
    };
    command(
        &gateway,
        &child_lease,
        &child.id,
        "child-write",
        "printf result > /workspace/file",
    )
    .await;
    let operation = gateway
        .preview_workspace_merge(
            &lease,
            WorkspaceMergePreviewRequest {
                id: OperationId::new("merge").unwrap(),
                invocation_id: InvocationId::new("merge").unwrap(),
                child_job_id: child_lease.job_id,
                environment_id: spec.id.clone(),
                source_id: child.id,
                base,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        gateway
            .admit_workspace_merge(&lease, &operation, authority.as_ref())
            .await
            .unwrap()
            .state,
        WorkspaceMergeState::Preparing
    );
    assert_eq!(
        gateway.get(&owner, &spec.id).await.unwrap().revision,
        2,
        "admission cannot publish the candidate"
    );
    drop(gateway);
    let gateway = Arc::new(
        DockerGateway::connect(Path::new(&socket), &ledger, authority.clone())
            .await
            .unwrap(),
    );
    let sink = Arc::new(MergeSink {
        lose_first: AtomicBool::new(true),
        received: std::sync::Mutex::new(Vec::new()),
    });
    let executor = zuno_environment::MergeExecutor::new(gateway.clone(), sink.clone(), 1).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        executor.advance().unwrap();
        if !sink.received.lock().unwrap().is_empty() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    executor.drain(Duration::from_secs(5)).await;
    assert_eq!(gateway.get(&owner, &spec.id).await.unwrap().revision, 3);
    drop(executor);
    drop(gateway);
    let gateway = Arc::new(
        DockerGateway::connect(Path::new(&socket), &ledger, authority)
            .await
            .unwrap(),
    );
    let executor = zuno_environment::MergeExecutor::new(gateway.clone(), sink.clone(), 1).unwrap();
    loop {
        executor.advance().unwrap();
        if sink.received.lock().unwrap().len() >= 2 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    executor.drain(Duration::from_secs(5)).await;
    {
        let received = sink.received.lock().unwrap();
        assert_eq!(
            received[0], received[1],
            "lost delivery acknowledgement reuses the exact committed receipt"
        );
    }
    assert_eq!(
        gateway.get(&owner, &spec.id).await.unwrap().revision,
        3,
        "delivery retry cannot publish another volume"
    );
}
