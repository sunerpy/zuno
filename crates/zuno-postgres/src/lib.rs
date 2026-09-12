//! PostgreSQL data-owner adapter. Worker credentials never include this pool.

mod activity;
mod authorization;
#[cfg(test)]
mod authorization_tests;
mod browser;
#[cfg(test)]
mod browser_tests;
mod client;
mod live;
mod memory;
mod migration;
mod operation;
mod runtime;
#[cfg(test)]
mod runtime_tests;
mod session;
#[cfg(test)]
mod tests;
mod turn;
#[cfg(test)]
mod turn_tests;
mod workspace_merge;

pub use activity::PostgresActivityPersistence;
pub use authorization::{PostgresOrganizationStore, bootstrap_organization};
pub use browser::{BrowserStoreLimits, PostgresBrowserStore};
pub use client::ClientJobState;
pub use memory::{MemoryStoreLimits, PostgresMemoryBackend, PostgresMemoryService};
pub use migration::{PREVIEW_SCHEMA, migrate};
pub use operation::PostgresOperationStore;
pub use runtime::PostgresRuntimeStore;
pub use session::PostgresSessionPersistence;
pub use turn::PostgresTurnPersistence;
pub use workspace_merge::PostgresWorkspaceMergeStore;

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use sqlx_core::{row::Row, transaction::Transaction};
use sqlx_postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx_postgres::{PgPool, Postgres};
use zuno_application::ApplicationError;
use zuno_types::identity::{PrincipalKey, PrincipalScope, TenantId};

/// Connection configuration deliberately has no Debug/Serialize implementation:
/// its URL may contain a credential and is never a client DTO.
pub struct PostgresOptions {
    pub url: String,
    pub root_certificate: Option<PathBuf>,
    pub max_connections: u32,
}

impl PostgresOptions {
    pub async fn connect(&self) -> Result<PgPool, ApplicationError> {
        if self.max_connections == 0 || self.max_connections > 128 {
            return Err(ApplicationError::Invalid(
                "PostgreSQL pool size must be 1–128".to_owned(),
            ));
        }
        let mut options = PgConnectOptions::from_str(&self.url)
            .map_err(database_error)?
            .ssl_mode(PgSslMode::VerifyFull);
        if let Some(certificate) = &self.root_certificate {
            options = options.ssl_root_cert(certificate);
        }
        PgPoolOptions::new()
            .max_connections(self.max_connections)
            .acquire_timeout(Duration::from_secs(10))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx_core::raw_sql::raw_sql(
                        "SET search_path=pg_catalog; SET idle_in_transaction_session_timeout='30s'",
                    )
                    .execute(connection)
                    .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .map_err(database_error)
    }
}

/// A pool is accepted only after validating the runtime role and the preview
/// schema. The migration role is intentionally a different credential.
#[derive(Clone)]
pub struct PostgresBackend {
    pool: PgPool,
}

impl PostgresBackend {
    pub fn workspace_merges(
        &self,
        gateway: zuno_types::identity::GatewayId,
    ) -> PostgresWorkspaceMergeStore {
        PostgresWorkspaceMergeStore::new(self.clone(), gateway)
    }
    pub fn gateway_operations(
        &self,
        gateway: zuno_types::identity::GatewayId,
    ) -> PostgresOperationStore {
        PostgresOperationStore::new(self.clone(), gateway)
    }

    /// Authentication state is accessible only to the host's BFF, under its fixed
    /// deployment tenant. Public requests cannot select this scope.
    pub fn browser_sessions(
        &self,
        tenant: TenantId,
        limits: BrowserStoreLimits,
    ) -> Result<PostgresBrowserStore, zuno_identity::login::LoginError> {
        PostgresBrowserStore::new(self.pool.clone(), tenant, limits)
    }

    pub async fn connect(options: PostgresOptions) -> Result<Self, ApplicationError> {
        Self::from_pool(options.connect().await?).await
    }

    async fn from_pool(pool: PgPool) -> Result<Self, ApplicationError> {
        migration::validate_runtime(&pool).await?;
        Ok(Self { pool })
    }

    /// The host supplies an authenticated/authorized principal, never a scope
    /// deserialized from a request body.
    pub fn sessions(&self, principal: PrincipalScope) -> PostgresSessionPersistence {
        PostgresSessionPersistence::new(self.pool.clone(), principal)
    }

    /// Tenant routing is selected by the authenticated control-plane deployment,
    /// not by a worker-supplied principal or a public request body.
    pub fn runtime(&self, tenant: TenantId) -> PostgresRuntimeStore {
        PostgresRuntimeStore::new(self.pool.clone(), tenant)
    }

    pub fn organizations(&self, tenant: TenantId) -> PostgresOrganizationStore {
        PostgresOrganizationStore::new(self.pool.clone(), tenant)
    }

    /// Construct only in the data owner after authenticating the Worker and
    /// resolving its environment. The directory is executor-visible, not a path
    /// supplied by a public client or a control-plane private directory.
    pub fn turn_state(
        &self,
        lease: zuno_application::runtime::ExecutionLease,
        executor_directory: String,
    ) -> Result<PostgresTurnPersistence, ApplicationError> {
        PostgresTurnPersistence::new(self.pool.clone(), lease, executor_directory)
    }

    /// State-only access for a remote Worker. No control-plane path is exposed.
    pub fn worker_state(
        &self,
        lease: zuno_application::runtime::ExecutionLease,
    ) -> PostgresTurnPersistence {
        PostgresTurnPersistence::for_worker(self.pool.clone(), lease)
    }

    /// Register a logical workspace through an already authorized host action.
    pub async fn register_workspace(
        &self,
        principal: &PrincipalScope,
        id: &zuno_types::identity::WorkspaceId,
        title: &str,
    ) -> Result<(), ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        sqlx_core::query::query(
            "INSERT INTO zuno_enterprise_preview.workspace(tenant_id,principal_id,id,title)
             VALUES($1,$2,$3,$4) ON CONFLICT(tenant_id,principal_id,id) DO NOTHING",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(id.as_str())
        .bind(title)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
        tx.commit().await.map_err(database_error)
    }
}

async fn scoped_transaction(
    pool: &PgPool,
    principal: &PrincipalScope,
) -> Result<Transaction<'static, Postgres>, ApplicationError> {
    let mut tx = owner_transaction(pool, &principal.owner()).await?;
    let access = authorization::access_in(&mut tx, &principal.owner())
        .await
        .map_err(|error| match error {
            ApplicationError::NotFound => ApplicationError::Forbidden,
            other => other,
        })?;
    if zuno_permission::enterprise::actor_denial(&access.policy, &access.member, principal)
        .is_some()
    {
        return Err(ApplicationError::Forbidden);
    }
    // access_in holds policy/member read locks through the data transaction.
    // Revocation and resource mutation therefore cannot pass each other.
    Ok(tx)
}

async fn owner_transaction(
    pool: &PgPool,
    owner: &PrincipalKey,
) -> Result<Transaction<'static, Postgres>, ApplicationError> {
    let mut tx = pool.begin().await.map_err(database_error)?;
    set_owner(&mut tx, owner).await?;
    Ok(tx)
}

async fn set_owner(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
) -> Result<(), ApplicationError> {
    // Transaction-local settings cannot leak into the next pool checkout.
    sqlx_core::query::query(
        "SELECT set_config('zuno.tenant_id',$1,true),set_config('zuno.principal_id',$2,true),
                set_config('statement_timeout','10000',true),set_config('lock_timeout','5000',true),
                set_config('search_path','pg_catalog',true)",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;
    Ok(())
}

async fn database_time(tx: &mut Transaction<'_, Postgres>) -> Result<i64, ApplicationError> {
    sqlx_core::query::query(
        "SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint AS now",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?
    .try_get("now")
    .map_err(database_error)
}

fn database_error(error: sqlx_core::Error) -> ApplicationError {
    match &error {
        sqlx_core::Error::RowNotFound => ApplicationError::NotFound,
        sqlx_core::Error::PoolTimedOut | sqlx_core::Error::PoolClosed | sqlx_core::Error::Io(_) => {
            ApplicationError::Unavailable
        }
        sqlx_core::Error::Database(database) => match database.code().as_deref() {
            Some("23505") => ApplicationError::Conflict,
            Some("40001" | "40P01" | "55P03" | "57014") => ApplicationError::Unavailable,
            _ => ApplicationError::storage(error),
        },
        _ => ApplicationError::storage(error),
    }
}
