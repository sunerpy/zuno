use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use zuno_db::learning_job::{LearningJobStore, NewLearningJob};
use zuno_db::learning_source::{LearningSource, LearningSourceKind, LearningSourceStore};
use zuno_db::{Pool, migration};
use zuno_learning::{ExperienceService, LearningExtraction};
use zuno_memory::{MemoryService, PromotionPolicy, ScopeLimits, ScopePaths};
use zuno_paths::DbLocation;

fn fixture() -> (TempDir, Arc<Pool>, ExperienceService, Vec<LearningSource>) {
    let directory = TempDir::new().expect("memory directory");
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("database"));
    {
        let mut connection = pool.get().expect("connection");
        migration::apply(&mut connection).expect("migration");
        connection.execute_batch(r#"
            INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
            VALUES ('project', '/workspace', 1, 1, '[]');
            INSERT INTO session
              (id, project_id, slug, directory, title, version, time_created, time_updated)
            VALUES ('session', 'project', 'session', '/workspace', 'source test', 'test', 1, 1);
            INSERT INTO message (id, session_id, time_created, time_updated, data)
            VALUES ('user', 'session', 1, 1, '{"role":"user"}'),
                   ('assistant', 'session', 2, 2, '{"role":"assistant"}');
            INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
            VALUES ('user-part', 'user', 'session', 1, 1,
                    '{"type":"text","text":"Verify this change before publishing."}'),
                   ('tool-part', 'assistant', 'session', 2, 2,
                    '{"type":"tool","callID":"verification-call","tool":"shell","state":{"status":"completed","input":{"command":"cargo test"},"output":"cargo test passed; token=example-secret-value"}}');
            INSERT INTO verification_receipt
              (id, session_id, tool_call_id, tool_id, summary, exit_code,
               exit_authority, outcome, time_created)
            VALUES ('receipt', 'session', 'verification-call', 'shell',
                    'cargo test', 0, 'authoritative', 'passed', 2);
        "#).expect("durable source fixture");
    }
    let window = LearningSourceStore::new(Arc::clone(&pool))
        .for_turn("session", "assistant", &|text| {
            text.replace("example-secret-value", "[REDACTED]")
        })
        .expect("capture sources");
    let memory = Arc::new(MemoryService::new(
        Arc::clone(&pool),
        ScopePaths::at(
            directory.path().join("global.md"),
            directory.path().join("project.md"),
        ),
        ScopeLimits::default(),
        PromotionPolicy::Review,
    ));
    let experiences = ExperienceService::new(Arc::clone(&pool), Some(memory));
    (directory, pool, experiences, window.sources)
}

fn extraction(reference: &str, excerpt: &str) -> LearningExtraction {
    serde_json::from_value(json!({
        "experiences": [{
            "kind": "procedure", "title": "Verified test command",
            "summary": "The project test command completed successfully.",
            "resolution": "Run cargo test before publishing.",
            "confidence": 0.99,
            "evidence": [{"kind": "tool", "source_id": reference, "excerpt": excerpt}]
        }],
        "memories": [{
            "experience_ordinal": 0, "scope": "project", "action": "add",
            "content": "Run cargo test before publishing.",
            "old_text": null, "reason": "A recorded verification supports this procedure.",
            "confidence": 0.99
        }]
    }))
    .expect("model output")
}

fn claim(pool: &Arc<Pool>, sources: &[LearningSource]) -> zuno_db::learning_job::LearningLease {
    let jobs = LearningJobStore::new(Arc::clone(pool));
    jobs.enqueue(NewLearningJob::extraction(
        "job",
        "project",
        "session",
        "assistant",
        "extractor-v2",
        json!({"request": {"sources": sources}}),
        10,
    ))
    .expect("enqueue");
    jobs.claim_due("worker", 11, 1_000)
        .expect("claim")
        .expect("job")
        .lease()
        .expect("lease")
}

#[test]
fn verified_source_and_exact_excerpt_can_support_automatic_project_memory() {
    let (_directory, pool, experiences, sources) = fixture();
    let tool = sources
        .iter()
        .find(|source| source.kind == LearningSourceKind::Tool)
        .expect("tool");
    assert!(tool.proves_success);
    assert!(!tool.content.contains("example-secret-value"));
    assert!(tool.content.contains("[REDACTED]"));
    assert_ne!(tool.source_digest, tool.content_digest);
    let lease = claim(&pool, &sources);
    let result = experiences
        .persist_extraction(
            "job",
            &lease,
            extraction(&tool.reference_id, "cargo test passed"),
            20,
        )
        .expect("verified extraction");
    assert!(result.experiences[0].verified_sources());
    assert!(result.memory_promotions[0].automatically_applied);
}

#[test]
fn fabricated_addresses_and_quotes_remain_unverified_and_never_auto_apply() {
    for (reference, excerpt) in [
        ("part:missing", "cargo test passed"),
        ("part:tool-part", "a command that never executed passed"),
    ] {
        let (_directory, pool, experiences, sources) = fixture();
        let lease = claim(&pool, &sources);
        let result = experiences
            .persist_extraction("job", &lease, extraction(reference, excerpt), 20)
            .expect("retain unverified observation");
        assert!(!result.experiences[0].verified_sources());
        assert!(!result.memory_promotions[0].automatically_applied);
    }
}

#[test]
fn source_drift_and_revoked_success_proofs_invalidate_automatic_promotion() {
    for mutation in [
        "UPDATE part SET data = json_set(data, '$.state.output', 'different output') WHERE id = 'tool-part'",
        "UPDATE verification_receipt SET outcome = 'failed' WHERE id = 'receipt'",
        "UPDATE part SET data = json_set(data, '$.state.input.command', 'a different command') WHERE id = 'tool-part'",
        "UPDATE verification_receipt SET exit_code = 2 WHERE id = 'receipt'",
    ] {
        let (_directory, pool, experiences, sources) = fixture();
        let lease = claim(&pool, &sources);
        pool.get()
            .expect("connection")
            .execute_batch(mutation)
            .expect("source changed");
        let result = experiences
            .persist_extraction(
                "job",
                &lease,
                extraction("part:tool-part", "cargo test passed"),
                20,
            )
            .expect("record observation");
        assert!(!result.experiences[0].verified_sources());
        assert!(!result.memory_promotions[0].automatically_applied);
    }
}

#[test]
fn revoked_lease_between_extraction_and_promotion_cannot_commit_memory() {
    let (_directory, pool, experiences, sources) = fixture();
    let lease = claim(&pool, &sources);
    // The proposal transaction is the last boundary before automatic authority commit.
    pool.get()
        .expect("connection")
        .execute_batch(
            "CREATE TRIGGER revoke_learning_attempt AFTER INSERT ON memory_candidate
         BEGIN UPDATE learning_job SET lease_token='new-attempt' WHERE id='job'; END;",
        )
        .expect("concurrent authority change");
    assert!(
        experiences
            .persist_extraction(
                "job",
                &lease,
                extraction("part:tool-part", "cargo test passed"),
                20
            )
            .is_err()
    );
    let applied = pool
        .get()
        .expect("connection")
        .query_row(
            "SELECT count(*) FROM memory_candidate WHERE status='applied'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("applied candidates");
    assert_eq!(applied, 0);
}

#[test]
fn session_scope_includes_feedback_with_a_revision_that_cannot_be_reused_after_edit() {
    let (_directory, pool, _experiences, _sources) = fixture();
    pool.get().expect("connection").execute_batch(
        "INSERT INTO message_feedback(message_id,session_id,rating,note,revision,time_created,time_updated)
         VALUES('assistant','session',1,'Keep the verified command.',1,3,3);"
    ).expect("feedback");
    let store = LearningSourceStore::new(pool.clone());
    let window = store
        .for_session("session", "assistant", &str::to_owned)
        .expect("session capture");
    let feedback = window
        .sources
        .iter()
        .find(|source| source.kind == LearningSourceKind::Feedback)
        .expect("feedback source");
    assert!(
        store
            .source_is_current("session", feedback)
            .expect("valid revision")
    );
    pool.get()
        .expect("connection")
        .execute_batch(
            "UPDATE message_feedback SET note='Withdrawn',revision=2 WHERE message_id='assistant';",
        )
        .expect("feedback changed");
    assert!(
        !store
            .source_is_current("session", feedback)
            .expect("old revision invalid")
    );
}

#[test]
fn revised_feedback_is_a_new_manual_input_and_fences_an_older_running_extraction() {
    use zuno_learning::{LearningIngestion, LearningScheduleOutcome, LearningScheduler};
    let (_directory, pool, _experiences, _sources) = fixture();
    let scheduler = LearningScheduler::new(
        pool.clone(),
        zuno_config::ResolvedLearningConfig {
            post_turn_idle_delay_ms: 0,
            ..Default::default()
        },
    );
    let ingestion = LearningIngestion::new(pool.clone());
    let (request, signals) = ingestion
        .request("project", "session", "assistant", false, &str::to_owned)
        .expect("input");
    let LearningScheduleOutcome::Queued(automatic) = scheduler
        .schedule_post_turn(request.clone(), signals, 10)
        .expect("automatic")
    else {
        panic!("queued");
    };
    let running = scheduler
        .claim(&automatic.id, "worker", 11, 100)
        .expect("claim")
        .expect("job");
    let old_lease = running.lease().expect("lease");
    let LearningScheduleOutcome::Queued(first) = scheduler
        .schedule_manual_reflection(request, 12)
        .expect("explicit input")
    else {
        panic!("manual queues its input");
    };
    assert!(
        !scheduler
            .heartbeat(&automatic.id, &old_lease, 13, 100)
            .expect("older owner fenced")
    );
    let first = scheduler
        .claim(&first.id, "manual", 13, 100)
        .expect("claim")
        .expect("job");
    scheduler
        .complete(&first.id, &first.lease().expect("lease"), &json!({}), 14)
        .expect("completed reflection");
    pool.get().expect("connection").execute_batch(
        "INSERT INTO message_feedback(message_id,session_id,rating,note,revision,time_created,time_updated)
         VALUES('assistant','session',-1,'A new correction must be reflected.',1,20,20);"
    ).expect("new feedback");
    let (updated, _) = ingestion
        .request("project", "session", "assistant", false, &str::to_owned)
        .expect("new input");
    let LearningScheduleOutcome::Queued(second) = scheduler
        .schedule_manual_reflection(updated.clone(), 21)
        .expect("feedback reflection")
    else {
        panic!("changed evidence is a new input");
    };
    assert_ne!(first.id, second.id);
    let LearningScheduleOutcome::Existing(repeated) = scheduler
        .schedule_manual_reflection(updated, 22)
        .expect("repeated input")
    else {
        panic!("same input deduplicates");
    };
    assert_eq!(repeated.id, second.id);
}

#[test]
fn a_retry_cannot_borrow_the_previous_attempts_verified_evidence_for_new_memory() {
    for changed_summary in [false, true] {
        let (_directory, pool, experiences, sources) = fixture();
        let lease = claim(&pool, &sources);
        pool.get().expect("connection").execute_batch(
            "CREATE TRIGGER interrupt_extraction_settlement BEFORE UPDATE OF status ON learning_job
             WHEN new.status='completed' BEGIN SELECT RAISE(ABORT,'interrupted settlement'); END;"
        ).expect("interrupt after experience persistence");
        let mut first = extraction("part:tool-part", "cargo test passed");
        first.memories.clear();
        assert!(
            experiences
                .persist_extraction("job", &lease, first, 20)
                .is_err()
        );
        pool.get()
            .expect("connection")
            .execute_batch("DROP TRIGGER interrupt_extraction_settlement;")
            .expect("resume");
        let mut retried = extraction("part:missing", "cargo test passed");
        if changed_summary {
            retried.experiences[0].summary = "A changed interpretation.".to_owned();
        }
        let error = experiences
            .persist_extraction("job", &lease, retried, 21)
            .expect_err("changed retry is refused");
        match error {
            zuno_learning::LearningServiceError::Database(zuno_error::DbError::Query {
                source,
            }) => {
                assert!(
                    source
                        .to_string()
                        .contains("an extraction retry produced different"),
                    "{source}"
                );
            }
            other => panic!("unexpected retry failure: {other:?}"),
        }
        let records = experiences
            .list_for_project("project", 10)
            .expect("durable observations");
        assert_eq!(records.len(), 1);
        assert!(records[0].verified_sources());
        assert_eq!(
            pool.get()
                .expect("connection")
                .query_row(
                    "SELECT count(*) FROM memory_candidate WHERE status='applied'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .expect("applied memory"),
            0
        );
    }
}

#[test]
fn legacy_source_less_jobs_cannot_auto_apply_self_reported_confidence() {
    let (_directory, pool, experiences, _sources) = fixture();
    let lease = claim(&pool, &[]);
    let result = experiences
        .persist_extraction(
            "job",
            &lease,
            extraction("verification-call", "cargo test passed"),
            20,
        )
        .expect("legacy observation");
    assert!(!result.memory_promotions[0].automatically_applied);
}
