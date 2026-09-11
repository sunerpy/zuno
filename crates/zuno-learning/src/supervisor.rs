//! Process-owned scheduling for minimal project learning executors.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Executes a bounded project wake using services independent of a foreground host.
#[async_trait]
pub trait LearningWork: Send + Sync + 'static {
    /// Implementations must observe cancellation while waiting and settle their leases.
    async fn tick(&self, cancel: CancellationToken);
}

#[derive(Clone)]
struct Binding {
    work: Arc<dyn LearningWork>,
    interval: Duration,
}

struct Worker {
    binding: watch::Sender<Binding>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

struct Inner {
    workers: Mutex<BTreeMap<String, Worker>>,
    cancel: CancellationToken,
    permits: Arc<Semaphore>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
        let workers = self
            .workers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, worker) in std::mem::take(workers) {
            worker.task.abort();
        }
    }
}

/// Owns learning work until process shutdown, including while ACP sessions sleep.
#[derive(Clone)]
pub struct LearningSupervisor {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LearningSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LearningSupervisor")
            .field("projects", &self.project_count())
            .field("cancelled", &self.inner.cancel.is_cancelled())
            .finish()
    }
}

impl Default for LearningSupervisor {
    fn default() -> Self {
        Self::new(NonZeroUsize::new(2).expect("positive process learning limit"))
    }
}

impl LearningSupervisor {
    #[must_use]
    pub fn new(maximum_running: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(Inner {
                workers: Mutex::new(BTreeMap::new()),
                cancel: CancellationToken::new(),
                permits: Arc::new(Semaphore::new(maximum_running.get())),
            }),
        }
    }

    /// Publish a project binding; an in-flight wake retains its captured version.
    pub fn ensure_project(
        &self,
        project_id: String,
        work: Arc<dyn LearningWork>,
        interval: Duration,
    ) {
        if self.inner.cancel.is_cancelled() {
            return;
        }
        let binding = Binding {
            work,
            interval: interval.max(Duration::from_millis(1)),
        };
        let mut workers = self
            .inner
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(worker) = workers.get(&project_id) {
            if !worker.task.is_finished() && !worker.cancel.is_cancelled() {
                worker.binding.send_replace(binding);
                return;
            }
            // A new enabled binding cannot overlap a previously suspended task
            // that has not yet observed its cooperative cancellation.
            worker.task.abort();
        }
        let (sender, receiver) = watch::channel(binding);
        let cancel = self.inner.cancel.child_token();
        let permits = Arc::clone(&self.inner.permits);
        let task = tokio::spawn(run_project(receiver, cancel.clone(), permits));
        workers.insert(
            project_id,
            Worker {
                binding: sender,
                cancel,
                task,
            },
        );
    }

    #[must_use]
    pub fn project_count(&self) -> usize {
        self.inner
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|worker| !worker.cancel.is_cancelled() && !worker.task.is_finished())
            .count()
    }

    /// Stop the existing binding when generation is disabled or the selected
    /// model is unavailable. Keep its task owned until shutdown or replacement.
    pub fn suspend_project(&self, project_id: &str) -> bool {
        let workers = self
            .inner
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(worker) = workers.get(project_id) else {
            return false;
        };
        if worker.cancel.is_cancelled() || worker.task.is_finished() {
            return false;
        }
        worker.cancel.cancel();
        true
    }

    /// Notify only the existing project worker. No session input or foreground
    /// agent turn is created, and simultaneous notifications are coalesced.
    pub fn wake_project(&self, project_id: &str) -> bool {
        if self.inner.cancel.is_cancelled() {
            return false;
        }
        let workers = self
            .inner
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(worker) = workers.get(project_id) else {
            return false;
        };
        if worker.task.is_finished() || worker.cancel.is_cancelled() {
            return false;
        }
        worker.binding.send_modify(|_| {});
        true
    }

    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn cancel(&self) {
        self.inner.cancel.cancel();
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.cancel.child_token()
    }

    /// Cooperate first, then abort a non-cooperating executor at the stop deadline.
    pub async fn shutdown(&self, grace: Duration) {
        self.cancel();
        let workers = {
            let mut workers = self
                .inner
                .workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *workers)
        };
        let deadline = tokio::time::Instant::now() + grace;
        for (_, mut worker) in workers {
            if tokio::time::timeout_at(deadline, &mut worker.task)
                .await
                .is_err()
            {
                worker.task.abort();
                let _ = worker.task.await;
            }
        }
    }
}

async fn run_project(
    mut receiver: watch::Receiver<Binding>,
    cancel: CancellationToken,
    permits: Arc<Semaphore>,
) {
    loop {
        let binding = receiver.borrow_and_update().clone();
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            permit = permits.acquire() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
        };
        binding.work.tick(cancel.clone()).await;
        drop(permit);
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            changed = receiver.changed() => {
                if changed.is_err() { return; }
            }
            () = tokio::time::sleep(binding.interval) => {}
        }
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
