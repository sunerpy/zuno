use super::*;
use serde_json::json;
use zuno_application::{
    authorization::{ApprovalBinding, ApprovalProposal},
    workspace_edit::*,
};
use zuno_permission::enterprise::{EffectKind, IsolationFact, PreparedEffectFacts};

impl GatewayControlService {
    async fn edit_proposal(
        &self,
        headers: &HeaderMap,
        admission: &WorkspaceEditAdmission,
    ) -> Result<ApprovalProposal, Failure> {
        let gateway = self.gateway(headers).await?;
        let context = self.context(&admission.lease).await?;
        admission.validate().map_err(application)?;
        if admission.gateway_id != *gateway.id()
            || context.assignment.gateway_id != *gateway.id()
            || context.assignment.environment != admission.environment.spec
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let job = self
            .backend
            .runtime(self.tenant.clone())
            .get(&admission.lease.owner, &admission.lease.job_id)
            .await
            .map_err(application)?;
        let proposal = ApprovalProposal {
            binding: ApprovalBinding {
                job_id: job.id,
                session_id: job.session_id,
                turn_id: job.turn_id,
                invocation_id: admission.operation.invocation_id.clone(),
                operation_id: admission.operation.id.clone(),
                arguments_sha256: admission.arguments_digest(),
                resources_sha256: admission.resources_digest(),
                effect: EffectKind::FileWrite,
            },
            facts: PreparedEffectFacts {
                kind: EffectKind::FileWrite,
                resource_authorized: true,
                isolation: IsolationFact::Enforced,
                builtin_handler: true,
                sensitive: admission
                    .operation
                    .edits
                    .iter()
                    .any(|edit| edit.path.as_str().starts_with(".git/")),
                explicit_deny: false,
                mandatory_human: true,
            },
            presentation: json!({"operationID":admission.operation.id,"environmentID":admission.environment.spec.id,
                "kind":"workspace_edit","changeCount":admission.review.len()}),
        };
        proposal.validate().map_err(application)?;
        Ok(proposal)
    }
}
pub(super) async fn prepare(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(admission): Json<WorkspaceEditAdmission>,
) -> Result<Json<ApprovalRecord>, Failure> {
    let proposal = service.edit_proposal(&headers, &admission).await?;
    service
        .backend
        .workspace_edits(admission.gateway_id.clone())
        .offer(&admission)
        .await
        .map_err(application)?;
    Ok(Json(
        service
            .backend
            .organizations(service.tenant.clone())
            .admit(&admission.lease, proposal)
            .await
            .map_err(application)?,
    ))
}
pub(super) async fn authorize(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(admission): Json<WorkspaceEditAdmission>,
) -> Result<Json<CheckedApproval>, Failure> {
    let proposal = service.edit_proposal(&headers, &admission).await?;
    Ok(Json(
        service
            .backend
            .organizations(service.tenant.clone())
            .check_workspace_edit_execution(proposal, &admission)
            .await
            .map_err(application)?,
    ))
}
pub(super) async fn complete(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(completion): Json<WorkspaceEditCompletion>,
) -> Result<StatusCode, Failure> {
    let gateway = service.gateway(&headers).await?;
    if completion.admission.lease.owner.tenant_id != service.tenant {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    service
        .backend
        .workspace_edits(gateway.id().clone())
        .complete(&completion)
        .await
        .map_err(application)?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn cancellations(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(limit): Json<u32>,
) -> Result<Json<Vec<WorkspaceEditAdmission>>, Failure> {
    let gateway = service.gateway(&headers).await?;
    Ok(Json(
        service
            .backend
            .workspace_edits(gateway.id().clone())
            .cancellations(&service.tenant, limit)
            .await
            .map_err(application)?,
    ))
}
