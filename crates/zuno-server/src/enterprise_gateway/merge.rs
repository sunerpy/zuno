use super::*;
use zuno_application::{
    authorization::{ApprovalBinding, ApprovalProposal},
    workspace_merge::{
        GatewayMergeRequest, WorkspaceEntry, WorkspaceMergeAdmission, WorkspaceMergeCompletion,
    },
};
use zuno_permission::enterprise::{EffectKind, IsolationFact, PreparedEffectFacts};

impl GatewayControlService {
    pub(super) async fn merge_context(
        &self,
        request: &GatewayRequest,
        context: &mut GatewayExecutionContext,
    ) -> Result<(), Failure> {
        let child = match &request.command {
            GatewayCommand::PreviewWorkspaceMerge { child_job_id, .. } => child_job_id,
            GatewayCommand::PrepareWorkspaceMerge { operation }
            | GatewayCommand::SubmitWorkspaceMerge { operation } => &operation.child_job_id,
            _ => return Ok(()),
        };
        self.merge_source_context(child, context).await
    }
    pub(super) async fn merge_source_context(
        &self,
        child: &zuno_types::identity::JobId,
        context: &mut GatewayExecutionContext,
    ) -> Result<(), Failure> {
        let source = self
            .backend
            .runtime(self.tenant.clone())
            .workspace_merge_source(&context.lease, child)
            .await
            .map_err(application)?;
        let configured = self
            .configuration
            .resolve(
                &self.tenant,
                &source.child_configuration,
                &source.child_session_id,
            )
            .map_err(application)?;
        if configured.gateway_id != source.gateway_id
            || configured.environment != source.source_environment
            || source.base.environment_id != context.assignment.environment.id
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        context.workspace_source = Some(configured);
        context.merge_source = Some(source);
        Ok(())
    }
    async fn merge_admission(
        &self,
        gateway: &AuthenticatedGateway,
        request: GatewayMergeRequest,
    ) -> Result<WorkspaceMergeAdmission, Failure> {
        let mut context = self.context(&request.lease).await?;
        if context.assignment.gateway_id != *gateway.id()
            || context.assignment.environment != request.environment.spec
            || request.environment.owner != request.lease.owner
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let command = GatewayRequest::new(GatewayCommand::PrepareWorkspaceMerge {
            operation: Box::new(request.operation.clone()),
        })
        .map_err(application)?;
        self.merge_context(&command, &mut context).await?;
        let admission = WorkspaceMergeAdmission {
            gateway_id: gateway.id().clone(),
            lease: request.lease,
            environment: request.environment,
            source: context.merge_source.ok_or(Failure(StatusCode::FORBIDDEN))?,
            operation: request.operation,
        };
        admission.operation.validate().map_err(application)?;
        Ok(admission)
    }
    async fn merge_proposal(
        &self,
        admission: &WorkspaceMergeAdmission,
    ) -> Result<ApprovalProposal, Failure> {
        let job = self
            .backend
            .runtime(self.tenant.clone())
            .get(&admission.lease.owner, &admission.lease.job_id)
            .await
            .map_err(application)?;
        let sensitive = admission.operation.plan.changes.iter().any(|change| {
            let attributes = |entry: &Option<WorkspaceEntry>| match entry {
                Some(
                    WorkspaceEntry::File { mode, uid, gid, .. }
                    | WorkspaceEntry::Directory { mode, uid, gid },
                ) => Some((*mode, *uid, *gid)),
                Some(WorkspaceEntry::Symlink { uid, gid, .. }) => Some((0o777, *uid, *gid)),
                _ => None,
            };
            matches!((attributes(&change.parent),attributes(&change.child)),(Some(parent),Some(child)) if parent!=child)
                || attributes(&change.child).is_some_and(|(_,uid,gid)|uid!=0||gid!=0)
                || change.path.as_str() == "."
                || change.path.as_str().starts_with(".git/")
        });
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
                sensitive,
                explicit_deny: false,
                mandatory_human: true,
            },
            presentation: serde_json::json!({"kind":"workspace_merge","operationId":admission.operation.id,
                "childJobId":admission.operation.child_job_id,"planDigest":admission.operation.plan.digest(),
                "changeCount":admission.operation.plan.changes.len()}),
        };
        proposal.validate().map_err(application)?;
        Ok(proposal)
    }
}

pub(super) async fn prepare(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<GatewayMergeRequest>,
) -> Result<Json<ApprovalRecord>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let admission = service.merge_admission(&gateway, request).await?;
    service
        .backend
        .workspace_merges(gateway.id().clone())
        .offer(&admission)
        .await
        .map_err(application)?;
    let proposal = service.merge_proposal(&admission).await?;
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
    Json(request): Json<GatewayMergeRequest>,
) -> Result<Json<CheckedApproval>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let admission = service.merge_admission(&gateway, request).await?;
    let proposal = service.merge_proposal(&admission).await?;
    Ok(Json(
        service
            .backend
            .organizations(service.tenant.clone())
            .check_workspace_merge_execution(proposal, &admission)
            .await
            .map_err(application)?,
    ))
}
pub(super) async fn complete(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(completion): Json<WorkspaceMergeCompletion>,
) -> Result<StatusCode, Failure> {
    let gateway = service.gateway(&headers).await?;
    if completion.lease.owner.tenant_id != service.tenant {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    service
        .backend
        .workspace_merges(gateway.id().clone())
        .complete(&completion)
        .await
        .map_err(application)?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn cancellations(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(limit): Json<u32>,
) -> Result<Json<Vec<WorkspaceMergeAdmission>>, Failure> {
    let gateway = service.gateway(&headers).await?;
    Ok(Json(
        service
            .backend
            .workspace_merges(gateway.id().clone())
            .cancellations(&service.tenant, limit)
            .await
            .map_err(application)?,
    ))
}

pub(super) async fn read_context(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<zuno_application::workspace_merge::MergeContentRequest>,
) -> Result<Json<zuno_application::workspace_merge::MergeContentContext>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let ticket = zuno_identity::gateway::GatewayReadTicket::try_from(
        field(&headers, zuno_worker::GATEWAY_READ_TICKET_HEADER)?.to_owned(),
    )
    .map_err(authentication)?;
    let viewer = service
        .tickets
        .verify_read(&gateway, &ticket, &request, now()?)
        .map_err(authentication)?;
    let (assigned, context) = service
        .backend
        .workspace_merge_content(&viewer, &request)
        .await
        .map_err(application)?;
    if assigned != *gateway.id() {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    Ok(Json(context))
}
