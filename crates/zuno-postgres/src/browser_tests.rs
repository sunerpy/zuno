use sqlx_core::{query::query, query_scalar::query_scalar, raw_sql::raw_sql};
use sqlx_postgres::PgPool;
use zuno_identity::login_state::{
    BrowserSessionRecord, BrowserSessionStore, EncryptedLogin, LoginTransactionStore,
};
use zuno_types::identity::{ClientId, PrincipalId, TenantId};

use crate::{BrowserStoreLimits, PostgresBackend};

pub(crate) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let tenant = TenantId::new("browser-a").unwrap();
    let limits = BrowserStoreLimits {
        pending_logins: 1,
        active_sessions: 2,
    };
    let store = backend.browser_sessions(tenant.clone(), limits).unwrap();
    let other = backend
        .browser_sessions(TenantId::new("browser-b").unwrap(), limits)
        .unwrap();
    let now: i64 = query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
        .fetch_one(admin)
        .await
        .unwrap();
    let now = u64::try_from(now).unwrap();
    let login = EncryptedLogin {
        schema_version: 1,
        key_id: "fixture".to_owned(),
        state_hash: [1; 32],
        browser_hash: [2; 32],
        expires_at: now + 300,
        nonce: [0; 12],
        ciphertext: vec![7; 32],
    };
    store.insert(login.clone()).await.unwrap();
    let mut second = login.clone();
    second.state_hash = [3; 32];
    assert!(
        store.insert(second.clone()).await.is_err(),
        "pending capacity is bounded"
    );
    assert!(
        other
            .take(login.state_hash, login.browser_hash, now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .take(login.state_hash, [9; 32], now)
            .await
            .unwrap()
            .is_none()
    );
    // A caller cannot move database time backwards or consume one state twice.
    let (one, two) = tokio::join!(
        store.take(login.state_hash, login.browser_hash, u64::MAX),
        store.take(login.state_hash, login.browser_hash, 0),
    );
    assert_eq!(
        usize::from(one.unwrap().is_some()) + usize::from(two.unwrap().is_some()),
        1
    );
    store.insert(second.clone()).await.unwrap();
    query("UPDATE zuno_enterprise_preview.browser_login SET expires_at=$1 WHERE tenant_id=$2")
        .bind(i64::try_from(now - 1).unwrap())
        .bind(tenant.as_str())
        .execute(admin)
        .await
        .unwrap();
    assert!(
        store
            .take(second.state_hash, second.browser_hash, 0)
            .await
            .unwrap()
            .is_none()
    );
    store.insert(login).await.unwrap();

    let alice = BrowserSessionRecord {
        token_hash: [10; 32],
        issuer: "https://issuer.example".to_owned(),
        tenant_id: tenant.clone(),
        principal_id: PrincipalId::new("alice").unwrap(),
        client_id: ClientId::new("enterprise-web").unwrap(),
        oauth_client_id: "enterprise-web".to_owned(),
        expires_at: now + 300,
    };
    let mut bob = alice.clone();
    bob.token_hash = [11; 32];
    bob.principal_id = PrincipalId::new("bob").unwrap();
    assert!(
        other.create(alice.clone()).await.is_err(),
        "the deployment owns its tenant"
    );
    store.create(alice.clone()).await.unwrap();
    store.create(bob.clone()).await.unwrap();
    assert!(other.lookup(alice.token_hash, now).await.unwrap().is_none());
    assert_eq!(
        store
            .lookup(alice.token_hash, now)
            .await
            .unwrap()
            .unwrap()
            .principal_id,
        alice.principal_id
    );
    assert_eq!(
        store
            .lookup(bob.token_hash, now)
            .await
            .unwrap()
            .unwrap()
            .principal_id,
        bob.principal_id
    );
    let mut third = alice.clone();
    third.token_hash = [12; 32];
    assert!(
        store.create(third.clone()).await.is_err(),
        "session capacity is bounded"
    );
    // Forced RLS applies even to SQL without a caller-supplied owner predicate.
    let mut unscoped = backend.pool.begin().await.unwrap();
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.browser_session")
            .fetch_one(&mut *unscoped)
            .await
            .unwrap(),
        0
    );
    unscoped.rollback().await.unwrap();
    other.revoke(alice.token_hash).await.unwrap();
    assert!(store.lookup(alice.token_hash, now).await.unwrap().is_some());

    raw_sql(
        "CREATE FUNCTION public.zuno_refuse_auth_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected authentication audit failure'; END $$;
         CREATE TRIGGER refuse_auth_audit BEFORE INSERT ON zuno_enterprise_preview.authentication_audit
           FOR EACH ROW EXECUTE FUNCTION public.zuno_refuse_auth_audit();",
    ).execute(admin).await.unwrap();
    assert!(store.revoke(alice.token_hash).await.is_err());
    assert!(
        store.lookup(alice.token_hash, now).await.unwrap().is_some(),
        "logout and audit roll back together"
    );
    raw_sql("DROP TRIGGER refuse_auth_audit ON zuno_enterprise_preview.authentication_audit; DROP FUNCTION public.zuno_refuse_auth_audit()")
        .execute(admin).await.unwrap();
    store.revoke(alice.token_hash).await.unwrap();
    store.revoke(alice.token_hash).await.unwrap();
    assert!(store.lookup(alice.token_hash, now).await.unwrap().is_none());
    assert!(
        store.lookup(bob.token_hash, now).await.unwrap().is_some(),
        "another user's session remains active"
    );
    store.create(third).await.unwrap();
    assert_eq!(query_scalar::<_,i64>(
        "SELECT count(*) FROM zuno_enterprise_preview.authentication_audit WHERE tenant_id=$1 AND type='authentication.session.revoked'",
    ).bind(tenant.as_str()).fetch_one(admin).await.unwrap(), 1);
    query("UPDATE zuno_enterprise_preview.browser_session SET expires_at=$1 WHERE tenant_id=$2")
        .bind(i64::try_from(now - 1).unwrap())
        .bind(tenant.as_str())
        .execute(admin)
        .await
        .unwrap();
    assert!(store.lookup(bob.token_hash, 0).await.unwrap().is_none());
}
