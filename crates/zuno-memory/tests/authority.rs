use std::sync::{Arc, Barrier};
use tempfile::TempDir;
use zuno_db::{Pool, migration};
use zuno_memory::{MemoryProposal, MemoryService, PromotionPolicy, Scope, ScopeLimits, ScopePaths};
use zuno_paths::DbLocation;
use zuno_types::{MemoryAction, MemoryCandidateStatus, MemoryScope, MemorySource};

fn fixture() -> (TempDir, Arc<Pool>, MemoryService) {
    let directory = TempDir::new().expect("temporary memory");
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("database"));
    migration::apply(&mut pool.get().expect("connection")).expect("migration");
    let service = MemoryService::new(
        Arc::clone(&pool),
        ScopePaths::at(
            directory.path().join("global.md"),
            directory.path().join("project.md"),
        ),
        ScopeLimits::default(),
        PromotionPolicy::Review,
    );
    (directory, pool, service)
}

fn proposal(content: &str) -> MemoryProposal {
    MemoryProposal {
        scope: MemoryScope::Project,
        action: MemoryAction::Add,
        content: Some(content.to_owned()),
        old_text: None,
        reason: "Explicitly remember this project fact.".to_owned(),
        confidence: 1.0,
        source: MemorySource::User,
        source_session_id: None,
        source_message_id: None,
    }
}

#[cfg(any(unix, windows))]
#[test]
fn managed_memory_rejects_linked_files_and_directories_before_adoption() {
    for directory_link in [false, true] {
        let root = TempDir::new().expect("owned fixture");
        let outside = root.path().join("outside");
        let project = root.path().join("project");
        std::fs::create_dir_all(&outside).expect("outside fixture");
        std::fs::create_dir_all(&project).expect("project fixture");
        let target = outside.join("RULES.md");
        std::fs::write(&target, "This is not managed memory.").expect("outside contents");
        let managed_dir = project.join(".zuno");
        let managed_file = managed_dir.join("RULES.md");
        let (source, link) = if directory_link {
            (outside.as_path(), managed_dir.as_path())
        } else {
            std::fs::create_dir_all(&managed_dir).expect("managed directory");
            (target.as_path(), managed_file.as_path())
        };
        #[cfg(unix)]
        std::os::unix::fs::symlink(source, link).expect("fixture link");
        #[cfg(windows)]
        if directory_link {
            std::os::windows::fs::symlink_dir(source, link).expect("fixture directory link");
        } else {
            std::os::windows::fs::symlink_file(source, link).expect("fixture file link");
        }
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("database"));
        migration::apply(&mut pool.get().expect("connection")).expect("migration");
        let service = MemoryService::new(
            pool.clone(),
            ScopePaths::at(root.path().join("global.md"), managed_file),
            ScopeLimits::default(),
            PromotionPolicy::Automatic,
        );
        assert!(
            service.reconcile().is_err(),
            "links must not silently import outside data"
        );
        assert!(service.snapshot(Scope::Project).is_err());
        assert!(service.propose(proposal("A bounded note.")).is_err());
        assert_eq!(
            std::fs::read_to_string(&target).expect("outside unchanged"),
            "This is not managed memory."
        );
        assert_eq!(
            pool.get()
                .expect("connection")
                .query_row(
                    "SELECT count(*) FROM resident_memory_document WHERE scope='project'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("no imported content"),
            0
        );
    }
}

#[test]
fn unchanged_writes_and_reconciliation_preserve_revision_and_notifications() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Observer(AtomicUsize);
    impl zuno_memory::MemoryObserver for Observer {
        fn changed(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let (_directory, _pool, service) = fixture();
    let observer = Arc::new(Observer(AtomicUsize::new(0)));
    let service = service.with_observer(observer.clone());
    let first = service.propose(proposal("Use cargo test.")).expect("first");
    service.apply(first.id()).expect("apply");
    let revision = service.snapshot(Scope::Project).expect("snapshot").revision;
    let duplicate = service
        .propose(proposal("Use cargo test."))
        .expect("duplicate proposal");
    service.apply(duplicate.id()).expect("idempotent content");
    assert_eq!(
        service.snapshot(Scope::Project).expect("snapshot").revision,
        revision
    );
    let changes = observer.0.load(Ordering::SeqCst);
    service.reconcile().expect("no-op recovery");
    service.reconcile().expect("repeated no-op recovery");
    assert_eq!(observer.0.load(Ordering::SeqCst), changes);
}

#[test]
fn an_uncertain_legacy_file_requires_explicit_import_and_can_then_be_cleared() {
    let (directory, pool, service) = fixture();
    let path = directory.path().join("project.md");
    std::fs::write(&path, "Externally inspected rule.").expect("third state");
    pool.get()
        .expect("connection")
        .execute(
            "INSERT INTO memory_candidate
         (id,target,target_path,action,content,reason,confidence,source_kind,status,
          before_entries,after_entries,time_created,time_updated)
         VALUES ('legacy','project',?1,'add','Originally proposed.','interrupted',9000,
            'user','applying','[]','[\"Originally proposed.\"]',1,1)",
            [path.to_string_lossy().as_ref()],
        )
        .expect("legacy candidate");
    service.reconcile().expect("classify without adopting");
    assert_eq!(
        service
            .candidate("legacy")
            .expect("candidate")
            .projection
            .status,
        MemoryCandidateStatus::Uncertain
    );
    assert_eq!(
        service
            .snapshot(Scope::Project)
            .expect("quarantine")
            .revision,
        0
    );
    assert!(service.entries().expect("prompt entries").is_empty());
    let imported = service
        .import_projection(MemoryScope::Project)
        .expect("explicit import");
    assert!(imported.content.contains("Externally inspected rule."));
    std::fs::write(&path, "").expect("explicitly empty projection");
    let cleared = service
        .import_projection(MemoryScope::Project)
        .expect("explicit clear");
    assert_eq!(cleared.revision, imported.revision + 1);
    assert!(cleared.content.is_empty());
}

#[test]
fn accepted_memory_survives_missing_projection_and_repairs_from_the_revision() {
    let (directory, pool, service) = fixture();
    let candidate = service
        .propose(proposal("Run verified migration checks."))
        .expect("proposal");
    let applied = service.apply(candidate.id()).expect("apply");
    assert_eq!(applied.projection.status, MemoryCandidateStatus::Applied);
    let before = service.snapshot(Scope::Project).expect("snapshot");
    std::fs::remove_file(directory.path().join("project.md")).expect("lose derived file");
    let restarted = MemoryService::new(
        pool,
        service.paths().unwrap().clone(),
        ScopeLimits::default(),
        PromotionPolicy::Review,
    );
    assert_eq!(
        restarted
            .snapshot(Scope::Project)
            .expect("durable snapshot")
            .content,
        before.content
    );
    restarted.reconcile().expect("repair projection");
    let repaired = restarted
        .snapshot(Scope::Project)
        .expect("repaired snapshot");
    assert_eq!(repaired.revision, before.revision);
    assert_eq!(repaired.projected_revision, repaired.revision);
    assert!(directory.path().join("project.md").is_file());
}

#[test]
fn a_projection_failure_does_not_lose_the_committed_candidate() {
    let (directory, _pool, service) = fixture();
    let candidate = service
        .propose(proposal("Preserve durable accepted facts."))
        .expect("proposal");
    let path = directory.path().join("project.md");
    std::fs::create_dir(&path).expect("block projection");
    let applied = service
        .apply(candidate.id())
        .expect("authority commits independently");
    assert_eq!(applied.projection.status, MemoryCandidateStatus::Applied);
    let pending = service
        .snapshot(Scope::Project)
        .expect("pending projection");
    assert!(pending.projection_error.is_some());
    assert!(pending.content.contains("Preserve durable accepted facts."));
    std::fs::remove_dir(&path).expect("remove projection obstruction");
    service.reconcile().expect("repair without replaying apply");
    let ready = service.snapshot(Scope::Project).expect("ready projection");
    assert_eq!(ready.revision, pending.revision);
    assert_eq!(ready.projected_revision, ready.revision);
    assert!(ready.projection_error.is_none());
}

#[test]
fn external_projection_edits_are_preserved_and_reported() {
    let (directory, _pool, service) = fixture();
    let first = service
        .propose(proposal("First accepted fact."))
        .expect("proposal");
    service.apply(first.id()).expect("first apply");
    let path = directory.path().join("project.md");
    std::fs::write(&path, "manually edited projection").expect("external edit");
    let second = service
        .propose(proposal("Second accepted fact."))
        .expect("second proposal");
    service.apply(second.id()).expect("second authority commit");
    assert_eq!(
        std::fs::read_to_string(path).expect("external bytes"),
        "manually edited projection"
    );
    let snapshot = service.snapshot(Scope::Project).expect("authority");
    assert!(snapshot.content.contains("First accepted fact."));
    assert!(snapshot.content.contains("Second accepted fact."));
    assert!(snapshot.projection_error.is_some());
}

#[test]
fn concurrent_candidate_commits_never_lose_an_accepted_entry() {
    let (_directory, _pool, service) = fixture();
    let first = service
        .propose(proposal("First independent fact."))
        .expect("first");
    let second = service
        .propose(proposal("Second independent fact."))
        .expect("second");
    let barrier = Arc::new(Barrier::new(2));
    let workers = [first, second]
        .into_iter()
        .map(|candidate| {
            let service = service.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                service.apply(candidate.id()).is_ok()
            })
        })
        .collect::<Vec<_>>();
    let accepted = workers
        .into_iter()
        .map(|worker| worker.join().expect("writer"))
        .filter(|accepted| *accepted)
        .count();
    assert!(accepted >= 1);
    assert_eq!(
        service
            .entries()
            .expect("authoritative entries")
            .iter()
            .filter(|entry| entry.scope == MemoryScope::Project)
            .count(),
        accepted,
    );
}

#[test]
fn undo_advances_the_same_document_and_a_later_snapshot_observes_it() {
    let (_directory, _pool, service) = fixture();
    let initial = service.snapshot(Scope::Project).expect("initial");
    let candidate = service
        .propose(proposal("A removable fact."))
        .expect("proposal");
    service.apply(candidate.id()).expect("apply");
    let applied = service.snapshot(Scope::Project).expect("applied");
    assert!(applied.revision > initial.revision);
    service.undo(candidate.id()).expect("undo");
    let undone = service.snapshot(Scope::Project).expect("undone");
    assert!(undone.revision > applied.revision);
    assert_eq!(undone.content, initial.content);
    assert_eq!(
        service
            .candidate(candidate.id())
            .expect("candidate")
            .projection
            .status,
        MemoryCandidateStatus::Undone
    );
}
