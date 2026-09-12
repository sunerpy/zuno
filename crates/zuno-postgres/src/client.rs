//! Authorized client reads. Worker lease reads use their separate fenced port.

use crate::{PostgresBackend, database_error, runtime, scoped_transaction};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use zuno_application::{ApplicationError, runtime::RuntimeJob};
use zuno_types::identity::{JobId, PrincipalScope, SessionId};
use zuno_types::wait::WaitRef;

/// Internal read model; public protocols select their own safe fields.
pub struct ClientJobState {
    pub job: RuntimeJob,
    pub waits: Vec<WaitRef>,
}

impl PostgresBackend {
    pub async fn client_job(
        &self,
        principal: &PrincipalScope,
        id: &JobId,
    ) -> Result<ClientJobState, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        let job = runtime::read_job(&mut tx, &principal.owner(), id.as_str()).await?;
        let rows = query(
            "SELECT reference FROM zuno_enterprise_preview.runtime_wait
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state IN ('pending','ready')
             ORDER BY id LIMIT 65",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(database_error)?;
        if rows.len() > 64 {
            return Err(ApplicationError::Invalid(
                "too many active invocation waits".to_owned(),
            ));
        }
        let mut waits = Vec::with_capacity(rows.len());
        for row in rows {
            let reference: WaitRef =
                serde_json::from_value(row.try_get("reference").map_err(database_error)?)
                    .map_err(ApplicationError::storage)?;
            reference
                .validate()
                .map_err(|message| ApplicationError::Invalid(message.to_owned()))?;
            if reference.turn_id != job.turn_id {
                return Err(ApplicationError::Invalid(
                    "wait belongs to another turn".to_owned(),
                ));
            }
            waits.push(reference);
        }
        tx.commit().await.map_err(database_error)?;
        Ok(ClientJobState { job, waits })
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
