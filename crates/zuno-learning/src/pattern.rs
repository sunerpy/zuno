use crate::{Result, digest_text};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;
use zuno_config::ResolvedLearningConfig;
use zuno_db::experience::{ExperienceRecord, ExperienceStore};
use zuno_db::learning_pattern::{
    LearningPatternRecord, LearningPatternStore, NewLearningPattern, PatternProposal, PatternScope,
};

#[derive(Clone)]
pub struct PatternMiner {
    experiences: ExperienceStore,
    patterns: LearningPatternStore,
    config: ResolvedLearningConfig,
}

impl PatternMiner {
    #[must_use]
    pub fn new(pool: Arc<zuno_db::Pool>, config: ResolvedLearningConfig) -> Self {
        Self {
            experiences: ExperienceStore::new(pool.clone()),
            patterns: LearningPatternStore::new(pool),
            config,
        }
    }

    /// Mine project patterns after the scheduler's minimum-new-record gate.
    pub fn mine_project(
        &self,
        project_id: &str,
        since: i64,
        now: i64,
    ) -> Result<Vec<PatternProposal>> {
        let records = self.experiences.list_active_since(project_id, since)?;
        if records.len() < self.config.aggregation_min_new_records as usize {
            return Ok(Vec::new());
        }
        let mut groups: BTreeMap<String, Vec<ExperienceRecord>> = BTreeMap::new();
        for record in records {
            if record.projection.kind.promotable() {
                groups
                    .entry(record.fingerprint.clone())
                    .or_default()
                    .push(record);
            }
        }
        let mut proposals = Vec::new();
        for (fingerprint, records) in groups {
            if records.len() < 2 {
                continue;
            }
            proposals.push(self.patterns.propose(build_project_pattern(
                project_id,
                fingerprint,
                &records,
                now,
            ))?);
        }
        Ok(proposals)
    }

    /// Explicit promotion bypasses evidence-count thresholds, but still creates
    /// only a pending pattern; a Skill candidate remains separately reviewable.
    pub fn propose_from_experience(
        &self,
        experience_id: &str,
        now: i64,
    ) -> Result<PatternProposal> {
        let record = self.experiences.get(experience_id)?;
        let project_id = record.projection.project_id.clone();
        self.patterns
            .propose(build_project_pattern(
                &project_id,
                record.fingerprint.clone(),
                &[record],
                now,
            ))
            .map_err(Into::into)
    }

    /// Mine global patterns only from already promoted project patterns.
    pub fn mine_global(&self, now: i64) -> Result<Vec<PatternProposal>> {
        let promoted = self.patterns.list_promoted_projects()?;
        let mut groups: BTreeMap<String, Vec<LearningPatternRecord>> = BTreeMap::new();
        for pattern in promoted {
            groups
                .entry(pattern.projection.fingerprint.clone())
                .or_default()
                .push(pattern);
        }
        let mut proposals = Vec::new();
        for (fingerprint, patterns) in groups {
            let project_ids: BTreeSet<&str> = patterns
                .iter()
                .filter_map(|pattern| pattern.projection.project_id.as_deref())
                .collect();
            if project_ids.len() < self.config.global_promotion_min_projects as usize {
                continue;
            }
            let mut evidence_ids = patterns
                .iter()
                .map(|pattern| pattern.projection.id.clone())
                .collect::<Vec<_>>();
            evidence_ids.sort();
            let mut learned_rules = unique_rules(
                patterns
                    .iter()
                    .flat_map(|pattern| pattern.projection.learned_rules.iter().cloned()),
            );
            learned_rules.truncate(self.config.skill_max_learned_rules as usize);
            let title = patterns.first().map_or_else(
                || "Cross-project learning".to_owned(),
                |pattern| pattern.projection.title.clone(),
            );
            let summary = format!(
                "Pattern supported by {} independent projects.",
                project_ids.len()
            );
            let evidence_digest = digest_text(&evidence_ids.join("\n"));
            proposals.push(
                self.patterns.propose(NewLearningPattern {
                    id: format!("pat_{}", Uuid::now_v7().simple()),
                    scope: PatternScope::Global,
                    project_id: None,
                    fingerprint,
                    title,
                    summary,
                    learned_rules,
                    evidence_ids,
                    evidence_digest,
                    evidence_version: now.max(1),
                    independent_sessions: patterns
                        .iter()
                        .map(|pattern| pattern.projection.independent_sessions)
                        .sum(),
                    project_count: u32::try_from(project_ids.len()).unwrap_or(u32::MAX),
                    time_created: now,
                })?,
            );
        }
        Ok(proposals)
    }

    /// Stable evidence identity for eligible cross-project aggregation.
    ///
    /// The scheduler includes this digest in its weekly idempotency key. A
    /// no-op check therefore stays deduplicated, while newly promoted project
    /// evidence can trigger another check in the same interval.
    pub fn global_evidence_digest(&self) -> Result<Option<String>> {
        let promoted = self.patterns.list_promoted_projects()?;
        let mut groups: BTreeMap<String, Vec<LearningPatternRecord>> = BTreeMap::new();
        for pattern in promoted {
            groups
                .entry(pattern.projection.fingerprint.clone())
                .or_default()
                .push(pattern);
        }
        let mut evidence = Vec::new();
        for (fingerprint, patterns) in groups {
            let project_ids = patterns
                .iter()
                .filter_map(|pattern| pattern.projection.project_id.as_deref())
                .collect::<BTreeSet<_>>();
            if project_ids.len() < self.config.global_promotion_min_projects as usize {
                continue;
            }
            evidence.push(fingerprint);
            evidence.extend(patterns.into_iter().map(|pattern| {
                format!(
                    "{}:{}:{}",
                    pattern.projection.id,
                    pattern.projection.evidence_version,
                    pattern.evidence_digest
                )
            }));
        }
        evidence.sort();
        Ok((!evidence.is_empty()).then(|| digest_text(&evidence.join("\n"))))
    }

    pub fn promote(&self, id: &str, now: i64) -> Result<LearningPatternRecord> {
        self.patterns.promote(id, now).map_err(Into::into)
    }

    pub fn reject(&self, id: &str, now: i64) -> Result<LearningPatternRecord> {
        self.patterns.reject(id, now).map_err(Into::into)
    }

    pub fn get(&self, id: &str) -> Result<LearningPatternRecord> {
        self.patterns.get(id).map_err(Into::into)
    }

    pub fn list_visible(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<LearningPatternRecord>> {
        self.patterns
            .list_visible(project_id, limit)
            .map_err(Into::into)
    }
}

fn build_project_pattern(
    project_id: &str,
    fingerprint: String,
    records: &[ExperienceRecord],
    now: i64,
) -> NewLearningPattern {
    let mut evidence_ids = records
        .iter()
        .map(|record| record.projection.id.clone())
        .collect::<Vec<_>>();
    evidence_ids.sort();
    let sessions: BTreeSet<&str> = records
        .iter()
        .filter_map(|record| record.projection.session_id.as_deref())
        .collect();
    let mut learned_rules = unique_rules(records.iter().map(|record| {
        record
            .projection
            .resolution
            .clone()
            .unwrap_or_else(|| record.projection.summary.clone())
    }));
    learned_rules.truncate(15);
    let first = &records[0].projection;
    NewLearningPattern {
        id: format!("pat_{}", Uuid::now_v7().simple()),
        scope: PatternScope::Project,
        project_id: Some(project_id.to_owned()),
        fingerprint,
        title: first.title.clone(),
        summary: format!(
            "Pattern supported by {} experiences across {} independent sessions.",
            records.len(),
            sessions.len()
        ),
        learned_rules,
        evidence_digest: digest_text(&evidence_ids.join("\n")),
        evidence_ids,
        evidence_version: now.max(1),
        independent_sessions: u32::try_from(sessions.len()).unwrap_or(u32::MAX),
        project_count: 1,
        time_created: now,
    }
}

fn unique_rules(rules: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    rules
        .into_iter()
        .map(|rule| rule.trim().to_owned())
        .filter(|rule| !rule.is_empty() && seen.insert(rule.clone()))
        .collect()
}
