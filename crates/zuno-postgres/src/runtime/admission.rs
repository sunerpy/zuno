//! Shared root admission for authenticated user input and native completion delivery.
use super::*;

pub(crate) fn request_key(
    principal: &PrincipalScope,
    session: &SessionId,
    request: &zuno_types::identity::RequestId,
) -> String {
    zuno_orchestration::sha256_json(&json!([
        "root-job",
        principal.owner(),
        principal.client_id(),
        session,
        request
    ]))
}

pub(super) async fn root_in(
    tx: &mut Transaction<'_, Postgres>,
    principal: &PrincipalScope,
    request: JobSubmission,
    completion: Option<&zuno_types::execution::CompletionEnvelope>,
) -> Result<RuntimeJob, ApplicationError> {
    request.configuration.validate()?;
    if let Some(selection) = &request.selection {
        selection.validate()?;
    }
    if request.text.trim().is_empty()
        || request.text.len() > zuno_application::MAX_INPUT_BYTES
        || request.text.contains('\0')
    {
        return Err(ApplicationError::Invalid(
            "invalid runtime input".to_owned(),
        ));
    }
    let expected = integer(request.expected_input_version)?;
    // Every session/input writer acquires this row before runtime state.
    let session = read_session(tx, principal, request.session_id.as_str(), true).await?;
    let coordinator: bool = query_scalar("SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_workflow w
        JOIN zuno_enterprise_preview.runtime_child c ON c.tenant_id=w.tenant_id AND c.principal_id=w.principal_id AND c.job_id=w.job_id
        WHERE w.tenant_id=$1 AND w.principal_id=$2 AND c.child_session_id=$3)")
        .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(request.session_id.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    if coordinator {
        return Err(ApplicationError::Forbidden);
    }
    let owner = principal.owner();
    let key = request_key(principal, &request.session_id, &request.request_id);
    let id = format!("job_{key}");
    let digest = zuno_orchestration::sha256_json(&match completion {
        Some(envelope) => json!([request, envelope]),
        None => json!(request),
    });
    let existing: Option<String> = query_scalar(
        "SELECT request_digest FROM zuno_enterprise_preview.runtime_job WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id)
        .fetch_optional(&mut **tx).await.map_err(database_error)?;
    if let Some(existing) = existing {
        if existing != digest {
            return Err(ApplicationError::Conflict);
        }
        let job = read_job(tx, &owner, &id).await?;
        return Ok(job);
    }
    if input_version(tx, &owner, request.session_id.as_str()).await? != expected {
        return Err(ApplicationError::Conflict);
    }
    crate::workspace_import::require_initialized(
        tx,
        principal,
        &request.session_id,
        &request.configuration,
    )
    .await?;
    let time = database_time(tx).await?;
    let turn = format!("turn_{key}");
    let input = format!("msg_{key}");
    let (agent, model) = match &request.selection {
        Some(selection) => (
            Some(selection.agent.clone()),
            Some(
                json!({"providerID":selection.model.provider_id,"modelID":selection.model.model_id}),
            ),
        ),
        None => (
            session
                .try_get::<Option<String>, _>("agent")
                .map_err(database_error)?,
            session
                .try_get::<Option<Value>, _>("model")
                .map_err(database_error)?,
        ),
    };
    let mut prompt = json!({"kind":"user","prompt":{"text":request.text,"files":[],"agents":[]},"agent":agent,"model":model});
    if let Some(completion) = completion {
        prompt["kind"] = json!("completion");
        prompt["completion"] = json!(completion);
    }
    let admitted = emit(tx,principal,request.session_id.as_str(),"session.input.admitted",json!({
        "inputID":input,"prompt":prompt,"delivery":"queue","state":"queued","triggerKind":if completion.is_some() {"automatic"} else {"user"},"timeCreated":time,
    })).await?;
    query(
        "INSERT INTO zuno_enterprise_preview.input(
           tenant_id,principal_id,session_id,id,request_key,prompt,state,admitted_sequence,time_created)
         VALUES($1,$2,$3,$4,$5,$6,'queued',$7,$8)",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(request.session_id.as_str())
        .bind(&input).bind(format!("runtime:{key}")).bind(prompt).bind(admitted).bind(time)
        .execute(&mut **tx).await.map_err(database_error)?;
    let created = emit(tx,principal,request.session_id.as_str(),"agent.job.created",json!({
        "jobID":id,"subject":{"kind":"rootTurn","turnID":turn},"status":"queued","reportDelivery":"quiet",
    })).await?;
    query(
        "INSERT INTO zuno_enterprise_preview.agent_job(
           tenant_id,principal_id,id,parent_session_id,subject_kind,subject_payload,status,report_delivery,created_seq,time_created,time_updated)
         VALUES($1,$2,$3,$4,'root-turn',$5,'queued','quiet',$6,$7,$7)",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id)
        .bind(request.session_id.as_str()).bind(json!({"kind":"rootTurn","turnID":turn})).bind(created).bind(time)
        .execute(&mut **tx).await.map_err(database_error)?;
    let version = input_version(tx, &owner, request.session_id.as_str()).await?;
    query(
        "INSERT INTO zuno_enterprise_preview.runtime_job(
           tenant_id,principal_id,job_id,session_id,turn_id,input_id,request_digest,principal,configuration,
           phase,input_version,ready_at,time_created,time_updated)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,'ready',$10,$11,$11,$11)",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id).bind(request.session_id.as_str())
        .bind(&turn).bind(&input).bind(digest).bind(json!(principal)).bind(json!(request.configuration)).bind(version).bind(time)
        .execute(&mut **tx).await.map_err(database_error)?;
    query("INSERT INTO zuno_enterprise_preview.runtime_owner_schedule(tenant_id,principal_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str())
        .execute(&mut **tx).await.map_err(database_error)?;
    if let Some(envelope) = completion {
        let child = envelope
            .payload
            .get("jobId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ApplicationError::Invalid("completion lacks child identity".to_owned())
            })?;
        let changed = query(
            "INSERT INTO zuno_enterprise_preview.runtime_continuation(tenant_id,principal_id,job_id,parent_job_id,source_child_id)
             SELECT tenant_id,principal_id,$3,parent_job_id,job_id FROM zuno_enterprise_preview.runtime_child
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$4 AND parent_session_id=$5",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&id).bind(child)
            .bind(request.session_id.as_str()).execute(&mut **tx).await.map_err(database_error)?.rows_affected();
        if changed != 1 {
            return Err(ApplicationError::Conflict);
        }
    }
    emit(tx,principal,request.session_id.as_str(),"runtime.job.admitted",json!({
        "jobID":id,"turnID":turn,"inputID":input,"inputVersion":version,"configuration":request.configuration,"principal":principal,
    })).await?;
    let job = read_job(tx, &owner, &id).await?;
    Ok(job)
}
