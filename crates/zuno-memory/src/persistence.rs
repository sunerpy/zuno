//! Persistence owned by the Memory data service.
//!
//! One provider owns candidates, document revisions, evidence and maintenance
//! settlement. They cannot be assembled from independent database instances.
//! These owner-side transactions are synchronous; an HTTP/state-service adapter
//! must run them in bounded blocking capacity, not block a worker's async reactor.

use std::sync::Arc;

use crate::service::MemoryServiceError;
use zuno_db::Pool;
use zuno_db::memory_candidate::{
    MemoryCandidateEdit, MemoryCandidateInsert, MemoryCandidateRecord, MemoryCandidateStore,
    NewMemoryCandidate,
};
use zuno_db::memory_maintenance::{
    MemoryBatchCommit, MemoryBatchResult, MemoryMaintenanceState, MemoryMaintenanceStore,
    MemorySourceRetraction,
};
use zuno_db::resident_memory::{
    ResidentMemoryCommit, ResidentMemoryDocument, ResidentMemoryStore, ResidentMemoryView,
};
use zuno_types::{MemoryCandidateStatus, MemoryScope};

/// Atomic Memory operations, independently replaceable from file projection and
/// from prompt/candidate validation. Every method must preserve its typed failure.
pub trait MemoryPersistence: Send + Sync {
    fn create_candidate(
        &self,
        candidate: NewMemoryCandidate,
    ) -> Result<MemoryCandidateInsert, MemoryServiceError>;
    fn create_model_candidate(
        &self,
        candidate: NewMemoryCandidate,
        session: &str,
    ) -> Result<MemoryCandidateInsert, MemoryServiceError>;
    fn candidate(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError>;
    fn candidates_for_paths(
        &self,
        global: &str,
        project: &str,
    ) -> Result<Vec<MemoryCandidateRecord>, MemoryServiceError>;
    fn edit_candidate(
        &self,
        input: MemoryCandidateEdit<'_>,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError>;
    fn set_candidate_status(
        &self,
        id: &str,
        status: MemoryCandidateStatus,
        error: Option<&str>,
        at: i64,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError>;

    fn document(&self, key: &str) -> Result<Option<ResidentMemoryDocument>, MemoryServiceError>;
    fn views(&self, keys: &[String]) -> Result<Vec<ResidentMemoryView>, MemoryServiceError>;
    fn require_model_use(&self, session: &str) -> Result<(), MemoryServiceError>;
    fn adopt(
        &self,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
        at: i64,
    ) -> Result<ResidentMemoryDocument, MemoryServiceError>;
    fn commit_document(
        &self,
        input: ResidentMemoryCommit<'_>,
    ) -> Result<ResidentMemoryDocument, MemoryServiceError>;
    fn revision_entries(&self, key: &str, revision: i64)
    -> Result<Vec<String>, MemoryServiceError>;
    fn record_projection(
        &self,
        key: &str,
        revision: i64,
        error: Option<&str>,
    ) -> Result<bool, MemoryServiceError>;
    fn import_projection(
        &self,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
        at: i64,
    ) -> Result<ResidentMemoryDocument, MemoryServiceError>;

    fn content_retired(&self, key: &str, content: &str) -> Result<bool, MemoryServiceError>;
    fn maintenance_state(
        &self,
        project: &str,
        key: &str,
    ) -> Result<Option<MemoryMaintenanceState>, MemoryServiceError>;
    /// Changes, evidence, revisions, job settlement and watermark share one transaction.
    fn commit_maintenance(
        &self,
        input: MemoryBatchCommit<'_>,
    ) -> Result<MemoryBatchResult, MemoryServiceError>;
    /// Source invalidation and retraction share one transaction, before recall.
    fn forget_sources(
        &self,
        ids: &[String],
        keys: &[String],
        session: Option<&str>,
        at: i64,
    ) -> Result<MemorySourceRetraction, MemoryServiceError>;
}

/// The local provider is constructed from one pool; its three stores are not
/// individually injectable, so their cross-table operations retain one authority.
pub struct SqliteMemoryPersistence {
    candidates: MemoryCandidateStore,
    documents: ResidentMemoryStore,
    maintenance: MemoryMaintenanceStore,
}

impl SqliteMemoryPersistence {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self {
            candidates: MemoryCandidateStore::new(pool.clone()),
            documents: ResidentMemoryStore::new(pool.clone()),
            maintenance: MemoryMaintenanceStore::new(pool),
        }
    }
}

impl MemoryPersistence for SqliteMemoryPersistence {
    fn create_candidate(
        &self,
        candidate: NewMemoryCandidate,
    ) -> Result<MemoryCandidateInsert, MemoryServiceError> {
        self.candidates.create_or_get(candidate).map_err(Into::into)
    }
    fn create_model_candidate(
        &self,
        candidate: NewMemoryCandidate,
        session: &str,
    ) -> Result<MemoryCandidateInsert, MemoryServiceError> {
        self.candidates
            .create_for_model(candidate, session)
            .map_err(Into::into)
    }
    fn candidate(&self, id: &str) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.candidates.get(id).map_err(Into::into)
    }
    fn candidates_for_paths(
        &self,
        global: &str,
        project: &str,
    ) -> Result<Vec<MemoryCandidateRecord>, MemoryServiceError> {
        self.candidates
            .list_for_paths(global, project)
            .map_err(Into::into)
    }
    fn edit_candidate(
        &self,
        input: MemoryCandidateEdit<'_>,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.candidates.edit_pending(input).map_err(Into::into)
    }
    fn set_candidate_status(
        &self,
        id: &str,
        status: MemoryCandidateStatus,
        error: Option<&str>,
        at: i64,
    ) -> Result<MemoryCandidateRecord, MemoryServiceError> {
        self.candidates
            .set_status(id, status, error, at)
            .map_err(Into::into)
    }
    fn document(&self, key: &str) -> Result<Option<ResidentMemoryDocument>, MemoryServiceError> {
        self.documents.get(key).map_err(Into::into)
    }
    fn views(&self, keys: &[String]) -> Result<Vec<ResidentMemoryView>, MemoryServiceError> {
        self.documents.views(keys).map_err(Into::into)
    }
    fn require_model_use(&self, session: &str) -> Result<(), MemoryServiceError> {
        self.documents
            .require_model_use(session)
            .map_err(Into::into)
    }
    fn adopt(
        &self,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
        at: i64,
    ) -> Result<ResidentMemoryDocument, MemoryServiceError> {
        self.documents
            .adopt(key, scope, entries, at)
            .map_err(Into::into)
    }
    fn commit_document(
        &self,
        input: ResidentMemoryCommit<'_>,
    ) -> Result<ResidentMemoryDocument, MemoryServiceError> {
        self.documents.commit(input).map_err(Into::into)
    }
    fn revision_entries(
        &self,
        key: &str,
        revision: i64,
    ) -> Result<Vec<String>, MemoryServiceError> {
        self.documents
            .revision_entries(key, revision)
            .map_err(Into::into)
    }
    fn record_projection(
        &self,
        key: &str,
        revision: i64,
        error: Option<&str>,
    ) -> Result<bool, MemoryServiceError> {
        self.documents
            .record_projection(key, revision, error)
            .map_err(Into::into)
    }
    fn import_projection(
        &self,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
        at: i64,
    ) -> Result<ResidentMemoryDocument, MemoryServiceError> {
        self.documents
            .import_projection(key, scope, entries, at)
            .map_err(Into::into)
    }
    fn content_retired(&self, key: &str, content: &str) -> Result<bool, MemoryServiceError> {
        self.maintenance
            .content_retired(key, content)
            .map_err(Into::into)
    }
    fn maintenance_state(
        &self,
        project: &str,
        key: &str,
    ) -> Result<Option<MemoryMaintenanceState>, MemoryServiceError> {
        self.maintenance.state(project, key).map_err(Into::into)
    }
    fn commit_maintenance(
        &self,
        input: MemoryBatchCommit<'_>,
    ) -> Result<MemoryBatchResult, MemoryServiceError> {
        self.maintenance.commit(input).map_err(Into::into)
    }
    fn forget_sources(
        &self,
        ids: &[String],
        keys: &[String],
        session: Option<&str>,
        at: i64,
    ) -> Result<MemorySourceRetraction, MemoryServiceError> {
        self.maintenance
            .forget_sources(ids, keys, session, at)
            .map_err(Into::into)
    }
}
