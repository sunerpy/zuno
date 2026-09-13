use super::*;
use serde::Deserialize;
use zuno_application::shared_memory::*;
use zuno_types::identity::{MemorySpaceId, RequestId};

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ListQuery {
    after: Option<MemorySpaceId>,
    #[serde(default)]
    limit: zuno_application::PageSize,
}
pub(super) async fn list(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(workspace): Path<WorkspaceId>,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
) -> Result<Json<SharedMemoryPage>, Failure> {
    let principal = service.principal(&identity).await?;
    if !service.workspaces.contains_key(&workspace) {
        return Err(ApplicationError::NotFound.into());
    }
    Ok(Json(
        service
            .backend
            .shared_memory()
            .list(&principal, &workspace, query.after.as_ref(), query.limit)
            .await?,
    ))
}
pub(super) async fn get(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<MemorySpaceId>,
) -> Result<Json<SharedMemorySpace>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .shared_memory()
            .read(&principal, &id)
            .await?,
    ))
}
pub(super) async fn configure(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<MemorySpaceId>,
    Json(request): Json<ConfigureSharedMemory>,
) -> Result<Json<SharedMemorySpace>, Failure> {
    let principal = service.principal(&identity).await?;
    if !service.workspaces.contains_key(&request.workspace_id) {
        return Err(ApplicationError::NotFound.into());
    }
    Ok(Json(
        service
            .backend
            .shared_memory()
            .configure(&principal, &id, request)
            .await?,
    ))
}
pub(super) async fn propose(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<MemorySpaceId>,
    Json(request): Json<ProposeSharedMemory>,
) -> Result<Json<SharedMemoryChange>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .shared_memory()
            .propose(&principal, &id, request)
            .await?,
    ))
}
pub(super) async fn change(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path((id, change)): Path<(MemorySpaceId, RequestId)>,
) -> Result<Json<SharedMemoryChange>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .shared_memory()
            .change(&principal, &id, &change)
            .await?,
    ))
}
pub(super) async fn review(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(id): Path<MemorySpaceId>,
    Json(request): Json<ReviewSharedMemory>,
) -> Result<Json<SharedMemoryChange>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .shared_memory()
            .review(&principal, &id, request)
            .await?,
    ))
}
