//! Frozen dependency results and exact admitted node inputs.
use super::*;

pub(super) async fn resolve_input(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
    index: usize,
    child: &JobId,
    base: &str,
) -> Result<(), ApplicationError> {
    let owner = coordinator.principal.owner();
    let mut dependencies = Vec::new();
    let mut sources = Vec::new();
    for dependency in &run.plan.template.nodes[index].depends_on {
        let row = query("SELECT child_job_id,result,result_digest FROM zuno_enterprise_preview.runtime_workflow_node
            WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3 AND node_id=$4 AND state='completed'")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str()).bind(dependency)
            .fetch_one(&mut **tx).await.map_err(database_error)?;
        let result: Value = row.try_get("result").map_err(database_error)?;
        let digest: String = row.try_get("result_digest").map_err(database_error)?;
        if zuno_orchestration::sha256_json(&result) != digest {
            return Err(ApplicationError::Conflict);
        }
        let job: String = row.try_get("child_job_id").map_err(database_error)?;
        dependencies.push(zuno_engine::workflow::DependencyOutput {
            node_id: dependency.clone(),
            job_id: Some(job.clone()),
            output: result
                .pointer("/payload/text")
                .and_then(Value::as_str)
                .ok_or(ApplicationError::Conflict)?
                .to_owned(),
        });
        sources.push(json!({"nodeId":dependency,"jobId":job,"completionDigest":digest}));
    }
    let prompt = zuno_engine::workflow::dependency_prompt(base, &dependencies)
        .map_err(ApplicationError::storage)?;
    if prompt.len() > zuno_application::MAX_INPUT_BYTES {
        return Err(invalid("workflow node input exceeds its bound"));
    }
    let sources = json!(sources);
    let digest = zuno_orchestration::sha256_json(&json!([run.id, child, prompt, sources]));
    let changed = query("UPDATE zuno_enterprise_preview.runtime_workflow_node SET input_prompt=$4,input_digest=$5,input_sources=$6
        WHERE tenant_id=$1 AND principal_id=$2 AND child_job_id=$3 AND state='pending' AND input_prompt IS NULL")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(child.as_str())
        .bind(prompt).bind(digest).bind(sources).execute(&mut **tx).await.map_err(database_error)?.rows_affected();
    if changed != 1 {
        return Err(ApplicationError::Conflict);
    }
    Ok(())
}

pub(in crate::runtime) async fn resolved_input(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    child: &JobId,
) -> Result<Option<String>, ApplicationError> {
    let row = query("SELECT run_id,input_prompt,input_digest,input_sources FROM zuno_enterprise_preview.runtime_workflow_node
        WHERE tenant_id=$1 AND principal_id=$2 AND child_job_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(child.as_str())
        .fetch_optional(&mut **tx).await.map_err(database_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let prompt: String = row.try_get("input_prompt").map_err(database_error)?;
    let run: String = row.try_get("run_id").map_err(database_error)?;
    let sources: Value = row.try_get("input_sources").map_err(database_error)?;
    let digest: String = row.try_get("input_digest").map_err(database_error)?;
    if zuno_orchestration::sha256_json(&json!([run, child, prompt, sources])) != digest {
        return Err(invalid("workflow input digest changed"));
    }
    Ok(Some(prompt))
}
