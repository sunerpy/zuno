//! Data-owner endpoints for gateway delegation and current operation approval.
mod merge;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use std::sync::Arc;
use zuno_application::{
    ApplicationError,
    authorization::{ApprovalRecord, CheckedApproval, OrganizationStore},
    environment::wire::{
        GatewayCommand, GatewayExecutionContext, GatewayOperationRequest, GatewayRequest,
        MAX_GATEWAY_FRAME_BYTES,
    },
    runtime::{ExecutionLease, RuntimeStore},
};
use zuno_engine::state::{TurnPersistence, TurnStateScope};
use zuno_environment::OrganizationOperationAuthority;
use zuno_identity::{
    gateway::{
        AuthenticatedGateway, GatewayServiceAuthority, GatewayTicket, GatewayTicketAuthority,
    },
    worker::{JobGrantAuthority, JobGrantToken, WorkerAuthority},
};
use zuno_postgres::PostgresBackend;
use zuno_types::identity::TenantId;
use zuno_worker::{
    GATEWAY_AUTHORIZE_PATH, GATEWAY_CANCELLATIONS_PATH, GATEWAY_CHILD_WORKSPACE_PATH,
    GATEWAY_COMPLETION_PATH, GATEWAY_PREPARE_PATH, GATEWAY_RESOLVE_PATH, GATEWAY_TICKET_HEADER,
    GATEWAY_TICKET_PATH, GRANT_HEADER, IssuedGatewayRequest,
};

use crate::gateway_configuration::GatewayConfigurationResolver;

#[derive(Clone)]
pub struct GatewayControlService {
    backend: PostgresBackend,
    tenant: TenantId,
    workers: Arc<WorkerAuthority>,
    grants: Arc<JobGrantAuthority>,
    gateways: Arc<GatewayServiceAuthority>,
    tickets: Arc<GatewayTicketAuthority>,
    configuration: Arc<dyn GatewayConfigurationResolver>,
}
impl GatewayControlService {
    pub fn new(
        backend: PostgresBackend,
        tenant: TenantId,
        workers: Arc<WorkerAuthority>,
        grants: Arc<JobGrantAuthority>,
        gateways: Arc<GatewayServiceAuthority>,
        tickets: Arc<GatewayTicketAuthority>,
        configuration: Arc<dyn GatewayConfigurationResolver>,
    ) -> Self {
        Self {
            backend,
            tenant,
            workers,
            grants,
            gateways,
            tickets,
            configuration,
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route(&format!("/{GATEWAY_TICKET_PATH}"), post(issue_ticket))
            .route(&format!("/{GATEWAY_RESOLVE_PATH}"), post(resolve))
            .route(&format!("/{GATEWAY_PREPARE_PATH}"), post(prepare))
            .route(&format!("/{GATEWAY_AUTHORIZE_PATH}"), post(authorize))
            .route(&format!("/{GATEWAY_COMPLETION_PATH}"), post(completion))
            .route(
                &format!("/{GATEWAY_CANCELLATIONS_PATH}"),
                post(cancellations),
            )
            .route(
                &format!("/{GATEWAY_CHILD_WORKSPACE_PATH}"),
                post(child_workspace_completed),
            )
            .route(
                &format!("/{}", zuno_worker::GATEWAY_MERGE_PREPARE_PATH),
                post(merge::prepare),
            )
            .route(
                &format!("/{}", zuno_worker::GATEWAY_MERGE_AUTHORIZE_PATH),
                post(merge::authorize),
            )
            .route(
                &format!("/{}", zuno_worker::GATEWAY_MERGE_COMPLETION_PATH),
                post(merge::complete),
            )
            .route(
                &format!("/{}", zuno_worker::GATEWAY_MERGE_CANCELLATIONS_PATH),
                post(merge::cancellations),
            )
            .route(
                &format!("/{}", zuno_worker::GATEWAY_MERGE_READ_RESOLVE_PATH),
                post(merge::read_context),
            )
            .layer(DefaultBodyLimit::max(MAX_GATEWAY_FRAME_BYTES))
            .with_state(self)
    }

    async fn context(&self, lease: &ExecutionLease) -> Result<GatewayExecutionContext, Failure> {
        if lease.owner.tenant_id != self.tenant {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let scope = TurnStateScope {
            owner: lease.owner.clone(),
            session_id: lease.session_id.to_string(),
        };
        self.backend
            .worker_state(lease.clone())
            .clock(&scope)
            .await
            .map_err(|error| match error {
                zuno_engine::r#loop::TurnError::State(
                    zuno_engine::state::TurnStateError::Forbidden,
                ) => Failure(StatusCode::FORBIDDEN),
                zuno_engine::r#loop::TurnError::State(
                    zuno_engine::state::TurnStateError::LeaseLost,
                ) => Failure(StatusCode::CONFLICT),
                _ => Failure(StatusCode::SERVICE_UNAVAILABLE),
            })?;
        let job = self
            .backend
            .runtime(self.tenant.clone())
            .get(&lease.owner, &lease.job_id)
            .await
            .map_err(application)?;
        let assignment = self
            .configuration
            .resolve(&self.tenant, &job.configuration, &job.session_id)
            .map_err(application)?;
        assignment.environment.validate().map_err(application)?;
        if assignment.environment.session_id != lease.session_id {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let workspace = self
            .backend
            .runtime(self.tenant.clone())
            .execution_workspace(lease)
            .await
            .map_err(application)?;
        if let Some((gateway, receipt)) = &workspace
            && (*gateway != assignment.gateway_id || receipt.target.spec != assignment.environment)
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        Ok(GatewayExecutionContext {
            lease: lease.clone(),
            assignment,
            existing_workspace: workspace.is_some(),
            child_workspace: None,
            prepared_workspace: None,
            merge_source: None,
        })
    }

    async fn child_context(
        &self,
        request: &GatewayRequest,
        context: &mut GatewayExecutionContext,
    ) -> Result<(), Failure> {
        let GatewayCommand::PrepareChildWorkspace { child_job_id } = &request.command else {
            return Ok(());
        };
        let runtime = self.backend.runtime(self.tenant.clone());
        let info = runtime
            .child_workspace(&context.lease, child_job_id)
            .await
            .map_err(application)?;
        let target = self
            .configuration
            .resolve(&self.tenant, &info.configuration, &info.child_session_id)
            .map_err(application)?;
        let parent = self
            .configuration
            .resolve(
                &self.tenant,
                &info.parent_configuration,
                &info.parent_session_id,
            )
            .map_err(application)?;
        // This Docker gateway owns both volumes. Transfer to another gateway
        // requires a separate authenticated snapshot transport.
        if target.gateway_id != context.assignment.gateway_id
            || target.endpoint != context.assignment.endpoint
            || parent.gateway_id != target.gateway_id
            || parent.endpoint != target.endpoint
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let assignment = zuno_application::child::ChildWorkspaceAssignment {
            child_job_id: child_job_id.clone(),
            gateway_id: target.gateway_id,
            parent: parent.environment.clone(),
            target: target.environment,
            resume: info.resume,
        };
        runtime
            .admit_child_workspace(&context.lease, &assignment)
            .await
            .map_err(application)?;
        if info.parent_session_id != context.lease.session_id {
            // The data owner proved this is a node of the caller's staged
            // workflow. Its immutable group workspace must already exist.
            context.existing_workspace = true;
        }
        context.assignment = parent;
        context.child_workspace = Some(assignment);
        context.prepared_workspace = info.receipt;
        Ok(())
    }

    async fn gateway(&self, headers: &HeaderMap) -> Result<AuthenticatedGateway, Failure> {
        let gateway = self
            .gateways
            .authenticate(bearer(headers)?)
            .await
            .map_err(authentication)?;
        if gateway.subject().tenant_id != self.tenant {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        Ok(gateway)
    }

    async fn operation(
        &self,
        gateway: &AuthenticatedGateway,
        request: &GatewayOperationRequest,
    ) -> Result<OrganizationOperationAuthority, Failure> {
        let context = self.context(&request.lease).await?;
        if *gateway.id() != context.assignment.gateway_id
            || request.environment.owner != request.lease.owner
            || request.environment.spec != context.assignment.environment
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        request.operation.validate().map_err(application)?;
        Ok(OrganizationOperationAuthority::new(
            Arc::new(self.backend.runtime(self.tenant.clone())),
            Arc::new(self.backend.organizations(self.tenant.clone())),
        ))
    }
}

async fn cancellations(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(limit): Json<u32>,
) -> Result<Json<Vec<zuno_application::environment::OperationAdmission>>, Failure> {
    let gateway = service.gateway(&headers).await?;
    Ok(Json(
        service
            .backend
            .gateway_operations(gateway.id().clone())
            .cancellations(&service.tenant, limit)
            .await
            .map_err(application)?,
    ))
}

struct Failure(StatusCode);
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let error = match self.0 {
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "conflict",
            StatusCode::SERVICE_UNAVAILABLE => "unavailable",
            _ => "invalid_request",
        };
        (
            self.0,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({"error":error})),
        )
            .into_response()
    }
}
fn application(error: ApplicationError) -> Failure {
    Failure(match error {
        ApplicationError::Invalid(_) => StatusCode::BAD_REQUEST,
        ApplicationError::Forbidden => StatusCode::FORBIDDEN,
        ApplicationError::NotFound => StatusCode::NOT_FOUND,
        ApplicationError::Conflict | ApplicationError::LeaseLost => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    })
}
fn authentication(error: zuno_identity::worker::WorkerAuthError) -> Failure {
    Failure(match error {
        zuno_identity::worker::WorkerAuthError::Identity(
            zuno_identity::IdentityError::KeysUnavailable
            | zuno_identity::IdentityError::IntrospectionUnavailable,
        ) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::UNAUTHORIZED,
    })
}
fn field<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, Failure> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or(Failure(StatusCode::UNAUTHORIZED))?;
    if values.next().is_some() {
        return Err(Failure(StatusCode::UNAUTHORIZED));
    }
    Ok(value)
}
fn bearer(headers: &HeaderMap) -> Result<&str, Failure> {
    field(headers, header::AUTHORIZATION.as_str())?
        .strip_prefix("Bearer ")
        .filter(|value| !value.is_empty() && value.len() <= 32768)
        .ok_or(Failure(StatusCode::UNAUTHORIZED))
}
fn now() -> Result<i64, Failure> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|time| i64::try_from(time.as_millis()).ok())
        .ok_or(Failure(StatusCode::SERVICE_UNAVAILABLE))
}
fn target(request: &GatewayRequest, context: &GatewayExecutionContext) -> Result<(), Failure> {
    if let GatewayCommand::PrepareWorkspaceMerge { operation }
    | GatewayCommand::SubmitWorkspaceMerge { operation } = &request.command
        && operation.environment_id != context.assignment.environment.id
    {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    if let GatewayCommand::PrepareCommand { operation }
    | GatewayCommand::SubmitCommand { operation } = &request.command
        && operation.environment_id != context.assignment.environment.id
    {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    Ok(())
}

async fn issue_ticket(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<IssuedGatewayRequest>, Failure> {
    let worker = service
        .workers
        .authenticate(bearer(&headers)?)
        .await
        .map_err(authentication)?;
    let grant = JobGrantToken::try_from(field(&headers, GRANT_HEADER)?.to_owned())
        .map_err(authentication)?;
    let grant = service
        .grants
        .verify(&worker, &grant, now()?)
        .map_err(authentication)?;
    let request = GatewayRequest::decode(&bytes).map_err(application)?;
    let mut context = service.context(grant.lease()).await?;
    service.child_context(&request, &mut context).await?;
    service.merge_context(&request, &mut context).await?;
    target(&request, &context)?;
    let ticket = service
        .tickets
        .issue(
            &grant,
            context.assignment.gateway_id.clone(),
            &request,
            now()?,
        )
        .map_err(authentication)?;
    Ok(Json(IssuedGatewayRequest {
        assignment: context.assignment,
        ticket,
    }))
}

async fn resolve(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<GatewayExecutionContext>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let ticket = GatewayTicket::try_from(field(&headers, GATEWAY_TICKET_HEADER)?.to_owned())
        .map_err(authentication)?;
    let request = GatewayRequest::decode(&bytes).map_err(application)?;
    let verified = service
        .tickets
        .verify(&gateway, &ticket, &request, now()?)
        .map_err(authentication)?;
    let mut context = service.context(verified.lease()).await?;
    if *gateway.id() != context.assignment.gateway_id {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    service.child_context(&request, &mut context).await?;
    service.merge_context(&request, &mut context).await?;
    target(&request, &context)?;
    Ok(Json(context))
}

async fn prepare(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<GatewayOperationRequest>,
) -> Result<Json<ApprovalRecord>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let authority = service.operation(&gateway, &request).await?;
    let proposal = authority
        .proposal(&request.lease, &request.environment, &request.operation)
        .await
        .map_err(application)?;
    let approval = service
        .backend
        .organizations(service.tenant.clone())
        .admit(&request.lease, proposal)
        .await
        .map_err(application)?;
    Ok(Json(approval))
}

async fn authorize(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(request): Json<GatewayOperationRequest>,
) -> Result<Json<CheckedApproval>, Failure> {
    let gateway = service.gateway(&headers).await?;
    let authority = service.operation(&gateway, &request).await?;
    let proposal = authority
        .proposal(&request.lease, &request.environment, &request.operation)
        .await
        .map_err(application)?;
    let checked = service
        .backend
        .organizations(service.tenant.clone())
        .check_gateway_execution(
            proposal,
            &zuno_application::environment::OperationAdmission {
                gateway_id: gateway.id().clone(),
                lease: request.lease,
                environment: request.environment,
                operation: request.operation,
            },
        )
        .await
        .map_err(application)?;
    Ok(Json(checked))
}

async fn completion(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(completion): Json<zuno_application::environment::OperationCompletion>,
) -> Result<StatusCode, Failure> {
    let gateway = service.gateway(&headers).await?;
    if completion.lease.owner.tenant_id != service.tenant {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    service
        .backend
        .gateway_operations(gateway.id().clone())
        .complete(&completion)
        .await
        .map_err(application)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn child_workspace_completed(
    State(service): State<GatewayControlService>,
    headers: HeaderMap,
    Json(completion): Json<zuno_application::child::ChildWorkspaceCompletion>,
) -> Result<StatusCode, Failure> {
    let gateway = service.gateway(&headers).await?;
    if completion.lease.owner.tenant_id != service.tenant {
        return Err(Failure(StatusCode::FORBIDDEN));
    }
    service
        .backend
        .runtime(service.tenant)
        .complete_child_workspace(gateway.id(), &completion)
        .await
        .map_err(application)?;
    Ok(StatusCode::NO_CONTENT)
}
