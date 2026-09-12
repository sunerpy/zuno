//! Gateway admission and result facts share the data owner's transactions.

use crate::{PostgresBackend, database_error, database_time, owner_transaction};
use serde_json::json;
use sqlx_core::{query::query, row::Row, transaction::Transaction};
use sqlx_postgres::Postgres;
use zuno_application::{
    ApplicationError,
    authorization::CheckedApproval,
    environment::{OperationAdmission, OperationCompletion},
};
use zuno_types::identity::{GatewayId, OperationId, PrincipalId, PrincipalKey, TenantId};

#[derive(Clone)]
pub struct PostgresOperationStore {
    backend: PostgresBackend,
    gateway: GatewayId,
}

impl PostgresOperationStore {
    pub(crate) fn new(backend: PostgresBackend, gateway: GatewayId) -> Self {
        Self { backend, gateway }
    }

    /// Only the authenticated gateway host selects its tenant/identity. This
    /// outbox survives Worker loss and never requires a live execution lease.
    pub async fn cancellations(
        &self,
        tenant: &TenantId,
        limit: u32,
    ) -> Result<Vec<OperationAdmission>, ApplicationError> {
        if !(1..=64).contains(&limit) {
            return Err(ApplicationError::Invalid(
                "invalid cancellation batch".to_owned(),
            ));
        }
        let rows = query(
            "SELECT principal_id,operation_id FROM zuno_enterprise_preview.gateway_cancellations($1,$2,$3)",
        )
        .bind(tenant.as_str()).bind(self.gateway.as_str()).bind(limit as i32)
        .fetch_all(&self.backend.pool).await.map_err(database_error)?;
        let mut pending = Vec::new();
        let mut encoded_bytes = 2usize;
        for row in rows {
            let owner = PrincipalKey {
                tenant_id: tenant.clone(),
                principal_id: PrincipalId::new(
                    row.try_get::<String, _>("principal_id")
                        .map_err(database_error)?,
                )
                .map_err(ApplicationError::storage)?,
            };
            let id: String = row.try_get("operation_id").map_err(database_error)?;
            let mut tx = owner_transaction(&self.backend.pool, &owner).await?;
            let raw = query(
                "SELECT o.admission FROM zuno_enterprise_preview.gateway_operation o
                 JOIN zuno_enterprise_preview.runtime_stop s
                   ON s.tenant_id=o.tenant_id AND s.principal_id=o.principal_id AND s.job_id=o.job_id
                 WHERE o.tenant_id=$1 AND o.principal_id=$2 AND o.operation_id=$3
                   AND o.gateway_id=$4 AND o.completion IS NULL",
            )
            .bind(tenant.as_str()).bind(owner.principal_id.as_str()).bind(&id).bind(self.gateway.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?;
            if let Some(raw) = raw {
                let admission: OperationAdmission =
                    serde_json::from_value(raw.try_get("admission").map_err(database_error)?)
                        .map_err(ApplicationError::storage)?;
                if admission.lease.owner != owner
                    || admission.gateway_id != self.gateway
                    || admission.operation.id.as_str() != id
                {
                    return Err(ApplicationError::Conflict);
                }
                let bytes = serde_json::to_vec(&admission)
                    .map_err(ApplicationError::storage)?
                    .len()
                    + 1;
                if encoded_bytes.saturating_add(bytes)
                    > zuno_application::environment::wire::MAX_GATEWAY_FRAME_BYTES
                {
                    if pending.is_empty() {
                        return Err(ApplicationError::Invalid(
                            "stored cancellation admission exceeds the protocol bound".to_owned(),
                        ));
                    }
                    tx.commit().await.map_err(database_error)?;
                    break;
                }
                encoded_bytes += bytes;
                let time = database_time(&mut tx).await?;
                query("UPDATE zuno_enterprise_preview.gateway_cancellation_delivery SET time_polled=$4
                    WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3")
                    .bind(tenant.as_str()).bind(owner.principal_id.as_str()).bind(&id).bind(time)
                    .execute(&mut *tx).await.map_err(database_error)?;
                pending.push(admission);
            }
            tx.commit().await.map_err(database_error)?;
        }
        Ok(pending)
    }

    /// The host authenticates the gateway and pins its ID before using this
    /// provider. Expired Worker leases do not invalidate an already admitted fact.
    pub async fn complete(&self, completion: &OperationCompletion) -> Result<(), ApplicationError> {
        completion.validate()?;
        let owner = &completion.lease.owner;
        let mut tx = owner_transaction(&self.backend.pool, owner).await?;
        query("SELECT id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 FOR UPDATE")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(completion.lease.session_id.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
        let row=query(
            "SELECT gateway_id,admission,completion_digest FROM zuno_enterprise_preview.gateway_operation
             WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND job_id=$4 AND session_id=$5 FOR UPDATE",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(completion.operation.id.as_str())
            .bind(completion.lease.job_id.as_str()).bind(completion.lease.session_id.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
        if row
            .try_get::<String, _>("gateway_id")
            .map_err(database_error)?
            != self.gateway.as_str()
        {
            return Err(ApplicationError::Forbidden);
        }
        let admission: OperationAdmission =
            serde_json::from_value(row.try_get("admission").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        if admission.operation != completion.operation || admission.lease.owner != *owner {
            return Err(ApplicationError::Conflict);
        }
        let attempt = query(
            "SELECT lease FROM zuno_enterprise_preview.gateway_operation_attempt
             WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND attempt_id=$4
               AND worker_id=$5 AND epoch=$6 AND checkpoint_version=$7",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(completion.operation.id.as_str())
        .bind(completion.lease.attempt_id.as_str())
        .bind(completion.lease.worker.as_str())
        .bind(i64::try_from(completion.lease.epoch).map_err(ApplicationError::storage)?)
        .bind(
            i64::try_from(completion.lease.checkpoint_version)
                .map_err(ApplicationError::storage)?,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?
        .ok_or(ApplicationError::Forbidden)?;
        let recorded: zuno_application::runtime::ExecutionLease =
            serde_json::from_value(attempt.try_get("lease").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        if recorded.owner != *owner
            || recorded.job_id != completion.lease.job_id
            || recorded.session_id != completion.lease.session_id
        {
            return Err(ApplicationError::Conflict);
        }
        let digest = zuno_orchestration::sha256_json(&json!(completion));
        if let Some(old) = row
            .try_get::<Option<String>, _>("completion_digest")
            .map_err(database_error)?
        {
            if old != digest {
                return Err(ApplicationError::Conflict);
            }
        } else {
            let time = database_time(&mut tx).await?;
            query(
                "UPDATE zuno_enterprise_preview.gateway_operation SET completion=$4,completion_digest=$5,time_completed=$6
                 WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3",
            ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(completion.operation.id.as_str())
                .bind(json!(completion)).bind(digest).bind(time).execute(&mut *tx).await.map_err(database_error)?;
            let job =
                crate::runtime::read_job(&mut tx, owner, completion.lease.job_id.as_str()).await?;
            crate::session::emit(
                &mut tx,
                &job.principal,
                job.session_id.as_str(),
                "runtime.operation.completed",
                json!({"operationID":completion.operation.id,"receipt":completion.receipt}),
            )
            .await?;
        }
        let job =
            crate::runtime::read_job(&mut tx, owner, completion.lease.job_id.as_str()).await?;
        crate::runtime::waiting::operation_completed(&mut tx, &job, &completion.operation.id)
            .await?;
        tx.commit().await.map_err(database_error)
    }

    pub async fn completion(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<Option<OperationCompletion>, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, owner).await?;
        let row=query(
            "SELECT completion,completion_digest FROM zuno_enterprise_preview.gateway_operation WHERE tenant_id=$1 AND principal_id=$2
             AND operation_id=$3 AND gateway_id=$4",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).bind(self.gateway.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?;
        let completion = if let Some(row) = row {
            let raw: Option<serde_json::Value> =
                row.try_get("completion").map_err(database_error)?;
            let value = raw
                .map(serde_json::from_value::<OperationCompletion>)
                .transpose()
                .map_err(ApplicationError::storage)?;
            if let Some(value) = &value {
                value.validate()?;
                let digest: Option<String> =
                    row.try_get("completion_digest").map_err(database_error)?;
                if value.lease.owner != *owner
                    || value.operation.id != *id
                    || digest.as_deref()
                        != Some(zuno_orchestration::sha256_json(&json!(value)).as_str())
                {
                    return Err(ApplicationError::Conflict);
                }
            }
            value
        } else {
            None
        };
        tx.commit().await.map_err(database_error)?;
        Ok(completion)
    }
}

pub(crate) async fn identity_lock(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    id: &OperationId,
) -> Result<(), ApplicationError> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!(
            "zuno.enterprise.operation:{}",
            zuno_orchestration::sha256_json(&json!([owner, id]))
        ))
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
    Ok(())
}

pub(crate) async fn admit_in(
    tx: &mut Transaction<'_, Postgres>,
    checked: &CheckedApproval,
    admission: &OperationAdmission,
) -> Result<(), ApplicationError> {
    identity_lock(tx, &admission.lease.owner, &admission.operation.id).await?;
    let conflict: bool = sqlx_core::query_scalar::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.gateway_merge_operation
        WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3)",
    )
    .bind(admission.lease.owner.tenant_id.as_str())
    .bind(admission.lease.owner.principal_id.as_str())
    .bind(admission.operation.id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    if conflict {
        return Err(ApplicationError::Conflict);
    }
    admission.operation.validate()?;
    admission.environment.spec.validate()?;
    if admission.lease.owner != checked.lease.owner
        || admission.lease.job_id != checked.lease.job_id
        || admission.lease.session_id != checked.lease.session_id
        || admission.lease.worker != checked.lease.worker
        || admission.lease.attempt_id != checked.lease.attempt_id
        || admission.lease.epoch != checked.lease.epoch
        || admission.lease.checkpoint_version != checked.lease.checkpoint_version
        || admission.environment.owner != checked.lease.owner
        || admission.environment.spec.session_id != checked.lease.session_id
        || admission.operation.id != checked.binding.operation_id
        || admission.operation.invocation_id != checked.binding.invocation_id
        || admission.operation.environment_id != admission.environment.spec.id
        || admission.operation.expected_revision != admission.environment.revision
        || checked.binding.arguments_sha256
            != zuno_orchestration::sha256_json(&json!(admission.operation.argv))
        || checked.binding.resources_sha256
            != zuno_orchestration::sha256_json(&json!([
                admission.environment.owner,
                admission.environment.spec,
                admission.environment.revision,
            ]))
    {
        return Err(ApplicationError::Conflict);
    }
    let owner = &checked.lease.owner;
    // Attempt identity is not part of the immutable logical operation digest.
    let digest = zuno_orchestration::sha256_json(&json!([
        admission.gateway_id,
        owner,
        checked.lease.job_id,
        checked.lease.session_id,
        admission.environment,
        admission.operation,
    ]));
    let existing = query(
        "SELECT admission_digest FROM zuno_enterprise_preview.gateway_operation
         WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(admission.operation.id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_error)?;
    if let Some(existing) = existing {
        if existing
            .try_get::<String, _>("admission_digest")
            .map_err(database_error)?
            != digest
        {
            return Err(ApplicationError::Conflict);
        }
    } else {
        let time = database_time(tx).await?;
        query(
        "INSERT INTO zuno_enterprise_preview.gateway_operation(
           tenant_id,principal_id,operation_id,gateway_id,job_id,session_id,invocation_id,admission,admission_digest,time_admitted)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str())
        .bind(admission.gateway_id.as_str()).bind(checked.lease.job_id.as_str()).bind(checked.lease.session_id.as_str())
        .bind(admission.operation.invocation_id.as_str()).bind(json!(admission)).bind(digest).bind(time)
            .execute(&mut **tx).await.map_err(database_error)?;
    }
    let time = database_time(tx).await?;
    query(
        "INSERT INTO zuno_enterprise_preview.gateway_operation_attempt
          (tenant_id,principal_id,operation_id,attempt_id,worker_id,epoch,checkpoint_version,lease,time_admitted)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT DO NOTHING",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str())
        .bind(admission.lease.attempt_id.as_str()).bind(admission.lease.worker.as_str())
        .bind(i64::try_from(admission.lease.epoch).map_err(ApplicationError::storage)?)
        .bind(i64::try_from(admission.lease.checkpoint_version).map_err(ApplicationError::storage)?)
        .bind(json!(admission.lease)).bind(time).execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}
