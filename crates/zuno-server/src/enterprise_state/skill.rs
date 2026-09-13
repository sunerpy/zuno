use super::*;
use zuno_application::skill::*;
use zuno_catalog::skill::{Form, Skill, Skills};
use zuno_tool::Tool as _;

pub(super) async fn call(
    State(service): State<WorkerStateService>,
    Extension(worker): Extension<AuthenticatedWorker>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<SkillExecutionReply>, ApiFailure> {
    if bytes.len() > 65536 {
        return Err(ApiFailure(StatusCode::PAYLOAD_TOO_LARGE));
    }
    let grant = service.grant(&worker, &headers)?;
    let job = service
        .backend
        .runtime(service.tenant.clone())
        .get(&grant.lease().owner, &grant.lease().job_id)
        .await
        .map_err(child_error)?;
    if !service
        .skill_configurations
        .as_ref()
        .is_some_and(|allowed| allowed.contains(&job.configuration))
    {
        return Err(ApiFailure(StatusCode::FORBIDDEN));
    }
    let request: SkillExecutionRequest =
        serde_json::from_slice(&bytes).map_err(|_| ApiFailure(StatusCode::BAD_REQUEST))?;
    let reader = service
        .skills
        .as_ref()
        .ok_or(ApiFailure(StatusCode::NOT_FOUND))?;
    let documents = reader
        .active_documents(grant.lease())
        .await
        .map_err(child_error)?;
    let digest = zuno_orchestration::sha256_json(&serde_json::json!(&documents));
    let skills = Arc::new(Skills::from_loaded(documents.into_iter().map(|document| {
        Skill::embedded(
            document.skill.name,
            Some(document.skill.description),
            document.skill.source,
            document.content,
        )
    })));
    let reply = match request {
        SkillExecutionRequest::Catalog => {
            let index = skills.render_within(Form::Index, 8192).text;
            SkillExecutionReply::Catalog {
                revision_digest: digest.clone(),
                index,
            }
        }
        SkillExecutionRequest::Invoke {
            invocation_id,
            arguments,
        } => {
            let tool = zuno_tool::Typed(zuno_tools::SkillTool::new(skills));
            let context = zuno_tool::ToolContext::new_scoped(
                job.session_id.to_string(),
                job.input_id.to_string(),
                invocation_id.to_string(),
                "enterprise",
                Arc::new(zuno_tool::AllowAll),
                Arc::new(zuno_tool::NeverInterrupted),
                job.principal.clone(),
            );
            match tool.invoke(arguments, context).await {
                Ok(output) => SkillExecutionReply::Output {
                    output: Box::new(output),
                    is_error: false,
                },
                Err(error) => SkillExecutionReply::Output {
                    output: Box::new(zuno_tool::ToolOutput::text("Skill", error.to_string())),
                    is_error: true,
                },
            }
        }
    };
    // A concurrent deactivation/version replacement invalidates this response
    // before it becomes model-visible, even if its content was already loaded.
    let current = reader
        .active_documents(grant.lease())
        .await
        .map_err(child_error)?;
    if zuno_orchestration::sha256_json(&serde_json::json!(current)) != digest {
        return Err(ApiFailure(StatusCode::CONFLICT));
    }
    Ok(Json(reply))
}
