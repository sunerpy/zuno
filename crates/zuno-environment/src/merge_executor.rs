//! Bounded gateway-owned merge work. The ledger survives process loss; these
//! futures are only execution capacity and never cross the Worker protocol.
use crate::{DockerGateway, ledger::MergeRecord};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::{sync::Semaphore, task::JoinSet};
use zuno_application::{
    ApplicationError,
    environment::Environment,
    runtime::ExecutionLease,
    workspace_merge::{
        WorkspaceMergeAuthority, WorkspaceMergeCompletionSink, WorkspaceMergeOperation,
        WorkspaceMergeState,
    },
};

struct Admitted(MergeRecord);
#[async_trait::async_trait]
impl WorkspaceMergeAuthority for Admitted {
    async fn authorize_merge(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        operation: &WorkspaceMergeOperation,
    ) -> Result<(), ApplicationError> {
        if *lease != self.0.lease
            || *operation != self.0.request
            || environment.owner != lease.owner
            || environment.spec.session_id != lease.session_id
            || environment.spec.id != operation.environment_id
            || environment.revision != operation.expected_revision
        {
            return Err(ApplicationError::Forbidden);
        }
        Ok(())
    }
}
struct Tasks {
    running: BTreeMap<tokio::task::Id, String>,
    jobs: JoinSet<()>,
}
pub struct MergeExecutor {
    gateway: Arc<DockerGateway>,
    sink: Arc<dyn WorkspaceMergeCompletionSink>,
    slots: Arc<Semaphore>,
    tasks: Mutex<Tasks>,
}
impl MergeExecutor {
    pub fn new(
        gateway: Arc<DockerGateway>,
        sink: Arc<dyn WorkspaceMergeCompletionSink>,
        parallelism: u32,
    ) -> Result<Self, ApplicationError> {
        if !(1..=16).contains(&parallelism) {
            return Err(ApplicationError::Invalid(
                "merge parallelism must be between 1 and 16".to_owned(),
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
        let records = self.gateway.scan_workspace_merges(32)?;
        let mut started = 0;
        for record in records {
            let key = zuno_orchestration::sha256_json(&serde_json::json!([
                record.lease.owner,
                record.request.id
            ]));
            if tasks.running.values().any(|active| active == &key) {
                continue;
            }
            let Ok(slot) = self.slots.clone().try_acquire_owned() else {
                break;
            };
            let gateway = self.gateway.clone();
            let sink = self.sink.clone();
            let task = tasks.jobs.spawn(async move {
                let _slot = slot;
                let owner = &record.lease.owner;
                let id = &record.request.id;
                if record.state == WorkspaceMergeState::Preparing {
                    let result = gateway
                        .merge_workspace(&record.lease, &record.request, &Admitted(record.clone()))
                        .await;
                    if let Err(error) = result {
                        if matches!(
                            error,
                            ApplicationError::Invalid(_)
                                | ApplicationError::Conflict
                                | ApplicationError::Forbidden
                                | ApplicationError::NotFound
                        ) {
                            // Cancellation only clears a proven unpublished
                            // reservation; an already committed receipt wins.
                            if gateway.cancel_workspace_merge(owner, id).is_err() {
                                let _ = gateway.defer_workspace_merge(owner, id);
                                return;
                            }
                        } else {
                            let _ = gateway.defer_workspace_merge(owner, id);
                            return;
                        }
                    }
                }
                match gateway.workspace_merge_completion(owner, id) {
                    Ok(Some(completion)) if sink.publish_merge(&completion).await.is_ok() => {
                        if gateway.acknowledge_workspace_merge(&completion).is_err() {
                            let _ = gateway.defer_workspace_merge(owner, id);
                        }
                    }
                    _ => {
                        let _ = gateway.defer_workspace_merge(owner, id);
                    }
                }
            });
            tasks.running.insert(task.id(), key);
            started += 1;
        }
        Ok(started)
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
