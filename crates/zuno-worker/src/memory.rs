//! Async, Job-bound Memory transport and shared-kernel context projection.

mod tools;
pub use tools::MemoryToolDispatcher;

use super::*;
use zuno_engine::r#loop::{DynamicContextRefresher, TurnError};
use zuno_llm::cache::DynamicContext;
use zuno_memory::remote::MemoryCommand;
use zuno_memory::{
    MemoryServiceError,
    remote::{MemoryDataService, MemoryReply, MemoryRequest, MemoryResponse},
};
use zuno_tool::ToolDynamicContextRefresh;
use zuno_types::identity::RequestId;

#[derive(Clone)]
pub struct RemoteMemoryService {
    client: WorkerClient,
    execution: WorkerExecution,
}
impl WorkerClient {
    pub fn memory(&self, execution: &WorkerExecution) -> RemoteMemoryService {
        RemoteMemoryService {
            client: self.clone(),
            execution: execution.clone(),
        }
    }
}

#[async_trait]
impl MemoryDataService for RemoteMemoryService {
    async fn request(&self, request: MemoryRequest) -> Result<MemoryReply, MemoryServiceError> {
        if self.execution.boundary_started()
            || self.execution.deadline().map_err(memory_transport_error)?
                <= tokio::time::Instant::now()
        {
            return Err(MemoryServiceError::Conflict);
        }
        let grant = self
            .execution
            .credential
            .read()
            .map_err(|_| MemoryServiceError::Unavailable)?
            .grant
            .clone();
        let bytes = serde_json::to_vec(&request)
            .map_err(|_| MemoryServiceError::Invalid("invalid Memory request".to_owned()))?;
        if bytes.len() > 65_536 {
            return Err(MemoryServiceError::Invalid(
                "Memory request exceeds 64 KiB".to_owned(),
            ));
        }
        let bytes = self
            .client
            .post(MEMORY_PATH, Some(&grant), bytes)
            .await
            .map_err(memory_transport_error)?;
        let response: MemoryResponse =
            serde_json::from_slice(&bytes).map_err(|_| MemoryServiceError::InvalidData)?;
        response.result.map_err(Into::into)
    }
}

fn memory_transport_error(error: TurnStateError) -> MemoryServiceError {
    match error {
        TurnStateError::Forbidden => MemoryServiceError::Denied,
        TurnStateError::InvalidData => MemoryServiceError::InvalidData,
        TurnStateError::Conflict | TurnStateError::LeaseLost => MemoryServiceError::Conflict,
        _ => MemoryServiceError::Unavailable,
    }
}

/// Reconstruct volatile context from authoritative state before every provider
/// request, including requests resumed from another Worker's checkpoint.
pub struct MemoryContextRefresher {
    pub service: Arc<dyn MemoryDataService>,
    pub session_id: String,
    pub base: DynamicContext,
}
impl MemoryContextRefresher {
    async fn load(&self, session: &str) -> Result<DynamicContext, TurnError> {
        if session != self.session_id {
            return Err(TurnStateError::Forbidden.into());
        }
        let reply = self
            .service
            .request(MemoryRequest {
                request_id: RequestId::new("memory-before-provider").expect("static identity"),
                command: MemoryCommand::Read,
            })
            .await
            .map_err(|error| match error {
                MemoryServiceError::Denied => TurnStateError::Forbidden,
                MemoryServiceError::Conflict => TurnStateError::LeaseLost,
                MemoryServiceError::Unavailable => TurnStateError::Unavailable,
                _ => TurnStateError::InvalidData,
            })?;
        let MemoryReply::Snapshot { documents } = reply else {
            return Err(TurnStateError::InvalidData.into());
        };
        // Empty content deliberately replaces any previous Memory rather than
        // leaving a withdrawn snapshot in a recovered checkpoint.
        Ok(self.base.clone().with_memory(
            documents
                .into_iter()
                .map(|document| document.content)
                .collect::<Vec<_>>()
                .join("\n\n"),
        ))
    }
}
#[async_trait]
impl DynamicContextRefresher for MemoryContextRefresher {
    async fn before_request(&self, session: &str) -> Result<Option<DynamicContext>, TurnError> {
        self.load(session).await.map(Some)
    }
    async fn refresh(
        &self,
        session: &str,
        _refresh: ToolDynamicContextRefresh,
    ) -> Result<DynamicContext, TurnError> {
        self.load(session).await
    }
}
