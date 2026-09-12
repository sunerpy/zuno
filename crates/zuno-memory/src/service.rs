//! Durable candidate workflow for resident memory.

#[path = "maintenance.rs"]
mod maintenance;
pub use maintenance::{MemoryMaintenanceContext, MemoryMaintenanceUpdate};

use crate::authority::{LocalMemoryAuthority, MemoryAccess, MemoryAuthority};
use crate::persistence::{MemoryPersistence, SqliteMemoryPersistence};
use crate::{MemoryError, MemoryStore, Operation, Scope, ScopeLimits};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;
use zuno_db::Pool;
use zuno_db::memory_candidate::{MemoryCandidateRecord, NewMemoryCandidate};
use zuno_db::resident_memory::{
    ResidentMemoryAuthority, ResidentMemoryCommit, ResidentMemoryDocument, ResidentMemoryOperation,
    ResidentMemoryView,
};
use zuno_error::DbError;
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

/// A storage identity, never a file path supplied by a model or client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDocumentKey(String);
impl MemoryDocumentKey {
    pub fn new(value: impl Into<String>) -> Result<Self, MemoryServiceError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 512
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
        {
            return Err(MemoryServiceError::Invalid(
                "invalid logical Memory document key".to_owned(),
            ));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
enum MemoryLocations {
    Files(ScopePaths),
    Logical {
        global: MemoryDocumentKey,
        project: MemoryDocumentKey,
    },
}
impl MemoryLocations {
    fn key(&self, scope: Scope) -> String {
        match self {
            Self::Files(paths) => paths.wire_path(scope),
            Self::Logical { global, project } => match scope {
                Scope::Global => global.as_str().to_owned(),
                Scope::Project => project.as_str().to_owned(),
            },
        }
    }
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

/// One immutable resident version selected for a model request or client view.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemorySnapshot {
    pub scope: MemoryScope,
    pub revision: i64,
    pub source: String,
    pub content: String,
    pub digest: String,
    pub projected_revision: i64,
    pub projection_error: Option<String>,
    pub withheld_entries: usize,
}

/// Candidate or resident-store failure.
#[derive(Debug, thiserror::Error)]
pub enum MemoryServiceError {
    #[error("memory access is not authorized for this scope")]
    Denied,
    #[error("the Memory state service is temporarily unavailable")]
    Unavailable,
    #[error("Memory state changed concurrently")]
    Conflict,
    #[error("Memory state is corrupt or incompatible")]
    InvalidData,
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
            Self::Database(_)
            | Self::Denied
            | Self::Unavailable
            | Self::Conflict
            | Self::InvalidData => false,
        }
    }
}

/// The single owner of candidate validation, promotion, apply, and undo.
#[derive(Clone)]
pub struct MemoryService {
    persistence: Arc<dyn MemoryPersistence>,
    authority: Arc<dyn MemoryAuthority>,
    locations: MemoryLocations,
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
        Self::with_persistence(
            Arc::new(SqliteMemoryPersistence::new(pool)),
            Arc::new(LocalMemoryAuthority),
            paths,
            limits,
            promotion,
        )
    }

    /// Inject a coherent Memory backend without changing validation, rendering,
    /// candidate transitions or the local file-projection contract.
    #[must_use]
    pub fn with_persistence(
        persistence: Arc<dyn MemoryPersistence>,
        authority: Arc<dyn MemoryAuthority>,
        paths: ScopePaths,
        limits: ScopeLimits,
        promotion: PromotionPolicy,
    ) -> Self {
        Self {
            persistence,
            authority,
            locations: MemoryLocations::Files(paths),
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
    pub fn paths(&self) -> Option<&ScopePaths> {
        match &self.locations {
            MemoryLocations::Files(paths) => Some(paths),
            MemoryLocations::Logical { .. } => None,
        }
    }

    /// Use logical storage identities without filesystem discovery or projection.
    pub fn storage_only(
        persistence: Arc<dyn MemoryPersistence>,
        authority: Arc<dyn MemoryAuthority>,
        global: MemoryDocumentKey,
        project: MemoryDocumentKey,
        limits: ScopeLimits,
        promotion: PromotionPolicy,
    ) -> Result<Self, MemoryServiceError> {
        if global == project {
            return Err(MemoryServiceError::Invalid(
                "Memory scopes need distinct keys".to_owned(),
            ));
        }
        Ok(Self {
            persistence,
            authority,
            locations: MemoryLocations::Logical { global, project },
            limits,
            promotion,
            observer: None,
        })
    }

    fn local_paths(&self) -> Result<&ScopePaths, MemoryServiceError> {
        self.paths().ok_or(MemoryServiceError::Denied)
    }

    pub fn scope_limit(&self, scope: MemoryScope) -> usize {
        self.limits.for_scope(Scope::from(scope))
    }

    pub fn scope_identity(&self, scope: MemoryScope) -> Result<String, MemoryServiceError> {
        self.authority.authorize(scope, MemoryAccess::Read)?;
        self.resolved_path(Scope::from(scope))
    }

    pub fn propose(
        &self,
        proposal: MemoryProposal,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.propose_with_policy(proposal, true, None, ResidentMemoryAuthority::Host)
    }

    /// Model-visible maintenance is data-only, revision-aware and policy-fenced.
    pub fn update_from_model(
        &self,
        proposal: MemoryProposal,
        mut expected_revision: Option<i64>,
        session_id: &str,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.authority
            .authorize(proposal.scope, MemoryAccess::Propose)?;
        if expected_revision.is_none()
            && matches!(
                proposal.action,
                MemoryAction::Replace | MemoryAction::Remove
            )
        {
            let current = self.document(Scope::from(proposal.scope))?;
            let exact = proposal.old_text.as_deref().unwrap_or_default().trim();
            if !current.entries.iter().any(|entry| entry == exact) {
                return Err(MemoryServiceError::Invalid(
                    "replace/remove need expected_revision from memory_read, or the exact full old entry"
                        .to_owned(),
                ));
            }
            expected_revision = Some(current.revision);
        }
        self.propose_with_policy(
            proposal,
            true,
            expected_revision,
            ResidentMemoryAuthority::Model { session_id },
        )
    }

    /// Explicitly stage a candidate for a caller that requested review.
    /// Ordinary updates and source retractions do not use this path.
    pub fn propose_for_review(
        &self,
        proposal: MemoryProposal,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.propose_with_policy(proposal, false, None, ResidentMemoryAuthority::Host)
    }

    fn propose_with_policy(
        &self,
        mut proposal: MemoryProposal,
        allow_automatic_promotion: bool,
        expected_revision: Option<i64>,
        authority: ResidentMemoryAuthority<'_>,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.authority
            .authorize(proposal.scope, MemoryAccess::Propose)?;
        proposal.content = proposal.content.map(|content| content.trim().to_owned());
        proposal.old_text = proposal.old_text.map(|text| text.trim().to_owned());
        let confidence = confidence_basis_points(proposal.confidence)?;
        let reason = proposal.reason.trim();
        if reason.is_empty() || reason.len() > 2_048 {
            return Err(MemoryServiceError::Invalid(
                "reason must be non-empty and at most 2048 bytes".to_owned(),
            ));
        }
        let operation = operation(
            proposal.action,
            proposal.content.as_deref(),
            proposal.old_text.as_deref(),
        )?;
        let fingerprint = proposal_fingerprint(&proposal)?;
        let scope = Scope::from(proposal.scope);
        let resident = self.document(scope)?;
        if expected_revision.is_some_and(|revision| revision != resident.revision) {
            return Err(MemoryServiceError::Invalid(format!(
                "memory revision changed: expected {expected_revision:?}, current {}; read the current memory and retry",
                resident.revision
            )));
        }
        crate::store::preview_entries(
            scope,
            self.limits.for_scope(scope),
            &resident.entries,
            std::slice::from_ref(&operation),
        )?;

        let now = zuno_db::message::now_millis();
        let candidate = NewMemoryCandidate {
            id: format!("mem_{}", Uuid::new_v4().simple()),
            target: proposal.scope,
            target_path: resident.path,
            action: proposal.action,
            content: proposal.content,
            old_text: proposal.old_text,
            reason: reason.to_owned(),
            confidence,
            source: proposal.source,
            source_session_id: proposal.source_session_id,
            source_message_id: proposal.source_message_id,
            fingerprint,
            base_revision: Some(resident.revision),
            evidence: None,
            time_created: now,
        };
        let insert = match authority {
            ResidentMemoryAuthority::Model { session_id } => self
                .persistence
                .create_model_candidate(candidate, session_id)?,
            _ => self.persistence.create_candidate(candidate)?,
        };
        let candidate = insert.record;
        if !insert.inserted {
            return Ok(candidate);
        }
        self.notify();
        if allow_automatic_promotion && self.promotion.applies(confidence) {
            return self.apply_with_authority(candidate.id(), authority, now);
        }
        Ok(candidate)
    }

    pub fn candidate(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let candidate = self.persistence.candidate(id)?;
        self.ensure_owned_path(&candidate)?;
        self.authority
            .authorize(candidate.projection.scope, MemoryAccess::Read)?;
        Ok(candidate)
    }

    pub fn apply(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.apply_with_authority(
            id,
            ResidentMemoryAuthority::Host,
            zuno_db::message::now_millis(),
        )
    }

    pub fn apply_from_learning(
        &self,
        id: &str,
        job_id: &str,
        lease: &zuno_db::learning_job::LearningLease,
        now: i64,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.apply_with_authority(id, ResidentMemoryAuthority::Learning { job_id, lease }, now)
    }

    fn apply_with_authority(
        &self,
        id: &str,
        authority: ResidentMemoryAuthority<'_>,
        now: i64,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let candidate = self.candidate(id)?;
        self.authority
            .authorize(candidate.projection.scope, MemoryAccess::Apply)?;
        if matches!(authority, ResidentMemoryAuthority::Learning { .. }) {
            self.authority
                .authorize(candidate.projection.scope, MemoryAccess::Maintain)?;
        }
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
        let resident = self.document(scope)?;
        let after = match crate::store::preview_entries(
            scope,
            self.limits.for_scope(scope),
            &resident.entries,
            std::slice::from_ref(&operation),
        ) {
            Ok(after) => after,
            Err(error) => {
                let _failed = self.persistence.set_candidate_status(
                    id,
                    MemoryCandidateStatus::Failed,
                    Some(&error.to_string()),
                    zuno_db::message::now_millis(),
                );
                self.notify();
                return Err(error.into());
            }
        };
        let committed = self.persistence.commit_document(ResidentMemoryCommit {
            path: &resident.path,
            scope: scope.into(),
            expected_revision: resident.revision,
            before: &resident.entries,
            after: &after,
            candidate_id: id,
            operation: ResidentMemoryOperation::Apply,
            now,
            authority,
        })?;
        self.project_document(&committed)?;
        let record = self.persistence.candidate(id)?;
        self.notify();
        Ok(record)
    }

    pub fn reject(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let candidate = self.candidate(id)?;
        self.authority
            .authorize(candidate.projection.scope, MemoryAccess::Reject)?;
        if !matches!(
            candidate.projection.status,
            MemoryCandidateStatus::Pending | MemoryCandidateStatus::Failed
        ) {
            return Err(MemoryServiceError::Invalid(format!(
                "candidate {id} cannot be rejected from {}",
                candidate.projection.status.as_str()
            )));
        }
        let record = self.persistence.set_candidate_status(
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
        let candidate = self.candidate(id)?;
        self.authority
            .authorize(candidate.projection.scope, MemoryAccess::Edit)?;
        let content = content.map(|value| value.trim().to_owned());
        let old_text = old_text.map(|value| value.trim().to_owned());
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
        let scope = Scope::from(candidate.projection.scope);
        let resident = self.document(scope)?;
        crate::store::preview_entries(
            scope,
            self.limits.for_scope(scope),
            &resident.entries,
            std::slice::from_ref(&operation),
        )?;
        let record =
            self.persistence
                .edit_candidate(zuno_db::memory_candidate::MemoryCandidateEdit {
                    id,
                    content: content.as_deref(),
                    old_text: old_text.as_deref(),
                    reason,
                    confidence,
                    base_revision: resident.revision,
                    time_updated: zuno_db::message::now_millis(),
                })?;
        self.notify();
        Ok(record)
    }

    pub fn undo(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        let candidate = self.candidate(id)?;
        self.authority
            .authorize(candidate.projection.scope, MemoryAccess::Undo)?;
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
        let resident = self.document(Scope::from(candidate.projection.scope))?;
        let committed = self.persistence.commit_document(ResidentMemoryCommit {
            path: &resident.path,
            scope: candidate.projection.scope,
            expected_revision: resident.revision,
            before: after,
            after: before,
            candidate_id: id,
            operation: ResidentMemoryOperation::Undo,
            now: zuno_db::message::now_millis(),
            authority: ResidentMemoryAuthority::Host,
        })?;
        self.project_document(&committed)?;
        let record = self.persistence.candidate(id)?;
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
        for view in self.read_views()? {
            entries.extend(
                view.entries
                    .into_iter()
                    .map(|content| MemoryEntryProjection {
                        scope: view.document.scope,
                        content,
                    }),
            );
        }
        Ok(entries)
    }

    pub fn snapshot(&self, scope: Scope) -> Result<MemorySnapshot, MemoryServiceError> {
        self.authority.authorize(scope.into(), MemoryAccess::Read)?;
        if self.import_required(scope)? {
            return Ok(MemorySnapshot {
                scope: scope.into(),
                revision: 0,
                source: format!("{}#unimported", self.locations.key(scope)),
                content: String::new(),
                digest: hex::encode(Sha256::digest(b"")),
                projected_revision: 0,
                projection_error: Some(
                    "An unresolved legacy write requires inspection and explicit import."
                        .to_owned(),
                ),
                withheld_entries: 0,
            });
        }
        let document = self.document(scope)?;
        let view = self
            .persistence
            .views(&[document.path])?
            .into_iter()
            .next()
            .expect("one requested memory view");
        Ok(self.snapshot_from_view(view))
    }

    /// Freeze both scopes from one SQLite read transaction, not from two moments.
    pub fn snapshots(&self) -> Result<Vec<MemorySnapshot>, MemoryServiceError> {
        let mut snapshots = self
            .read_views()?
            .into_iter()
            .map(|view| self.snapshot_from_view(view))
            .collect::<Vec<_>>();
        for scope in Scope::ALL {
            if !snapshots
                .iter()
                .any(|snapshot| snapshot.scope == scope.into())
            {
                snapshots.push(self.snapshot(scope)?);
            }
        }
        snapshots.sort_by_key(|snapshot| snapshot.scope.as_str());
        Ok(snapshots)
    }

    /// Canonical entries plus source status for the bounded maintenance planner.
    pub fn read_views(&self) -> Result<Vec<ResidentMemoryView>, MemoryServiceError> {
        let mut paths = Vec::new();
        for scope in Scope::ALL {
            self.authority.authorize(scope.into(), MemoryAccess::Read)?;
            if !self.import_required(scope)? {
                paths.push(self.document(scope)?.path);
            }
        }
        self.persistence.views(&paths)
    }

    pub fn read_for_model(
        &self,
        session_id: &str,
    ) -> Result<Vec<ResidentMemoryView>, MemoryServiceError> {
        self.persistence.require_model_use(session_id)?;
        self.read_views()
    }

    fn snapshot_from_view(&self, view: ResidentMemoryView) -> MemorySnapshot {
        let document = view.document;
        let scope = Scope::from(document.scope);
        let block =
            crate::render_block_with_limit(scope, &view.entries, self.limits.for_scope(scope));
        let content = if block.is_empty() {
            block
        } else {
            format!(
                "Memory scope: {}; revision: {}\n{block}",
                document.scope.as_str(),
                document.revision
            )
        };
        let digest = hex::encode(Sha256::digest(content.as_bytes()));
        MemorySnapshot {
            scope: document.scope,
            revision: document.revision,
            source: format!(
                "{}#revision={}&recall={digest}",
                document.path, document.revision
            ),
            content,
            digest,
            projected_revision: document.projected_revision,
            projection_error: document.projection_error,
            withheld_entries: view.suppressed.len(),
        }
    }

    /// Reconcile process loss around apply or undo without replaying a write.
    pub fn reconcile(&self) -> Result<(), MemoryServiceError> {
        for scope in Scope::ALL {
            self.authority
                .authorize(scope.into(), MemoryAccess::Maintain)?;
        }
        if matches!(self.locations, MemoryLocations::Logical { .. }) {
            if self.records()?.iter().any(|candidate| {
                matches!(
                    candidate.projection.status,
                    MemoryCandidateStatus::Applying
                        | MemoryCandidateStatus::Undoing
                        | MemoryCandidateStatus::Uncertain
                )
            }) {
                return Err(MemoryServiceError::Invalid(
                    "logical Memory contains an unresolved legacy write; inspect its authoritative state".to_owned()));
            }
            return Ok(());
        }
        let mut changed = false;
        for candidate in self.records()?.into_iter().filter(|candidate| {
            matches!(
                candidate.projection.status,
                MemoryCandidateStatus::Applying | MemoryCandidateStatus::Undoing
            )
        }) {
            changed = true;
            let Some(before) = candidate.before_entries.as_deref() else {
                self.settle_reconciled(
                    candidate.id(),
                    MemoryCandidateStatus::Uncertain,
                    "applying candidate has no before snapshot",
                )?;
                continue;
            };
            let Some(after) = candidate.after_entries.as_deref() else {
                self.settle_reconciled(
                    candidate.id(),
                    MemoryCandidateStatus::Uncertain,
                    "applying candidate has no after snapshot",
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
            self.settle_reconciled(
                candidate.id(),
                status,
                "reconciled after process restart without replay",
            )?;
        }
        for scope in Scope::ALL {
            if self.import_required(scope)? {
                continue;
            }
            let document = self.document(scope)?;
            changed |= self.project_document(&document)?;
        }
        if changed {
            self.notify();
        }
        Ok(())
    }

    /// Settle one reconciled candidate, yielding to a live writer that got there first.
    ///
    /// `set_status` is a compare-and-set on `status`, so a candidate that an active
    /// process settled between `list_inflight_for_paths` and this write reports
    /// `DbError::Conflict`, and one that was pruned reports `DbError::NotFound`.
    /// Both mean this pass has nothing left to settle for that row: the live writer
    /// observed the resident file itself, which is better evidence than a restart
    /// reconciler has. Skip the row and keep reconciling the rest rather than
    /// failing the whole pass and leaving later candidates in flight.
    fn settle_reconciled(
        &self,
        id: &str,
        status: MemoryCandidateStatus,
        detail: &str,
    ) -> Result<(), MemoryServiceError> {
        match self.persistence.set_candidate_status(
            id,
            status,
            Some(detail),
            zuno_db::message::now_millis(),
        ) {
            Ok(_) => Ok(()),
            Err(
                MemoryServiceError::Database(DbError::Conflict { .. } | DbError::NotFound { .. })
                | MemoryServiceError::Conflict,
            ) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn records(&self) -> Result<Vec<MemoryCandidateRecord>, MemoryServiceError> {
        for scope in Scope::ALL {
            self.authority.authorize(scope.into(), MemoryAccess::Read)?;
        }
        let global = self.locations.key(Scope::Global);
        let project = self.locations.key(Scope::Project);
        let mut records = self.persistence.candidates_for_paths(&global, &project)?;
        let canonical_global = self.resolved_path(Scope::Global)?;
        let canonical_project = self.resolved_path(Scope::Project)?;
        if global != canonical_global || project != canonical_project {
            records.extend(
                self.persistence
                    .candidates_for_paths(&canonical_global, &canonical_project)?,
            );
            records.sort_by_key(|record| {
                std::cmp::Reverse((record.projection.time_created, record.projection.id.clone()))
            });
            records.dedup_by(|left, right| left.projection.id == right.projection.id);
        }
        Ok(records)
    }

    fn open(&self, scope: Scope) -> Result<MemoryStore, MemoryServiceError> {
        let paths = self.local_paths()?;
        validate_managed_path(paths.for_scope(scope))?;
        MemoryStore::open_with_limit(
            scope,
            paths.for_scope(scope).to_path_buf(),
            self.limits.for_scope(scope),
        )
        .map_err(Into::into)
    }

    fn ensure_owned_path(
        &self,
        candidate: &MemoryCandidateRecord,
    ) -> Result<(), MemoryServiceError> {
        let expected = self.resolved_path(Scope::from(candidate.projection.scope))?;
        if matches!(self.locations, MemoryLocations::Logical { .. }) {
            return if candidate.target_path == expected {
                Ok(())
            } else {
                Err(MemoryServiceError::Denied)
            };
        }
        let candidate_path =
            zuno_atomic_file::canonical_destination(Path::new(&candidate.target_path))
                .map_err(|_| MemoryServiceError::Denied)?;
        if candidate_path.to_string_lossy() != expected {
            return Err(MemoryServiceError::Denied);
        }
        Ok(())
    }

    fn resolved_path(&self, scope: Scope) -> Result<String, MemoryServiceError> {
        if matches!(self.locations, MemoryLocations::Logical { .. }) {
            return Ok(self.locations.key(scope));
        }
        let paths = self.local_paths()?;
        validate_managed_path(paths.for_scope(scope))?;
        let path =
            zuno_atomic_file::canonical_destination(paths.for_scope(scope)).map_err(|source| {
                MemoryError::Io {
                    operation: "resolve resident memory identity",
                    path: paths.for_scope(scope).to_path_buf(),
                    source,
                }
            })?;
        path.to_str().map(str::to_owned).ok_or_else(|| {
            MemoryServiceError::Invalid("resident memory path must be valid Unicode".to_owned())
        })
    }

    fn document(&self, scope: Scope) -> Result<ResidentMemoryDocument, MemoryServiceError> {
        self.authority.authorize(scope.into(), MemoryAccess::Read)?;
        let key = self.resolved_path(scope)?;
        if let Some(document) = self.persistence.document(&key)? {
            return Ok(document);
        }
        if self.records()?.iter().any(|candidate| {
            candidate.projection.scope == scope.into()
                && matches!(
                    candidate.projection.status,
                    MemoryCandidateStatus::Uncertain
                        | MemoryCandidateStatus::Applying
                        | MemoryCandidateStatus::Undoing
                )
        }) {
            return Err(MemoryServiceError::Invalid(format!(
                "resident memory at {key} has an unresolved legacy write; inspect it and explicitly import the projection"
            )));
        }
        let entries = if matches!(self.locations, MemoryLocations::Logical { .. }) {
            Vec::new()
        } else {
            self.open(scope)?.entries().to_vec()
        };
        self.persistence
            .adopt(&key, scope.into(), &entries, zuno_db::message::now_millis())
    }

    fn project_document(
        &self,
        document: &ResidentMemoryDocument,
    ) -> Result<bool, MemoryServiceError> {
        if matches!(self.locations, MemoryLocations::Logical { .. }) {
            if document.path != self.resolved_path(Scope::from(document.scope))? {
                return Err(MemoryServiceError::Denied);
            }
            return Ok(true);
        }
        let paths = self.local_paths()?;
        let expected = if document.projected_revision == 0 {
            Vec::new()
        } else {
            self.persistence
                .revision_entries(&document.path, document.projected_revision)?
        };
        let scope = Scope::from(document.scope);
        let projection = (|| -> Result<(), MemoryError> {
            validate_managed_path(paths.for_scope(scope))?;
            validate_managed_path(Path::new(&document.path))?;
            let mut file = MemoryStore::open_with_limit(
                scope,
                PathBuf::from(&document.path),
                self.limits.for_scope(scope),
            )?;
            if file.entries() != document.entries {
                let absent = std::fs::symlink_metadata(&document.path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
                let before = if absent { &[][..] } else { expected.as_slice() };
                file.replace_exact(before, &document.entries)?;
            }
            Ok(())
        })();
        let error = projection.err().map(|error| error.to_string());
        self.persistence
            .record_projection(&document.path, document.revision, error.as_deref())
    }

    fn import_required(&self, scope: Scope) -> Result<bool, MemoryServiceError> {
        if matches!(self.locations, MemoryLocations::Logical { .. }) {
            return Ok(false);
        }
        let paths = self.local_paths()?;
        let key =
            zuno_atomic_file::canonical_destination(paths.for_scope(scope)).map_err(|source| {
                MemoryError::Io {
                    operation: "resolve resident memory identity",
                    path: paths.for_scope(scope).to_path_buf(),
                    source,
                }
            })?;
        if self.persistence.document(&key.to_string_lossy())?.is_some() {
            return Ok(false);
        }
        Ok(self.records()?.iter().any(|candidate| {
            candidate.projection.scope == scope.into()
                && matches!(
                    candidate.projection.status,
                    MemoryCandidateStatus::Uncertain
                        | MemoryCandidateStatus::Applying
                        | MemoryCandidateStatus::Undoing
                )
        }))
    }

    /// Accept the currently inspected file as a new version, preserving prior revisions.
    pub fn import_projection(
        &self,
        scope: MemoryScope,
    ) -> Result<MemorySnapshot, MemoryServiceError> {
        self.authority.authorize(scope, MemoryAccess::Import)?;
        let scope = Scope::from(scope);
        let paths = self.local_paths()?;
        let key =
            zuno_atomic_file::canonical_destination(paths.for_scope(scope)).map_err(|source| {
                MemoryError::Io {
                    operation: "resolve resident memory identity",
                    path: paths.for_scope(scope).to_path_buf(),
                    source,
                }
            })?;
        let resident = self.open(scope)?;
        let operations: Vec<_> = resident
            .entries()
            .iter()
            .map(|content| operation(MemoryAction::Add, Some(content), None))
            .collect::<Result<_, _>>()?;
        let entries = if operations.is_empty() {
            Vec::new()
        } else {
            crate::store::preview_entries(scope, self.limits.for_scope(scope), &[], &operations)?
        };
        self.persistence.import_projection(
            &key.to_string_lossy(),
            scope.into(),
            &entries,
            zuno_db::message::now_millis(),
        )?;
        self.notify();
        self.snapshot(scope)
    }

    pub fn projection_entries(&self, scope: Scope) -> Result<Vec<String>, MemoryServiceError> {
        self.authority.authorize(scope.into(), MemoryAccess::Read)?;
        Ok(self.open(scope)?.entries().to_vec())
    }

    fn notify(&self) {
        if let Some(observer) = &self.observer {
            observer.changed();
        }
    }
}

/// Memory has a fixed, application-owned file and its immediate managed
/// directory. A repository-controlled link must not turn the managed-data
/// approval exemption into arbitrary filesystem access. The selected worktree
/// and config-root prefixes may still have ordinary user-selected aliases.
///
/// On the pinned Rust toolchain, FileType::is_symlink also rejects Windows
/// name-surrogate reparse points (including junctions), not cloud placeholders.
fn validate_managed_path(path: &Path) -> Result<(), MemoryError> {
    for candidate in std::iter::once(path).chain(path.parent()) {
        let metadata = match std::fs::symlink_metadata(candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(MemoryError::Io {
                    operation: "inspect managed memory path",
                    path: candidate.to_path_buf(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(MemoryError::Io {
                operation: "validate managed memory path",
                path: candidate.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed memory files and directories must not be symbolic links or junctions",
                ),
            });
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zuno_db::memory_candidate::MemoryCandidateStore;

    fn fixture(directory: &TempDir) -> (Arc<Pool>, MemoryService) {
        let pool = Arc::new(Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
        let mut connection = pool.open_connection().expect("database connection");
        zuno_db::migration::apply(&mut connection).expect("initialize schema");
        drop(connection);
        let service = MemoryService::new(
            Arc::clone(&pool),
            crate::ScopePaths::at(
                directory.path().join("global").join("MEMORY.md"),
                directory.path().join("project").join("RULES.md"),
            ),
            ScopeLimits::default(),
            crate::PromotionPolicy::Review,
        );
        (pool, service)
    }

    fn proposal() -> MemoryProposal {
        MemoryProposal {
            scope: MemoryScope::Project,
            action: MemoryAction::Add,
            content: Some("durable entry".to_owned()),
            old_text: None,
            reason: "verified repository rule".to_owned(),
            confidence: 1.0,
            source: MemorySource::User,
            source_session_id: None,
            source_message_id: None,
        }
    }

    #[test]
    fn reconciling_yields_to_a_writer_that_already_settled_the_candidate() {
        let directory = TempDir::new().expect("temp dir");
        let (pool, service) = fixture(&directory);
        let store = MemoryCandidateStore::new(pool);
        let candidate = service.propose(proposal()).expect("proposal");
        store
            .begin_apply(candidate.id(), &[], &["durable entry".to_owned()], 20)
            .expect("record an interrupted apply");

        // A live writer observes the resident file and settles the candidate first.
        store
            .set_status(candidate.id(), MemoryCandidateStatus::Applied, None, 30)
            .expect("live settlement");

        // The bare compare-and-set is what makes the tolerance necessary: `Applied` is
        // not a state `Failed` may be entered from, so the write is refused outright.
        let conflict = store
            .set_status(candidate.id(), MemoryCandidateStatus::Failed, None, 31)
            .expect_err("the compare-and-set refuses an already settled candidate");
        assert!(
            matches!(conflict, DbError::Conflict { .. }),
            "unexpected error: {conflict}"
        );

        // The restart reconciler decided from the stale in-flight snapshot. It must
        // neither overwrite that settlement nor fail the pass it is in the middle of.
        service
            .settle_reconciled(
                candidate.id(),
                MemoryCandidateStatus::Failed,
                "reconciled after process restart without replay",
            )
            .expect("a lost settlement race is not an error");
        assert_eq!(
            store
                .get(candidate.id())
                .expect("candidate")
                .projection
                .status,
            MemoryCandidateStatus::Applied
        );
    }

    #[test]
    fn reconciling_a_pruned_candidate_is_not_an_error() {
        let directory = TempDir::new().expect("temp dir");
        let (pool, service) = fixture(&directory);
        let missing = "mem_pruned_between_the_query_and_the_write";
        let absent = MemoryCandidateStore::new(pool)
            .set_status(missing, MemoryCandidateStatus::Failed, None, 31)
            .expect_err("the compare-and-set reports a row that is not there");
        assert!(
            matches!(absent, DbError::NotFound { .. }),
            "unexpected error: {absent}"
        );
        service
            .settle_reconciled(
                missing,
                MemoryCandidateStatus::Failed,
                "reconciled after process restart without replay",
            )
            .expect("a candidate that no longer exists has nothing to settle");
    }
}
