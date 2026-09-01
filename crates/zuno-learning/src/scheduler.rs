use crate::Result;
use crate::extraction::ExtractionRequest;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;
use zuno_config::ResolvedLearningConfig;
use zuno_db::experience::ExperienceStore;
use zuno_db::learning_job::{
    LearningJobInsert, LearningJobKind, LearningJobRecord, LearningJobStatus, LearningJobStore,
    LeaseReconciliation, NewLearningJob,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedTaskSignals {
    pub completed: bool,
    pub had_tool_calls: bool,
    pub had_artifacts: bool,
    pub recovered_from_error: bool,
    pub user_corrected: bool,
    pub explicit_feedback: bool,
}

impl CompletedTaskSignals {
    #[must_use]
    pub const fn eligible(self) -> bool {
        self.completed
            && (self.had_tool_calls
                || self.had_artifacts
                || self.recovered_from_error
                || self.user_corrected
                || self.explicit_feedback)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LearningScheduleOutcome {
    Disabled,
    Ineligible,
    SkippedInsufficientRecords { observed: u32, required: u32 },
    Queued(LearningJobRecord),
    Existing(LearningJobRecord),
}

#[derive(Clone)]
pub struct LearningScheduler {
    jobs: LearningJobStore,
    experiences: ExperienceStore,
    config: ResolvedLearningConfig,
    extractor_version: String,
}

impl LearningScheduler {
    #[must_use]
    pub fn new(pool: Arc<zuno_db::Pool>, config: ResolvedLearningConfig) -> Self {
        let extractor_version = config
            .extractor_model
            .clone()
            .unwrap_or_else(|| "learning-disabled".to_owned());
        Self {
            jobs: LearningJobStore::new(pool.clone()),
            experiences: ExperienceStore::new(pool),
            config,
            extractor_version,
        }
    }

    /// Use the extractor implementation version, rather than its model route,
    /// in the durable `(session, message, extractor_version)` idempotency key.
    #[must_use]
    pub fn with_extractor_version(mut self, version: impl Into<String>) -> Self {
        let version = version.into();
        assert!(
            !version.trim().is_empty(),
            "learning extractor version must not be empty"
        );
        self.extractor_version = version;
        self
    }

    pub fn schedule_post_turn(
        &self,
        project_id: &str,
        session_id: &str,
        source_message_id: &str,
        transcript: &str,
        signals: CompletedTaskSignals,
        now: i64,
    ) -> Result<LearningScheduleOutcome> {
        if !self.config.enabled || !self.config.post_turn_enabled {
            return Ok(LearningScheduleOutcome::Disabled);
        }
        if !signals.eligible() {
            return Ok(LearningScheduleOutcome::Ineligible);
        }
        let request = ExtractionRequest {
            project_id: project_id.to_owned(),
            session_id: session_id.to_owned(),
            source_message_id: source_message_id.to_owned(),
            transcript: transcript.to_owned(),
            had_tool_calls: signals.had_tool_calls,
            had_artifacts: signals.had_artifacts,
            recovered_from_error: signals.recovered_from_error,
            user_corrected: signals.user_corrected,
            explicit_feedback: signals.explicit_feedback,
        };
        self.enqueue_extraction(request, now)
    }

    pub fn schedule_manual_reflection(
        &self,
        request: ExtractionRequest,
        now: i64,
    ) -> Result<LearningScheduleOutcome> {
        if !self.config.enabled {
            return Ok(LearningScheduleOutcome::Disabled);
        }
        self.enqueue_extraction(request, now)
    }

    pub fn schedule_project_aggregation(
        &self,
        project_id: &str,
        now: i64,
    ) -> Result<LearningScheduleOutcome> {
        if !self.config.enabled {
            return Ok(LearningScheduleOutcome::Disabled);
        }
        let interval = i64::try_from(self.config.aggregation_interval_ms).unwrap_or(i64::MAX);
        let since = now.saturating_sub(interval);
        let observed = self.experiences.count_new_since(project_id, since)?;
        if observed < self.config.aggregation_min_new_records {
            return Ok(LearningScheduleOutcome::SkippedInsufficientRecords {
                observed,
                required: self.config.aggregation_min_new_records,
            });
        }
        let bucket = bucket(now, interval);
        self.enqueue(NewLearningJob {
            id: format!("lrn_{}", Uuid::now_v7().simple()),
            project_id: Some(project_id.to_owned()),
            session_id: None,
            source_message_id: None,
            kind: LearningJobKind::ProjectAggregation,
            extractor_version: None,
            idempotency_key: format!("project-aggregation:{project_id}:{bucket}"),
            scheduled_at: now,
            payload: Some(json!({"since": since, "observed": observed})),
            time_created: now,
        })
    }

    pub fn schedule_global_aggregation(
        &self,
        evidence_digest: &str,
        now: i64,
    ) -> Result<LearningScheduleOutcome> {
        if !self.config.enabled {
            return Ok(LearningScheduleOutcome::Disabled);
        }
        if evidence_digest.trim().is_empty() {
            return Ok(LearningScheduleOutcome::Ineligible);
        }
        let interval = i64::try_from(self.config.global_promotion_interval_ms).unwrap_or(i64::MAX);
        let bucket = bucket(now, interval);
        self.enqueue(NewLearningJob {
            id: format!("lrn_{}", Uuid::now_v7().simple()),
            project_id: None,
            session_id: None,
            source_message_id: None,
            kind: LearningJobKind::GlobalAggregation,
            extractor_version: None,
            idempotency_key: format!("global-aggregation:{bucket}:{evidence_digest}"),
            scheduled_at: now,
            payload: Some(json!({
                "minProjects": self.config.global_promotion_min_projects,
                "evidenceDigest": evidence_digest,
            })),
            time_created: now,
        })
    }

    pub fn claim_due(
        &self,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
    ) -> Result<Option<LearningJobRecord>> {
        self.jobs
            .claim_due(owner_id, now, lease_expires)
            .map_err(Into::into)
    }

    pub fn claim_due_for_project(
        &self,
        project_id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
    ) -> Result<Option<LearningJobRecord>> {
        self.jobs
            .claim_due_for_project(project_id, owner_id, now, lease_expires)
            .map_err(Into::into)
    }

    pub fn claim(
        &self,
        job_id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
    ) -> Result<Option<LearningJobRecord>> {
        self.jobs
            .claim(job_id, owner_id, now, lease_expires)
            .map_err(Into::into)
    }

    pub fn fail(&self, job_id: &str, owner_id: &str, error: &str, now: i64) -> Result<()> {
        self.jobs
            .settle(
                job_id,
                owner_id,
                LearningJobStatus::Failed,
                None,
                Some(error),
                now,
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn complete(
        &self,
        job_id: &str,
        owner_id: &str,
        result: &serde_json::Value,
        now: i64,
    ) -> Result<()> {
        self.jobs
            .settle(
                job_id,
                owner_id,
                LearningJobStatus::Completed,
                Some(result),
                None,
                now,
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn reconcile_expired(&self, now: i64) -> Result<LeaseReconciliation> {
        self.jobs.reconcile_expired(now).map_err(Into::into)
    }

    pub fn get(&self, job_id: &str) -> Result<LearningJobRecord> {
        self.jobs.get(job_id).map_err(Into::into)
    }

    fn enqueue_extraction(
        &self,
        request: ExtractionRequest,
        now: i64,
    ) -> Result<LearningScheduleOutcome> {
        let payload = serde_json::to_value(&request).expect("ExtractionRequest is serializable");
        self.enqueue(NewLearningJob::extraction(
            format!("lrn_{}", Uuid::now_v7().simple()),
            request.project_id,
            request.session_id,
            request.source_message_id,
            self.extractor_version.clone(),
            payload,
            now,
        ))
    }

    fn enqueue(&self, job: NewLearningJob) -> Result<LearningScheduleOutcome> {
        let LearningJobInsert { record, inserted } = self.jobs.enqueue(job)?;
        Ok(if inserted {
            LearningScheduleOutcome::Queued(record)
        } else {
            LearningScheduleOutcome::Existing(record)
        })
    }
}

fn bucket(now: i64, interval: i64) -> i64 {
    if interval <= 0 {
        0
    } else {
        now.div_euclid(interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_db::migration;
    use zuno_paths::DbLocation;

    #[test]
    fn post_turn_requires_a_completed_learning_signal_and_is_idempotent() {
        let pool = Arc::new(zuno_db::Pool::open(&DbLocation::Memory).expect("pool"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection
                .execute_batch(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('project-1', '/workspace', 1, 1, '[]');
                     INSERT INTO session
                       (id, project_id, slug, directory, title, version, time_created, time_updated)
                     VALUES ('session-1', 'project-1', 'slug', '/workspace', 'title', '1', 1, 1);
                     INSERT INTO message (id, session_id, time_created, time_updated, data)
                     VALUES ('assistant-1', 'session-1', 1, 1, '{\"role\":\"assistant\"}');",
                )
                .expect("fixture");
        }
        let scheduler = LearningScheduler::new(
            pool,
            ResolvedLearningConfig {
                enabled: true,
                extractor_model: Some("provider/extractor-v1".to_owned()),
                ..ResolvedLearningConfig::default()
            },
        );
        let no_signal = scheduler
            .schedule_post_turn(
                "project-1",
                "session-1",
                "assistant-1",
                "transcript",
                CompletedTaskSignals {
                    completed: true,
                    had_tool_calls: false,
                    had_artifacts: false,
                    recovered_from_error: false,
                    user_corrected: false,
                    explicit_feedback: false,
                },
                10,
            )
            .expect("ineligible");
        assert_eq!(no_signal, LearningScheduleOutcome::Ineligible);
        let signals = CompletedTaskSignals {
            completed: true,
            had_tool_calls: true,
            had_artifacts: false,
            recovered_from_error: false,
            user_corrected: false,
            explicit_feedback: false,
        };
        assert!(matches!(
            scheduler
                .schedule_post_turn(
                    "project-1",
                    "session-1",
                    "assistant-1",
                    "transcript",
                    signals,
                    11,
                )
                .expect("queued"),
            LearningScheduleOutcome::Queued(_)
        ));
        assert!(matches!(
            scheduler
                .schedule_post_turn(
                    "project-1",
                    "session-1",
                    "assistant-1",
                    "transcript",
                    signals,
                    12,
                )
                .expect("existing"),
            LearningScheduleOutcome::Existing(_)
        ));
    }

    #[test]
    fn global_aggregation_deduplicates_unchanged_evidence_but_accepts_new_evidence() {
        let pool = Arc::new(zuno_db::Pool::open(&DbLocation::Memory).expect("pool"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
        }
        let scheduler = LearningScheduler::new(
            pool,
            ResolvedLearningConfig {
                enabled: true,
                extractor_model: Some("provider/extractor-v1".to_owned()),
                ..ResolvedLearningConfig::default()
            },
        );
        let now = 1_800_000_000_000;

        assert!(matches!(
            scheduler
                .schedule_global_aggregation("evidence-v1", now)
                .expect("first global job"),
            LearningScheduleOutcome::Queued(_)
        ));
        assert!(matches!(
            scheduler
                .schedule_global_aggregation("evidence-v1", now + 1)
                .expect("same evidence"),
            LearningScheduleOutcome::Existing(_)
        ));
        assert!(matches!(
            scheduler
                .schedule_global_aggregation("evidence-v2", now + 2)
                .expect("new evidence in the same interval"),
            LearningScheduleOutcome::Queued(_)
        ));
    }
}
