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
            GatewayExecutionContext, GatewayOperationRequest, GatewayReply, GatewayRequest,
            MAX_GATEWAY_FRAME_BYTES,
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
        if issued.assignment.environment.session_id != execution.job.session_id {
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
            return Err(ApplicationError::Forbidden);
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
        if checked.lease != *lease
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
