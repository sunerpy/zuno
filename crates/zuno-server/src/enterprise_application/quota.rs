use super::*;
use zuno_application::quota::*;

pub(super) async fn get(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
) -> Result<Json<QuotaSnapshot>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(service.backend.quotas().snapshot(&principal).await?))
}
pub(super) async fn replace(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Json(request): Json<ReplaceQuotaPolicy>,
) -> Result<Json<QuotaPolicy>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .quotas()
            .replace(&principal, request)
            .await?,
    ))
}
