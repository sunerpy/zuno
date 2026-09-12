use super::*;
use zuno_application::workspace_import::{
    WorkspaceImportAssignment, WorkspaceInitializationCompletion, WorkspaceUploadRequest,
};
use zuno_identity::gateway::GatewayImportTicket;

impl GatewayControlService {
    async fn import_destination(
        &self,
        gateway: &AuthenticatedGateway,
        assigned: &WorkspaceImportAssignment,
    ) -> Result<(), Failure> {
        if assigned.principal.tenant_id() != &self.tenant || assigned.gateway_id != *gateway.id() {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let expected = self
            .configuration
            .resolve(&self.tenant, &assigned.configuration, &assigned.session_id)
            .map_err(application)?;
        if expected.gateway_id != *gateway.id() || expected.environment != assigned.environment {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        Ok(())
    }
}
pub(super) async fn resolve(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<WorkspaceUploadRequest>,
) -> Result<Json<WorkspaceImportAssignment>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let ticket = GatewayImportTicket::try_from(
        field(&headers, zuno_worker::GATEWAY_IMPORT_TICKET_HEADER)?.to_owned(),
    )
    .map_err(authentication)?;
    let viewer = service
        .tickets
        .verify_import(&gateway, &ticket, &request, now()?)
        .map_err(authentication)?;
    let assigned = service
        .backend
        .import_assignment(&viewer, &request.session_id, &request.import_id)
        .await
        .map_err(application)?;
    service.import_destination(&gateway, &assigned).await?;
    Ok(Json(assigned))
}
pub(super) async fn authorize(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(assigned): Json<WorkspaceImportAssignment>,
) -> Result<Json<Option<zuno_application::workspace_import::WorkspaceImportReceipt>>, Failure> {
    let gateway = service.gateway(&headers).await?;
    service.import_destination(&gateway, &assigned).await?;
    let receipt = service
        .backend
        .authorize_workspace_initialization(gateway.id(), &assigned)
        .await
        .map_err(application)?;
    Ok(Json(receipt))
}
pub(super) async fn complete(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(completion): Json<WorkspaceInitializationCompletion>,
) -> Result<StatusCode, Failure> {
    let gateway = service.gateway(&headers).await?;
    service
        .import_destination(&gateway, &completion.assignment)
        .await?;
    service
        .backend
        .complete_workspace_import(
            gateway.id(),
            &completion.assignment.principal.owner(),
            &completion.assignment.session_id,
            &completion.receipt,
        )
        .await
        .map_err(application)?;
    Ok(StatusCode::NO_CONTENT)
}
