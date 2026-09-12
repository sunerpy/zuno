//! Gateway transports carry scoped capabilities, never database credentials.

use super::*;
use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use std::{sync::Arc, time::Duration};
use zuno_application::{
    ApplicationError,
    authorization::{ApprovalRecord, CheckedApproval},
    environment::{
        CommandOperation, Environment, OperationAuthority,
        wire::{
            GatewayCommand, GatewayExecutionContext, GatewayOperationRequest, GatewayReply,
            GatewayRequest, MAX_GATEWAY_FRAME_BYTES,
        },
    },
    runtime::ExecutionLease,
};
use zuno_engine::state::TurnStateError;
use zuno_identity::gateway::GatewayTicket;

impl WorkerClient {
    pub async fn gateway_ticket(
        &self,
        execution: &WorkerExecution,
        request: &GatewayRequest,
    ) -> Result<IssuedGatewayRequest, TurnStateError> {
        if execution.boundary_started() {
            return Err(TurnStateError::LeaseLost);
        }
        let grant = execution
            .credential
            .read()
            .map_err(|_| TurnStateError::InvalidData)?
            .grant
            .clone();
        let bytes = self
            .post(
                GATEWAY_TICKET_PATH,
                Some(&grant),
                request.encode().map_err(|_| TurnStateError::InvalidData)?,
            )
            .await?;
        let issued: IssuedGatewayRequest =
            serde_json::from_slice(&bytes).map_err(|_| TurnStateError::InvalidData)?;
        issued
            .assignment
            .environment
            .validate()
            .map_err(|_| TurnStateError::InvalidData)?;
        if issued.assignment.environment.session_id != execution.job.session_id
            && !matches!(
                request.command,
                GatewayCommand::PrepareChildWorkspace { .. }
            )
        {
            return Err(TurnStateError::InvalidData);
        }
        validate_endpoint(&issued.assignment.endpoint)?;
        Ok(issued)
    }
}

/// The gateway uses its own workload credential to talk to the control plane.
#[derive(Clone)]
pub struct GatewayStateClient {
    control: WorkerClient,
}
impl GatewayStateClient {
    pub async fn import_context(
        &self,
        ticket: &zuno_identity::gateway::GatewayImportTicket,
        request: &zuno_application::workspace_import::WorkspaceUploadRequest,
    ) -> Result<zuno_application::workspace_import::WorkspaceImportAssignment, ApplicationError>
    {
        let bytes = self
            .control
            .post_header(
                GATEWAY_IMPORT_RESOLVE_PATH,
                Some((GATEWAY_IMPORT_TICKET_HEADER, ticket.expose())),
                serde_json::to_vec(request).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        serde_json::from_slice(&bytes).map_err(ApplicationError::storage)
    }
    pub async fn merge_read_context(
        &self,
        ticket: &zuno_identity::gateway::GatewayReadTicket,
        request: &zuno_application::workspace_merge::MergeContentRequest,
    ) -> Result<zuno_application::workspace_merge::MergeContentContext, ApplicationError> {
        let bytes = self
            .control
            .post_header(
                GATEWAY_MERGE_READ_RESOLVE_PATH,
                Some((GATEWAY_READ_TICKET_HEADER, ticket.expose())),
                serde_json::to_vec(request).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        serde_json::from_slice(&bytes).map_err(ApplicationError::storage)
    }
    pub async fn prepare_merge(
        &self,
        request: zuno_application::workspace_merge::GatewayMergeRequest,
    ) -> Result<ApprovalRecord, ApplicationError> {
        let bytes = self
            .control
            .post(
                GATEWAY_MERGE_PREPARE_PATH,
                None,
                serde_json::to_vec(&request).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        serde_json::from_slice(&bytes).map_err(ApplicationError::storage)
    }
    pub async fn merge_cancellations(
        &self,
        limit: u32,
    ) -> Result<Vec<zuno_application::workspace_merge::WorkspaceMergeAdmission>, ApplicationError>
    {
        if !(1..=32).contains(&limit) {
            return Err(ApplicationError::Invalid(
                "invalid merge cancellation limit".to_owned(),
            ));
        }
        let bytes = self
            .control
            .post(
                GATEWAY_MERGE_CANCELLATIONS_PATH,
                None,
                serde_json::to_vec(&limit).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        let result: Vec<zuno_application::workspace_merge::WorkspaceMergeAdmission> =
            serde_json::from_slice(&bytes).map_err(ApplicationError::storage)?;
        if result.len() > limit as usize {
            return Err(ApplicationError::Invalid(
                "unbounded merge cancellation response".to_owned(),
            ));
        }
        Ok(result)
    }
    pub async fn cancellations(
        &self,
        limit: u32,
    ) -> Result<Vec<zuno_application::environment::OperationAdmission>, ApplicationError> {
        if !(1..=64).contains(&limit) {
            return Err(ApplicationError::Invalid(
                "invalid cancellation batch".to_owned(),
            ));
        }
        let bytes = self
            .control
            .post(
                GATEWAY_CANCELLATIONS_PATH,
                None,
                serde_json::to_vec(&limit).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        let pending: Vec<zuno_application::environment::OperationAdmission> =
            serde_json::from_slice(&bytes).map_err(ApplicationError::storage)?;
        if pending.len() > limit as usize {
            return Err(ApplicationError::Invalid(
                "unbounded cancellation response".to_owned(),
            ));
        }
        Ok(pending)
    }

    pub fn new(
        endpoint: Url,
        tokens: Arc<dyn AccessTokenSource>,
        certificate: Option<reqwest::Certificate>,
    ) -> Result<Self, TurnStateError> {
        Ok(Self {
            control: WorkerClient::new(endpoint, tokens, certificate)?,
        })
    }

    pub async fn resolve(
        &self,
        ticket: &GatewayTicket,
        request: &GatewayRequest,
    ) -> Result<GatewayExecutionContext, ApplicationError> {
        let bytes = self
            .control
            .post_header(
                GATEWAY_RESOLVE_PATH,
                Some((GATEWAY_TICKET_HEADER, ticket.expose())),
                request.encode()?,
            )
            .await
            .map_err(state_error)?;
        let context: GatewayExecutionContext =
            serde_json::from_slice(&bytes).map_err(ApplicationError::storage)?;
        context.assignment.environment.validate()?;
        if context.assignment.environment.session_id != context.lease.session_id {
            // Only the control-plane-resolved preparation of this exact child
            // may copy from its immutable Workflow group workspace. This does
            // not authorize commands in another session.
            let GatewayCommand::PrepareChildWorkspace { child_job_id } = &request.command else {
                return Err(ApplicationError::Forbidden);
            };
            let assignment = context
                .child_workspace
                .as_ref()
                .ok_or(ApplicationError::Forbidden)?;
            if assignment.child_job_id != *child_job_id
                || assignment.parent != context.assignment.environment
                || assignment.gateway_id != context.assignment.gateway_id
                || !context.existing_workspace
            {
                return Err(ApplicationError::Forbidden);
            }
        }
        Ok(context)
    }

    pub async fn prepare(
        &self,
        request: GatewayOperationRequest,
    ) -> Result<ApprovalRecord, ApplicationError> {
        let bytes = self
            .control
            .post(
                GATEWAY_PREPARE_PATH,
                None,
                serde_json::to_vec(&request).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        serde_json::from_slice(&bytes).map_err(ApplicationError::storage)
    }

    pub async fn child_workspace_completed(
        &self,
        completion: &zuno_application::child::ChildWorkspaceCompletion,
    ) -> Result<(), ApplicationError> {
        self.control
            .post(
                GATEWAY_CHILD_WORKSPACE_PATH,
                None,
                serde_json::to_vec(completion).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        Ok(())
    }
}
#[async_trait]
impl zuno_application::workspace_import::WorkspaceInitializationAuthority for GatewayStateClient {
    async fn authorize_initialization(
        &self,
        assigned: &zuno_application::workspace_import::WorkspaceImportAssignment,
    ) -> Result<Option<zuno_application::workspace_import::WorkspaceImportReceipt>, ApplicationError>
    {
        let response = self
            .control
            .post(
                GATEWAY_IMPORT_AUTHORIZE_PATH,
                None,
                serde_json::to_vec(assigned).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        serde_json::from_slice(&response).map_err(ApplicationError::storage)
    }
    async fn initialized(
        &self,
        assigned: &zuno_application::workspace_import::WorkspaceImportAssignment,
        receipt: &zuno_application::workspace_import::WorkspaceImportReceipt,
    ) -> Result<(), ApplicationError> {
        assigned.validate_receipt(receipt)?;
        self.control
            .post(
                GATEWAY_IMPORT_COMPLETE_PATH,
                None,
                serde_json::to_vec(
                    &zuno_application::workspace_import::WorkspaceInitializationCompletion {
                        assignment: assigned.clone(),
                        receipt: receipt.clone(),
                    },
                )
                .map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        Ok(())
    }
}

#[async_trait]
impl zuno_application::workspace_merge::WorkspaceMergeAuthority for GatewayStateClient {
    async fn authorize_merge(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        operation: &zuno_application::workspace_merge::WorkspaceMergeOperation,
    ) -> Result<(), ApplicationError> {
        let request = zuno_application::workspace_merge::GatewayMergeRequest {
            lease: lease.clone(),
            environment: environment.clone(),
            operation: operation.clone(),
        };
        let bytes = self
            .control
            .post(
                GATEWAY_MERGE_AUTHORIZE_PATH,
                None,
                serde_json::to_vec(&request).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        let checked: CheckedApproval =
            serde_json::from_slice(&bytes).map_err(ApplicationError::storage)?;
        if checked.lease.owner != lease.owner
            || checked.lease.job_id != lease.job_id
            || checked.lease.session_id != lease.session_id
            || checked.lease.attempt_id != lease.attempt_id
            || checked.lease.worker != lease.worker
            || checked.lease.epoch != lease.epoch
            || checked.lease.checkpoint_version != lease.checkpoint_version
            || checked.lease.expires_at_ms < lease.expires_at_ms
            || checked.binding.operation_id != operation.id
            || checked.binding.invocation_id != operation.invocation_id
            || checked.binding.job_id != lease.job_id
            || checked.binding.session_id != lease.session_id
            || checked.binding.arguments_sha256 != operation.plan.digest()
        {
            return Err(ApplicationError::Forbidden);
        }
        Ok(())
    }
}
#[async_trait]
impl zuno_application::workspace_merge::WorkspaceMergeCompletionSink for GatewayStateClient {
    async fn publish_merge(
        &self,
        completion: &zuno_application::workspace_merge::WorkspaceMergeCompletion,
    ) -> Result<(), ApplicationError> {
        completion.validate()?;
        self.control
            .post(
                GATEWAY_MERGE_COMPLETION_PATH,
                None,
                serde_json::to_vec(completion).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        Ok(())
    }
}

#[async_trait]
impl OperationAuthority for GatewayStateClient {
    async fn authorize(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        operation: &CommandOperation,
    ) -> Result<(), ApplicationError> {
        let request = GatewayOperationRequest {
            lease: lease.clone(),
            environment: environment.clone(),
            operation: operation.clone(),
        };
        let bytes = self
            .control
            .post(
                GATEWAY_AUTHORIZE_PATH,
                None,
                serde_json::to_vec(&request).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        let checked: CheckedApproval =
            serde_json::from_slice(&bytes).map_err(ApplicationError::storage)?;
        if checked.lease.owner != lease.owner
            || checked.lease.job_id != lease.job_id
            || checked.lease.session_id != lease.session_id
            || checked.lease.attempt_id != lease.attempt_id
            || checked.lease.worker != lease.worker
            || checked.lease.epoch != lease.epoch
            || checked.lease.checkpoint_version != lease.checkpoint_version
            || checked.lease.expires_at_ms < lease.expires_at_ms
            || checked.binding.operation_id != operation.id
            || checked.binding.invocation_id != operation.invocation_id
            || checked.binding.job_id != lease.job_id
            || checked.binding.session_id != lease.session_id
        {
            return Err(ApplicationError::Forbidden);
        }
        Ok(())
    }
}

#[async_trait]
impl zuno_application::environment::OperationCompletionSink for GatewayStateClient {
    async fn publish(
        &self,
        completion: &zuno_application::environment::OperationCompletion,
    ) -> Result<(), ApplicationError> {
        completion.validate()?;
        self.control
            .post(
                GATEWAY_COMPLETION_PATH,
                None,
                serde_json::to_vec(completion).map_err(ApplicationError::storage)?,
            )
            .await
            .map_err(state_error)?;
        Ok(())
    }
}

/// Worker-facing execution client. Each request needs a separately minted
/// ticket bound to its full payload; no OAuth access token is sent to the gateway.
pub struct GatewayClient {
    client: reqwest::Client,
}
impl GatewayClient {
    pub fn new(certificate: Option<reqwest::Certificate>) -> Result<Self, TurnStateError> {
        let mut builder = zuno_network::client_builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(30));
        if let Some(certificate) = certificate {
            builder = builder.add_root_certificate(certificate)
        }
        Ok(Self {
            client: builder.build().map_err(|_| TurnStateError::Unavailable)?,
        })
    }

    pub async fn execute(
        &self,
        issued: &IssuedGatewayRequest,
        request: &GatewayRequest,
    ) -> Result<GatewayReply, ApplicationError> {
        let endpoint = validate_endpoint(&issued.assignment.endpoint).map_err(state_error)?;
        let mut credential = HeaderValue::from_str(issued.ticket.expose())
            .map_err(|_| ApplicationError::Forbidden)?;
        credential.set_sensitive(true);
        let mut response = self
            .client
            .post(
                endpoint
                    .join(GATEWAY_EXECUTE_PATH)
                    .map_err(ApplicationError::storage)?,
            )
            .header(GATEWAY_TICKET_HEADER, credential)
            .header(CONTENT_TYPE, "application/json")
            .body(request.encode()?)
            .send()
            .await
            .map_err(|_| ApplicationError::Unavailable)?;
        match response.status().as_u16() {
            200..=299 => {}
            401 | 403 => return Err(ApplicationError::Forbidden),
            404 => return Err(ApplicationError::NotFound),
            409 => return Err(ApplicationError::Conflict),
            500..=599 => return Err(ApplicationError::Unavailable),
            _ => {
                return Err(ApplicationError::Invalid(
                    "gateway rejected the request".to_owned(),
                ));
            }
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ApplicationError::Unavailable)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_GATEWAY_FRAME_BYTES {
                return Err(ApplicationError::Invalid(
                    "gateway response is too large".to_owned(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let reply: GatewayReply =
            serde_json::from_slice(&bytes).map_err(ApplicationError::storage)?;
        use zuno_application::environment::wire::GatewayCommand;
        let matches = match (&request.command, &reply) {
            (
                GatewayCommand::PreviewWorkspaceMerge {
                    id,
                    invocation_id,
                    child_job_id,
                },
                GatewayReply::WorkspaceMergePreview(operation),
            ) => {
                operation.id == *id
                    && operation.invocation_id == *invocation_id
                    && operation.child_job_id == *child_job_id
                    && operation.environment_id == issued.assignment.environment.id
                    && operation.validate().is_ok()
            }
            (
                GatewayCommand::PrepareWorkspaceMerge { operation },
                GatewayReply::Approval(approval),
            ) => {
                approval.binding.operation_id == operation.id
                    && approval.binding.invocation_id == operation.invocation_id
                    && approval.binding.arguments_sha256 == operation.plan.digest()
            }
            (
                GatewayCommand::SubmitWorkspaceMerge { operation },
                GatewayReply::WorkspaceMergeReceipt(receipt),
            ) => {
                receipt.id == operation.id
                    && receipt.environment_id == issued.assignment.environment.id
                    && receipt.plan_digest == operation.plan.digest()
            }
            (
                GatewayCommand::InspectWorkspaceMerge { operation_id },
                GatewayReply::WorkspaceMergeReceipt(receipt),
            ) => {
                receipt.id == *operation_id
                    && receipt.environment_id == issued.assignment.environment.id
            }
            (
                GatewayCommand::Acquire | GatewayCommand::Get,
                GatewayReply::Environment(environment),
            ) => environment.spec == issued.assignment.environment,
            (GatewayCommand::PrepareCommand { operation }, GatewayReply::Approval(approval)) => {
                approval.binding.operation_id == operation.id
                    && approval.binding.invocation_id == operation.invocation_id
            }
            (GatewayCommand::SubmitCommand { operation }, GatewayReply::Operation(receipt)) => {
                receipt.id == operation.id
                    && receipt.environment_id == issued.assignment.environment.id
            }
            (GatewayCommand::Inspect { operation_id }, GatewayReply::Operation(receipt)) => {
                receipt.id == *operation_id
                    && receipt.environment_id == issued.assignment.environment.id
            }
            (GatewayCommand::Output { .. }, GatewayReply::Output(_)) => true,
            (
                GatewayCommand::PrepareChildWorkspace { child_job_id },
                GatewayReply::ChildWorkspace(receipt),
            ) => {
                receipt.child_job_id == *child_job_id
                    && receipt.parent_environment_id == issued.assignment.environment.id
                    && receipt.target.spec.validate().is_ok()
                    && receipt.target.revision > 0
            }
            _ => false,
        };
        if !matches {
            return Err(ApplicationError::Invalid(
                "gateway reply does not match its request".to_owned(),
            ));
        }
        Ok(reply)
    }
}

fn validate_endpoint(value: &str) -> Result<Url, TurnStateError> {
    let url = Url::parse(value).map_err(|_| TurnStateError::InvalidData)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with('/')
    {
        return Err(TurnStateError::InvalidData);
    }
    Ok(url)
}
fn state_error(error: TurnStateError) -> ApplicationError {
    match error {
        TurnStateError::Forbidden => ApplicationError::Forbidden,
        TurnStateError::LeaseLost => ApplicationError::LeaseLost,
        TurnStateError::NotFound => ApplicationError::NotFound,
        TurnStateError::Conflict => ApplicationError::Conflict,
        TurnStateError::InvalidData => {
            ApplicationError::Invalid("invalid gateway state response".to_owned())
        }
        _ => ApplicationError::Unavailable,
    }
}
