use super::*;
use sqlx_postgres::PgRow;
use zuno_application::{learning::LearningExecutionLease, runtime::ConfigurationRef};
use zuno_types::identity::WorkerInstanceId;

pub(super) struct NewExecution<'a> {
    pub id: &'a JobId,
    pub source_job: &'a JobId,
    pub session: &'a SessionId,
    pub configuration: &'a ConfigurationRef,
    pub input: LearningInput,
    pub limits: &'a LearningExecutionLimits,
    pub context: Value,
}

fn phase(input: &LearningInput) -> &'static str {
    match input {
        LearningInput::Extraction(_) => "extraction",
        LearningInput::Maintenance(_) => "maintenance",
        LearningInput::SkillEvaluation(_) => "skill_evaluation",
    }
}

pub(super) fn execution(row: &PgRow) -> Result<LearningExecution, Error> {
    let input: LearningInput =
        serde_json::from_value(row.try_get("input").map_err(sql_error)?).map_err(decode_error)?;
    let digest: String = row.try_get("input_digest").map_err(sql_error)?;
    if zuno_orchestration::sha256_json(&json!(input)) != digest {
        return Err(Error::Conflict);
    }
    let session = SessionId::new(row.try_get::<String, _>("session_id").map_err(sql_error)?)
        .map_err(decode_error)?;
    if input.session() != session.as_str() {
        return Err(Error::Conflict);
    }
    let limits: LearningExecutionLimits =
        serde_json::from_value(row.try_get("limits").map_err(sql_error)?).map_err(decode_error)?;
    limits.validate().map_err(app_error)?;
    Ok(LearningExecution {
        id: JobId::new(row.try_get::<String, _>("job_id").map_err(sql_error)?)
            .map_err(decode_error)?,
        principal: serde_json::from_value(row.try_get("principal").map_err(sql_error)?)
            .map_err(decode_error)?,
        workspace: WorkspaceId::new(
            row.try_get::<String, _>("workspace_id")
                .map_err(sql_error)?,
        )
        .map_err(decode_error)?,
        session,
        configuration: serde_json::from_value(row.try_get("configuration").map_err(sql_error)?)
            .map_err(decode_error)?,
        input,
        input_digest: digest,
        limits,
        deadline_ms: row
            .try_get::<Option<i64>, _>("deadline_at")
            .map_err(sql_error)?
            .unwrap_or_default(),
        tokens_charged: u64::try_from(row.try_get::<i64, _>("charged_tokens").map_err(sql_error)?)
            .map_err(decode_error)?,
        cached_output: None,
    })
}

impl TransactionMemory {
    async fn cached_output(
        &self,
        tx: &mut Tx,
        job: &LearningExecution,
    ) -> Result<Option<LearningOutput>, Error> {
        if matches!(job.input, LearningInput::SkillEvaluation(_)) {
            return Ok(None);
        }
        let row=query("SELECT request_id,outcome,outcome_digest FROM zuno_enterprise_preview.learning_model_request
            WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='completed' ORDER BY created_at DESC,request_id DESC LIMIT 1")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.id.as_str())
            .fetch_optional(&mut **tx).await.map_err(sql_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw: Value = row.try_get("outcome").map_err(sql_error)?;
        if row
            .try_get::<String, _>("outcome_digest")
            .map_err(sql_error)?
            != zuno_orchestration::sha256_json(&raw)
        {
            return Err(Error::InvalidData);
        }
        let record: zuno_learning::LearningModelRecord =
            serde_json::from_value(raw).map_err(decode_error)?;
        let zuno_learning::LearningModelEvent::Outcome {
            request_id,
            outcome:
                zuno_learning::LearningModelOutcome::Completed {
                    output,
                    output_digest,
                    ..
                },
            ..
        } = record.event
        else {
            return Err(Error::InvalidData);
        };
        if request_id != row.try_get::<String, _>("request_id").map_err(sql_error)?
            || record.session_id != job.session.as_str()
            || output_digest != zuno_db::learning_source::digest(&output)
        {
            return Err(Error::InvalidData);
        }
        Ok(LearningOutput::decode(job.input.phase(), &output).ok())
    }
    pub(super) async fn insert_execution(
        &self,
        tx: &mut Tx,
        new: NewExecution<'_>,
    ) -> Result<(), Error> {
        insert_execution_in(tx, &self.principal, &self.workspace, new).await
    }

    pub(super) async fn execution_row(&self, tx: &mut Tx, job: &JobId) -> Result<PgRow, Error> {
        query("SELECT e.*,j.workspace_id,j.session_id,j.status,j.owner_id,j.lease_token,j.lease_expires
            FROM zuno_enterprise_preview.learning_execution e
            JOIN zuno_enterprise_preview.learning_job j ON j.tenant_id=e.tenant_id AND j.principal_id=e.principal_id AND j.id=e.job_id
            WHERE e.tenant_id=$1 AND e.principal_id=$2 AND e.job_id=$3 AND j.workspace_id=$4 FOR UPDATE OF e,j")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
            .bind(job.as_str()).bind(self.workspace.as_str()).fetch_optional(&mut **tx).await.map_err(sql_error)?.ok_or(Error::Denied)
    }

    pub(super) async fn check_execution(
        &self,
        tx: &mut Tx,
        lease: &LearningExecutionLease,
    ) -> Result<LearningExecution, Error> {
        let result = self.check_execution_lease(tx, lease).await?;
        if !self.extraction_sources_current(tx, &result).await?
            || !self.maintenance_current(tx, &result).await?
        {
            return Err(Error::Denied);
        }
        Ok(result)
    }

    /// Releasing a valid lease does not read or apply its frozen inputs. Keep
    /// stop/settlement of reservations possible after source policy changes.
    async fn check_execution_lease(
        &self,
        tx: &mut Tx,
        lease: &LearningExecutionLease,
    ) -> Result<LearningExecution, Error> {
        lease.validate().map_err(app_error)?;
        if lease.owner != self.principal.owner() {
            return Err(Error::Denied);
        }
        let row = self.execution_row(tx, &lease.job_id).await?;
        let now = database_time(tx).await.map_err(app_error)?;
        if row.try_get::<String, _>("status").map_err(sql_error)? != "running"
            || row
                .try_get::<Option<String>, _>("owner_id")
                .map_err(sql_error)?
                .as_deref()
                != Some(lease.worker.as_str())
            || row
                .try_get::<Option<String>, _>("lease_token")
                .map_err(sql_error)?
                .as_deref()
                != Some(&lease.token)
            || row.try_get::<i64, _>("attempt").map_err(sql_error)?
                != i64::try_from(lease.epoch).map_err(decode_error)?
            || row
                .try_get::<Option<i64>, _>("lease_expires")
                .map_err(sql_error)?
                .is_none_or(|expires| expires <= now)
            || row
                .try_get::<Option<i64>, _>("deadline_at")
                .map_err(sql_error)?
                .is_none_or(|expires| expires <= now)
        {
            return Err(Error::Conflict);
        }
        let result = execution(&row)?;
        self.require_learning_authority(tx, Some(result.session.as_str()))
            .await?;
        Ok(result)
    }

    async fn claim_execution(
        &self,
        tx: &mut Tx,
        job: &JobId,
        worker: &WorkerInstanceId,
        lease_ms: u32,
    ) -> Result<Option<ClaimedLearning>, Error> {
        let row = self.execution_row(tx, job).await?;
        let mut value = execution(&row)?;
        value.cached_output = self.cached_output(tx, &value).await?;
        let now = database_time(tx).await.map_err(app_error)?;
        let status: String = row.try_get("status").map_err(sql_error)?;
        if status == "running"
            && row
                .try_get::<Option<i64>, _>("lease_expires")
                .map_err(sql_error)?
                .is_some_and(|n| n <= now)
        {
            let amount: i64=query_scalar("WITH changed AS(UPDATE zuno_enterprise_preview.learning_model_request
                SET state='unknown' WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='prepared' RETURNING reserved_tokens)
                SELECT COALESCE(sum(reserved_tokens),0)::bigint FROM changed")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.as_str())
                .fetch_one(&mut **tx).await.map_err(sql_error)?;
            query("UPDATE zuno_enterprise_preview.learning_execution SET charged_tokens=charged_tokens+$4,reserved_tokens=reserved_tokens-$4
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.as_str()).bind(amount)
                .execute(&mut **tx).await.map_err(sql_error)?;
            query("UPDATE zuno_enterprise_preview.learning_job SET status='queued',owner_id=NULL,lease_token=NULL,lease_expires=NULL,time_updated=$4
                WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.as_str()).bind(now)
                .execute(&mut **tx).await.map_err(sql_error)?;
            value.tokens_charged = value.tokens_charged.saturating_add(amount as u64);
            if value.cached_output.is_none() {
                query(
                    "UPDATE zuno_enterprise_preview.learning_execution SET ready_at=$4
                    WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
                )
                .bind(self.principal.tenant_id().as_str())
                .bind(self.principal.principal_id().as_str())
                .bind(job.as_str())
                .bind(now.saturating_add(1000))
                .execute(&mut **tx)
                .await
                .map_err(sql_error)?;
                crate::learning_client::publish_in(tx, &self.principal.owner(), job)
                    .await
                    .map_err(app_error)?;
                return Ok(None);
            }
        } else if status != "queued" {
            return Ok(None);
        }
        let attempts: i64 = row.try_get("attempt").map_err(sql_error)?;
        if row.try_get::<i64, _>("ready_at").map_err(sql_error)? > now {
            return Ok(None);
        }
        if value.cached_output.is_none()
            && (attempts >= i64::from(value.limits.maximum_attempts)
                || value.tokens_charged >= value.limits.total_tokens)
            || value.deadline_ms > 0 && value.deadline_ms <= now
        {
            query("UPDATE zuno_enterprise_preview.learning_job SET status='failed',owner_id=NULL,lease_token=NULL,lease_expires=NULL,
                result='{\"code\":\"learning_budget\"}',time_updated=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.as_str()).bind(now)
                .execute(&mut **tx).await.map_err(sql_error)?;
            crate::learning_client::publish_in(tx, &self.principal.owner(), job)
                .await
                .map_err(app_error)?;
            return Ok(None);
        }
        self.require_learning_authority(tx, Some(value.session.as_str()))
            .await?;
        if !self.extraction_sources_current(tx, &value).await? {
            return Err(Error::Denied);
        }
        if !self.maintenance_current(tx, &value).await? {
            query(
                "UPDATE zuno_enterprise_preview.learning_job SET status='skipped',
                owner_id=NULL,lease_token=NULL,lease_expires=NULL,
                result='{\"reason\":\"maintenance_superseded\"}',time_updated=$4
                WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
            )
            .bind(self.principal.tenant_id().as_str())
            .bind(self.principal.principal_id().as_str())
            .bind(job.as_str())
            .bind(now)
            .execute(&mut **tx)
            .await
            .map_err(sql_error)?;
            crate::learning_client::publish_in(tx, &self.principal.owner(), job)
                .await
                .map_err(app_error)?;
            return Ok(None);
        }
        if value.deadline_ms == 0 {
            value.deadline_ms = now.saturating_add(value.limits.duration_ms as i64);
        }
        if !crate::quota::can_claim(tx, &self.principal.owner(), true)
            .await
            .map_err(app_error)?
        {
            return Ok(None);
        }
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let epoch = attempts + 1;
        let expires_at_ms = now
            .saturating_add(i64::from(lease_ms))
            .min(value.deadline_ms);
        query("UPDATE zuno_enterprise_preview.learning_execution SET attempt=$4,deadline_at=$5 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.as_str()).bind(epoch).bind(value.deadline_ms)
            .execute(&mut **tx).await.map_err(sql_error)?;
        query("UPDATE zuno_enterprise_preview.learning_job SET status='running',owner_id=$4,lease_token=$5,lease_expires=$6,time_updated=$7
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.as_str())
            .bind(worker.as_str()).bind(&token).bind(expires_at_ms).bind(now).execute(&mut **tx).await.map_err(sql_error)?;
        query("INSERT INTO zuno_enterprise_preview.learning_execution_attempt(tenant_id,principal_id,job_id,epoch,worker_id,lease_token,started_at)
            VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.as_str()).bind(epoch)
            .bind(worker.as_str()).bind(&token).bind(now).execute(&mut **tx).await.map_err(sql_error)?;
        crate::learning_client::publish_in(tx, &self.principal.owner(), job)
            .await
            .map_err(app_error)?;
        Ok(Some(ClaimedLearning {
            lease: LearningExecutionLease {
                owner: self.principal.owner(),
                job_id: job.clone(),
                worker: worker.clone(),
                token,
                epoch: epoch as u64,
                expires_at_ms,
            },
            execution: value,
        }))
    }
}

impl PostgresLearningRuntime {
    async fn retire_denied(&self, owner: &PrincipalKey, job: &JobId) -> Result<(), Error> {
        let mut tx = owner_transaction(&self.memory.backend.pool, owner)
            .await
            .map_err(app_error)?;
        query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(zuno_orchestration::sha256_json(&json!(["memory", owner])))
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        let now = database_time(&mut tx).await.map_err(app_error)?;
        let changed=query("UPDATE zuno_enterprise_preview.learning_job SET status='skipped',owner_id=NULL,lease_token=NULL,
            lease_expires=NULL,result='{\"reason\":\"automation_authority_revoked\"}',time_updated=$4
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND status IN('queued','running')")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(job.as_str()).bind(now)
            .execute(&mut *tx).await.map_err(sql_error)?.rows_affected();
        if changed > 0 {
            query(
                "UPDATE zuno_enterprise_preview.learning_model_request SET state='unknown'
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='prepared'",
            )
            .bind(owner.tenant_id.as_str())
            .bind(owner.principal_id.as_str())
            .bind(job.as_str())
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
            query("UPDATE zuno_enterprise_preview.learning_execution SET charged_tokens=charged_tokens+reserved_tokens,reserved_tokens=0
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(job.as_str())
                .execute(&mut *tx).await.map_err(sql_error)?;
            crate::learning_client::publish_in(&mut tx, owner, job)
                .await
                .map_err(app_error)?;
        }
        tx.commit().await.map_err(sql_error)
    }
    pub(super) async fn job_binding(
        &self,
        owner: &PrincipalKey,
        job: &JobId,
    ) -> Result<(PrincipalScope, WorkspaceId, SessionId), Error> {
        if owner.tenant_id != self.tenant {
            return Err(Error::Denied);
        }
        let mut tx = owner_transaction(&self.memory.backend.pool, owner)
            .await
            .map_err(app_error)?;
        let row=query("SELECT e.principal,j.workspace_id,j.session_id FROM zuno_enterprise_preview.learning_execution e
            JOIN zuno_enterprise_preview.learning_job j ON j.tenant_id=e.tenant_id AND j.principal_id=e.principal_id AND j.id=e.job_id
            WHERE e.tenant_id=$1 AND e.principal_id=$2 AND e.job_id=$3")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(job.as_str())
            .fetch_optional(&mut *tx).await.map_err(sql_error)?.ok_or(Error::Denied)?;
        let actor: PrincipalScope =
            serde_json::from_value(row.try_get("principal").map_err(sql_error)?)
                .map_err(decode_error)?;
        if actor.owner() != *owner {
            return Err(Error::Denied);
        }
        let workspace = WorkspaceId::new(
            row.try_get::<String, _>("workspace_id")
                .map_err(sql_error)?,
        )
        .map_err(decode_error)?;
        let session = SessionId::new(row.try_get::<String, _>("session_id").map_err(sql_error)?)
            .map_err(decode_error)?;
        tx.commit().await.map_err(sql_error)?;
        Ok((actor, workspace, session))
    }

    pub async fn claim(
        &self,
        worker: WorkerInstanceId,
        configurations: Vec<ConfigurationRef>,
        lease_ms: u32,
    ) -> Result<Option<ClaimedLearning>, Error> {
        if configurations.is_empty()
            || configurations.len() > 64
            || !(1000..=300000).contains(&lease_ms)
        {
            return Err(invalid("invalid learning claim bounds"));
        }
        for config in &configurations {
            config.validate().map_err(app_error)?;
        }
        self.schedule(8).await?;
        let owners =
            query("SELECT principal_id,dispatch_clock FROM zuno_enterprise_preview.learning_dispatch_owners($1)")
                .bind(self.tenant.as_str())
                .fetch_all(&self.memory.backend.pool)
                .await
                .map_err(sql_error)?;
        for (index, row) in owners.into_iter().enumerate() {
            let id: String = row.try_get("principal_id").map_err(sql_error)?;
            let owner = PrincipalKey {
                tenant_id: self.tenant.clone(),
                principal_id: zuno_types::identity::PrincipalId::new(id).map_err(decode_error)?,
            };
            let mut tx = owner_transaction(&self.memory.backend.pool, &owner)
                .await
                .map_err(app_error)?;
            let jobs: Vec<String>=query_scalar("SELECT e.job_id FROM zuno_enterprise_preview.learning_execution e
                JOIN zuno_enterprise_preview.learning_job j ON j.tenant_id=e.tenant_id AND j.principal_id=e.principal_id AND j.id=e.job_id
                WHERE e.tenant_id=$1 AND e.principal_id=$2 AND e.configuration IN(SELECT value FROM jsonb_array_elements($3))
                AND (j.status='queued' OR (j.status='running' AND j.lease_expires<=$4))
                ORDER BY e.ready_at,e.job_id LIMIT 16")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(json!(configurations))
                .bind(database_time(&mut tx).await.map_err(app_error)?).fetch_all(&mut *tx).await.map_err(sql_error)?;
            let sequence = row
                .try_get::<i64, _>("dispatch_clock")
                .map_err(sql_error)?
                .checked_add(index as i64 + 1)
                .ok_or(Error::Conflict)?;
            query("UPDATE zuno_enterprise_preview.runtime_owner_schedule SET learning_dispatch_sequence=GREATEST(learning_dispatch_sequence,$3)
                WHERE tenant_id=$1 AND principal_id=$2")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(sequence)
                .execute(&mut *tx).await.map_err(sql_error)?;
            tx.commit().await.map_err(sql_error)?;
            for job in jobs {
                let job = JobId::new(job).map_err(decode_error)?;
                let denied_job = job.clone();
                let worker = worker.clone();
                let operation_job = job.clone();
                match self
                    .with_job(&owner, &operation_job, move |provider| {
                        provider.execute(async |tx| {
                            provider.claim_execution(tx, &job, &worker, lease_ms).await
                        })
                    })
                    .await
                {
                    Ok(Some(claimed)) => return Ok(Some(claimed)),
                    Err(Error::Denied) => self.retire_denied(&owner, &denied_job).await?,
                    Ok(None) | Err(Error::Conflict) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(None)
    }

    pub async fn renew(
        &self,
        lease: LearningExecutionLease,
        lease_ms: u32,
    ) -> Result<LearningExecutionLease, Error> {
        if !(1000..=300000).contains(&lease_ms) {
            return Err(invalid("invalid learning lease duration"));
        }
        let owner = lease.owner.clone();
        let job = lease.job_id.clone();
        self.with_job(&owner, &job, move |provider| {
            provider.execute(async |tx| {
                let job = provider.check_execution(tx, &lease).await?;
                let now = database_time(tx).await.map_err(app_error)?;
                let expires = now.saturating_add(i64::from(lease_ms)).min(job.deadline_ms);
                let expires_at_ms: i64 = query_scalar(
                    "UPDATE zuno_enterprise_preview.learning_job
                    SET lease_expires=GREATEST(lease_expires,$4),time_updated=$5
                    WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 RETURNING lease_expires",
                )
                .bind(lease.owner.tenant_id.as_str())
                .bind(lease.owner.principal_id.as_str())
                .bind(lease.job_id.as_str())
                .bind(expires)
                .bind(now)
                .fetch_one(&mut **tx)
                .await
                .map_err(sql_error)?;
                Ok(LearningExecutionLease {
                    expires_at_ms,
                    ..lease.clone()
                })
            })
        })
        .await
    }

    pub async fn stop(
        &self,
        lease: LearningExecutionLease,
        stop: LearningStop,
    ) -> Result<(), Error> {
        let owner = lease.owner.clone();
        let id = lease.job_id.clone();
        self.with_job(&owner,&id,move |provider| provider.execute(async |tx| {
            let job=provider.check_execution_lease(tx,&lease).await?;
            let now=database_time(tx).await.map_err(app_error)?;
            let amount:i64=query_scalar("WITH changed AS(UPDATE zuno_enterprise_preview.learning_model_request
                SET state='unknown' WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='prepared' RETURNING reserved_tokens)
                SELECT COALESCE(sum(reserved_tokens),0)::bigint FROM changed")
                .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.job_id.as_str())
                .fetch_one(&mut **tx).await.map_err(sql_error)?;
            let (mut state,ready,result)=match &stop {
                LearningStop::Retry{after_ms,detail}=>{
                    let local=1000u64.saturating_mul(1u64<<lease.epoch.min(6));
                    let delay=local.max(after_ms.unwrap_or(0).min(300000)).min(300000);
                    ("queued",now.saturating_add(delay as i64),json!({"code":"retry","detail":zuno_error::ProviderError::sanitize_diagnostic(detail,&[])}))
                }
                LearningStop::Failed{code,detail}=>("failed",now,json!({"code":code,"detail":zuno_error::ProviderError::sanitize_diagnostic(detail,&[])})),
            };
            let cached=provider.cached_output(tx,&job).await?.is_some();
            if !cached && (job.tokens_charged.saturating_add(amount as u64)>=job.limits.total_tokens
                || lease.epoch>=u64::from(job.limits.maximum_attempts)) || ready>=job.deadline_ms {
                state="failed";
            }
            query("UPDATE zuno_enterprise_preview.learning_execution
                SET charged_tokens=charged_tokens+$4,reserved_tokens=reserved_tokens-$4,ready_at=$5
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
                .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.job_id.as_str())
                .bind(amount).bind(ready).execute(&mut **tx).await.map_err(sql_error)?;
            query("UPDATE zuno_enterprise_preview.learning_job SET status=$4,result=$5,owner_id=NULL,lease_token=NULL,lease_expires=NULL,time_updated=$6
                WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
                .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.job_id.as_str())
                .bind(state).bind(result).bind(now).execute(&mut **tx).await.map_err(sql_error)?;
            crate::learning_client::publish_in(tx,&lease.owner,&lease.job_id).await.map_err(app_error)?;
            Ok(())
        })).await
    }
}

pub(super) async fn insert_execution_in(
    tx: &mut Tx,
    principal: &PrincipalScope,
    workspace: &WorkspaceId,
    new: NewExecution<'_>,
) -> Result<(), Error> {
    new.limits.validate().map_err(app_error)?;
    crate::quota::admit(
        tx,
        &principal.owner(),
        zuno_application::quota::QuotaResource::LearningJobs,
    )
    .await
    .map_err(app_error)?;
    let now = database_time(tx).await.map_err(app_error)?;
    let input_digest = zuno_orchestration::sha256_json(&json!(new.input));
    let payload = match &new.input {
        LearningInput::SkillEvaluation(_) => {
            json!({"purpose":"skill_evaluation","inputDigest":input_digest})
        }
        LearningInput::Extraction(_) => {
            json!({"purpose":"memory_extraction","inputDigest":input_digest})
        }
        LearningInput::Maintenance(_) => new
            .context
            .get("batch")
            .cloned()
            .ok_or(Error::InvalidData)?,
    };
    query(
        "INSERT INTO zuno_enterprise_preview.learning_job
            (tenant_id,principal_id,id,workspace_id,session_id,kind,status,payload,time_updated)
            VALUES($1,$2,$3,$4,$5,$6,'queued',$7,$8)",
    )
    .bind(principal.tenant_id().as_str())
    .bind(principal.principal_id().as_str())
    .bind(new.id.as_str())
    .bind(workspace.as_str())
    .bind(new.session.as_str())
    .bind(match new.input {
        LearningInput::Extraction(_) => "extraction",
        LearningInput::Maintenance(_) => "project_aggregation",
        LearningInput::SkillEvaluation(_) => "evaluation",
    })
    .bind(payload)
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(sql_error)?;
    query("INSERT INTO zuno_enterprise_preview.learning_execution
            (tenant_id,principal_id,job_id,source_job_id,phase,principal,configuration,input,input_digest,limits,context,ready_at,created_at)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$12)")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str())
            .bind(new.id.as_str()).bind(new.source_job.as_str()).bind(phase(&new.input)).bind(json!(principal))
            .bind(json!(new.configuration)).bind(json!(new.input)).bind(input_digest).bind(json!(new.limits))
            .bind(new.context).bind(now).execute(&mut **tx).await.map_err(sql_error)?;
    crate::learning_client::publish_in(tx, &principal.owner(), new.id)
        .await
        .map_err(app_error)?;
    Ok(())
}
