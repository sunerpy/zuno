use super::*;
use zuno_application::runtime::{JobFinish, RuntimeCheckpoint};
use zuno_engine::advance::{PreparedBegin, prepare_begin, prepare_commit};
use zuno_engine::r#loop::{TurnOutcome, TurnRecovery};

async fn latest(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
) -> Result<Option<SessionEvent>, TurnError> {
    query(
        "SELECT * FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3
         AND type='runtime.driver.advance' ORDER BY sequence DESC LIMIT 1",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .fetch_optional(&mut **tx).await.map_err(sql_error)?.map(decode_event).transpose()
}

fn bound(request: &AdvanceRequest, job: &RuntimeJob) -> Result<(), AdvanceError> {
    if request.run.session_id != job.session_id.as_str()
        || request.run.turn_id != job.turn_id.as_str()
        || request.configuration_digest() != job.configuration.sha256
    {
        return Err(AdvanceError::Conflict);
    }
    let requested = request
        .checkpoint
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| AdvanceError::Conflict)?;
    let stored = job
        .checkpoint
        .as_ref()
        .map(|checkpoint| &checkpoint.reference);
    if requested.as_ref() != stored {
        return Err(AdvanceError::Conflict);
    }
    if job.checkpoint.as_ref().is_some_and(|checkpoint| {
        checkpoint.driver != "default"
            || checkpoint.schema_version != zuno_engine::advance::DRIVER_CHECKPOINT_VERSION
    }) {
        return Err(AdvanceError::Conflict);
    }
    Ok(())
}

pub(super) async fn begin(
    store: &PostgresTurnPersistence,
    scope: &TurnStateScope,
    request: &AdvanceRequest,
) -> Result<BeginAdvance, AdvanceError> {
    let (mut tx, job) = store.transaction(scope).await?;
    bound(request, &job)?;
    let previous = latest(&mut tx, scope).await?;
    let unfinished = !history::unfinished(&mut tx, scope).await?.is_empty();
    let uncertain = history::uncertain(&mut tx, scope).await?;
    let plan = prepare_begin(
        request,
        scope.owner.clone(),
        previous,
        unfinished,
        uncertain,
    )?;
    let result = match plan {
        PreparedBegin::AlreadyCommitted(outcome) => BeginAdvance::AlreadyCommitted(outcome),
        PreparedBegin::Admit(prepared) => {
            let receipt = event(&mut tx, &job, prepared.event()?).await?;
            BeginAdvance::Admitted(Box::new(prepared.committed(&receipt)?))
        }
    };
    store.commit_transaction(tx).await?;
    Ok(result)
}

pub(super) async fn commit(
    store: &PostgresTurnPersistence,
    scope: &TurnStateScope,
    request: &AdvanceRequest,
    admission: &AdvanceAdmission,
    state: AdvanceState,
) -> Result<CheckpointRef, AdvanceError> {
    let (mut tx, job) = store.transaction(scope).await?;
    bound(request, &job)?;
    let previous = latest(&mut tx, scope)
        .await?
        .ok_or(AdvanceError::Conflict)?;
    let finish = match &state {
        AdvanceState::Started => return Err(AdvanceError::Conflict),
        AdvanceState::Checkpointed { .. } => {
            if !history::unfinished(&mut tx, scope).await?.is_empty()
                || history::uncertain(&mut tx, scope).await?
            {
                return Err(AdvanceError::NeedsInspection);
            }
            None
        }
        AdvanceState::Completed {
            outcome: TurnOutcome::Completed { .. },
        } => Some(JobFinish::Completed {
            result: json!(state),
        }),
        AdvanceState::Completed {
            outcome: TurnOutcome::Interrupted { .. },
        } => Some(JobFinish::Cancelled {
            reason: "turn interrupted".to_owned(),
        }),
        AdvanceState::Completed {
            outcome: TurnOutcome::WaitingForHuman { .. },
        } => {
            // A local human-request result is not a distributed wait reference.
            // Keep its fact for inspection; enterprise profiles must install the
            // durable wait consumer before exposing such a tool.
            Some(JobFinish::Uncertain {
                reason: "local human request has no distributed wait checkpoint".to_owned(),
            })
        }
        AdvanceState::Failed { code, recovery, .. } => Some(match recovery {
            TurnRecovery::Fail => JobFinish::Failed { code: code.clone() },
            _ => JobFinish::Uncertain {
                reason: format!("turn requires recovery: {code}"),
            },
        }),
    };
    let receipt = event(
        &mut tx,
        &job,
        prepare_commit(request, admission, &previous, state)?,
    )
    .await?;
    let reference = zuno_engine::advance::checkpoint_reference(&receipt, job.turn_id.as_str())?;
    if let Some(outcome) = finish {
        crate::runtime::finish_in(&mut tx, &store.lease, outcome)
            .await
            .map_err(state_error)?;
    } else {
        crate::runtime::checkpoint_in(
            &mut tx,
            &store.lease,
            RuntimeCheckpoint {
                job_id: job.id,
                session_id: job.session_id,
                turn_id: job.turn_id,
                driver: "default".to_owned(),
                schema_version: zuno_engine::advance::DRIVER_CHECKPOINT_VERSION,
                reference: json!(reference),
            },
        )
        .await
        .map_err(state_error)?;
    }
    tx.commit().await.map_err(sql_error)?;
    Ok(reference)
}
