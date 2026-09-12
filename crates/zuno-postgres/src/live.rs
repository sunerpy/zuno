//! Replaceable progress snapshots. They are never model history or billing facts.

use crate::{
    PostgresBackend, database_error, database_time, owner_transaction, scoped_transaction,
};
use serde_json::json;
use sqlx_core::{query::query, row::Row};
use zuno_application::{ApplicationError, live::LiveUpdate, runtime::ExecutionLease};
use zuno_types::{
    activity::{ACTIVITY_PROTOCOL_VERSION, Counter, LiveEvent, LiveFrame},
    identity::{PrincipalScope, SessionId, TurnId},
};

impl PostgresBackend {
    pub async fn publish_live(
        &self,
        lease: &ExecutionLease,
        update: &LiveUpdate,
    ) -> Result<(), ApplicationError> {
        update.validate()?;
        let sequence = i64::try_from(update.sequence).map_err(ApplicationError::storage)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let job = crate::runtime::verify_lease(&mut tx, lease).await?;
        let access = crate::authorization::access_in(&mut tx, &lease.owner).await?;
        if zuno_permission::enterprise::actor_denial(&access.policy, &access.member, &job.principal)
            .is_some()
        {
            return Err(ApplicationError::Forbidden);
        }
        if let Some(message) = &update.message_id {
            let valid: bool = sqlx_core::query_scalar::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND principal_id=$2
                 AND session_id=$3 AND id=$4 AND execution_job_id=$5 AND role='assistant'
                 AND NOT COALESCE(data->'time' ? 'completed',false))",
            ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.session_id.as_str())
                .bind(message).bind(lease.job_id.as_str()).fetch_one(&mut *tx).await.map_err(database_error)?;
            if !valid {
                return Err(ApplicationError::Conflict);
            }
        }
        let old=query("SELECT attempt_id,epoch,generation,sequence,body_digest FROM zuno_enterprise_preview.live_progress
            WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 FOR UPDATE")
            .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.job_id.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?;
        let digest = zuno_orchestration::sha256_json(&json!(update));
        if let Some(old) = old {
            let same_attempt = old
                .try_get::<String, _>("attempt_id")
                .map_err(database_error)?
                == lease.attempt_id.as_str()
                && old.try_get::<i64, _>("epoch").map_err(database_error)?
                    == i64::try_from(lease.epoch).map_err(ApplicationError::storage)?;
            if same_attempt {
                if old
                    .try_get::<String, _>("generation")
                    .map_err(database_error)?
                    != update.generation
                {
                    return Err(ApplicationError::Conflict);
                }
                let previous: i64 = old.try_get("sequence").map_err(database_error)?;
                if sequence < previous {
                    return Err(ApplicationError::Conflict);
                }
                if sequence == previous {
                    if old
                        .try_get::<String, _>("body_digest")
                        .map_err(database_error)?
                        != digest
                    {
                        return Err(ApplicationError::Conflict);
                    }
                    tx.commit().await.map_err(database_error)?;
                    return Ok(());
                }
            }
        }
        let at = database_time(&mut tx).await?;
        query("INSERT INTO zuno_enterprise_preview.live_progress
            (tenant_id,principal_id,job_id,session_id,attempt_id,epoch,generation,sequence,body,body_digest,time_updated,message_id)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
            ON CONFLICT(tenant_id,principal_id,job_id) DO UPDATE SET attempt_id=excluded.attempt_id,epoch=excluded.epoch,
              generation=excluded.generation,sequence=excluded.sequence,body=excluded.body,body_digest=excluded.body_digest,time_updated=excluded.time_updated,message_id=excluded.message_id")
            .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.job_id.as_str())
            .bind(lease.session_id.as_str()).bind(lease.attempt_id.as_str()).bind(i64::try_from(lease.epoch).map_err(ApplicationError::storage)?)
            .bind(&update.generation).bind(sequence).bind(json!(update)).bind(digest).bind(at).bind(&update.message_id)
            .execute(&mut *tx).await.map_err(database_error)?;
        crate::runtime::verify_lease(&mut tx, lease).await?;
        tx.commit().await.map_err(database_error)
    }

    pub async fn live_progress(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
    ) -> Result<Option<LiveFrame>, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        crate::session::read_session(&mut tx, principal, session.as_str(), false).await?;
        let at = database_time(&mut tx).await?;
        let row=query("SELECT p.body,p.body_digest,p.generation,p.sequence,p.message_id,j.turn_id,a.sequence AS committed FROM zuno_enterprise_preview.live_progress p
            JOIN zuno_enterprise_preview.runtime_job j ON j.tenant_id=p.tenant_id AND j.principal_id=p.principal_id AND j.job_id=p.job_id
            JOIN zuno_enterprise_preview.runtime_session s ON s.tenant_id=p.tenant_id AND s.principal_id=p.principal_id AND s.session_id=p.session_id
            JOIN zuno_enterprise_preview.activity_session a ON a.tenant_id=p.tenant_id AND a.principal_id=p.principal_id AND a.session_id=p.session_id
            JOIN zuno_enterprise_preview.message m ON m.tenant_id=p.tenant_id AND m.principal_id=p.principal_id
              AND m.session_id=p.session_id AND m.id=p.message_id AND m.execution_job_id=p.job_id
            WHERE p.tenant_id=$1 AND p.principal_id=$2 AND p.session_id=$3 AND j.phase='running'
              AND s.lease_job_id=p.job_id AND s.lease_attempt_id=p.attempt_id AND s.lease_epoch=p.epoch
              AND s.lease_expires>$4 AND p.time_updated>$4-30000 AND NOT COALESCE(m.data->'time' ? 'completed',false)")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(session.as_str()).bind(at)
            .fetch_optional(&mut *tx).await.map_err(database_error)?;
        let frame = if let Some(row) = row {
            let raw: serde_json::Value = row.try_get("body").map_err(database_error)?;
            if zuno_orchestration::sha256_json(&raw)
                != row
                    .try_get::<String, _>("body_digest")
                    .map_err(database_error)?
            {
                return Err(ApplicationError::Conflict);
            }
            let update: LiveUpdate =
                serde_json::from_value(raw).map_err(ApplicationError::storage)?;
            update.validate()?;
            if update.generation
                != row
                    .try_get::<String, _>("generation")
                    .map_err(database_error)?
                || update.sequence
                    != u64::try_from(row.try_get::<i64, _>("sequence").map_err(database_error)?)
                        .map_err(ApplicationError::storage)?
                || update.message_id
                    != row
                        .try_get::<Option<String>, _>("message_id")
                        .map_err(database_error)?
            {
                return Err(ApplicationError::Conflict);
            }
            Some(LiveFrame {
                version: ACTIVITY_PROTOCOL_VERSION,
                session_id: session.clone(),
                turn_id: TurnId::new(
                    row.try_get::<String, _>("turn_id")
                        .map_err(database_error)?,
                )
                .map_err(ApplicationError::storage)?,
                generation: update.generation,
                sequence: Counter(update.sequence),
                after_committed: Counter(
                    u64::try_from(row.try_get::<i64, _>("committed").map_err(database_error)?)
                        .map_err(ApplicationError::storage)?,
                ),
                event: LiveEvent::Snapshot {
                    items: update.items,
                },
            })
        } else {
            None
        };
        tx.commit().await.map_err(database_error)?;
        Ok(frame)
    }
}
