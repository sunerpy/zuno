//! Transactional SQLite runtime provider over native agent_job identities.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zuno_application::ApplicationError;
use zuno_application::runtime::{
    ClaimedJob, ExecutionLease, JobFinish, JobPhase, JobSubmission, LeaseDuration,
    RuntimeCheckpoint, RuntimeJob, RuntimeStore,
};
use zuno_error::DbError;
use zuno_types::execution::InputTriggerKind;
use zuno_types::identity::{
    ExecutionAttemptId, InputId, JobId, PrincipalKey, PrincipalScope, SessionId, TurnId,
    WorkerInstanceId,
};

use crate::event_log::{NewSessionEvent, append_in};
use crate::inbox::{InputDelivery, NewSessionInput, SubmissionState};
use crate::job::{JobSettlement, JobStatus, JobSubject, NewAgentJob, ReportDelivery};
use crate::{Pool, session};

#[derive(Clone)]
pub struct SqliteRuntimeStore {
    pool: Arc<Pool>,
    slots: Arc<Semaphore>,
}

impl SqliteRuntimeStore {
    pub fn new(pool: Arc<Pool>, concurrency: NonZeroUsize) -> Result<Self, ApplicationError> {
        if concurrency.get() > 64 {
            return Err(ApplicationError::Invalid(
                "runtime store concurrency exceeds 64".to_owned(),
            ));
        }
        Ok(Self {
            pool,
            slots: Arc::new(Semaphore::new(concurrency.get())),
        })
    }

    async fn transaction<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> StoreResult<T> + Send + 'static,
    ) -> Result<T, ApplicationError> {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ApplicationError::Unavailable)?;
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            pool.try_transaction(work)
                .map_err(|error: StoreFailure| error.0)
        })
        .await
        .map_err(ApplicationError::storage)?
    }
}

struct StoreFailure(ApplicationError);
type StoreResult<T> = Result<T, StoreFailure>;

impl From<DbError> for StoreFailure {
    fn from(error: DbError) -> Self {
        Self(match error {
            DbError::NotFound { .. } => ApplicationError::NotFound,
            DbError::Conflict { .. } => ApplicationError::Conflict,
            DbError::Busy { .. } => ApplicationError::Unavailable,
            other => ApplicationError::storage(other),
        })
    }
}

fn storage(error: impl std::error::Error + Send + Sync + 'static) -> StoreFailure {
    StoreFailure(ApplicationError::storage(error))
}
fn conflict() -> StoreFailure {
    StoreFailure(ApplicationError::Conflict)
}
fn lease_lost() -> StoreFailure {
    StoreFailure(ApplicationError::LeaseLost)
}
fn sql(error: rusqlite::Error) -> StoreFailure {
    crate::open::map_error(error).into()
}
fn integer(value: u64) -> StoreResult<i64> {
    i64::try_from(value).map_err(storage)
}

fn now(connection: &Connection) -> StoreResult<i64> {
    connection
        .query_row(
            "SELECT CAST(unixepoch('subsec')*1000 AS INTEGER)",
            [],
            |row| row.get(0),
        )
        .map_err(sql)
}

fn emit(tx: &Transaction<'_>, session_id: &str, kind: &str, properties: Value) -> StoreResult<()> {
    append_in(
        tx,
        session_id,
        NewSessionEvent::new(
            kind,
            properties
                .as_object()
                .expect("typed runtime event object")
                .clone(),
        )?,
    )?;
    Ok(())
}

#[async_trait]
impl RuntimeStore for SqliteRuntimeStore {
    async fn submit(
        &self,
        principal: &PrincipalScope,
        request: JobSubmission,
    ) -> Result<RuntimeJob, ApplicationError> {
        request.configuration.validate()?;
        if let Some(selection) = &request.selection {
            selection.validate()?;
        }
        if request.text.trim().is_empty()
            || request.text.len() > zuno_application::MAX_INPUT_BYTES
            || request.text.contains('\0')
            || request.expected_input_version > i64::MAX as u64
        {
            return Err(ApplicationError::Invalid(
                "invalid runtime input".to_owned(),
            ));
        }
        let principal = principal.clone();
        self.transaction(move |tx| {
            let session = session::get_owned(tx, request.session_id.as_str(), &principal.owner())?;
            let key = zuno_orchestration::sha256_json(&json!([
                "root-job", principal.owner(), principal.client_id(), request.session_id, request.request_id,
            ]));
            let id = format!("job_{key}");
            let digest = zuno_orchestration::sha256_json(&json!(request));
            let existing: Option<String> = tx.query_row(
                "SELECT request_digest FROM runtime_job WHERE job_id=?1", [&id], |row| row.get(0),
            ).optional().map_err(sql)?;
            if let Some(existing) = existing {
                if existing != digest { return Err(conflict()); }
                return read_job(tx, &id, Some(&principal.owner()));
            }
            let version = input_version(tx, request.session_id.as_str())?;
            if version != request.expected_input_version { return Err(conflict()); }
            let time = now(tx)?;
            let turn_id = format!("turn_{key}");
            let input_id = format!("msg_{key}");
            let model = session.model.as_deref().map(|raw| {
                session::decode_model_reference(raw)
                    .ok_or_else(|| storage(std::io::Error::other("invalid stored model selection")))
            }).transpose()?;
            let (agent, model) = match &request.selection {
                Some(selection) => (Some(selection.agent.clone()), Some(json!({
                    "providerId":selection.model.provider_id,"modelId":selection.model.model_id,
                }))),
                None => (session.agent, model.map(|model|json!({
                    "providerId":model.provider_id,"modelId":model.model_id,
                }))),
            };
            crate::inbox::admit_in(tx, NewSessionInput::new(
                &input_id, request.session_id.as_str(), json!({
                    "kind":"user","prompt":{"text":request.text,"files":[],"agents":[]},
                    "agent":agent,
                    "model":model,
                }), InputDelivery::Queue, time,
            ).with_source_key(format!("runtime:{key}")).with_trigger_kind(InputTriggerKind::User))?;
            crate::job::create_in(tx, NewAgentJob::new(
                &id, request.session_id.as_str(), JobSubject::root_turn(&turn_id), ReportDelivery::Quiet, time,
            ).queued())?;
            let admitted_version = input_version(tx, request.session_id.as_str())?;
            tx.execute(
                "INSERT INTO runtime_job(job_id,session_id,turn_id,input_id,request_digest,principal,
                   configuration,phase,input_version,ready_at,time_created,time_updated)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'ready',?8,?9,?9,?9)",
                params![id,request.session_id.as_str(),turn_id,input_id,digest,
                    serde_json::to_string(&principal).map_err(storage)?,
                    serde_json::to_string(&request.configuration).map_err(storage)?,
                    integer(admitted_version)?,time],
            ).map_err(sql)?;
            tx.execute(
                "INSERT OR IGNORE INTO runtime_owner_schedule(tenant_id,principal_id) VALUES(?1,?2)",
                params![principal.tenant_id().as_str(),principal.principal_id().as_str()],
            ).map_err(sql)?;
            emit(tx, request.session_id.as_str(), "runtime.job.admitted", json!({
                "jobID":id,"turnID":turn_id,"inputID":input_id,
                "inputVersion":admitted_version,"configuration":request.configuration,"principal":principal,
            }))?;
            read_job(tx, &id, Some(&principal.owner()))
        }).await
    }

    async fn input_version(
        &self,
        owner: &PrincipalKey,
        session: &SessionId,
    ) -> Result<u64, ApplicationError> {
        let (owner, session) = (owner.clone(), session.clone());
        self.transaction(move |tx| {
            session::get_owned(tx, session.as_str(), &owner)?;
            input_version(tx, session.as_str())
        })
        .await
    }

    async fn get(&self, owner: &PrincipalKey, job: &JobId) -> Result<RuntimeJob, ApplicationError> {
        let (owner, job) = (owner.clone(), job.clone());
        self.transaction(move |tx| read_job(tx, job.as_str(), Some(&owner)))
            .await
    }

    async fn claim(
        &self,
        worker: &WorkerInstanceId,
        duration: LeaseDuration,
    ) -> Result<Option<ClaimedJob>, ApplicationError> {
        let worker = worker.clone();
        self.transaction(move |tx| {
            let time = now(tx)?;
            expire_inflight(tx, time)?;
            // Keep one logical turn until it settles, even while its worker slot
            // is released. Least-recently-served owners precede local FIFO order.
            let candidate: Option<String> = tx.query_row(
                "SELECT r.job_id FROM runtime_job r
                 JOIN runtime_session s ON s.session_id=r.session_id
                 JOIN session_ownership o ON o.session_id=r.session_id
                 JOIN runtime_owner_schedule q ON q.tenant_id=o.tenant_id AND q.principal_id=o.principal_id
                 JOIN session_input i ON i.id=r.input_id
                 WHERE r.phase='ready' AND r.ready_at<=?1 AND s.lease_job_id IS NULL
                   AND (s.current_job_id IS NULL OR s.current_job_id=r.job_id)
                   AND ((r.checkpoint_version=0 AND i.state='queued')
                     OR (r.checkpoint_version>0 AND i.state='consumed'))
                   AND NOT EXISTS(SELECT 1 FROM session_input earlier
                     WHERE earlier.session_id=r.session_id AND earlier.admitted_seq<i.admitted_seq
                     AND earlier.state IN('queued','steering'))
                 ORDER BY q.last_dispatch_sequence,r.ready_at,i.admitted_seq,r.job_id LIMIT 1",
                [time], |row| row.get(0),
            ).optional().map_err(sql)?;
            let Some(id) = candidate else { return Ok(None); };
            let job = read_job(tx, &id, None)?;
            let attempt_id = format!("attempt_{}", Uuid::new_v4().simple());
            let expires = time.checked_add(i64::from(duration.milliseconds())).ok_or_else(conflict)?;
            tx.execute(
                "UPDATE runtime_session SET lease_epoch=lease_epoch+1,current_job_id=?2,
                 lease_job_id=?2,lease_attempt_id=?3,lease_worker_id=?4,lease_expires=?5
                 WHERE session_id=?1 AND lease_job_id IS NULL",
                params![job.session_id.as_str(),id,attempt_id,worker.as_str(),expires],
            ).map_err(sql)?;
            let epoch: i64 = tx.query_row(
                "SELECT lease_epoch FROM runtime_session WHERE session_id=?1",
                [job.session_id.as_str()], |row| row.get(0),
            ).map_err(sql)?;
            tx.execute(
                "INSERT INTO runtime_attempt(id,job_id,worker_id,lease_epoch,state,started_at)
                 VALUES(?1,?2,?3,?4,'running',?5)",
                params![attempt_id,id,worker.as_str(),epoch,time],
            ).map_err(sql)?;
            tx.execute(
                "UPDATE runtime_job SET phase='running',active_attempt_id=?2,time_updated=?3 WHERE job_id=?1",
                params![id,attempt_id,time],
            ).map_err(sql)?;
            let native = crate::job::get_in(tx, &id)?.ok_or_else(conflict)?;
            if native.status == JobStatus::Queued { crate::job::start_in(tx, &id, time)?; }
            tx.execute(
                "UPDATE runtime_owner_schedule SET last_dispatch_sequence=
                   (SELECT COALESCE(MAX(last_dispatch_sequence),0)+1 FROM runtime_owner_schedule)
                 WHERE tenant_id=?1 AND principal_id=?2",
                params![job.principal.tenant_id().as_str(),job.principal.principal_id().as_str()],
            ).map_err(sql)?;
            emit(tx, job.session_id.as_str(), "runtime.attempt.started", json!({
                "jobID":id,"attemptID":attempt_id,"workerID":worker,"epoch":epoch,"expiresAt":expires,
            }))?;
            Ok(Some(ClaimedJob {
                lease: ExecutionLease {
                    owner:job.principal.owner(),
                    job_id:job.id.clone(),session_id:job.session_id.clone(),
                    attempt_id:ExecutionAttemptId::new(attempt_id).map_err(storage)?,
                    worker,epoch:u64::try_from(epoch).map_err(storage)?,
                    checkpoint_version:job.checkpoint_version,expires_at_ms:expires,
                },
                job:read_job(tx, &id, None)?,
            }))
        }).await
    }

    async fn renew(
        &self,
        lease: &ExecutionLease,
        duration: LeaseDuration,
    ) -> Result<ExecutionLease, ApplicationError> {
        let lease = lease.clone();
        self.transaction(move |tx| {
            let time = now(tx)?;
            verify_lease(tx, &lease, time)?;
            let expires = time.checked_add(i64::from(duration.milliseconds())).ok_or_else(conflict)?;
            // A renewal never shortens a longer already-committed renewal.
            tx.execute(
                "UPDATE runtime_session SET lease_expires=MAX(lease_expires,?2) WHERE session_id=?1",
                params![lease.session_id.as_str(),expires],
            ).map_err(sql)?;
            let expires_at_ms = tx.query_row(
                "SELECT lease_expires FROM runtime_session WHERE session_id=?1",
                [lease.session_id.as_str()], |row| row.get(0),
            ).map_err(sql)?;
            Ok(ExecutionLease { expires_at_ms, ..lease })
        }).await
    }

    async fn checkpoint(
        &self,
        lease: &ExecutionLease,
        checkpoint: RuntimeCheckpoint,
    ) -> Result<RuntimeJob, ApplicationError> {
        checkpoint.validate()?;
        let lease = lease.clone();
        self.transaction(move |tx| {
            let time = now(tx)?;
            let job = verify_lease(tx, &lease, time)?;
            if checkpoint.job_id != job.id
                || checkpoint.session_id != job.session_id
                || checkpoint.turn_id != job.turn_id
            {
                return Err(conflict());
            }
            require_consumed_input(tx, &job)?;
            tx.execute(
                "UPDATE runtime_job SET checkpoint=?2,checkpoint_version=checkpoint_version+1,
                 phase='ready',active_attempt_id=NULL,ready_at=?3,time_updated=?3 WHERE job_id=?1",
                params![
                    job.id.as_str(),
                    serde_json::to_string(&checkpoint).map_err(storage)?,
                    time
                ],
            )
            .map_err(sql)?;
            release(tx, &lease, "released", time, false)?;
            emit(
                tx,
                job.session_id.as_str(),
                "runtime.checkpoint.committed",
                json!({
                    "jobID":job.id,"attemptID":lease.attempt_id,"epoch":lease.epoch,
                    "checkpointVersion":job.checkpoint_version+1,"checkpoint":checkpoint,
                }),
            )?;
            read_job(tx, job.id.as_str(), None)
        })
        .await
    }

    async fn finish(
        &self,
        lease: &ExecutionLease,
        outcome: JobFinish,
    ) -> Result<RuntimeJob, ApplicationError> {
        outcome.validate()?;
        let lease = lease.clone();
        self.transaction(move |tx| {
            let time = now(tx)?;
            let job = verify_lease(tx, &lease, time)?;
            let (phase, settlement) = match outcome {
                JobFinish::Completed { result } => {
                    require_consumed_input(tx, &job)?;
                    ("completed",JobSettlement::completed(result,time,None))
                }
                JobFinish::Failed { code } => ("failed",JobSettlement::failed(code,time,None)),
                JobFinish::Cancelled { reason } => ("cancelled",JobSettlement::cancelled(reason,time,None)),
                JobFinish::Uncertain { reason } => ("uncertain",JobSettlement::uncertain(reason,time,None)),
            };
            crate::job::settle_in(tx, job.id.as_str(), settlement)?;
            if matches!(phase, "failed" | "cancelled") {
                let (state, event) = if phase=="failed" {
                    (SubmissionState::Failed,"session.input.failed")
                } else {
                    (SubmissionState::Cancelled,"session.input.cancelled")
                };
                crate::inbox::transition_in(
                    tx,job.session_id.as_str(),job.input_id.as_str(),
                    &[SubmissionState::Queued,SubmissionState::Steering,SubmissionState::Promoted],
                    state,Some("runtime job ended before input materialization"),event,
                )?;
            }
            tx.execute(
                "UPDATE runtime_job SET phase=?2,active_attempt_id=NULL,time_updated=?3 WHERE job_id=?1",
                params![job.id.as_str(),phase,time],
            ).map_err(sql)?;
            release(tx, &lease, "completed", time, phase!="uncertain")?;
            emit(tx, job.session_id.as_str(), "runtime.job.finished", json!({
                "jobID":job.id,"attemptID":lease.attempt_id,"epoch":lease.epoch,"phase":phase,
            }))?;
            read_job(tx, job.id.as_str(), None)
        }).await
    }
}

fn input_version(connection: &Connection, session: &str) -> StoreResult<u64> {
    let version: i64 = connection
        .query_row(
            "SELECT input_version FROM runtime_session WHERE session_id=?1",
            [session],
            |row| row.get(0),
        )
        .map_err(sql)?;
    u64::try_from(version).map_err(storage)
}

fn require_consumed_input(tx: &Transaction<'_>, job: &RuntimeJob) -> StoreResult<()> {
    let input = crate::inbox::read_in(tx, job.session_id.as_str(), job.input_id.as_str())?
        .ok_or_else(conflict)?;
    if input.state != SubmissionState::Consumed {
        return Err(conflict());
    }
    Ok(())
}

fn verify_lease(
    tx: &Transaction<'_>,
    lease: &ExecutionLease,
    time: i64,
) -> StoreResult<RuntimeJob> {
    let valid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_session s JOIN runtime_job r ON r.job_id=s.lease_job_id
         WHERE s.session_id=?1 AND s.current_job_id=?2 AND s.lease_job_id=?2
         AND s.lease_attempt_id=?3 AND s.lease_worker_id=?4 AND s.lease_epoch=?5
         AND s.lease_expires>?6 AND r.phase='running' AND r.active_attempt_id=?3
         AND r.checkpoint_version=?7
         AND EXISTS(SELECT 1 FROM session_ownership o WHERE o.session_id=s.session_id
           AND o.tenant_id=?8 AND o.principal_id=?9))",
        params![lease.session_id.as_str(),lease.job_id.as_str(),lease.attempt_id.as_str(),
            lease.worker.as_str(),integer(lease.epoch)?,time,integer(lease.checkpoint_version)?,
            lease.owner.tenant_id.as_str(),lease.owner.principal_id.as_str()],
        |row| row.get(0),
    ).map_err(sql)?;
    if !valid {
        return Err(lease_lost());
    }
    read_job(tx, lease.job_id.as_str(), None)
}

fn release(
    tx: &Transaction<'_>,
    lease: &ExecutionLease,
    state: &str,
    time: i64,
    clear_job: bool,
) -> StoreResult<()> {
    tx.execute(
        "UPDATE runtime_attempt SET state=?2,finished_at=?3 WHERE id=?1",
        params![lease.attempt_id.as_str(), state, time],
    )
    .map_err(sql)?;
    tx.execute(
        "UPDATE runtime_session SET lease_job_id=NULL,lease_attempt_id=NULL,lease_worker_id=NULL,
         lease_expires=NULL,current_job_id=CASE WHEN ?2 THEN NULL ELSE current_job_id END WHERE session_id=?1",
        params![lease.session_id.as_str(),clear_job],
    ).map_err(sql)?;
    Ok(())
}

/// Expiry revokes authority but does not prove that an effect stopped. Keep the
/// logical session blocked on the uncertain job instead of replaying it.
fn expire_inflight(tx: &Transaction<'_>, time: i64) -> StoreResult<()> {
    let expired = {
        let mut statement = tx
            .prepare(
                "SELECT s.session_id,s.lease_job_id,s.lease_attempt_id FROM runtime_session s
             JOIN runtime_job r ON r.job_id=s.lease_job_id JOIN agent_job j ON j.id=r.job_id
             WHERE s.lease_expires<=?1 AND r.phase='running' AND j.status='running'",
            )
            .map_err(sql)?;
        statement
            .query_map([time], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?
    };
    for (session, job, attempt) in expired {
        crate::job::settle_in(
            tx,
            &job,
            JobSettlement::uncertain(
                "execution lease expired; inspect the last checkpoint and external operations",
                time,
                None,
            ),
        )?;
        tx.execute(
            "UPDATE runtime_job SET phase='uncertain',active_attempt_id=NULL,time_updated=?2 WHERE job_id=?1",
            params![job,time],
        ).map_err(sql)?;
        tx.execute(
            "UPDATE runtime_attempt SET state='lost',finished_at=?2 WHERE id=?1",
            params![attempt, time],
        )
        .map_err(sql)?;
        tx.execute(
            "UPDATE runtime_session SET lease_job_id=NULL,lease_attempt_id=NULL,lease_worker_id=NULL,
             lease_expires=NULL WHERE session_id=?1", [&session],
        ).map_err(sql)?;
        emit(
            tx,
            &session,
            "runtime.lease.expired",
            json!({"jobID":job,"attemptID":attempt}),
        )?;
    }
    Ok(())
}

struct StoredJob {
    id: String,
    session: String,
    turn: String,
    input: String,
    principal: String,
    configuration: String,
    phase: String,
    checkpoint: Option<String>,
    checkpoint_version: i64,
    input_version: i64,
    result: Option<String>,
}

fn read_job(
    connection: &Connection,
    id: &str,
    owner: Option<&PrincipalKey>,
) -> StoreResult<RuntimeJob> {
    let row = connection
        .query_row(
            "SELECT r.job_id,r.session_id,r.turn_id,r.input_id,r.principal,r.configuration,r.phase,
         r.checkpoint,r.checkpoint_version,r.input_version,j.result
         FROM runtime_job r JOIN agent_job j ON j.id=r.job_id
         JOIN session_ownership o ON o.session_id=r.session_id
         WHERE r.job_id=?1 AND (?2 IS NULL OR (o.tenant_id=?2 AND o.principal_id=?3))",
            params![
                id,
                owner.map(|owner| owner.tenant_id.as_str()),
                owner.map(|owner| owner.principal_id.as_str())
            ],
            |row| {
                Ok(StoredJob {
                    id: row.get(0)?,
                    session: row.get(1)?,
                    turn: row.get(2)?,
                    input: row.get(3)?,
                    principal: row.get(4)?,
                    configuration: row.get(5)?,
                    phase: row.get(6)?,
                    checkpoint: row.get(7)?,
                    checkpoint_version: row.get(8)?,
                    input_version: row.get(9)?,
                    result: row.get(10)?,
                })
            },
        )
        .optional()
        .map_err(sql)?
        .ok_or(StoreFailure(ApplicationError::NotFound))?;
    let phase = match row.phase.as_str() {
        "ready" => JobPhase::Ready,
        "running" => JobPhase::Running,
        "paused" => JobPhase::Paused,
        "completed" => JobPhase::Completed,
        "failed" => JobPhase::Failed,
        "cancelled" => JobPhase::Cancelled,
        "uncertain" => JobPhase::Uncertain,
        _ => return Err(storage(std::io::Error::other("unknown runtime job phase"))),
    };
    let job = RuntimeJob {
        id: JobId::new(row.id).map_err(storage)?,
        session_id: SessionId::new(row.session).map_err(storage)?,
        turn_id: TurnId::new(row.turn).map_err(storage)?,
        input_id: InputId::new(row.input).map_err(storage)?,
        principal: serde_json::from_str(&row.principal).map_err(storage)?,
        configuration: serde_json::from_str(&row.configuration).map_err(storage)?,
        phase,
        checkpoint: row
            .checkpoint
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(storage)?,
        checkpoint_version: u64::try_from(row.checkpoint_version).map_err(storage)?,
        input_version: u64::try_from(row.input_version).map_err(storage)?,
        result: row
            .result
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(storage)?,
    };
    job.configuration.validate().map_err(StoreFailure)?;
    if let Some(checkpoint) = &job.checkpoint {
        checkpoint.validate().map_err(StoreFailure)?;
        if checkpoint.job_id != job.id
            || checkpoint.session_id != job.session_id
            || checkpoint.turn_id != job.turn_id
        {
            return Err(conflict());
        }
    }
    Ok(job)
}
