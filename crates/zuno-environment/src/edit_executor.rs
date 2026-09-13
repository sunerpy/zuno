//! Gateway-owned edit execution outlives individual Worker requests.
use crate::DockerGateway;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::{sync::Semaphore, task::JoinSet};
use zuno_application::{
    ApplicationError,
    workspace_edit::{WorkspaceEditCompletionSink, WorkspaceEditState},
};

struct Tasks {
    running: BTreeMap<tokio::task::Id, String>,
    jobs: JoinSet<()>,
}
pub struct EditExecutor {
    gateway: Arc<DockerGateway>,
    sink: Arc<dyn WorkspaceEditCompletionSink>,
    slots: Arc<Semaphore>,
    tasks: Mutex<Tasks>,
}
impl EditExecutor {
    pub fn new(
        gateway: Arc<DockerGateway>,
        sink: Arc<dyn WorkspaceEditCompletionSink>,
        parallelism: u32,
    ) -> Result<Self, ApplicationError> {
        if !(1..=16).contains(&parallelism) {
            return Err(ApplicationError::Invalid(
                "invalid edit parallelism".to_owned(),
            ));
        }
        Ok(Self {
            gateway,
            sink,
            slots: Arc::new(Semaphore::new(parallelism as usize)),
            tasks: Mutex::new(Tasks {
                running: BTreeMap::new(),
                jobs: JoinSet::new(),
            }),
        })
    }
    pub fn advance(&self) -> Result<u32, ApplicationError> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        while let Some(joined) = tasks.jobs.try_join_next_with_id() {
            let id = match joined {
                Ok((id, ())) => id,
                Err(error) => error.id(),
            };
            tasks.running.remove(&id);
        }
        if self.slots.available_permits() == 0 {
            return Ok(0);
        }
        let mut count = 0;
        for record in self.gateway.scan_workspace_edits(32)? {
            let key = zuno_orchestration::sha256_json(&serde_json::json!([
                record.admission.lease.owner,
                record.admission.operation.id
            ]));
            if tasks.running.values().any(|value| value == &key) {
                continue;
            }
            let Ok(slot) = self.slots.clone().try_acquire_owned() else {
                break;
            };
            let gateway = self.gateway.clone();
            let sink = self.sink.clone();
            let task = tasks.jobs.spawn(async move {
                let _slot = slot;
                let owner = &record.admission.lease.owner;
                let id = &record.admission.operation.id;
                if record.state == WorkspaceEditState::Preparing
                    && let Err(error) = gateway.apply_workspace_edit(&record.admission).await
                {
                    if matches!(
                        error,
                        ApplicationError::Invalid(_)
                            | ApplicationError::Conflict
                            | ApplicationError::Forbidden
                            | ApplicationError::NotFound
                    ) {
                        if gateway.cancel_workspace_edit(owner, id).is_err() {
                            let _ = gateway.defer_workspace_edit(owner, id);
                            return;
                        }
                    } else {
                        let _ = gateway.defer_workspace_edit(owner, id);
                        return;
                    }
                }
                match gateway.workspace_edit_completion(owner, id) {
                    Ok(Some(completion)) if sink.publish_edit(&completion).await.is_ok() => {
                        if gateway.acknowledge_workspace_edit(&completion).is_err() {
                            let _ = gateway.defer_workspace_edit(owner, id);
                        }
                    }
                    _ => {
                        let _ = gateway.defer_workspace_edit(owner, id);
                    }
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
