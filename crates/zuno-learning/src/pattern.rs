use crate::{Result, digest_text};
use std::collections::BTreeSet;
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
    consolidator: Option<Arc<dyn crate::PatternConsolidator>>,
}

impl PatternMiner {
    #[must_use]
    pub fn new(pool: Arc<zuno_db::Pool>, config: ResolvedLearningConfig) -> Self {
        Self {
            experiences: ExperienceStore::new(pool.clone()),
            patterns: LearningPatternStore::new(pool),
            config,
            consolidator: None,
        }
    }

    #[must_use]
    pub fn with_consolidator(mut self, consolidator: Arc<dyn crate::PatternConsolidator>) -> Self {
        self.consolidator = Some(consolidator);
        self
    }

    /// Mine project patterns after the scheduler's minimum-new-record gate.
    pub async fn mine_project(
        &self,
        project_id: &str,
        since: i64,
        now: i64,
    ) -> Result<Vec<PatternProposal>> {
        self.mine_project_authorized(project_id, since, now, None)
            .await
    }

    pub async fn mine_project_claimed(
        &self,
        project_id: &str,
        since: i64,
        now: i64,
        job_id: &str,
        lease: &zuno_db::learning_job::LearningLease,
    ) -> Result<Vec<PatternProposal>> {
        self.mine_project_authorized(project_id, since, now, Some((job_id, lease)))
            .await
    }

    async fn mine_project_authorized(
        &self,
        project_id: &str,
        since: i64,
        now: i64,
        authority: Option<(&str, &zuno_db::learning_job::LearningLease)>,
    ) -> Result<Vec<PatternProposal>> {
        let records = self.experiences.list_for_project(project_id, 256)?;
        if records
            .iter()
            .filter(|record| record.projection.time_created > since)
            .count()
            < self.config.aggregation_min_new_records as usize
        {
            return Ok(Vec::new());
        }
        let records: Vec<_> = records
            .into_iter()
            .filter(|record| record.projection.kind.promotable() && record.verified_sources())
            .collect();
        if records.len() < 2 {
            return Ok(Vec::new());
        }
        let Some(consolidator) = &self.consolidator else {
            return Err(crate::model::invalid(
                "semantic consolidation has no model binding",
            ));
        };
        let existing: Vec<_> = self
            .patterns
            .list_visible(project_id, 100)?
            .into_iter()
            .filter(|record| record.scope == PatternScope::Project)
            .collect();
        let session_id = records
            .iter()
            .find_map(|record| record.projection.session_id.clone())
            .ok_or_else(|| crate::model::invalid("consolidation has no durable source session"))?;
        let request = crate::ConsolidationRequest {
            scope: crate::ConsolidationScope::Project,
            project_id: project_id.to_owned(),
            session_id,
            experiences: records
                .iter()
                .map(|record| record.projection.clone())
                .collect(),
            patterns: existing
                .iter()
                .map(|record| record.projection.clone())
                .collect(),
        };
        let output = consolidator.consolidate(request).await?;
        self.persist_consolidation(project_id, &records, &existing, output, now, authority)
    }

    fn persist_consolidation(
        &self,
        project_id: &str,
        records: &[ExperienceRecord],
        existing: &[LearningPatternRecord],
        output: crate::Consolidation,
        now: i64,
        authority: Option<(&str, &zuno_db::learning_job::LearningLease)>,
    ) -> Result<Vec<PatternProposal>> {
        if output.groups.len() > 32 {
            return Err(crate::model::invalid(
                "consolidation returned too many groups",
            ));
        }
        let mut proposals = Vec::new();
        let mut prepared = Vec::new();
        for group in output.groups {
            let ids: BTreeSet<_> = group.evidence_ids.iter().collect();
            if ids.len() < 2
                || ids.len() != group.evidence_ids.len()
                || group.title.trim().is_empty()
                || group.title.len() > 512
                || group.learned_rules.is_empty()
                || group.learned_rules.len() > self.config.skill_max_learned_rules as usize
                || group
                    .learned_rules
                    .iter()
                    .any(|rule| rule.trim().is_empty() || rule.len() > 2048)
            {
                return Err(crate::model::invalid(
                    "invalid semantic group bounds or evidence",
                ));
            }
            let cited: Vec<_> = records
                .iter()
                .filter(|record| ids.contains(&record.projection.id))
                .cloned()
                .collect();
            if cited.len() != ids.len() {
                return Err(crate::model::invalid(
                    "consolidation cited an unknown experience",
                ));
            }
            let fingerprint = match &group.existing_pattern_id {
                Some(id) => existing
                    .iter()
                    .find(|record| &record.projection.id == id)
                    .ok_or_else(|| crate::model::invalid("consolidation named an unknown pattern"))?
                    .projection
                    .fingerprint
                    .clone(),
                None => digest_text(&unique_rules(group.learned_rules.clone()).join("\n")),
            };
            let mut pattern = build_project_pattern(project_id, fingerprint, &cited, now);
            pattern.title = group.title;
            pattern.learned_rules = unique_rules(group.learned_rules);
            prepared.push(pattern);
        }
        for pattern in prepared {
            proposals.push(match authority {
                Some((job_id, lease)) => self.patterns.propose_with_lease(
                    pattern,
                    job_id,
                    lease,
                    zuno_db::message::now_millis(),
                )?,
                None => self.patterns.propose(pattern)?,
            });
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
    pub async fn mine_global(&self, now: i64) -> Result<Vec<PatternProposal>> {
        self.mine_global_authorized(now, None).await
    }

    pub async fn mine_global_claimed(
        &self,
        now: i64,
        job_id: &str,
        lease: &zuno_db::learning_job::LearningLease,
    ) -> Result<Vec<PatternProposal>> {
        self.mine_global_authorized(now, Some((job_id, lease)))
            .await
    }

    async fn mine_global_authorized(
        &self,
        now: i64,
        authority: Option<(&str, &zuno_db::learning_job::LearningLease)>,
    ) -> Result<Vec<PatternProposal>> {
        let promoted = self.patterns.list_promoted_projects()?;
        let projects = promoted
            .iter()
            .filter_map(|pattern| pattern.projection.project_id.as_ref())
            .collect::<BTreeSet<_>>();
        if projects.len() < self.config.global_promotion_min_projects as usize {
            return Ok(Vec::new());
        }
        let Some(consolidator) = &self.consolidator else {
            return Err(crate::model::invalid(
                "global consolidation has no model binding",
            ));
        };
        let existing = self.patterns.list_visible("", 100)?;
        let mut source_session = None;
        for pattern in &promoted {
            for id in &pattern.evidence_ids {
                let record = self.experiences.get(id)?;
                if record.verified_sources()
                    && record.projection.status != zuno_types::ExperienceStatus::Forgotten
                {
                    source_session = record.projection.session_id;
                }
                if source_session.is_some() {
                    break;
                }
            }
            if source_session.is_some() {
                break;
            }
        }
        let Some(session_id) = source_session else {
            return Ok(Vec::new());
        };
        let output = consolidator
            .consolidate(crate::ConsolidationRequest {
                scope: crate::ConsolidationScope::Global,
                project_id: String::new(),
                session_id,
                experiences: Vec::new(),
                patterns: promoted
                    .iter()
                    .chain(existing.iter())
                    .map(|pattern| pattern.projection.clone())
                    .collect(),
            })
            .await?;
        if output.groups.len() > 32 {
            return Err(crate::model::invalid("too many global patterns"));
        }
        let mut prepared = Vec::new();
        for group in output.groups {
            let ids = group.evidence_ids.iter().collect::<BTreeSet<_>>();
            let sources = promoted
                .iter()
                .filter(|pattern| ids.contains(&pattern.projection.id))
                .collect::<Vec<_>>();
            let projects = sources
                .iter()
                .filter_map(|pattern| pattern.projection.project_id.as_ref())
                .collect::<BTreeSet<_>>();
            if ids.len() != group.evidence_ids.len()
                || sources.len() != ids.len()
                || projects.len() < self.config.global_promotion_min_projects as usize
                || group.learned_rules.is_empty()
                || group.learned_rules.len() > self.config.skill_max_learned_rules as usize
                || group.title.trim().is_empty()
                || group.title.len() > 512
                || group
                    .learned_rules
                    .iter()
                    .any(|rule| rule.trim().is_empty() || rule.len() > 2048)
            {
                return Err(crate::model::invalid(
                    "global pattern lacks bounded independent project evidence",
                ));
            }
            let rules = unique_rules(group.learned_rules);
            let fingerprint = match group.existing_pattern_id {
                Some(id) => existing
                    .iter()
                    .find(|pattern| pattern.projection.id == id)
                    .ok_or_else(|| crate::model::invalid("unknown existing global pattern"))?
                    .projection
                    .fingerprint
                    .clone(),
                None => digest_text(&rules.join("\n")),
            };
            let mut evidence_ids = group.evidence_ids;
            evidence_ids.sort();
            let evidence_digest = digest_text(
                &sources
                    .iter()
                    .map(|pattern| format!("{}:{}", pattern.projection.id, pattern.evidence_digest))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            prepared.push(NewLearningPattern {
                id: format!("pat_{}", Uuid::now_v7().simple()),
                scope: PatternScope::Global,
                project_id: None,
                fingerprint,
                title: group.title,
                summary: format!("Supported by {} independent projects.", projects.len()),
                learned_rules: rules,
                evidence_ids,
                evidence_digest,
                evidence_version: now.max(1),
                independent_sessions: sources.iter().fold(0u32, |sum, pattern| {
                    sum.saturating_add(pattern.projection.independent_sessions)
                }),
                project_count: u32::try_from(projects.len()).unwrap_or(u32::MAX),
                time_created: now,
            });
        }
        prepared
            .into_iter()
            .map(|pattern| {
                match authority {
                    Some((id, lease)) => self.patterns.propose_with_lease(
                        pattern,
                        id,
                        lease,
                        zuno_db::message::now_millis(),
                    ),
                    None => self.patterns.propose(pattern),
                }
                .map_err(Into::into)
            })
            .collect()
    }

    /// Input identity is independent of the semantic grouping chosen by the model.
    pub fn global_evidence_digest(&self) -> Result<Option<String>> {
        let promoted = self.patterns.list_promoted_projects()?;
        let projects = promoted
            .iter()
            .filter_map(|pattern| pattern.projection.project_id.as_ref())
            .collect::<BTreeSet<_>>();
        if projects.len() < self.config.global_promotion_min_projects as usize {
            return Ok(None);
        }
        let mut evidence = promoted
            .iter()
            .map(|pattern| {
                format!(
                    "{}:{}:{}",
                    pattern.projection.id,
                    pattern.projection.evidence_version,
                    pattern.evidence_digest
                )
            })
            .collect::<Vec<_>>();
        evidence.sort();
        Ok(Some(digest_text(&evidence.join("\n"))))
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
