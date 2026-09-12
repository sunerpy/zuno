//! Bounded semantic plans become atomic data changes, never shell commands.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use zuno_db::learning_job::{LearningJobRecord, LearningLease};
use zuno_db::memory_evidence::MemoryEvidenceReference;
use zuno_db::memory_maintenance::{MemoryBatchChange, MemoryBatchCommit, MemoryMaintenanceState};

#[derive(Debug, Clone)]
pub struct MemoryMaintenanceUpdate {
    pub scope: MemoryScope,
    pub action: MemoryAction,
    pub content: Option<String>,
    pub old_text: Option<String>,
    pub reason: String,
    pub confidence: f64,
    pub evidence: Vec<MemoryEvidenceReference>,
}

pub struct MemoryMaintenanceContext<'a> {
    pub job: &'a LearningJobRecord,
    pub lease: &'a LearningLease,
    pub views: &'a [ResidentMemoryView],
    pub evidence: &'a [MemoryEvidenceReference],
    pub input_digest: &'a str,
    pub now: i64,
}

impl MemoryService {
    pub fn forget_sources(
        &self,
        ids: &[String],
        source_session: Option<&str>,
        now: i64,
    ) -> Result<zuno_db::memory_maintenance::MemorySourceRetraction, MemoryServiceError> {
        for scope in Scope::ALL {
            self.authority
                .authorize(scope.into(), super::MemoryAccess::Forget)?;
        }
        let paths = self
            .read_views()?
            .into_iter()
            .map(|view| view.document.path)
            .collect::<Vec<_>>();
        let result = self
            .persistence
            .forget_sources(ids, &paths, source_session, now)?;
        for document in &result.documents {
            self.project_document(document)?;
        }
        self.notify();
        Ok(result)
    }

    pub fn maintenance_state(
        &self,
        project_id: &str,
    ) -> Result<Option<MemoryMaintenanceState>, MemoryServiceError> {
        let project = self.document(Scope::Project)?;
        self.persistence
            .maintenance_state(project_id, &project.path)
    }

    /// Existing direct user changes are higher-priority correction/forget signals
    /// for consolidation. They are data in its prompt, never execution policy.
    pub fn maintenance_signals(
        &self,
    ) -> Result<Vec<MemoryCandidateProjection>, MemoryServiceError> {
        let mut records = self.records()?;
        records.sort_by_key(|record| {
            std::cmp::Reverse((record.projection.time_updated, record.projection.id.clone()))
        });
        Ok(records
            .into_iter()
            .filter(|record| {
                record.projection.status == MemoryCandidateStatus::Undone
                    || (record.projection.status == MemoryCandidateStatus::Applied
                        && record.projection.source != MemorySource::Reflection)
            })
            .take(64)
            .map(|record| record.projection)
            .collect())
    }

    pub fn commit_maintenance(
        &self,
        context: MemoryMaintenanceContext<'_>,
        mut updates: Vec<MemoryMaintenanceUpdate>,
    ) -> Result<Vec<MemoryCandidateRecord>, MemoryServiceError> {
        for scope in Scope::ALL {
            self.authority
                .authorize(scope.into(), super::MemoryAccess::Maintain)?;
        }
        if updates.len() > 32 {
            return Err(MemoryServiceError::Invalid(
                "memory maintenance exceeds 32 changes".to_owned(),
            ));
        }
        let project_id = context.job.project_id.as_deref().ok_or_else(|| {
            MemoryServiceError::Invalid("memory maintenance has no project".to_owned())
        })?;
        let mut views = BTreeMap::new();
        for scope in Scope::ALL {
            let view = context
                .views
                .iter()
                .find(|view| view.document.scope == scope.into())
                .ok_or_else(|| {
                    MemoryServiceError::Invalid("memory scope is not ready".to_owned())
                })?;
            if self.document(scope)?.path != view.document.path {
                return Err(MemoryServiceError::Invalid(
                    "memory scope identity changed".to_owned(),
                ));
            }
            views.insert(view.document.scope.as_str().to_owned(), view);
        }
        let global = views["global"];
        let project = views["project"];
        let mut touched = BTreeSet::new();
        for update in &mut updates {
            update.content = update.content.take().map(|text| text.trim().to_owned());
            update.old_text = update.old_text.take().map(|text| text.trim().to_owned());
            if update.reason.trim().is_empty() || update.reason.len() > 1_024 {
                return Err(MemoryServiceError::Invalid(
                    "memory change needs a bounded reason".to_owned(),
                ));
            }
            confidence_basis_points(update.confidence)?;
            operation(
                update.action,
                update.content.as_deref(),
                update.old_text.as_deref(),
            )?;
            if matches!(update.action, MemoryAction::Replace | MemoryAction::Remove) {
                let old = update.old_text.as_deref().unwrap_or_default();
                let view = views[update.scope.as_str()];
                if !view.document.entries.iter().any(|entry| entry == old)
                    || !touched.insert((update.scope.as_str(), old.to_owned()))
                    || !view.managed.contains_key(old)
                {
                    return Err(MemoryServiceError::Invalid(
                        "maintenance must name one exact managed entry only once; user-owned entries are protected".to_owned(),
                    ));
                }
            }
            if update
                .evidence
                .iter()
                .any(|reference| !context.evidence.contains(reference))
            {
                return Err(MemoryServiceError::Invalid(
                    "maintenance invented an evidence reference".to_owned(),
                ));
            }
            if let Some(content) = update.content.as_deref()
                && self
                    .persistence
                    .content_retired(&views[update.scope.as_str()].document.path, content)?
            {
                return Err(MemoryServiceError::Invalid(
                    "maintenance cannot resurrect explicitly forgotten or undone content"
                        .to_owned(),
                ));
            }
            if update.evidence.is_empty()
                && !(update.action == MemoryAction::Remove
                    && update
                        .old_text
                        .as_ref()
                        .is_some_and(|text| views[update.scope.as_str()].suppressed.contains(text)))
            {
                return Err(MemoryServiceError::Invalid(
                    "memory change needs verified evidence".to_owned(),
                ));
            }
        }
        // Shrink before growing so each intermediate journal snapshot can be
        // undone safely even when the complete store is close to its cap.
        updates.sort_by_key(|update| {
            let delta = update
                .content
                .as_ref()
                .map_or(0, |text| text.chars().count()) as i64
                - update
                    .old_text
                    .as_ref()
                    .map_or(0, |text| text.chars().count()) as i64;
            let phase = match update.action {
                MemoryAction::Remove => 0,
                MemoryAction::Replace if delta <= 0 => 1,
                MemoryAction::Replace => 2,
                MemoryAction::Add => 3,
            };
            (update.scope.as_str(), phase, delta)
        });
        let automatic = updates.iter().all(|update| {
            update.evidence.is_empty()
                || self
                    .promotion
                    .applies((update.confidence * 10_000.0).round() as u16)
        });
        let mut entries = BTreeMap::from([
            ("global", global.document.entries.clone()),
            ("project", project.document.entries.clone()),
        ]);
        let mut revisions = BTreeMap::from([
            ("global", global.document.revision),
            ("project", project.document.revision),
        ]);
        let mut changes = Vec::new();
        for (ordinal, update) in updates.into_iter().enumerate() {
            let key = update.scope.as_str();
            let before = entries[key].clone();
            let op = operation(
                update.action,
                update.content.as_deref(),
                update.old_text.as_deref(),
            )?;
            let after = crate::store::preview_entries(
                Scope::from(update.scope),
                self.limits.for_scope(Scope::from(update.scope)),
                &before,
                &[op],
            )?;
            if before == after {
                let existing = update
                    .content
                    .as_ref()
                    .and_then(|content| views[key].managed.get(content));
                if existing.is_none()
                    || existing.is_some_and(|existing| {
                        update.evidence.iter().all(|item| existing.contains(item))
                    })
                {
                    continue;
                }
            }
            let apply = automatic || update.evidence.is_empty();
            let base_revision = if apply {
                revisions[key]
            } else {
                views[key].document.revision
            };
            let id = format!("mem_{}_{}", context.job.id, ordinal);
            let candidate = NewMemoryCandidate {
                id,
                target: update.scope,
                target_path: views[key].document.path.clone(),
                action: update.action,
                content: update.content,
                old_text: update.old_text,
                reason: update.reason,
                confidence: confidence_basis_points(update.confidence)?,
                source: MemorySource::Reflection,
                source_session_id: context.job.session_id.clone(),
                source_message_id: context.job.source_message_id.clone(),
                fingerprint: None,
                base_revision: Some(base_revision),
                evidence: Some(update.evidence),
                time_created: context.now,
            };
            if apply && before != after {
                *revisions.get_mut(key).expect("both scopes are initialized") += 1;
            }
            entries.insert(key, after.clone());
            changes.push(MemoryBatchChange {
                candidate,
                before,
                after,
                apply,
            });
        }
        let committed = self.persistence.commit_maintenance(MemoryBatchCommit {
            project_id,
            global_path: &global.document.path,
            project_path: &project.document.path,
            global_revision: global.document.revision,
            project_revision: project.document.revision,
            input_digest: context.input_digest,
            job_id: &context.job.id,
            lease: context.lease,
            evidence: context.evidence,
            changes,
            now: context.now,
        })?;
        for document in &committed.documents {
            self.project_document(document)?;
        }
        self.notify();
        Ok(committed.candidates)
    }
}
