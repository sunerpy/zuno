use crate::{PostgresBackend, database_error, owner_transaction};
use async_trait::async_trait;
use serde_json::json;
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::Postgres;
use zuno_application::{ApplicationError, quota::*};
use zuno_permission::enterprise::{OrganizationRole, actor_denial};
use zuno_types::{
    activity::Counter,
    identity::{PrincipalKey, PrincipalKind, PrincipalScope},
};
type Tx<'a> = Transaction<'a, Postgres>;
#[derive(Clone)]
pub struct PostgresQuotaStore {
    backend: PostgresBackend,
}
impl PostgresBackend {
    pub fn quotas(&self) -> PostgresQuotaStore {
        PostgresQuotaStore {
            backend: self.clone(),
        }
    }
}
async fn policy(tx: &mut Tx<'_>, owner: &PrincipalKey) -> Result<QuotaPolicy, ApplicationError> {
    let row=query("SELECT revision,limits FROM zuno_enterprise_preview.organization_quota WHERE tenant_id=$1 FOR SHARE")
        .bind(owner.tenant_id.as_str()).fetch_one(&mut **tx).await.map_err(database_error)?;
    let limits: QuotaLimits =
        serde_json::from_value(row.try_get("limits").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    limits.validate()?;
    let revision = u64::try_from(row.try_get::<i64, _>("revision").map_err(database_error)?)
        .map_err(ApplicationError::storage)?;
    Ok(QuotaPolicy {
        revision: Counter(revision),
        limits,
    })
}
async fn usage(
    tx: &mut Tx<'_>,
    owner: &PrincipalKey,
    resource: QuotaResource,
) -> Result<u64, ApplicationError> {
    let sql=match resource {
        QuotaResource::RootSessions=>"SELECT count(*) FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND parent_id IS NULL",
        QuotaResource::RootJobs=>"SELECT count(*) FROM zuno_enterprise_preview.runtime_job r
            JOIN zuno_enterprise_preview.input i ON i.tenant_id=r.tenant_id AND i.principal_id=r.principal_id AND i.id=r.input_id
            WHERE r.tenant_id=$1 AND r.principal_id=$2 AND r.phase NOT IN('completed','failed','cancelled')
              AND i.prompt->>'kind'='user'",
        QuotaResource::ChildJobs=>"SELECT count(*) FROM zuno_enterprise_preview.runtime_child c
            LEFT JOIN zuno_enterprise_preview.runtime_job r ON r.tenant_id=c.tenant_id AND r.principal_id=c.principal_id AND r.job_id=c.activated_job_id
            WHERE c.tenant_id=$1 AND c.principal_id=$2 AND c.state<>'cancelled'
              AND (r.phase IS NULL OR r.phase NOT IN('completed','failed','cancelled'))",
        QuotaResource::Executions=>"SELECT count(*) FROM zuno_enterprise_preview.runtime_session
            WHERE tenant_id=$1 AND principal_id=$2 AND lease_job_id IS NOT NULL AND lease_expires>floor(extract(epoch FROM clock_timestamp())*1000)::bigint",
        QuotaResource::LearningJobs=>"SELECT count(*) FROM zuno_enterprise_preview.learning_job
            WHERE tenant_id=$1 AND principal_id=$2 AND status IN('queued','running','uncertain')",
        QuotaResource::LearningExecutions=>"SELECT count(*) FROM zuno_enterprise_preview.learning_job
            WHERE tenant_id=$1 AND principal_id=$2 AND status='running' AND lease_expires>floor(extract(epoch FROM clock_timestamp())*1000)::bigint",
    };
    let count: i64 = query_scalar(sql)
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(database_error)?;
    u64::try_from(count).map_err(ApplicationError::storage)
}
pub(crate) async fn admit(
    tx: &mut Tx<'_>,
    owner: &PrincipalKey,
    resource: QuotaResource,
) -> Result<(), ApplicationError> {
    // All admissions/claims for an owner take this bounded transaction lock.
    // Finishing a Job only decreases usage and does not wait on this lock.
    let limits = policy(tx, owner).await?;
    query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(zuno_orchestration::sha256_json(&json!(["quota", owner])))
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
    if usage(tx, owner, resource).await?
        > u64::from(limits.limits.limit(resource)).saturating_sub(1)
    {
        return Err(ApplicationError::QuotaExceeded(resource));
    }
    Ok(())
}
pub(crate) async fn can_claim(
    tx: &mut Tx<'_>,
    owner: &PrincipalKey,
    learning: bool,
) -> Result<bool, ApplicationError> {
    let resource = if learning {
        QuotaResource::LearningExecutions
    } else {
        QuotaResource::Executions
    };
    // Claim only skips capacity; an accepted Job never becomes failed because a
    // quota was lowered while it was paused or waiting.
    match admit(tx, owner, resource).await {
        Ok(()) => Ok(true),
        Err(ApplicationError::QuotaExceeded(_)) => Ok(false),
        Err(error) => Err(error),
    }
}
#[async_trait]
impl QuotaStore for PostgresQuotaStore {
    async fn snapshot(
        &self,
        principal: &PrincipalScope,
    ) -> Result<QuotaSnapshot, ApplicationError> {
        let mut tx = crate::scoped_transaction(&self.backend.pool, principal).await?;
        let policy = policy(&mut tx, &principal.owner()).await?;
        let mut values = Vec::new();
        for resource in [
            QuotaResource::RootSessions,
            QuotaResource::RootJobs,
            QuotaResource::ChildJobs,
            QuotaResource::Executions,
            QuotaResource::LearningJobs,
            QuotaResource::LearningExecutions,
        ] {
            values.push(QuotaUsage {
                resource,
                used: Counter(usage(&mut tx, &principal.owner(), resource).await?),
                limit: Counter(u64::from(policy.limits.limit(resource))),
            });
        }
        tx.commit().await.map_err(database_error)?;
        Ok(QuotaSnapshot {
            policy,
            usage: values,
        })
    }
    async fn replace(
        &self,
        principal: &PrincipalScope,
        request: ReplaceQuotaPolicy,
    ) -> Result<QuotaPolicy, ApplicationError> {
        request.limits.validate()?;
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        let access = crate::authorization::access_in(&mut tx, &principal.owner()).await?;
        if actor_denial(&access.policy, &access.member, principal).is_some()
            || access.member.role != OrganizationRole::Administrator
            || principal.kind() != PrincipalKind::User
            || principal
                .client_id()
                .is_none_or(|id| !access.policy.approval_apps.contains(id))
        {
            return Err(ApplicationError::Forbidden);
        }
        let previous:i64=query_scalar("SELECT revision FROM zuno_enterprise_preview.organization_quota WHERE tenant_id=$1 FOR UPDATE")
            .bind(principal.tenant_id().as_str()).fetch_one(&mut *tx).await.map_err(database_error)?;
        let hash = zuno_orchestration::sha256_json(&json!(request));
        let row=query("SELECT request_digest,result FROM zuno_enterprise_preview.organization_quota_request WHERE tenant_id=$1 AND principal_id=$2 AND request_id=$3")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(request.request_id.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?;
        if let Some(row) = row {
            if row
                .try_get::<String, _>("request_digest")
                .map_err(database_error)?
                != hash
            {
                return Err(ApplicationError::Conflict);
            }
            return serde_json::from_value(row.try_get("result").map_err(database_error)?)
                .map_err(ApplicationError::storage);
        }
        if u64::try_from(previous).map_err(ApplicationError::storage)?
            != request.expected_revision.0
        {
            return Err(ApplicationError::Conflict);
        }
        let next = previous.checked_add(1).ok_or(ApplicationError::Conflict)?;
        let value = QuotaPolicy {
            revision: Counter(next as u64),
            limits: request.limits,
        };
        query("UPDATE zuno_enterprise_preview.organization_quota SET revision=$2,limits=$3 WHERE tenant_id=$1")
            .bind(principal.tenant_id().as_str()).bind(next).bind(json!(value.limits)).execute(&mut *tx).await.map_err(database_error)?;
        query("INSERT INTO zuno_enterprise_preview.organization_quota_request(tenant_id,principal_id,request_id,request_digest,result,actor,time_created)
            VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(request.request_id.as_str()).bind(hash)
            .bind(json!(value)).bind(json!(principal)).bind(crate::database_time(&mut tx).await?)
            .execute(&mut *tx).await.map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
}
