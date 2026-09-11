//! Tenant-bound BFF state. Only the data-owning authentication service uses it.

use async_trait::async_trait;
use serde_json::json;
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::{PgPool, Postgres};
use zuno_identity::login::LoginError;
use zuno_identity::login_state::{
    BrowserSessionRecord, BrowserSessionStore, EncryptedLogin, LoginTransactionStore,
};
use zuno_types::identity::{PrincipalId, PrincipalKey, TenantId};

#[derive(Debug, Clone, Copy)]
pub struct BrowserStoreLimits {
    pub pending_logins: u32,
    pub active_sessions: u32,
}

impl Default for BrowserStoreLimits {
    fn default() -> Self {
        Self {
            pending_logins: 4096,
            active_sessions: 65536,
        }
    }
}

#[derive(Clone)]
pub struct PostgresBrowserStore {
    pool: PgPool,
    tenant: TenantId,
    limits: BrowserStoreLimits,
}

impl PostgresBrowserStore {
    pub(crate) fn new(
        pool: PgPool,
        tenant: TenantId,
        limits: BrowserStoreLimits,
    ) -> Result<Self, LoginError> {
        if limits.pending_logins == 0
            || limits.pending_logins > 100_000
            || limits.active_sessions == 0
            || limits.active_sessions > 1_000_000
        {
            return Err(LoginError::Transaction);
        }
        Ok(Self {
            pool,
            tenant,
            limits,
        })
    }

    async fn transaction(&self) -> Result<Transaction<'static, Postgres>, LoginError> {
        crate::owner_transaction(
            &self.pool,
            &PrincipalKey {
                tenant_id: self.tenant.clone(),
                principal_id: PrincipalId::new("authentication-service")
                    .expect("fixed service identity"),
            },
        )
        .await
        .map_err(|_| LoginError::Unavailable)
    }

    async fn lock_capacity(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), LoginError> {
        query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("zuno.browser:{}", self.tenant))
            .execute(&mut **tx)
            .await
            .map_err(|_| LoginError::Unavailable)?;
        Ok(())
    }
}

async fn now(tx: &mut Transaction<'_, Postgres>) -> Result<i64, LoginError> {
    crate::database_time(tx)
        .await
        .map(|time| time / 1000)
        .map_err(|_| LoginError::Unavailable)
}

#[async_trait]
impl LoginTransactionStore for PostgresBrowserStore {
    async fn insert(&self, login: EncryptedLogin) -> Result<(), LoginError> {
        login.validate()?;
        let expires = i64::try_from(login.expires_at).map_err(|_| LoginError::Transaction)?;
        let mut tx = self.transaction().await?;
        self.lock_capacity(&mut tx).await?;
        let time = now(&mut tx).await?;
        if expires <= time || expires > time.saturating_add(600) {
            return Err(LoginError::Transaction);
        }
        query(
            "DELETE FROM zuno_enterprise_preview.browser_login WHERE tenant_id=$1 AND state_hash IN
             (SELECT state_hash FROM zuno_enterprise_preview.browser_login WHERE tenant_id=$1 AND expires_at<=$2 LIMIT 1024)",
        ).bind(self.tenant.as_str()).bind(time).execute(&mut *tx).await.map_err(|_| LoginError::Unavailable)?;
        let active: i64 = query_scalar(
            "SELECT count(*) FROM zuno_enterprise_preview.browser_login WHERE tenant_id=$1 AND expires_at>$2",
        ).bind(self.tenant.as_str()).bind(time).fetch_one(&mut *tx).await.map_err(|_| LoginError::Unavailable)?;
        if active >= i64::from(self.limits.pending_logins) {
            return Err(LoginError::Unavailable);
        }
        query(
            "INSERT INTO zuno_enterprise_preview.browser_login(tenant_id,state_hash,browser_hash,expires_at,encrypted)
             VALUES($1,$2,$3,$4,$5)",
        ).bind(self.tenant.as_str()).bind(login.state_hash.as_slice()).bind(login.browser_hash.as_slice())
            .bind(expires).bind(json!(login)).execute(&mut *tx).await.map_err(|_| LoginError::Transaction)?;
        tx.commit().await.map_err(|_| LoginError::Unavailable)
    }

    async fn take(
        &self,
        state_hash: [u8; 32],
        browser_hash: [u8; 32],
        _now: u64,
    ) -> Result<Option<EncryptedLogin>, LoginError> {
        let mut tx = self.transaction().await?;
        let time = now(&mut tx).await?;
        let data: Option<serde_json::Value> = query_scalar(
            "DELETE FROM zuno_enterprise_preview.browser_login
             WHERE tenant_id=$1 AND state_hash=$2 AND browser_hash=$3 AND expires_at>$4 RETURNING encrypted",
        ).bind(self.tenant.as_str()).bind(state_hash.as_slice()).bind(browser_hash.as_slice()).bind(time)
            .fetch_optional(&mut *tx).await.map_err(|_| LoginError::Unavailable)?;
        let login = data
            .map(serde_json::from_value::<EncryptedLogin>)
            .transpose()
            .map_err(|_| LoginError::Transaction)?;
        if let Some(login) = &login {
            login.validate()?;
            if login.state_hash != state_hash
                || login.browser_hash != browser_hash
                || login.expires_at <= u64::try_from(time).map_err(|_| LoginError::Transaction)?
            {
                return Err(LoginError::Transaction);
            }
        }
        tx.commit().await.map_err(|_| LoginError::Unavailable)?;
        Ok(login)
    }
}

#[async_trait]
impl BrowserSessionStore for PostgresBrowserStore {
    async fn create(&self, session: BrowserSessionRecord) -> Result<(), LoginError> {
        if session.tenant_id != self.tenant {
            return Err(LoginError::Transaction);
        }
        let expires = i64::try_from(session.expires_at).map_err(|_| LoginError::Transaction)?;
        let mut tx = self.transaction().await?;
        self.lock_capacity(&mut tx).await?;
        let time = now(&mut tx).await?;
        if expires <= time {
            return Err(LoginError::Transaction);
        }
        query(
            "DELETE FROM zuno_enterprise_preview.browser_session WHERE tenant_id=$1 AND token_hash IN
             (SELECT token_hash FROM zuno_enterprise_preview.browser_session WHERE tenant_id=$1 AND expires_at<=$2 LIMIT 1024)",
        ).bind(self.tenant.as_str()).bind(time).execute(&mut *tx).await.map_err(|_| LoginError::Unavailable)?;
        let count: i64 = query_scalar(
            "SELECT count(*) FROM zuno_enterprise_preview.browser_session WHERE tenant_id=$1 AND expires_at>$2",
        ).bind(self.tenant.as_str()).bind(time).fetch_one(&mut *tx).await.map_err(|_| LoginError::Unavailable)?;
        if count >= i64::from(self.limits.active_sessions) {
            return Err(LoginError::Unavailable);
        }
        query(
            "INSERT INTO zuno_enterprise_preview.browser_session(tenant_id,token_hash,principal_id,expires_at,identity)
             VALUES($1,$2,$3,$4,$5)",
        ).bind(self.tenant.as_str()).bind(session.token_hash.as_slice()).bind(session.principal_id.as_str())
            .bind(expires).bind(json!(session)).execute(&mut *tx).await.map_err(|_| LoginError::Transaction)?;
        audit(
            &mut tx,
            &self.tenant,
            session.principal_id.as_str(),
            "authentication.session.created",
            time,
        )
        .await?;
        tx.commit().await.map_err(|_| LoginError::Unavailable)
    }

    async fn lookup(
        &self,
        token_hash: [u8; 32],
        _now: u64,
    ) -> Result<Option<BrowserSessionRecord>, LoginError> {
        let mut tx = self.transaction().await?;
        let time = now(&mut tx).await?;
        let row = query(
            "SELECT identity,principal_id,expires_at FROM zuno_enterprise_preview.browser_session
             WHERE tenant_id=$1 AND token_hash=$2 AND expires_at>$3",
        )
        .bind(self.tenant.as_str())
        .bind(token_hash.as_slice())
        .bind(time)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| LoginError::Unavailable)?;
        let result = row
            .map(|row| {
                let record: BrowserSessionRecord = serde_json::from_value(
                    row.try_get("identity")
                        .map_err(|_| LoginError::Transaction)?,
                )
                .map_err(|_| LoginError::Transaction)?;
                if record.token_hash != token_hash
                    || record.tenant_id != self.tenant
                    || record.principal_id.as_str()
                        != row
                            .try_get::<String, _>("principal_id")
                            .map_err(|_| LoginError::Transaction)?
                    || i64::try_from(record.expires_at).ok()
                        != Some(
                            row.try_get("expires_at")
                                .map_err(|_| LoginError::Transaction)?,
                        )
                {
                    return Err(LoginError::Transaction);
                }
                Ok(record)
            })
            .transpose()?;
        tx.commit().await.map_err(|_| LoginError::Unavailable)?;
        Ok(result)
    }

    async fn revoke(&self, token_hash: [u8; 32]) -> Result<(), LoginError> {
        let mut tx = self.transaction().await?;
        let time = now(&mut tx).await?;
        let principal: Option<String> = query_scalar(
            "DELETE FROM zuno_enterprise_preview.browser_session WHERE tenant_id=$1 AND token_hash=$2 RETURNING principal_id",
        ).bind(self.tenant.as_str()).bind(token_hash.as_slice()).fetch_optional(&mut *tx).await.map_err(|_| LoginError::Unavailable)?;
        if let Some(principal) = principal {
            audit(
                &mut tx,
                &self.tenant,
                &principal,
                "authentication.session.revoked",
                time,
            )
            .await?;
        }
        tx.commit().await.map_err(|_| LoginError::Unavailable)
    }
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &TenantId,
    principal: &str,
    kind: &str,
    time: i64,
) -> Result<(), LoginError> {
    query(
        "INSERT INTO zuno_enterprise_preview.authentication_audit(tenant_id,id,principal_id,type,time_created) VALUES($1,$2,$3,$4,$5)",
    ).bind(tenant.as_str()).bind(uuid::Uuid::new_v4().to_string()).bind(principal).bind(kind).bind(time)
        .execute(&mut **tx).await.map_err(|_| LoginError::Unavailable)?;
    Ok(())
}
