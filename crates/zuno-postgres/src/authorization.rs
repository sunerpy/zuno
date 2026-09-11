mod administration;
mod approvals;
mod decisions;

use crate::runtime::verify_lease;
use crate::session::emit;
use crate::{database_error, database_time, owner_transaction, set_owner};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::{PgPool, Postgres};
use std::num::NonZeroU64;
use uuid::Uuid;
use zuno_application::ApplicationError;
use zuno_application::authorization::{
    AnswerApproval, ApprovalAnswer, ApprovalBinding, ApprovalProposal, ApprovalRecord,
    ApprovalState, CheckedApproval, OrganizationAccess, OrganizationMutationReceipt,
    OrganizationStore, UpdateOrganizationMember, UpdateOrganizationPolicy,
};
use zuno_application::runtime::ExecutionLease;
use zuno_permission::enterprise::{
    ApprovalAudience, EnterpriseDecision, OrganizationMember, OrganizationPolicy, OrganizationRole,
    actor_denial, can_approve, evaluate_enterprise,
};
use zuno_types::identity::{
    ApprovalId, PrincipalId, PrincipalKey, PrincipalKind, PrincipalScope, TenantId,
};

#[derive(Clone)]
pub struct PostgresOrganizationStore {
    pool: PgPool,
    tenant: TenantId,
}
impl PostgresOrganizationStore {
    pub(crate) fn new(pool: PgPool, tenant: TenantId) -> Self {
        Self { pool, tenant }
    }
    fn check_tenant(&self, tenant: &TenantId) -> Result<(), ApplicationError> {
        if tenant != &self.tenant {
            return Err(ApplicationError::NotFound);
        }
        Ok(())
    }
}

/// Explicit setup with the schema-owner credential. Repeating setup never
/// reinstalls a revoked administrator or overwrites an existing policy.
pub async fn bootstrap_organization(
    migration_pool: &PgPool,
    policy: &OrganizationPolicy,
    administrator: &PrincipalKey,
) -> Result<bool, ApplicationError> {
    if !policy.is_valid()
        || policy.revision.get() != 1
        || administrator.tenant_id != policy.tenant_id
    {
        return Err(ApplicationError::Invalid(
            "invalid organization bootstrap".to_owned(),
        ));
    }
    let mut tx = owner_transaction(migration_pool, administrator).await?;
    let owner: bool = query_scalar(
        "SELECT nspowner=(SELECT oid FROM pg_roles WHERE rolname=current_user)
         FROM pg_namespace WHERE nspname='zuno_enterprise_preview'",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(database_error)?;
    if !owner {
        return Err(ApplicationError::Forbidden);
    }
    let created = query(
        "INSERT INTO zuno_enterprise_preview.organization_policy(
           tenant_id,revision,allowed_apps,approval_apps,auto_read_apps,approval_lifetime_seconds)
         VALUES($1,1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
    )
    .bind(policy.tenant_id.as_str())
    .bind(json!(policy.allowed_apps))
    .bind(json!(policy.approval_apps))
    .bind(json!(policy.auto_read_apps))
    .bind(i32::try_from(policy.approval_lifetime_seconds).map_err(ApplicationError::storage)?)
    .execute(&mut *tx)
    .await
    .map_err(database_error)?
    .rows_affected()
        == 1;
    if created {
        query(
            "INSERT INTO zuno_enterprise_preview.organization_member(tenant_id,principal_id,role,active)
             VALUES($1,$2,'administrator',true)",
        ).bind(administrator.tenant_id.as_str()).bind(administrator.principal_id.as_str())
            .execute(&mut *tx).await.map_err(database_error)?;
        let actor: String = query_scalar("SELECT current_user::text")
            .fetch_one(&mut *tx)
            .await
            .map_err(database_error)?;
        let time = database_time(&mut tx).await?;
        query(
            "INSERT INTO zuno_enterprise_preview.organization_audit(tenant_id,id,actor,type,data,time_created)
             VALUES($1,$2,$3,'organization.bootstrapped',$4,$5)",
        ).bind(policy.tenant_id.as_str()).bind(format!("org_evt_{}",Uuid::new_v4().simple()))
            .bind(json!({"kind":"migration","databaseRole":actor})).bind(json!({"administrator":administrator,"policy":policy}))
            .bind(time).execute(&mut *tx).await.map_err(database_error)?;
    }
    tx.commit().await.map_err(database_error)?;
    Ok(created)
}

pub(crate) async fn access_in(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
) -> Result<OrganizationAccess, ApplicationError> {
    let row = query(
        "SELECT revision,allowed_apps,approval_apps,auto_read_apps,approval_lifetime_seconds
         FROM zuno_enterprise_preview.organization_policy WHERE tenant_id=$1 FOR SHARE",
    )
    .bind(owner.tenant_id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    let revision = nonzero(row.try_get("revision").map_err(database_error)?)?;
    let policy = OrganizationPolicy {
        tenant_id: owner.tenant_id.clone(),
        revision,
        allowed_apps: serde_json::from_value(row.try_get("allowed_apps").map_err(database_error)?)
            .map_err(ApplicationError::storage)?,
        approval_apps: serde_json::from_value(
            row.try_get("approval_apps").map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        auto_read_apps: serde_json::from_value(
            row.try_get("auto_read_apps").map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        approval_lifetime_seconds: u32::try_from(
            row.try_get::<i32, _>("approval_lifetime_seconds")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
    };
    if !policy.is_valid() {
        return Err(ApplicationError::Forbidden);
    }
    let row = query(
        "SELECT role,active FROM zuno_enterprise_preview.organization_member
         WHERE tenant_id=$1 AND principal_id=$2 FOR SHARE",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    let member = OrganizationMember {
        owner: owner.clone(),
        active: row.try_get("active").map_err(database_error)?,
        role: serde_json::from_value(json!(
            row.try_get::<String, _>("role").map_err(database_error)?
        ))
        .map_err(ApplicationError::storage)?,
    };
    Ok(OrganizationAccess { policy, member })
}

fn nonzero(value: i64) -> Result<NonZeroU64, ApplicationError> {
    NonZeroU64::new(u64::try_from(value).map_err(ApplicationError::storage)?).ok_or_else(|| {
        ApplicationError::storage(std::io::Error::other("invalid stored policy revision"))
    })
}
fn text<T: serde::Serialize>(value: &T) -> Result<String, ApplicationError> {
    serde_json::to_value(value)
        .map_err(ApplicationError::storage)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            ApplicationError::Invalid("expected a typed string discriminator".to_owned())
        })
}

async fn record_in(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    id: &str,
    lock: bool,
) -> Result<ApprovalRecord, ApplicationError> {
    let sql = if lock {
        "SELECT * FROM zuno_enterprise_preview.operation_approval WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 FOR UPDATE"
    } else {
        "SELECT * FROM zuno_enterprise_preview.operation_approval WHERE tenant_id=$1 AND principal_id=$2 AND id=$3"
    };
    let row = query(sql)
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(database_error)?;
    let binding: ApprovalBinding =
        serde_json::from_value(row.try_get("binding").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    binding.validate()?;
    let requester: PrincipalScope =
        serde_json::from_value(row.try_get("requester").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let policy_revision = nonzero(row.try_get("policy_revision").map_err(database_error)?)?;
    if requester.owner() != *owner || requester.policy_revision() != policy_revision {
        return Err(ApplicationError::storage(std::io::Error::other(
            "approval ownership or revision disagrees",
        )));
    }
    let decided_by: Option<Value> = row.try_get("decided_by").map_err(database_error)?;
    let decided_by = decided_by
        .map(serde_json::from_value::<PrincipalKey>)
        .transpose()
        .map_err(ApplicationError::storage)?;
    if decided_by
        .as_ref()
        .is_some_and(|actor| actor.tenant_id != owner.tenant_id)
    {
        return Err(ApplicationError::storage(std::io::Error::other(
            "approval decision belongs to another tenant",
        )));
    }
    Ok(ApprovalRecord {
        id: ApprovalId::new(id).map_err(ApplicationError::storage)?,
        binding,
        requester,
        policy_revision,
        audience: serde_json::from_value(json!(
            row.try_get::<String, _>("audience")
                .map_err(database_error)?
        ))
        .map_err(ApplicationError::storage)?,
        state: serde_json::from_value(json!(
            row.try_get::<String, _>("state").map_err(database_error)?
        ))
        .map_err(ApplicationError::storage)?,
        presentation: row.try_get("presentation").map_err(database_error)?,
        decided_by,
        created_at_ms: row.try_get("created_at").map_err(database_error)?,
        expires_at_ms: row.try_get("expires_at").map_err(database_error)?,
        decided_at_ms: row.try_get("decided_at").map_err(database_error)?,
    })
}

async fn coordinates(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &TenantId,
    id: &ApprovalId,
) -> Result<(PrincipalKey, String), ApplicationError> {
    let row = query(
        "SELECT principal_id,session_id FROM zuno_enterprise_preview.approval_coordinates($1,$2)",
    )
    .bind(tenant.as_str())
    .bind(id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    Ok((
        PrincipalKey {
            tenant_id: tenant.clone(),
            principal_id: PrincipalId::new(
                row.try_get::<String, _>("principal_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?,
        },
        row.try_get("session_id").map_err(database_error)?,
    ))
}

async fn lock_session(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    session: &str,
) -> Result<(), ApplicationError> {
    query("SELECT id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 FOR UPDATE")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session)
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    Ok(())
}

fn viewer_active(access: &OrganizationAccess, actor: &PrincipalScope) -> bool {
    actor.kind() == PrincipalKind::User
        && actor_denial(&access.policy, &access.member, actor).is_none()
}

async fn invalidate_in(
    tx: &mut Transaction<'_, Postgres>,
    record: &ApprovalRecord,
    state: ApprovalState,
    reason: &str,
) -> Result<(), ApplicationError> {
    let owner = record.requester.owner();
    query("UPDATE zuno_enterprise_preview.operation_approval SET state=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(record.id.as_str()).bind(text(&state)?)
        .execute(&mut **tx).await.map_err(database_error)?;
    emit(tx,&record.requester,record.binding.session_id.as_str(),"authorization.approval.invalidated",
        json!({"approvalID":record.id,"operationID":record.binding.operation_id,"state":state,"reason":reason})).await?;
    Ok(())
}

#[async_trait]
impl OrganizationStore for PostgresOrganizationStore {
    async fn update_member(
        &self,
        actor: &PrincipalScope,
        request: UpdateOrganizationMember,
    ) -> Result<OrganizationMutationReceipt, ApplicationError> {
        administration::member(self, actor, request).await
    }
    async fn update_policy(
        &self,
        actor: &PrincipalScope,
        request: UpdateOrganizationPolicy,
    ) -> Result<OrganizationMutationReceipt, ApplicationError> {
        administration::policy(self, actor, request).await
    }
    async fn access(&self, owner: &PrincipalKey) -> Result<OrganizationAccess, ApplicationError> {
        self.check_tenant(&owner.tenant_id)?;
        let mut tx = owner_transaction(&self.pool, owner).await?;
        let result = access_in(&mut tx, owner).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(result)
    }
    async fn admit(
        &self,
        lease: &ExecutionLease,
        proposal: ApprovalProposal,
    ) -> Result<ApprovalRecord, ApplicationError> {
        approvals::admit(self, lease, proposal).await
    }
    async fn approval(
        &self,
        viewer: &PrincipalScope,
        id: &ApprovalId,
    ) -> Result<ApprovalRecord, ApplicationError> {
        decisions::read(self, viewer, id).await
    }
    async fn answer(
        &self,
        actor: &PrincipalScope,
        request: AnswerApproval,
    ) -> Result<ApprovalRecord, ApplicationError> {
        decisions::answer(self, actor, request).await
    }
    async fn check_execution(
        &self,
        lease: &ExecutionLease,
        proposal: ApprovalProposal,
    ) -> Result<CheckedApproval, ApplicationError> {
        approvals::check_execution(self, lease, proposal).await
    }
}
