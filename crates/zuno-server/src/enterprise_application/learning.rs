use super::*;
use zuno_application::learning_api::{
    CancelLearning, LearningCancellation, LearningCursor, LearningJobView, LearningPage,
    LearningPageRequest, LearningStage, LearningState,
};
use zuno_types::activity::Counter;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Page {
    before_created_at_ms: Option<Counter>,
    before_job_id: Option<JobId>,
    stage: Option<LearningStage>,
    state: Option<LearningState>,
    #[serde(default)]
    limit: PageSize,
}
pub(super) async fn list(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(workspace): Path<WorkspaceId>,
    Query(query): Query<Page>,
) -> Result<Json<LearningPage>, Failure> {
    let principal = service.principal(&identity).await?;
    if !service.workspaces.contains_key(&workspace) {
        return Err(Failure(StatusCode::NOT_FOUND));
    }
    let before = match (query.before_created_at_ms, query.before_job_id) {
        (None, None) => None,
        (Some(created_at_ms), Some(job_id)) => Some(LearningCursor {
            created_at_ms,
            job_id,
        }),
        _ => return Err(Failure(StatusCode::BAD_REQUEST)),
    };
    Ok(Json(
        service
            .backend
            .client_learning_jobs(
                &principal,
                &workspace,
                LearningPageRequest {
                    before,
                    stage: query.stage,
                    state: query.state,
                    limit: query.limit,
                },
            )
            .await?,
    ))
}
pub(super) async fn get(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(job): Path<JobId>,
) -> Result<Json<LearningJobView>, Failure> {
    let principal = service.principal(&identity).await?;
    let result = service
        .backend
        .client_learning_job(&principal, &job)
        .await?;
    if !service.workspaces.contains_key(&result.workspace_id) {
        return Err(Failure(StatusCode::NOT_FOUND));
    }
    Ok(Json(result))
}
pub(super) async fn cancel(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(job): Path<JobId>,
    Json(request): Json<CancelLearning>,
) -> Result<Json<LearningCancellation>, Failure> {
    let principal = service.principal(&identity).await?;
    let current = service
        .backend
        .client_learning_job(&principal, &job)
        .await?;
    if !service.workspaces.contains_key(&current.workspace_id) {
        return Err(Failure(StatusCode::NOT_FOUND));
    }
    Ok(Json(
        service
            .backend
            .cancel_learning_job(&principal, &job, request)
            .await?,
    ))
}
