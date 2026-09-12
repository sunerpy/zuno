//! Foreground orchestration may suspend its original tool invocation. Ordinary
//! tool runners require a completed result; durable dispatchers preserve waits.

use zuno_error::ToolError;
use zuno_tool::ToolContext;
use zuno_types::wait::{WaitRef, WaitTarget};

#[derive(Debug, Clone, PartialEq)]
pub enum OrchestrationDispatch<T> {
    Ready(T),
    Pending(WaitRef),
}

impl<T> OrchestrationDispatch<T> {
    pub fn into_ready(self, tool: &str) -> Result<T, ToolError> {
        match self {
            Self::Ready(value) => Ok(value),
            Self::Pending(_) => Err(failed(
                tool,
                "pending orchestration requires a durable tool dispatcher",
            )),
        }
    }
}

pub(crate) fn validate_pending(
    tool: &str,
    ctx: &ToolContext,
    background: bool,
    reference: &WaitRef,
) -> Result<(), ToolError> {
    reference
        .validate()
        .map_err(|detail| failed(tool, detail))?;
    if background
        || reference.invocation_id.as_str() != ctx.permission_origin().call_id()
        || !matches!(reference.target, WaitTarget::Child { .. })
        || ctx
            .orchestration_snapshot()
            .is_some_and(|snapshot| snapshot.turn_id != reference.turn_id.as_str())
    {
        return Err(failed(
            tool,
            "orchestration wait does not belong to this foreground invocation",
        ));
    }
    Ok(())
}

fn failed(tool: &str, detail: &str) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(std::io::Error::other(detail.to_owned())),
    }
}
