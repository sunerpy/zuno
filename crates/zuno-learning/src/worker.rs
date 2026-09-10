//! Project-owned queue execution independent of TUI, HTTP or ACP session lifetimes.

use crate::{
    ExperienceService, LearningAttempt, LearningExtractor, LearningScheduleOutcome,
    LearningScheduler, PatternMiner, SkillCandidateService, decode_extraction_job_payload,
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use zuno_db::learning_job::{LearningJobKind, LearningJobRecord, LearningJobStatus};
use zuno_db::learning_pattern::PatternProposal;
use zuno_error::Recovery;

#[derive(Clone)]
pub struct ProjectLearningService {
    pub scheduler: LearningScheduler,
    pub extractor: Arc<dyn LearningExtractor>,
    pub experiences: ExperienceService,
    pub patterns: PatternMiner,
    pub skills: SkillCandidateService,
    pub memory: Option<Arc<crate::MemoryMaintainer>>,
    pub project_id: String,
    pub project_root: PathBuf,
}

impl ProjectLearningService {
    pub fn schedule_maintenance(&self, now: i64) -> crate::Result<bool> {
        let mut changed = if let Some(memory) = &self.memory {
            matches!(
                memory.schedule(&self.scheduler, now)?,
                LearningScheduleOutcome::Queued(_)
            )
        } else {
            false
        };
        changed |= matches!(
            self.scheduler
                .schedule_project_aggregation(&self.project_id, now)?,
            LearningScheduleOutcome::Queued(_)
        );
        if let Some(digest) = self.patterns.global_evidence_digest()? {
            changed |= matches!(
                self.scheduler.schedule_global_aggregation(&digest, now)?,
                LearningScheduleOutcome::Queued(_)
            );
        }
        Ok(changed)
    }

    pub fn claim(
        &self,
        owner: &str,
        busy_sessions: &[String],
    ) -> crate::Result<Option<LearningJobRecord>> {
        let now = zuno_db::message::now_millis();
        self.scheduler.claim_due_for_project_excluding(
            &self.project_id,
            owner,
            now,
            now.saturating_add(3_600_000),
            busy_sessions,
        )
    }

    pub async fn execute(
        &self,
        job: LearningJobRecord,
        cancel: &CancellationToken,
    ) -> crate::Result<()> {
        let lease = job.lease()?;
        let work = async {
            match job.kind {
                LearningJobKind::Extraction => {
                    let request = decode_extraction_job_payload(
                        job.payload
                            .clone()
                            .ok_or_else(|| crate::model::invalid("learning job has no input"))?,
                    )
                    .map_err(|error| {
                        crate::model::invalid(&format!("corrupt learning job input: {error}"))
                    })?
                    .into_request();
                    let request = crate::execution::prepare_extraction_request(
                        self.extractor.as_ref(),
                        request,
                        &self.scheduler,
                        &job.id,
                        &lease,
                    )?;
                    let extraction = self.extractor.extract(request).await?;
                    self.experiences.persist_extraction(
                        &job.id,
                        &lease,
                        extraction,
                        zuno_db::message::now_millis(),
                    )?;
                    self.schedule_maintenance(zuno_db::message::now_millis())?;
                    Ok(())
                }
                LearningJobKind::ProjectAggregation | LearningJobKind::GlobalAggregation => {
                    if let Some(purpose) = job
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("purpose"))
                    {
                        if job.kind != LearningJobKind::ProjectAggregation
                            || purpose.as_str()
                                != Some(zuno_db::memory_maintenance::MEMORY_MAINTENANCE_PURPOSE)
                        {
                            return Err(crate::model::invalid(
                                "unknown learning aggregation purpose",
                            ));
                        }
                        let memory = self.memory.as_ref().ok_or_else(|| {
                            crate::model::invalid("automatic memory maintenance is unavailable")
                        })?;
                        return memory.execute(&job, &lease, &self.scheduler).await;
                    }
                    let result = self.aggregate(&job, &lease).await?;
                    self.scheduler.complete(
                        &job.id,
                        &lease,
                        &result,
                        zuno_db::message::now_millis(),
                    )
                }
                _ => Err(crate::model::invalid(
                    "project learning worker cannot execute a side-effect job",
                )),
            }
        };
        let outcome =
            crate::execution::run_with_lease(work, &self.scheduler, &job.id, &lease, cancel).await;
        let error = match outcome {
            LearningAttempt::Finished(Ok(())) => return Ok(()),
            LearningAttempt::Finished(Err(error)) => error,
            LearningAttempt::Cancelled | LearningAttempt::LeaseLost => {
                // The token fence refuses this release if another worker already owns it.
                let _ = self.scheduler.retry(
                    &job.id,
                    &lease,
                    "learning execution interrupted",
                    Some(Duration::from_secs(1)),
                    zuno_db::message::now_millis(),
                );
                return Ok(());
            }
        };
        self.settle_failure(&job.id, &lease, &error)?;
        Err(error)
    }

    pub fn settle_failure(
        &self,
        job_id: &str,
        lease: &zuno_db::learning_job::LearningLease,
        error: &crate::LearningServiceError,
    ) -> crate::Result<()> {
        let current = self.scheduler.get(job_id)?;
        if current.status == LearningJobStatus::Running
            && current.lease().as_ref().ok() == Some(lease)
        {
            let now = zuno_db::message::now_millis();
            match error.recovery() {
                Recovery::Retry { after } => {
                    self.scheduler
                        .retry(job_id, lease, &error.to_string(), after, now)?;
                }
                Recovery::Reauthenticate | Recovery::Compact | Recovery::Fail => {
                    self.scheduler
                        .fail(job_id, lease, &error.to_string(), now)?;
                }
            }
        }
        Ok(())
    }

    async fn aggregate(
        &self,
        job: &LearningJobRecord,
        lease: &zuno_db::learning_job::LearningLease,
    ) -> crate::Result<Value> {
        let now = zuno_db::message::now_millis();
        let proposals = if job.kind == LearningJobKind::ProjectAggregation {
            if job.project_id.as_deref() != Some(&self.project_id) {
                return Err(crate::model::invalid(
                    "aggregation belongs to another project",
                ));
            }
            let since = job
                .payload
                .as_ref()
                .and_then(|value| value.get("since"))
                .and_then(Value::as_i64)
                .ok_or_else(|| crate::model::invalid("aggregation has no since boundary"))?;
            self.patterns
                .mine_project_claimed(&self.project_id, since, now, &job.id, lease)
                .await?
        } else {
            self.patterns
                .mine_global_claimed(now, &job.id, lease)
                .await?
        };
        let mut patterns = Vec::new();
        let mut candidates = Vec::new();
        for proposal in proposals {
            match proposal {
                PatternProposal::Suppressed { record } => patterns.push(record.projection.id),
                PatternProposal::Proposed { record, .. } => {
                    patterns.push(record.projection.id.clone());
                    if record.projection.project_id.as_deref() == Some(&self.project_id)
                        && let Some(candidate) = self.skills.create_companion_from_pattern(
                            &record.projection.id,
                            &self.project_root,
                            false,
                            zuno_db::message::now_millis(),
                        )?
                    {
                        candidates.push(candidate.projection.id);
                    }
                }
            }
        }
        Ok(json!({"patterns":patterns,"skillCandidates":candidates}))
    }
}
