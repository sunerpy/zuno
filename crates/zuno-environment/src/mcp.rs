//! Durable external MCP execution. A running operation without a recorded
//! result becomes uncertain after restart and is never sent a second time.
mod journal;
use journal::{Journal, Record};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::{sync::Semaphore, task::JoinSet};
use zuno_application::{
    ApplicationError,
    mcp::{
        McpAdmission, McpCompletionSink, McpConnectionProvider, McpFailure, McpOperationAuthority,
        McpOperationState, McpReceipt,
    },
};
use zuno_types::identity::{OperationId, PrincipalKey};

struct Tasks {
    running: BTreeMap<tokio::task::Id, String>,
    jobs: JoinSet<()>,
}
pub struct McpExecutor {
    journal: Arc<Journal>,
    authority: Arc<dyn McpOperationAuthority>,
    provider: Arc<dyn McpConnectionProvider>,
    sink: Arc<dyn McpCompletionSink>,
    slots: Arc<Semaphore>,
    tasks: Mutex<Tasks>,
}
impl McpExecutor {
    pub fn open(
        path: &Path,
        authority: Arc<dyn McpOperationAuthority>,
        provider: Arc<dyn McpConnectionProvider>,
        sink: Arc<dyn McpCompletionSink>,
        parallelism: u32,
    ) -> Result<Self, ApplicationError> {
        if !(1..=16).contains(&parallelism) {
            return Err(ApplicationError::Invalid(
                "invalid MCP parallelism".to_owned(),
            ));
        }
        Ok(Self {
            journal: Arc::new(Journal::open(path)?),
            authority,
            provider,
            sink,
            slots: Arc::new(Semaphore::new(parallelism as usize)),
            tasks: Mutex::new(Tasks {
                running: BTreeMap::new(),
                jobs: JoinSet::new(),
            }),
        })
    }
    pub async fn submit(&self, admission: &McpAdmission) -> Result<McpReceipt, ApplicationError> {
        admission.validate()?;
        self.authority.authorize_mcp(admission).await?;
        self.journal.admit(admission, false)
    }
    pub fn receipt(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<McpReceipt, ApplicationError> {
        Ok(self.journal.get(owner, id)?.receipt)
    }
    pub fn receipt_for(
        &self,
        lease: &zuno_application::runtime::ExecutionLease,
        environment: &zuno_types::identity::EnvironmentId,
        id: &OperationId,
    ) -> Result<McpReceipt, ApplicationError> {
        let record = self.journal.get(&lease.owner, id)?;
        if record.admission.lease.job_id != lease.job_id
            || record.admission.lease.session_id != lease.session_id
            || record.admission.operation.environment_id != *environment
        {
            return Err(ApplicationError::Forbidden);
        }
        Ok(record.receipt)
    }
    pub fn cancel(&self, admission: &McpAdmission) -> Result<(), ApplicationError> {
        self.journal.admit(admission, true)?;
        Ok(())
    }
    pub fn advance(&self) -> Result<u32, ApplicationError> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        while let Some(result) = tasks.jobs.try_join_next_with_id() {
            let id = match result {
                Ok((id, ())) => id,
                Err(error) => error.id(),
            };
            if let Some(key) = tasks.running.remove(&id) {
                // A panic/cancel after the durable Running transition has the
                // same uncertainty as a process loss. Never replay it.
                self.journal.recover_key(&key)?;
            }
        }
        if self.slots.available_permits() == 0 {
            return Ok(0);
        }
        let mut count = 0;
        for record in self.journal.scan(32)? {
            let key = record.key();
            if tasks.running.values().any(|value| value == &key) {
                continue;
            }
            let Ok(slot) = self.slots.clone().try_acquire_owned() else {
                break;
            };
            let journal = self.journal.clone();
            let authority = self.authority.clone();
            let provider = self.provider.clone();
            let sink = self.sink.clone();
            let task = tasks.jobs.spawn(async move {
                let _slot = slot;
                let result =
                    execute(&journal, authority.as_ref(), provider.as_ref(), &record).await;
                if result.is_err() {
                    let _ = journal.recover_key(&record.key());
                }
                let result = async {
                    let current = journal.get(
                        &record.admission.lease.owner,
                        &record.admission.operation.id,
                    )?;
                    if !current.receipt.state.terminal() {
                        return Ok(());
                    }
                    let completion = current.completion();
                    completion.validate()?;
                    sink.publish_mcp(&completion).await?;
                    journal.acknowledge(&current)
                }
                .await;
                if result.is_err() {
                    let _ = journal.defer(&record.key());
                }
            });
            tasks.running.insert(task.id(), key);
            count += 1;
        }
        Ok(count)
    }
    pub async fn drain(&self, timeout: std::time::Duration) {
        self.slots.close();
        let mut jobs = if let Ok(mut tasks) = self.tasks.lock() {
            tasks.running.clear();
            std::mem::replace(&mut tasks.jobs, JoinSet::new())
        } else {
            return;
        };
        if tokio::time::timeout(timeout, async { while jobs.join_next().await.is_some() {} })
            .await
            .is_err()
        {
            jobs.shutdown().await;
        }
    }
}
async fn execute(
    journal: &Journal,
    authority: &dyn McpOperationAuthority,
    provider: &dyn McpConnectionProvider,
    record: &Record,
) -> Result<(), ApplicationError> {
    if record.receipt.state != McpOperationState::Queued {
        return Ok(());
    }
    let prepared = match provider
        .prepare(
            &record.admission.lease.owner,
            &record.admission.operation.binding,
        )
        .await
    {
        Ok(prepared) => prepared,
        Err(failure) => {
            return journal.finish(record, McpOperationState::Failed, None, Some(failure));
        }
    };
    if let Err(error) = authority.check_admitted_mcp(&record.admission).await {
        if matches!(
            error,
            ApplicationError::Unavailable | ApplicationError::Storage { .. }
        ) {
            journal.defer(&record.key())?;
            return Ok(());
        }
        return journal.finish(
            record,
            McpOperationState::Cancelled,
            None,
            Some(McpFailure::AuthorizationRevoked),
        );
    }
    if !journal.start(record)? {
        return Ok(());
    }
    match prepared.call(&record.admission.operation.arguments).await {
        Ok(value) => {
            let state = if value.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
                McpOperationState::Failed
            } else {
                McpOperationState::Succeeded
            };
            journal.finish(record, state, Some(value), None)
        }
        Err(failure) => journal.finish(record, McpOperationState::Uncertain, None, Some(failure)),
    }
}
