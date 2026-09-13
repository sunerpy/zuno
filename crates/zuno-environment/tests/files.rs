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
    ApplicationError, environment::*, runtime::ExecutionLease, workspace_files::*,
    workspace_merge::WorkspacePath,
};
use zuno_environment::DockerGateway;
use zuno_types::{activity::Counter, identity::*};

struct Authority(AtomicBool);
struct Sink;
#[async_trait]
impl OperationCompletionSink for Sink {
    async fn publish(&self, completion: &OperationCompletion) -> Result<(), ApplicationError> {
        completion.validate()
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
impl WorkspaceFileAuthority for Authority {
    async fn authorize_files(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        operation: &WorkspaceFileOperation,
    ) -> Result<(), ApplicationError> {
        if self.0.load(Ordering::SeqCst)
            && lease.owner == environment.owner
            && lease.session_id == environment.spec.session_id
            && operation.expected_revision == environment.revision
        {
            Ok(())
        } else {
            Err(ApplicationError::Forbidden)
        }
    }
}
async fn command(gateway: &DockerGateway, lease: &ExecutionLease, id: &str, script: &str) {
    let current = gateway
        .get(&lease.owner, &EnvironmentId::new("files").unwrap())
        .await
        .unwrap();
    let operation = CommandOperation {
        id: OperationId::new(id).unwrap(),
        invocation_id: InvocationId::new(id).unwrap(),
        environment_id: current.spec.id,
        expected_revision: current.revision,
        argv: vec!["sh".into(), "-c".into(), script.into()],
    };
    gateway.submit(lease, operation.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let receipt = gateway.inspect(&lease.owner, &operation.id).await.unwrap();
            if receipt.phase == OperationPhase::Completed {
                assert_eq!(receipt.exit_code, Some(0));
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires scripts/check_enterprise_docker.py and its isolated rootless daemon"]
async fn workspace_queries_use_immutable_owned_snapshots_and_current_authority() {
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
        tenant_id: TenantId::new("file-fixture").unwrap(),
        principal_id: PrincipalId::new("owner").unwrap(),
    };
    let spec=EnvironmentSpec {
        id:EnvironmentId::new("files").unwrap(),session_id:SessionId::new("files").unwrap(),
        image:"public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce".to_owned(),
        memory_bytes:134217728,pids_limit:64,cpu_millis:1000,
    };
    gateway.acquire(&owner, spec.clone()).await.unwrap();
    let lease = ExecutionLease {
        owner: owner.clone(),
        job_id: JobId::new("files-job").unwrap(),
        session_id: spec.session_id.clone(),
        attempt_id: ExecutionAttemptId::new("attempt").unwrap(),
        worker: WorkerInstanceId::new("worker").unwrap(),
        epoch: 1,
        checkpoint_version: 0,
        expires_at_ms: i64::MAX,
    };
    command(&gateway,&lease,"seed","mkdir -p src; printf 'alpha\\n中文\\nneedle\\n' > src/data; ln src/data alias; ln -s src/data link; printf '\\000binary' > blob; head -c 5000 /dev/zero | tr '\\000' x > long; printf 'needle\\n' >> long").await;
    let environment = gateway.get(&owner, &spec.id).await.unwrap();
    let operation = WorkspaceFileOperation {
        id: OperationId::new("read").unwrap(),
        invocation_id: InvocationId::new("read").unwrap(),
        environment_id: spec.id.clone(),
        expected_revision: environment.revision,
        query: WorkspaceFileQuery::Read {
            path: WorkspacePath::new("src/data").unwrap(),
            offset: Counter(0),
            maximum_bytes: 9,
        },
    };
    let read = gateway
        .query_files(&lease, &operation, &*authority)
        .await
        .unwrap();
    read.validate_for(&operation).unwrap();
    let WorkspaceFileResult::Read {
        text: Some(text),
        next_offset,
        ..
    } = &read.result
    else {
        panic!("text");
    };
    assert_eq!(text, "alpha\n中");
    assert_eq!(next_offset.0, 9);
    command(&gateway, &lease, "change", "printf changed > src/data").await;
    let repeated = gateway
        .query_files(&lease, &operation, &*authority)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&read).unwrap(),
        serde_json::to_value(&repeated).unwrap(),
        "re-reading the same approved revision cannot observe later writes"
    );
    let mut listing = operation.clone();
    listing.id = OperationId::new("list").unwrap();
    listing.query = WorkspaceFileQuery::List {
        path: WorkspacePath::root(),
        after: None,
        limit: 2,
    };
    let page = gateway
        .query_files(&lease, &listing, &*authority)
        .await
        .unwrap();
    page.validate_for(&listing).unwrap();
    let WorkspaceFileResult::List {
        entries,
        after: Some(after),
    } = page.result
    else {
        panic!("page");
    };
    assert_eq!(entries.len(), 2);
    listing.query = WorkspaceFileQuery::List {
        path: WorkspacePath::root(),
        after: Some(after),
        limit: 200,
    };
    let page = gateway
        .query_files(&lease, &listing, &*authority)
        .await
        .unwrap();
    page.validate_for(&listing).unwrap();
    let mut search = operation.clone();
    search.id = OperationId::new("search").unwrap();
    search.query = WorkspaceFileQuery::Search {
        path: WorkspacePath::root(),
        text: "needle".to_owned(),
        limit: 100,
    };
    let results = gateway
        .query_files(&lease, &search, &*authority)
        .await
        .unwrap();
    results.validate_for(&search).unwrap();
    let WorkspaceFileResult::Search {
        matches,
        skipped_files,
        ..
    } = results.result
    else {
        panic!("matches");
    };
    assert!(matches.iter().any(|value| value.path.as_str() == "long"
        && value.truncated
        && value.text.contains("needle")));
    assert!(
        !matches.iter().any(|value| value.path.as_str() == "link"),
        "symbolic links are not followed"
    );
    assert_eq!(skipped_files.0, 1);
    authority.0.store(false, Ordering::SeqCst);
    assert!(
        gateway
            .query_files(&lease, &operation, &*authority)
            .await
            .is_err(),
        "cached data still needs current authority"
    );
    authority.0.store(true, Ordering::SeqCst);
    let mut foreign = lease.clone();
    foreign.owner.principal_id = PrincipalId::new("other").unwrap();
    assert!(
        gateway
            .query_files(&foreign, &operation, &*authority)
            .await
            .is_err()
    );
    drop(gateway);
    let restored = DockerGateway::connect(Path::new(&socket), &ledger, authority.clone())
        .await
        .unwrap();
    let replay = restored
        .query_files(&lease, &operation, &*authority)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(read).unwrap(),
        serde_json::to_value(replay).unwrap(),
        "snapshot receipt survives gateway restart"
    );
    restored.deliver_completions(&Sink, 16).await.unwrap();
    let current = restored.get(&owner, &spec.id).await.unwrap();
    restored
        .release(&owner, &spec.id, current.revision)
        .await
        .unwrap();
}
