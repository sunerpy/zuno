//! Host-owned durable human interaction. Model tools publish; clients respond.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use zuno_error::DbError;
use zuno_types::question::{
    QuestionCommand, QuestionReceipt, QuestionSpec, QuestionState, QuestionValidationError,
    QuestionView,
};

use crate::InterruptHandle;

pub type QuestionResult<T> = Result<T, QuestionError>;

/// Typed errors shared by the store, service, tools, and client adapters.
#[derive(Debug, Error)]
pub enum QuestionError {
    #[error("invalid question: {0}")]
    Invalid(String),
    #[error("question `{request_id}` does not exist in session `{session_id}`")]
    NotFound {
        session_id: String,
        request_id: String,
    },
    #[error("question `{request_id}` changed: expected revision {expected}, found {actual}")]
    Conflict {
        request_id: String,
        expected: i64,
        actual: i64,
    },
    #[error("command `{command_id}` was already used for different question input")]
    CommandConflict { command_id: String },
    #[error("question `{request_id}` is already {state:?}")]
    Closed {
        request_id: String,
        state: QuestionState,
    },
    #[error("question interaction is unavailable: {0}")]
    Unavailable(String),
    #[error("{code}: {detail}")]
    Rejected { code: &'static str, detail: String },
    #[error("question wait was interrupted; the durable request remains unanswered")]
    Interrupted,
    #[error(transparent)]
    Database(#[from] DbError),
}

impl From<QuestionValidationError> for QuestionError {
    fn from(error: QuestionValidationError) -> Self {
        Self::Invalid(error.0)
    }
}

/// One interface for tools and clients; only `open` is exposed as a model tool.
///
/// All successful writes are durable before returning. `wait_for_change` is a
/// notification adapter and never owns the lifetime of the stored request.
#[async_trait]
pub trait QuestionPort: Send + Sync {
    async fn open(&self, spec: QuestionSpec) -> QuestionResult<QuestionReceipt>;

    async fn apply(
        &self,
        session_id: &str,
        request_id: &str,
        command: QuestionCommand,
    ) -> QuestionResult<QuestionReceipt>;

    async fn get(&self, session_id: &str, request_id: &str) -> QuestionResult<QuestionView>;

    async fn pending(&self, session_id: &str) -> QuestionResult<Vec<QuestionView>>;

    async fn wait_for_change(
        &self,
        session_id: &str,
        request_id: &str,
        after_revision: i64,
        interrupt: Arc<dyn InterruptHandle>,
    ) -> QuestionResult<QuestionView>;
}
