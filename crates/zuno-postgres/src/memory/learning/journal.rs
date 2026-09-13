use super::*;
use zuno_learning::{LearningModelEvent, LearningModelOutcome};

impl PostgresLearningRuntime {
    pub async fn journal(&self, request: LearningJournalRequest) -> Result<(), Error> {
        if matches!(request.record.event, LearningModelEvent::Outcome { .. }) {
            return self.outcome(request).await;
        }
        let (actor, workspace, session) = self
            .job_binding(&request.lease.owner, &request.lease.job_id)
            .await?;
        let grants = self.grants.clone();
        self.memory.automate(actor,workspace,session,move |provider| provider.execute(async |tx| {
            let job=provider.check_execution(tx,&request.lease).await?;
            if request.record.session_id != job.session.as_str() {return Err(Error::Denied);}
            let expected_model=grants.iter().find_map(|grant| {
                if grant.extraction==job.configuration {Some(&grant.extraction_model)}
                else if grant.maintenance==job.configuration {Some(&grant.maintenance_model)}
                else {None}
            }).ok_or(Error::Denied)?;
            let LearningModelEvent::Request {request_id,model,request:input,prompt_digest,tools,..}=&request.record.event else{return Err(Error::Denied);};
            let operation=match job.input {LearningInput::Extraction(_)=>"learning.extraction",LearningInput::Maintenance(_)=>"learning.memory_consolidation"};
            if request.record.operation!=operation || model!=expected_model || !tools.is_empty()
                || input.get("tools")!=Some(&json!([]))
                || zuno_db::learning_source::digest(&input.to_string())!=*prompt_digest
                || input.to_string().len()>job.limits.maximum_input_bytes as usize
                || input.pointer("/parameters/maxTokens").and_then(Value::as_u64)
                    .is_none_or(|n|n==0||n>u64::from(job.limits.maximum_output_tokens))
                || request_id.is_empty() || request_id.len()>128
            {return Err(Error::Denied);}
            let digest=zuno_orchestration::sha256_json(&json!(request.record));
            let prior:Option<String>=query_scalar("SELECT request_digest FROM zuno_enterprise_preview.learning_model_request
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND request_id=$4")
                .bind(request.lease.owner.tenant_id.as_str()).bind(request.lease.owner.principal_id.as_str())
                .bind(job.id.as_str()).bind(request_id).fetch_optional(&mut **tx).await.map_err(sql_error)?;
            if let Some(prior)=prior {
                return if prior==digest {Ok(())} else {Err(Error::Conflict)};
            }
            let row=provider.execution_row(tx,&job.id).await?;
            let reserved:i64=row.try_get("reserved_tokens").map_err(sql_error)?;
            if reserved!=0 || job.tokens_charged.saturating_add(job.limits.request_tokens)>job.limits.total_tokens {
                return Err(Error::Conflict);
            }
            let now=database_time(tx).await.map_err(app_error)?;
            query("INSERT INTO zuno_enterprise_preview.learning_model_request
                (tenant_id,principal_id,job_id,request_id,epoch,request,request_digest,reserved_tokens,state,created_at)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,'prepared',$9)")
                .bind(request.lease.owner.tenant_id.as_str()).bind(request.lease.owner.principal_id.as_str())
                .bind(job.id.as_str()).bind(request_id).bind(request.lease.epoch as i64).bind(json!(request.record))
                .bind(digest).bind(job.limits.request_tokens as i64).bind(now)
                .execute(&mut **tx).await.map_err(sql_error)?;
            query("UPDATE zuno_enterprise_preview.learning_execution SET reserved_tokens=$4 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
                .bind(request.lease.owner.tenant_id.as_str()).bind(request.lease.owner.principal_id.as_str())
                .bind(job.id.as_str()).bind(job.limits.request_tokens as i64).execute(&mut **tx).await.map_err(sql_error)?;
            Ok(())
        })).await
    }

    /// A source-authenticated model receipt can settle an existing reservation
    /// after lease or consent loss. It cannot create a new request or apply Memory.
    async fn outcome(&self, request: LearningJournalRequest) -> Result<(), Error> {
        if request.lease.owner.tenant_id != self.tenant {
            return Err(Error::Denied);
        }
        request.lease.validate().map_err(app_error)?;
        let LearningModelEvent::Outcome {
            request_id,
            outcome,
            usage,
        } = &request.record.event
        else {
            return Err(Error::Denied);
        };
        if let LearningModelOutcome::Completed {
            output,
            output_digest,
            tool_calls,
        } = outcome
            && (zuno_db::learning_source::digest(output) != *output_digest
                || !tool_calls.is_empty()
                || output.len() > 2_097_152)
        {
            return Err(Error::InvalidData);
        }
        if usage.total_tokens
            != usage
                .input_tokens
                .saturating_add(usage.output_tokens)
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_write_input_tokens)
            || usage.provider_attempts == 0
        {
            return Err(Error::InvalidData);
        }
        let mut tx = owner_transaction(&self.memory.backend.pool, &request.lease.owner)
            .await
            .map_err(app_error)?;
        query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(zuno_orchestration::sha256_json(&json!([
                "memory",
                request.lease.owner
            ])))
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        let row=query("SELECT r.*,a.worker_id,a.lease_token,j.session_id FROM zuno_enterprise_preview.learning_model_request r
            JOIN zuno_enterprise_preview.learning_execution_attempt a
              ON a.tenant_id=r.tenant_id AND a.principal_id=r.principal_id AND a.job_id=r.job_id AND a.epoch=r.epoch
            JOIN zuno_enterprise_preview.learning_job j ON j.tenant_id=r.tenant_id AND j.principal_id=r.principal_id AND j.id=r.job_id
            WHERE r.tenant_id=$1 AND r.principal_id=$2 AND r.job_id=$3 AND r.request_id=$4 FOR UPDATE OF r")
            .bind(request.lease.owner.tenant_id.as_str()).bind(request.lease.owner.principal_id.as_str())
            .bind(request.lease.job_id.as_str()).bind(request_id).fetch_optional(&mut *tx).await.map_err(sql_error)?.ok_or(Error::Denied)?;
        if row.try_get::<String, _>("worker_id").map_err(sql_error)?
            != request.lease.worker.as_str()
            || row.try_get::<String, _>("lease_token").map_err(sql_error)? != request.lease.token
            || row.try_get::<i64, _>("epoch").map_err(sql_error)? != request.lease.epoch as i64
            || row.try_get::<String, _>("session_id").map_err(sql_error)?
                != request.record.session_id
        {
            return Err(Error::Denied);
        }
        let prepared: zuno_learning::LearningModelRecord =
            serde_json::from_value(row.try_get("request").map_err(sql_error)?)
                .map_err(decode_error)?;
        if prepared.operation != request.record.operation
            || zuno_orchestration::sha256_json(&json!(prepared))
                != row
                    .try_get::<String, _>("request_digest")
                    .map_err(sql_error)?
        {
            return Err(Error::Conflict);
        }
        let digest = zuno_orchestration::sha256_json(&json!(request.record));
        if let Some(prior) = row
            .try_get::<Option<String>, _>("outcome_digest")
            .map_err(sql_error)?
        {
            return if prior == digest {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        let reserved: i64 = row.try_get("reserved_tokens").map_err(sql_error)?;
        let charge = if usage.accounted {
            usage.total_tokens
        } else {
            usage.total_tokens.max(reserved as u64)
        };
        let charge = i64::try_from(charge).map_err(decode_error)?;
        let was_unknown = row.try_get::<String, _>("state").map_err(sql_error)? == "unknown";
        query(
            "UPDATE zuno_enterprise_preview.learning_execution SET
            charged_tokens=charged_tokens+$4-$5,reserved_tokens=reserved_tokens-$6
            WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
        )
        .bind(request.lease.owner.tenant_id.as_str())
        .bind(request.lease.owner.principal_id.as_str())
        .bind(request.lease.job_id.as_str())
        .bind(charge)
        .bind(if was_unknown { reserved } else { 0 })
        .bind(if was_unknown { 0 } else { reserved })
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
        let now = database_time(&mut tx).await.map_err(app_error)?;
        query("UPDATE zuno_enterprise_preview.learning_model_request SET state=$5,outcome=$6,outcome_digest=$7,completed_at=$8
            WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND request_id=$4")
            .bind(request.lease.owner.tenant_id.as_str()).bind(request.lease.owner.principal_id.as_str()).bind(request.lease.job_id.as_str())
            .bind(request_id).bind(if matches!(outcome,LearningModelOutcome::Completed{..}){"completed"}else{"failed"})
            .bind(json!(request.record)).bind(digest).bind(now).execute(&mut *tx).await.map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)
    }
}
