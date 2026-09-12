//! Immutable source facts for gateway-to-gateway snapshot delivery.
use crate::{PostgresBackend, database_error, database_time, owner_transaction};
use serde_json::{Value, json};
use sqlx_core::{query::query, row::Row, transaction::Transaction};
use sqlx_postgres::Postgres;
use zuno_application::{
    ApplicationError,
    environment::EnvironmentSnapshot,
    workspace_transfer::{SnapshotTransferAssignment, SnapshotTransferCompletion},
};
use zuno_types::identity::{EnvironmentSnapshotId, GatewayId, JobId, PrincipalKey};

async fn read_in(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    id: &EnvironmentSnapshotId,
) -> Result<Option<(SnapshotTransferAssignment, Option<EnvironmentSnapshot>)>, ApplicationError> {
    let row = query(
        "SELECT * FROM zuno_enterprise_preview.workspace_snapshot_transfer
        WHERE tenant_id=$1 AND principal_id=$2 AND snapshot_id=$3 FOR UPDATE",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let assigned: SnapshotTransferAssignment =
        serde_json::from_value(row.try_get("admission").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    if assigned.request.lease.owner != *owner
        || assigned.request.snapshot_id()? != *id
        || assigned.digest()
            != row
                .try_get::<String, _>("admission_digest")
                .map_err(database_error)?
        || assigned.source.gateway_id.as_str()
            != row
                .try_get::<String, _>("source_gateway_id")
                .map_err(database_error)?
        || assigned.target_gateway_id.as_str()
            != row
                .try_get::<String, _>("target_gateway_id")
                .map_err(database_error)?
        || assigned.request.lease.job_id.as_str()
            != row.try_get::<String, _>("job_id").map_err(database_error)?
    {
        return Err(ApplicationError::Conflict);
    }
    let raw: Option<Value> = row.try_get("snapshot").map_err(database_error)?;
    let snapshot = raw
        .map(|raw| {
            if row
                .try_get::<Option<String>, _>("snapshot_digest")
                .map_err(database_error)?
                .as_deref()
                != Some(zuno_orchestration::sha256_json(&raw).as_str())
            {
                return Err(ApplicationError::Conflict);
            }
            let snapshot: EnvironmentSnapshot =
                serde_json::from_value(raw).map_err(ApplicationError::storage)?;
            assigned.validate_snapshot(&snapshot)?;
            Ok(snapshot)
        })
        .transpose()?;
    Ok(Some((assigned, snapshot)))
}

pub(crate) async fn require_snapshot_in(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    job: &JobId,
    source: &GatewayId,
    target: &GatewayId,
    snapshot: &EnvironmentSnapshot,
) -> Result<(), ApplicationError> {
    let (assigned, committed) = read_in(tx, owner, &snapshot.id)
        .await?
        .ok_or(ApplicationError::Forbidden)?;
    if assigned.request.lease.job_id != *job
        || assigned.source.gateway_id != *source
        || assigned.target_gateway_id != *target
        || committed.as_ref() != Some(snapshot)
    {
        return Err(ApplicationError::Forbidden);
    }
    Ok(())
}

impl PostgresBackend {
    /// Called by the trusted configuration resolver, not with caller-supplied
    /// routing. A repeated request may carry a fresh lease for the same Job.
    pub async fn admit_snapshot_transfer(
        &self,
        assigned: &SnapshotTransferAssignment,
    ) -> Result<Option<EnvironmentSnapshot>, ApplicationError> {
        let lease = &assigned.request.lease;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        crate::runtime::validate_snapshot_transfer_in(&mut tx, assigned).await?;
        let id = assigned.request.snapshot_id()?;
        // A stable transfer can be redeemed concurrently by source and target.
        query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(zuno_orchestration::sha256_json(&json!([
                "workspace-transfer",
                lease.owner,
                id
            ])))
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        let prior = read_in(&mut tx, &lease.owner, &id).await?;
        let snapshot = if let Some((prior, snapshot)) = prior {
            if prior.digest() != assigned.digest() {
                return Err(ApplicationError::Conflict);
            }
            snapshot
        } else {
            query("INSERT INTO zuno_enterprise_preview.workspace_snapshot_transfer
                (tenant_id,principal_id,snapshot_id,source_gateway_id,target_gateway_id,job_id,admission,admission_digest,time_admitted)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(id.as_str())
                .bind(assigned.source.gateway_id.as_str()).bind(assigned.target_gateway_id.as_str())
                .bind(lease.job_id.as_str()).bind(json!(assigned)).bind(assigned.digest())
                .bind(database_time(&mut tx).await?).execute(&mut *tx).await.map_err(database_error)?;
            None
        };
        crate::runtime::verify_lease(&mut tx, lease).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(snapshot)
    }

    /// Preserve truthful late source facts without restoring an expired lease
    /// or authorizing publication in a target environment.
    pub async fn complete_snapshot_transfer(
        &self,
        gateway: &GatewayId,
        completion: &SnapshotTransferCompletion,
    ) -> Result<(), ApplicationError> {
        let assigned = &completion.assignment;
        assigned.validate_snapshot(&completion.snapshot)?;
        let owner = &assigned.request.lease.owner;
        let mut tx = owner_transaction(&self.pool, owner).await?;
        let (expected, prior) = read_in(&mut tx, owner, &completion.snapshot.id)
            .await?
            .ok_or(ApplicationError::Forbidden)?;
        if expected.source.gateway_id != *gateway || expected.digest() != assigned.digest() {
            return Err(ApplicationError::Forbidden);
        }
        if let Some(prior) = prior {
            if prior != completion.snapshot {
                return Err(ApplicationError::Conflict);
            }
        } else {
            query(
                "UPDATE zuno_enterprise_preview.workspace_snapshot_transfer
                SET snapshot=$4,snapshot_digest=$5,time_completed=$6
                WHERE tenant_id=$1 AND principal_id=$2 AND snapshot_id=$3 AND snapshot IS NULL",
            )
            .bind(owner.tenant_id.as_str())
            .bind(owner.principal_id.as_str())
            .bind(completion.snapshot.id.as_str())
            .bind(json!(completion.snapshot))
            .bind(zuno_orchestration::sha256_json(&json!(completion.snapshot)))
            .bind(database_time(&mut tx).await?)
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        }
        tx.commit().await.map_err(database_error)
    }
}
