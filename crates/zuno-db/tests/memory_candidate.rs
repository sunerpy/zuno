use std::sync::{Arc, Barrier};

use zuno_db::memory_candidate::{MemoryCandidateStore, NewMemoryCandidate};
use zuno_db::{Pool, migration, session};
use zuno_error::DbError;
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

#[test]
fn a_stale_reject_cannot_overwrite_an_in_flight_apply_and_a_settled_candidate_stays_settled() {
    let store = MemoryCandidateStore::new(initialized());
    let created = store
        .create(candidate("mem_race"))
        .expect("create candidate");
    // Reviewer A read `pending` and started applying.
    store
        .begin_apply(created.id(), &[], &["run cargo test".to_owned()], 20)
        .expect("begin apply");
    // Reviewer B read the same `pending` row moments earlier and now rejects it.
    let error = store
        .set_status(created.id(), MemoryCandidateStatus::Rejected, None, 21)
        .expect_err("a stale reject must not overwrite an in-flight apply");
    assert!(
        matches!(
            error,
            DbError::Conflict { ref table, ref id, .. }
                if table == "memory_candidate" && id == "mem_race"
        ),
        "{error:?}"
    );
    assert_eq!(
        store.get("mem_race").expect("read").projection.status,
        MemoryCandidateStatus::Applying
    );

    // The in-flight writer still finishes exactly once.
    let applied = store
        .set_status(created.id(), MemoryCandidateStatus::Applied, None, 30)
        .expect("finish apply");
    assert_eq!(applied.projection.status, MemoryCandidateStatus::Applied);

    // A restart reconciler that read `applying` before the finish must not
    // downgrade the settled row.
    let error = store
        .set_status(
            created.id(),
            MemoryCandidateStatus::Uncertain,
            Some("reconciled after process restart without replay"),
            31,
        )
        .expect_err("a settled candidate is not rewritten");
    assert!(matches!(error, DbError::Conflict { .. }), "{error:?}");
    let record = store.get("mem_race").expect("read");
    assert_eq!(record.projection.status, MemoryCandidateStatus::Applied);
    assert_eq!(record.projection.error, None);
    assert_eq!(record.time_applied, Some(30));
    assert_eq!(record.projection.time_updated, 30);
}

#[test]
fn two_racing_settlements_of_one_in_flight_candidate_commit_exactly_one() {
    let pool = initialized();
    let store = MemoryCandidateStore::new(Arc::clone(&pool));
    store
        .create(candidate("mem_race"))
        .expect("create candidate");
    store
        .begin_apply("mem_race", &[], &["run cargo test".to_owned()], 20)
        .expect("begin apply");

    let barrier = Arc::new(Barrier::new(2));
    let settle = |status: MemoryCandidateStatus, error: Option<&'static str>| {
        let store = MemoryCandidateStore::new(Arc::clone(&pool));
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            store.set_status("mem_race", status, error, 30)
        })
    };
    let applied = settle(MemoryCandidateStatus::Applied, None);
    let failed = settle(MemoryCandidateStatus::Failed, Some("resident file changed"));
    let outcomes = [
        applied.join().expect("applied thread"),
        failed.join().expect("failed thread"),
    ];

    let winners: Vec<_> = outcomes.iter().filter_map(|o| o.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "{outcomes:?}");
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Err(DbError::Conflict { .. })))
            .count(),
        1,
        "{outcomes:?}"
    );
    let record = store.get("mem_race").expect("read");
    assert_eq!(record.projection.status, winners[0].projection.status);
    assert_eq!(record.projection.error, winners[0].projection.error);
}

#[test]
fn every_legal_settlement_path_still_succeeds_under_the_guard() {
    let store = MemoryCandidateStore::new(initialized());
    // pending -> failed (preview failed) -> failed (failed again) -> rejected
    store.create(candidate("mem_reject")).expect("create");
    store
        .set_status(
            "mem_reject",
            MemoryCandidateStatus::Failed,
            Some("preview"),
            20,
        )
        .expect("pending may fail");
    store
        .set_status(
            "mem_reject",
            MemoryCandidateStatus::Failed,
            Some("again"),
            21,
        )
        .expect("a failed candidate may fail again");
    store
        .set_status("mem_reject", MemoryCandidateStatus::Rejected, None, 22)
        .expect("a failed candidate may be rejected");

    // pending -> applying -> failed -> applying -> applied -> undoing -> uncertain
    store.create(candidate("mem_apply")).expect("create");
    store
        .begin_apply("mem_apply", &[], &["a".to_owned()], 30)
        .expect("begin apply");
    store
        .set_status(
            "mem_apply",
            MemoryCandidateStatus::Failed,
            Some("write"),
            31,
        )
        .expect("applying may fail");
    store
        .begin_apply("mem_apply", &[], &["a".to_owned()], 32)
        .expect("failed may be retried");
    store
        .set_status("mem_apply", MemoryCandidateStatus::Applied, None, 33)
        .expect("applying may finish");
    store
        .begin_undo("mem_apply", 34)
        .expect("applied may be undone");
    let record = store
        .set_status(
            "mem_apply",
            MemoryCandidateStatus::Uncertain,
            Some("lost"),
            35,
        )
        .expect("undoing may become uncertain");
    assert_eq!(record.projection.status, MemoryCandidateStatus::Uncertain);

    // A missing row is reported as absent, not as a lifecycle conflict.
    let error = store
        .set_status("mem_missing", MemoryCandidateStatus::Rejected, None, 40)
        .expect_err("missing candidate");
    assert!(matches!(error, DbError::NotFound { .. }), "{error:?}");
}
