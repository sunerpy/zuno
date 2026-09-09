//! Frontend adapter for the process-owned project learning service.

use super::{AtomicBool, Ordering, SessionRunRegistry, redact_learning_text};
use async_trait::async_trait;
use zuno_learning::{LearningIngestion, ProjectLearningService};

pub(super) struct ProjectLearningWork {
    pub service: ProjectLearningService,
    pub ingestion: LearningIngestion,
    pub credential: Option<String>,
    pub catch_up_pending: AtomicBool,
    pub owner_id: String,
    pub changes: crate::cmd::child_turn::ChangeNotifier,
    pub runs: SessionRunRegistry,
    pub max_jobs: u32,
}

#[async_trait]
impl zuno_learning::LearningWork for ProjectLearningWork {
    async fn tick(&self, cancel: tokio_util::sync::CancellationToken) {
        let now = zuno_db::message::now_millis();
        if self.catch_up_pending.swap(false, Ordering::AcqRel) {
            match self.ingestion.catch_up(
                &self.service.project_id,
                &self.service.scheduler,
                now,
                &|text| redact_learning_text(text, self.credential.as_deref()),
            ) {
                Ok(0) => {}
                Ok(_) => self.changes.changed(),
                Err(error) => {
                    self.catch_up_pending.store(true, Ordering::Release);
                    tracing::warn!(%error,"learning startup catch-up failed");
                }
            }
        }
        if let Err(error) = self.service.scheduler.reconcile_expired(now) {
            tracing::warn!(%error,"learning lease recovery failed");
            return;
        }
        match self.service.schedule_maintenance(now) {
            Ok(true) => self.changes.changed(),
            Ok(false) => {}
            Err(error) => tracing::warn!(%error,"learning maintenance admission failed"),
        }
        for _ in 0..self.max_jobs {
            if cancel.is_cancelled() {
                break;
            }
            let busy = self.runs.active_sessions().into_iter().collect::<Vec<_>>();
            match self.service.claim(&self.owner_id, &busy) {
                Ok(Some(job)) => {
                    let job_id = job.id.clone();
                    let lease = job.lease().ok();
                    let job = match self.ingestion.upgrade_legacy_job(
                        job,
                        &self.service.scheduler,
                        &|text| redact_learning_text(text, self.credential.as_deref()),
                    ) {
                        Ok(job) => job,
                        Err(error) => {
                            if let Some(lease) = lease
                                && let Err(settle_error) =
                                    self.service.settle_failure(&job_id, &lease, &error)
                            {
                                tracing::warn!(%settle_error,"learning source failure could not be settled");
                            }
                            tracing::warn!(%error,"learning source refresh failed");
                            self.changes.changed();
                            continue;
                        }
                    };
                    if let Err(error) = self.service.execute(job, &cancel).await {
                        tracing::warn!(%error,"learning job failed");
                    }
                    self.changes.changed();
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(%error,"learning queue claim failed");
                    break;
                }
            }
        }
    }
}
