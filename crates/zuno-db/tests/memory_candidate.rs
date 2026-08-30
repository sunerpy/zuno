use std::sync::Arc;

use zuno_db::memory_candidate::{MemoryCandidateStore, NewMemoryCandidate};
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;
use zuno_types::{MemoryAction, MemoryCandidateStatus, MemoryScope, MemorySource};

const SESSION_ID: &str = "ses_memory";
const GLOBAL_PATH: &str = "/memory/MEMORY.md";
const PROJECT_PATH: &str = "/workspace/.zuno/RULES.md";

fn initialized() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("open database"));
    {
        let mut connection = pool.get().expect("database connection");
        migration::apply(&mut connection).expect("apply schema");
        connection
            .execute(
                "INSERT INTO project \
                 (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project', '/workspace', 1, 1, '[]')",
                [],
            )
            .expect("create project");
    }
    pool.transaction(|transaction| {
        session::create(
            transaction,
            &session::SessionCreate::new(
                SESSION_ID,
                "memory",
                "project",
                "/workspace",
                "/workspace",
                "Memory",
                "zuno",
            )
            .at(1),
        )
        .map(|_| ())
    })
    .expect("create session");
    pool
}

fn candidate(id: &str) -> NewMemoryCandidate {
    NewMemoryCandidate {
        id: id.to_owned(),
        target: MemoryScope::Project,
        target_path: PROJECT_PATH.to_owned(),
        action: MemoryAction::Add,
        content: Some("run cargo test".to_owned()),
        old_text: None,
        reason: "verified repository gate".to_owned(),
        confidence: 9_500,
        source: MemorySource::Reflection,
        source_session_id: Some(SESSION_ID.to_owned()),
        source_message_id: Some("msg_memory".to_owned()),
        fingerprint: Some(format!("fingerprint-{id}")),
        time_created: 10,
    }
}

#[test]
fn reflection_candidate_source_and_fingerprint_are_idempotent() {
    let store = MemoryCandidateStore::new(initialized());
    let first = store
        .create_or_get(candidate("mem_first"))
        .expect("insert first candidate");
    assert!(first.inserted);

    let mut replay = candidate("mem_replay");
    replay.fingerprint = first.record.fingerprint.clone();
    let replay = store.create_or_get(replay).expect("replay candidate");

    assert!(!replay.inserted);
    assert_eq!(replay.record.id(), "mem_first");
}

#[test]
fn candidate_lifecycle_persists_snapshots_and_an_explicit_undo_state() {
    let store = MemoryCandidateStore::new(initialized());
    let created = store.create(candidate("mem_1")).expect("create candidate");
    assert_eq!(created.projection.status, MemoryCandidateStatus::Pending);
    assert_eq!(created.projection.confidence, 9_500);

    let applying = store
        .begin_apply(
            created.id(),
            &["existing".to_owned()],
            &["existing".to_owned(), "run cargo test".to_owned()],
            20,
        )
        .expect("begin apply");
    assert_eq!(applying.projection.status, MemoryCandidateStatus::Applying);
    assert_eq!(
        applying.before_entries.as_deref(),
        Some(["existing".to_owned()].as_slice())
    );

    let applied = store
        .set_status(created.id(), MemoryCandidateStatus::Applied, None, 30)
        .expect("finish apply");
    assert_eq!(applied.projection.status, MemoryCandidateStatus::Applied);
    assert_eq!(applied.time_applied, Some(30));

    let undoing = store.begin_undo(created.id(), 40).expect("begin undo");
    assert_eq!(undoing.projection.status, MemoryCandidateStatus::Undoing);
    let undone = store
        .set_status(created.id(), MemoryCandidateStatus::Undone, None, 50)
        .expect("finish undo");
    assert_eq!(undone.projection.status, MemoryCandidateStatus::Undone);
    assert_eq!(undone.time_applied, Some(30));
}

#[test]
fn path_projection_lists_only_owned_candidates_and_inflight_work() {
    let store = MemoryCandidateStore::new(initialized());
    store
        .create(candidate("mem_project"))
        .expect("project candidate");
    let mut global = candidate("mem_global");
    global.target = MemoryScope::Global;
    global.target_path = GLOBAL_PATH.to_owned();
    store.create(global).expect("global candidate");

    store
        .begin_apply("mem_project", &[], &["run cargo test".to_owned()], 20)
        .expect("begin project apply");
    store
        .set_status("mem_global", MemoryCandidateStatus::Rejected, None, 20)
        .expect("reject global candidate");

    let listed = store
        .list_for_paths(GLOBAL_PATH, PROJECT_PATH)
        .expect("list candidates");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id(), "mem_project");

    let inflight = store
        .list_inflight_for_paths(GLOBAL_PATH, PROJECT_PATH)
        .expect("list in-flight candidates");
    assert_eq!(inflight.len(), 1);
    assert_eq!(inflight[0].id(), "mem_project");
}

#[test]
fn deleting_the_source_session_retains_the_audit_record_without_a_dangling_owner() {
    let pool = initialized();
    let store = MemoryCandidateStore::new(Arc::clone(&pool));
    store.create(candidate("mem_1")).expect("create candidate");

    pool.transaction(|transaction| session::remove(transaction, SESSION_ID).map(|_| ()))
        .expect("delete source session");

    let retained = store.get("mem_1").expect("retained candidate");
    assert_eq!(retained.projection.source_session_id, None);
    assert_eq!(
        retained.projection.source_message_id.as_deref(),
        Some("msg_memory")
    );
}
