#![cfg(target_os = "linux")]
use async_trait::async_trait;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use zuno_application::{
    ApplicationError, environment::*, runtime::ExecutionLease, workspace_edit::*,
    workspace_files::*, workspace_merge::WorkspacePath,
};
use zuno_environment::{DockerGateway, EditExecutor};
use zuno_types::{activity::Counter, identity::*};

struct Authority(AtomicBool);
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
impl WorkspaceFileAuthority for Authority {
    async fn authorize_files(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        _: &WorkspaceFileOperation,
    ) -> Result<(), ApplicationError> {
        if lease.owner == environment.owner && lease.session_id == environment.spec.session_id {
            Ok(())
        } else {
            Err(ApplicationError::Forbidden)
        }
    }
}
#[async_trait]
impl WorkspaceEditAuthority for Authority {
    async fn authorize_edit(
        &self,
        admission: &WorkspaceEditAdmission,
    ) -> Result<(), ApplicationError> {
        admission.validate()?;
        if self.0.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(ApplicationError::Forbidden)
        }
    }
}
struct Sink {
    lose: AtomicBool,
    receipts: Mutex<Vec<WorkspaceEditCompletion>>,
}
#[async_trait]
impl WorkspaceEditCompletionSink for Sink {
    async fn publish_edit(&self, value: &WorkspaceEditCompletion) -> Result<(), ApplicationError> {
        value.validate()?;
        self.receipts.lock().unwrap().push(value.clone());
        if self.lose.swap(false, Ordering::SeqCst) {
            Err(ApplicationError::Unavailable)
        } else {
            Ok(())
        }
    }
}
async fn read(
    gateway: &DockerGateway,
    lease: &ExecutionLease,
    id: &EnvironmentId,
    path: &str,
    authority: &Authority,
) -> String {
    let environment = gateway.get(&lease.owner, id).await.unwrap();
    let value = gateway
        .query_files(
            lease,
            &WorkspaceFileOperation {
                id: OperationId::new(format!("read-{}", environment.revision)).unwrap(),
                invocation_id: InvocationId::new("read").unwrap(),
                environment_id: id.clone(),
                expected_revision: environment.revision,
                query: WorkspaceFileQuery::Read {
                    path: WorkspacePath::new(path).unwrap(),
                    offset: Counter(0),
                    maximum_bytes: 4096,
                },
            },
            authority,
        )
        .await
        .unwrap();
    let WorkspaceFileResult::Read {
        text: Some(text), ..
    } = value.result
    else {
        panic!("file");
    };
    text
}

#[tokio::test]
#[ignore = "requires scripts/check_enterprise_docker.py and its isolated rootless daemon"]
async fn approved_file_edit_publishes_atomically_and_lost_ack_does_not_repeat_it() {
    let root = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let authority = Arc::new(Authority(AtomicBool::new(false)));
    let socket = std::env::var("ZUNO_ROOTLESS_DOCKER_SOCKET").unwrap();
    let gateway = Arc::new(
        DockerGateway::connect(
            Path::new(&socket),
            &root.path().join("gateway.sqlite"),
            authority.clone(),
        )
        .await
        .unwrap(),
    );
    let owner = PrincipalKey {
        tenant_id: TenantId::new("native-edit").unwrap(),
        principal_id: PrincipalId::new("owner").unwrap(),
    };
    let spec=EnvironmentSpec {
        id:EnvironmentId::new("edit").unwrap(),session_id:SessionId::new("edit").unwrap(),
        image:"public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce".to_owned(),
        memory_bytes:134217728,pids_limit:64,cpu_millis:1000,
    };
    gateway.acquire(&owner, spec.clone()).await.unwrap();
    let lease = ExecutionLease {
        owner: owner.clone(),
        job_id: JobId::new("edit-job").unwrap(),
        session_id: spec.session_id.clone(),
        attempt_id: ExecutionAttemptId::new("attempt").unwrap(),
        worker: WorkerInstanceId::new("worker").unwrap(),
        epoch: 1,
        checkpoint_version: 0,
        expires_at_ms: i64::MAX,
    };
    let command = CommandOperation {
        id: OperationId::new("seed").unwrap(),
        invocation_id: InvocationId::new("seed").unwrap(),
        environment_id: spec.id.clone(),
        expected_revision: 1,
        argv: vec![
            "sh".into(),
            "-c".into(),
            "printf before > change; printf keep > keep".into(),
        ],
    };
    gateway.submit(&lease, command.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let receipt = gateway.inspect(&owner, &command.id).await.unwrap();
            if receipt.phase == OperationPhase::Completed {
                assert_eq!(receipt.exit_code, Some(0));
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let operation = WorkspaceEditOperation {
        id: OperationId::new("edit").unwrap(),
        invocation_id: InvocationId::new("edit").unwrap(),
        environment_id: spec.id.clone(),
        expected_revision: 2,
        edits: vec![WorkspaceFileEdit {
            path: WorkspacePath::new("change").unwrap(),
            expected: FileExpectation::File {
                sha256: zuno_orchestration::sha256_text("before"),
            },
            content: Some("after".to_owned()),
        }],
    };
    let admission = gateway
        .preview_workspace_edit(
            GatewayId::new("gateway").unwrap(),
            &lease,
            operation.clone(),
        )
        .await
        .unwrap();
    assert_eq!(admission.review[0].before.as_deref(), Some("before"));
    assert_eq!(
        read(&gateway, &lease, &spec.id, "change", &authority).await,
        "before"
    );
    assert!(
        gateway
            .admit_workspace_edit(&admission, &*authority)
            .await
            .is_err()
    );
    authority.0.store(true, Ordering::SeqCst);
    let prepared = gateway
        .admit_workspace_edit(&admission, &*authority)
        .await
        .unwrap();
    assert_eq!(prepared.state, WorkspaceEditState::Preparing);
    let sink = Arc::new(Sink {
        lose: AtomicBool::new(true),
        receipts: Mutex::new(Vec::new()),
    });
    let executor = EditExecutor::new(gateway.clone(), sink.clone(), 2).unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            executor.advance().unwrap();
            if sink.receipts.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(gateway.get(&owner, &spec.id).await.unwrap().revision, 3);
    assert_eq!(
        read(&gateway, &lease, &spec.id, "change", &authority).await,
        "after"
    );
    assert_eq!(
        read(&gateway, &lease, &spec.id, "keep", &authority).await,
        "keep"
    );
    let replay = gateway
        .admit_workspace_edit(&admission, &*authority)
        .await
        .unwrap();
    assert_eq!(replay.state, WorkspaceEditState::Committed);
    assert_eq!(gateway.get(&owner, &spec.id).await.unwrap().revision, 3);
    executor.drain(Duration::from_secs(2)).await;
}
