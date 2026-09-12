use super::*;
use zuno_application::workspace_transfer::{
    SnapshotTransferAssignment, SnapshotTransferCompletion, SnapshotTransferContext,
    SnapshotTransferPurpose, SnapshotTransferRequest,
};

impl GatewayControlService {
    async fn transfer_context(
        &self,
        request: &SnapshotTransferRequest,
        gateway: &AuthenticatedGateway,
        source_digest: Option<&str>,
    ) -> Result<SnapshotTransferContext, Failure> {
        let mut context = self.context(&request.lease).await?;
        let existing_source = match &request.purpose {
            SnapshotTransferPurpose::ChildWorkspace { child_job_id } => {
                let command = GatewayRequest::new(GatewayCommand::PrepareChildWorkspace {
                    child_job_id: child_job_id.clone(),
                })
                .map_err(application)?;
                self.resolve_child_context(&command, &mut context).await?;
                if context.child_workspace.as_ref().is_none_or(|a| a.resume) {
                    return Err(Failure(StatusCode::FORBIDDEN));
                }
                context.existing_workspace
            }
            SnapshotTransferPurpose::MergeSource { child_job_id, .. } => {
                self.merge_source_context(child_job_id, &mut context)
                    .await?;
                true
            }
        };
        let assignment = SnapshotTransferAssignment {
            request: request.clone(),
            source: context
                .workspace_source
                .ok_or(Failure(StatusCode::FORBIDDEN))?,
            target_gateway_id: context.assignment.gateway_id,
            existing_source,
        };
        let allowed = match source_digest {
            Some(digest) => {
                assignment.source.gateway_id == *gateway.id() && assignment.digest() == digest
            }
            None => assignment.target_gateway_id == *gateway.id(),
        };
        if !allowed {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let snapshot = self
            .backend
            .admit_snapshot_transfer(&assignment)
            .await
            .map_err(application)?;
        Ok(SnapshotTransferContext {
            assignment,
            snapshot,
        })
    }
}

pub(super) async fn ticket(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<SnapshotTransferRequest>,
) -> Result<Json<zuno_worker::IssuedSnapshotTransfer>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let context = service.transfer_context(&request, &gateway, None).await?;
    let ticket = service
        .tickets
        .issue_snapshot(&context.assignment, now()?)
        .map_err(authentication)?;
    Ok(Json(zuno_worker::IssuedSnapshotTransfer {
        context,
        ticket,
    }))
}

pub(super) async fn resolve(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<SnapshotTransferRequest>,
) -> Result<Json<SnapshotTransferContext>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let ticket = zuno_identity::gateway::GatewaySnapshotTicket::try_from(
        field(&headers, zuno_worker::GATEWAY_SNAPSHOT_TICKET_HEADER)?.to_owned(),
    )
    .map_err(authentication)?;
    let digest = service
        .tickets
        .verify_snapshot(&gateway, &ticket, &request, now()?)
        .map_err(authentication)?;
    let context = service
        .transfer_context(&request, &gateway, Some(&digest))
        .await?;
    Ok(Json(context))
}

pub(super) async fn complete(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(completion): Json<SnapshotTransferCompletion>,
) -> Result<StatusCode, Failure> {
    let gateway = service.gateway(&headers).await?;
    if completion.assignment.request.lease.owner.tenant_id != service.tenant {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    service
        .backend
        .complete_snapshot_transfer(gateway.id(), &completion)
        .await
        .map_err(application)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn fact(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<SnapshotTransferRequest>,
) -> Result<Json<SnapshotTransferContext>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let context = service.transfer_context(&request, &gateway, None).await?;
    Ok(Json(context))
}
