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
