use crate::{LearningServiceError, Result, digest_text};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use similar::TextDiff;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use zuno_config::ResolvedLearningConfig;
use zuno_db::evaluation::{EvaluationCaseKind, NewEvaluationCase, NewEvaluationSuite};
use zuno_db::experience::{ExperienceRecord, ExperienceStore};
use zuno_db::learning_pattern::{LearningPatternRecord, LearningPatternStore, PatternScope};
use zuno_db::skill_candidate::{NewSkillCandidate, SkillCandidateRecord, SkillCandidateStore};
use zuno_error::LearningError;
use zuno_eval::{
    AttemptSnapshot, CandidateEvaluationRequest, EvaluationDecision, EvaluationService,
    OfflineCaseEvaluator,
};
use zuno_types::{LearningPatternStatus, SkillCandidateOperation, SkillCandidateStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillTarget {
    /// Exact catalog source identity, not only a display name.
    pub source_identity: String,
    /// Existing source digest captured while the candidate was generated.
    pub source_digest: String,
    /// Writable source path, or destination of a project companion Skill.
    pub target_path: PathBuf,
    pub writable: bool,
    /// Required for a built-in or otherwise read-only source.
    pub companion_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCandidateRequest {
    pub pattern_id: String,
    pub name: String,
    pub baseline_content: String,
    pub proposed_content: String,
    pub target: SkillTarget,
    pub explicit_promotion: bool,
    pub time_created: i64,
}

struct PatternCandidateRequest {
    pattern: LearningPatternRecord,
    target_project_id: Option<String>,
    evidence_ids: Option<Vec<String>>,
    name: String,
    baseline_content: String,
    proposed_content: String,
    target: SkillTarget,
    explicit_promotion: bool,
    time_created: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillCleanupPreparation {
    pub revocation_candidate_ids: Vec<String>,
    pub rejected_candidate_ids: Vec<String>,
}

#[async_trait]
pub trait SkillSourceResolver: Send + Sync {
    async fn read_source(
        &self,
        source_identity: &str,
    ) -> std::result::Result<String, LearningError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileSnapshot {
    exists: bool,
    content: String,
}

#[derive(Clone)]
pub struct SkillCandidateService {
    candidates: SkillCandidateStore,
    patterns: LearningPatternStore,
    experiences: ExperienceStore,
    evaluation: EvaluationService,
    config: ResolvedLearningConfig,
}

impl SkillCandidateService {
    #[must_use]
    pub fn new(pool: Arc<zuno_db::Pool>, config: ResolvedLearningConfig) -> Self {
        Self {
            candidates: SkillCandidateStore::new(pool.clone()),
            patterns: LearningPatternStore::new(pool.clone()),
            experiences: ExperienceStore::new(pool.clone()),
            evaluation: EvaluationService::new(pool),
            config,
        }
    }

    pub fn create_from_pattern(
        &self,
        request: SkillCandidateRequest,
    ) -> Result<SkillCandidateRecord> {
        let pattern = self.patterns.get(&request.pattern_id)?;
        self.create_from_pattern_record(PatternCandidateRequest {
            pattern,
            target_project_id: None,
            evidence_ids: None,
            name: request.name,
            baseline_content: request.baseline_content,
            proposed_content: request.proposed_content,
            target: request.target,
            explicit_promotion: request.explicit_promotion,
            time_created: request.time_created,
        })
    }

    fn create_from_pattern_record(
        &self,
        request: PatternCandidateRequest,
    ) -> Result<SkillCandidateRecord> {
        let PatternCandidateRequest {
            pattern,
            target_project_id,
            evidence_ids,
            name,
            baseline_content,
            proposed_content,
            target,
            explicit_promotion,
            time_created,
        } = request;
        if !explicit_promotion
            && pattern.projection.independent_sessions < self.config.skill_min_independent_sessions
        {
            return Err(invalid(
                "pattern.independent_sessions",
                "automatic Skill candidates require independent session support",
            ));
        }
        if pattern.projection.learned_rules.is_empty()
            || pattern.projection.learned_rules.len() > self.config.skill_max_learned_rules as usize
        {
            return Err(invalid(
                "pattern.learned_rules",
                "Skill candidates must contain 1..=15 learned rules",
            ));
        }
        if !self.config.skill_require_review {
            return Err(invalid(
                "learning.skill.require_review",
                "Skill review cannot be disabled",
            ));
        }
        let observed_source_digest = digest_text(&baseline_content);
        if observed_source_digest != target.source_digest {
            return Err(LearningError::SkillSourceDrift {
                candidate_id: "not-created".to_owned(),
                expected_digest: target.source_digest,
                observed_digest: observed_source_digest,
            }
            .into());
        }
        if !target.writable
            && target
                .companion_name
                .as_deref()
                .is_none_or(|companion| companion.trim().is_empty() || companion == name)
        {
            return Err(invalid(
                "target.companion_name",
                "read-only sources require a distinct project companion Skill name",
            ));
        }
        let candidate_name = target.companion_name.as_deref().unwrap_or(&name).to_owned();
        let diff = TextDiff::from_lines(&baseline_content, &proposed_content)
            .unified_diff()
            .header(&target.source_identity, &candidate_name)
            .to_string();
        let project_id = match pattern.scope {
            PatternScope::Project => {
                let project_id = pattern.projection.project_id.clone().ok_or_else(|| {
                    invalid(
                        "pattern.project_id",
                        "project pattern has no project identity",
                    )
                })?;
                if target_project_id
                    .as_deref()
                    .is_some_and(|target| target != project_id)
                {
                    return Err(invalid(
                        "target.project_id",
                        "project pattern cannot create a Skill candidate in another project",
                    ));
                }
                project_id
            }
            PatternScope::Global => {
                if pattern.projection.project_count < self.config.global_promotion_min_projects {
                    return Err(invalid(
                        "pattern.project_count",
                        "global Skill candidates require independent cross-project support",
                    ));
                }
                target_project_id
                    .as_deref()
                    .filter(|project_id| !project_id.trim().is_empty())
                    .ok_or_else(|| {
                        invalid(
                            "target.project_id",
                            "global patterns need an explicit target project before Skill creation",
                        )
                    })?
                    .to_owned()
            }
        };
        let proposed_digest = digest_text(&proposed_content);
        let candidate = self.candidates.create(NewSkillCandidate {
            id: format!("skc_{}", Uuid::now_v7().simple()),
            project_id,
            pattern_id: Some(pattern.projection.id.clone()),
            name: candidate_name,
            target_source: target.source_identity,
            target_path: Some(target.target_path.to_string_lossy().into_owned()),
            target_writable: target.writable,
            target_digest: target.source_digest,
            proposed_content,
            proposed_digest,
            diff,
            evidence_ids: evidence_ids.unwrap_or(pattern.evidence_ids),
            learned_rules: pattern.projection.learned_rules,
            operation: zuno_types::SkillCandidateOperation::Apply,
            reverts_candidate_id: None,
            companion_name: target.companion_name,
            time_created,
        })?;
        if pattern.projection.status == LearningPatternStatus::Pending {
            self.patterns
                .promote(&pattern.projection.id, time_created)?;
        }
        Ok(candidate)
    }

    /// Build a complete project companion Skill from one pattern. This is the
    /// automatic proposal path: it creates a reviewable candidate only and
    /// never writes the destination.
    pub fn create_companion_from_pattern(
        &self,
        pattern_id: &str,
        project_root: &Path,
        explicit_promotion: bool,
        now: i64,
    ) -> Result<Option<SkillCandidateRecord>> {
        let pattern = self.patterns.get(pattern_id)?;
        let project_id = pattern.projection.project_id.clone().ok_or_else(|| {
            invalid(
                "pattern.project_id",
                "global patterns require create_companion_from_pattern_for_project",
            )
        })?;
        self.create_companion_from_pattern_record(
            pattern,
            &project_id,
            project_root,
            explicit_promotion,
            now,
        )
    }

    /// Materialize either a project pattern or an eligible cross-project
    /// pattern as a reviewable companion Skill in one explicit target project.
    pub fn create_companion_from_pattern_for_project(
        &self,
        pattern_id: &str,
        target_project_id: &str,
        project_root: &Path,
        explicit_promotion: bool,
        now: i64,
    ) -> Result<Option<SkillCandidateRecord>> {
        let pattern = self.patterns.get(pattern_id)?;
        self.create_companion_from_pattern_record(
            pattern,
            target_project_id,
            project_root,
            explicit_promotion,
            now,
        )
    }

    fn create_companion_from_pattern_record(
        &self,
        pattern: LearningPatternRecord,
        target_project_id: &str,
        project_root: &Path,
        explicit_promotion: bool,
        now: i64,
    ) -> Result<Option<SkillCandidateRecord>> {
        if !explicit_promotion
            && pattern.projection.independent_sessions < self.config.skill_min_independent_sessions
        {
            return Ok(None);
        }
        let evidence_ids = match pattern.scope {
            PatternScope::Project => pattern.evidence_ids.clone(),
            PatternScope::Global => self.global_experience_evidence(&pattern)?,
        };
        let suffix = pattern
            .projection
            .id
            .strip_prefix("pat_")
            .unwrap_or(&pattern.projection.id);
        let fallback = format!("learned-{}", suffix.chars().take(12).collect::<String>());
        let base = skill_slug(&pattern.projection.title);
        let name = if base.is_empty() {
            fallback
        } else {
            format!("learned-{base}")
        };
        let proposed = render_companion_skill(
            &name,
            &pattern.projection.title,
            &pattern.projection.summary,
            &pattern.projection.learned_rules,
            &evidence_ids,
        );
        let pattern_id = pattern.projection.id.clone();
        self.create_from_pattern_record(PatternCandidateRequest {
            pattern,
            target_project_id: Some(target_project_id.to_owned()),
            evidence_ids: Some(evidence_ids),
            name: "learning-pattern-source".to_owned(),
            baseline_content: String::new(),
            proposed_content: proposed,
            target: SkillTarget {
                source_identity: format!("learning://pattern/{pattern_id}"),
                source_digest: digest_text(""),
                target_path: project_root
                    .join(".agents")
                    .join("skills")
                    .join(&name)
                    .join("SKILL.md"),
                writable: false,
                companion_name: Some(name),
            },
            explicit_promotion,
            time_created: now,
        })
        .map(Some)
    }

    fn global_experience_evidence(&self, pattern: &LearningPatternRecord) -> Result<Vec<String>> {
        if pattern.projection.project_count < self.config.global_promotion_min_projects {
            return Err(invalid(
                "pattern.project_count",
                "global Skill candidates require independent cross-project support",
            ));
        }
        let mut project_ids = BTreeSet::new();
        let mut experience_ids = BTreeSet::new();
        for source_pattern_id in &pattern.evidence_ids {
            let source = self.patterns.get(source_pattern_id)?;
            if source.scope != PatternScope::Project
                || source.projection.status != LearningPatternStatus::Promoted
            {
                return Err(invalid(
                    "pattern.evidence_ids",
                    "global patterns must cite promoted project patterns",
                ));
            }
            let project_id = source.projection.project_id.as_deref().ok_or_else(|| {
                invalid(
                    "pattern.evidence_ids",
                    "global pattern source has no project identity",
                )
            })?;
            project_ids.insert(project_id.to_owned());
            experience_ids.extend(source.evidence_ids);
        }
        if project_ids.len() < self.config.global_promotion_min_projects as usize {
            return Err(invalid(
                "pattern.evidence_ids",
                "global Skill candidates require evidence from independent projects",
            ));
        }
        if experience_ids.is_empty() {
            return Err(invalid(
                "pattern.evidence_ids",
                "global Skill candidates require Experience evidence",
            ));
        }
        Ok(experience_ids.into_iter().collect())
    }

    /// Human approval enters evaluation. Passing evaluation does not apply the file.
    pub async fn review_and_evaluate(
        &self,
        candidate_id: &str,
        suite_id: &str,
        baseline_skill: &str,
        attempt: AttemptSnapshot,
        evaluator: &dyn OfflineCaseEvaluator,
        now: i64,
    ) -> Result<EvaluationDecision> {
        let candidate = self.candidates.begin_evaluation(candidate_id, now)?;
        let request = CandidateEvaluationRequest {
            suite_id: suite_id.to_owned(),
            candidate_id: candidate_id.to_owned(),
            baseline_skill: baseline_skill.to_owned(),
            candidate_skill: candidate.proposed_content.clone(),
            attempt,
            time_created: now,
            time_completed: zuno_db::message::now_millis(),
        };
        let decision = match self.evaluation.evaluate_candidate(request, evaluator).await {
            Ok(decision) => decision,
            Err(error) => {
                let _ = self.candidates.fail_evaluation(
                    candidate_id,
                    &error.to_string(),
                    zuno_db::message::now_millis(),
                );
                return Err(error.into());
            }
        };
        self.candidates.settle_evaluation(
            candidate_id,
            &decision.run_id,
            decision.passed,
            (!decision.passed).then_some("offline evaluation policy rejected the candidate"),
            zuno_db::message::now_millis(),
        )?;
        if !decision.passed {
            return Err(LearningError::EvaluationRejected {
                candidate_id: candidate_id.to_owned(),
            }
            .into());
        }
        Ok(decision)
    }

    /// Materialize the immutable, cassette-only suite used to review one
    /// candidate. Retries verify the exact suite instead of silently replacing
    /// cases after evidence has changed.
    pub fn ensure_evaluation_suite(&self, candidate_id: &str, now: i64) -> Result<String> {
        let candidate = self.candidates.get(candidate_id)?;
        let evidence_ids = candidate
            .evidence_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut cases = Vec::new();
        for (index, evidence_id) in candidate.evidence_ids.iter().enumerate() {
            let experience = self.experiences.get(evidence_id).map_err(|error| {
                LearningError::InvalidRequest {
                    field: "candidate.evidence_ids".to_owned(),
                    detail: format!(
                        "Skill candidate `{candidate_id}` cites unavailable experience `{evidence_id}`: {error}"
                    ),
                }
            })?;
            if !experience.projection.kind.promotable() {
                return Err(invalid(
                    "candidate.evidence_ids",
                    "unresolved issues cannot become Skill evaluation evidence",
                ));
            }
            let kind = match experience.projection.kind {
                zuno_types::ExperienceKind::Problem
                | zuno_types::ExperienceKind::UserCorrection
                | zuno_types::ExperienceKind::ExplicitFeedback => EvaluationCaseKind::Failure,
                zuno_types::ExperienceKind::Outcome | zuno_types::ExperienceKind::Procedure => {
                    EvaluationCaseKind::General
                }
                zuno_types::ExperienceKind::UnresolvedIssue => unreachable!(
                    "non-promotable unresolved evidence was rejected before classification"
                ),
            };
            cases.push(evaluation_case(
                candidate_id,
                index,
                "evidence",
                &experience,
                kind,
                if kind == EvaluationCaseKind::Failure {
                    2
                } else {
                    1
                },
            ));
        }

        let protection = self
            .experiences
            .list_for_project(&candidate.projection.project_id, 100)?
            .into_iter()
            .filter(|record| {
                record.projection.kind.promotable() && !evidence_ids.contains(&record.projection.id)
            })
            .take(3);
        let protection_start = cases.len();
        for (offset, experience) in protection.enumerate() {
            cases.push(evaluation_case(
                candidate_id,
                protection_start + offset,
                "protection",
                &experience,
                EvaluationCaseKind::Protection,
                1,
            ));
        }
        if cases.is_empty() {
            return Err(invalid(
                "candidate.evidence_ids",
                "Skill evaluation requires at least one recorded experience",
            ));
        }

        let suite_id = format!("suite_{candidate_id}");
        self.evaluation.ensure_suite(NewEvaluationSuite {
            id: suite_id.clone(),
            project_id: candidate.projection.project_id.clone(),
            name: format!("Skill candidate {candidate_id}"),
            description: format!(
                "Immutable offline cassette suite for proposed digest {}",
                candidate.projection.proposed_digest
            ),
            cases,
            time_created: now,
        })?;
        Ok(suite_id)
    }

    /// Create one reviewable reversal for every applied Skill supported by the
    /// experiences being removed. This copies the original snapshots and never
    /// mutates the Skill on its own.
    pub fn prepare_cleanup_for_experiences(
        &self,
        experience_ids: &[String],
        now: i64,
    ) -> Result<SkillCleanupPreparation> {
        let mut preparation = SkillCleanupPreparation::default();
        for candidate in self.candidates.list_referencing(experience_ids)? {
            match candidate.projection.status {
                SkillCandidateStatus::Applied
                    if candidate.projection.operation == SkillCandidateOperation::Apply =>
                {
                    let revocation = self.create_revocation_candidate(&candidate, now)?;
                    preparation
                        .revocation_candidate_ids
                        .push(revocation.projection.id);
                }
                SkillCandidateStatus::PendingReview
                | SkillCandidateStatus::Evaluating
                | SkillCandidateStatus::Approved
                | SkillCandidateStatus::Failed => {
                    let rejected = self.candidates.reject(&candidate.projection.id, now)?;
                    preparation
                        .rejected_candidate_ids
                        .push(rejected.projection.id);
                }
                SkillCandidateStatus::Applying
                | SkillCandidateStatus::Undoing
                | SkillCandidateStatus::Uncertain => {
                    return Err(invalid(
                        "session.cleanup",
                        &format!(
                            "Skill candidate `{}` is {}; reconcile it before deleting derived experience",
                            candidate.projection.id,
                            candidate.projection.status.as_str()
                        ),
                    ));
                }
                SkillCandidateStatus::Applied
                | SkillCandidateStatus::Rejected
                | SkillCandidateStatus::Stale
                | SkillCandidateStatus::Undone => {}
            }
        }
        Ok(preparation)
    }

    fn create_revocation_candidate(
        &self,
        original: &SkillCandidateRecord,
        now: i64,
    ) -> Result<SkillCandidateRecord> {
        if original.projection.status != SkillCandidateStatus::Applied
            || original.projection.operation != SkillCandidateOperation::Apply
        {
            return Err(invalid(
                "candidate.status",
                "only an applied Skill change can produce a revocation candidate",
            ));
        }
        let before = decode_snapshot(original.before_content.as_deref(), &original.projection.id)?;
        let after = decode_snapshot(original.after_content.as_deref(), &original.projection.id)?;
        let path = candidate_path(original)?;
        let name = format!("revoke-{}", original.projection.name);
        let target_source = format!("file://{}", path.to_string_lossy());
        let proposed_digest = digest_text(&before.content);
        let diff = TextDiff::from_lines(&after.content, &before.content)
            .unified_diff()
            .header(&original.projection.name, &name)
            .to_string();
        self.candidates
            .create(NewSkillCandidate {
                id: format!("skc_{}", Uuid::now_v7().simple()),
                project_id: original.projection.project_id.clone(),
                pattern_id: None,
                name,
                target_source,
                target_path: Some(path.to_string_lossy().into_owned()),
                target_writable: true,
                target_digest: digest_text(&after.content),
                proposed_content: before.content,
                proposed_digest,
                diff,
                evidence_ids: original.evidence_ids.clone(),
                learned_rules: original.projection.learned_rules.clone(),
                operation: SkillCandidateOperation::Revoke,
                reverts_candidate_id: Some(original.projection.id.clone()),
                companion_name: original.companion_name.clone(),
                time_created: now,
            })
            .map_err(Into::into)
    }

    /// Apply one already reviewed and evaluated candidate with source CAS.
    pub async fn apply(
        &self,
        id: &str,
        resolver: &dyn SkillSourceResolver,
        now: i64,
    ) -> Result<SkillCandidateRecord> {
        let candidate = self.candidates.get(id)?;
        if candidate.projection.status != SkillCandidateStatus::Approved {
            return Err(LearningError::SkillReviewRequired {
                candidate_id: id.to_owned(),
            }
            .into());
        }
        let source = resolver
            .read_source(&candidate.projection.target_source)
            .await?;
        let observed_digest = digest_text(&source);
        if observed_digest != candidate.projection.target_digest {
            self.candidates.mark_stale(
                id,
                "Skill source changed after candidate generation",
                now,
            )?;
            return Err(LearningError::SkillSourceDrift {
                candidate_id: id.to_owned(),
                expected_digest: candidate.projection.target_digest,
                observed_digest,
            }
            .into());
        }
        let path = candidate_path(&candidate)?;
        let before = read_snapshot(&path)?;
        if !candidate.target_writable && before.exists {
            self.candidates
                .mark_stale(id, "project companion destination already exists", now)?;
            return Err(LearningError::SkillSourceDrift {
                candidate_id: id.to_owned(),
                expected_digest: digest_text(""),
                observed_digest: digest_text(&before.content),
            }
            .into());
        }
        if candidate.target_writable
            && (!before.exists
                || digest_text(&before.content) != candidate.projection.target_digest)
        {
            let observed = if before.exists {
                digest_text(&before.content)
            } else {
                digest_text("")
            };
            self.candidates
                .mark_stale(id, "writable Skill target drifted before apply", now)?;
            return Err(LearningError::SkillSourceDrift {
                candidate_id: id.to_owned(),
                expected_digest: candidate.projection.target_digest,
                observed_digest: observed,
            }
            .into());
        }
        let after = if let Some(original_id) = candidate.projection.reverts_candidate_id.as_deref()
        {
            let original = self.candidates.get(original_id)?;
            let original_before = decode_snapshot(original.before_content.as_deref(), original_id)?;
            let original_after = decode_snapshot(original.after_content.as_deref(), original_id)?;
            if before != original_after {
                self.candidates.mark_stale(
                    id,
                    "Skill target no longer matches the applied source",
                    now,
                )?;
                return Err(LearningError::SkillSourceDrift {
                    candidate_id: id.to_owned(),
                    expected_digest: digest_text(&original_after.content),
                    observed_digest: digest_text(&before.content),
                }
                .into());
            }
            original_before
        } else {
            FileSnapshot {
                exists: true,
                content: candidate.proposed_content.clone(),
            }
        };
        let operation_id = format!("ska_{}", Uuid::now_v7().simple());
        let before_json = serde_json::to_string(&before).expect("FileSnapshot is serializable");
        let after_json = serde_json::to_string(&after).expect("FileSnapshot is serializable");
        self.candidates
            .begin_apply(id, &operation_id, &before_json, &after_json, now)?;
        match write_snapshot(&path, &after) {
            Ok(()) => self
                .candidates
                .finish_effect(
                    id,
                    SkillCandidateStatus::Applying,
                    SkillCandidateStatus::Applied,
                    None,
                    zuno_db::message::now_millis(),
                )
                .map_err(Into::into),
            Err(error) => {
                let status = reconcile_snapshot(&path, &before, &after, true)?;
                self.candidates.finish_effect(
                    id,
                    SkillCandidateStatus::Applying,
                    status,
                    Some(&error.to_string()),
                    zuno_db::message::now_millis(),
                )?;
                Err(error)
            }
        }
    }

    pub fn undo(&self, id: &str, now: i64) -> Result<SkillCandidateRecord> {
        let candidate = self.candidates.get(id)?;
        if candidate.projection.status != SkillCandidateStatus::Applied {
            return Err(invalid(
                "candidate.status",
                "only an applied Skill candidate can be undone",
            ));
        }
        let before = decode_snapshot(candidate.before_content.as_deref(), id)?;
        let after = decode_snapshot(candidate.after_content.as_deref(), id)?;
        let path = candidate_path(&candidate)?;
        let current = read_snapshot(&path)?;
        if current != after {
            self.candidates
                .mark_stale(id, "Skill target changed after apply", now)?;
            return Err(LearningError::SkillSourceDrift {
                candidate_id: id.to_owned(),
                expected_digest: digest_text(&after.content),
                observed_digest: digest_text(&current.content),
            }
            .into());
        }
        self.candidates.begin_undo(id, now)?;
        match write_snapshot(&path, &before) {
            Ok(()) => self
                .candidates
                .finish_effect(
                    id,
                    SkillCandidateStatus::Undoing,
                    SkillCandidateStatus::Undone,
                    None,
                    zuno_db::message::now_millis(),
                )
                .map_err(Into::into),
            Err(error) => {
                let status = reconcile_snapshot(&path, &after, &before, false)?;
                self.candidates.finish_effect(
                    id,
                    SkillCandidateStatus::Undoing,
                    status,
                    Some(&error.to_string()),
                    zuno_db::message::now_millis(),
                )?;
                Err(error)
            }
        }
    }

    /// Reconcile interrupted filesystem effects without replaying them.
    pub fn reconcile(&self, now: i64) -> Result<usize> {
        let mut reconciled = self.evaluation.reconcile_running(now)?;
        reconciled = reconciled.saturating_add(self.candidates.fail_interrupted_evaluations(now)?);
        let inflight = self.candidates.list_inflight()?;
        for candidate in inflight {
            let before = decode_snapshot(
                candidate.before_content.as_deref(),
                &candidate.projection.id,
            )?;
            let after =
                decode_snapshot(candidate.after_content.as_deref(), &candidate.projection.id)?;
            let path = candidate_path(&candidate)?;
            let current = read_snapshot(&path)?;
            let (expected, status) = match candidate.projection.status {
                SkillCandidateStatus::Applying if current == after => (
                    SkillCandidateStatus::Applying,
                    SkillCandidateStatus::Applied,
                ),
                SkillCandidateStatus::Applying if current == before => {
                    (SkillCandidateStatus::Applying, SkillCandidateStatus::Failed)
                }
                SkillCandidateStatus::Undoing if current == before => {
                    (SkillCandidateStatus::Undoing, SkillCandidateStatus::Undone)
                }
                SkillCandidateStatus::Undoing if current == after => {
                    (SkillCandidateStatus::Undoing, SkillCandidateStatus::Failed)
                }
                SkillCandidateStatus::Applying => (
                    SkillCandidateStatus::Applying,
                    SkillCandidateStatus::Uncertain,
                ),
                SkillCandidateStatus::Undoing => (
                    SkillCandidateStatus::Undoing,
                    SkillCandidateStatus::Uncertain,
                ),
                _ => continue,
            };
            self.candidates.finish_effect(
                &candidate.projection.id,
                expected,
                status,
                (status != SkillCandidateStatus::Applied && status != SkillCandidateStatus::Undone)
                    .then_some("reconciled from authoritative filesystem state"),
                now,
            )?;
            reconciled += 1;
        }
        Ok(reconciled)
    }

    pub fn reject(&self, id: &str, now: i64) -> Result<SkillCandidateRecord> {
        self.candidates.reject(id, now).map_err(Into::into)
    }

    pub fn get(&self, id: &str) -> Result<SkillCandidateRecord> {
        self.candidates.get(id).map_err(Into::into)
    }

    pub fn list_for_project(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<SkillCandidateRecord>> {
        self.candidates
            .list_for_project(project_id, limit)
            .map_err(Into::into)
    }
}

fn evaluation_case(
    candidate_id: &str,
    index: usize,
    role: &str,
    experience: &ExperienceRecord,
    kind: EvaluationCaseKind,
    weight: u32,
) -> NewEvaluationCase {
    let projection = &experience.projection;
    let expected = projection
        .resolution
        .as_deref()
        .unwrap_or(&projection.summary)
        .trim()
        .to_owned();
    let evidence = experience
        .evidence
        .iter()
        .map(|item| {
            json!({
                "kind": item.kind.as_str(),
                "sourceID": item.source_id,
                "excerpt": item.excerpt,
                "digest": item.digest,
            })
        })
        .collect::<Vec<_>>();
    NewEvaluationCase {
        id: format!("case_{candidate_id}_{index:03}"),
        name: format!("{role}:{}", projection.id),
        prompt: format!(
            "Respond to the recorded work situation using only the supplied cassette.\
\nTitle: {}\nSituation: {}",
            projection.title, projection.summary
        ),
        expected,
        tool_cassette: json!({
            "mode": "recorded-only",
            "experienceID": projection.id,
            "sessionID": projection.session_id,
            "sourceMessageID": projection.source_message_id,
            "kind": projection.kind.as_str(),
            "evidence": evidence,
        }),
        kind,
        weight,
    }
}

fn skill_slug(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('-');
            }
            output.push(character);
            separator = false;
        } else {
            separator = true;
        }
        if output.len() >= 48 {
            break;
        }
    }
    output.trim_matches('-').to_owned()
}

fn render_companion_skill(
    name: &str,
    title: &str,
    summary: &str,
    learned_rules: &[String],
    evidence_ids: &[String],
) -> String {
    let rules = learned_rules
        .iter()
        .map(|rule| format!("- {}", rule.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    let evidence = evidence_ids
        .iter()
        .map(|id| format!("- `{id}`"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "---\nname: {name}\ndescription: Project companion Skill proposed from reviewed Zuno \
learning evidence.\n---\n\n# {title}\n\n{summary}\n\n## Learned rules\n\n{rules}\n\n\
## Evidence\n\n{evidence}\n"
    )
}

fn candidate_path(candidate: &SkillCandidateRecord) -> Result<PathBuf> {
    candidate
        .target_path
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| invalid("candidate.target_path", "candidate has no target path"))
}

fn decode_snapshot(snapshot: Option<&str>, id: &str) -> Result<FileSnapshot> {
    let snapshot = snapshot.ok_or_else(|| {
        invalid(
            "candidate.snapshot",
            &format!("Skill candidate `{id}` has no effect snapshot"),
        )
    })?;
    serde_json::from_str(snapshot).map_err(|_| {
        invalid(
            "candidate.snapshot",
            &format!("Skill candidate `{id}` has a corrupt effect snapshot"),
        )
    })
}

fn read_snapshot(path: &Path) -> Result<FileSnapshot> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(FileSnapshot {
            exists: true,
            content,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(FileSnapshot {
            exists: false,
            content: String::new(),
        }),
        Err(source) => Err(LearningError::Io {
            operation: "read Skill snapshot".to_owned(),
            path: path.to_path_buf(),
            source,
        }
        .into()),
    }
}

fn write_snapshot(path: &Path, snapshot: &FileSnapshot) -> Result<()> {
    if !snapshot.exists {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(LearningError::Io {
                    operation: "remove Skill companion".to_owned(),
                    path: path.to_path_buf(),
                    source,
                }
                .into());
            }
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LearningError::Io {
            operation: "create Skill directory".to_owned(),
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let temporary = path.with_extension(format!("tmp.{nanos}"));
    fs::write(&temporary, &snapshot.content).map_err(|source| LearningError::Io {
        operation: "write Skill temporary file".to_owned(),
        path: temporary.clone(),
        source,
    })?;
    fs::rename(&temporary, path).map_err(|source| {
        let _ = fs::remove_file(&temporary);
        LearningError::Io {
            operation: "rename Skill into place".to_owned(),
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

fn reconcile_snapshot(
    path: &Path,
    before: &FileSnapshot,
    after: &FileSnapshot,
    applying: bool,
) -> Result<SkillCandidateStatus> {
    let current = read_snapshot(path)?;
    Ok(if current == *after {
        if applying {
            SkillCandidateStatus::Applied
        } else {
            SkillCandidateStatus::Undone
        }
    } else if current == *before {
        SkillCandidateStatus::Failed
    } else {
        SkillCandidateStatus::Uncertain
    })
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
    use zuno_db::experience::{ExperienceEvidenceKind, NewExperience, NewExperienceEvidence};
    use zuno_db::learning_pattern::{LearningPatternStore, NewLearningPattern, PatternScope};
    use zuno_db::migration;
    use zuno_error::BoxSource;
    use zuno_paths::DbLocation;
    use zuno_types::{ExperienceKind, SkillCandidateOperation};

    struct ImprovingEvaluator;

    #[async_trait]
    impl OfflineCaseEvaluator for ImprovingEvaluator {
        async fn evaluate(
            &self,
            request: zuno_eval::OfflineCaseRequest,
        ) -> std::result::Result<zuno_eval::CaseObservation, BoxSource> {
            let candidate =
                !request.skill_content.is_empty() && request.skill_content != "# Original\n";
            Ok(zuno_eval::CaseObservation {
                score: if candidate { 10 } else { 5 },
                passed: candidate,
                critical_failure: false,
                details: json!({"recordedOnly": request.tool_cassette["mode"]}),
            })
        }
    }

    struct TestResolver;

    #[async_trait]
    impl SkillSourceResolver for TestResolver {
        async fn read_source(
            &self,
            source_identity: &str,
        ) -> std::result::Result<String, LearningError> {
            if source_identity.starts_with("learning://pattern/") {
                return Ok(String::new());
            }
            let path = source_identity
                .strip_prefix("file://")
                .unwrap_or(source_identity);
            std::fs::read_to_string(path).map_err(|source| LearningError::Io {
                operation: "read test Skill source".to_owned(),
                path: PathBuf::from(path),
                source,
            })
        }
    }

    fn fixture() -> Arc<zuno_db::Pool> {
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
                     VALUES ('session-1', 'project-1', 'slug', '/workspace', 'title', '1', 1, 1);",
                )
                .expect("fixture");
        }
        ExperienceStore::new(Arc::clone(&pool))
            .create_manual(NewExperience {
                id: "experience-1".to_owned(),
                project_id: "project-1".to_owned(),
                session_id: Some("session-1".to_owned()),
                source_message_id: None,
                extraction_job_id: None,
                extraction_ordinal: None,
                kind: ExperienceKind::Problem,
                title: "Repeated repository failure".to_owned(),
                summary: "The old workflow missed a durable check.".to_owned(),
                resolution: Some("Use the learned candidate workflow.".to_owned()),
                confidence: 9_800,
                fingerprint: "repository-failure".to_owned(),
                evidence: vec![NewExperienceEvidence {
                    id: "evidence-1".to_owned(),
                    kind: ExperienceEvidenceKind::Tool,
                    source_id: None,
                    excerpt: "recorded failure and fix".to_owned(),
                    digest: "evidence-digest".to_owned(),
                }],
                time_created: 2,
            })
            .expect("experience");
        LearningPatternStore::new(Arc::clone(&pool))
            .propose(NewLearningPattern {
                id: "pattern-1".to_owned(),
                scope: PatternScope::Project,
                project_id: Some("project-1".to_owned()),
                fingerprint: "repository-failure".to_owned(),
                title: "Repository verification".to_owned(),
                summary: "Three sessions support this method.".to_owned(),
                learned_rules: vec!["Use the durable candidate workflow.".to_owned()],
                evidence_ids: vec!["experience-1".to_owned()],
                evidence_digest: "pattern-evidence".to_owned(),
                evidence_version: 1,
                independent_sessions: 3,
                project_count: 1,
                time_created: 3,
            })
            .expect("pattern");
        pool
    }

    fn attempt() -> AttemptSnapshot {
        AttemptSnapshot {
            model: "provider/evaluator".to_owned(),
            toolset_digest: "toolset".to_owned(),
            max_output_tokens: 1_000,
            max_steps: 4,
            temperature_millis: 0,
            seed: 7,
        }
    }

    async fn approve(service: &SkillCandidateService, candidate_id: &str, baseline: &str) {
        let suite = service
            .ensure_evaluation_suite(candidate_id, 10)
            .expect("suite");
        let decision = service
            .review_and_evaluate(
                candidate_id,
                &suite,
                baseline,
                attempt(),
                &ImprovingEvaluator,
                11,
            )
            .await
            .expect("review and evaluation");
        assert!(decision.passed);
    }

    #[tokio::test]
    async fn source_drift_marks_an_approved_candidate_stale_without_overwrite() {
        let pool = fixture();
        let service =
            SkillCandidateService::new(Arc::clone(&pool), ResolvedLearningConfig::default());
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("SKILL.md");
        fs::write(&path, "# Original\n").expect("baseline");
        let candidate = service
            .create_from_pattern(SkillCandidateRequest {
                pattern_id: "pattern-1".to_owned(),
                name: "repository-check".to_owned(),
                baseline_content: "# Original\n".to_owned(),
                proposed_content: "# Candidate\n".to_owned(),
                target: SkillTarget {
                    source_identity: format!("file://{}", path.display()),
                    source_digest: digest_text("# Original\n"),
                    target_path: path.clone(),
                    writable: true,
                    companion_name: None,
                },
                explicit_promotion: false,
                time_created: 4,
            })
            .expect("candidate");
        approve(&service, &candidate.projection.id, "# Original\n").await;

        fs::write(&path, "# External drift\n").expect("drift");
        let error = service
            .apply(&candidate.projection.id, &TestResolver, 20)
            .await
            .expect_err("drift must stop apply");
        assert!(matches!(
            error,
            LearningServiceError::Learning(LearningError::SkillSourceDrift { .. })
        ));
        assert_eq!(
            service
                .get(&candidate.projection.id)
                .expect("candidate")
                .projection
                .status,
            SkillCandidateStatus::Stale
        );
        assert_eq!(
            fs::read_to_string(path).expect("unchanged source"),
            "# External drift\n"
        );
    }

    #[tokio::test]
    async fn cleanup_creates_a_reviewable_revocation_without_touching_applied_skill() {
        let pool = fixture();
        let service =
            SkillCandidateService::new(Arc::clone(&pool), ResolvedLearningConfig::default());
        let directory = tempfile::tempdir().expect("directory");
        let candidate = service
            .create_companion_from_pattern("pattern-1", directory.path(), false, 4)
            .expect("candidate")
            .expect("enough independent sessions");
        approve(&service, &candidate.projection.id, "").await;
        let applied = service
            .apply(&candidate.projection.id, &TestResolver, 20)
            .await
            .expect("apply");
        let path = candidate_path(&applied).expect("path");
        let applied_content = fs::read_to_string(&path).expect("applied content");

        let cleanup = service
            .prepare_cleanup_for_experiences(&["experience-1".to_owned()], 30)
            .expect("cleanup preparation");
        assert_eq!(cleanup.revocation_candidate_ids.len(), 1);
        assert!(cleanup.rejected_candidate_ids.is_empty());
        assert_eq!(
            fs::read_to_string(&path).expect("still applied"),
            applied_content
        );
        let revocation = service
            .get(&cleanup.revocation_candidate_ids[0])
            .expect("revocation");
        assert_eq!(
            revocation.projection.operation,
            SkillCandidateOperation::Revoke
        );
        assert_eq!(
            revocation.projection.status,
            SkillCandidateStatus::PendingReview
        );
    }

    #[test]
    fn global_pattern_creates_reviewable_companion_in_explicit_target_project() {
        let pool = fixture();
        {
            let connection = pool.get().expect("connection");
            connection
                .execute_batch(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('project-2', '/workspace-2', 1, 1, '[]');
                     INSERT INTO session
                       (id, project_id, slug, directory, title, version, time_created, time_updated)
                     VALUES (
                       'session-2', 'project-2', 'slug-2', '/workspace-2', 'title-2', '1', 1, 1
                     );",
                )
                .expect("second project");
        }
        ExperienceStore::new(Arc::clone(&pool))
            .create_manual(NewExperience {
                id: "experience-2".to_owned(),
                project_id: "project-2".to_owned(),
                session_id: Some("session-2".to_owned()),
                source_message_id: None,
                extraction_job_id: None,
                extraction_ordinal: None,
                kind: ExperienceKind::Procedure,
                title: "Repeated repository failure".to_owned(),
                summary: "The same durable check worked in another project.".to_owned(),
                resolution: Some("Use the learned candidate workflow.".to_owned()),
                confidence: 9_700,
                fingerprint: "repository-failure".to_owned(),
                evidence: vec![NewExperienceEvidence {
                    id: "evidence-2".to_owned(),
                    kind: ExperienceEvidenceKind::Tool,
                    source_id: None,
                    excerpt: "second project confirmed the workflow".to_owned(),
                    digest: "evidence-digest-2".to_owned(),
                }],
                time_created: 2,
            })
            .expect("second experience");
        let patterns = LearningPatternStore::new(Arc::clone(&pool));
        patterns.promote("pattern-1", 4).expect("promote first");
        patterns
            .propose(NewLearningPattern {
                id: "pattern-2".to_owned(),
                scope: PatternScope::Project,
                project_id: Some("project-2".to_owned()),
                fingerprint: "repository-failure".to_owned(),
                title: "Repository verification".to_owned(),
                summary: "Three sessions support this method.".to_owned(),
                learned_rules: vec!["Use the durable candidate workflow.".to_owned()],
                evidence_ids: vec!["experience-2".to_owned()],
                evidence_digest: "pattern-evidence-2".to_owned(),
                evidence_version: 2,
                independent_sessions: 3,
                project_count: 1,
                time_created: 5,
            })
            .expect("second pattern");
        patterns.promote("pattern-2", 6).expect("promote second");
        patterns
            .propose(NewLearningPattern {
                id: "pattern-global".to_owned(),
                scope: PatternScope::Global,
                project_id: None,
                fingerprint: "repository-failure".to_owned(),
                title: "Cross-project repository verification".to_owned(),
                summary: "Two independent projects support this method.".to_owned(),
                learned_rules: vec!["Use the durable candidate workflow.".to_owned()],
                evidence_ids: vec!["pattern-1".to_owned(), "pattern-2".to_owned()],
                evidence_digest: "global-pattern-evidence".to_owned(),
                evidence_version: 3,
                independent_sessions: 6,
                project_count: 2,
                time_created: 7,
            })
            .expect("global pattern");

        let service =
            SkillCandidateService::new(Arc::clone(&pool), ResolvedLearningConfig::default());
        let directory = tempfile::tempdir().expect("directory");
        let candidate = service
            .create_companion_from_pattern_for_project(
                "pattern-global",
                "project-1",
                directory.path(),
                true,
                8,
            )
            .expect("candidate")
            .expect("global candidate");

        assert_eq!(candidate.projection.project_id, "project-1");
        assert_eq!(
            candidate.evidence_ids,
            ["experience-1".to_owned(), "experience-2".to_owned()]
        );
        assert_eq!(
            candidate.projection.status,
            SkillCandidateStatus::PendingReview
        );
        assert_eq!(
            patterns
                .get("pattern-global")
                .expect("global pattern")
                .projection
                .status,
            LearningPatternStatus::Promoted
        );
        assert!(!candidate_path(&candidate).expect("path").exists());
    }
}
