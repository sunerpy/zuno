use super::*;
use zuno_application::skill::*;
use zuno_types::identity::RequestId;

pub(super) async fn propose(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(source): Path<JobId>,
    Json(request): Json<ProposeSkill>,
) -> Result<Json<SkillCandidateView>, Failure> {
    let actor = service.principal(&identity).await?;
    let skills = service.skills.as_ref().ok_or(ApplicationError::NotFound)?;
    Ok(Json(skills.propose(&actor, &source, request).await?))
}
pub(super) async fn get(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<RequestId>,
) -> Result<Json<SkillCandidateView>, Failure> {
    let actor = service.principal(&identity).await?;
    let skills = service.skills.as_ref().ok_or(ApplicationError::NotFound)?;
    Ok(Json(skills.candidate(&actor, &id).await?))
}
pub(super) async fn evaluate(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<RequestId>,
    Json(request): Json<ReviewSkillEvaluation>,
) -> Result<Json<SkillCandidateView>, Failure> {
    let actor = service.principal(&identity).await?;
    let skills = service.skills.as_ref().ok_or(ApplicationError::NotFound)?;
    Ok(Json(skills.evaluate(&actor, &id, request).await?))
}
pub(super) async fn install(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<RequestId>,
    Json(request): Json<InstallSkill>,
) -> Result<Json<InstalledSkillView>, Failure> {
    let actor = service.principal(&identity).await?;
    Ok(Json(
        service
            .skill_library
            .as_ref()
            .ok_or(ApplicationError::NotFound)?
            .install(&actor, &id, request)
            .await?,
    ))
}
pub(super) async fn activate(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<RequestId>,
    Json(request): Json<ActivateSkill>,
) -> Result<Json<InstalledSkillView>, Failure> {
    let actor = service.principal(&identity).await?;
    Ok(Json(
        service
            .skill_library
            .as_ref()
            .ok_or(ApplicationError::NotFound)?
            .activate(&actor, &id, request)
            .await?,
    ))
}
pub(super) async fn document(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<RequestId>,
) -> Result<Json<InstalledSkillDocument>, Failure> {
    let actor = service.principal(&identity).await?;
    Ok(Json(
        service
            .skill_library
            .as_ref()
            .ok_or(ApplicationError::NotFound)?
            .document(&actor, &id)
            .await?,
    ))
}
pub(super) async fn rollback(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<RequestId>,
    Json(request): Json<RollbackSkill>,
) -> Result<Json<InstalledSkillView>, Failure> {
    let actor = service.principal(&identity).await?;
    Ok(Json(
        service
            .skill_library
            .as_ref()
            .ok_or(ApplicationError::NotFound)?
            .rollback(&actor, &id, request)
            .await?,
    ))
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct InstalledQuery {
    after: Option<RequestId>,
    #[serde(default)]
    limit: PageSize,
}
pub(super) async fn installed(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(workspace): Path<WorkspaceId>,
    Query(query): Query<InstalledQuery>,
) -> Result<Json<InstalledSkillPage>, Failure> {
    let actor = service.principal(&identity).await?;
    if !service.workspaces.contains_key(&workspace) {
        return Err(ApplicationError::NotFound.into());
    }
    Ok(Json(
        service
            .skill_library
            .as_ref()
            .ok_or(ApplicationError::NotFound)?
            .list(&actor, &workspace, query.after.as_ref(), query.limit)
            .await?,
    ))
}
