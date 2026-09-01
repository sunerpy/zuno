use std::time::Duration;

use thiserror::Error;
use zuno_error::{DbError, ToolError};

/// Typed failures at the continuity provider boundary.
#[derive(Debug, Error)]
pub enum ContinuityError {
    /// A model-supplied cursor, name, revision, query, or action is invalid.
    #[error("{0}")]
    Invalid(String),
    /// Durable storage could not complete the operation.
    #[error(transparent)]
    Database(#[from] DbError),
    /// An internal response or cursor could not be encoded.
    #[error("continuity value could not be encoded")]
    Encoding(#[source] serde_json::Error),
    /// Profile composition could not publish the requested tool surface.
    #[error("continuity composition failed: {0}")]
    Composition(String),
}

impl ContinuityError {
    pub(crate) fn into_tool_error(self, tool: &str) -> ToolError {
        match self {
            Self::Invalid(_) => ToolError::InvalidArgs {
                tool: tool.to_owned(),
                source: Box::new(self),
            },
            Self::Database(error) if error.is_retryable() => ToolError::Transient {
                tool: tool.to_owned(),
                retry_after: error.retry_after().or(Some(Duration::from_millis(50))),
                source: Box::new(error),
            },
            Self::Database(error) => ToolError::Failed {
                tool: tool.to_owned(),
                source: Box::new(error),
            },
            Self::Encoding(_) | Self::Composition(_) => ToolError::Failed {
                tool: tool.to_owned(),
                source: Box::new(self),
            },
        }
    }
}
