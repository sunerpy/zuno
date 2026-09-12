//! Real PostgreSQL storage contracts. Consumption simulates the kernel's
//! materialization boundary; it does not certify a remote Agent Worker.
mod activity;
mod children;
mod concurrency;
mod recovery;

use crate::{PostgresBackend, scoped_transaction};
use serde_json::json;
use sqlx_core::{query::query, query_scalar::query_scalar, raw_sql::raw_sql};
use sqlx_postgres::PgPool;
use std::num::NonZeroU64;
use std::sync::Arc;
use zuno_application::runtime::{
    ConfigurationRef, JobFinish, JobPhase, JobSubmission, LeaseDuration, RuntimeCheckpoint,
    RuntimeJob, RuntimeStore,
};
use zuno_application::{AgentApplication, ApplicationError, CreateSession};
use zuno_types::identity::{
    ClientId, ConfigurationId, PrincipalId, PrincipalKind, PrincipalScope, RequestId, SessionId,
    TenantId, WorkerInstanceId, WorkspaceId,
};

fn principal(tenant: &str, name: &str) -> PrincipalScope {
    PrincipalScope::new(
        TenantId::new(tenant).unwrap(),
        PrincipalId::new(name).unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("web").unwrap()),
        NonZeroU64::MIN,
    )
}
fn worker(name: &str) -> WorkerInstanceId {
    WorkerInstanceId::new(name).unwrap()
}
fn duration() -> LeaseDuration {
    LeaseDuration::new(30_000).unwrap()
}
fn submission(session: &SessionId, id: &str, version: u64) -> JobSubmission {
    JobSubmission {
        selection: None,
        session_id: session.clone(),
        request_id: RequestId::new(id).unwrap(),
        expected_input_version: version,
        text: format!("Investigate {id}"),
        configuration: ConfigurationRef {
            id: ConfigurationId::new("definition").unwrap(),
            version: 1,
            sha256: "a".repeat(64),
        },
    }
}
fn checkpoint(job: &RuntimeJob) -> RuntimeCheckpoint {
    RuntimeCheckpoint {
        job_id: job.id.clone(),
        session_id: job.session_id.clone(),
        turn_id: job.turn_id.clone(),
        driver: "default".to_owned(),
        schema_version: 2,
        reference: json!({"eventId":"fixture-boundary","sequence":1}),
    }
}
async fn session(backend: &PostgresBackend, owner: &PrincipalScope, id: &str) -> SessionId {
    let workspace = WorkspaceId::new("runtime-workspace").unwrap();
    backend
        .register_workspace(owner, &workspace, "Runtime workspace")
        .await
        .unwrap();
    AgentApplication::new(Arc::new(backend.sessions(owner.clone())))
        .create_session(CreateSession {
            request_id: RequestId::new(id).unwrap(),
            workspace_id: workspace,
            title: id.to_owned(),
        })
        .await
        .unwrap()
        .id
}
async fn consume(admin: &PgPool, job: &RuntimeJob) {
    query("UPDATE zuno_enterprise_preview.input SET state='consumed' WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(job.principal.tenant_id().as_str()).bind(job.principal.principal_id().as_str()).bind(job.input_id.as_str())
        .execute(admin).await.unwrap();
}
pub(crate) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    for (tenant, subject) in [
        ("runtime-concurrency", "alice"),
        ("runtime-concurrency", "bob"),
        ("runtime-recovery", "alice"),
        ("runtime-recovery", "bob"),
        ("runtime-fairness", "zz-active"),
    ] {
        crate::tests::install_access(admin, &principal(tenant, subject)).await;
    }
    concurrency::exercise(backend, admin).await;
    recovery::exercise(backend, admin).await;
    children::execution_binding(backend, admin).await;
    activity::exercise(backend, admin).await;
}
