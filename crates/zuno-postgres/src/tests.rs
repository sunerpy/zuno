use std::num::NonZeroU64;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;
use sqlx_core::query::query;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::raw_sql::raw_sql;
use zuno_application::{AgentApplication, CreateSession, PageSize, QueueText, SessionPageRequest};
use zuno_types::identity::{
    ClientId, PrincipalId, PrincipalKind, RequestId, TenantId, WorkspaceId,
};

use super::*;

#[derive(Deserialize)]
struct Fixture {
    admin_url: String,
    runtime_url: String,
    root_certificate: PathBuf,
    runtime_role: String,
}

impl Fixture {
    fn options(&self, admin: bool, connections: u32) -> PostgresOptions {
        PostgresOptions {
            url: if admin {
                self.admin_url.clone()
            } else {
                self.runtime_url.clone()
            },
            root_certificate: Some(self.root_certificate.clone()),
            max_connections: connections,
        }
    }
}

fn principal(tenant: &str, subject: &str) -> PrincipalScope {
    PrincipalScope::new(
        TenantId::new(tenant).unwrap(),
        PrincipalId::new(subject).unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("enterprise-web").unwrap()),
        NonZeroU64::MIN,
    )
}

fn create(id: &str) -> CreateSession {
    CreateSession {
        request_id: RequestId::new(id).unwrap(),
        workspace_id: WorkspaceId::new("workspace").unwrap(),
        title: format!("Task {id}"),
    }
}

#[tokio::test]
#[ignore = "run scripts/check_enterprise_postgres.py to provide an isolated TLS PostgreSQL cluster"]
async fn real_postgres_enforces_scopes_transactions_role_boundaries_and_schema_integrity() {
    let path = std::env::var("ZUNO_POSTGRES_TEST_CONFIG").expect("isolated PostgreSQL fixture");
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let admin = fixture.options(true, 4).connect().await.unwrap();
    assert!(
        PostgresBackend::connect(fixture.options(true, 1))
            .await
            .is_err(),
        "a superuser cannot be a runtime backend"
    );
    assert!(migrate(&admin, "role;DROP SCHEMA public").await.is_err());
    migrate(&admin, &fixture.runtime_role).await.unwrap();
    migrate(&admin, &fixture.runtime_role).await.unwrap();

    let mut untrusted_certificate = fixture.options(false, 1);
    untrusted_certificate.root_certificate = None;
    untrusted_certificate.url.push_str("?sslmode=disable");
    assert!(
        untrusted_certificate.connect().await.is_err(),
        "a URL cannot disable certificate verification"
    );

    let backend = PostgresBackend::connect(fixture.options(false, 4))
        .await
        .unwrap();
    let alice = principal("organization-a", "alice");
    let bob = principal("organization-a", "bob");
    let other_alice = principal("organization-b", "alice");
    let workspace = WorkspaceId::new("workspace").unwrap();
    for principal in [&alice, &bob, &other_alice] {
        backend
            .register_workspace(principal, &workspace, "Workspace")
            .await
            .unwrap();
    }
    let a = AgentApplication::new(Arc::new(backend.sessions(alice.clone())));
    let b = AgentApplication::new(Arc::new(backend.sessions(bob.clone())));
    let c = AgentApplication::new(Arc::new(backend.sessions(other_alice.clone())));
    let request = create("same-request");
    let (first, repeated) = tokio::join!(
        a.create_session(request.clone()),
        a.create_session(request.clone()),
    );
    let first = first.unwrap();
    assert_eq!(first, repeated.unwrap());
    let theirs = c.create_session(request.clone()).await.unwrap();
    assert_ne!(first.id, theirs.id);
    assert!(matches!(
        b.session(&first.id).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(matches!(
        c.session(&first.id).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(matches!(
        a.create_session(CreateSession {
            title: "changed content".to_owned(),
            ..request.clone()
        })
        .await,
        Err(ApplicationError::Conflict)
    ));

    let text = QueueText {
        request_id: RequestId::new("input").unwrap(),
        session_id: first.id.clone(),
        text: "Investigate the task".to_owned(),
    };
    let (input, repeated) = tokio::join!(a.queue_text(text.clone()), a.queue_text(text.clone()));
    let input = input.unwrap();
    assert_eq!(input, repeated.unwrap());
    assert!(matches!(
        a.queue_text(QueueText {
            text: "changed text".to_owned(),
            ..text.clone()
        })
        .await,
        Err(ApplicationError::Conflict)
    ));
    assert!(matches!(
        b.queue_text(text).await,
        Err(ApplicationError::NotFound)
    ));
    let admissions: i64 = query_scalar(
        "SELECT count(*) FROM zuno_enterprise_preview.event
         WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND type='session.input.admitted'",
    ).bind(alice.tenant_id().as_str()).bind(alice.principal_id().as_str()).bind(first.id.as_str())
        .fetch_one(&admin).await.unwrap();
    assert_eq!(admissions, 1);

    // RLS is independent of application WHERE clauses and pool identity never
    // survives its transaction. Use a one-connection pool to force reuse.
    let single = PostgresBackend::connect(fixture.options(false, 1))
        .await
        .unwrap();
    let no_context: i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.session")
        .fetch_one(&single.pool)
        .await
        .unwrap();
    assert_eq!(no_context, 0);
    for principal in [&alice, &bob, &other_alice, &alice] {
        let mut tx = scoped_transaction(&single.pool, principal).await.unwrap();
        let foreign: i64 = query_scalar(
            "SELECT count(*) FROM zuno_enterprise_preview.session WHERE tenant_id<>$1 OR principal_id<>$2",
        ).bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str())
            .fetch_one(&mut *tx).await.unwrap();
        assert_eq!(foreign, 0);
        tx.rollback().await.unwrap();
        let leaked: i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.session")
            .fetch_one(&single.pool)
            .await
            .unwrap();
        assert_eq!(leaked, 0);
    }

    // Equal timestamps retain all rows and keep another owner's records out.
    let mut expected = vec![first.id.clone()];
    for index in 0..5 {
        expected.push(
            a.create_session(create(&format!("page-{index}")))
                .await
                .unwrap()
                .id,
        );
    }
    b.create_session(create("bob")).await.unwrap();
    query("UPDATE zuno_enterprise_preview.session SET time_updated=100")
        .execute(&admin)
        .await
        .unwrap();
    expected.sort_by(|left, right| right.cmp(left));
    let mut after = None;
    let mut observed = Vec::new();
    loop {
        let page = a
            .sessions(SessionPageRequest {
                after,
                limit: PageSize::new(2).unwrap(),
            })
            .await
            .unwrap();
        observed.extend(page.items.into_iter().map(|item| item.id));
        after = page.next;
        if after.is_none() {
            break;
        }
        assert!(observed.len() <= expected.len());
    }
    assert_eq!(observed, expected);

    // An event failure after the input insert must roll the whole admission back.
    raw_sql(
        "CREATE FUNCTION zuno_enterprise_preview.refuse_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.type='application.input.queued' THEN RAISE EXCEPTION 'injected audit failure'; END IF;
         RETURN NEW; END $$;
         CREATE TRIGGER refuse_audit BEFORE INSERT ON zuno_enterprise_preview.event
         FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.refuse_audit();",
    ).execute(&admin).await.unwrap();
    let before: i64 = query_scalar(
        "SELECT event_sequence FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(alice.tenant_id().as_str()).bind(alice.principal_id().as_str()).bind(first.id.as_str())
        .fetch_one(&admin).await.unwrap();
    let failed = QueueText {
        request_id: RequestId::new("rollback").unwrap(),
        session_id: first.id.clone(),
        text: "Atomic input".to_owned(),
    };
    assert!(a.queue_text(failed.clone()).await.is_err());
    let after: i64 = query_scalar(
        "SELECT event_sequence FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(alice.tenant_id().as_str()).bind(alice.principal_id().as_str()).bind(first.id.as_str())
        .fetch_one(&admin).await.unwrap();
    assert_eq!(before, after);
    raw_sql(
        "DROP TRIGGER refuse_audit ON zuno_enterprise_preview.event;
         DROP FUNCTION zuno_enterprise_preview.refuse_audit();",
    )
    .execute(&admin)
    .await
    .unwrap();
    a.queue_text(failed).await.unwrap();

    // Schema drift and future versions are diagnosed without a destructive repair.
    raw_sql("GRANT TRUNCATE ON zuno_enterprise_preview.session TO zuno_preview_runtime")
        .execute(&admin)
        .await
        .unwrap();
    assert!(
        PostgresBackend::connect(fixture.options(false, 1))
            .await
            .is_err(),
        "a runtime role cannot bypass row policy with TRUNCATE"
    );
    raw_sql("REVOKE TRUNCATE ON zuno_enterprise_preview.session FROM zuno_preview_runtime")
        .execute(&admin)
        .await
        .unwrap();
    raw_sql("ALTER TABLE zuno_enterprise_preview.session DISABLE ROW LEVEL SECURITY")
        .execute(&admin)
        .await
        .unwrap();
    assert!(
        PostgresBackend::connect(fixture.options(false, 1))
            .await
            .is_err()
    );
    raw_sql("ALTER TABLE zuno_enterprise_preview.session ENABLE ROW LEVEL SECURITY")
        .execute(&admin)
        .await
        .unwrap();
    query("UPDATE zuno_enterprise_preview.schema_format SET version=999")
        .execute(&admin)
        .await
        .unwrap();
    assert!(migrate(&admin, &fixture.runtime_role).await.is_err());
    let count: i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.session")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(count, 8);
    query("UPDATE zuno_enterprise_preview.schema_format SET version=1")
        .execute(&admin)
        .await
        .unwrap();
    PostgresBackend::connect(fixture.options(false, 1))
        .await
        .unwrap();
    raw_sql("DROP TABLE zuno_enterprise_preview.schema_format")
        .execute(&admin)
        .await
        .unwrap();
    assert!(migrate(&admin, &fixture.runtime_role).await.is_err());
    let count_after: i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.session")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(count_after, count);
    assert_eq!(serde_json::to_value(&first).unwrap().get("directory"), None);
    assert_eq!(json!(input.state), "queued");
}
