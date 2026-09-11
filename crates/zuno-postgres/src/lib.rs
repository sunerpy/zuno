//! PostgreSQL data-owner adapter. Worker credentials never include this pool.

mod migration;
mod session;
#[cfg(test)]
mod tests;

pub use migration::{PREVIEW_SCHEMA, migrate};
pub use session::PostgresSessionPersistence;

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use sqlx_core::{row::Row, transaction::Transaction};
use sqlx_postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx_postgres::{PgPool, Postgres};
use zuno_application::ApplicationError;
use zuno_types::identity::PrincipalScope;

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
    let mut tx = pool.begin().await.map_err(database_error)?;
    // Transaction-local settings cannot leak into the next pool checkout.
    sqlx_core::query::query(
        "SELECT set_config('zuno.tenant_id',$1,true),set_config('zuno.principal_id',$2,true),
                set_config('statement_timeout','10000',true),set_config('lock_timeout','5000',true),
                set_config('search_path','pg_catalog',true)",
    )
    .bind(principal.tenant_id().as_str())
    .bind(principal.principal_id().as_str())
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    Ok(tx)
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
