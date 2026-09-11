//! Cancellation, deadlines and lease authority for isolated model work.

use crate::{
    ExtractionRequest, LearningExtraction, LearningExtractor, LearningScheduler,
    LearningServiceError,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use zuno_db::learning_job::LearningLease;

pub enum LearningAttempt<T = LearningExtraction> {
    Finished(crate::Result<T>),
    Cancelled,
    LeaseLost,
}

/// A cancelled manual command must not leave a live lease or restart in the
/// automatic worker. Normal settlement makes the token unusable, so Drop is a no-op.
pub struct ManualReflectionGuard {
    scheduler: LearningScheduler,
    job_id: String,
    lease: LearningLease,
}

impl ManualReflectionGuard {
    pub fn new(scheduler: LearningScheduler, job_id: String, lease: LearningLease) -> Self {
        Self {
            scheduler,
            job_id,
            lease,
        }
    }
}

impl Drop for ManualReflectionGuard {
    fn drop(&mut self) {
        let _ = self.scheduler.skip(
            &self.job_id,
            &self.lease,
            "manual reflection interrupted",
            zuno_db::message::now_millis(),
        );
    }
}

/// Keep a claimed extraction bounded and stop as soon as its authority is lost.
pub async fn run_claimed_extraction(
    extractor: Arc<dyn LearningExtractor>,
    request: ExtractionRequest,
    scheduler: &LearningScheduler,
    job_id: &str,
    lease: &LearningLease,
    cancel: &CancellationToken,
) -> LearningAttempt {
    let work = async {
        let request =
            prepare_extraction_request(extractor.as_ref(), request, scheduler, job_id, lease)?;
        extractor.extract(request).await
    };
    run_with_lease(work, scheduler, job_id, lease, cancel).await
}

pub(crate) fn prepare_extraction_request(
    extractor: &dyn LearningExtractor,
    request: ExtractionRequest,
    scheduler: &LearningScheduler,
    job_id: &str,
    lease: &LearningLease,
) -> crate::Result<ExtractionRequest> {
    let prepared = extractor.prepare_request(request)?;
    if prepared.sources.is_empty() {
        return Err(crate::model::invalid(
            "learning extraction has no verifiable closed sources",
        ));
    }
    let job = scheduler.get(job_id)?;
    let mut payload = crate::decode_extraction_job_payload(
        job.payload
            .ok_or_else(|| crate::model::invalid("extraction payload is missing"))?,
    )
    .map_err(|error| crate::model::invalid(&format!("corrupt extraction input: {error}")))?;
    payload.request = prepared.clone();
    if let Some(snapshot) = &mut payload.source_snapshot {
        snapshot.manifest_digest =
            zuno_db::learning_source::source_manifest_digest(&prepared.sources);
    } else {
        payload.source_snapshot = Some(scheduler.capture_snapshot(&prepared, payload.trigger)?);
    }
    scheduler.refresh_legacy_input(
        job_id,
        lease,
        &serde_json::to_value(payload).expect("input"),
        zuno_db::message::now_millis(),
    )?;
    Ok(prepared)
}

pub async fn run_with_lease<T>(
    work: impl std::future::Future<Output = crate::Result<T>>,
    scheduler: &LearningScheduler,
    job_id: &str,
    lease: &LearningLease,
    cancel: &CancellationToken,
) -> LearningAttempt<T> {
    let timeout = scheduler.execution_timeout();
    let extraction = work;
    tokio::pin!(extraction);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return LearningAttempt::Cancelled,
            () = &mut deadline => return LearningAttempt::Finished(Err(
                LearningServiceError::Extractor {
                    version: crate::LEARNING_EXTRACTOR_VERSION.to_owned(),
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut, "learning request reached its total deadline",
                    )),
                },
            )),
            _ = heartbeat.tick() => {
                let now = zuno_db::message::now_millis();
                match scheduler.heartbeat(job_id, lease, now, now.saturating_add(3_600_000)) {
                    Ok(true) => {}
                    Ok(false) => return LearningAttempt::LeaseLost,
                    Err(error) => return LearningAttempt::Finished(Err(error)),
                }
            }
            result = &mut extraction => return LearningAttempt::Finished(result),
        }
    }
}
