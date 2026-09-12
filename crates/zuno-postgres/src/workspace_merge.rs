//! The authenticated gateway supplies observations; this owner checks lineage,
//! current authorization and the exact immutable offer before operation admission.
use crate::{PostgresBackend, database_error, database_time, owner_transaction};
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::Postgres;
use zuno_application::{
    ApplicationError,
    authorization::CheckedApproval,
    workspace_merge::{WorkspaceMergeAdmission, WorkspaceMergeCompletion},
};
use zuno_types::identity::{GatewayId, OperationId, PrincipalKey};

#[derive(Clone)]
pub struct PostgresWorkspaceMergeStore {
    backend: PostgresBackend,
    gateway: GatewayId,
}

impl PostgresBackend {
    /// Reuses the approval viewer policy in the same transaction as the
    /// immutable offer read. Requester-only approvals never disclose another
    /// user's workspace to an ordinary organization member.
    pub async fn workspace_merge_for_approval(
        &self,
        viewer: &zuno_types::identity::PrincipalScope,
        approval: &zuno_types::identity::ApprovalId,
    ) -> Result<(WorkspaceMergeAdmission, bool), ApplicationError> {
        let mut tx = owner_transaction(&self.pool, &viewer.owner()).await?;
        let approval = crate::authorization::visible_approval_in(&mut tx, viewer, approval).await?;
        let owner = approval.requester.owner();
        let row=query("SELECT offer,offer_digest,admitted FROM zuno_enterprise_preview.gateway_merge_operation
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND job_id=$4")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(approval.binding.operation_id.as_str())
            .bind(approval.binding.job_id.as_str()).fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
        let admission: WorkspaceMergeAdmission =
            serde_json::from_value(row.try_get("offer").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        if admission.lease.owner != owner
            || admission.operation.id != approval.binding.operation_id
            || admission.operation.plan.digest() != approval.binding.arguments_sha256
            || admission.resources_digest() != approval.binding.resources_sha256
            || digest(&admission)
                != row
                    .try_get::<String, _>("offer_digest")
                    .map_err(database_error)?
        {
            return Err(ApplicationError::Conflict);
        }
        let admitted = row.try_get("admitted").map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        Ok((admission, admitted))
    }
    pub async fn workspace_merge_content(
        &self,
        viewer: &zuno_types::identity::PrincipalScope,
        request: &zuno_application::workspace_merge::MergeContentRequest,
    ) -> Result<
        (
            GatewayId,
            zuno_application::workspace_merge::MergeContentContext,
        ),
        ApplicationError,
    > {
        use zuno_application::workspace_merge::{MergeContentContext, MergeContentSide};
        let (admission, _) = self
            .workspace_merge_for_approval(viewer, &request.approval_id)
            .await?;
        let change = admission
            .operation
            .plan
            .changes
            .iter()
            .find(|change| change.path == request.path)
            .ok_or(ApplicationError::NotFound)?;
        let (snapshot, expected) = match request.side {
            MergeContentSide::Base => (admission.operation.base.clone(), change.base.clone()),
            MergeContentSide::Parent => (admission.operation.parent.clone(), change.parent.clone()),
            MergeContentSide::Child => (admission.operation.child.clone(), change.child.clone()),
        };
        let expected = expected.ok_or(ApplicationError::NotFound)?;
        if matches!(
            expected,
            zuno_application::workspace_merge::WorkspaceEntry::Directory { .. }
        ) {
            return Err(ApplicationError::Invalid(
                "directory metadata has no file body".to_owned(),
            ));
        }
        Ok((
            admission.gateway_id.clone(),
            MergeContentContext {
                gateway_id: admission.gateway_id,
                owner: admission.lease.owner,
                snapshot,
                path: request.path.clone(),
                expected,
            },
        ))
    }
}
fn digest(admission: &WorkspaceMergeAdmission) -> String {
    zuno_orchestration::sha256_json(&json!([
        admission.gateway_id,
        admission.lease.owner,
        admission.lease.job_id,
        admission.lease.session_id,
        admission.environment,
        admission.source,
        admission.operation
    ]))
}
async fn verify_source(
    tx: &mut Transaction<'_, Postgres>,
    admission: &WorkspaceMergeAdmission,
) -> Result<(), ApplicationError> {
    admission.operation.validate()?;
    let source =
        crate::runtime::merge_source_in(tx, &admission.lease, &admission.operation.child_job_id)
            .await?;
    if source != admission.source
        || source.base != admission.operation.base
        || source.source_environment.id != admission.operation.child.environment_id
        || admission.environment.owner != admission.lease.owner
        || admission.environment.spec.session_id != admission.lease.session_id
        || admission.environment.spec.id != admission.operation.environment_id
        || admission.environment.revision != admission.operation.expected_revision
        || admission.source.base.environment_id != admission.environment.spec.id
    {
        return Err(ApplicationError::Forbidden);
    }
    if source.gateway_id != admission.gateway_id {
        crate::workspace_transfer::require_snapshot_in(
            tx,
            &admission.lease.owner,
            &admission.lease.job_id,
            &source.gateway_id,
            &admission.gateway_id,
            &admission.operation.child,
        )
        .await?;
    }
    Ok(())
}
impl PostgresWorkspaceMergeStore {
    pub async fn cancellations(
        &self,
        tenant: &zuno_types::identity::TenantId,
        limit: u32,
    ) -> Result<Vec<WorkspaceMergeAdmission>, ApplicationError> {
        if !(1..=32).contains(&limit) {
            return Err(ApplicationError::Invalid(
                "invalid merge cancellation limit".to_owned(),
            ));
        }
        let rows=query("SELECT principal_id,operation_id FROM zuno_enterprise_preview.gateway_merge_cancellations($1,$2,$3)")
            .bind(tenant.as_str()).bind(self.gateway.as_str()).bind(limit as i32)
            .fetch_all(&self.backend.pool).await.map_err(database_error)?;
        let mut result = Vec::new();
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
            let lease:Value=query_scalar("SELECT lease FROM zuno_enterprise_preview.gateway_merge_attempt WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 ORDER BY attempt_id,checkpoint_version LIMIT 1")
                .bind(tenant.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).fetch_one(&mut *tx).await.map_err(database_error)?;
            let lease: zuno_application::runtime::ExecutionLease =
                serde_json::from_value(lease).map_err(ApplicationError::storage)?;
            if lease.owner != owner
                || lease.job_id != admission.lease.job_id
                || lease.session_id != admission.lease.session_id
            {
                return Err(ApplicationError::Conflict);
            }
            admission.lease = lease;
            let length = serde_json::to_vec(&admission)
                .map_err(ApplicationError::storage)?
                .len()
                + 1;
            if bytes + length > zuno_application::environment::wire::MAX_GATEWAY_FRAME_BYTES {
                if result.is_empty() {
                    return Err(ApplicationError::Invalid(
                        "merge cancellation exceeds transport bounds".to_owned(),
                    ));
                }
                break;
            }
            query("UPDATE zuno_enterprise_preview.gateway_merge_cancellation SET time_polled=$4 WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3")
                .bind(tenant.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).bind(database_time(&mut tx).await?)
                .execute(&mut *tx).await.map_err(database_error)?;
            tx.commit().await.map_err(database_error)?;
            result.push(admission);
            bytes += length;
        }
        Ok(result)
    }

    pub async fn complete(
        &self,
        completion: &WorkspaceMergeCompletion,
    ) -> Result<(), ApplicationError> {
        completion.validate()?;
        let owner = &completion.lease.owner;
        let mut tx = owner_transaction(&self.backend.pool, owner).await?;
        let job =
            crate::runtime::read_job(&mut tx, owner, completion.lease.job_id.as_str()).await?;
        crate::runtime::lock_job_session(&mut tx, &job).await?;
        let row=query("SELECT offer,offer_digest,completion_digest FROM zuno_enterprise_preview.gateway_merge_operation
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND gateway_id=$4 AND job_id=$5 AND session_id=$6 AND admitted FOR UPDATE")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(completion.operation.id.as_str()).bind(self.gateway.as_str())
            .bind(completion.lease.job_id.as_str()).bind(completion.lease.session_id.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
        let admission: WorkspaceMergeAdmission =
            serde_json::from_value(row.try_get("offer").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        if admission.operation != completion.operation
            || admission.lease.owner != *owner
            || row
                .try_get::<String, _>("offer_digest")
                .map_err(database_error)?
                != digest(&admission)
        {
            return Err(ApplicationError::Conflict);
        }
        let recorded:Value=query_scalar("SELECT lease FROM zuno_enterprise_preview.gateway_merge_attempt
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND attempt_id=$4 AND checkpoint_version=$5")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(completion.operation.id.as_str())
            .bind(completion.lease.attempt_id.as_str()).bind(i64::try_from(completion.lease.checkpoint_version).map_err(ApplicationError::storage)?)
            .fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::Forbidden)?;
        if recorded != json!(completion.lease) {
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
            query("UPDATE zuno_enterprise_preview.gateway_merge_operation SET completion=$4,completion_digest=$5,time_completed=$6
                WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(completion.operation.id.as_str())
                .bind(json!(completion)).bind(hash).bind(database_time(&mut tx).await?)
                .execute(&mut *tx).await.map_err(database_error)?;
            crate::session::emit(
                &mut tx,
                &job.principal,
                job.session_id.as_str(),
                "runtime.workspace_merge.completed",
                json!({"operationID":completion.operation.id,"receipt":completion.receipt}),
            )
            .await?;
        }
        crate::runtime::waiting::operation_completed(&mut tx, &job, &completion.operation.id)
            .await?;
        tx.commit().await.map_err(database_error)
    }

    pub(crate) fn new(backend: PostgresBackend, gateway: GatewayId) -> Self {
        Self { backend, gateway }
    }
    /// An immutable preview is inert until a separate current approval is
    /// checked in the atomic execution-admission transaction.
    pub async fn offer(&self, admission: &WorkspaceMergeAdmission) -> Result<(), ApplicationError> {
        if admission.gateway_id != self.gateway {
            return Err(ApplicationError::Forbidden);
        }
        let owner = &admission.lease.owner;
        let mut tx = owner_transaction(&self.backend.pool, owner).await?;
        verify_source(&mut tx, admission).await?;
        crate::operation::identity_lock(&mut tx, owner, &admission.operation.id).await?;
        let command: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.gateway_operation
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3)",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(admission.operation.id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        if command {
            return Err(ApplicationError::Conflict);
        }
        let expected = digest(admission);
        let old: Option<String> = query_scalar(
            "SELECT offer_digest FROM zuno_enterprise_preview.gateway_merge_operation
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
            query("INSERT INTO zuno_enterprise_preview.gateway_merge_operation
                (tenant_id,principal_id,operation_id,gateway_id,job_id,session_id,invocation_id,child_job_id,offer,offer_digest,time_created)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str())
                .bind(self.gateway.as_str()).bind(admission.lease.job_id.as_str()).bind(admission.lease.session_id.as_str())
                .bind(admission.operation.invocation_id.as_str()).bind(admission.operation.child_job_id.as_str())
                .bind(json!(admission)).bind(expected).bind(database_time(&mut tx).await?)
                .execute(&mut *tx).await.map_err(database_error)?;
        }
        tx.commit().await.map_err(database_error)
    }
    pub async fn offered(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<WorkspaceMergeAdmission, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, owner).await?;
        let row=query("SELECT offer,offer_digest,gateway_id FROM zuno_enterprise_preview.gateway_merge_operation
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
        let admission: WorkspaceMergeAdmission =
            serde_json::from_value(row.try_get("offer").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        if admission.lease.owner != *owner
            || admission.gateway_id != self.gateway
            || admission.operation.id != *id
            || row
                .try_get::<String, _>("gateway_id")
                .map_err(database_error)?
                != self.gateway.as_str()
            || row
                .try_get::<String, _>("offer_digest")
                .map_err(database_error)?
                != digest(&admission)
        {
            return Err(ApplicationError::Conflict);
        }
        tx.commit().await.map_err(database_error)?;
        Ok(admission)
    }
    pub async fn completion(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<Option<WorkspaceMergeCompletion>, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, owner).await?;
        let row=query("SELECT completion,completion_digest FROM zuno_enterprise_preview.gateway_merge_operation
            WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3 AND gateway_id=$4")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).bind(self.gateway.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
        let raw: Option<Value> = row.try_get("completion").map_err(database_error)?;
        let hash: Option<String> = row.try_get("completion_digest").map_err(database_error)?;
        if raw.as_ref().map(zuno_orchestration::sha256_json) != hash {
            return Err(ApplicationError::Conflict);
        }
        let result = raw
            .map(serde_json::from_value)
            .transpose()
            .map_err(ApplicationError::storage)?;
        tx.commit().await.map_err(database_error)?;
        Ok(result)
    }
}

pub(crate) async fn admit_in(
    tx: &mut Transaction<'_, Postgres>,
    checked: &CheckedApproval,
    admission: &WorkspaceMergeAdmission,
) -> Result<(), ApplicationError> {
    verify_source(tx, admission).await?;
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
        || checked.binding.effect != zuno_permission::enterprise::EffectKind::FileWrite
    {
        return Err(ApplicationError::Forbidden);
    }
    let owner = &admission.lease.owner;
    let expected = digest(admission);
    let row = query(
        "SELECT offer_digest FROM zuno_enterprise_preview.gateway_merge_operation
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
    if row
        .try_get::<String, _>("offer_digest")
        .map_err(database_error)?
        != expected
    {
        return Err(ApplicationError::Conflict);
    }
    query("UPDATE zuno_enterprise_preview.gateway_merge_operation SET admitted=true,time_admitted=COALESCE(time_admitted,$4)
        WHERE tenant_id=$1 AND principal_id=$2 AND operation_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str()).bind(database_time(tx).await?)
        .execute(&mut **tx).await.map_err(database_error)?;
    query("INSERT INTO zuno_enterprise_preview.gateway_merge_attempt
        (tenant_id,principal_id,operation_id,attempt_id,checkpoint_version,lease) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(admission.operation.id.as_str())
        .bind(admission.lease.attempt_id.as_str()).bind(i64::try_from(admission.lease.checkpoint_version).map_err(ApplicationError::storage)?)
        .bind(json!(admission.lease)).execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}
