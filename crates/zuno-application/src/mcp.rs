//! Owner-scoped external MCP operations. Credentials and connection sessions
//! belong to the gateway; neither appears in these durable contracts.
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zuno_types::{
    activity::ActivityName,
    identity::{EnvironmentId, GatewayId, InvocationId, OperationId, PrincipalKey},
};

use crate::{ApplicationError, runtime::ExecutionLease};

pub const MAX_MCP_ARGUMENT_BYTES: usize = 65536;
pub const MAX_MCP_RESULT_BYTES: usize = 262144;
pub const MAX_MCP_DEFINITION_BYTES: usize = 32768;

/// Installed with an immutable Agent definition. `connection` selects a
/// gateway-owned, owner-bound target; the model cannot supply a URL or token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpToolBinding {
    pub connection: ActivityName,
    pub server: ActivityName,
    pub tool: ActivityName,
    pub definition: Value,
    /// Explicitly reviewed resource/credential assignment revision.
    pub revision: u64,
    /// Exact reviewed network target, configured by the host. Never selected
    /// from tool arguments or obtained by following a redirect.
    pub endpoint: String,
}
impl McpToolBinding {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        let endpoint = url::Url::parse(&self.endpoint)
            .map_err(|_| ApplicationError::Invalid("invalid MCP target".to_owned()))?;
        if self.revision == 0
            || endpoint.scheme() != "https"
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || self.endpoint.len() > 2048
            || !self.definition.is_object()
            || self.definition.get("name").and_then(Value::as_str) != Some(self.tool.as_str())
            || !self
                .definition
                .get("inputSchema")
                .is_some_and(Value::is_object)
            || serde_json::to_vec(&self.definition)
                .map_err(ApplicationError::storage)?
                .len()
                > MAX_MCP_DEFINITION_BYTES
        {
            return Err(ApplicationError::Invalid(
                "invalid frozen MCP declaration".to_owned(),
            ));
        }
        Ok(())
    }
    pub fn digest(&self) -> String {
        zuno_orchestration::sha256_json(&json!(self))
    }
    pub fn wire_name(&self) -> String {
        format!("mcp_{}", &self.digest()[..32])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpOperation {
    pub id: OperationId,
    pub invocation_id: InvocationId,
    pub environment_id: EnvironmentId,
    pub binding: McpToolBinding,
    pub arguments: Value,
}
/// Human review of the full frozen declaration, target and arguments. No
/// execution lease, token or gateway credential is exposed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCallView {
    pub approval_id: zuno_types::identity::ApprovalId,
    pub operation_id: OperationId,
    pub server: ActivityName,
    pub tool: ActivityName,
    pub definition: Value,
    pub arguments: Value,
    pub endpoint: String,
    pub admitted: bool,
}
impl McpOperation {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.binding.validate()?;
        if !self.arguments.is_object()
            || serde_json::to_vec(&self.arguments)
                .map_err(ApplicationError::storage)?
                .len()
                > MAX_MCP_ARGUMENT_BYTES
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded MCP arguments".to_owned(),
            ));
        }
        Ok(())
    }
    pub fn digest(&self) -> String {
        zuno_orchestration::sha256_json(&json!(self))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAdmission {
    pub gateway_id: GatewayId,
    pub lease: ExecutionLease,
    pub operation: McpOperation,
}
impl McpAdmission {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.operation.validate()
    }
    /// Stable across Worker attempts and lease renewals.
    pub fn digest(&self) -> String {
        zuno_orchestration::sha256_json(&json!([
            self.gateway_id,
            self.lease.owner,
            self.lease.job_id,
            self.lease.session_id,
            self.operation
        ]))
    }
    pub fn arguments_digest(&self) -> String {
        zuno_orchestration::sha256_json(&self.operation.arguments)
    }
    pub fn resources_digest(&self) -> String {
        zuno_orchestration::sha256_json(&json!([
            self.gateway_id,
            self.lease.owner,
            self.operation.environment_id,
            self.operation.binding
        ]))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpOperationState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Uncertain,
}
impl McpOperationState {
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpReceipt {
    pub id: OperationId,
    pub request_digest: String,
    pub state: McpOperationState,
    pub cancellation_requested: bool,
    /// Full bounded MCP result, retained as untrusted tool content.
    pub result: Option<Value>,
    /// Typed gateway outcome, never a peer error containing credentials.
    pub failure: Option<McpFailure>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpFailure {
    ConnectionUnavailable,
    DefinitionChanged,
    ResultTooLarge,
    LostOutcome,
    CancelledBeforeCall,
    AuthorizationRevoked,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCompletion {
    pub admission: McpAdmission,
    pub receipt: McpReceipt,
}
impl McpCompletion {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.admission.validate()?;
        let receipt = &self.receipt;
        if receipt.id != self.admission.operation.id
            || receipt.request_digest != self.admission.operation.digest()
            || !receipt.state.terminal()
            || receipt
                .result
                .as_ref()
                .is_some_and(|result| !result.is_object())
            || serde_json::to_vec(&receipt.result)
                .map_err(ApplicationError::storage)?
                .len()
                > MAX_MCP_RESULT_BYTES
            || (receipt.state == McpOperationState::Succeeded
                && (receipt.result.is_none() || receipt.failure.is_some()))
            || (receipt.state == McpOperationState::Uncertain
                && (receipt.result.is_some() || receipt.failure.is_none()))
            || (receipt.state == McpOperationState::Cancelled
                && (receipt.result.is_some() || receipt.failure.is_none()))
            || (receipt.state == McpOperationState::Failed
                && receipt.result.is_none()
                && receipt.failure.is_none())
            || receipt.result.as_ref().is_some_and(|result| {
                !result.get("content").is_some_and(Value::is_array)
                    || result
                        .get("isError")
                        .is_some_and(|value| !value.is_boolean())
                    || (receipt.state == McpOperationState::Succeeded
                        && result.get("isError") == Some(&Value::Bool(true)))
                    || (receipt.state == McpOperationState::Failed
                        && result.get("isError") != Some(&Value::Bool(true)))
            })
        {
            return Err(ApplicationError::Conflict);
        }
        Ok(())
    }
}

#[async_trait]
pub trait McpOperationAuthority: Send + Sync {
    async fn authorize_mcp(&self, admission: &McpAdmission) -> Result<(), ApplicationError>;
    /// Rechecks a gateway-owned operation already durably admitted. Releasing a
    /// Worker slot is allowed; current cancellation/policy/approval still apply.
    async fn check_admitted_mcp(&self, admission: &McpAdmission) -> Result<(), ApplicationError>;
}
#[async_trait]
pub trait McpCompletionSink: Send + Sync {
    async fn publish_mcp(&self, completion: &McpCompletion) -> Result<(), ApplicationError>;
}
/// Implementations resolve only explicit owner assignments. A missing binding
/// is a denial, never a fallback to process-global MCP configuration.
#[async_trait]
pub trait McpConnectionProvider: Send + Sync {
    async fn prepare(
        &self,
        owner: &PrincipalKey,
        binding: &McpToolBinding,
    ) -> Result<Box<dyn PreparedMcpCall>, McpFailure>;
}
/// A freshly authenticated connection with its declaration verified. The
/// executor rechecks execution authority immediately before this one call.
#[async_trait]
pub trait PreparedMcpCall: Send {
    async fn call(self: Box<Self>, arguments: &Value) -> Result<Value, McpFailure>;
}
