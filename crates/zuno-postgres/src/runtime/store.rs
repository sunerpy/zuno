use super::*;

#[async_trait]
impl RuntimeStore for PostgresRuntimeStore {
    async fn submit(
        &self,
        principal: &PrincipalScope,
        request: JobSubmission,
    ) -> Result<RuntimeJob, ApplicationError> {
        self.check_owner(&principal.owner())?;
        request.configuration.validate()?;
        if request.text.trim().is_empty()
            || request.text.len() > zuno_application::MAX_INPUT_BYTES
            || request.text.contains('\0')
        {
            return Err(ApplicationError::Invalid(
                "invalid runtime input".to_owned(),
            ));
        }
        let expected = integer(request.expected_input_version)?;
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        // Every session/input writer acquires this row before runtime state.
        let session = read_session(&mut tx, principal, request.session_id.as_str(), true).await?;
        let owner = principal.owner();
        let key = zuno_orchestration::sha256_json(&json!([
            "root-job",
            owner,
            principal.client_id(),
            request.session_id,
            request.request_id,
        ]));
        let id = format!("job_{key}");
        let digest = zuno_orchestration::sha256_json(&json!(request));
        let existing: Option<String> = query_scalar(
            "SELECT request_digest FROM zuno_enterprise_preview.runtime_job WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id)
            .fetch_optional(&mut *tx).await.map_err(database_error)?;
        if let Some(existing) = existing {
            if existing != digest {
                return Err(ApplicationError::Conflict);
            }
            let job = read_job(&mut tx, &owner, &id).await?;
            tx.commit().await.map_err(database_error)?;
            return Ok(job);
        }
        if input_version(&mut tx, &owner, request.session_id.as_str()).await? != expected {
            return Err(ApplicationError::Conflict);
        }
        let time = database_time(&mut tx).await?;
        let turn = format!("turn_{key}");
        let input = format!("msg_{key}");
        let agent: Option<String> = session.try_get("agent").map_err(database_error)?;
        let model: Option<Value> = session.try_get("model").map_err(database_error)?;
        let prompt = json!({"kind":"user","prompt":{"text":request.text,"files":[],"agents":[]},"agent":agent,"model":model});
        let admitted = emit(&mut tx,principal,request.session_id.as_str(),"session.input.admitted",json!({
            "inputID":input,"prompt":prompt,"delivery":"queue","state":"queued","triggerKind":"user","timeCreated":time,
        })).await?;
        query(
            "INSERT INTO zuno_enterprise_preview.input(
               tenant_id,principal_id,session_id,id,request_key,prompt,state,admitted_sequence,time_created)
             VALUES($1,$2,$3,$4,$5,$6,'queued',$7,$8)",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(request.session_id.as_str())
            .bind(&input).bind(format!("runtime:{key}")).bind(prompt).bind(admitted).bind(time)
            .execute(&mut *tx).await.map_err(database_error)?;
        let created = emit(&mut tx,principal,request.session_id.as_str(),"agent.job.created",json!({
            "jobID":id,"subject":{"kind":"rootTurn","turnID":turn},"status":"queued","reportDelivery":"quiet",
        })).await?;
        query(
            "INSERT INTO zuno_enterprise_preview.agent_job(
               tenant_id,principal_id,id,parent_session_id,subject_kind,subject_payload,status,report_delivery,created_seq,time_created,time_updated)
             VALUES($1,$2,$3,$4,'root-turn',$5,'queued','quiet',$6,$7,$7)",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id)
            .bind(request.session_id.as_str()).bind(json!({"kind":"rootTurn","turnID":turn})).bind(created).bind(time)
            .execute(&mut *tx).await.map_err(database_error)?;
        let version = input_version(&mut tx, &owner, request.session_id.as_str()).await?;
        query(
            "INSERT INTO zuno_enterprise_preview.runtime_job(
               tenant_id,principal_id,job_id,session_id,turn_id,input_id,request_digest,principal,configuration,
               phase,input_version,ready_at,time_created,time_updated)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,'ready',$10,$11,$11,$11)",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id).bind(request.session_id.as_str())
            .bind(&turn).bind(&input).bind(digest).bind(json!(principal)).bind(json!(request.configuration)).bind(version).bind(time)
            .execute(&mut *tx).await.map_err(database_error)?;
        query("INSERT INTO zuno_enterprise_preview.runtime_owner_schedule(tenant_id,principal_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str())
            .execute(&mut *tx).await.map_err(database_error)?;
        emit(&mut tx,principal,request.session_id.as_str(),"runtime.job.admitted",json!({
            "jobID":id,"turnID":turn,"inputID":input,"inputVersion":version,"configuration":request.configuration,"principal":principal,
        })).await?;
        let job = read_job(&mut tx, &owner, &id).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(job)
    }

    async fn input_version(
        &self,
        owner: &PrincipalKey,
        session: &SessionId,
    ) -> Result<u64, ApplicationError> {
        self.check_owner(owner)?;
        let mut tx = owner_transaction(&self.pool, owner).await?;
        let version = unsigned(input_version(&mut tx, owner, session.as_str()).await?)?;
        tx.commit().await.map_err(database_error)?;
        Ok(version)
    }

    async fn get(&self, owner: &PrincipalKey, job: &JobId) -> Result<RuntimeJob, ApplicationError> {
        self.check_owner(owner)?;
        let mut tx = owner_transaction(&self.pool, owner).await?;
        let job = read_job(&mut tx, owner, job.as_str()).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(job)
    }

    async fn claim(
        &self,
        worker: &WorkerInstanceId,
        duration: LeaseDuration,
    ) -> Result<Option<ClaimedJob>, ApplicationError> {
        // The privileged helper exposes owner/order metadata only. Private Job,
        // input and checkpoint reads still run under an exact owner RLS context.
        let owners = query(
            "SELECT principal_id,dispatch_clock FROM zuno_enterprise_preview.dispatch_owners($1)",
        )
        .bind(self.tenant.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        for (index, row) in owners.iter().enumerate() {
            let owner = PrincipalKey {
                tenant_id: self.tenant.clone(),
                principal_id: PrincipalId::new(
                    row.try_get::<String, _>("principal_id")
                        .map_err(database_error)?,
                )
                .map_err(ApplicationError::storage)?,
            };
            let mut tx = owner_transaction(&self.pool, &owner).await?;
            let time = database_time(&mut tx).await?;
            expire_inflight(&mut tx, &owner, time).await?;
            waiting::wake_timers(&mut tx, &owner, time).await?;
            let time = database_time(&mut tx).await?;
            let candidate = query(
                "SELECT r.job_id FROM zuno_enterprise_preview.runtime_job r
                 JOIN zuno_enterprise_preview.runtime_session s
                   ON s.tenant_id=r.tenant_id AND s.principal_id=r.principal_id AND s.session_id=r.session_id
                 JOIN zuno_enterprise_preview.session session
                   ON session.tenant_id=r.tenant_id AND session.principal_id=r.principal_id AND session.id=r.session_id
                 JOIN zuno_enterprise_preview.input i
                   ON i.tenant_id=r.tenant_id AND i.principal_id=r.principal_id AND i.id=r.input_id
                 WHERE r.tenant_id=$1 AND r.principal_id=$2 AND r.phase='ready' AND r.ready_at<=$3
                   AND ($4::jsonb IS NULL OR r.configuration IN (SELECT value FROM jsonb_array_elements($4::jsonb)))
                   AND s.lease_job_id IS NULL AND (s.current_job_id IS NULL OR s.current_job_id=r.job_id)
                   AND ((r.checkpoint_version=0 AND i.state='queued') OR (r.checkpoint_version>0 AND i.state='consumed'))
                   AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.input earlier
                     WHERE earlier.tenant_id=r.tenant_id AND earlier.principal_id=r.principal_id AND earlier.session_id=r.session_id
                     AND earlier.admitted_sequence<i.admitted_sequence AND earlier.state IN('queued','steering'))
                 ORDER BY r.ready_at,i.admitted_sequence,r.job_id LIMIT 1 FOR UPDATE OF session SKIP LOCKED",
            ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(time).bind(&self.configurations)
                .fetch_optional(&mut *tx).await.map_err(database_error)?;
            // Empty owners advance as well, so the bounded catalog scan cannot
            // repeatedly stop before a later owner that has eligible work.
            let sequence = row
                .try_get::<i64, _>("dispatch_clock")
                .map_err(database_error)?
                .checked_add(index as i64 + 1)
                .ok_or(ApplicationError::Conflict)?;
            query(
                "UPDATE zuno_enterprise_preview.runtime_owner_schedule SET last_dispatch_sequence=GREATEST(last_dispatch_sequence,$3)
                 WHERE tenant_id=$1 AND principal_id=$2",
            ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(sequence)
                .execute(&mut *tx).await.map_err(database_error)?;
            let Some(candidate) = candidate else {
                tx.commit().await.map_err(database_error)?;
                continue;
            };
            let id: String = candidate.try_get("job_id").map_err(database_error)?;
            let job = read_job(&mut tx, &owner, &id).await?;
            let attempt = format!("attempt_{}", Uuid::new_v4().simple());
            let time = database_time(&mut tx).await?;
            let expires = time
                .checked_add(i64::from(duration.milliseconds()))
                .ok_or(ApplicationError::Conflict)?;
            // Recheck readiness after acquiring the session row lock.
            let epoch: Option<i64> = query_scalar(
                "UPDATE zuno_enterprise_preview.runtime_session SET lease_epoch=lease_epoch+1,current_job_id=$3,
                   lease_job_id=$3,lease_attempt_id=$4,lease_worker_id=$5,lease_expires=$6
                 WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$7 AND lease_job_id IS NULL
                   AND (current_job_id IS NULL OR current_job_id=$3)
                   AND EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_job r
                     WHERE r.tenant_id=$1 AND r.principal_id=$2 AND r.job_id=$3 AND r.phase='ready')
                 RETURNING lease_epoch",
            ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id)
                .bind(&attempt).bind(worker.as_str()).bind(expires).bind(job.session_id.as_str())
                .fetch_optional(&mut *tx).await.map_err(database_error)?;
            let Some(epoch) = epoch else {
                tx.commit().await.map_err(database_error)?;
                continue;
            };
            query(
                "INSERT INTO zuno_enterprise_preview.runtime_attempt(tenant_id,principal_id,id,job_id,worker_id,lease_epoch,state,started_at)
                 VALUES($1,$2,$3,$4,$5,$6,'running',$7)",
            ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&attempt).bind(&id)
                .bind(worker.as_str()).bind(epoch).bind(time).execute(&mut *tx).await.map_err(database_error)?;
            query(
                "UPDATE zuno_enterprise_preview.runtime_job SET phase='running',active_attempt_id=$4,time_updated=$5
                 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
            ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id).bind(&attempt).bind(time)
                .execute(&mut *tx).await.map_err(database_error)?;
            query(
                "UPDATE zuno_enterprise_preview.agent_job SET status='running',time_updated=$4
                 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND status='queued'",
            )
            .bind(owner.tenant_id.as_str())
            .bind(owner.principal_id.as_str())
            .bind(&id)
            .bind(time)
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
            emit(&mut tx,&job.principal,job.session_id.as_str(),"runtime.attempt.started",json!({
                "jobID":id,"attemptID":attempt,"workerID":worker,"epoch":epoch,"expiresAt":expires,
            })).await?;
            let claimed = ClaimedJob {
                lease: ExecutionLease {
                    owner: owner.clone(),
                    job_id: job.id.clone(),
                    session_id: job.session_id.clone(),
                    attempt_id: ExecutionAttemptId::new(attempt)
                        .map_err(ApplicationError::storage)?,
                    worker: worker.clone(),
                    epoch: unsigned(epoch)?,
                    checkpoint_version: job.checkpoint_version,
                    expires_at_ms: expires,
                },
                job: read_job(&mut tx, &owner, &id).await?,
            };
            tx.commit().await.map_err(database_error)?;
            return Ok(Some(claimed));
        }
        Ok(None)
    }

    async fn renew(
        &self,
        lease: &ExecutionLease,
        duration: LeaseDuration,
    ) -> Result<ExecutionLease, ApplicationError> {
        self.check_owner(&lease.owner)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        verify_lease(&mut tx, lease).await?;
        let expires = database_time(&mut tx)
            .await?
            .checked_add(i64::from(duration.milliseconds()))
            .ok_or(ApplicationError::Conflict)?;
        let expires_at_ms: i64 = query_scalar(
            "UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=GREATEST(lease_expires,$4)
             WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 RETURNING lease_expires",
        ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.session_id.as_str()).bind(expires)
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        Ok(ExecutionLease {
            expires_at_ms,
            ..lease.clone()
        })
    }

    async fn checkpoint(
        &self,
        lease: &ExecutionLease,
        checkpoint: RuntimeCheckpoint,
    ) -> Result<RuntimeJob, ApplicationError> {
        self.check_owner(&lease.owner)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let job = checkpoint_in(&mut tx, lease, checkpoint).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(job)
    }

    async fn finish(
        &self,
        lease: &ExecutionLease,
        outcome: JobFinish,
    ) -> Result<RuntimeJob, ApplicationError> {
        self.check_owner(&lease.owner)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let job = finish_in(&mut tx, lease, outcome).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(job)
    }
}
