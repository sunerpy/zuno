//! Runtime mutations reusable inside a driver checkpoint transaction.

use super::*;

pub(crate) async fn checkpoint_in(
    tx: &mut Transaction<'_, Postgres>,
    lease: &ExecutionLease,
    checkpoint: RuntimeCheckpoint,
) -> Result<RuntimeJob, ApplicationError> {
    checkpoint.validate()?;
    let job = verify_lease(tx, lease).await?;
    if checkpoint.job_id != job.id
        || checkpoint.session_id != job.session_id
        || checkpoint.turn_id != job.turn_id
    {
        return Err(ApplicationError::Conflict);
    }
    require_consumed_input(tx, &job).await?;
    let time = database_time(tx).await?;
    query(
        "UPDATE zuno_enterprise_preview.runtime_job SET phase='ready',active_attempt_id=NULL,
           checkpoint=$4,checkpoint_version=checkpoint_version+1,ready_at=$5,time_updated=$5
         WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
    )
    .bind(lease.owner.tenant_id.as_str())
    .bind(lease.owner.principal_id.as_str())
    .bind(job.id.as_str())
    .bind(json!(checkpoint))
    .bind(time)
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;
    release(tx, lease, "released", time, false).await?;
    emit(
        tx,
        &job.principal,
        job.session_id.as_str(),
        "runtime.checkpoint.committed",
        json!({
            "jobID":job.id,"attemptID":lease.attempt_id,"epoch":lease.epoch,
            "checkpointVersion":job.checkpoint_version+1,"checkpoint":checkpoint,
        }),
    )
    .await?;
    let job = read_job(tx, &lease.owner, job.id.as_str()).await?;
    Ok(job)
}

pub(crate) async fn finish_in(
    tx: &mut Transaction<'_, Postgres>,
    lease: &ExecutionLease,
    outcome: JobFinish,
) -> Result<RuntimeJob, ApplicationError> {
    outcome.validate()?;
    let job = verify_lease(tx, lease).await?;
    let time = database_time(tx).await?;
    let (phase, result, error) = match outcome {
        JobFinish::Completed { result } => {
            require_consumed_input(tx, &job).await?;
            ("completed", Some(result), None)
        }
        JobFinish::Failed { code } => ("failed", None, Some(code)),
        JobFinish::Cancelled { reason } => ("cancelled", None, Some(reason)),
        JobFinish::Uncertain { reason } => ("uncertain", None, Some(reason)),
    };
    let settled = emit(
        tx,
        &job.principal,
        job.session_id.as_str(),
        "agent.job.settled",
        json!({
            "jobID":job.id,"status":phase,"result":result,"error":error,"reportDelivery":"quiet",
        }),
    )
    .await?;
    query(
        "UPDATE zuno_enterprise_preview.agent_job SET status=$4,result=$5,error=$6,settled_seq=$7,time_completed=$8,time_updated=$8
         WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(job.id.as_str())
        .bind(phase).bind(result).bind(error).bind(settled).bind(time)
        .execute(&mut **tx).await.map_err(database_error)?;
    if matches!(phase, "failed" | "cancelled") {
        let changed = query(
            "UPDATE zuno_enterprise_preview.input SET state=$4
             WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND state IN('queued','steering','promoted')",
        ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(job.input_id.as_str()).bind(phase)
            .execute(&mut **tx).await.map_err(database_error)?.rows_affected();
        if changed > 0 {
            let kind = if phase == "failed" {
                "session.input.failed"
            } else {
                "session.input.cancelled"
            };
            emit(
                tx,
                &job.principal,
                job.session_id.as_str(),
                kind,
                json!({"inputID":job.input_id,"state":phase}),
            )
            .await?;
        }
    }
    query(
        "UPDATE zuno_enterprise_preview.runtime_job SET phase=$4,active_attempt_id=NULL,time_updated=$5
         WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
    ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(job.id.as_str()).bind(phase).bind(time)
        .execute(&mut **tx).await.map_err(database_error)?;
    release(tx, lease, "completed", time, phase != "uncertain").await?;
    emit(
        tx,
        &job.principal,
        job.session_id.as_str(),
        "runtime.job.finished",
        json!({
            "jobID":job.id,"attemptID":lease.attempt_id,"epoch":lease.epoch,"phase":phase,
        }),
    )
    .await?;
    let job = read_job(tx, &lease.owner, job.id.as_str()).await?;
    Ok(job)
}
