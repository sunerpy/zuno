use super::*;
use zuno_application::environment::{
    CommandOperation, Environment, EnvironmentSpec, OperationAuthority,
};
use zuno_environment::OrganizationOperationAuthority;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "environment-authority").await;
    let authority = OrganizationOperationAuthority::new(
        Arc::new(f.runtime.clone()),
        Arc::new(f.authority.clone()),
    );
    let environment = Environment {
        owner: f.owner.owner(),
        revision: 1,
        spec: EnvironmentSpec {
            id: EnvironmentId::new("environment").unwrap(),
            session_id: f.job.session_id.clone(),
            image: format!("image@sha256:{}", "a".repeat(64)),
            memory_bytes: 128 * 1024 * 1024,
            pids_limit: 64,
            cpu_millis: 1000,
        },
    };
    let operation = CommandOperation {
        id: OperationId::new("command").unwrap(),
        invocation_id: InvocationId::new("call").unwrap(),
        environment_id: environment.spec.id.clone(),
        expected_revision: 1,
        argv: vec!["inspect".to_owned()],
    };
    assert!(
        authority
            .authorize(&f.lease, &environment, &operation)
            .await
            .is_err()
    );
    let proposal = authority
        .proposal(&f.lease, &environment, &operation)
        .await
        .unwrap();
    let approval = f.authority.admit(&f.lease, proposal).await.unwrap();
    assert_eq!(approval.state, ApprovalState::Pending);
    assert!(
        authority
            .authorize(&f.lease, &environment, &operation)
            .await
            .is_err()
    );
    f.authority
        .answer(
            &f.owner,
            AnswerApproval {
                request_id: RequestId::new("approve-command").unwrap(),
                approval_id: approval.id,
                answer: ApprovalAnswer::Approve,
            },
        )
        .await
        .unwrap();
    authority
        .authorize(&f.lease, &environment, &operation)
        .await
        .unwrap();
    let mut changed = operation.clone();
    changed.argv.push("different".to_owned());
    assert!(
        authority
            .authorize(&f.lease, &environment, &changed)
            .await
            .is_err()
    );
    let mut newer = environment.clone();
    newer.revision = 2;
    assert!(
        authority
            .authorize(&f.lease, &newer, &operation)
            .await
            .is_err()
    );
    let mut stale = f.lease.clone();
    stale.epoch = stale.epoch.saturating_sub(1);
    assert!(
        authority
            .authorize(&stale, &environment, &operation)
            .await
            .is_err()
    );
}
