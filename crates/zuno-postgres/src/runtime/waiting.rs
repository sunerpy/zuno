//! Wait registration, completion facts and scheduler wakeups share the session
//! transaction. Only trusted completion sources can publish; Workers cannot.

use super::*;
use zuno_db::event_log::SessionEvent;
use zuno_engine::state::{TurnStateError, TurnStateScope};
use zuno_engine::wait::{self, WaitCompletion};
use zuno_types::wait::{WaitRef, WaitTarget};

fn scope(job: &RuntimeJob) -> TurnStateScope {
    TurnStateScope {
        owner: job.principal.owner(),
        session_id: job.session_id.to_string(),
    }
}

fn payload_error(error: zuno_engine::r#loop::TurnError) -> ApplicationError {
    match error {
        zuno_engine::r#loop::TurnError::State(TurnStateError::Conflict) => {
            ApplicationError::Conflict
        }
        error => ApplicationError::storage(error),
    }
}

async fn completion(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
    reference: &WaitRef,
) -> Result<Option<WaitCompletion>, ApplicationError> {
    let row = query(
        "SELECT * FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .bind(wait::completion_event_id(scope, reference)).fetch_optional(&mut **tx).await.map_err(database_error)?;
    row.map(|row| {
        let event = crate::turn::decode_event(row).map_err(payload_error)?;
        wait::decode_completion(event, reference).map_err(payload_error)
    })
    .transpose()
}

pub(crate) async fn ready_completions(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
    references: &[WaitRef],
) -> Result<Option<Vec<WaitCompletion>>, ApplicationError> {
    let scope = scope(job);
    let mut ready = Vec::with_capacity(references.len());
    for reference in references {
        let row = query(
            "SELECT reference,state FROM zuno_enterprise_preview.runtime_wait
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND id=$4",
        )
        .bind(scope.owner.tenant_id.as_str())
        .bind(scope.owner.principal_id.as_str())
        .bind(job.id.as_str())
        .bind(reference.id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(database_error)?
        .ok_or(ApplicationError::Conflict)?;
        if row
            .try_get::<Value, _>("reference")
            .map_err(database_error)?
            != json!(reference)
        {
            return Err(ApplicationError::Conflict);
        }
        match row
            .try_get::<String, _>("state")
            .map_err(database_error)?
            .as_str()
        {
            "pending" => return Ok(None),
            "ready" => {}
            _ => return Err(ApplicationError::Conflict),
        }
        ready.push(
            completion(tx, &scope, reference)
                .await?
                .ok_or(ApplicationError::Conflict)?,
        );
    }
    Ok(Some(ready))
}

/// Register under the current lease. A completion may already exist: reading it
/// here closes the completion-before-suspension lost-wakeup window.
pub(crate) async fn register(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
    references: &[WaitRef],
) -> Result<bool, ApplicationError> {
    if references.is_empty() || references.len() > 64 {
        return Err(ApplicationError::Invalid(
            "a suspended Job requires 1–64 waits".to_owned(),
        ));
    }
    let scope = scope(job);
    let time = database_time(tx).await?;
    let mut ids = std::collections::BTreeSet::new();
    let mut all_ready = true;
    for reference in references {
        reference
            .validate()
            .map_err(|message| ApplicationError::Invalid(message.to_owned()))?;
        if reference.turn_id != job.turn_id || !ids.insert(&reference.id) {
            return Err(ApplicationError::Conflict);
        }
        let ready = completion(tx, &scope, reference).await?;
        let deadline = match reference.target {
            WaitTarget::Timer { deadline_ms } => Some(deadline_ms),
            _ => None,
        };
        let event_id = ready
            .as_ref()
            .map(|_| wait::completion_event_id(&scope, reference));
        let state = if ready.is_some() { "ready" } else { "pending" };
        let inserted = query(
            "INSERT INTO zuno_enterprise_preview.runtime_wait
             (tenant_id,principal_id,id,job_id,session_id,turn_id,invocation_id,reference,state,deadline_ms,completion_event_id,time_created,time_updated)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$12) ON CONFLICT(tenant_id,principal_id,id) DO NOTHING",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(reference.id.as_str())
            .bind(job.id.as_str()).bind(job.session_id.as_str()).bind(job.turn_id.as_str()).bind(reference.invocation_id.as_str())
            .bind(json!(reference)).bind(state).bind(deadline).bind(event_id).bind(time)
            .execute(&mut **tx).await.map_err(database_error)?.rows_affected();
        if inserted != 1 {
            // Registration is part of one checkpoint transaction. A different
            // checkpoint cannot reuse an already registered or consumed identity.
            return Err(ApplicationError::Conflict);
        }
        all_ready &= ready.is_some();
    }
    Ok(all_ready)
}

pub(crate) async fn consume(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
    completions: &[WaitCompletion],
) -> Result<(), ApplicationError> {
    let scope = scope(job);
    let time = database_time(tx).await?;
    for completion in completions {
        let id = wait::consumed_event_id(&scope, &completion.reference);
        let event = wait::consumed_event(completion).map_err(payload_error)?;
        crate::session::emit_identified(
            tx,
            &job.principal,
            job.session_id.as_str(),
            &id,
            &event.event_type,
            Value::Object(event.properties),
        )
        .await?;
        let changed = query(
            "UPDATE zuno_enterprise_preview.runtime_wait SET state='consumed',consumed_event_id=$5,time_updated=$6
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND id=$4 AND state='ready' AND reference=$7",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(job.id.as_str())
            .bind(completion.reference.id.as_str()).bind(id).bind(time).bind(json!(completion.reference))
            .execute(&mut **tx).await.map_err(database_error)?.rows_affected();
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
    }
    Ok(())
}

async fn wake(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
    time: i64,
) -> Result<(), ApplicationError> {
    let changed = query(
        "UPDATE zuno_enterprise_preview.runtime_job r SET phase='ready',ready_at=$4,time_updated=$4
         WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND phase='waiting'
           AND EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_wait w
             WHERE w.tenant_id=r.tenant_id AND w.principal_id=r.principal_id AND w.job_id=r.job_id AND w.state='ready')
           AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_wait w
             WHERE w.tenant_id=r.tenant_id AND w.principal_id=r.principal_id AND w.job_id=r.job_id AND w.state IN('pending','cancelled'))",
    ).bind(job.principal.tenant_id().as_str()).bind(job.principal.principal_id().as_str()).bind(job.id.as_str()).bind(time)
        .execute(&mut **tx).await.map_err(database_error)?.rows_affected();
    if changed == 1 {
        emit(
            tx,
            &job.principal,
            job.session_id.as_str(),
            "runtime.wait.ready",
            json!({"jobID":job.id}),
        )
        .await?;
    }
    Ok(())
}

async fn publish(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
    completion: &WaitCompletion,
) -> Result<SessionEvent, ApplicationError> {
    completion.validate().map_err(ApplicationError::storage)?;
    if completion.reference.turn_id != job.turn_id {
        return Err(ApplicationError::Conflict);
    }
    let scope = scope(job);
    let event_id = wait::completion_event_id(&scope, &completion.reference);
    let event = wait::completion_event(completion).map_err(payload_error)?;
    let sequence = crate::session::emit_identified(
        tx,
        &job.principal,
        job.session_id.as_str(),
        &event_id,
        &event.event_type,
        Value::Object(event.properties.clone()),
    )
    .await?;
    let time = database_time(tx).await?;
    let registered = query(
        "SELECT job_id,reference FROM zuno_enterprise_preview.runtime_wait WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(completion.reference.id.as_str())
        .fetch_optional(&mut **tx).await.map_err(database_error)?;
    if let Some(registered) = registered {
        if registered
            .try_get::<String, _>("job_id")
            .map_err(database_error)?
            != job.id.as_str()
            || registered
                .try_get::<Value, _>("reference")
                .map_err(database_error)?
                != json!(completion.reference)
        {
            return Err(ApplicationError::Conflict);
        }
        query(
            "UPDATE zuno_enterprise_preview.runtime_wait SET state='ready',completion_event_id=$4,time_updated=$5
             WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND state='pending'",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(completion.reference.id.as_str())
            .bind(event_id.clone()).bind(time).execute(&mut **tx).await.map_err(database_error)?;
        wake(tx, job, time).await?;
    }
    Ok(SessionEvent {
        id: event_id,
        session_id: job.session_id.to_string(),
        sequence,
        event_type: event.event_type,
        version: 1,
        properties: event.properties,
    })
}

impl PostgresRuntimeStore {
    /// An internal completion source first verifies the producing operation or
    /// child. A lease is intentionally not required for an authoritative late
    /// receipt; a paused/cancelled parent retains it without being resumed.
    pub async fn publish_completion(
        &self,
        owner: &PrincipalKey,
        job_id: &JobId,
        completion: &WaitCompletion,
    ) -> Result<SessionEvent, ApplicationError> {
        self.check_owner(owner)?;
        let mut tx = owner_transaction(&self.pool, owner).await?;
        let initial = read_job(&mut tx, owner, job_id.as_str()).await?;
        query(
            "SELECT id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 FOR UPDATE",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(initial.session_id.as_str())
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        let job = read_job(&mut tx, owner, job_id.as_str()).await?;
        let event = publish(&mut tx, &job, completion).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(event)
    }
}

#[async_trait]
impl zuno_engine::wait::WaitCompletionStore for PostgresRuntimeStore {
    async fn publish(
        &self,
        scope: &TurnStateScope,
        completion: &WaitCompletion,
    ) -> Result<SessionEvent, zuno_engine::r#loop::TurnError> {
        self.check_owner(&scope.owner)
            .map_err(crate::turn::state_error)?;
        completion.validate()?;
        let mut tx = owner_transaction(&self.pool, &scope.owner)
            .await
            .map_err(crate::turn::state_error)?;
        query(
            "SELECT id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 FOR UPDATE",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .fetch_one(&mut *tx).await.map_err(database_error).map_err(crate::turn::state_error)?;
        let job_id: String = query_scalar(
            "SELECT job_id FROM zuno_enterprise_preview.runtime_job
             WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND turn_id=$4",
        )
        .bind(scope.owner.tenant_id.as_str())
        .bind(scope.owner.principal_id.as_str())
        .bind(&scope.session_id)
        .bind(completion.reference.turn_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)
        .map_err(crate::turn::state_error)?;
        let job = read_job(&mut tx, &scope.owner, &job_id)
            .await
            .map_err(crate::turn::state_error)?;
        let event = publish(&mut tx, &job, completion)
            .await
            .map_err(crate::turn::state_error)?;
        tx.commit()
            .await
            .map_err(database_error)
            .map_err(crate::turn::state_error)?;
        Ok(event)
    }
}

/// The dispatcher scans bounded owner-scoped due waits before selecting work.
/// No waiting parent consumes a Worker slot or an in-process sleeping Future.
pub(crate) async fn wake_timers(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    time: i64,
) -> Result<(), ApplicationError> {
    let rows = query(
        "SELECT w.job_id,w.reference FROM zuno_enterprise_preview.runtime_wait w
         JOIN zuno_enterprise_preview.runtime_job r ON r.tenant_id=w.tenant_id AND r.principal_id=w.principal_id AND r.job_id=w.job_id
         JOIN zuno_enterprise_preview.session s ON s.tenant_id=w.tenant_id AND s.principal_id=w.principal_id AND s.id=w.session_id
         WHERE w.tenant_id=$1 AND w.principal_id=$2 AND w.state='pending' AND w.deadline_ms<=$3 AND r.phase='waiting'
         ORDER BY w.deadline_ms,w.id LIMIT 64 FOR UPDATE OF s SKIP LOCKED",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(time)
        .fetch_all(&mut **tx).await.map_err(database_error)?;
    for row in rows {
        let job_id: String = row.try_get("job_id").map_err(database_error)?;
        let reference: WaitRef =
            serde_json::from_value(row.try_get("reference").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        reference
            .validate()
            .map_err(|message| ApplicationError::Invalid(message.to_owned()))?;
        let job = read_job(tx, owner, &job_id).await?;
        if let Some(completion) = wait::timer_completion(&reference, time) {
            publish(tx, &job, &completion).await?;
        }
    }
    Ok(())
}
