//! Persistence owned by the Memory data service.
//!
//! One provider owns candidates, document revisions, evidence and maintenance
//! settlement. They cannot be assembled from independent database instances.
//! These owner-side transactions are synchronous; an HTTP/state-service adapter
//! must run them in bounded blocking capacity, not block a worker's async reactor.

use std::sync::Arc;

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
use zuno_error::DbError;
use zuno_types::{MemoryCandidateStatus, MemoryScope};

/// Atomic Memory operations, independently replaceable from file projection and
/// from prompt/candidate validation. Every method must preserve its typed failure.
pub trait MemoryPersistence: Send + Sync {
    fn create_candidate(
        &self,
        candidate: NewMemoryCandidate,
    ) -> Result<MemoryCandidateInsert, DbError>;
    fn create_model_candidate(
        &self,
        candidate: NewMemoryCandidate,
        session: &str,
    ) -> Result<MemoryCandidateInsert, DbError>;
    fn candidate(&self, id: &str) -> Result<MemoryCandidateRecord, DbError>;
    fn candidates_for_paths(
        &self,
        global: &str,
        project: &str,
    ) -> Result<Vec<MemoryCandidateRecord>, DbError>;
    fn edit_candidate(
        &self,
        input: MemoryCandidateEdit<'_>,
    ) -> Result<MemoryCandidateRecord, DbError>;
    fn set_candidate_status(
        &self,
        id: &str,
        status: MemoryCandidateStatus,
        error: Option<&str>,
        at: i64,
    ) -> Result<MemoryCandidateRecord, DbError>;

    fn document(&self, key: &str) -> Result<Option<ResidentMemoryDocument>, DbError>;
    fn views(&self, keys: &[String]) -> Result<Vec<ResidentMemoryView>, DbError>;
    fn require_model_use(&self, session: &str) -> Result<(), DbError>;
    fn adopt(
        &self,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
        at: i64,
    ) -> Result<ResidentMemoryDocument, DbError>;
    fn commit_document(
        &self,
        input: ResidentMemoryCommit<'_>,
    ) -> Result<ResidentMemoryDocument, DbError>;
    fn revision_entries(&self, key: &str, revision: i64) -> Result<Vec<String>, DbError>;
    fn record_projection(
        &self,
        key: &str,
        revision: i64,
        error: Option<&str>,
    ) -> Result<bool, DbError>;
    fn import_projection(
        &self,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
        at: i64,
    ) -> Result<ResidentMemoryDocument, DbError>;

    fn content_retired(&self, key: &str, content: &str) -> Result<bool, DbError>;
    fn maintenance_state(
        &self,
        project: &str,
        key: &str,
    ) -> Result<Option<MemoryMaintenanceState>, DbError>;
    /// Changes, evidence, revisions, job settlement and watermark share one transaction.
    fn commit_maintenance(
        &self,
        input: MemoryBatchCommit<'_>,
    ) -> Result<MemoryBatchResult, DbError>;
    /// Source invalidation and retraction share one transaction, before recall.
    fn forget_sources(
        &self,
        ids: &[String],
        keys: &[String],
        session: Option<&str>,
        at: i64,
    ) -> Result<MemorySourceRetraction, DbError>;
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
    ) -> Result<MemoryCandidateInsert, DbError> {
        self.candidates.create_or_get(candidate)
    }
    fn create_model_candidate(
        &self,
        candidate: NewMemoryCandidate,
        session: &str,
    ) -> Result<MemoryCandidateInsert, DbError> {
        self.candidates.create_for_model(candidate, session)
    }
    fn candidate(&self, id: &str) -> Result<MemoryCandidateRecord, DbError> {
        self.candidates.get(id)
    }
    fn candidates_for_paths(
        &self,
        global: &str,
        project: &str,
    ) -> Result<Vec<MemoryCandidateRecord>, DbError> {
        self.candidates.list_for_paths(global, project)
    }
    fn edit_candidate(
        &self,
        input: MemoryCandidateEdit<'_>,
    ) -> Result<MemoryCandidateRecord, DbError> {
        self.candidates.edit_pending(input)
    }
    fn set_candidate_status(
        &self,
        id: &str,
        status: MemoryCandidateStatus,
        error: Option<&str>,
        at: i64,
    ) -> Result<MemoryCandidateRecord, DbError> {
        self.candidates.set_status(id, status, error, at)
    }
    fn document(&self, key: &str) -> Result<Option<ResidentMemoryDocument>, DbError> {
        self.documents.get(key)
    }
    fn views(&self, keys: &[String]) -> Result<Vec<ResidentMemoryView>, DbError> {
        self.documents.views(keys)
    }
    fn require_model_use(&self, session: &str) -> Result<(), DbError> {
        self.documents.require_model_use(session)
    }
    fn adopt(
        &self,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
        at: i64,
    ) -> Result<ResidentMemoryDocument, DbError> {
        self.documents.adopt(key, scope, entries, at)
    }
    fn commit_document(
        &self,
        input: ResidentMemoryCommit<'_>,
    ) -> Result<ResidentMemoryDocument, DbError> {
        self.documents.commit(input)
    }
    fn revision_entries(&self, key: &str, revision: i64) -> Result<Vec<String>, DbError> {
        self.documents.revision_entries(key, revision)
    }
    fn record_projection(
        &self,
        key: &str,
        revision: i64,
        error: Option<&str>,
    ) -> Result<bool, DbError> {
        self.documents.record_projection(key, revision, error)
    }
    fn import_projection(
        &self,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
        at: i64,
    ) -> Result<ResidentMemoryDocument, DbError> {
        self.documents.import_projection(key, scope, entries, at)
    }
    fn content_retired(&self, key: &str, content: &str) -> Result<bool, DbError> {
        self.maintenance.content_retired(key, content)
    }
    fn maintenance_state(
        &self,
        project: &str,
        key: &str,
    ) -> Result<Option<MemoryMaintenanceState>, DbError> {
        self.maintenance.state(project, key)
    }
    fn commit_maintenance(
        &self,
        input: MemoryBatchCommit<'_>,
    ) -> Result<MemoryBatchResult, DbError> {
        self.maintenance.commit(input)
    }
    fn forget_sources(
        &self,
        ids: &[String],
        keys: &[String],
        session: Option<&str>,
        at: i64,
    ) -> Result<MemorySourceRetraction, DbError> {
        self.maintenance.forget_sources(ids, keys, session, at)
    }
}
