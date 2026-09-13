use super::*;
use zuno_application::skill::*;
use zuno_types::identity::RequestId;

fn app(error: Error) -> ApplicationError {
    match error {
        Error::Denied => ApplicationError::Forbidden,
        Error::Conflict => ApplicationError::Conflict,
        Error::Invalid(message) => ApplicationError::Invalid(message),
        _ => ApplicationError::Unavailable,
    }
}
async fn access(
    tx: &mut Tx,
    actor: &PrincipalScope,
    review: bool,
) -> Result<u64, ApplicationError> {
    let access = crate::authorization::access_in(tx, &actor.owner()).await?;
    if zuno_permission::enterprise::actor_denial(&access.policy, &access.member, actor).is_some()
        || actor.kind() != zuno_types::identity::PrincipalKind::User
        || (review
            && actor
                .client_id()
                .is_none_or(|id| !access.policy.approval_apps.contains(id)))
    {
        return Err(ApplicationError::Forbidden);
    }
    Ok(access.policy.revision.get())
}
async fn lock(tx: &mut Tx, actor: &PrincipalScope) -> Result<(), ApplicationError> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(zuno_orchestration::sha256_json(&json!([
            "memory",
            actor.owner()
        ])))
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
    Ok(())
}
async fn read(
    tx: &mut Tx,
    actor: &PrincipalScope,
    id: &RequestId,
) -> Result<SkillCandidateView, ApplicationError> {
    let row=query("SELECT s.*,j.status AS job_status FROM zuno_enterprise_preview.skill_candidate s
        LEFT JOIN zuno_enterprise_preview.learning_job j ON j.tenant_id=s.tenant_id AND j.principal_id=s.principal_id AND j.id=s.evaluation_job_id
        WHERE s.tenant_id=$1 AND s.principal_id=$2 AND s.id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(id.as_str())
        .fetch_optional(&mut **tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
    let input: ProposeSkill = serde_json::from_value(row.try_get("input").map_err(database_error)?)
        .map_err(ApplicationError::storage)?;
    let hash: String = row.try_get("input_digest").map_err(database_error)?;
    let source_job_id = JobId::new(
        row.try_get::<String, _>("source_job_id")
            .map_err(database_error)?,
    )
    .map_err(ApplicationError::storage)?;
    let evaluation: zuno_application::runtime::ConfigurationRef =
        serde_json::from_value(row.try_get("evaluation").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    if hash != zuno_orchestration::sha256_json(&json!([source_job_id, evaluation, input])) {
        return Err(ApplicationError::Conflict);
    }
    let mut state: SkillEvaluationState =
        serde_json::from_value(Value::String(row.try_get("state").map_err(database_error)?))
            .map_err(ApplicationError::storage)?;
    if state == SkillEvaluationState::Evaluating {
        state = match row
            .try_get::<Option<String>, _>("job_status")
            .map_err(database_error)?
            .as_deref()
        {
            Some("failed" | "uncertain") => SkillEvaluationState::Failed,
            Some("skipped") => SkillEvaluationState::Cancelled,
            _ => state,
        };
    }
    Ok(SkillCandidateView {
        id: id.clone(),
        source_job_id,
        name: input.name,
        baseline_content: input.baseline_content,
        proposed_content: input.proposed_content,
        cases: input.cases,
        digest: hash,
        evaluation,
        state,
        job_id: row
            .try_get::<Option<String>, _>("evaluation_job_id")
            .map_err(database_error)?
            .map(JobId::new)
            .transpose()
            .map_err(ApplicationError::storage)?,
        report: row
            .try_get::<Option<Value>, _>("report")
            .map_err(database_error)?
            .map(serde_json::from_value)
            .transpose()
            .map_err(ApplicationError::storage)?,
    })
}
async fn prior(
    tx: &mut Tx,
    actor: &PrincipalScope,
    id: &RequestId,
    hash: &str,
) -> Result<Option<SkillCandidateView>, ApplicationError> {
    let row = query(
        "SELECT request_digest,response FROM zuno_enterprise_preview.skill_request
        WHERE tenant_id=$1 AND principal_id=$2 AND request_id=$3",
    )
    .bind(actor.tenant_id().as_str())
    .bind(actor.principal_id().as_str())
    .bind(id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_error)?;
    row.map(|row| {
        if row
            .try_get::<String, _>("request_digest")
            .map_err(database_error)?
            != hash
        {
            return Err(ApplicationError::Conflict);
        }
        serde_json::from_value(row.try_get("response").map_err(database_error)?)
            .map_err(ApplicationError::storage)
    })
    .transpose()
}
async fn record(
    tx: &mut Tx,
    actor: &PrincipalScope,
    id: &RequestId,
    request: &RequestId,
    operation: &str,
    hash: &str,
    value: &SkillCandidateView,
) -> Result<(), ApplicationError> {
    query("INSERT INTO zuno_enterprise_preview.skill_request(tenant_id,principal_id,request_id,request_digest,response) VALUES($1,$2,$3,$4,$5)")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(request.as_str()).bind(hash)
        .bind(json!(value)).execute(&mut **tx).await.map_err(database_error)?;
    let now = database_time(tx).await?;
    query("INSERT INTO zuno_enterprise_preview.skill_audit(tenant_id,principal_id,id,candidate_id,actor,operation,data,time_created)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(uuid::Uuid::now_v7().to_string()).bind(id.as_str())
        .bind(json!(actor)).bind(operation).bind(json!({"requestId":request,"requestDigest":hash,"state":value.state})).bind(now)
        .execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}
#[async_trait]
impl SkillApplication for PostgresLearningRuntime {
    async fn propose(
        &self,
        actor: &PrincipalScope,
        source: &JobId,
        request: ProposeSkill,
    ) -> Result<SkillCandidateView, ApplicationError> {
        request.validate()?;
        if actor.tenant_id() != &self.tenant {
            return Err(ApplicationError::Forbidden);
        }
        let mut tx = owner_transaction(&self.memory.backend.pool, &actor.owner()).await?;
        access(&mut tx, actor, false).await?;
        lock(&mut tx, actor).await?;
        let source = crate::runtime::read_job(&mut tx, &actor.owner(), source.as_str()).await?;
        if source.phase != zuno_application::runtime::JobPhase::Completed {
            return Err(ApplicationError::Conflict);
        }
        let grant = self
            .skills
            .iter()
            .find(|grant| grant.source == source.configuration)
            .ok_or(ApplicationError::Forbidden)?;
        let workspace:String=query_scalar("SELECT workspace_id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(source.session_id.as_str())
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        if workspace != grant.workspace.as_str() {
            return Err(ApplicationError::Forbidden);
        }
        let hash = zuno_orchestration::sha256_json(&json!([source.id, grant.evaluation, request]));
        if let Some(value) = prior(&mut tx, actor, &request.request_id, &hash).await? {
            return Ok(value);
        }
        let id = RequestId::new(format!(
            "skc_{}",
            zuno_orchestration::sha256_json(&json!([actor.owner(), request.request_id]))
        ))
        .map_err(ApplicationError::storage)?;
        let now = database_time(&mut tx).await?;
        query("INSERT INTO zuno_enterprise_preview.skill_candidate
            (tenant_id,principal_id,id,source_job_id,session_id,workspace_id,configuration,evaluation,input,input_digest,state,time_created,time_updated)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'pending_review',$11,$11)")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(id.as_str()).bind(source.id.as_str())
            .bind(source.session_id.as_str()).bind(&workspace).bind(json!(source.configuration)).bind(json!(grant.evaluation))
            .bind(json!(request)).bind(&hash).bind(now).execute(&mut *tx).await.map_err(database_error)?;
        let value = read(&mut tx, actor, &id).await?;
        record(
            &mut tx,
            actor,
            &id,
            &request.request_id,
            "propose",
            &hash,
            &value,
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    async fn candidate(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
    ) -> Result<SkillCandidateView, ApplicationError> {
        if actor.tenant_id() != &self.tenant {
            return Err(ApplicationError::Forbidden);
        }
        let mut tx = owner_transaction(&self.memory.backend.pool, &actor.owner()).await?;
        access(&mut tx, actor, false).await?;
        let value = read(&mut tx, actor, id).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    async fn evaluate(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
        request: ReviewSkillEvaluation,
    ) -> Result<SkillCandidateView, ApplicationError> {
        if actor.tenant_id() != &self.tenant {
            return Err(ApplicationError::Forbidden);
        }
        let mut tx = owner_transaction(&self.memory.backend.pool, &actor.owner()).await?;
        let policy = access(&mut tx, actor, true).await?;
        lock(&mut tx, actor).await?;
        let view = read(&mut tx, actor, id).await?;
        let hash = zuno_orchestration::sha256_json(&json!([id, request]));
        if let Some(value) = prior(&mut tx, actor, &request.request_id, &hash).await? {
            return Ok(value);
        }
        if view.digest != request.expected_digest
            || !matches!(
                view.state,
                SkillEvaluationState::PendingReview
                    | SkillEvaluationState::Failed
                    | SkillEvaluationState::Cancelled
            )
        {
            return Err(ApplicationError::Conflict);
        }
        let source =
            crate::runtime::read_job(&mut tx, &actor.owner(), view.source_job_id.as_str()).await?;
        let grant = self
            .skills
            .iter()
            .find(|grant| {
                grant.source == source.configuration && grant.evaluation == view.evaluation
            })
            .ok_or(ApplicationError::Forbidden)?;
        if grant.limits.total_tokens
            < grant
                .limits
                .request_tokens
                .saturating_mul(view.cases.len() as u64)
                .saturating_mul(4)
        {
            return Err(ApplicationError::Invalid(
                "Skill evaluation budget cannot fit one paired attempt and grade per case"
                    .to_owned(),
            ));
        }
        let job = JobId::new(format!(
            "learn_{}",
            zuno_orchestration::sha256_json(&json!([
                "skill",
                actor.owner(),
                id,
                view.digest,
                request.request_id
            ]))
        ))
        .map_err(ApplicationError::storage)?;
        let input = SkillEvaluationInput {
            session_id: source.session_id.to_string(),
            candidate_id: id.clone(),
            baseline_content: view.baseline_content,
            proposed_content: view.proposed_content,
            cases: view.cases,
            maximum_steps: grant.maximum_steps,
        };
        input.validate()?;
        store::insert_execution_in(
            &mut tx,
            actor,
            &grant.workspace,
            NewExecution {
                id: &job,
                source_job: &source.id,
                session: &source.session_id,
                configuration: &grant.evaluation,
                input: LearningInput::SkillEvaluation(input),
                limits: &grant.limits,
                context: json!({"candidateId":id,"candidateDigest":view.digest}),
            },
        )
        .await
        .map_err(app)?;
        let now = database_time(&mut tx).await?;
        query("UPDATE zuno_enterprise_preview.skill_candidate SET state='evaluating',evaluation_job_id=$4,reviewer=$5,policy_revision=$6,time_updated=$7,report=NULL
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(id.as_str()).bind(job.as_str())
            .bind(json!(actor)).bind(policy as i64).bind(now).execute(&mut *tx).await.map_err(database_error)?;
        let value = read(&mut tx, actor, id).await?;
        record(
            &mut tx,
            actor,
            id,
            &request.request_id,
            "evaluate",
            &hash,
            &value,
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
}
