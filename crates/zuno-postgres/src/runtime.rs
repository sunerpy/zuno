//! Tenant-bound scheduling in the data owner, never in an Agent Worker.

mod admission;
pub(crate) use admission::request_key;
pub(crate) mod children;
pub(crate) use children::merge_source_in;
mod control;
pub(crate) use control::lock_session as lock_job_session;
mod store;
mod transactions;
pub(crate) mod waiting;
pub(crate) mod workflow;
pub(crate) use transactions::{checkpoint_in, finish_in, suspend_in};

use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::{PgPool, Postgres};
use uuid::Uuid;
use zuno_application::ApplicationError;
use zuno_application::runtime::{
    ClaimedJob, ConfigurationRef, ExecutionLease, JobFinish, JobPhase, JobSubmission,
    LeaseDuration, RuntimeCheckpoint, RuntimeJob, RuntimeStore,
};
use zuno_types::identity::{
    ExecutionAttemptId, InputId, JobId, PrincipalId, PrincipalKey, PrincipalScope, SessionId,
    TenantId, TurnId, WorkerInstanceId,
};

use crate::session::{emit, read_session};
use crate::{database_error, database_time, owner_transaction, scoped_transaction};

#[derive(Clone)]
pub struct PostgresRuntimeStore {
    pool: PgPool,
    tenant: TenantId,
    configurations: Option<Value>,
}

impl PostgresRuntimeStore {
    pub(crate) fn new(pool: PgPool, tenant: TenantId) -> Self {
        Self {
            pool,
            tenant,
            configurations: None,
        }
    }

    /// Restrict claims before acquiring a Job. Incompatible Workers must not
    /// settle or hold a task belonging to another installed configuration.
    pub fn with_configurations(
        mut self,
        configurations: &[ConfigurationRef],
    ) -> Result<Self, ApplicationError> {
        if configurations.is_empty() || configurations.len() > 64 {
            return Err(ApplicationError::Invalid(
                "invalid Worker configuration set".to_owned(),
            ));
        }
        for (index, configuration) in configurations.iter().enumerate() {
            configuration.validate()?;
            if configurations[..index].contains(configuration) {
                return Err(ApplicationError::Invalid(
                    "duplicate Worker configuration".to_owned(),
                ));
            }
        }
        self.configurations =
            Some(serde_json::to_value(configurations).map_err(ApplicationError::storage)?);
        Ok(self)
    }

    fn check_owner(&self, owner: &PrincipalKey) -> Result<(), ApplicationError> {
        if owner.tenant_id != self.tenant {
            return Err(ApplicationError::NotFound);
        }
        Ok(())
    }
}

fn integer(value: u64) -> Result<i64, ApplicationError> {
    i64::try_from(value).map_err(|_| {
        ApplicationError::Invalid("runtime counter exceeds its storage range".to_owned())
    })
}
fn unsigned(value: i64) -> Result<u64, ApplicationError> {
    u64::try_from(value).map_err(ApplicationError::storage)
}

async fn input_version(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    session: &str,
) -> Result<i64, ApplicationError> {
    query_scalar("SELECT input_version FROM zuno_enterprise_preview.runtime_session WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session)
        .fetch_one(&mut **tx).await.map_err(database_error)
}

pub(crate) async fn read_job(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    id: &str,
) -> Result<RuntimeJob, ApplicationError> {
    let row = query(
        "SELECT r.*,j.result FROM zuno_enterprise_preview.runtime_job r
         JOIN zuno_enterprise_preview.agent_job j ON j.tenant_id=r.tenant_id AND j.principal_id=r.principal_id AND j.id=r.job_id
         WHERE r.tenant_id=$1 AND r.principal_id=$2 AND r.job_id=$3",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id)
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    let principal: PrincipalScope =
        serde_json::from_value(row.try_get("principal").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let configuration: ConfigurationRef =
        serde_json::from_value(row.try_get("configuration").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    configuration.validate()?;
    let checkpoint: Option<Value> = row.try_get("checkpoint").map_err(database_error)?;
    let checkpoint = checkpoint
        .map(serde_json::from_value::<RuntimeCheckpoint>)
        .transpose()
        .map_err(ApplicationError::storage)?;
    let phase: String = row.try_get("phase").map_err(database_error)?;
    let phase = match phase.as_str() {
        "ready" => JobPhase::Ready,
        "running" => JobPhase::Running,
        "waiting" => JobPhase::Waiting,
        "paused" => JobPhase::Paused,
        "completed" => JobPhase::Completed,
        "failed" => JobPhase::Failed,
        "cancelled" => JobPhase::Cancelled,
        "uncertain" => JobPhase::Uncertain,
        _ => {
            return Err(ApplicationError::storage(std::io::Error::other(
                "unknown stored Job phase",
            )));
        }
    };
    let job = RuntimeJob {
        id: JobId::new(row.try_get::<String, _>("job_id").map_err(database_error)?)
            .map_err(ApplicationError::storage)?,
        session_id: SessionId::new(
            row.try_get::<String, _>("session_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        turn_id: TurnId::new(
            row.try_get::<String, _>("turn_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        input_id: InputId::new(
            row.try_get::<String, _>("input_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        principal,
        configuration,
        phase,
        checkpoint,
        checkpoint_version: unsigned(row.try_get("checkpoint_version").map_err(database_error)?)?,
        input_version: unsigned(row.try_get("input_version").map_err(database_error)?)?,
        result: row.try_get("result").map_err(database_error)?,
    };
    if &job.principal.owner() != owner {
        return Err(ApplicationError::storage(std::io::Error::other(
            "stored Job ownership disagrees",
        )));
    }
    if let Some(checkpoint) = &job.checkpoint {
        checkpoint.validate()?;
        if checkpoint.job_id != job.id
            || checkpoint.session_id != job.session_id
            || checkpoint.turn_id != job.turn_id
        {
            return Err(ApplicationError::storage(std::io::Error::other(
                "stored checkpoint identity disagrees",
            )));
        }
    }
    Ok(job)
}

pub(crate) async fn verify_lease(
    tx: &mut Transaction<'_, Postgres>,
    lease: &ExecutionLease,
) -> Result<RuntimeJob, ApplicationError> {
    let exists: Option<String> = query_scalar(
        "SELECT id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 FOR UPDATE",
    ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.session_id.as_str())
        .fetch_optional(&mut **tx).await.map_err(database_error)?;
    if exists.is_none() {
        return Err(ApplicationError::LeaseLost);
    }
    let time = database_time(tx).await?;
    let valid: bool = query_scalar(
        "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_session s
         JOIN zuno_enterprise_preview.runtime_job r ON r.tenant_id=s.tenant_id AND r.principal_id=s.principal_id AND r.job_id=s.lease_job_id
         JOIN zuno_enterprise_preview.runtime_attempt a ON a.tenant_id=s.tenant_id AND a.principal_id=s.principal_id AND a.id=s.lease_attempt_id
         WHERE s.tenant_id=$1 AND s.principal_id=$2 AND s.session_id=$3 AND s.current_job_id=$4 AND s.lease_job_id=$4
           AND s.lease_attempt_id=$5 AND s.lease_worker_id=$6 AND s.lease_epoch=$7 AND s.lease_expires>$8
           AND r.phase='running' AND r.active_attempt_id=$5 AND r.checkpoint_version=$9
           AND a.state='running' AND a.job_id=$4 AND a.worker_id=$6 AND a.lease_epoch=$7)",
    ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.session_id.as_str()).bind(lease.job_id.as_str())
        .bind(lease.attempt_id.as_str()).bind(lease.worker.as_str()).bind(integer(lease.epoch)?).bind(time).bind(integer(lease.checkpoint_version)?)
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    if !valid {
        return Err(ApplicationError::LeaseLost);
    }
    read_job(tx, &lease.owner, lease.job_id.as_str()).await
}

async fn require_consumed_input(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
) -> Result<(), ApplicationError> {
    let consumed: bool = query_scalar(
        "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4 AND state='consumed')",
    ).bind(job.principal.tenant_id().as_str()).bind(job.principal.principal_id().as_str()).bind(job.session_id.as_str()).bind(job.input_id.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    if !consumed {
        return Err(ApplicationError::Conflict);
    }
    Ok(())
}

async fn release(
    tx: &mut Transaction<'_, Postgres>,
    lease: &ExecutionLease,
    state: &str,
    time: i64,
    clear_job: bool,
) -> Result<(), ApplicationError> {
    query("UPDATE zuno_enterprise_preview.runtime_attempt SET state=$4,finished_at=$5 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.attempt_id.as_str()).bind(state).bind(time)
        .execute(&mut **tx).await.map_err(database_error)?;
    query(
        "UPDATE zuno_enterprise_preview.runtime_session SET lease_job_id=NULL,lease_attempt_id=NULL,lease_worker_id=NULL,
           lease_expires=NULL,current_job_id=CASE WHEN $4 THEN NULL ELSE current_job_id END
         WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
    ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.session_id.as_str()).bind(clear_job)
        .execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}

async fn expire_inflight(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    time: i64,
) -> Result<(), ApplicationError> {
    let expired = query(
        "SELECT s.session_id FROM zuno_enterprise_preview.runtime_session s
         JOIN zuno_enterprise_preview.session session ON session.tenant_id=s.tenant_id AND session.principal_id=s.principal_id AND session.id=s.session_id
         WHERE s.tenant_id=$1 AND s.principal_id=$2 AND s.lease_job_id IS NOT NULL AND s.lease_expires<=$3
           AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_job r
             WHERE r.tenant_id=s.tenant_id AND r.principal_id=s.principal_id AND r.job_id=s.lease_job_id
               AND r.deadline_at IS NOT NULL AND r.deadline_at<=$3)
         ORDER BY s.session_id LIMIT 64 FOR UPDATE OF session SKIP LOCKED",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(time)
        .fetch_all(&mut **tx).await.map_err(database_error)?;
    for row in expired {
        let session: String = row.try_get("session_id").map_err(database_error)?;
        // Recheck after obtaining the session lock; a renewal may have committed.
        let current = query(
            "SELECT lease_job_id,lease_attempt_id FROM zuno_enterprise_preview.runtime_session
             WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND lease_job_id IS NOT NULL AND lease_expires<=$4",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&session).bind(time)
            .fetch_optional(&mut **tx).await.map_err(database_error)?;
        let Some(current) = current else {
            continue;
        };
        let id: String = current.try_get("lease_job_id").map_err(database_error)?;
        let attempt: String = current
            .try_get("lease_attempt_id")
            .map_err(database_error)?;
        let job = read_job(tx, owner, &id).await?;
        let reclaimable = crate::turn::reclaimable_checkpoint(tx, &job).await?;
        let seq = emit(
            tx,
            &job.principal,
            &session,
            "runtime.lease.expired",
            json!({"jobID":id,"attemptID":attempt,"reclaimableCheckpoint":reclaimable}),
        )
        .await?;
        if reclaimable {
            query(
                "UPDATE zuno_enterprise_preview.runtime_job SET phase='ready',active_attempt_id=NULL,ready_at=$4,time_updated=$4
                 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
            ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id).bind(time)
                .execute(&mut **tx).await.map_err(database_error)?;
        } else {
            query(
            "UPDATE zuno_enterprise_preview.agent_job SET status='uncertain',error='execution lease expired; inspect checkpoint and external operations',
             settled_seq=$4,time_completed=$5,time_updated=$5 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id).bind(seq).bind(time)
            .execute(&mut **tx).await.map_err(database_error)?;
            query("UPDATE zuno_enterprise_preview.runtime_job SET phase='uncertain',active_attempt_id=NULL,time_updated=$4 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id).bind(time)
            .execute(&mut **tx).await.map_err(database_error)?;
        }
        query("UPDATE zuno_enterprise_preview.runtime_attempt SET state='lost',finished_at=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(attempt).bind(time)
            .execute(&mut **tx).await.map_err(database_error)?;
        query(
            "UPDATE zuno_enterprise_preview.runtime_session SET lease_job_id=NULL,lease_attempt_id=NULL,lease_worker_id=NULL,lease_expires=NULL
             WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&session)
            .execute(&mut **tx).await.map_err(database_error)?;
        crate::activity::execution_changed(tx, owner, &session, &id).await?;
    }
    Ok(())
}
