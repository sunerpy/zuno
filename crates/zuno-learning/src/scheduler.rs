use crate::Result;
use crate::extraction::{ExtractionJobPayload, ExtractionRequest, ExtractionTrigger};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use zuno_config::ResolvedLearningConfig;
use zuno_db::experience::ExperienceStore;
use zuno_db::learning_job::{
    ExtractionJobInsert, LearningJobInsert, LearningJobKind, LearningJobRecord, LearningJobStatus,
    LearningJobStore, LeaseReconciliation, NewLearningJob,
};
use zuno_types::SessionMemoryGeneration;

/// How many times one learning job may be handed to a worker before it is settled
/// `Failed` instead of claimed again.
///
/// `LearningJobStore::claim_due` increments `attempt` and never reads it, and
/// `reconcile_expired` requeues any expired `running` extraction unconditionally, so
/// without a cap a job that can never succeed is re-claimed on every lease cycle —
/// and each cycle of an extraction job is a full paid model call. Three is chosen so
/// the genuinely recoverable causes still recover (SQLite contention, a lost lease
/// across a restart, one provider hiccup) while a permanent failure that the worker
/// reports without settling stops costing tokens.
///
/// This is the consumer-side half of the bound. The store-side half — refusing to
/// requeue or claim an over-cap row in SQL, so a caller that bypasses this scheduler
/// is bounded too — is an integrator seam in `zuno-db`, which this lane does not own.
const MAX_JOB_ATTEMPTS: u32 = 3;
const RETRY_INITIAL_DELAY_MS: i64 = 1_000;
const RETRY_MAX_DELAY_MS: i64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedTaskSignals {
    pub completed: bool,
    pub had_tool_calls: bool,
    pub had_artifacts: bool,
    pub recovered_from_error: bool,
    pub user_corrected: bool,
    pub explicit_feedback: bool,
    #[serde(default)]
    pub external_context: bool,
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
    Excluded,
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

    /// Whether durable Experience records may be retrieved for model context.
    #[must_use]
    pub const fn use_existing(&self) -> bool {
        self.config.use_existing
    }

    /// Whether automatic extraction should exclude turns that consumed external context.
    #[must_use]
    pub const fn excludes_external_context(&self) -> bool {
        self.config.disable_on_external_context
    }

    /// Background automatic-extraction poll interval.
    #[must_use]
    pub const fn post_turn_poll_interval_ms(&self) -> u64 {
        self.config.post_turn_poll_interval_ms
    }

    /// Maximum jobs one automatic worker wake may claim.
    #[must_use]
    pub const fn post_turn_max_jobs_per_wake(&self) -> u32 {
        self.config.post_turn_max_jobs_per_wake
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
        if !self.config.generate || !self.config.post_turn_enabled {
            return Ok(LearningScheduleOutcome::Disabled);
        }
        if self.config.disable_on_external_context && signals.external_context {
            return Ok(LearningScheduleOutcome::Excluded);
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
        let delay = i64::try_from(self.config.post_turn_idle_delay_ms).unwrap_or(i64::MAX);
        self.enqueue_extraction(
            request,
            ExtractionTrigger::AutomaticPostTurn,
            now.saturating_add(delay),
            now,
        )
    }

    pub fn schedule_manual_reflection(
        &self,
        request: ExtractionRequest,
        now: i64,
    ) -> Result<LearningScheduleOutcome> {
        if !self.config.generate {
            return Ok(LearningScheduleOutcome::Disabled);
        }
        self.enqueue_extraction(request, ExtractionTrigger::Manual, now, now)
    }

    pub fn schedule_project_aggregation(
        &self,
        project_id: &str,
        now: i64,
    ) -> Result<LearningScheduleOutcome> {
        if !self.config.generate {
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
        if !self.config.generate {
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

    /// Claim the next due job, unless it has already exhausted [`MAX_JOB_ATTEMPTS`].
    pub fn claim_due(
        &self,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
    ) -> Result<Option<LearningJobRecord>> {
        let claimed = self.jobs.claim_due(owner_id, now, lease_expires)?;
        self.bound_attempts(claimed, owner_id, now)
    }

    /// Claim the next due job for one project, unless it has already exhausted
    /// [`MAX_JOB_ATTEMPTS`].
    pub fn claim_due_for_project(
        &self,
        project_id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
    ) -> Result<Option<LearningJobRecord>> {
        self.claim_due_for_project_excluding(project_id, owner_id, now, lease_expires, &[])
    }

    /// Claim project work while withholding extraction for process-local live sessions.
    pub fn claim_due_for_project_excluding(
        &self,
        project_id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
        busy_session_ids: &[String],
    ) -> Result<Option<LearningJobRecord>> {
        let idle_delay = i64::try_from(self.config.post_turn_idle_delay_ms).unwrap_or(i64::MAX);
        let claimed = self.jobs.claim_due_for_project_eligible_excluding(
            project_id,
            owner_id,
            now,
            lease_expires,
            now.saturating_sub(idle_delay),
            busy_session_ids,
        )?;
        self.bound_attempts(claimed, owner_id, now)
    }

    /// Claim one known job, unless it has already exhausted [`MAX_JOB_ATTEMPTS`].
    pub fn claim(
        &self,
        job_id: &str,
        owner_id: &str,
        now: i64,
        lease_expires: i64,
    ) -> Result<Option<LearningJobRecord>> {
        let record = self.jobs.get(job_id)?;
        let manual = record.kind == LearningJobKind::Extraction
            && record
                .payload
                .as_ref()
                .and_then(|payload| payload.get("trigger"))
                .and_then(serde_json::Value::as_str)
                == Some("manual");
        let claimed = if record.kind == LearningJobKind::Extraction && !manual {
            let idle_delay = i64::try_from(self.config.post_turn_idle_delay_ms).unwrap_or(i64::MAX);
            self.jobs.claim_automatic_extraction(
                job_id,
                owner_id,
                now,
                lease_expires,
                now.saturating_sub(idle_delay),
            )?
        } else {
            self.jobs.claim(job_id, owner_id, now, lease_expires)?
        };
        self.bound_attempts(claimed, owner_id, now)
    }

    /// Settle a job that has been handed out too many times instead of running it
    /// again.
    ///
    /// The check runs after the claim because `attempt` is incremented by the claim
    /// itself and because settling requires holding the lease. `None` is returned for
    /// a capped job, which every caller already treats as "no work", so no worker
    /// spends a model call on it.
    fn bound_attempts(
        &self,
        claimed: Option<LearningJobRecord>,
        owner_id: &str,
        now: i64,
    ) -> Result<Option<LearningJobRecord>> {
        let Some(record) = claimed else {
            return Ok(None);
        };
        if record.attempt <= MAX_JOB_ATTEMPTS {
            return Ok(Some(record));
        }
        // Best effort by construction: if settling fails the row stays `running`, the
        // reconciler requeues it, and this cap refuses it again on the next claim, so
        // the failure cannot turn into work. Reporting the settle error instead would
        // replace a bounded, silent no-op with an error the callers treat as fatal.
        let _ = self.jobs.settle(
            &record.id,
            owner_id,
            LearningJobStatus::Failed,
            None,
            Some(&format!(
                "learning job stopped after {} attempts without a durable result; the last \
                 recorded error was {}",
                record.attempt,
                record
                    .error
                    .as_deref()
                    .unwrap_or("not recorded by the worker"),
            )),
            now,
        );
        Ok(None)
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

    pub fn retry(
        &self,
        job_id: &str,
        owner_id: &str,
        error: &str,
        retry_after: Option<Duration>,
        now: i64,
    ) -> Result<()> {
        let attempt = self.jobs.get(job_id)?.attempt.max(1);
        let exponent = attempt.saturating_sub(1).min(20);
        let local_delay = RETRY_INITIAL_DELAY_MS
            .checked_shl(exponent)
            .unwrap_or(i64::MAX);
        let requested_delay = retry_after
            .map(|delay| i64::try_from(delay.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let delay = local_delay.max(requested_delay).min(RETRY_MAX_DELAY_MS);
        self.jobs
            .retry(job_id, owner_id, error, now.saturating_add(delay), now)
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
        trigger: ExtractionTrigger,
        scheduled_at: i64,
        now: i64,
    ) -> Result<LearningScheduleOutcome> {
        let project_id = request.project_id.clone();
        let session_id = request.session_id.clone();
        let source_message_id = request.source_message_id.clone();
        let payload = serde_json::to_value(ExtractionJobPayload { trigger, request })
            .expect("ExtractionJobPayload is serializable");
        let mut job = NewLearningJob::extraction(
            format!("lrn_{}", Uuid::now_v7().simple()),
            project_id,
            session_id,
            source_message_id,
            self.extractor_version.clone(),
            payload.clone(),
            now,
        );
        job.scheduled_at = scheduled_at;
        let LearningJobInsert {
            mut record,
            inserted,
        } = match self.jobs.enqueue_extraction_if_enabled(job)? {
            ExtractionJobInsert::Admitted(insert) => *insert,
            ExtractionJobInsert::Blocked(SessionMemoryGeneration::Disabled) => {
                return Ok(LearningScheduleOutcome::Disabled);
            }
            ExtractionJobInsert::Blocked(SessionMemoryGeneration::Excluded) => {
                return Ok(LearningScheduleOutcome::Excluded);
            }
            ExtractionJobInsert::Blocked(SessionMemoryGeneration::Enabled) => {
                unreachable!("enabled session generation admits extraction")
            }
        };
        if trigger == ExtractionTrigger::Manual {
            record = self
                .jobs
                .expedite_manual_extraction(&record.id, &payload, now)?;
        }
        Ok(if inserted {
            LearningScheduleOutcome::Queued(record)
        } else {
            LearningScheduleOutcome::Existing(record)
        })
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

    fn pool() -> Arc<zuno_db::Pool> {
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
        pool
    }

    fn config() -> ResolvedLearningConfig {
        ResolvedLearningConfig {
            generate: true,
            extractor_model: Some("provider/extractor-v1".to_owned()),
            post_turn_enabled: true,
            post_turn_idle_delay_ms: 25,
            disable_on_external_context: true,
            ..ResolvedLearningConfig::default()
        }
    }

    const fn useful_signals() -> CompletedTaskSignals {
        CompletedTaskSignals {
            completed: true,
            had_tool_calls: true,
            had_artifacts: false,
            recovered_from_error: false,
            user_corrected: false,
            explicit_feedback: false,
            external_context: false,
        }
    }

    fn request() -> ExtractionRequest {
        ExtractionRequest {
            project_id: "project-1".to_owned(),
            session_id: "session-1".to_owned(),
            source_message_id: "assistant-1".to_owned(),
            transcript: "transcript".to_owned(),
            had_tool_calls: true,
            had_artifacts: false,
            recovered_from_error: false,
            user_corrected: false,
            explicit_feedback: false,
        }
    }

    #[test]
    fn post_turn_outcomes_separate_disabled_ineligible_and_excluded() {
        let mut disabled = config();
        disabled.generate = false;
        assert_eq!(
            LearningScheduler::new(pool(), disabled)
                .schedule_post_turn(
                    "project-1",
                    "session-1",
                    "assistant-1",
                    "transcript",
                    useful_signals(),
                    10,
                )
                .expect("disabled"),
            LearningScheduleOutcome::Disabled
        );

        let mut disabled = config();
        disabled.post_turn_enabled = false;
        assert_eq!(
            LearningScheduler::new(pool(), disabled)
                .schedule_post_turn(
                    "project-1",
                    "session-1",
                    "assistant-1",
                    "transcript",
                    useful_signals(),
                    10,
                )
                .expect("post-turn disabled"),
            LearningScheduleOutcome::Disabled
        );

        let scheduler = LearningScheduler::new(pool(), config());
        assert_eq!(
            scheduler
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
                        external_context: false,
                    },
                    10,
                )
                .expect("ineligible"),
            LearningScheduleOutcome::Ineligible
        );
        assert_eq!(
            scheduler
                .schedule_post_turn(
                    "project-1",
                    "session-1",
                    "assistant-1",
                    "transcript",
                    CompletedTaskSignals {
                        external_context: true,
                        ..useful_signals()
                    },
                    10,
                )
                .expect("excluded"),
            LearningScheduleOutcome::Excluded
        );

        let mut allowed = config();
        allowed.disable_on_external_context = false;
        assert!(matches!(
            LearningScheduler::new(pool(), allowed)
                .schedule_post_turn(
                    "project-1",
                    "session-1",
                    "assistant-1",
                    "transcript",
                    CompletedTaskSignals {
                        external_context: true,
                        ..useful_signals()
                    },
                    10,
                )
                .expect("external context allowed"),
            LearningScheduleOutcome::Queued(_)
        ));
    }

    #[test]
    fn durable_session_policy_blocks_new_automatic_and_manual_extraction() {
        let pool = pool();
        {
            let connection = pool.get().expect("connection");
            connection
                .execute(
                    "INSERT INTO session_memory_policy (
                       session_id, use_memories, generation, reason, source, revision,
                       time_created, time_updated
                     ) VALUES (
                       'session-1', 1, 'disabled', 'user choice', 'test', 1, 1, 1
                     )",
                    [],
                )
                .expect("disable generation");
        }
        let scheduler =
            LearningScheduler::new(Arc::clone(&pool), config()).with_extractor_version("v1");
        assert_eq!(
            scheduler
                .schedule_post_turn(
                    "project-1",
                    "session-1",
                    "assistant-1",
                    "transcript",
                    useful_signals(),
                    10,
                )
                .expect("disabled automatic admission"),
            LearningScheduleOutcome::Disabled
        );
        assert_eq!(
            scheduler
                .schedule_manual_reflection(request(), 10)
                .expect("disabled manual admission"),
            LearningScheduleOutcome::Disabled
        );

        {
            let connection = pool.get().expect("connection");
            connection
                .execute(
                    "UPDATE session_memory_policy
                     SET generation = 'excluded', revision = 2, time_updated = 2
                     WHERE session_id = 'session-1'",
                    [],
                )
                .expect("exclude generation");
        }
        assert_eq!(
            scheduler
                .schedule_post_turn(
                    "project-1",
                    "session-1",
                    "assistant-1",
                    "transcript",
                    useful_signals(),
                    11,
                )
                .expect("excluded automatic admission"),
            LearningScheduleOutcome::Excluded
        );
        assert_eq!(
            scheduler
                .schedule_manual_reflection(request(), 11)
                .expect("excluded manual admission"),
            LearningScheduleOutcome::Excluded
        );

        let connection = pool.get().expect("connection");
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM learning_job", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("count jobs"),
            0
        );
    }

    #[test]
    fn manual_reflection_revives_an_automatic_job_skipped_by_a_temporary_disable() {
        let pool = pool();
        let scheduler =
            LearningScheduler::new(Arc::clone(&pool), config()).with_extractor_version("v1");
        let queued = scheduler
            .schedule_post_turn(
                "project-1",
                "session-1",
                "assistant-1",
                "automatic transcript",
                useful_signals(),
                10,
            )
            .expect("queue automatic");
        let LearningScheduleOutcome::Queued(automatic) = queued else {
            panic!("automatic extraction must be queued");
        };
        let policies =
            zuno_db::session_memory_policy::SessionMemoryPolicyStore::new(Arc::clone(&pool));
        policies
            .set(zuno_db::session_memory_policy::SessionMemoryPolicyUpdate {
                session_id: "session-1".to_owned(),
                use_memories: true,
                generation: SessionMemoryGeneration::Disabled,
                reason: "temporary pause".to_owned(),
                source: "test".to_owned(),
                expected_revision: 0,
                time_updated: 20,
            })
            .expect("disable generation");
        assert_eq!(
            LearningJobStore::new(Arc::clone(&pool))
                .get(&automatic.id)
                .expect("skipped automatic")
                .status,
            LearningJobStatus::Skipped
        );
        policies
            .set(zuno_db::session_memory_policy::SessionMemoryPolicyUpdate {
                session_id: "session-1".to_owned(),
                use_memories: true,
                generation: SessionMemoryGeneration::Enabled,
                reason: "resume explicit reflection".to_owned(),
                source: "test".to_owned(),
                expected_revision: 1,
                time_updated: 30,
            })
            .expect("enable generation");

        let manual = scheduler
            .schedule_manual_reflection(request(), 40)
            .expect("manual reflection");
        let LearningScheduleOutcome::Existing(manual) = manual else {
            panic!("manual reflection keeps the original idempotency identity");
        };
        assert_eq!(manual.id, automatic.id);
        assert_eq!(manual.status, LearningJobStatus::Queued);
        assert_eq!(manual.attempt, 0);
        let claimed = scheduler
            .claim(&manual.id, "worker", 40, 60)
            .expect("claim manual")
            .expect("revived manual job");
        assert_eq!(
            claimed.payload.expect("payload")["trigger"],
            json!("manual")
        );
    }

    #[test]
    fn post_turn_is_delayed_and_idempotent_with_an_automatic_payload() {
        let scheduler =
            LearningScheduler::new(pool(), config()).with_extractor_version("extractor-v1");
        let queued = scheduler
            .schedule_post_turn(
                "project-1",
                "session-1",
                "assistant-1",
                "transcript",
                useful_signals(),
                11,
            )
            .expect("queued");
        let LearningScheduleOutcome::Queued(job) = queued else {
            panic!("first admission must queue");
        };
        assert_eq!(job.scheduled_at, 36);
        assert_eq!(job.time_created, 11);
        let payload = crate::decode_extraction_job_payload(job.payload.clone().expect("payload"))
            .expect("decode payload");
        assert_eq!(payload.trigger, ExtractionTrigger::AutomaticPostTurn);
        assert_eq!(payload.request, request());

        let existing = scheduler
            .schedule_post_turn(
                "project-1",
                "session-1",
                "assistant-1",
                "ignored duplicate",
                useful_signals(),
                12,
            )
            .expect("existing");
        let LearningScheduleOutcome::Existing(existing) = existing else {
            panic!("duplicate admission must return the durable job");
        };
        assert_eq!(existing.id, job.id);
        assert_eq!(existing.scheduled_at, 36);
        assert_eq!(existing.payload, job.payload);
    }

    #[test]
    fn manual_reflection_is_immediate_and_bypasses_post_turn_policy() {
        let mut policy = config();
        policy.post_turn_enabled = false;
        policy.disable_on_external_context = true;
        let scheduler =
            LearningScheduler::new(pool(), policy).with_extractor_version("extractor-v1");

        let queued = scheduler
            .schedule_manual_reflection(request(), 50)
            .expect("manual reflection");
        let LearningScheduleOutcome::Queued(job) = queued else {
            panic!("manual reflection must queue");
        };
        assert_eq!(job.scheduled_at, 50);
        let payload = crate::decode_extraction_job_payload(job.payload.expect("payload"))
            .expect("decode payload");
        assert_eq!(payload.trigger, ExtractionTrigger::Manual);
        assert_eq!(payload.request, request());

        let mut disabled = config();
        disabled.generate = false;
        assert_eq!(
            LearningScheduler::new(pool(), disabled)
                .schedule_manual_reflection(request(), 51)
                .expect("generation disabled"),
            LearningScheduleOutcome::Disabled
        );
    }

    #[test]
    fn retryable_worker_failure_uses_bounded_backoff_and_preserves_attempt_count() {
        let pool = pool();
        let scheduler = LearningScheduler::new(Arc::clone(&pool), config());
        let jobs = LearningJobStore::new(pool);
        jobs.enqueue(NewLearningJob::extraction(
            "job-retry",
            "project-1",
            "session-1",
            "assistant-1",
            "extractor-v1",
            json!({"trigger":"manual","request":{"transcript":"durable"}}),
            10,
        ))
        .expect("enqueue");
        scheduler
            .claim("job-retry", "worker-1", 10, 100)
            .expect("claim")
            .expect("running job");

        scheduler
            .retry(
                "job-retry",
                "worker-1",
                "provider unavailable",
                Some(Duration::from_secs(10 * 60)),
                20,
            )
            .expect("retry");
        let job = jobs.get("job-retry").expect("job");
        assert_eq!(job.status, LearningJobStatus::Queued);
        assert_eq!(job.attempt, 1);
        assert_eq!(
            job.scheduled_at,
            20 + RETRY_MAX_DELAY_MS,
            "peer retry hints are clamped to the configured ceiling"
        );
    }

    /// The second half of the unbounded-requeue fix, independent of whether the
    /// worker settles the row.
    ///
    /// `LearningJobStore::claim_due` only increments `attempt` and never reads it, and
    /// `reconcile_expired` requeues an expired `running` extraction unconditionally,
    /// so a worker that reports a failure without settling (the live post-turn path
    /// only logs it) produced `requeued: 1` with `attempt` climbing 2, 3, 4, 5 and
    /// status still `Running`, each cycle a full paid model extraction. The cap
    /// settles the row instead of handing it out again.
    #[test]
    fn a_job_that_exhausts_its_attempts_is_settled_instead_of_reclaimed() {
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
            Arc::clone(&pool),
            ResolvedLearningConfig {
                generate: true,
                ..ResolvedLearningConfig::default()
            },
        );
        let jobs = LearningJobStore::new(pool);
        jobs.enqueue(NewLearningJob::extraction(
            "job-capped",
            "project-1",
            "session-1",
            "assistant-1",
            "extractor-v1",
            json!({}),
            10,
        ))
        .expect("enqueue");

        // A worker that never settles the row: claim, let the lease expire, requeue.
        let mut now = 11_i64;
        let mut claimed_attempts = Vec::new();
        for _ in 0..6 {
            if let Some(record) = scheduler
                .claim_due("worker-1", now, now + 10)
                .expect("claim")
            {
                claimed_attempts.push(record.attempt);
            }
            now += 20;
            scheduler.reconcile_expired(now).expect("reconcile");
            now += 1;
        }

        assert_eq!(
            claimed_attempts,
            vec![1, 2, 3],
            "a job may only be handed to a worker MAX_JOB_ATTEMPTS times"
        );
        let job = jobs.get("job-capped").expect("job");
        assert_eq!(job.status, LearningJobStatus::Failed);
        assert!(job.owner_id.is_none());
        assert!(
            job.error
                .as_deref()
                .is_some_and(|error| error.contains("stopped after 4 attempts")),
            "unexpected settled error: {:?}",
            job.error
        );
        // Terminal, so the reconciler has nothing left to requeue.
        assert_eq!(
            scheduler
                .reconcile_expired(now + 100)
                .expect("reconcile")
                .requeued,
            0
        );
        assert!(
            scheduler
                .claim_due("worker-1", now + 101, now + 200)
                .expect("claim")
                .is_none()
        );
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
                generate: true,
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
