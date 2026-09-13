use super::*;
use zuno_application::{authorization::ApprovalProposal, workspace_files::GatewayFileRequest};

impl GatewayControlService {
    async fn file_proposal(
        &self,
        headers: &HeaderMap,
        request: &GatewayFileRequest,
    ) -> Result<ApprovalProposal, Failure> {
        let gateway = self.gateway(headers).await?;
        let context = self.context(&request.lease).await?;
        if context.assignment.gateway_id != *gateway.id()
            || context.assignment.environment != request.environment.spec
            || request.environment.owner != request.lease.owner
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        OrganizationOperationAuthority::new(
            Arc::new(self.backend.runtime(self.tenant.clone())),
            Arc::new(self.backend.organizations(self.tenant.clone())),
        )
        .file_proposal(&request.lease, &request.environment, &request.operation)
        .await
        .map_err(application)
    }
}

pub(super) async fn prepare(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<GatewayFileRequest>,
) -> Result<Json<ApprovalRecord>, Failure> {
    let proposal = service.file_proposal(&headers, &request).await?;
    Ok(Json(
        service
            .backend
            .organizations(service.tenant.clone())
            .admit(&request.lease, proposal)
            .await
            .map_err(application)?,
    ))
}

pub(super) async fn authorize(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<GatewayFileRequest>,
) -> Result<Json<CheckedApproval>, Failure> {
    let proposal = service.file_proposal(&headers, &request).await?;
    Ok(Json(
        service
            .backend
            .organizations(service.tenant.clone())
            .check_execution(&request.lease, proposal)
            .await
            .map_err(application)?,
    ))
}
