use super::*;
use zuno_application::skill::*;

impl TransactionMemory {
    pub(super) async fn skill_trace(
        &self,
        tx: &mut Tx,
        job: &LearningExecution,
        case: &SkillEvaluationCase,
        baseline: bool,
        epoch: u64,
    ) -> Result<(Option<String>, Value, u64), Error> {
        let role = if baseline { "baseline" } else { "candidate" };
        let rows = query(
            "SELECT outcome,outcome_digest FROM zuno_enterprise_preview.learning_model_request
            WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND epoch=$4 AND state='completed'
              AND request->>'operation'=$5 ORDER BY created_at,request_id",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(job.id.as_str())
        .bind(epoch as i64)
        .bind(format!(
            "skill.{}.{role}.learning.evaluation.attempt",
            case.id
        ))
        .fetch_all(&mut **tx)
        .await
        .map_err(sql_error)?;
        if rows.is_empty() {
            return Err(Error::Denied);
        }
        let mut outcomes = Vec::new();
        for row in rows {
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
            let zuno_learning::LearningModelEvent::Outcome { outcome, .. } = record.event else {
                return Err(Error::InvalidData);
            };
            outcomes.push(outcome);
        }
        zuno_learning::skill_attempt_trace(case, &outcomes).map_err(|_| Error::InvalidData)
    }
    pub(super) async fn validate_skill_grader(
        &self,
        tx: &mut Tx,
        job: &LearningExecution,
        input: &SkillEvaluationInput,
        record: &zuno_learning::LearningModelRecord,
        epoch: u64,
    ) -> Result<(), Error> {
        let (case, baseline, grade) =
            zuno_learning::skill_request_identity(input, &record.operation).ok_or(Error::Denied)?;
        if !grade {
            return Ok(());
        }
        let (answer, trace, unmatched) = self.skill_trace(tx, job, case, baseline, epoch).await?;
        let answer = answer.ok_or(Error::Denied)?;
        let zuno_learning::LearningModelEvent::Request { request, .. } = &record.event else {
            return Err(Error::InvalidData);
        };
        let text = request
            .pointer("/messages/1/content/0/text")
            .and_then(Value::as_str)
            .ok_or(Error::InvalidData)?;
        let value: Value = serde_json::from_str(text).map_err(decode_error)?;
        if value["actualAnswer"] != answer
            || value["trace"] != trace
            || value["unmatchedCalls"] != unmatched
        {
            return Err(Error::Denied);
        }
        Ok(())
    }
    pub(super) async fn verify_skill_report(
        &self,
        tx: &mut Tx,
        job: &LearningExecution,
        input: &SkillEvaluationInput,
        report: &SkillEvaluationReport,
        epoch: u64,
    ) -> Result<(), Error> {
        input.validate().map_err(app_error)?;
        let expected = SkillEvaluationReport::from_cases(&input.cases, report.cases.clone())
            .map_err(app_error)?;
        if expected != *report {
            return Err(Error::Denied);
        }
        for result in &report.cases {
            for (role, observation) in [
                ("baseline", &result.baseline),
                ("candidate", &result.candidate),
            ] {
                let operation =
                    format!("skill.{}.{role}.learning.evaluation.grade", result.case_id);
                let row=query("SELECT request,outcome,outcome_digest FROM zuno_enterprise_preview.learning_model_request
                    WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND epoch=$4
                      AND state='completed' AND request->>'operation'=$5
                    ORDER BY created_at DESC,request_id DESC LIMIT 1")
                    .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.id.as_str())
                    .bind(epoch as i64).bind(&operation).fetch_optional(&mut **tx).await.map_err(sql_error)?;
                let Some(row) = row else {
                    let count:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.learning_model_request
                        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND epoch=$4 AND state='completed'
                          AND request->>'operation'=$5 AND jsonb_array_length(outcome->'event'->'outcome'->'toolCalls')>0")
                        .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.id.as_str())
                        .bind(epoch as i64).bind(format!("skill.{}.{role}.learning.evaluation.attempt",result.case_id))
                        .fetch_one(&mut **tx).await.map_err(sql_error)?;
                    if observation.score != 0
                        || observation.passed
                        || observation.critical_failure
                        || observation.details["reason"] != "step_budget"
                        || count != i64::from(input.maximum_steps)
                    {
                        return Err(Error::Denied);
                    }
                    let case = input
                        .cases
                        .iter()
                        .find(|case| case.id == result.case_id)
                        .ok_or(Error::InvalidData)?;
                    let (answer, trace, _) = self
                        .skill_trace(tx, job, case, role == "baseline", epoch)
                        .await?;
                    if answer.is_some() || observation.details["trace"] != trace {
                        return Err(Error::Denied);
                    }
                    continue;
                };
                let raw: Value = row.try_get("outcome").map_err(sql_error)?;
                if row
                    .try_get::<String, _>("outcome_digest")
                    .map_err(sql_error)?
                    != zuno_orchestration::sha256_json(&raw)
                {
                    return Err(Error::InvalidData);
                }
                let outcome: zuno_learning::LearningModelRecord =
                    serde_json::from_value(raw).map_err(decode_error)?;
                let zuno_learning::LearningModelEvent::Outcome {
                    outcome:
                        zuno_learning::LearningModelOutcome::Completed {
                            output, tool_calls, ..
                        },
                    ..
                } = outcome.event
                else {
                    return Err(Error::Denied);
                };
                if !tool_calls.is_empty() {
                    return Err(Error::Denied);
                }
                let grade =
                    zuno_learning::decode_skill_grade(&output).map_err(|_| Error::InvalidData)?;
                if grade.score != observation.score
                    || grade.passed != observation.passed
                    || grade.critical_failure != observation.critical_failure
                    || grade.explanation != observation.details["grader"]
                    || observation.details["liveTools"] != false
                {
                    return Err(Error::Denied);
                }
                let prepared: zuno_learning::LearningModelRecord =
                    serde_json::from_value(row.try_get("request").map_err(sql_error)?)
                        .map_err(decode_error)?;
                let zuno_learning::LearningModelEvent::Request { request, .. } = prepared.event
                else {
                    return Err(Error::InvalidData);
                };
                let text = request
                    .pointer("/messages/1/content/0/text")
                    .and_then(Value::as_str)
                    .ok_or(Error::InvalidData)?;
                let value: Value = serde_json::from_str(text).map_err(decode_error)?;
                if value["actualAnswer"] != observation.details["answer"]
                    || value["trace"] != observation.details["trace"]
                    || value["unmatchedCalls"] != observation.details["unmatchedCalls"]
                {
                    return Err(Error::Denied);
                }
            }
        }
        Ok(())
    }
    pub(super) async fn settle_skill(
        &self,
        tx: &mut Tx,
        job: &LearningExecution,
        input: &SkillEvaluationInput,
        report: &SkillEvaluationReport,
    ) -> Result<(), Error> {
        let now = database_time(tx).await.map_err(app_error)?;
        let changed=query("UPDATE zuno_enterprise_preview.skill_candidate SET state=$4,report=$5,time_updated=$6
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND evaluation_job_id=$7 AND state='evaluating'")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(input.candidate_id.as_str())
            .bind(if report.passed{"passed"}else{"failed"}).bind(json!(report)).bind(now).bind(job.id.as_str())
            .execute(&mut **tx).await.map_err(sql_error)?.rows_affected();
        if changed != 1 {
            return Err(Error::Conflict);
        }
        query("UPDATE zuno_enterprise_preview.learning_job SET status='completed',owner_id=NULL,lease_token=NULL,lease_expires=NULL,
            result=$4,time_updated=$5 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.id.as_str())
            .bind(json!({"skillCandidateId":input.candidate_id,"passed":report.passed})).bind(now)
            .execute(&mut **tx).await.map_err(sql_error)?;
        Ok(())
    }
    pub(super) async fn require_learning_authority(
        &self,
        tx: &mut Tx,
        session: Option<&str>,
    ) -> Result<(), Error> {
        let Some(job) = &self.skill_evaluation else {
            return self.require_automation(tx, session).await;
        };
        let session = session.ok_or(Error::Denied)?;
        self.ensure_session(tx, session).await?;
        let access = crate::authorization::access_in(tx, &self.principal.owner())
            .await
            .map_err(app_error)?;
        if zuno_permission::enterprise::actor_denial(
            &access.policy,
            &access.member,
            &self.principal,
        )
        .is_some()
            || self.principal.kind() != zuno_types::identity::PrincipalKind::User
            || self
                .principal
                .client_id()
                .is_none_or(|id| !access.policy.approval_apps.contains(id))
        {
            return Err(Error::Denied);
        }
        let row = query(
            "SELECT reviewer,policy_revision FROM zuno_enterprise_preview.skill_candidate
            WHERE tenant_id=$1 AND principal_id=$2 AND evaluation_job_id=$3 AND session_id=$4
              AND workspace_id=$5 AND state IN('evaluating','passed','failed')",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(job.as_str())
        .bind(session)
        .bind(self.workspace.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(sql_error)?
        .ok_or(Error::Denied)?;
        let reviewer: PrincipalScope =
            serde_json::from_value(row.try_get("reviewer").map_err(sql_error)?)
                .map_err(decode_error)?;
        if reviewer != self.principal
            || row
                .try_get::<i64, _>("policy_revision")
                .map_err(sql_error)?
                != access.policy.revision.get() as i64
        {
            return Err(Error::Denied);
        }
        Ok(())
    }
}
impl PostgresLearningRuntime {
    pub(super) async fn skill_job(&self, owner: &PrincipalKey, id: &JobId) -> Result<bool, Error> {
        let mut tx = owner_transaction(&self.memory.backend.pool, owner)
            .await
            .map_err(app_error)?;
        let value: bool = query_scalar(
            "SELECT phase='skill_evaluation' FROM zuno_enterprise_preview.learning_execution
            WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(sql_error)?
        .ok_or(Error::Denied)?;
        tx.commit().await.map_err(sql_error)?;
        Ok(value)
    }
    pub(super) async fn with_job<T: Send + 'static>(
        &self,
        owner: &PrincipalKey,
        id: &JobId,
        work: impl FnOnce(Arc<TransactionMemory>) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let (actor, workspace, session) = self.job_binding(owner, id).await?;
        let skill = self.skill_job(owner, id).await?.then(|| id.clone());
        self.memory
            .learning_transaction(actor, workspace, session, skill, work)
            .await
    }
}
