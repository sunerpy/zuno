use crate::extraction::{ExtractedExperience, LearningExtraction};
use crate::{LearningServiceError, Result, digest_text};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;
use zuno_db::experience::{
    ExperienceEvidenceKind, ExperienceRecord, ExperienceStore, NewExperience, NewExperienceEvidence,
};
use zuno_db::learning_job::LearningJobStore;
use zuno_error::LearningError;
use zuno_memory::{MemoryProposal, MemoryService};
use zuno_types::{
    ExperienceKind, MemoryAction, MemoryCandidateProjection, MemoryCandidateStatus, MemoryScope,
    MemorySource,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPromotionResult {
    pub experience_id: Option<String>,
    pub candidate: Option<MemoryCandidateProjection>,
    pub automatically_applied: bool,
    pub rejected_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionPersistence {
    pub experiences: Vec<ExperienceRecord>,
    pub memory_promotions: Vec<MemoryPromotionResult>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionExperienceCleanup {
    pub forgotten_experience_ids: Vec<String>,
    pub memory_revocation_candidate_ids: Vec<String>,
    pub rejected_memory_candidate_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualExperienceRequest {
    pub project_id: String,
    pub session_id: Option<String>,
    pub source_message_id: Option<String>,
    pub kind: ExperienceKind,
    pub title: String,
    pub summary: String,
    pub resolution: Option<String>,
    pub time_created: i64,
}

#[derive(Clone)]
pub struct ExperienceService {
    store: ExperienceStore,
    jobs: LearningJobStore,
    memory: Option<Arc<MemoryService>>,
}

impl ExperienceService {
    #[must_use]
    pub fn new(pool: Arc<zuno_db::Pool>, memory: Option<Arc<MemoryService>>) -> Self {
        Self {
            store: ExperienceStore::new(pool.clone()),
            jobs: LearningJobStore::new(pool),
            memory,
        }
    }

    pub fn persist_extraction(
        &self,
        job_id: &str,
        owner_id: &str,
        extraction: LearningExtraction,
        now: i64,
    ) -> Result<ExtractionPersistence> {
        let job = self.jobs.get(job_id)?;
        let project_id = job.project_id.ok_or_else(|| {
            invalid(
                "job.project_id",
                "extraction job has no durable project identity",
            )
        })?;
        let session_id = job.session_id.ok_or_else(|| {
            invalid(
                "job.session_id",
                "extraction job has no durable session identity",
            )
        })?;
        let source_message_id = job.source_message_id.ok_or_else(|| {
            invalid(
                "job.source_message_id",
                "extraction job has no durable source message",
            )
        })?;

        let mut new_experiences = Vec::with_capacity(extraction.experiences.len());
        for (ordinal, extracted) in extraction.experiences.iter().enumerate() {
            new_experiences.push(build_experience(
                extracted,
                ordinal,
                job_id,
                &project_id,
                &session_id,
                &source_message_id,
                now,
            )?);
        }
        for memory in &extraction.memories {
            if memory.experience_ordinal >= new_experiences.len() {
                return Err(invalid(
                    "memories.experience_ordinal",
                    "memory points outside the extracted experience list",
                ));
            }
        }

        let result = serde_json::to_value(&extraction)
            .expect("LearningExtraction derives a total Serialize implementation");
        let experiences =
            self.store
                .complete_extraction(job_id, owner_id, &new_experiences, &result, now)?;
        let mut memory_promotions = Vec::with_capacity(extraction.memories.len());
        for memory in extraction.memories {
            let linked = &experiences[memory.experience_ordinal];
            if !linked.projection.kind.promotable() {
                memory_promotions.push(MemoryPromotionResult {
                    experience_id: Some(linked.projection.id.clone()),
                    candidate: None,
                    automatically_applied: false,
                    rejected_reason: Some(
                        "unresolved issues cannot become Memory or Skill".to_owned(),
                    ),
                });
                continue;
            }
            let Some(service) = &self.memory else {
                memory_promotions.push(MemoryPromotionResult {
                    experience_id: Some(linked.projection.id.clone()),
                    candidate: None,
                    automatically_applied: false,
                    rejected_reason: Some("resident Memory is disabled".to_owned()),
                });
                continue;
            };
            let confidence = checked_confidence(memory.confidence, "memories.confidence")?;
            let proposal = service.propose_for_review(MemoryProposal {
                scope: memory.scope.into(),
                action: memory.action.into(),
                content: memory.content,
                old_text: memory.old_text,
                reason: memory.reason,
                confidence: memory.confidence,
                source: MemorySource::Reflection,
                source_session_id: Some(session_id.clone()),
                source_message_id: Some(source_message_id.clone()),
            });
            let mut candidate = match proposal {
                Ok(candidate) => candidate,
                Err(error) => {
                    memory_promotions.push(MemoryPromotionResult {
                        experience_id: Some(linked.projection.id.clone()),
                        candidate: None,
                        automatically_applied: false,
                        rejected_reason: Some(error.to_string()),
                    });
                    continue;
                }
            };
            let eligible_for_auto = candidate.projection.scope == MemoryScope::Project
                && confidence >= 9_000
                && candidate.projection.status == MemoryCandidateStatus::Pending;
            if eligible_for_auto {
                if let Err(error) = service.apply(candidate.id()) {
                    self.store
                        .mark_promoted(&linked.projection.id, candidate.id(), now)?;
                    let current = service
                        .candidate(candidate.id())
                        .map_or(candidate.projection, |record| record.projection);
                    memory_promotions.push(MemoryPromotionResult {
                        experience_id: Some(linked.projection.id.clone()),
                        candidate: Some(current),
                        automatically_applied: false,
                        rejected_reason: Some(error.to_string()),
                    });
                    continue;
                }
                candidate = service.candidate(candidate.id())?;
            }
            self.store
                .mark_promoted(&linked.projection.id, candidate.id(), now)?;
            memory_promotions.push(MemoryPromotionResult {
                experience_id: Some(linked.projection.id.clone()),
                automatically_applied: candidate.projection.status
                    == MemoryCandidateStatus::Applied,
                candidate: Some(candidate.projection),
                rejected_reason: None,
            });
        }
        Ok(ExtractionPersistence {
            experiences,
            memory_promotions,
        })
    }

    pub fn record_manual(&self, request: ManualExperienceRequest) -> Result<ExperienceRecord> {
        let ManualExperienceRequest {
            project_id,
            session_id,
            source_message_id,
            kind,
            title,
            summary,
            resolution,
            time_created,
        } = request;
        let title = title.trim().to_owned();
        let summary = summary.trim().to_owned();
        if title.is_empty() || summary.is_empty() {
            return Err(invalid(
                "experience",
                "manual experience title and summary must not be empty",
            ));
        }
        if !kind.promotable() && resolution.is_some() {
            return Err(invalid(
                "resolution",
                "an unresolved issue cannot already have a resolution",
            ));
        }
        let resolution = resolution.map(|value| value.trim().to_owned());
        let fingerprint = experience_fingerprint(kind, &title, &summary, resolution.as_deref());
        let excerpt = resolution.as_deref().unwrap_or(&summary).to_owned();
        let excerpt_digest = digest_text(&excerpt);
        let evidence_source_id = source_message_id.clone();
        self.store
            .create_manual(NewExperience {
                id: format!("exp_{}", Uuid::now_v7().simple()),
                project_id,
                session_id,
                source_message_id,
                extraction_job_id: None,
                extraction_ordinal: None,
                kind,
                title,
                summary,
                resolution,
                confidence: 10_000,
                fingerprint,
                evidence: vec![NewExperienceEvidence {
                    id: format!("eve_{}", Uuid::now_v7().simple()),
                    kind: ExperienceEvidenceKind::User,
                    source_id: evidence_source_id,
                    excerpt,
                    digest: excerpt_digest,
                }],
                time_created,
            })
            .map_err(Into::into)
    }

    pub fn solve(&self, id: &str, resolution: &str, now: i64) -> Result<ExperienceRecord> {
        self.store.solve(id, resolution, now).map_err(Into::into)
    }

    pub fn mark_promoted(
        &self,
        experience_id: &str,
        memory_candidate_id: &str,
        now: i64,
    ) -> Result<ExperienceRecord> {
        self.store
            .mark_promoted(experience_id, memory_candidate_id, now)
            .map_err(Into::into)
    }

    pub fn get(&self, id: &str) -> Result<ExperienceRecord> {
        self.store.get(id).map_err(Into::into)
    }

    pub fn list_for_project(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<ExperienceRecord>> {
        self.store
            .list_for_project(project_id, limit)
            .map_err(Into::into)
    }

    pub fn list_for_session(&self, session_id: &str) -> Result<Vec<ExperienceRecord>> {
        self.store.list_for_session(session_id).map_err(Into::into)
    }

    /// Prepare reviewable Memory reversals, then hide the session's derived
    /// experiences. Resident Memory is never removed in this operation.
    pub fn prepare_session_cleanup(
        &self,
        session_id: &str,
        now: i64,
    ) -> Result<SessionExperienceCleanup> {
        let experience_ids = self
            .store
            .list_for_session(session_id)?
            .into_iter()
            .map(|record| record.projection.id)
            .collect::<Vec<_>>();
        self.prepare_cleanup_for_experiences(&experience_ids, Some(session_id), now)
    }

    /// Prepare reviewable Memory reversals, then hide an exact evidence set.
    /// Applied resident Memory is never changed by this operation.
    pub fn prepare_cleanup_for_experiences(
        &self,
        experience_ids: &[String],
        source_session_id: Option<&str>,
        now: i64,
    ) -> Result<SessionExperienceCleanup> {
        let experiences = experience_ids
            .iter()
            .map(|id| self.store.get(id))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let memory_ids = experiences
            .iter()
            .filter_map(|record| record.projection.promoted_memory_candidate_id.clone())
            .collect::<BTreeSet<_>>();
        let mut memory_revocation_candidate_ids = Vec::new();
        let mut rejected_memory_candidate_ids = Vec::new();
        if !memory_ids.is_empty() && self.memory.is_none() {
            return Err(invalid(
                "session.cleanup",
                "resident Memory is unavailable, so promoted experience cannot be revoked safely",
            ));
        }
        if let Some(memory) = &self.memory {
            for memory_id in memory_ids {
                let candidate = memory.candidate(&memory_id)?;
                match candidate.projection.status {
                    MemoryCandidateStatus::Applied => {
                        let proposal =
                            inverse_memory_proposal(&candidate.projection, source_session_id)?;
                        let revocation = memory.propose_for_review(proposal)?;
                        memory_revocation_candidate_ids.push(revocation.projection.id);
                    }
                    MemoryCandidateStatus::Pending | MemoryCandidateStatus::Failed => {
                        let rejected = memory.reject(&memory_id)?;
                        rejected_memory_candidate_ids.push(rejected.projection.id);
                    }
                    MemoryCandidateStatus::Rejected | MemoryCandidateStatus::Undone => {}
                    MemoryCandidateStatus::Applying
                    | MemoryCandidateStatus::Undoing
                    | MemoryCandidateStatus::Uncertain => {
                        return Err(invalid(
                            "session.cleanup",
                            &format!(
                                "Memory candidate `{memory_id}` is {}; reconcile it before deleting derived experience",
                                candidate.projection.status.as_str()
                            ),
                        ));
                    }
                }
            }
        }
        let forgotten_experience_ids = self.store.forget_many(experience_ids, now)?;
        Ok(SessionExperienceCleanup {
            forgotten_experience_ids,
            memory_revocation_candidate_ids,
            rejected_memory_candidate_ids,
        })
    }
}

fn inverse_memory_proposal(
    candidate: &MemoryCandidateProjection,
    source_session_id: Option<&str>,
) -> Result<MemoryProposal> {
    let (action, content, old_text) = match candidate.action {
        MemoryAction::Add => (
            MemoryAction::Remove,
            None,
            candidate
                .content
                .clone()
                .or_else(|| candidate.old_text.clone()),
        ),
        MemoryAction::Remove => (MemoryAction::Add, candidate.old_text.clone(), None),
        MemoryAction::Replace => (
            MemoryAction::Replace,
            candidate.old_text.clone(),
            candidate.content.clone(),
        ),
    };
    if (action == MemoryAction::Add && content.is_none())
        || (matches!(action, MemoryAction::Remove | MemoryAction::Replace) && old_text.is_none())
    {
        return Err(invalid(
            "memory.revocation",
            &format!(
                "Memory candidate `{}` does not contain enough content to construct a reversal",
                candidate.id
            ),
        ));
    }
    Ok(MemoryProposal {
        scope: candidate.scope,
        action,
        content,
        old_text,
        reason: format!(
            "review revocation after removing source learning evidence for Memory candidate `{}`",
            candidate.id,
        ),
        confidence: 1.0,
        source: MemorySource::User,
        source_session_id: source_session_id.map(str::to_owned),
        source_message_id: None,
    })
}

fn build_experience(
    extracted: &ExtractedExperience,
    ordinal: usize,
    job_id: &str,
    project_id: &str,
    session_id: &str,
    source_message_id: &str,
    now: i64,
) -> Result<NewExperience> {
    let kind = ExperienceKind::from(extracted.kind);
    let title = extracted.title.trim();
    let summary = extracted.summary.trim();
    if title.is_empty() || summary.is_empty() || extracted.evidence.is_empty() {
        return Err(invalid(
            "experiences",
            "each extracted experience needs a title, summary, and durable evidence",
        ));
    }
    if !kind.promotable() && extracted.resolution.is_some() {
        return Err(invalid(
            "experiences.resolution",
            "an unresolved issue cannot carry a resolution",
        ));
    }
    let confidence = checked_confidence(extracted.confidence, "experiences.confidence")?;
    let resolution = extracted
        .resolution
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let evidence = extracted
        .evidence
        .iter()
        .map(|item| {
            let excerpt = item.excerpt.trim();
            if excerpt.is_empty() {
                return Err(invalid(
                    "experiences.evidence.excerpt",
                    "evidence excerpts must not be empty",
                ));
            }
            Ok(NewExperienceEvidence {
                id: format!("eve_{}", Uuid::now_v7().simple()),
                kind: item.kind.into(),
                source_id: item.source_id.clone(),
                excerpt: excerpt.to_owned(),
                digest: digest_text(excerpt),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(NewExperience {
        id: format!("exp_{}", Uuid::now_v7().simple()),
        project_id: project_id.to_owned(),
        session_id: Some(session_id.to_owned()),
        source_message_id: Some(source_message_id.to_owned()),
        extraction_job_id: Some(job_id.to_owned()),
        extraction_ordinal: Some(u32::try_from(ordinal).map_err(|_| {
            invalid(
                "experiences",
                "extractor returned more experiences than can be indexed",
            )
        })?),
        kind,
        title: title.to_owned(),
        summary: summary.to_owned(),
        resolution: resolution.clone(),
        confidence,
        fingerprint: experience_fingerprint(kind, title, summary, resolution.as_deref()),
        evidence,
        time_created: now,
    })
}

fn checked_confidence(value: f64, field: &str) -> Result<u16> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(invalid(field, "confidence must be between 0 and 1"));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the finite inclusive 0..=1 check above bounds the rounded scale to 0..=10_000"
    )]
    Ok((value * 10_000.0).round() as u16)
}

fn experience_fingerprint(
    kind: ExperienceKind,
    title: &str,
    summary: &str,
    resolution: Option<&str>,
) -> String {
    digest_text(
        &json!({
            "kind": kind.as_str(),
            "title": normalize(title),
            "summary": normalize(summary),
            "resolution": resolution.map(normalize),
        })
        .to_string(),
    )
}

fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn invalid(field: &str, detail: &str) -> LearningServiceError {
    LearningError::InvalidRequest {
        field: field.to_owned(),
        detail: detail.to_owned(),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extraction::{
        ExtractedEvidence, ExtractedEvidenceKind, ExtractedExperienceKind, ExtractedMemory,
        ExtractedMemoryAction, ExtractedMemoryScope,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use zuno_db::learning_job::{LearningJobStore, NewLearningJob};
    use zuno_db::migration;
    use zuno_memory::{PromotionPolicy, ScopeLimits, ScopePaths};
    use zuno_paths::DbLocation;

    fn fixture() -> (Arc<zuno_db::Pool>, ExperienceService) {
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
        (pool.clone(), ExperienceService::new(pool, None))
    }

    fn memory_fixture(
        promotion: PromotionPolicy,
    ) -> (
        TempDir,
        Arc<zuno_db::Pool>,
        ExperienceService,
        Arc<MemoryService>,
    ) {
        let directory = tempfile::tempdir().expect("memory directory");
        let (pool, _) = fixture();
        let memory = Arc::new(MemoryService::new(
            Arc::clone(&pool),
            ScopePaths::at(
                directory.path().join("global/MEMORY.md"),
                directory.path().join("project/RULES.md"),
            ),
            ScopeLimits::default(),
            promotion,
        ));
        let service = ExperienceService::new(Arc::clone(&pool), Some(Arc::clone(&memory)));
        (directory, pool, service, memory)
    }

    fn extracted(
        kind: ExtractedExperienceKind,
        title: &str,
        resolution: Option<&str>,
    ) -> ExtractedExperience {
        ExtractedExperience {
            kind,
            title: title.to_owned(),
            summary: format!("{title} was observed."),
            resolution: resolution.map(str::to_owned),
            confidence: 0.98,
            evidence: vec![ExtractedEvidence {
                kind: ExtractedEvidenceKind::Message,
                source_id: Some("assistant-1".to_owned()),
                excerpt: title.to_owned(),
            }],
        }
    }

    #[test]
    fn unresolved_experience_is_saved_but_its_memory_is_rejected() {
        let (pool, service) = fixture();
        LearningJobStore::new(pool)
            .enqueue(NewLearningJob::extraction(
                "job-1",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({}),
                10,
            ))
            .expect("enqueue");
        service
            .jobs
            .claim_due("worker-1", 11, 30)
            .expect("claim")
            .expect("job");
        let outcome = service
            .persist_extraction(
                "job-1",
                "worker-1",
                LearningExtraction {
                    experiences: vec![ExtractedExperience {
                        kind: ExtractedExperienceKind::UnresolvedIssue,
                        title: "Unknown failure".to_owned(),
                        summary: "The task still fails.".to_owned(),
                        resolution: None,
                        confidence: 0.95,
                        evidence: vec![ExtractedEvidence {
                            kind: ExtractedEvidenceKind::Message,
                            source_id: Some("assistant-1".to_owned()),
                            excerpt: "still fails".to_owned(),
                        }],
                    }],
                    memories: vec![ExtractedMemory {
                        experience_ordinal: 0,
                        scope: ExtractedMemoryScope::Project,
                        action: ExtractedMemoryAction::Add,
                        content: Some("Assume the failure is permanent.".to_owned()),
                        old_text: None,
                        reason: "bad inference".to_owned(),
                        confidence: 0.99,
                    }],
                },
                20,
            )
            .expect("persist");
        assert_eq!(outcome.experiences.len(), 1);
        assert_eq!(
            outcome.experiences[0].projection.status,
            zuno_types::ExperienceStatus::Active
        );
        assert!(outcome.memory_promotions[0].candidate.is_none());
        assert!(
            outcome.memory_promotions[0]
                .rejected_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("unresolved"))
        );
    }

    #[test]
    fn extraction_auto_applies_only_high_confidence_project_memory() {
        let (_directory, pool, service, memory) = memory_fixture(PromotionPolicy::Automatic);
        LearningJobStore::new(pool)
            .enqueue(NewLearningJob::extraction(
                "job-memory-policy",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({}),
                10,
            ))
            .expect("enqueue");
        service
            .jobs
            .claim_due("worker-1", 11, 30)
            .expect("claim")
            .expect("job");

        let outcome = service
            .persist_extraction(
                "job-memory-policy",
                "worker-1",
                LearningExtraction {
                    experiences: vec![
                        extracted(
                            ExtractedExperienceKind::Procedure,
                            "Project high confidence",
                            Some("Keep the project-specific rule."),
                        ),
                        extracted(
                            ExtractedExperienceKind::Procedure,
                            "Global high confidence",
                            Some("Keep the cross-project preference."),
                        ),
                        extracted(
                            ExtractedExperienceKind::Procedure,
                            "Project low confidence",
                            Some("Review the uncertain project rule."),
                        ),
                    ],
                    memories: vec![
                        ExtractedMemory {
                            experience_ordinal: 0,
                            scope: ExtractedMemoryScope::Project,
                            action: ExtractedMemoryAction::Add,
                            content: Some("Project high-confidence rule.".to_owned()),
                            old_text: None,
                            reason: "verified project rule".to_owned(),
                            confidence: 0.90,
                        },
                        ExtractedMemory {
                            experience_ordinal: 1,
                            scope: ExtractedMemoryScope::Global,
                            action: ExtractedMemoryAction::Add,
                            content: Some("Global high-confidence preference.".to_owned()),
                            old_text: None,
                            reason: "cross-project preference".to_owned(),
                            confidence: 1.0,
                        },
                        ExtractedMemory {
                            experience_ordinal: 2,
                            scope: ExtractedMemoryScope::Project,
                            action: ExtractedMemoryAction::Add,
                            content: Some("Project low-confidence rule.".to_owned()),
                            old_text: None,
                            reason: "needs review".to_owned(),
                            confidence: 0.89,
                        },
                    ],
                },
                20,
            )
            .expect("persist");

        assert_eq!(outcome.memory_promotions.len(), 3);
        assert!(outcome.memory_promotions[0].automatically_applied);
        assert_eq!(
            outcome.memory_promotions[0]
                .candidate
                .as_ref()
                .expect("project candidate")
                .status,
            MemoryCandidateStatus::Applied
        );
        for promotion in &outcome.memory_promotions[1..] {
            assert!(!promotion.automatically_applied);
            assert_eq!(
                promotion
                    .candidate
                    .as_ref()
                    .expect("review candidate")
                    .status,
                MemoryCandidateStatus::Pending
            );
        }
        assert_eq!(
            memory
                .entries()
                .expect("resident entries")
                .into_iter()
                .map(|entry| entry.content)
                .collect::<Vec<_>>(),
            ["Project high-confidence rule."]
        );
    }

    #[test]
    fn restart_retry_persists_one_experience_for_one_extraction_identity() {
        let (pool, service) = fixture();
        let jobs = LearningJobStore::new(Arc::clone(&pool));
        jobs.enqueue(NewLearningJob::extraction(
            "job-restart-1",
            "project-1",
            "session-1",
            "assistant-1",
            "extractor-v1",
            json!({"transcript":"durable"}),
            10,
        ))
        .expect("enqueue");
        jobs.claim_due("worker-before-restart", 11, 20)
            .expect("claim")
            .expect("job");
        let reconciliation = jobs.reconcile_expired(20).expect("reconcile");
        assert_eq!(reconciliation.requeued, 1);
        jobs.claim_due("worker-after-restart", 21, 40)
            .expect("reclaim")
            .expect("job");

        service
            .persist_extraction(
                "job-restart-1",
                "worker-after-restart",
                LearningExtraction {
                    experiences: vec![extracted(
                        ExtractedExperienceKind::Procedure,
                        "Restart-safe extraction",
                        Some("Use the durable idempotency key."),
                    )],
                    memories: Vec::new(),
                },
                30,
            )
            .expect("persist after restart");

        let duplicate = jobs
            .enqueue(NewLearningJob::extraction(
                "job-restart-2",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({"transcript":"duplicate"}),
                31,
            ))
            .expect("deduplicate");
        assert!(!duplicate.inserted);
        assert_eq!(duplicate.record.id, "job-restart-1");
        assert_eq!(
            duplicate.record.status,
            zuno_db::learning_job::LearningJobStatus::Completed
        );
        assert_eq!(
            ExperienceStore::new(pool)
                .list_for_project("project-1", 10)
                .expect("experiences")
                .len(),
            1
        );
    }

    #[test]
    fn forgetting_promoted_experience_requires_reviewed_memory_revocation() {
        let (_directory, pool, service, memory) = memory_fixture(PromotionPolicy::Automatic);
        LearningJobStore::new(pool)
            .enqueue(NewLearningJob::extraction(
                "job-forget",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({}),
                10,
            ))
            .expect("enqueue");
        service
            .jobs
            .claim_due("worker-1", 11, 30)
            .expect("claim")
            .expect("job");
        let persisted = service
            .persist_extraction(
                "job-forget",
                "worker-1",
                LearningExtraction {
                    experiences: vec![extracted(
                        ExtractedExperienceKind::Procedure,
                        "Promoted procedure",
                        Some("Keep the verified rule."),
                    )],
                    memories: vec![ExtractedMemory {
                        experience_ordinal: 0,
                        scope: ExtractedMemoryScope::Project,
                        action: ExtractedMemoryAction::Add,
                        content: Some("Keep the verified rule.".to_owned()),
                        old_text: None,
                        reason: "verified project procedure".to_owned(),
                        confidence: 0.99,
                    }],
                },
                20,
            )
            .expect("persist");
        let experience_id = persisted.experiences[0].projection.id.clone();
        let cleanup = service
            .prepare_cleanup_for_experiences(
                std::slice::from_ref(&experience_id),
                Some("session-1"),
                30,
            )
            .expect("prepare cleanup");

        assert_eq!(
            cleanup.forgotten_experience_ids,
            std::slice::from_ref(&experience_id)
        );
        assert_eq!(cleanup.memory_revocation_candidate_ids.len(), 1);
        assert_eq!(
            service
                .get(&experience_id)
                .expect("experience")
                .projection
                .status,
            zuno_types::ExperienceStatus::Forgotten
        );
        assert_eq!(
            memory
                .entries()
                .expect("resident memory is not silently changed")
                .into_iter()
                .map(|entry| entry.content)
                .collect::<Vec<_>>(),
            ["Keep the verified rule."]
        );
        let revocation = memory
            .candidate(&cleanup.memory_revocation_candidate_ids[0])
            .expect("review candidate");
        assert_eq!(revocation.projection.status, MemoryCandidateStatus::Pending);
        assert_eq!(revocation.projection.action, MemoryAction::Remove);
    }
}
