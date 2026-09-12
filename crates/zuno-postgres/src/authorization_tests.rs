mod administration;
mod boundaries;
mod environment;
mod lifecycle;
mod operation_results;
mod waiting;

use crate::{
    PostgresBackend, PostgresOrganizationStore, PostgresRuntimeStore, bootstrap_organization,
};
use serde_json::json;
use sqlx_core::{query::query, query_scalar::query_scalar};
use sqlx_postgres::PgPool;
use std::num::NonZeroU64;
use std::sync::Arc;
use zuno_application::authorization::*;
use zuno_application::runtime::*;
use zuno_application::{AgentApplication, ApplicationError, CreateSession};
use zuno_permission::enterprise::*;
use zuno_types::identity::*;

struct Fixture {
    owner: PrincipalScope,
    reviewer: PrincipalScope,
    outsider: PrincipalScope,
    authority: PostgresOrganizationStore,
    runtime: PostgresRuntimeStore,
    job: RuntimeJob,
    lease: ExecutionLease,
    policy: OrganizationPolicy,
}
fn scope(tenant: &str, id: &str, client: &str, revision: u64) -> PrincipalScope {
    PrincipalScope::new(
        TenantId::new(tenant).unwrap(),
        PrincipalId::new(id).unwrap(),
        PrincipalKind::User,
        Some(ClientId::new(client).unwrap()),
        NonZeroU64::new(revision).unwrap(),
    )
}
async fn fixture(
    backend: &PostgresBackend,
    admin: &PgPool,
    migrator: &PgPool,
    tenant: &str,
) -> Fixture {
    let owner = scope(tenant, "alice", "web", 1);
    let reviewer = scope(tenant, "bob", "review", 1);
    let outsider = scope(tenant, "other", "web", 1);
    let policy = OrganizationPolicy {
        tenant_id: owner.tenant_id().clone(),
        revision: NonZeroU64::MIN,
        allowed_apps: [
            ClientId::new("web").unwrap(),
            ClientId::new("api").unwrap(),
            ClientId::new("review").unwrap(),
        ]
        .into(),
        auto_read_apps: [ClientId::new("web").unwrap()].into(),
        approval_apps: [
            ClientId::new("web").unwrap(),
            ClientId::new("review").unwrap(),
        ]
        .into(),
        approval_lifetime_seconds: 300,
    };
    assert!(
        bootstrap_organization(migrator, &policy, &owner.owner())
            .await
            .unwrap()
    );
    assert!(
        !bootstrap_organization(migrator, &policy, &owner.owner())
            .await
            .unwrap()
    );
    assert!(matches!(
        bootstrap_organization(&backend.pool, &policy, &owner.owner()).await,
        Err(ApplicationError::Forbidden)
    ));
    for (principal, role) in [(&reviewer, "approver"), (&outsider, "member")] {
        query("INSERT INTO zuno_enterprise_preview.organization_member(tenant_id,principal_id,role,active) VALUES($1,$2,$3,true)")
            .bind(tenant).bind(principal.principal_id().as_str()).bind(role).execute(admin).await.unwrap();
    }
    let workspace = WorkspaceId::new("workspace").unwrap();
    backend
        .register_workspace(&owner, &workspace, "Approval workspace")
        .await
        .unwrap();
    let session = AgentApplication::new(Arc::new(backend.sessions(owner.clone())))
        .create_session(CreateSession {
            request_id: RequestId::new("session").unwrap(),
            workspace_id: workspace,
            title: "Approval task".to_owned(),
        })
        .await
        .unwrap();
    let runtime = backend.runtime(owner.tenant_id().clone());
    let job = runtime
        .submit(
            &owner,
            JobSubmission {
                selection: None,
                session_id: session.id,
                request_id: RequestId::new("job").unwrap(),
                expected_input_version: 0,
                text: "Investigate the workspace".to_owned(),
                configuration: ConfigurationRef {
                    id: ConfigurationId::new("definition").unwrap(),
                    version: 1,
                    sha256: "a".repeat(64),
                },
            },
        )
        .await
        .unwrap();
    let claimed = runtime
        .claim(
            &WorkerInstanceId::new("worker").unwrap(),
            LeaseDuration::new(300_000).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    Fixture {
        authority: backend.organizations(owner.tenant_id().clone()),
        owner,
        reviewer,
        outsider,
        runtime,
        job,
        lease: claimed.lease,
        policy,
    }
}
fn proposal(f: &Fixture, id: &str, kind: EffectKind) -> ApprovalProposal {
    ApprovalProposal {
        binding: ApprovalBinding {
            job_id: f.job.id.clone(),
            session_id: f.job.session_id.clone(),
            turn_id: f.job.turn_id.clone(),
            invocation_id: InvocationId::new(format!("invocation-{id}")).unwrap(),
            operation_id: OperationId::new(format!("operation-{id}")).unwrap(),
            arguments_sha256: zuno_orchestration::sha256_json(
                &json!({"command":"echo hello","operation":id}),
            ),
            resources_sha256: "b".repeat(64),
            effect: kind,
        },
        facts: PreparedEffectFacts {
            kind,
            resource_authorized: true,
            isolation: IsolationFact::Enforced,
            builtin_handler: true,
            sensitive: false,
            explicit_deny: false,
            mandatory_human: false,
        },
        presentation: json!({"command":"echo hello","workspace":"workspace"}),
    }
}
fn answer(id: &ApprovalId, key: &str) -> AnswerApproval {
    AnswerApproval {
        request_id: RequestId::new(key).unwrap(),
        approval_id: id.clone(),
        answer: ApprovalAnswer::Approve,
    }
}
pub(crate) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    lifecycle::exercise(backend, admin, migrator).await;
    boundaries::exercise(backend, admin, migrator).await;
    administration::exercise(backend, admin, migrator).await;
    environment::exercise(backend, admin, migrator).await;
    waiting::exercise(backend, admin, migrator).await;
    operation_results::exercise(backend, admin, migrator).await;
}
