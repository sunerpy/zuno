//! Private gateway protocol. It contains no host paths, Docker socket or DB
//! credentials. Public activity projections use a different protocol.

use crate::child::{ChildWorkspaceAssignment, ChildWorkspaceReceipt};
use serde::{Deserialize, Serialize};
use zuno_types::identity::{GatewayId, JobId, OperationId};

use super::{
    CommandOperation, Environment, EnvironmentSpec, OperationReceipt, OutputCursor, OutputPage,
};
use crate::{ApplicationError, authorization::ApprovalRecord, runtime::ExecutionLease};

pub const GATEWAY_PROTOCOL_VERSION: u32 = 3;
pub const MAX_GATEWAY_FRAME_BYTES: usize = 1024 * 1024;

/// A data-owner response, never a caller-selected deployment.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayAssignment {
    pub gateway_id: GatewayId,
    pub endpoint: String,
    pub environment: EnvironmentSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayExecutionContext {
    pub lease: ExecutionLease,
    pub assignment: GatewayAssignment,
    pub existing_workspace: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_workspace: Option<ChildWorkspaceAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_workspace: Option<ChildWorkspaceReceipt>,
}

/// Only an authenticated, assigned gateway may present these observed facts.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayOperationRequest {
    pub lease: ExecutionLease,
    pub environment: Environment,
    pub operation: CommandOperation,
}

/// The environment is selected by the data owner for the signed Job context.
/// A child workspace may only be prepared for a server-resolved staged child.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayCommand {
    Acquire,
    Get,
    PrepareChildWorkspace {
        child_job_id: JobId,
    },
    PrepareCommand {
        operation: CommandOperation,
    },
    SubmitCommand {
        operation: CommandOperation,
    },
    Inspect {
        operation_id: OperationId,
    },
    Output {
        operation_id: OperationId,
        cursor: OutputCursor,
        maximum_bytes: u32,
    },
}

impl GatewayCommand {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        match self {
            Self::PrepareCommand { operation } | Self::SubmitCommand { operation } => {
                operation.validate()
            }
            Self::Output {
                cursor,
                maximum_bytes,
                ..
            } => {
                let hash_valid = cursor.prefix_sha256.as_ref().is_none_or(|hash| {
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                });
                if !(1..=65536).contains(maximum_bytes)
                    || !hash_valid
                    || (cursor.offset > 0 && cursor.prefix_sha256.is_none())
                {
                    return Err(ApplicationError::Invalid(
                        "invalid gateway output cursor or bound".to_owned(),
                    ));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayRequest {
    version: u32,
    pub command: GatewayCommand,
}

impl GatewayRequest {
    pub fn new(command: GatewayCommand) -> Result<Self, ApplicationError> {
        command.validate()?;
        Ok(Self {
            version: GATEWAY_PROTOCOL_VERSION,
            command,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, ApplicationError> {
        self.validate()?;
        let data = serde_json::to_vec(self).map_err(ApplicationError::storage)?;
        if data.len() > MAX_GATEWAY_FRAME_BYTES {
            return Err(ApplicationError::Invalid(
                "gateway request is too large".to_owned(),
            ));
        }
        Ok(data)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ApplicationError> {
        if bytes.len() > MAX_GATEWAY_FRAME_BYTES {
            return Err(ApplicationError::Invalid(
                "gateway request is too large".to_owned(),
            ));
        }
        let request: Self = serde_json::from_slice(bytes).map_err(ApplicationError::storage)?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.version != GATEWAY_PROTOCOL_VERSION {
            return Err(ApplicationError::Invalid(
                "unsupported gateway protocol".to_owned(),
            ));
        }
        self.command.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum GatewayReply {
    Environment(Environment),
    Approval(Box<ApprovalRecord>),
    Operation(OperationReceipt),
    Output(OutputPage),
    ChildWorkspace(ChildWorkspaceReceipt),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_output_page_cursor_can_be_polled_again() {
        let request = GatewayRequest::new(GatewayCommand::Output {
            operation_id: OperationId::new("operation").unwrap(),
            cursor: OutputCursor {
                offset: 0,
                prefix_sha256: Some(
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
                ),
            },
            maximum_bytes: 65536,
        })
        .expect("the gateway returns a digest even before its first output byte");
        GatewayRequest::decode(&request.encode().unwrap()).unwrap();
    }
}
