//! Authorized client reads. Worker lease reads use their separate fenced port.

use crate::{PostgresBackend, database_error, runtime, scoped_transaction};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use zuno_application::{ApplicationError, runtime::RuntimeJob};
use zuno_types::identity::{JobId, OperationId, PrincipalScope, SessionId};
use zuno_types::wait::WaitRef;

/// Internal read model; public protocols select their own safe fields.
pub struct ClientJobState {
    pub job: RuntimeJob,
    pub waits: Vec<WaitRef>,
    pub stop_requested: bool,
    pub pending_operations: Vec<OperationId>,
}

impl PostgresBackend {
    pub async fn client_submission(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
        request: &zuno_types::identity::RequestId,
    ) -> Result<ClientJobState, ApplicationError> {
        let id = JobId::new(format!(
            "job_{}",
            runtime::request_key(principal, session, request)
        ))
        .map_err(ApplicationError::storage)?;
        let state = self.client_job(principal, &id).await?;
        if state.job.session_id != *session {
            return Err(ApplicationError::Conflict);
        }
        Ok(state)
    }

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
        let stop_requested: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_stop
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3)",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        let pending: Vec<String> = query_scalar(
            "SELECT o.operation_id FROM zuno_enterprise_preview.gateway_operation o
             JOIN zuno_enterprise_preview.runtime_stop s
               ON s.tenant_id=o.tenant_id AND s.principal_id=o.principal_id AND s.job_id=o.job_id
             WHERE o.tenant_id=$1 AND o.principal_id=$2 AND (s.root_job_id=$3 OR s.job_id=$3)
               AND o.completion IS NULL ORDER BY o.operation_id",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(database_error)?;
        let pending_operations = pending
            .into_iter()
            .map(|id| OperationId::new(id).map_err(ApplicationError::storage))
            .collect::<Result<Vec<_>, _>>()?;
        tx.commit().await.map_err(database_error)?;
        Ok(ClientJobState {
            job,
            waits,
            stop_requested,
            pending_operations,
        })
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
