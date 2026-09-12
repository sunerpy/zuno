use super::*;
use serde::{Deserialize, Serialize};
use zuno_application::child::{
    ChildWorkspaceAssignment, ChildWorkspaceCompletion, ChildWorkspaceInfo, ChildWorkspaceReceipt,
};
use zuno_types::identity::GatewayId;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Admission {
    lease: ExecutionLease,
    assignment: ChildWorkspaceAssignment,
}

async fn authorized_parent(
    tx: &mut Transaction<'_, Postgres>,
    lease: &ExecutionLease,
) -> Result<RuntimeJob, ApplicationError> {
    let parent = verify_lease(tx, lease).await?;
    let access = crate::authorization::access_in(tx, &lease.owner).await?;
    if zuno_permission::enterprise::actor_denial(&access.policy, &access.member, &parent.principal)
        .is_some()
    {
        return Err(ApplicationError::Forbidden);
    }
    Ok(parent)
}

async fn preparation(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    id: &JobId,
) -> Result<Option<(Admission, Option<ChildWorkspaceReceipt>)>, ApplicationError> {
    let row=query("SELECT admission,admission_digest,receipt,receipt_digest FROM zuno_enterprise_preview.child_workspace_preparation
        WHERE tenant_id=$1 AND principal_id=$2 AND child_job_id=$3 FOR UPDATE")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str())
        .fetch_optional(&mut **tx).await.map_err(database_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let admission: Admission =
        serde_json::from_value(row.try_get("admission").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    if admission.lease.owner != *owner
        || admission.assignment.child_job_id != *id
        || row
            .try_get::<String, _>("admission_digest")
            .map_err(database_error)?
            != zuno_orchestration::sha256_json(&json!([
                owner,
                admission.lease.job_id,
                admission.assignment
            ]))
    {
        return Err(ApplicationError::Conflict);
    }
    let raw: Option<Value> = row.try_get("receipt").map_err(database_error)?;
    let receipt = raw
        .map(|raw| {
            if row
                .try_get::<Option<String>, _>("receipt_digest")
                .map_err(database_error)?
                .as_deref()
                != Some(zuno_orchestration::sha256_json(&raw).as_str())
            {
                return Err(ApplicationError::Conflict);
            }
            serde_json::from_value(raw).map_err(ApplicationError::storage)
        })
        .transpose()?;
    Ok(Some((admission, receipt)))
}

pub(crate) async fn merge_source_in(
    tx: &mut Transaction<'_, Postgres>,
    lease: &ExecutionLease,
    child: &JobId,
) -> Result<zuno_application::workspace_merge::WorkspaceMergeSource, ApplicationError> {
    let parent = authorized_parent(tx, lease).await?;
    let job = read_job(tx, &lease.owner, child.as_str()).await?;
    if job.id == parent.id || job.phase != JobPhase::Completed {
        return Err(ApplicationError::Conflict);
    }
    super::super::control::lock_session(tx, &job).await?;
    let current_input =
        super::super::input_version(tx, &lease.owner, job.session_id.as_str()).await?;
    if unsigned(current_input)? != job.input_version {
        return Err(ApplicationError::Conflict);
    }
    let (source_admission, source_receipt) = preparation(tx, &lease.owner, child)
        .await?
        .ok_or(ApplicationError::Forbidden)?;
    let source_receipt = source_receipt.ok_or(ApplicationError::Conflict)?;
    if source_receipt.target.spec.session_id != job.session_id {
        return Err(ApplicationError::Conflict);
    }
    let gateway = source_admission.assignment.gateway_id;
    let mut cursor = child.clone();
    let mut seen = std::collections::BTreeSet::new();
    let base = loop {
        if seen.len() >= 64 || !seen.insert(cursor.clone()) {
            return Err(ApplicationError::Forbidden);
        }
        let record = read(tx, &lease.owner, &cursor).await?;
        if !matches!(record.state.as_str(), "completed" | "consumed")
            || record.ticket.workspace == ChildWorkspaceState::ModelOnly
        {
            return Err(ApplicationError::Forbidden);
        }
        let (admission, receipt) = preparation(tx, &lease.owner, &cursor)
            .await?
            .ok_or(ApplicationError::Forbidden)?;
        if admission.assignment.gateway_id != gateway {
            return Err(ApplicationError::Forbidden);
        }
        let receipt = receipt.ok_or(ApplicationError::Conflict)?;
        if record.parent_job_id == parent.id {
            if record.parent_session_id != parent.session_id
                || admission.assignment.parent.session_id != parent.session_id
            {
                return Err(ApplicationError::Forbidden);
            }
            break receipt.snapshot.ok_or(ApplicationError::Forbidden)?;
        }
        cursor = record.parent_job_id;
    };
    let source = zuno_application::workspace_merge::WorkspaceMergeSource {
        child_job_id: job.id,
        child_session_id: job.session_id,
        child_configuration: job.configuration,
        child_input_version: job.input_version,
        gateway_id: gateway,
        source_environment: source_receipt.target.spec,
        base,
    };
    verify_lease(tx, lease).await?;
    Ok(source)
}

impl PostgresRuntimeStore {
    /// Resolve the earliest fork below this parent as the common baseline, so
    /// nested Agent/Workflow contributions include their inherited changes.
    /// A child with new input or without a provable fork cannot be published.
    pub async fn workspace_merge_source(
        &self,
        lease: &ExecutionLease,
        child: &JobId,
    ) -> Result<zuno_application::workspace_merge::WorkspaceMergeSource, ApplicationError> {
        self.check_owner(&lease.owner)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let source = merge_source_in(&mut tx, lease, child).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(source)
    }

    pub async fn child_workspace(
        &self,
        lease: &ExecutionLease,
        child: &JobId,
    ) -> Result<ChildWorkspaceInfo, ApplicationError> {
        self.check_owner(&lease.owner)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let mut parent = authorized_parent(&mut tx, lease).await?;
        let record = read(&mut tx, &lease.owner, child).await?;
        if record.parent_job_id != parent.id {
            parent =
                super::super::workflow::preparation_parent(&mut tx, &parent, &record.parent_job_id)
                    .await?
                    .ok_or(ApplicationError::Forbidden)?;
        }
        if record.parent_job_id != parent.id
            || record.parent_session_id != parent.session_id
            || record.ticket.workspace == ChildWorkspaceState::ModelOnly
            || record.state == "cancelled"
        {
            return Err(ApplicationError::Forbidden);
        }
        let receipt = preparation(&mut tx, &lease.owner, child)
            .await?
            .and_then(|(_, receipt)| receipt);
        let info = ChildWorkspaceInfo {
            parent_session_id: parent.session_id,
            parent_configuration: parent.configuration,
            child_session_id: record.ticket.session_id,
            configuration: record.configuration,
            resume: record.invocation.resume_session_id.is_some(),
            receipt,
        };
        tx.commit().await.map_err(database_error)?;
        Ok(info)
    }

    /// Only the configured resolver constructs the target spec. Repeated
    /// admission can renew Worker authority without changing the operation.
    pub async fn admit_child_workspace(
        &self,
        lease: &ExecutionLease,
        assignment: &ChildWorkspaceAssignment,
    ) -> Result<(), ApplicationError> {
        assignment.parent.validate()?;
        assignment.target.validate()?;
        self.check_owner(&lease.owner)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let mut parent = authorized_parent(&mut tx, lease).await?;
        let record = read(&mut tx, &lease.owner, &assignment.child_job_id).await?;
        if record.parent_job_id != parent.id {
            parent =
                super::super::workflow::preparation_parent(&mut tx, &parent, &record.parent_job_id)
                    .await?
                    .ok_or(ApplicationError::Forbidden)?;
        }
        if record.parent_job_id != parent.id
            || record.parent_session_id != parent.session_id
            || record.ticket.workspace == ChildWorkspaceState::ModelOnly
            || record.state == "cancelled"
            || assignment.parent.session_id != parent.session_id
            || assignment.target.session_id != record.ticket.session_id
            || assignment.parent.id == assignment.target.id
            || assignment.resume != record.invocation.resume_session_id.is_some()
        {
            return Err(ApplicationError::Forbidden);
        }
        if let Some((prior, _)) =
            preparation(&mut tx, &lease.owner, &assignment.child_job_id).await?
        {
            if prior.assignment != *assignment {
                return Err(ApplicationError::Conflict);
            }
        } else {
            let now = database_time(&mut tx).await?;
            query("INSERT INTO zuno_enterprise_preview.child_workspace_preparation(tenant_id,principal_id,child_job_id,gateway_id,admission,admission_digest,time_admitted)
                VALUES($1,$2,$3,$4,$5,$6,$7)")
                .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(assignment.child_job_id.as_str())
                .bind(assignment.gateway_id.as_str()).bind(json!(Admission{lease:lease.clone(),assignment:assignment.clone()}))
                .bind(zuno_orchestration::sha256_json(&json!([lease.owner,lease.job_id,assignment]))).bind(now)
                .execute(&mut *tx).await.map_err(database_error)?;
        }
        verify_lease(&mut tx, lease).await?;
        tx.commit().await.map_err(database_error)
    }

    /// A truthful gateway receipt is retained after the admitting lease expires.
    /// It never activates a child or restores a revoked parent execution right.
    pub async fn complete_child_workspace(
        &self,
        gateway: &GatewayId,
        completion: &ChildWorkspaceCompletion,
    ) -> Result<(), ApplicationError> {
        let owner = &completion.lease.owner;
        self.check_owner(owner)?;
        let mut tx = owner_transaction(&self.pool, owner).await?;
        let receipt = &completion.receipt;
        let (admission, prior) = preparation(&mut tx, owner, &receipt.child_job_id)
            .await?
            .ok_or(ApplicationError::Forbidden)?;
        let expected = &admission.assignment;
        if expected.gateway_id != *gateway
            || admission.lease.job_id != completion.lease.job_id
            || admission.lease.session_id != completion.lease.session_id
            || receipt.parent_environment_id != expected.parent.id
            || receipt.target.owner != *owner
            || receipt.target.spec != expected.target
            || receipt.target.revision == 0
            || expected.resume != receipt.snapshot.is_none()
        {
            return Err(ApplicationError::Forbidden);
        }
        if let Some(snapshot) = &receipt.snapshot {
            let expected_id = format!("child-{}", receipt.child_job_id);
            if snapshot.id.as_str() != expected_id
                || snapshot.environment_id != expected.parent.id
                || snapshot.revision == 0
                || snapshot.bytes == 0
                || snapshot.bytes > 512 * 1024 * 1024
                || snapshot.sha256.len() != 64
                || !snapshot
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || receipt.target.revision != 1
            {
                return Err(ApplicationError::Conflict);
            }
        }
        if let Some(prior) = prior {
            if prior != *receipt {
                return Err(ApplicationError::Conflict);
            }
        } else {
            let now = database_time(&mut tx).await?;
            query("UPDATE zuno_enterprise_preview.child_workspace_preparation SET receipt=$4,receipt_digest=$5,time_completed=$6
                WHERE tenant_id=$1 AND principal_id=$2 AND child_job_id=$3 AND receipt IS NULL")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(receipt.child_job_id.as_str()).bind(json!(receipt))
                .bind(zuno_orchestration::sha256_json(&json!(receipt))).bind(now).execute(&mut *tx).await.map_err(database_error)?;
            query("UPDATE zuno_enterprise_preview.runtime_child SET workspace_state='ready',time_updated=$4
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND workspace_policy='fork_parent' AND workspace_state='pending'")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(receipt.child_job_id.as_str()).bind(now)
                .execute(&mut *tx).await.map_err(database_error)?;
        }
        tx.commit().await.map_err(database_error)
    }

    /// Root Jobs use their configured environment. Child Jobs require an
    /// admitted workspace and never fall back to a newly empty volume.
    pub async fn execution_workspace(
        &self,
        lease: &ExecutionLease,
    ) -> Result<Option<(GatewayId, ChildWorkspaceReceipt)>, ApplicationError> {
        self.check_owner(&lease.owner)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let _ = authorized_parent(&mut tx, lease).await?;
        let exists:bool=query_scalar("SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_child WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3)")
            .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.job_id.as_str())
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        let value = if exists {
            let (admission, receipt) = preparation(&mut tx, &lease.owner, &lease.job_id)
                .await?
                .ok_or(ApplicationError::Forbidden)?;
            Some((
                admission.assignment.gateway_id,
                receipt.ok_or(ApplicationError::Forbidden)?,
            ))
        } else {
            None
        };
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
}
