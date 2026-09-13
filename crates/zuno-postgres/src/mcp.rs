use crate::{PostgresBackend, database_error, database_time, owner_transaction};
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::Postgres;
use zuno_application::{ApplicationError, authorization::CheckedApproval, mcp::*};
use zuno_types::identity::{GatewayId, OperationId, PrincipalKey};

#[derive(Clone)]
pub struct PostgresMcpStore {
    backend: PostgresBackend,
    gateway: GatewayId,
}
fn digest(admission: &McpAdmission) -> String {
    admission.digest()
}

impl PostgresBackend {
    pub fn mcp_operations(&self, gateway: GatewayId) -> PostgresMcpStore {
        PostgresMcpStore {
            backend: self.clone(),
            gateway,
        }
    }
    pub async fn mcp_for_approval(
        &self,
        viewer: &zuno_types::identity::PrincipalScope,
        id: &zuno_types::identity::ApprovalId,
    ) -> Result<(McpAdmission, bool), ApplicationError> {
        let mut tx = owner_transaction(&self.pool, &viewer.owner()).await?;
        let approval = crate::authorization::visible_approval_in(&mut tx, viewer, id).await?;
        let owner = approval.requester.owner();
        let row = query(
            "SELECT offer,offer_digest,admitted FROM zuno_enterprise_preview.gateway_mcp_operation
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND job_id=$4",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(approval.binding.operation_id.as_str())
        .bind(approval.binding.job_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?
        .ok_or(ApplicationError::NotFound)?;
        let admission: McpAdmission =
            serde_json::from_value(row.try_get("offer").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        admission.validate()?;
        if digest(&admission)
            != row
                .try_get::<String, _>("offer_digest")
                .map_err(database_error)?
            || admission.arguments_digest() != approval.binding.arguments_sha256
            || admission.resources_digest() != approval.binding.resources_sha256
            || admission.lease.owner != owner
        {
            return Err(ApplicationError::Conflict);
        }
        let admitted = row.try_get("admitted").map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        Ok((admission, admitted))
    }
}
impl PostgresMcpStore {
    pub async fn cancellations(
        &self,
        tenant: &zuno_types::identity::TenantId,
        limit: u32,
    ) -> Result<Vec<McpAdmission>, ApplicationError> {
        if !(1..=32).contains(&limit) {
            return Err(ApplicationError::Invalid(
                "invalid MCP cancellation bound".to_owned(),
            ));
        }
        let rows=query("SELECT principal_id,operation_id FROM zuno_enterprise_preview.gateway_mcp_cancellations($1,$2,$3)")
            .bind(tenant.as_str()).bind(self.gateway.as_str()).bind(limit as i32).fetch_all(&self.backend.pool).await.map_err(database_error)?;
        let mut output = Vec::new();
        let mut bytes = 2usize;
        for row in rows {
            let owner = PrincipalKey {
                tenant_id: tenant.clone(),
                principal_id: zuno_types::identity::PrincipalId::new(
                    row.try_get::<String, _>("principal_id")
                        .map_err(database_error)?,
                )
                .map_err(ApplicationError::storage)?,
            };
            let id = OperationId::new(
                row.try_get::<String, _>("operation_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?;
            let mut admission = self.offered(&owner, &id).await?;
            let mut tx = owner_transaction(&self.backend.pool, &owner).await?;
            let lease:Value=query_scalar("SELECT lease FROM zuno_enterprise_preview.gateway_mcp_attempt
                WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 ORDER BY attempt_id,checkpoint_version LIMIT 1")
                .bind(tenant.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str())
                .fetch_one(&mut *tx).await.map_err(database_error)?;
            let lease: zuno_application::runtime::ExecutionLease =
                serde_json::from_value(lease).map_err(ApplicationError::storage)?;
            if lease.owner != owner
                || lease.job_id != admission.lease.job_id
                || lease.session_id != admission.lease.session_id
            {
                return Err(ApplicationError::Conflict);
            }
            admission.lease = lease;
            let size = serde_json::to_vec(&admission)
                .map_err(ApplicationError::storage)?
                .len()
                + 1;
            if bytes + size > zuno_application::environment::wire::MAX_GATEWAY_FRAME_BYTES {
                break;
            }
            let now = database_time(&mut tx).await?;
            query("UPDATE zuno_enterprise_preview.gateway_mcp_cancellation SET time_polled=$4 WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3")
                .bind(tenant.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).bind(now).execute(&mut *tx).await.map_err(database_error)?;
            tx.commit().await.map_err(database_error)?;
            bytes += size;
            output.push(admission);
        }
        Ok(output)
    }
    pub async fn offer(&self, admission: &McpAdmission) -> Result<(), ApplicationError> {
        admission.validate()?;
        if admission.gateway_id != self.gateway {
            return Err(ApplicationError::Forbidden);
        }
        let owner = &admission.lease.owner;
        let mut tx = owner_transaction(&self.backend.pool, owner).await?;
        crate::runtime::verify_lease(&mut tx, &admission.lease).await?;
        crate::operation::identity_lock(&mut tx, owner, &admission.operation.id).await?;
        let collision:bool=query_scalar("SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.gateway_operation WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3)
            OR EXISTS(SELECT 1 FROM zuno_enterprise_preview.gateway_merge_operation WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3)
            OR EXISTS(SELECT 1 FROM zuno_enterprise_preview.gateway_edit_operation WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3)")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str())
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        if collision {
            return Err(ApplicationError::Conflict);
        }
        let expected = digest(admission);
        let old: Option<String> = query_scalar(
            "SELECT offer_digest FROM zuno_enterprise_preview.gateway_mcp_operation
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 FOR UPDATE",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(admission.operation.id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?;
        if let Some(old) = old {
            if old != expected {
                return Err(ApplicationError::Conflict);
            }
        } else {
            let now = database_time(&mut tx).await?;
            query("INSERT INTO zuno_enterprise_preview.gateway_mcp_operation
                (tenant_id,principal_id,operation_id,gateway_id,job_id,session_id,invocation_id,offer,offer_digest,time_created)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str()).bind(self.gateway.as_str())
                .bind(admission.lease.job_id.as_str()).bind(admission.lease.session_id.as_str()).bind(admission.operation.invocation_id.as_str())
                .bind(json!(admission)).bind(expected).bind(now).execute(&mut *tx).await.map_err(database_error)?;
        }
        tx.commit().await.map_err(database_error)
    }
    pub async fn offered(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<McpAdmission, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, owner).await?;
        let row = query(
            "SELECT offer,offer_digest FROM zuno_enterprise_preview.gateway_mcp_operation
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND gateway_id=$4",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(id.as_str())
        .bind(self.gateway.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?
        .ok_or(ApplicationError::NotFound)?;
        let admission: McpAdmission =
            serde_json::from_value(row.try_get("offer").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        admission.validate()?;
        if admission.lease.owner != *owner
            || admission.operation.id != *id
            || digest(&admission)
                != row
                    .try_get::<String, _>("offer_digest")
                    .map_err(database_error)?
        {
            return Err(ApplicationError::Conflict);
        }
        tx.commit().await.map_err(database_error)?;
        Ok(admission)
    }
    pub async fn complete(&self, completion: &McpCompletion) -> Result<(), ApplicationError> {
        completion.validate()?;
        let admission = &completion.admission;
        let owner = &admission.lease.owner;
        if admission.gateway_id != self.gateway {
            return Err(ApplicationError::Forbidden);
        }
        let mut tx = owner_transaction(&self.backend.pool, owner).await?;
        let job = crate::runtime::read_job(&mut tx, owner, admission.lease.job_id.as_str()).await?;
        crate::runtime::lock_job_session(&mut tx, &job).await?;
        let row=query("SELECT offer_digest,completion_digest FROM zuno_enterprise_preview.gateway_mcp_operation
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND gateway_id=$4 AND job_id=$5 AND admitted FOR UPDATE")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str()).bind(self.gateway.as_str())
            .bind(job.id.as_str()).fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
        if row
            .try_get::<String, _>("offer_digest")
            .map_err(database_error)?
            != digest(admission)
        {
            return Err(ApplicationError::Conflict);
        }
        let recorded:Value=query_scalar("SELECT lease FROM zuno_enterprise_preview.gateway_mcp_attempt
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND attempt_id=$4 AND checkpoint_version=$5")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str())
            .bind(admission.lease.attempt_id.as_str()).bind(i64::try_from(admission.lease.checkpoint_version).map_err(ApplicationError::storage)?)
            .fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::Forbidden)?;
        if recorded != json!(admission.lease) {
            return Err(ApplicationError::Conflict);
        }
        let hash = zuno_orchestration::sha256_json(&json!(completion));
        if let Some(old) = row
            .try_get::<Option<String>, _>("completion_digest")
            .map_err(database_error)?
        {
            if old != hash {
                return Err(ApplicationError::Conflict);
            }
        } else {
            let now = database_time(&mut tx).await?;
            query("UPDATE zuno_enterprise_preview.gateway_mcp_operation SET completion=$4,completion_digest=$5,time_completed=$6
                WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str())
                .bind(json!(completion)).bind(hash).bind(now).execute(&mut *tx).await.map_err(database_error)?;
            crate::session::emit(
                &mut tx,
                &job.principal,
                job.session_id.as_str(),
                "runtime.mcp.completed",
                json!({"operationID":admission.operation.id,"receipt":completion.receipt}),
            )
            .await?;
        }
        crate::runtime::waiting::operation_completed(&mut tx, &job, &admission.operation.id)
            .await?;
        tx.commit().await.map_err(database_error)
    }
}

pub(crate) async fn admit_in(
    tx: &mut Transaction<'_, Postgres>,
    checked: &CheckedApproval,
    admission: &McpAdmission,
) -> Result<(), ApplicationError> {
    admission.validate()?;
    if checked.lease.owner != admission.lease.owner
        || checked.lease.job_id != admission.lease.job_id
        || checked.lease.session_id != admission.lease.session_id
        || checked.lease.attempt_id != admission.lease.attempt_id
        || checked.lease.worker != admission.lease.worker
        || checked.lease.epoch != admission.lease.epoch
        || checked.lease.checkpoint_version != admission.lease.checkpoint_version
        || checked.binding.operation_id != admission.operation.id
        || checked.binding.invocation_id != admission.operation.invocation_id
        || checked.binding.arguments_sha256 != admission.arguments_digest()
        || checked.binding.resources_sha256 != admission.resources_digest()
        || checked.binding.effect != zuno_permission::enterprise::EffectKind::ExternalTool
    {
        return Err(ApplicationError::Forbidden);
    }
    let owner = &admission.lease.owner;
    let stored: String = query_scalar(
        "SELECT offer_digest FROM zuno_enterprise_preview.gateway_mcp_operation
        WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND gateway_id=$4 FOR UPDATE",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(admission.operation.id.as_str())
    .bind(admission.gateway_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_error)?
    .ok_or(ApplicationError::NotFound)?;
    if stored != digest(admission) {
        return Err(ApplicationError::Conflict);
    }
    let now = database_time(tx).await?;
    query("UPDATE zuno_enterprise_preview.gateway_mcp_operation SET admitted=true,time_admitted=COALESCE(time_admitted,$4)
        WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str()).bind(now)
        .execute(&mut **tx).await.map_err(database_error)?;
    query("INSERT INTO zuno_enterprise_preview.gateway_mcp_attempt(tenant_id,principal_id,operation_id,attempt_id,checkpoint_version,lease)
        VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str())
        .bind(admission.lease.attempt_id.as_str()).bind(i64::try_from(admission.lease.checkpoint_version).map_err(ApplicationError::storage)?)
        .bind(json!(admission.lease)).execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}

/// A gateway may execute an already admitted operation after its parent has
/// released the Worker slot. This never creates an admission or a new attempt.
pub(crate) async fn verify_start_in(
    tx: &mut Transaction<'_, Postgres>,
    admission: &McpAdmission,
) -> Result<zuno_application::runtime::RuntimeJob, ApplicationError> {
    admission.validate()?;
    let owner = &admission.lease.owner;
    let job = crate::runtime::read_job(tx, owner, admission.lease.job_id.as_str()).await?;
    crate::runtime::lock_job_session(tx, &job).await?;
    let job = crate::runtime::read_job(tx, owner, admission.lease.job_id.as_str()).await?;
    if job.session_id != admission.lease.session_id
        || !matches!(
            job.phase,
            zuno_application::runtime::JobPhase::Ready
                | zuno_application::runtime::JobPhase::Running
                | zuno_application::runtime::JobPhase::Waiting
        )
    {
        return Err(ApplicationError::Forbidden);
    }
    let stopped: bool = query_scalar(
        "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_stop
        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3)",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(job.id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    if stopped {
        return Err(ApplicationError::Forbidden);
    }
    let row=query("SELECT o.offer_digest,a.lease FROM zuno_enterprise_preview.gateway_mcp_operation o
        JOIN zuno_enterprise_preview.gateway_mcp_attempt a USING(tenant_id,principal_id,operation_id)
        WHERE o.tenant_id=$1 AND o.principal_id=$2 AND o.operation_id=$3 AND o.gateway_id=$4
          AND o.job_id=$5 AND o.admitted AND o.completion IS NULL
          AND a.attempt_id=$6 AND a.checkpoint_version=$7 FOR UPDATE OF o")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str())
        .bind(admission.gateway_id.as_str()).bind(job.id.as_str()).bind(admission.lease.attempt_id.as_str())
        .bind(i64::try_from(admission.lease.checkpoint_version).map_err(ApplicationError::storage)?)
        .fetch_optional(&mut **tx).await.map_err(database_error)?.ok_or(ApplicationError::Forbidden)?;
    if row
        .try_get::<String, _>("offer_digest")
        .map_err(database_error)?
        != admission.digest()
        || row.try_get::<Value, _>("lease").map_err(database_error)? != json!(admission.lease)
    {
        return Err(ApplicationError::Forbidden);
    }
    Ok(job)
}
