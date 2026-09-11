//! Durable waiting coordinates. They name facts; they grant no authority.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::identity::{ApprovalId, InvocationId, JobId, OperationId, RequestId, TurnId, WaitId};

/// Scheduling and UI can distinguish causes without inspecting tool names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitTarget {
    Approval { approval_id: ApprovalId },
    UserInput { request_id: RequestId },
    Child { job_id: JobId },
    Operation { operation_id: OperationId },
    Timer { deadline_ms: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitContinuation {
    CurrentTurn,
    NextTurn,
}

/// Bound to the original invocation and arguments, never to an execution attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WaitRef {
    pub id: WaitId,
    pub turn_id: TurnId,
    pub invocation_id: InvocationId,
    pub arguments_sha256: String,
    pub target: WaitTarget,
    pub continuation: WaitContinuation,
}

impl WaitRef {
    /// Identity constructors validate IDs; validate this relation at wire/storage
    /// boundaries before trusting it as a checkpoint dependency.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.arguments_sha256.len() != 64
            || !self
                .arguments_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("wait arguments require a lowercase SHA-256 digest");
        }
        if matches!(self.target, WaitTarget::Timer { deadline_ms } if deadline_ms <= 0) {
            return Err("a timer requires a positive durable deadline");
        }
        Ok(())
    }
}
