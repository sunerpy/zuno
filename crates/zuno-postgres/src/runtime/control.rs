//! Stop intent, lease fencing, child settlement and operation cancellation share
//! one owner transaction. Network cancellation is delivered after commit.

use super::*;
use std::collections::{BTreeSet, VecDeque};
use zuno_application::control::{CancelJob, CancellationReceipt, RuntimeControl};
use zuno_types::identity::OperationId;

#[async_trait]
impl RuntimeControl for PostgresRuntimeStore {
    async fn cancel(
        &self,
        principal: &PrincipalScope,
        id: &JobId,
        request: CancelJob,
    ) -> Result<CancellationReceipt, ApplicationError> {
        request.validate()?;
        self.check_owner(&principal.owner())?;
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        let owner = principal.owner();
        let root = read_job(&mut tx, &owner, id.as_str()).await?;
        lock_session(&mut tx, &root).await?;
        if root.turn_id != request.expected_turn_id {
            return Err(ApplicationError::Conflict);
        }
        let digest = zuno_orchestration::sha256_json(&json!([id, request, principal.client_id()]));
        if let Some(row) = query(
            "SELECT request_digest,receipt FROM zuno_enterprise_preview.runtime_control_request
             WHERE tenant_id=$1 AND principal_id=$2 AND request_id=$3",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(request.request_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?
        {
            if row
                .try_get::<String, _>("request_digest")
                .map_err(database_error)?
                != digest
            {
                return Err(ApplicationError::Conflict);
            }
            let receipt = serde_json::from_value(row.try_get("receipt").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
            tx.commit().await.map_err(database_error)?;
            return Ok(receipt);
        }

        let time = database_time(&mut tx).await?;
        let (stopped, operations) = cancel_tree_in(&mut tx, &root, &request.reason, time).await?;
        let receipt = CancellationReceipt {
            request_id: request.request_id,
            job_id: root.id.clone(),
            turn_id: root.turn_id,
            stopped_jobs: stopped,
            pending_operations: operations,
        };
        let inserted = query(
            "INSERT INTO zuno_enterprise_preview.runtime_control_request
             (tenant_id,principal_id,request_id,job_id,request_digest,receipt,time_created)
             VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(receipt.request_id.as_str())
        .bind(root.id.as_str())
        .bind(digest)
        .bind(json!(receipt))
        .bind(time)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?
        .rows_affected();
        if inserted != 1 {
            return Err(ApplicationError::Conflict);
        }
        emit(
            &mut tx,
            principal,
            root.session_id.as_str(),
            "runtime.cancellation.requested",
            json!(receipt),
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        Ok(receipt)
    }
}

/// A data-owner coordinator may stop only Jobs in its verified subtree. Callers
/// retain their coordination/session locks through this transaction.
pub(super) async fn cancel_tree_in(
    tx: &mut Transaction<'_, Postgres>,
    root: &RuntimeJob,
    reason: &str,
    time: i64,
) -> Result<(Vec<JobId>, Vec<OperationId>), ApplicationError> {
    let owner = root.principal.owner();
    let mut queue = VecDeque::from([root.id.clone()]);
    let mut seen = BTreeSet::new();
    let mut stopped = Vec::new();
    let mut operations = Vec::new();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            return Err(ApplicationError::Invalid(
                "cyclic child job graph".to_owned(),
            ));
        }
        let job = read_job(tx, &owner, id.as_str()).await?;
        // Dispatch also takes its parent's session lock. Once held, the
        // subtree cannot grow behind this traversal. Child completion never
        // takes its parent's session lock.
        lock_session(tx, &job).await?;
        let job = read_job(tx, &owner, id.as_str()).await?;
        let descendants: Vec<String> = query_scalar(
            "SELECT activated_job_id AS id FROM zuno_enterprise_preview.runtime_child
             WHERE tenant_id=$1 AND principal_id=$2 AND parent_job_id=$3
               AND activated_job_id IS NOT NULL
             UNION SELECT job_id AS id FROM zuno_enterprise_preview.runtime_continuation
             WHERE tenant_id=$1 AND principal_id=$2 AND parent_job_id=$3 ORDER BY id",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(database_error)?;
        for child in descendants {
            queue.push_back(JobId::new(child).map_err(ApplicationError::storage)?);
        }
        query(
            "INSERT INTO zuno_enterprise_preview.runtime_stop
             (tenant_id,principal_id,job_id,root_job_id,time_requested)
             VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(id.as_str())
        .bind(root.id.as_str())
        .bind(time)
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
        children::retire_staged(tx, &job, time).await?;
        query(
            "INSERT INTO zuno_enterprise_preview.gateway_cancellation_delivery(tenant_id,principal_id,operation_id)
             SELECT tenant_id,principal_id,operation_id FROM zuno_enterprise_preview.gateway_operation
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND completion IS NULL ON CONFLICT DO NOTHING",
        )
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str())
        .execute(&mut **tx).await.map_err(database_error)?;
        let pending: Vec<String> = query_scalar(
            "SELECT operation_id FROM zuno_enterprise_preview.gateway_operation
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND completion IS NULL ORDER BY operation_id",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(database_error)?;
        for operation in pending {
            operations.push(OperationId::new(operation).map_err(ApplicationError::storage)?);
        }
        if matches!(
            job.phase,
            JobPhase::Completed | JobPhase::Failed | JobPhase::Cancelled
        ) {
            continue;
        }
        // Uncertainty is evidence, not an executable phase to relabel.
        if job.phase != JobPhase::Uncertain {
            settle_cancelled(tx, &job, reason, time).await?;
        }
        query(
            "UPDATE zuno_enterprise_preview.runtime_attempt SET state='lost',finished_at=$4
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='running'",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(id.as_str())
        .bind(time)
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
        query(
            "UPDATE zuno_enterprise_preview.runtime_session SET lease_epoch=lease_epoch+1,
               lease_job_id=NULL,lease_attempt_id=NULL,lease_worker_id=NULL,lease_expires=NULL,
               current_job_id=CASE WHEN $4 THEN current_job_id ELSE NULL END
             WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND current_job_id=$5",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(job.session_id.as_str())
        .bind(job.phase == JobPhase::Uncertain)
        .bind(id.as_str())
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
        stopped.push(id);
    }
    Ok((stopped, operations))
}

pub(super) async fn lock_session(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
) -> Result<(), ApplicationError> {
    query("SELECT id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 FOR UPDATE")
        .bind(job.principal.tenant_id().as_str()).bind(job.principal.principal_id().as_str())
        .bind(job.session_id.as_str()).fetch_one(&mut **tx).await.map_err(database_error)?;
    Ok(())
}

async fn settle_cancelled(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
    reason: &str,
    time: i64,
) -> Result<(), ApplicationError> {
    query(
        "UPDATE zuno_enterprise_preview.runtime_council c SET state='cancelled'
        FROM zuno_enterprise_preview.runtime_workflow w
        WHERE c.tenant_id=$1 AND c.principal_id=$2 AND w.job_id=$3
          AND w.tenant_id=c.tenant_id AND w.principal_id=c.principal_id AND w.run_id=c.run_id
          AND c.state IN('seats','stopping','synthesis')",
    )
    .bind(job.principal.tenant_id().as_str())
    .bind(job.principal.principal_id().as_str())
    .bind(job.id.as_str())
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;
    query("UPDATE zuno_enterprise_preview.runtime_council_seat c SET status='cancelled',retry_after_at=NULL
        FROM zuno_enterprise_preview.runtime_workflow w
        WHERE c.tenant_id=$1 AND c.principal_id=$2 AND w.job_id=$3
          AND w.tenant_id=c.tenant_id AND w.principal_id=c.principal_id AND w.run_id=c.run_id
          AND (c.status IN('pending','running','waiting','retrying') OR (c.status='invalid' AND c.retry_after_at IS NOT NULL))")
        .bind(job.principal.tenant_id().as_str()).bind(job.principal.principal_id().as_str()).bind(job.id.as_str())
        .execute(&mut **tx).await.map_err(database_error)?;
    let owner = job.principal.owner();
    let sequence = emit(tx, &job.principal, job.session_id.as_str(), "agent.job.settled",
        json!({"jobID":job.id,"status":"cancelled","error":reason,"result":null,"reportDelivery":"quiet"})).await?;
    query(
        "UPDATE zuno_enterprise_preview.agent_job SET status='cancelled',error=$4,settled_seq=$5,
        time_completed=$6,time_updated=$6 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(job.id.as_str())
    .bind(reason)
    .bind(sequence)
    .bind(time)
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;
    children::completed(tx, job, "cancelled", None, Some(reason), sequence, time).await?;
    query("UPDATE zuno_enterprise_preview.runtime_job SET phase='cancelled',active_attempt_id=NULL,time_updated=$4
        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(job.id.as_str())
        .bind(time).execute(&mut **tx).await.map_err(database_error)?;
    let changed = query("UPDATE zuno_enterprise_preview.input SET state='cancelled' WHERE tenant_id=$1 AND principal_id=$2 AND id=$3
        AND state IN('queued','steering','promoted')")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(job.input_id.as_str())
        .execute(&mut **tx).await.map_err(database_error)?.rows_affected();
    if changed > 0 {
        emit(
            tx,
            &job.principal,
            job.session_id.as_str(),
            "session.input.cancelled",
            json!({"inputID":job.input_id,"state":"cancelled"}),
        )
        .await?;
    }
    query("UPDATE zuno_enterprise_preview.input_execution_receipt SET state='cancelled',completed_at=COALESCE(completed_at,$4),time_updated=$4
        WHERE tenant_id=$1 AND principal_id=$2 AND input_id=$3 AND state IN('admitted','recorded','applied')")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(job.input_id.as_str()).bind(time)
        .execute(&mut **tx).await.map_err(database_error)?;
    query("UPDATE zuno_enterprise_preview.runtime_workflow SET state='cancelled',revision=revision+1,time_updated=$4
        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state IN('preparing','prepared','active')")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(job.id.as_str()).bind(time)
        .execute(&mut **tx).await.map_err(database_error)?;
    emit(
        tx,
        &job.principal,
        job.session_id.as_str(),
        "runtime.job.finished",
        json!({"jobID":job.id,"phase":"cancelled","reason":reason}),
    )
    .await?;
    Ok(())
}
