use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

struct Work {
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
}

#[async_trait]
impl LearningWork for Work {
    async fn tick(&self, _cancel: CancellationToken) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
    }
}

#[tokio::test(start_paused = true)]
async fn project_work_survives_the_registering_session_scope() {
    let supervisor = LearningSupervisor::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    {
        let session_view = supervisor.clone();
        session_view.ensure_project(
            "project".to_owned(),
            Arc::new(Work {
                calls: Arc::clone(&calls),
                started: Arc::clone(&started),
            }),
            Duration::from_millis(10),
        );
    }
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("first wake");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("later wake");
    assert!(calls.load(Ordering::SeqCst) >= 2);
    assert_eq!(supervisor.project_count(), 1);
    supervisor.shutdown(Duration::from_secs(1)).await;
    assert_eq!(supervisor.project_count(), 0);
}

struct HeldWork {
    started: Arc<Notify>,
    stopped: Arc<AtomicUsize>,
}

#[async_trait]
impl LearningWork for HeldWork {
    async fn tick(&self, cancel: CancellationToken) {
        self.started.notify_one();
        cancel.cancelled().await;
        self.stopped.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_propagates_cancellation_to_in_flight_work() {
    let supervisor = LearningSupervisor::default();
    let started = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicUsize::new(0));
    supervisor.ensure_project(
        "project".to_owned(),
        Arc::new(HeldWork {
            started: Arc::clone(&started),
            stopped: Arc::clone(&stopped),
        }),
        Duration::from_secs(60),
    );
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("wake");
    supervisor.shutdown(Duration::from_secs(1)).await;
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
}

struct DropWitness(Arc<AtomicUsize>);
impl Drop for DropWitness {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct UncooperativeWork {
    started: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}

#[async_trait]
impl LearningWork for UncooperativeWork {
    async fn tick(&self, _cancel: CancellationToken) {
        let _witness = DropWitness(Arc::clone(&self.dropped));
        self.started.notify_one();
        std::future::pending::<()>().await;
    }
}

#[tokio::test(start_paused = true)]
async fn stop_deadline_aborts_and_joins_uncooperative_work() {
    let supervisor = LearningSupervisor::default();
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    supervisor.ensure_project(
        "project".to_owned(),
        Arc::new(UncooperativeWork {
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
        }),
        Duration::from_secs(60),
    );
    started.notified().await;
    supervisor.shutdown(Duration::from_millis(10)).await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(supervisor.project_count(), 0);
}
