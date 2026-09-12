use std::sync::Arc;

use zuno_db::memory_candidate::MemoryCandidateStore;
use zuno_db::{Pool, migration};
use zuno_memory::authority::{LocalMemoryAuthority, MemoryAccess, MemoryAuthority};
use zuno_memory::persistence::{MemoryPersistence, SqliteMemoryPersistence};
use zuno_memory::service::{
    MemoryDocumentKey, MemoryProposal, MemoryService, MemoryServiceError, PromotionPolicy,
    ScopePaths,
};
use zuno_memory::{Scope, ScopeLimits};
use zuno_paths::DbLocation;
use zuno_types::{MemoryAction, MemoryCandidateStatus, MemoryScope, MemorySource};

fn pool() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).unwrap());
    migration::apply(&mut pool.get().unwrap()).unwrap();
    pool
}

fn proposal() -> MemoryProposal {
    MemoryProposal {
        scope: MemoryScope::Project,
        action: MemoryAction::Add,
        content: Some("Keep reviewed deployment instructions.".to_owned()),
        old_text: None,
        reason: "A verified project convention.".to_owned(),
        confidence: 1.0,
        source: MemorySource::User,
        source_session_id: None,
        source_message_id: None,
    }
}

fn paths(directory: &tempfile::TempDir, project: &str) -> ScopePaths {
    ScopePaths::at(
        directory.path().join("global").join("MEMORY.md"),
        directory.path().join(project).join("RULES.md"),
    )
}

struct Deny(MemoryAccess);
impl MemoryAuthority for Deny {
    fn authorize(
        &self,
        _scope: MemoryScope,
        access: MemoryAccess,
    ) -> Result<(), MemoryServiceError> {
        if access == self.0 {
            Err(MemoryServiceError::Denied)
        } else {
            Ok(())
        }
    }
}

fn service(
    backend: Arc<dyn MemoryPersistence>,
    authority: Arc<dyn MemoryAuthority>,
    paths: ScopePaths,
) -> MemoryService {
    MemoryService::with_persistence(
        backend,
        authority,
        paths,
        ScopeLimits::default(),
        PromotionPolicy::Review,
    )
}

#[test]
fn injected_backend_preserves_candidates_revisions_projection_and_undo() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool();
    let backend: Arc<dyn MemoryPersistence> = Arc::new(SqliteMemoryPersistence::new(pool.clone()));
    let memory = service(
        backend.clone(),
        Arc::new(LocalMemoryAuthority),
        paths(&directory, "project"),
    );
    let candidate = memory.propose(proposal()).unwrap();
    let applied = memory.apply(candidate.id()).unwrap();
    assert_eq!(applied.projection.status, MemoryCandidateStatus::Applied);
    let snapshot = memory.snapshot(Scope::Project).unwrap();
    assert!(
        snapshot
            .content
            .contains("Keep reviewed deployment instructions.")
    );
    assert!(snapshot.revision > 1);
    drop(memory);
    let reopened = service(
        backend,
        Arc::new(LocalMemoryAuthority),
        paths(&directory, "project"),
    );
    assert_eq!(reopened.snapshot(Scope::Project).unwrap(), snapshot);
    reopened.undo(candidate.id()).unwrap();
    assert!(reopened.entries().unwrap().is_empty());
    assert_eq!(
        MemoryCandidateStore::new(pool)
            .get(candidate.id())
            .unwrap()
            .projection
            .status,
        MemoryCandidateStatus::Undone
    );
}

#[test]
fn authority_denial_prevents_proposal_and_model_managed_writes() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool();
    let memory = service(
        Arc::new(SqliteMemoryPersistence::new(pool.clone())),
        Arc::new(Deny(MemoryAccess::Propose)),
        paths(&directory, "project"),
    );
    assert!(matches!(
        memory.propose(proposal()),
        Err(MemoryServiceError::Denied)
    ));
    assert!(matches!(
        memory.update_from_model(proposal(), None, "ses_unadmitted"),
        Err(MemoryServiceError::Denied)
    ));
    let connection = pool.get().unwrap();
    for table in ["memory_candidate", "resident_memory_document"] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "denied writes must not create {table}");
    }
}

#[test]
fn proposal_authority_does_not_imply_apply_authority() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool();
    let memory = service(
        Arc::new(SqliteMemoryPersistence::new(pool.clone())),
        Arc::new(Deny(MemoryAccess::Apply)),
        paths(&directory, "project"),
    );
    let candidate = memory.propose(proposal()).unwrap();
    assert!(matches!(
        memory.apply(candidate.id()),
        Err(MemoryServiceError::Denied)
    ));
    assert_eq!(
        memory.candidate(candidate.id()).unwrap().projection.status,
        MemoryCandidateStatus::Pending
    );
    assert!(memory.entries().unwrap().is_empty());
}

#[test]
fn a_foreign_project_candidate_cannot_be_read_edited_rejected_or_applied() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool();
    let backend: Arc<dyn MemoryPersistence> = Arc::new(SqliteMemoryPersistence::new(pool.clone()));
    let first = service(
        backend.clone(),
        Arc::new(LocalMemoryAuthority),
        paths(&directory, "private-a"),
    );
    let other = service(
        backend,
        Arc::new(LocalMemoryAuthority),
        paths(&directory, "private-b"),
    );
    let candidate = first.propose(proposal()).unwrap();
    assert!(matches!(
        other.candidate(candidate.id()),
        Err(MemoryServiceError::Denied)
    ));
    assert!(matches!(
        other.edit(
            candidate.id(),
            Some("Overwritten.".to_owned()),
            None,
            "Edit".to_owned(),
            1.0
        ),
        Err(MemoryServiceError::Denied)
    ));
    assert!(matches!(
        other.reject(candidate.id()),
        Err(MemoryServiceError::Denied)
    ));
    assert!(matches!(
        other.apply(candidate.id()),
        Err(MemoryServiceError::Denied)
    ));
    assert!(matches!(
        other.undo(candidate.id()),
        Err(MemoryServiceError::Denied)
    ));
    assert_eq!(first.candidate(candidate.id()).unwrap(), candidate);
    assert!(!MemoryServiceError::Denied.to_string().contains("private-a"));
}

#[test]
fn an_unavailable_authoritative_backend_does_not_fall_back_to_the_projection_file() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool();
    let memory = service(
        Arc::new(SqliteMemoryPersistence::new(pool.clone())),
        Arc::new(LocalMemoryAuthority),
        paths(&directory, "project"),
    );
    let candidate = memory.propose(proposal()).unwrap();
    memory.apply(candidate.id()).unwrap();
    let path = memory.paths().unwrap().for_scope(Scope::Project);
    let before = std::fs::read(path).unwrap();
    pool.get()
        .unwrap()
        .execute_batch("ALTER TABLE resident_memory_document RENAME TO unavailable_document")
        .unwrap();
    assert!(matches!(
        memory.snapshot(Scope::Project),
        Err(MemoryServiceError::Database(_))
    ));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn logical_memory_preserves_versions_without_resolving_or_creating_files() {
    let pool = pool();
    let backend: Arc<dyn MemoryPersistence> = Arc::new(SqliteMemoryPersistence::new(pool.clone()));
    let memory = MemoryService::storage_only(
        backend.clone(),
        Arc::new(LocalMemoryAuthority),
        MemoryDocumentKey::new("private:global").unwrap(),
        MemoryDocumentKey::new("private:workspace").unwrap(),
        ScopeLimits::default(),
        PromotionPolicy::Review,
    )
    .unwrap();
    assert!(memory.paths().is_none());
    assert_eq!(
        memory.scope_identity(MemoryScope::Project).unwrap(),
        "private:workspace"
    );
    let candidate = memory.propose(proposal()).unwrap();
    memory.apply(candidate.id()).unwrap();
    let snapshot = memory.snapshot(Scope::Project).unwrap();
    assert!(
        snapshot
            .content
            .contains("Keep reviewed deployment instructions.")
    );
    assert!(snapshot.source.starts_with("private:workspace"));
    assert_eq!(candidate.target_path, "private:workspace");
    assert!(matches!(
        memory.import_projection(MemoryScope::Project),
        Err(MemoryServiceError::Denied)
    ));
    memory.undo(candidate.id()).unwrap();
    assert!(memory.entries().unwrap().is_empty());
    assert!(memory.snapshot(Scope::Project).unwrap().revision > snapshot.revision);

    let foreign = MemoryService::storage_only(
        backend,
        Arc::new(LocalMemoryAuthority),
        MemoryDocumentKey::new("another:global").unwrap(),
        MemoryDocumentKey::new("another:workspace").unwrap(),
        ScopeLimits::default(),
        PromotionPolicy::Review,
    )
    .unwrap();
    assert!(matches!(
        foreign.candidate(candidate.id()),
        Err(MemoryServiceError::Denied)
    ));
    assert!(MemoryDocumentKey::new("../private").is_err());
}
