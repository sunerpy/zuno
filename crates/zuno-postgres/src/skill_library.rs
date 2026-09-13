//! Owner-scoped, versioned Skill documents. Installing reviewed content and
//! activating it are separate explicit decisions.
use crate::{PostgresBackend, database_error, database_time, owner_transaction};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::{PgRow, Postgres};
use zuno_application::{ApplicationError, PageSize, skill::*};
use zuno_types::{
    activity::Counter,
    identity::{JobId, PrincipalScope, RequestId, WorkspaceId},
};
type Tx<'a> = Transaction<'a, Postgres>;
#[derive(Clone)]
pub struct PostgresSkillLibrary {
    backend: PostgresBackend,
}
impl PostgresBackend {
    pub fn skill_library(&self) -> PostgresSkillLibrary {
        PostgresSkillLibrary {
            backend: self.clone(),
        }
    }
}
fn invalid() -> ApplicationError {
    ApplicationError::Invalid("invalid Skill installation or revision".to_owned())
}
fn number(value: u64) -> Result<i64, ApplicationError> {
    i64::try_from(value).map_err(|_| invalid())
}
async fn access(
    tx: &mut Tx<'_>,
    actor: &PrincipalScope,
    review: bool,
) -> Result<(), ApplicationError> {
    let access = crate::authorization::access_in(tx, &actor.owner()).await?;
    if zuno_permission::enterprise::actor_denial(&access.policy, &access.member, actor).is_some()
        || (review
            && (actor.kind() != zuno_types::identity::PrincipalKind::User
                || actor
                    .client_id()
                    .is_none_or(|id| !access.policy.approval_apps.contains(id))))
    {
        return Err(ApplicationError::Forbidden);
    }
    Ok(())
}
async fn lock(tx: &mut Tx<'_>, actor: &PrincipalScope) -> Result<(), ApplicationError> {
    query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(zuno_orchestration::sha256_json(&json!([
            "skill-library",
            actor.owner()
        ])))
        .execute(&mut **tx)
        .await
        .map_err(database_error)?;
    Ok(())
}
fn view(row: &PgRow) -> Result<InstalledSkillView, ApplicationError> {
    let revision = u64::try_from(row.try_get::<i64, _>("revision").map_err(database_error)?)
        .map_err(|_| invalid())?;
    Ok(InstalledSkillView {
        id: RequestId::new(row.try_get::<String, _>("id").map_err(database_error)?)
            .map_err(ApplicationError::storage)?,
        workspace_id: WorkspaceId::new(
            row.try_get::<String, _>("workspace_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        candidate_id: RequestId::new(
            row.try_get::<String, _>("candidate_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        name: row.try_get("name").map_err(database_error)?,
        description: row.try_get("description").map_err(database_error)?,
        source: row.try_get("source").map_err(database_error)?,
        revision: Counter(revision),
        content_digest: row.try_get("content_digest").map_err(database_error)?,
        active: row.try_get("active").map_err(database_error)?,
    })
}
async fn read(
    tx: &mut Tx<'_>,
    actor: &PrincipalScope,
    id: &RequestId,
) -> Result<InstalledSkillDocument, ApplicationError> {
    let row=query("SELECT * FROM zuno_enterprise_preview.skill_installation WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(id.as_str())
        .fetch_optional(&mut **tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
    let skill = view(&row)?;
    let content: String = row.try_get("content").map_err(database_error)?;
    if zuno_orchestration::sha256_text(&content) != skill.content_digest {
        return Err(ApplicationError::Conflict);
    }
    if skill.source != format!("enterprise-skill://{}/{}", skill.id, skill.revision.0) {
        return Err(ApplicationError::Conflict);
    }
    let candidate=query("SELECT source_job_id,session_id,workspace_id,evaluation,input,input_digest FROM zuno_enterprise_preview.skill_candidate
        WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(skill.candidate_id.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    let input: ProposeSkill =
        serde_json::from_value(candidate.try_get("input").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let evaluation: zuno_application::runtime::ConfigurationRef =
        serde_json::from_value(candidate.try_get("evaluation").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let source = JobId::new(
        candidate
            .try_get::<String, _>("source_job_id")
            .map_err(database_error)?,
    )
    .map_err(ApplicationError::storage)?;
    let digest = zuno_orchestration::sha256_json(&json!([source, evaluation, input]));
    if input.name != skill.name
        || candidate
            .try_get::<String, _>("workspace_id")
            .map_err(database_error)?
            != skill.workspace_id.as_str()
        || input.proposed_content != content
        || digest
            != candidate
                .try_get::<String, _>("input_digest")
                .map_err(database_error)?
        || digest
            != row
                .try_get::<String, _>("candidate_digest")
                .map_err(database_error)?
    {
        return Err(ApplicationError::Conflict);
    }
    let report: SkillEvaluationReport =
        serde_json::from_value(row.try_get("evaluation_report").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let proof = query(
        "SELECT j.result,e.input,e.context,e.configuration,e.source_job_id FROM zuno_enterprise_preview.learning_job j
        JOIN zuno_enterprise_preview.learning_execution e
          ON e.tenant_id=j.tenant_id AND e.principal_id=j.principal_id AND e.job_id=j.id
        WHERE j.tenant_id=$1 AND j.principal_id=$2 AND j.id=$3 AND j.status='completed'
          AND e.phase='skill_evaluation'",
    )
    .bind(actor.tenant_id().as_str())
    .bind(actor.principal_id().as_str())
    .bind(
        row.try_get::<String, _>("evaluation_job_id")
            .map_err(database_error)?,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_error)?
    .ok_or(ApplicationError::Conflict)?;
    let execution_input: zuno_learning::distributed::LearningInput =
        serde_json::from_value(proof.try_get("input").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let zuno_learning::distributed::LearningInput::SkillEvaluation(execution_input) =
        execution_input
    else {
        return Err(ApplicationError::Conflict);
    };
    let context: Value = proof.try_get("context").map_err(database_error)?;
    let result: Value = proof.try_get("result").map_err(database_error)?;
    if execution_input.candidate_id != skill.candidate_id
        || execution_input.baseline_content != input.baseline_content
        || execution_input.proposed_content != input.proposed_content
        || execution_input.cases != input.cases
        || execution_input.session_id
            != candidate
                .try_get::<String, _>("session_id")
                .map_err(database_error)?
        || context["candidateDigest"] != digest
        || proof
            .try_get::<String, _>("source_job_id")
            .map_err(database_error)?
            != source.as_str()
        || proof
            .try_get::<Value, _>("configuration")
            .map_err(database_error)?
            != json!(evaluation)
    {
        return Err(ApplicationError::Conflict);
    }
    if !report.passed
        || SkillEvaluationReport::from_cases(&input.cases, report.cases.clone())? != report
        || result["completionDigest"]
            != zuno_orchestration::sha256_json(&json!(
                zuno_learning::distributed::LearningOutput::SkillEvaluation(report)
            ))
    {
        return Err(ApplicationError::Conflict);
    }
    Ok(InstalledSkillDocument { skill, content })
}
async fn prior(
    tx: &mut Tx<'_>,
    actor: &PrincipalScope,
    id: &RequestId,
    digest: &str,
) -> Result<Option<InstalledSkillView>, ApplicationError> {
    let row = query(
        "SELECT request_digest,response FROM zuno_enterprise_preview.skill_installation_request
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
            != digest
        {
            return Err(ApplicationError::Conflict);
        }
        serde_json::from_value(row.try_get("response").map_err(database_error)?)
            .map_err(ApplicationError::storage)
    })
    .transpose()
}
async fn record(
    tx: &mut Tx<'_>,
    actor: &PrincipalScope,
    request: &RequestId,
    digest: &str,
    document: &InstalledSkillDocument,
) -> Result<(), ApplicationError> {
    let now = database_time(tx).await?;
    let proof=query("SELECT evaluation_job_id,candidate_digest,evaluation_report FROM zuno_enterprise_preview.skill_installation
        WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(document.skill.id.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    let data = json!({"document":document,"evaluationJobId":proof.try_get::<String,_>("evaluation_job_id").map_err(database_error)?,
        "candidateDigest":proof.try_get::<String,_>("candidate_digest").map_err(database_error)?,
        "evaluationReport":proof.try_get::<Value,_>("evaluation_report").map_err(database_error)?});
    query("INSERT INTO zuno_enterprise_preview.skill_installation_revision
        (tenant_id,principal_id,installation_id,revision,data,actor,time_created) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(document.skill.id.as_str())
        .bind(number(document.skill.revision.0)?).bind(data).bind(json!(actor)).bind(now)
        .execute(&mut **tx).await.map_err(database_error)?;
    query("INSERT INTO zuno_enterprise_preview.skill_installation_request(tenant_id,principal_id,request_id,request_digest,response) VALUES($1,$2,$3,$4,$5)")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(request.as_str()).bind(digest).bind(json!(document.skill))
        .execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}
#[async_trait]
impl SkillLibrary for PostgresSkillLibrary {
    async fn install(
        &self,
        actor: &PrincipalScope,
        candidate: &RequestId,
        request: InstallSkill,
    ) -> Result<InstalledSkillView, ApplicationError> {
        if request.description.trim().is_empty()
            || request.description.len() > 1200
            || request.description.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let mut tx = owner_transaction(&self.backend.pool, &actor.owner()).await?;
        access(&mut tx, actor, true).await?;
        lock(&mut tx, actor).await?;
        let hash = zuno_orchestration::sha256_json(&json!(["install", candidate, request]));
        if let Some(value) = prior(&mut tx, actor, &request.request_id, &hash).await? {
            return Ok(value);
        }
        let row=query("SELECT c.*,j.status AS job_status FROM zuno_enterprise_preview.skill_candidate c
            JOIN zuno_enterprise_preview.learning_job j ON j.tenant_id=c.tenant_id AND j.principal_id=c.principal_id AND j.id=c.evaluation_job_id
            WHERE c.tenant_id=$1 AND c.principal_id=$2 AND c.id=$3")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(candidate.as_str())
            .fetch_optional(&mut *tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
        if row.try_get::<String, _>("state").map_err(database_error)? != "passed"
            || row
                .try_get::<String, _>("job_status")
                .map_err(database_error)?
                != "completed"
            || row
                .try_get::<String, _>("input_digest")
                .map_err(database_error)?
                != request.expected_digest
        {
            return Err(ApplicationError::Conflict);
        }
        let input: ProposeSkill =
            serde_json::from_value(row.try_get("input").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        input.validate()?;
        let source = JobId::new(
            row.try_get::<String, _>("source_job_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?;
        let evaluation: zuno_application::runtime::ConfigurationRef =
            serde_json::from_value(row.try_get("evaluation").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        if zuno_orchestration::sha256_json(&json!([source, evaluation, input]))
            != request.expected_digest
        {
            return Err(ApplicationError::Conflict);
        }
        zuno_types::activity::ActivityName::new(&input.name).map_err(|_| invalid())?;
        let report: SkillEvaluationReport =
            serde_json::from_value(row.try_get("report").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        if !report.passed
            || SkillEvaluationReport::from_cases(&input.cases, report.cases.clone())? != report
        {
            return Err(ApplicationError::Conflict);
        }
        let workspace: String = row.try_get("workspace_id").map_err(database_error)?;
        let existing: Option<String> = query_scalar(
            "SELECT id FROM zuno_enterprise_preview.skill_installation
            WHERE tenant_id=$1 AND principal_id=$2 AND workspace_id=$3 AND name=$4",
        )
        .bind(actor.tenant_id().as_str())
        .bind(actor.principal_id().as_str())
        .bind(&workspace)
        .bind(&input.name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?;
        let revision = request
            .expected_revision
            .0
            .checked_add(1)
            .ok_or_else(invalid)?;
        let id = if let Some(id) = existing {
            let id = RequestId::new(id).map_err(ApplicationError::storage)?;
            let prior = read(&mut tx, actor, &id).await?;
            if prior.skill.revision != request.expected_revision
                || prior.content != input.baseline_content
            {
                return Err(ApplicationError::Conflict);
            }
            id
        } else {
            if request.expected_revision.0 != 0 {
                return Err(ApplicationError::Conflict);
            }
            let count:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.skill_installation WHERE tenant_id=$1 AND principal_id=$2 AND workspace_id=$3")
                .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(&workspace).fetch_one(&mut *tx).await.map_err(database_error)?;
            if count >= 64 {
                return Err(ApplicationError::Invalid(
                    "at most 64 installed Skills per workspace".to_owned(),
                ));
            }
            RequestId::new(format!(
                "ski_{}",
                zuno_orchestration::sha256_json(&json!([actor.owner(), workspace, input.name]))
            ))
            .map_err(ApplicationError::storage)?
        };
        let source = format!("enterprise-skill://{id}/{revision}");
        query("INSERT INTO zuno_enterprise_preview.skill_installation
            (tenant_id,principal_id,id,workspace_id,candidate_id,name,description,revision,source,content,content_digest,active,evaluation_job_id,candidate_digest,evaluation_report)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,false,$12,$13,$14)
            ON CONFLICT(tenant_id,principal_id,id) DO UPDATE SET candidate_id=excluded.candidate_id,description=excluded.description,
            revision=excluded.revision,source=excluded.source,content=excluded.content,content_digest=excluded.content_digest,
            active=false,evaluation_job_id=excluded.evaluation_job_id,candidate_digest=excluded.candidate_digest,evaluation_report=excluded.evaluation_report")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(id.as_str()).bind(&workspace).bind(candidate.as_str())
            .bind(&input.name).bind(&request.description).bind(number(revision)?).bind(source).bind(&input.proposed_content)
            .bind(zuno_orchestration::sha256_text(&input.proposed_content))
            .bind(row.try_get::<String,_>("evaluation_job_id").map_err(database_error)?)
            .bind(&request.expected_digest).bind(json!(report))
            .execute(&mut *tx).await.map_err(database_error)?;
        let document = read(&mut tx, actor, &id).await?;
        record(&mut tx, actor, &request.request_id, &hash, &document).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(document.skill)
    }
    async fn activate(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
        request: ActivateSkill,
    ) -> Result<InstalledSkillView, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &actor.owner()).await?;
        access(&mut tx, actor, true).await?;
        lock(&mut tx, actor).await?;
        let hash = zuno_orchestration::sha256_json(&json!(["activate", id, request]));
        if let Some(value) = prior(&mut tx, actor, &request.request_id, &hash).await? {
            return Ok(value);
        }
        let old = read(&mut tx, actor, id).await?;
        if old.skill.revision != request.expected_revision {
            return Err(ApplicationError::Conflict);
        }
        let revision = old.skill.revision.0.checked_add(1).ok_or_else(invalid)?;
        number(revision)?;
        query(
            "UPDATE zuno_enterprise_preview.skill_installation SET active=$4,revision=revision+1,
            source=$5 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
        )
        .bind(actor.tenant_id().as_str())
        .bind(actor.principal_id().as_str())
        .bind(id.as_str())
        .bind(request.active)
        .bind(format!("enterprise-skill://{id}/{revision}"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
        let document = read(&mut tx, actor, id).await?;
        record(&mut tx, actor, &request.request_id, &hash, &document).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(document.skill)
    }
    async fn rollback(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
        request: RollbackSkill,
    ) -> Result<InstalledSkillView, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &actor.owner()).await?;
        access(&mut tx, actor, true).await?;
        lock(&mut tx, actor).await?;
        let hash = zuno_orchestration::sha256_json(&json!(["rollback", id, request]));
        if let Some(value) = prior(&mut tx, actor, &request.request_id, &hash).await? {
            return Ok(value);
        }
        let old = read(&mut tx, actor, id).await?;
        if old.skill.revision != request.expected_revision
            || request.target_revision.0 == 0
            || request.target_revision.0 >= old.skill.revision.0
        {
            return Err(ApplicationError::Conflict);
        }
        let data: Value = query_scalar(
            "SELECT data FROM zuno_enterprise_preview.skill_installation_revision
             WHERE tenant_id=$1 AND principal_id=$2 AND installation_id=$3 AND revision=$4",
        )
        .bind(actor.tenant_id().as_str())
        .bind(actor.principal_id().as_str())
        .bind(id.as_str())
        .bind(number(request.target_revision.0)?)
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?
        .ok_or(ApplicationError::NotFound)?;
        let target: InstalledSkillDocument =
            serde_json::from_value(data["document"].clone()).map_err(ApplicationError::storage)?;
        if target.skill.id != *id
            || target.skill.workspace_id != old.skill.workspace_id
            || target.skill.name != old.skill.name
            || target.skill.revision != request.target_revision
        {
            return Err(ApplicationError::Conflict);
        }
        let revision = old.skill.revision.0.checked_add(1).ok_or_else(invalid)?;
        let evaluation_job = data["evaluationJobId"].as_str().ok_or_else(invalid)?;
        let candidate_digest = data["candidateDigest"].as_str().ok_or_else(invalid)?;
        query(
            "UPDATE zuno_enterprise_preview.skill_installation SET candidate_id=$4,
            description=$5,revision=$6,source=$7,content=$8,content_digest=$9,active=false,
            evaluation_job_id=$10,candidate_digest=$11,evaluation_report=$12
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
        )
        .bind(actor.tenant_id().as_str())
        .bind(actor.principal_id().as_str())
        .bind(id.as_str())
        .bind(target.skill.candidate_id.as_str())
        .bind(&target.skill.description)
        .bind(number(revision)?)
        .bind(format!("enterprise-skill://{id}/{revision}"))
        .bind(&target.content)
        .bind(&target.skill.content_digest)
        .bind(evaluation_job)
        .bind(candidate_digest)
        .bind(&data["evaluationReport"])
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
        // Verify the original completed evaluation before restored content can
        // commit. Rollback never restores an old activation decision.
        let document = read(&mut tx, actor, id).await?;
        record(&mut tx, actor, &request.request_id, &hash, &document).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(document.skill)
    }
    async fn list(
        &self,
        actor: &PrincipalScope,
        workspace: &WorkspaceId,
        after: Option<&RequestId>,
        limit: PageSize,
    ) -> Result<InstalledSkillPage, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &actor.owner()).await?;
        access(&mut tx, actor, false).await?;
        let rows=query("SELECT * FROM zuno_enterprise_preview.skill_installation WHERE tenant_id=$1 AND principal_id=$2 AND workspace_id=$3
            AND ($4::text IS NULL OR id>$4) ORDER BY id LIMIT $5")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(workspace.as_str()).bind(after.map(RequestId::as_str))
            .bind(i64::from(limit.get())+1).fetch_all(&mut *tx).await.map_err(database_error)?;
        let more = rows.len() > usize::from(limit.get());
        let items = rows
            .iter()
            .take(usize::from(limit.get()))
            .map(view)
            .collect::<Result<Vec<_>, _>>()?;
        let after = more
            .then(|| items.last().map(|value| value.id.clone()))
            .flatten();
        tx.commit().await.map_err(database_error)?;
        Ok(InstalledSkillPage { items, after })
    }
    async fn document(
        &self,
        actor: &PrincipalScope,
        id: &RequestId,
    ) -> Result<InstalledSkillDocument, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &actor.owner()).await?;
        access(&mut tx, actor, false).await?;
        let value = read(&mut tx, actor, id).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
}
#[async_trait]
impl SkillExecutionReader for PostgresSkillLibrary {
    async fn active_documents(
        &self,
        lease: &zuno_application::runtime::ExecutionLease,
    ) -> Result<Vec<InstalledSkillDocument>, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &lease.owner).await?;
        let job = crate::runtime::verify_lease(&mut tx, lease).await?;
        access(&mut tx, &job.principal, false).await?;
        let workspace:String=query_scalar("SELECT workspace_id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.session_id.as_str())
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        let ids:Vec<String>=query_scalar("SELECT id FROM zuno_enterprise_preview.skill_installation WHERE tenant_id=$1 AND principal_id=$2 AND workspace_id=$3 AND active ORDER BY id LIMIT 65")
            .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(&workspace)
            .fetch_all(&mut *tx).await.map_err(database_error)?;
        if ids.len() > 64 {
            return Err(ApplicationError::Conflict);
        }
        let mut result = Vec::new();
        for id in ids {
            let value = read(
                &mut tx,
                &job.principal,
                &RequestId::new(id).map_err(ApplicationError::storage)?,
            )
            .await?;
            if value.skill.active {
                result.push(value);
            }
        }
        crate::runtime::verify_lease(&mut tx, lease).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(result)
    }
}
