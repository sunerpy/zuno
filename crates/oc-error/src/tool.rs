//! Failures from executing a model-requested tool.

use crate::recovery::{Recoverable, Recovery};
use crate::source::BoxSource;
use std::time::Duration;

/// A failure from executing a tool.
///
/// Every variant names the tool, because a tool failure is always reported
/// somewhere that needs to say which tool it was, and recovering that name from a
/// rendered message is the defect this crate exists to prevent.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The permission layer refused the call.
    ///
    /// Not retryable and not model-correctable: a human or a stored grant has to
    /// change before this call can proceed. How the refusal is surfaced — fed
    /// back to the model, or raised to the user — belongs to the agent layer, not
    /// here.
    #[error("tool {tool} was denied by the permission layer")]
    Denied { tool: String },

    /// The arguments failed validation before the tool ran.
    ///
    /// The cause chains from the validator so a reporter can show which field was
    /// wrong; the model can correct the call and try again.
    #[error("tool {tool} received invalid arguments")]
    InvalidArgs {
        tool: String,
        #[source]
        source: BoxSource,
    },

    /// The tool exceeded its time budget. `elapsed` is what it actually spent, so
    /// a retry policy can widen the budget instead of guessing.
    #[error("tool {tool} timed out after {elapsed:?}")]
    Timeout { tool: String, elapsed: Duration },

    /// No tool by that name is registered. The model can pick a different one.
    #[error("tool {tool} is not registered")]
    NotFound { tool: String },

    /// The tool ran and failed. The cause chains from the tool.
    ///
    /// Deliberately not retryable: the reason a tool failed is opaque at this
    /// layer. A tool that knows its failure is transient reports
    /// [`ToolError::Timeout`], or wraps a typed source its caller can inspect,
    /// rather than leaving the next layer to guess from prose.
    #[error("tool {tool} failed")]
    Failed {
        tool: String,
        #[source]
        source: BoxSource,
    },
}

impl ToolError {
    /// The name of the tool that failed.
    #[must_use]
    pub fn tool(&self) -> &str {
        match self {
            Self::Denied { tool }
            | Self::InvalidArgs { tool, .. }
            | Self::Timeout { tool, .. }
            | Self::NotFound { tool }
            | Self::Failed { tool, .. } => tool,
        }
    }

    /// The action this failure calls for.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// True when running the identical call again may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        Recoverable::recovery(self).is_retry()
    }

    /// True when the model can fix this itself by issuing a corrected call.
    ///
    /// Bad arguments and a wrong tool name are both the model's mistake and both
    /// recoverable by handing the error back as a tool result.
    /// [`ToolError::Denied`] is excluded on purpose: it needs a grant, not a
    /// better call.
    #[must_use]
    pub fn is_model_correctable(&self) -> bool {
        match self {
            Self::InvalidArgs { .. } | Self::NotFound { .. } => true,
            Self::Denied { .. } | Self::Timeout { .. } | Self::Failed { .. } => false,
        }
    }
}

impl Recoverable for ToolError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Timeout { .. } => Recovery::Retry { after: None },
            Self::Denied { .. }
            | Self::InvalidArgs { .. }
            | Self::NotFound { .. }
            | Self::Failed { .. } => Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<ToolError> {
        vec![
            ToolError::Denied {
                tool: "bash".to_owned(),
            },
            ToolError::InvalidArgs {
                tool: "bash".to_owned(),
                source: Box::new(std::io::Error::other("missing field `command`")),
            },
            ToolError::Timeout {
                tool: "bash".to_owned(),
                elapsed: Duration::from_secs(120),
            },
            ToolError::NotFound {
                tool: "bash".to_owned(),
            },
            ToolError::Failed {
                tool: "bash".to_owned(),
                source: Box::new(std::io::Error::other("exit status 1")),
            },
        ]
    }

    #[test]
    fn every_variant_names_its_tool() {
        for e in every_variant() {
            assert_eq!(e.tool(), "bash", "{e}");
        }
    }

    #[test]
    fn only_timeout_is_retryable() {
        for e in every_variant() {
            let expected = matches!(e, ToolError::Timeout { .. });
            assert_eq!(e.is_retryable(), expected, "{e}");
        }
    }

    #[test]
    fn timeout_carries_the_budget_it_actually_spent() {
        let e = ToolError::Timeout {
            tool: "bash".to_owned(),
            elapsed: Duration::from_secs(120),
        };
        let ToolError::Timeout { elapsed, .. } = e else {
            panic!("constructed a Timeout, matched something else");
        };
        assert_eq!(elapsed, Duration::from_secs(120));
    }

    #[test]
    fn invalid_args_and_not_found_are_model_correctable() {
        for e in every_variant() {
            let expected = matches!(
                e,
                ToolError::InvalidArgs { .. } | ToolError::NotFound { .. }
            );
            assert_eq!(e.is_model_correctable(), expected, "{e}");
        }
    }

    #[test]
    fn denial_is_neither_retryable_nor_model_correctable() {
        let e = ToolError::Denied {
            tool: "write".to_owned(),
        };
        assert!(!e.is_retryable());
        assert!(!e.is_model_correctable());
        assert_eq!(e.recovery(), Recovery::Fail);
    }

    #[test]
    fn failures_chain_their_cause() {
        use std::error::Error as _;

        let e = ToolError::Failed {
            tool: "bash".to_owned(),
            source: Box::new(std::io::Error::other("exit status 1")),
        };
        assert_eq!(e.to_string(), "tool bash failed");
        assert_eq!(
            e.source().map(ToString::to_string).as_deref(),
            Some("exit status 1")
        );
    }
}
