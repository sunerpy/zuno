//! Durable candidate workflow for resident memory.

use crate::{MemoryError, MemoryStore, Operation, Scope, ScopeLimits};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;
use zuno_db::Pool;
use zuno_db::memory_candidate::{MemoryCandidateRecord, MemoryCandidateStore, NewMemoryCandidate};
use zuno_types::{
    MemoryAction, MemoryCandidateProjection, MemoryCandidateStatus, MemoryEntryProjection,
    MemoryScope, MemorySource,
};

/// Where each resident-memory scope is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopePaths {
    global: PathBuf,
    project: PathBuf,
}

impl ScopePaths {
    #[must_use]
    pub fn discover(worktree: &Path) -> Self {
        Self {
            global: Scope::Global.path(worktree),
            project: Scope::Project.path(worktree),
        }
    }

    #[must_use]
    pub fn at(global: impl Into<PathBuf>, project: impl Into<PathBuf>) -> Self {
        Self {
            global: global.into(),
            project: project.into(),
        }
    }

    #[must_use]
    pub fn for_scope(&self, scope: Scope) -> &Path {
        match scope {
            Scope::Global => &self.global,
            Scope::Project => &self.project,
        }
    }

    fn wire_path(&self, scope: Scope) -> String {
        self.for_scope(scope).to_string_lossy().into_owned()
    }
}

/// Candidate promotion policy after validation and durable insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionPolicy {
    Review,
    HighConfidence { threshold: u16 },
    Automatic,
}

impl PromotionPolicy {
    fn applies(self, confidence: u16) -> bool {
        match self {
            Self::Review => false,
            Self::HighConfidence { threshold } => confidence >= threshold,
            Self::Automatic => true,
        }
    }
}

/// One validated proposal entering the durable candidate queue.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryProposal {
    pub scope: MemoryScope,
    pub action: MemoryAction,
    pub content: Option<String>,
    pub old_text: Option<String>,
    pub reason: String,
    pub confidence: f64,
    pub source: MemorySource,
    pub source_session_id: Option<String>,
    pub source_message_id: Option<String>,
}

/// Notification emitted after durable memory state changes.
pub trait MemoryObserver: Send + Sync {
    fn changed(&self);
}

/// Candidate or resident-store failure.
#[derive(Debug, thiserror::Error)]
pub enum MemoryServiceError {
    #[error(transparent)]
    Database(#[from] zuno_error::DbError),
    #[error(transparent)]
    Resident(#[from] MemoryError),
    #[error("invalid memory candidate: {0}")]
    Invalid(String),
}

impl MemoryServiceError {
    /// Whether the model can fix the proposal without operator or storage repair.
    #[must_use]
    pub const fn is_model_correctable(&self) -> bool {
        match self {
            Self::Invalid(_) => true,
            Self::Resident(error) => error.is_proposal_correctable(),
            Self::Database(_) => false,
        }
    }
}

/// The single owner of candidate validation, promotion, apply, and undo.
#[derive(Clone)]
pub struct MemoryService {
    store: MemoryCandidateStore,
    paths: ScopePaths,
    limits: ScopeLimits,
    promotion: PromotionPolicy,
    observer: Option<Arc<dyn MemoryObserver>>,
}

impl MemoryService {
    #[must_use]
    pub fn new(
        pool: Arc<Pool>,
        paths: ScopePaths,
        limits: ScopeLimits,
        promotion: PromotionPolicy,
    ) -> Self {
        Self {
            store: MemoryCandidateStore::new(pool),
            paths,
            limits,
            promotion,
            observer: None,
        }
    }

    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn MemoryObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    #[must_use]
    pub fn paths(&self) -> &ScopePaths {
        &self.paths
    }

    pub fn propose(
        &self,
        proposal: MemoryProposal,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.propose_with_policy(proposal, true)
    }

    /// Insert a validated candidate while deliberately bypassing automatic
    /// promotion. Cleanup and revocation flows use this because removing learned
    /// state must remain an explicit human review even when ordinary additions
    /// may auto-promote.
    pub fn propose_for_review(
        &self,
        proposal: MemoryProposal,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.propose_with_policy(proposal, false)
    }

    fn propose_with_policy(
        &self,
        proposal: MemoryProposal,
        allow_automatic_promotion: bool,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let confidence = confidence_basis_points(proposal.confidence)?;
        let reason = proposal.reason.trim();
        if reason.is_empty() {
            return Err(MemoryServiceError::Invalid(
                "reason must not be empty".to_owned(),
            ));
        }
        let operation = operation(
            proposal.action,
            proposal.content.as_deref(),
            proposal.old_text.as_deref(),
        )?;
        let fingerprint = proposal_fingerprint(&proposal)?;
        let scope = Scope::from(proposal.scope);
        let mut resident = self.open(scope)?;
        resident.preview_batch(std::slice::from_ref(&operation))?;

        let now = zuno_db::message::now_millis();
        let insert = self.store.create_or_get(NewMemoryCandidate {
            id: format!("mem_{}", Uuid::new_v4().simple()),
            target: proposal.scope,
            target_path: self.paths.wire_path(scope),
            action: proposal.action,
            content: proposal.content,
            old_text: proposal.old_text,
            reason: reason.to_owned(),
            confidence,
            source: proposal.source,
            source_session_id: proposal.source_session_id,
            source_message_id: proposal.source_message_id,
            fingerprint,
            time_created: now,
        })?;
        let candidate = insert.record;
        if !insert.inserted {
            return Ok(candidate);
        }
        self.notify();
        if allow_automatic_promotion && self.promotion.applies(confidence) {
            return self.apply(candidate.id());
        }
        Ok(candidate)
    }

    pub fn candidate(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.store.get(id).map_err(Into::into)
    }

    pub fn apply(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let candidate = self.store.get(id)?;
        if !matches!(
            candidate.projection.status,
            MemoryCandidateStatus::Pending | MemoryCandidateStatus::Failed
        ) {
            return Err(MemoryServiceError::Invalid(format!(
                "candidate {id} is {}, not pending",
                candidate.projection.status.as_str()
            )));
        }
        self.ensure_owned_path(&candidate)?;
        let operation = operation(
            candidate.projection.action,
            candidate.projection.content.as_deref(),
            candidate.projection.old_text.as_deref(),
        )?;
        let scope = Scope::from(candidate.projection.scope);
        let mut resident = self.open(scope)?;
        let after = match resident.preview_batch(std::slice::from_ref(&operation)) {
            Ok(after) => after,
            Err(error) => {
                let _failed = self.store.set_status(
                    id,
                    MemoryCandidateStatus::Failed,
                    Some(&error.to_string()),
                    zuno_db::message::now_millis(),
                );
                self.notify();
                return Err(error.into());
            }
        };
        let before = resident.entries().to_vec();
        self.store
            .begin_apply(id, &before, &after, zuno_db::message::now_millis())?;
        let applied = resident.replace_exact(&before, &after);
        let record = match applied {
            Ok(_) => self.store.set_status(
                id,
                MemoryCandidateStatus::Applied,
                None,
                zuno_db::message::now_millis(),
            )?,
            Err(error) => {
                let status = self.reconcile_status(scope, &before, &after, &error)?;
                let record = self.store.set_status(
                    id,
                    status,
                    Some(&error.to_string()),
                    zuno_db::message::now_millis(),
                )?;
                self.notify();
                if status != MemoryCandidateStatus::Applied {
                    return Err(error.into());
                }
                record
            }
        };
        self.notify();
        Ok(record)
    }

    pub fn reject(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let candidate = self.store.get(id)?;
        if !matches!(
            candidate.projection.status,
            MemoryCandidateStatus::Pending | MemoryCandidateStatus::Failed
        ) {
            return Err(MemoryServiceError::Invalid(format!(
                "candidate {id} cannot be rejected from {}",
                candidate.projection.status.as_str()
            )));
        }
        let record = self.store.set_status(
            id,
            MemoryCandidateStatus::Rejected,
            None,
            zuno_db::message::now_millis(),
        )?;
        self.notify();
        Ok(record)
    }

    pub fn edit(
        &self,
        id: &str,
        content: Option<String>,
        old_text: Option<String>,
        reason: String,
        confidence: f64,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let candidate = self.store.get(id)?;
        let confidence = confidence_basis_points(confidence)?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(MemoryServiceError::Invalid(
                "reason must not be empty".to_owned(),
            ));
        }
        let operation = operation(
            candidate.projection.action,
            content.as_deref(),
            old_text.as_deref(),
        )?;
        let mut resident = self.open(Scope::from(candidate.projection.scope))?;
        resident.preview_batch(std::slice::from_ref(&operation))?;
        let record = self.store.edit_pending(
            id,
            content.as_deref(),
            old_text.as_deref(),
            reason,
            confidence,
            zuno_db::message::now_millis(),
        )?;
        self.notify();
        Ok(record)
    }

    pub fn undo(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let candidate = self.store.get(id)?;
        if candidate.projection.status != MemoryCandidateStatus::Applied {
            return Err(MemoryServiceError::Invalid(format!(
                "candidate {id} is not applied"
            )));
        }
        self.ensure_owned_path(&candidate)?;
        let before = candidate.before_entries.as_deref().ok_or_else(|| {
            MemoryServiceError::Invalid(format!("candidate {id} has no before snapshot"))
        })?;
        let after = candidate.after_entries.as_deref().ok_or_else(|| {
            MemoryServiceError::Invalid(format!("candidate {id} has no after snapshot"))
        })?;
        let mut resident = self.open(Scope::from(candidate.projection.scope))?;
        self.store.begin_undo(id, zuno_db::message::now_millis())?;
        let record = match resident.replace_exact(after, before) {
            Ok(_) => self.store.set_status(
                id,
                MemoryCandidateStatus::Undone,
                None,
                zuno_db::message::now_millis(),
            )?,
            Err(error) => {
                let status = self.reconcile_undo_status(
                    Scope::from(candidate.projection.scope),
                    before,
                    after,
                    &error,
                )?;
                let record = self.store.set_status(
                    id,
                    status,
                    Some(&error.to_string()),
                    zuno_db::message::now_millis(),
                )?;
                self.notify();
                if status != MemoryCandidateStatus::Undone {
                    return Err(error.into());
                }
                record
            }
        };
        self.notify();
        Ok(record)
    }

    /// Remove one current entry through the same audited candidate path.
    pub fn remove_entry(
        &self,
        scope: MemoryScope,
        locator: String,
        reason: String,
        source_session_id: Option<String>,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let candidate = self.propose(MemoryProposal {
            scope,
            action: MemoryAction::Remove,
            content: None,
            old_text: Some(locator),
            reason,
            confidence: 1.0,
            source: MemorySource::User,
            source_session_id,
            source_message_id: None,
        })?;
        if candidate.projection.status == MemoryCandidateStatus::Pending {
            return self.apply(candidate.id());
        }
        Ok(candidate)
    }

    pub fn candidates(&self) -> Result<Vec<MemoryCandidateProjection>, MemoryServiceError> {
        Ok(self
            .records()?
            .into_iter()
            .map(|candidate| candidate.projection)
            .collect())
    }

    pub fn entries(&self) -> Result<Vec<MemoryEntryProjection>, MemoryServiceError> {
        let mut entries = Vec::new();
        for scope in Scope::ALL {
            let resident = self.open(scope)?;
            entries.extend(resident.entries().iter().cloned().map(|content| {
                MemoryEntryProjection {
                    scope: scope.into(),
                    content,
                }
            }));
        }
        Ok(entries)
    }

    /// Reconcile process loss around apply or undo without replaying a write.
    pub fn reconcile(&self) -> Result<(), MemoryServiceError> {
        let global = self.paths.wire_path(Scope::Global);
        let project = self.paths.wire_path(Scope::Project);
        for candidate in self.store.list_inflight_for_paths(&global, &project)? {
            let Some(before) = candidate.before_entries.as_deref() else {
                self.store.set_status(
                    candidate.id(),
                    MemoryCandidateStatus::Uncertain,
                    Some("applying candidate has no before snapshot"),
                    zuno_db::message::now_millis(),
                )?;
                continue;
            };
            let Some(after) = candidate.after_entries.as_deref() else {
                self.store.set_status(
                    candidate.id(),
                    MemoryCandidateStatus::Uncertain,
                    Some("applying candidate has no after snapshot"),
                    zuno_db::message::now_millis(),
                )?;
                continue;
            };
            let resident = self.open(Scope::from(candidate.projection.scope))?;
            let status = match candidate.projection.status {
                MemoryCandidateStatus::Applying if resident.entries() == after => {
                    MemoryCandidateStatus::Applied
                }
                MemoryCandidateStatus::Applying if resident.entries() == before => {
                    MemoryCandidateStatus::Failed
                }
                MemoryCandidateStatus::Undoing if resident.entries() == before => {
                    MemoryCandidateStatus::Undone
                }
                MemoryCandidateStatus::Undoing if resident.entries() == after => {
                    MemoryCandidateStatus::Applied
                }
                MemoryCandidateStatus::Applying | MemoryCandidateStatus::Undoing => {
                    MemoryCandidateStatus::Uncertain
                }
                _ => unreachable!("only in-flight memory candidates were queried"),
            };
            self.store.set_status(
                candidate.id(),
                status,
                Some("reconciled after process restart without replay"),
                zuno_db::message::now_millis(),
            )?;
        }
        self.notify();
        Ok(())
    }

    fn records(&self) -> Result<Vec<MemoryCandidateRecord>, MemoryServiceError> {
        Ok(self.store.list_for_paths(
            &self.paths.wire_path(Scope::Global),
            &self.paths.wire_path(Scope::Project),
        )?)
    }

    fn open(&self, scope: Scope) -> Result<MemoryStore, MemoryError> {
        MemoryStore::open_with_limit(
            scope,
            self.paths.for_scope(scope).to_path_buf(),
            self.limits.for_scope(scope),
        )
    }

    fn ensure_owned_path(
        &self,
        candidate: &MemoryCandidateRecord,
    ) -> Result<(), MemoryServiceError> {
        let expected = self
            .paths
            .wire_path(Scope::from(candidate.projection.scope));
        if candidate.target_path != expected {
            return Err(MemoryServiceError::Invalid(format!(
                "candidate {} belongs to {}, not {}",
                candidate.id(),
                candidate.target_path,
                expected
            )));
        }
        Ok(())
    }

    fn reconcile_status(
        &self,
        scope: Scope,
        before: &[String],
        after: &[String],
        error: &MemoryError,
    ) -> Result<MemoryCandidateStatus, MemoryServiceError> {
        let resident = self.open(scope)?;
        if resident.entries() == after {
            return Ok(MemoryCandidateStatus::Applied);
        }
        if resident.entries() == before && !error.may_have_written() {
            return Ok(MemoryCandidateStatus::Failed);
        }
        Ok(MemoryCandidateStatus::Uncertain)
    }

    fn reconcile_undo_status(
        &self,
        scope: Scope,
        before: &[String],
        after: &[String],
        error: &MemoryError,
    ) -> Result<MemoryCandidateStatus, MemoryServiceError> {
        let resident = self.open(scope)?;
        if resident.entries() == before {
            return Ok(MemoryCandidateStatus::Undone);
        }
        if resident.entries() == after && !error.may_have_written() {
            return Ok(MemoryCandidateStatus::Applied);
        }
        Ok(MemoryCandidateStatus::Uncertain)
    }

    fn notify(&self) {
        if let Some(observer) = &self.observer {
            observer.changed();
        }
    }
}

fn operation(
    action: MemoryAction,
    content: Option<&str>,
    old_text: Option<&str>,
) -> Result<Operation, MemoryError> {
    Operation::parse(1, action.as_str(), content, old_text)
}

fn confidence_basis_points(confidence: f64) -> Result<u16, MemoryServiceError> {
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(MemoryServiceError::Invalid(
            "confidence must be between 0 and 1".to_owned(),
        ));
    }
    Ok((confidence * 10_000.0).round() as u16)
}

fn proposal_fingerprint(proposal: &MemoryProposal) -> Result<Option<String>, MemoryServiceError> {
    if proposal.source != MemorySource::Reflection {
        return Ok(None);
    }
    if proposal
        .source_session_id
        .as_deref()
        .is_none_or(str::is_empty)
        || proposal
            .source_message_id
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return Err(MemoryServiceError::Invalid(
            "reflection candidates require source session and message ids".to_owned(),
        ));
    }
    let normalized = [
        proposal.scope.as_str(),
        proposal.action.as_str(),
        normalize_fingerprint_text(proposal.content.as_deref()),
        normalize_fingerprint_text(proposal.old_text.as_deref()),
    ]
    .join("\u{0}");
    Ok(Some(hex::encode(Sha256::digest(normalized.as_bytes()))))
}

fn normalize_fingerprint_text(value: Option<&str>) -> &str {
    value.map_or("", str::trim)
}
