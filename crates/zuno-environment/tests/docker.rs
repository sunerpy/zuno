#![cfg(target_os = "linux")]
use async_trait::async_trait;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use zuno_application::runtime::ExecutionLease;
use zuno_application::{ApplicationError, environment::*};
use zuno_environment::DockerGateway;
use zuno_types::identity::*;

struct Authority(AtomicBool);
#[derive(Default)]
struct Sink(std::sync::Mutex<Vec<OperationCompletion>>);
#[async_trait]
impl OperationCompletionSink for Sink {
    async fn publish(&self, completion: &OperationCompletion) -> Result<(), ApplicationError> {
        completion.validate()?;
        self.0.lock().unwrap().push(completion.clone());
        Ok(())
    }
}
#[async_trait]
impl OperationAuthority for Authority {
    async fn authorize(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        _operation: &CommandOperation,
    ) -> Result<(), ApplicationError> {
        if !self.0.load(Ordering::SeqCst)
            || lease.owner != environment.owner
            || lease.session_id != environment.spec.session_id
        {
            return Err(ApplicationError::Forbidden);
        }
        Ok(())
    }
}

async fn terminal(
    gateway: &DockerGateway,
    owner: &PrincipalKey,
    id: &OperationId,
) -> OperationReceipt {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let receipt = gateway.inspect(owner, id).await.unwrap();
            if matches!(
                receipt.phase,
                OperationPhase::Completed | OperationPhase::Cancelled | OperationPhase::Uncertain
            ) {
                return receipt;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "set ZUNO_ROOTLESS_DOCKER_SOCKET to an isolated rootless Docker daemon"]
async fn rootless_gateway_reopens_receipts_without_replaying_the_command() {
    let sink = Sink::default();
    let socket = std::env::var("ZUNO_ROOTLESS_DOCKER_SOCKET").unwrap();
    let directory = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let ledger = directory.path().join("gateway.sqlite");
    let owner = PrincipalKey {
        tenant_id: TenantId::new("gateway-test").unwrap(),
        principal_id: PrincipalId::new(format!("owner-{}", std::process::id())).unwrap(),
    };
    let authority = Arc::new(Authority(AtomicBool::new(true)));
    let gateway = DockerGateway::connect(std::path::Path::new(&socket), &ledger, authority.clone())
        .await
        .unwrap();
    let spec=EnvironmentSpec {
        id:EnvironmentId::new(format!("environment-{}",std::process::id())).unwrap(),session_id:SessionId::new("session").unwrap(),
        image:"public.ecr.aws/docker/library/alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce".to_owned(),
        memory_bytes:128*1024*1024,pids_limit:64,cpu_millis:1000,
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
    let request=CommandOperation {
        id:OperationId::new("write-once").unwrap(),invocation_id:InvocationId::new("invocation").unwrap(),
        environment_id:spec.id.clone(),expected_revision:environment.revision,
        argv:vec!["sh".to_owned(),"-c".to_owned(),
            "set -eu; test ! -S /var/run/docker.sock; test -z \"${DATABASE_URL:-}\"; test -z \"${OPENAI_API_KEY:-}\"; \
             if touch /must-not-write 2>/dev/null; then exit 20; fi; \
             test \"$(ls /sys/class/net)\" = lo; test \"$(cat /sys/fs/cgroup/memory.max)\" = 134217728; \
             test \"$(cat /sys/fs/cgroup/pids.max)\" = 64; \
             printf once >> /workspace/counter; cat /workspace/counter".to_owned()],
    };
    gateway.submit(&lease, request.clone()).await.unwrap();
    let receipt = terminal(&gateway, &owner, &request.id).await;
    assert_eq!(receipt.phase, OperationPhase::Completed);
    assert_eq!(receipt.exit_code, Some(0));
    drop(gateway);
    let gateway = DockerGateway::connect(std::path::Path::new(&socket), &ledger, authority.clone())
        .await
        .unwrap();
    assert_eq!(gateway.inspect(&owner, &request.id).await.unwrap(), receipt);
    assert_eq!(
        gateway.submit(&lease, request.clone()).await.unwrap(),
        receipt
    );
    let first = gateway
        .output(&owner, &request.id, OutputCursor::default(), 2)
        .await
        .unwrap();
    assert!(!first.end_of_available);
    let last = gateway
        .output(&owner, &request.id, first.next.clone(), 100)
        .await
        .unwrap();
    assert!(last.end_of_available);
    let bytes = first
        .chunks
        .into_iter()
        .chain(last.chunks)
        .flat_map(|part| part.bytes)
        .collect::<Vec<_>>();
    assert_eq!(bytes, b"once");
    let mut wrong = first.next;
    wrong.prefix_sha256 = Some("0".repeat(64));
    assert!(
        gateway
            .output(&owner, &request.id, wrong, 100)
            .await
            .is_err()
    );
    let mut changed = request.clone();
    changed.argv.push("different".to_owned());
    assert!(gateway.submit(&lease, changed).await.is_err());
    let foreign = PrincipalKey {
        tenant_id: owner.tenant_id.clone(),
        principal_id: PrincipalId::new("foreign").unwrap(),
    };
    assert!(gateway.inspect(&foreign, &request.id).await.is_err());
    authority.0.store(false, Ordering::SeqCst);
    assert!(gateway.submit(&lease, request.clone()).await.is_err());
    authority.0.store(true, Ordering::SeqCst);
    let revision = gateway.get(&owner, &spec.id).await.unwrap().revision;
    assert_eq!(revision, 2);
    let snapshot = gateway.snapshot(&owner, &spec.id, revision).await.unwrap();
    let mut interrupted_target = spec.clone();
    interrupted_target.id =
        EnvironmentId::new(format!("interrupted-{}", std::process::id())).unwrap();
    let injection = rusqlite::Connection::open(&ledger).unwrap();
    injection
        .execute_batch(&format!(
            "CREATE TRIGGER refuse_fork_publication BEFORE INSERT ON environment WHEN NEW.id='{}'
         BEGIN SELECT RAISE(ABORT,'injected fork publication failure'); END;",
            interrupted_target.id.as_str(),
        ))
        .unwrap();
    assert!(
        gateway
            .fork(&owner, &snapshot, interrupted_target.clone())
            .await
            .is_err()
    );
    assert!(
        gateway.get(&owner, &interrupted_target.id).await.is_err(),
        "an incomplete fork is not an executable environment"
    );
    assert!(
        gateway
            .acquire(&owner, interrupted_target.clone())
            .await
            .is_err(),
        "acquire cannot publish an incomplete copied volume"
    );
    injection
        .execute_batch("DROP TRIGGER refuse_fork_publication")
        .unwrap();
    drop(injection);
    drop(gateway);
    let gateway = DockerGateway::connect(std::path::Path::new(&socket), &ledger, authority.clone())
        .await
        .unwrap();
    let recovered = gateway
        .fork(&owner, &snapshot, interrupted_target.clone())
        .await
        .unwrap();
    assert_eq!(recovered.revision, 1);
    gateway
        .release(&owner, &interrupted_target.id, 1)
        .await
        .unwrap();

    let other_ledger = directory.path().join("other-owner-ledger.sqlite");
    let other = DockerGateway::connect(
        std::path::Path::new(&socket),
        &other_ledger,
        authority.clone(),
    )
    .await
    .unwrap();
    let mut reserved_target = spec.clone();
    reserved_target.id =
        EnvironmentId::new(format!("other-ledger-{}", std::process::id())).unwrap();
    let preserved = other
        .acquire(&owner, reserved_target.clone())
        .await
        .unwrap();
    assert!(
        gateway
            .fork(&owner, &snapshot, reserved_target.clone())
            .await
            .is_err(),
        "another ledger's volume cannot be adopted for destructive fork recovery"
    );
    assert_eq!(
        other.get(&owner, &reserved_target.id).await.unwrap(),
        preserved
    );
    other
        .release(&owner, &reserved_target.id, preserved.revision)
        .await
        .unwrap();
    let mut branch = spec.clone();
    branch.id = EnvironmentId::new(format!("branch-{}", std::process::id())).unwrap();
    let forked = gateway
        .fork(&owner, &snapshot, branch.clone())
        .await
        .unwrap();
    let branch_operation = CommandOperation {
        id: OperationId::new("branch-write").unwrap(),
        invocation_id: InvocationId::new("branch-write").unwrap(),
        environment_id: branch.id.clone(),
        expected_revision: forked.revision,
        argv: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "test \"$(cat /workspace/counter)\" = once; printf branch > /workspace/counter"
                .to_owned(),
        ],
    };
    gateway
        .submit(&lease, branch_operation.clone())
        .await
        .unwrap();
    assert_eq!(
        terminal(&gateway, &owner, &branch_operation.id)
            .await
            .exit_code,
        Some(0)
    );
    let branch_before = gateway.snapshot(&owner, &branch.id, 2).await.unwrap();
    drop(gateway);
    let gateway = DockerGateway::connect(std::path::Path::new(&socket), &ledger, authority.clone())
        .await
        .unwrap();
    let replay = gateway.fork(&owner, &snapshot, branch.clone()).await;
    assert!(
        replay.is_ok(),
        "reopening the same fork after a lost response must return its existing branch: {replay:?}"
    );
    assert_eq!(replay.unwrap().revision, 2);
    let branch_after = gateway.snapshot(&owner, &branch.id, 2).await.unwrap();
    assert_eq!(
        branch_before.sha256, branch_after.sha256,
        "fork retry must not overwrite subsequent child work"
    );
    let unchanged = gateway.snapshot(&owner, &spec.id, revision).await.unwrap();
    assert_eq!(unchanged.sha256, snapshot.sha256);
    assert!(
        gateway
            .fork(&foreign, &snapshot, branch.clone())
            .await
            .is_err()
    );
    gateway.deliver_completions(&sink, 128).await.unwrap();
    gateway.release(&owner, &branch.id, 2).await.unwrap();
    assert!(gateway.get(&owner, &branch.id).await.is_err());
    let pending = CommandOperation {
        id: OperationId::new("cancel").unwrap(),
        invocation_id: InvocationId::new("cancel").unwrap(),
        environment_id: spec.id.clone(),
        expected_revision: revision,
        argv: vec!["sleep".to_owned(), "30".to_owned()],
    };
    gateway.submit(&lease, pending.clone()).await.unwrap();
    let cancellation = OperationAdmission {
        gateway_id: GatewayId::new("test-gateway").unwrap(),
        lease: lease.clone(),
        environment: gateway.get(&owner, &spec.id).await.unwrap(),
        operation: pending.clone(),
    };
    authority.0.store(false, Ordering::SeqCst);
    assert!(gateway.cancel(&lease, &pending.id).await.is_err());
    drop(gateway);
    let gateway = DockerGateway::connect(std::path::Path::new(&socket), &ledger, authority.clone())
        .await
        .unwrap();
    let mut forged = cancellation.clone();
    forged.operation.argv.push("changed".to_owned());
    assert!(gateway.cancel_admitted(&forged).await.is_err());
    gateway.cancel_admitted(&cancellation).await.unwrap();
    assert_eq!(
        terminal(&gateway, &owner, &pending.id).await.phase,
        OperationPhase::Cancelled
    );
    gateway.deliver_completions(&sink, 128).await.unwrap();
    assert_eq!(
        gateway.cancel_admitted(&cancellation).await.unwrap().phase,
        OperationPhase::Cancelled,
        "durable stop intent survives Worker revocation and gateway restart"
    );
    let tombstone = OperationAdmission {
        environment: gateway.get(&owner, &spec.id).await.unwrap(),
        operation: CommandOperation {
            id: OperationId::new("never-start").unwrap(),
            invocation_id: InvocationId::new("never-start").unwrap(),
            expected_revision: 3,
            argv: vec!["touch".to_owned(), "/workspace/forbidden".to_owned()],
            ..pending
        },
        ..cancellation
    };
    assert_eq!(
        gateway.cancel_admitted(&tombstone).await.unwrap().phase,
        OperationPhase::Cancelled
    );
    authority.0.store(true, Ordering::SeqCst);
    assert_eq!(
        gateway
            .submit(&lease, tombstone.operation)
            .await
            .unwrap()
            .phase,
        OperationPhase::Cancelled,
        "a delayed submit cannot start an operation cancelled before local admission"
    );
    gateway.deliver_completions(&sink, 128).await.unwrap();
    let final_revision = gateway.get(&owner, &spec.id).await.unwrap().revision;
    gateway
        .release(&owner, &spec.id, final_revision)
        .await
        .unwrap();
    gateway
        .release(&owner, &spec.id, final_revision)
        .await
        .unwrap();
    assert!(gateway.get(&owner, &spec.id).await.is_err());
    assert!(gateway.acquire(&owner, spec).await.is_err());
}
