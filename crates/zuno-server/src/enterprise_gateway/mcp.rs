use super::*;
use serde_json::json;
use zuno_application::{
    authorization::{ApprovalBinding, ApprovalProposal},
    mcp::*,
};
use zuno_permission::enterprise::{EffectKind, IsolationFact, PreparedEffectFacts};

impl GatewayControlService {
    async fn mcp_proposal(
        &self,
        headers: &HeaderMap,
        admission: &McpAdmission,
    ) -> Result<ApprovalProposal, Failure> {
        let gateway = self.gateway(headers).await?;
        admission.validate().map_err(application)?;
        if admission.gateway_id != *gateway.id() || admission.lease.owner.tenant_id != self.tenant {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let job = self
            .backend
            .runtime(self.tenant.clone())
            .get(&admission.lease.owner, &admission.lease.job_id)
            .await
            .map_err(application)?;
        let assignment = self
            .configuration
            .resolve(&self.tenant, &job.configuration, &job.session_id)
            .map_err(application)?;
        if assignment.gateway_id != *gateway.id()
            || assignment.environment.id != admission.operation.environment_id
            || job.session_id != admission.lease.session_id
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        if !self
            .configuration
            .mcp_tools(&self.tenant, &job.configuration)
            .map_err(application)?
            .contains(&admission.operation.binding)
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let proposal = ApprovalProposal {
            binding: ApprovalBinding {
                job_id: job.id,
                session_id: job.session_id,
                turn_id: job.turn_id,
                invocation_id: admission.operation.invocation_id.clone(),
                operation_id: admission.operation.id.clone(),
                arguments_sha256: admission.arguments_digest(),
                resources_sha256: admission.resources_digest(),
                effect: EffectKind::ExternalTool,
            },
            facts: PreparedEffectFacts {
                kind: EffectKind::ExternalTool,
                resource_authorized: true,
                isolation: IsolationFact::NotApplicable,
                builtin_handler: false,
                sensitive: false,
                explicit_deny: false,
                mandatory_human: true,
            },
            presentation: json!({"operationID":admission.operation.id,"kind":"mcp",
                "server":admission.operation.binding.server,"tool":admission.operation.binding.tool,
                "argumentsSHA256":admission.arguments_digest(),"bindingSHA256":admission.resources_digest()}),
        };
        proposal.validate().map_err(application)?;
        Ok(proposal)
    }
}
pub(super) async fn recheck(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(admission): Json<McpAdmission>,
) -> Result<StatusCode, Failure> {
    let proposal = service.mcp_proposal(&headers, &admission).await?;
    service
        .backend
        .organizations(service.tenant.clone())
        .check_admitted_mcp(proposal, &admission)
        .await
        .map_err(application)?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn prepare(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(admission): Json<McpAdmission>,
) -> Result<Json<ApprovalRecord>, Failure> {
    let proposal = service.mcp_proposal(&headers, &admission).await?;
    service
        .backend
        .mcp_operations(admission.gateway_id.clone())
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
    Json(admission): Json<McpAdmission>,
) -> Result<Json<CheckedApproval>, Failure> {
    let proposal = service.mcp_proposal(&headers, &admission).await?;
    Ok(Json(
        service
            .backend
            .organizations(service.tenant.clone())
            .check_mcp_execution(proposal, &admission)
            .await
            .map_err(application)?,
    ))
}
pub(super) async fn complete(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(completion): Json<McpCompletion>,
) -> Result<StatusCode, Failure> {
    let gateway = service.gateway(&headers).await?;
    if completion.admission.lease.owner.tenant_id != service.tenant {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    service
        .backend
        .mcp_operations(gateway.id().clone())
        .complete(&completion)
        .await
        .map_err(application)?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn cancellations(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(limit): Json<u32>,
) -> Result<Json<Vec<McpAdmission>>, Failure> {
    let gateway = service.gateway(&headers).await?;
    Ok(Json(
        service
            .backend
            .mcp_operations(gateway.id().clone())
            .cancellations(&service.tenant, limit)
            .await
            .map_err(application)?,
    ))
}
