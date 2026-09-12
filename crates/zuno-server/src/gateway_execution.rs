//! Execution-side router. The gateway owns Docker and its ledger; it has only a
//! scoped HTTP state client, never a PostgreSQL pool or model credential.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use std::{path::Path, sync::Arc};
use zuno_application::{
    ApplicationError,
    environment::{
        EnvironmentProvider, OperationGateway,
        wire::{
            GatewayCommand, GatewayExecutionContext, GatewayOperationRequest, GatewayReply,
            GatewayRequest, MAX_GATEWAY_FRAME_BYTES,
        },
    },
};
use zuno_environment::DockerGateway;
use zuno_identity::gateway::GatewayTicket;
use zuno_types::identity::GatewayId;
use zuno_worker::{GATEWAY_EXECUTE_PATH, GATEWAY_TICKET_HEADER, gateway::GatewayStateClient};

#[derive(Clone)]
pub struct GatewayExecutionService {
    id: GatewayId,
    gateway: Arc<DockerGateway>,
    state: GatewayStateClient,
}
impl GatewayExecutionService {
    /// Host lifecycle calls this bounded scan repeatedly with interruptible
    /// backoff. Work is reconstructed from the ledger, not an in-memory list.
    pub async fn deliver_completions(&self, limit: u32) -> Result<u32, ApplicationError> {
        self.gateway.deliver_completions(&self.state, limit).await
    }

    /// Host administration owns environment lifecycle; this is not an HTTP
    /// capability and is never installed in the Worker's tool registry.
    pub fn environments(&self) -> Arc<dyn EnvironmentProvider> {
        self.gateway.clone()
    }

    pub async fn connect(
        id: GatewayId,
        socket: &Path,
        ledger: &Path,
        state: GatewayStateClient,
    ) -> Result<Self, ApplicationError> {
        let gateway =
            Arc::new(DockerGateway::connect(socket, ledger, Arc::new(state.clone())).await?);
        Ok(Self { id, gateway, state })
    }

    pub fn router(self) -> Router {
        Router::new()
            .route(&format!("/{GATEWAY_EXECUTE_PATH}"), post(execute))
            .layer(DefaultBodyLimit::max(MAX_GATEWAY_FRAME_BYTES))
            .with_state(self)
    }

    async fn environment(
        &self,
        context: &GatewayExecutionContext,
    ) -> Result<zuno_application::environment::Environment, ApplicationError> {
        let environment = self
            .gateway
            .get(&context.lease.owner, &context.assignment.environment.id)
            .await?;
        if environment.spec != context.assignment.environment {
            return Err(ApplicationError::Conflict);
        }
        Ok(environment)
    }

    async fn run(
        &self,
        ticket: &GatewayTicket,
        request: GatewayRequest,
    ) -> Result<GatewayReply, ApplicationError> {
        let context = self.state.resolve(ticket, &request).await?;
        if context.assignment.gateway_id != self.id {
            return Err(ApplicationError::Forbidden);
        }
        let reply = match request.command {
            GatewayCommand::Acquire => GatewayReply::Environment(if context.existing_workspace {
                self.environment(&context).await?
            } else {
                self.gateway
                    .acquire(&context.lease.owner, context.assignment.environment)
                    .await?
            }),
            GatewayCommand::Get => GatewayReply::Environment(self.environment(&context).await?),
            GatewayCommand::PrepareChildWorkspace { child_job_id } => {
                let assignment = context
                    .child_workspace
                    .as_ref()
                    .ok_or(ApplicationError::Forbidden)?;
                if assignment.child_job_id != child_job_id
                    || assignment.gateway_id != self.id
                    || assignment.parent != context.assignment.environment
                {
                    return Err(ApplicationError::Forbidden);
                }
                let receipt = if let Some(receipt) = context.prepared_workspace {
                    receipt
                } else {
                    self.gateway
                        .prepare_child_workspace(
                            &context.lease.owner,
                            assignment,
                            context.existing_workspace,
                        )
                        .await?
                };
                self.state
                    .child_workspace_completed(&zuno_application::child::ChildWorkspaceCompletion {
                        lease: context.lease,
                        receipt: receipt.clone(),
                    })
                    .await?;
                GatewayReply::ChildWorkspace(receipt)
            }
            GatewayCommand::PrepareCommand { operation } => {
                let environment = self.environment(&context).await?;
                GatewayReply::Approval(Box::new(
                    self.state
                        .prepare(GatewayOperationRequest {
                            lease: context.lease,
                            environment,
                            operation,
                        })
                        .await?,
                ))
            }
            GatewayCommand::SubmitCommand { operation } => {
                self.environment(&context).await?;
                GatewayReply::Operation(self.gateway.submit(&context.lease, operation).await?)
            }
            GatewayCommand::Inspect { operation_id } => {
                let receipt = self
                    .gateway
                    .inspect(&context.lease.owner, &operation_id)
                    .await?;
                if receipt.environment_id != context.assignment.environment.id {
                    return Err(ApplicationError::Forbidden);
                }
                GatewayReply::Operation(receipt)
            }
            GatewayCommand::Output {
                operation_id,
                cursor,
                maximum_bytes,
            } => {
                let receipt = self
                    .gateway
                    .inspect(&context.lease.owner, &operation_id)
                    .await?;
                if receipt.environment_id != context.assignment.environment.id {
                    return Err(ApplicationError::Forbidden);
                }
                GatewayReply::Output(
                    self.gateway
                        .output(&context.lease.owner, &operation_id, cursor, maximum_bytes)
                        .await?,
                )
            }
        };
        Ok(reply)
    }
}

struct Failure(ApplicationError);
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (status, error) = match self.0 {
            ApplicationError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApplicationError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApplicationError::Conflict | ApplicationError::LeaseLost => {
                (StatusCode::CONFLICT, "conflict")
            }
            ApplicationError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            _ => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        };
        (
            status,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({"error":error})),
        )
            .into_response()
    }
}
impl From<ApplicationError> for Failure {
    fn from(error: ApplicationError) -> Self {
        Self(error)
    }
}

async fn execute(
    State(service): State<GatewayExecutionService>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<impl IntoResponse, Failure> {
    let mut values = headers.get_all(GATEWAY_TICKET_HEADER).iter();
    let value = values
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or(Failure(ApplicationError::Forbidden))?;
    if values.next().is_some() {
        return Err(Failure(ApplicationError::Forbidden));
    }
    let ticket = GatewayTicket::try_from(value.to_owned())
        .map_err(|_| Failure(ApplicationError::Forbidden))?;
    let request = GatewayRequest::decode(&bytes)?;
    let reply = service.run(&ticket, request).await?;
    let bytes = serde_json::to_vec(&reply).map_err(ApplicationError::storage)?;
    if bytes.len() > MAX_GATEWAY_FRAME_BYTES {
        return Err(Failure(ApplicationError::Invalid(
            "gateway response is too large".to_owned(),
        )));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    ))
}
