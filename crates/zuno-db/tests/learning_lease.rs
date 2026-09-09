use serde_json::json;
use std::sync::Arc;
use zuno_db::experience::ExperienceStore;
use zuno_db::learning_job::{LearningJobStatus, LearningJobStore, NewLearningJob};
use zuno_db::{Pool, migration};
use zuno_paths::DbLocation;

fn fixture() -> (Arc<Pool>, LearningJobStore) {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("database"));
    {
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("schema");
        connection
            .execute_batch(
                r#"
            INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
            VALUES ('p', '/workspace', 1, 1, '[]');
            INSERT INTO session
              (id, project_id, slug, directory, title, version, time_created, time_updated)
            VALUES ('s', 'p', 'session', '/workspace', 'lease test', 'test', 1, 1);
            INSERT INTO message (id, session_id, time_created, time_updated, data)
            VALUES ('m', 's', 1, 1, '{"role":"assistant"}');
        "#,
            )
            .expect("source");
    }
    let jobs = LearningJobStore::new(Arc::clone(&pool));
    jobs.enqueue(NewLearningJob::extraction(
        "job",
        "p",
        "s",
        "m",
        "v2",
        json!({"trigger":"automatic_post_turn"}),
        10,
    ))
    .expect("job");
    (pool, jobs)
}

#[test]
fn reclaiming_with_the_same_worker_id_fences_the_old_attempt() {
    let (_pool, jobs) = fixture();
    let old = jobs
        .claim("job", "same-worker", 11, 20)
        .expect("claim")
        .expect("job");
    let old_lease = old.lease().expect("lease");
    jobs.reconcile_expired(20).expect("expired");
    let new = jobs
        .claim("job", "same-worker", 21, 40)
        .expect("reclaim")
        .expect("job");
    let new_lease = new.lease().expect("new lease");
    assert_ne!(old_lease.token, new_lease.token);
    assert!(
        !jobs
            .heartbeat("job", &old_lease, 22, 50)
            .expect("stale heartbeat")
    );
    assert!(
        jobs.settle(
            "job",
            &old_lease,
            LearningJobStatus::Completed,
            None,
            None,
            22
        )
        .is_err()
    );
    jobs.settle(
        "job",
        &new_lease,
        LearningJobStatus::Completed,
        Some(&json!({"new":true})),
        None,
        23,
    )
    .expect("current attempt settles");
    assert_eq!(
        jobs.get("job").expect("job").result,
        Some(json!({"new":true}))
    );
}

#[test]
fn expired_leases_cannot_record_experiences_before_reconciliation() {
    let (pool, jobs) = fixture();
    let lease = jobs
        .claim("job", "worker", 11, 20)
        .expect("claim")
        .expect("job")
        .lease()
        .expect("lease");
    assert!(
        ExperienceStore::new(pool)
            .record_extraction("job", &lease, &[], 21)
            .is_err()
    );
}

#[test]
fn disabling_generation_stops_heartbeat_and_skips_the_released_attempt() {
    let (pool, jobs) = fixture();
    let lease = jobs
        .claim("job", "worker", 11, 40)
        .expect("claim")
        .expect("job")
        .lease()
        .expect("lease");
    pool.get().expect("connection").execute_batch(
        "INSERT INTO session_memory_policy
          (session_id, use_memories, generation, reason, source, revision, time_created, time_updated)
         VALUES ('s', 1, 'disabled', 'user disabled learning', 'user', 1, 12, 12)",
    ).expect("disable");
    assert!(
        !jobs
            .heartbeat("job", &lease, 13, 50)
            .expect("policy heartbeat")
    );
    jobs.retry("job", &lease, "cancelled after policy change", 20, 13)
        .expect("release");
    assert_eq!(
        jobs.get("job").expect("job").status,
        LearningJobStatus::Skipped
    );
}
