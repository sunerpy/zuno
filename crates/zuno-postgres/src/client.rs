//! Authorized client reads. Worker lease reads use their separate fenced port.

use crate::{PostgresBackend, database_error, runtime, scoped_transaction};
use sqlx_core::query_scalar::query_scalar;
use zuno_application::{ApplicationError, runtime::RuntimeJob};
use zuno_types::identity::{JobId, PrincipalScope, SessionId};

impl PostgresBackend {
    pub async fn client_job(
        &self,
        principal: &PrincipalScope,
        id: &JobId,
    ) -> Result<RuntimeJob, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        let job = runtime::read_job(&mut tx, &principal.owner(), id.as_str()).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(job)
    }

    pub async fn client_input_version(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
    ) -> Result<u64, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        crate::session::read_session(&mut tx, principal, session.as_str(), false).await?;
        let version: i64 = query_scalar(
            "SELECT input_version FROM zuno_enterprise_preview.runtime_session
             WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(session.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        let version = u64::try_from(version).map_err(ApplicationError::storage)?;
        tx.commit().await.map_err(database_error)?;
        Ok(version)
    }
}
