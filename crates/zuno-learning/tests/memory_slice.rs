use serde_json::json;
use std::sync::Arc;
use zuno_config::ResolvedLearningConfig;
use zuno_db::{
    Pool,
    learning_job::{LearningJobStore, NewLearningJob},
    migration,
};
use zuno_learning::{LearningIngestion, LearningScheduleOutcome, LearningScheduler};
use zuno_paths::DbLocation;

fn pool() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("test database"));
    let mut connection = pool.get().expect("connection");
    migration::apply(&mut connection).expect("schema");
    connection.execute_batch(r#"
        INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
        VALUES('p','/workspace',1,1,'[]');
        INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
        VALUES('s','p','s','/workspace','closed source','test',1,10);
        INSERT INTO message(id,session_id,time_created,time_updated,data)
        VALUES('u','s',1,1,'{"role":"user"}'),
              ('a','s',2,10,'{"role":"assistant","parentID":"u","finish":"stop","time":{"created":2,"completed":10}}');
        INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
        VALUES('up','u','s',1,1,'{"type":"text","text":"Correction: use the project test command."}'),
              ('ap','a','s',2,10,'{"type":"text","text":"I will use the project test command."}');
    "#).expect("closed-turn fixture");
    drop(connection);
    pool
}

fn scheduler(pool: &Arc<Pool>, delay: u64) -> LearningScheduler {
    LearningScheduler::new(
        Arc::clone(pool),
        ResolvedLearningConfig {
            post_turn_idle_delay_ms: delay,
            ..ResolvedLearningConfig::default()
        },
    )
    .with_extractor_version(zuno_learning::LEARNING_EXTRACTOR_VERSION)
}

fn schedule(pool: &Arc<Pool>, scheduler: &LearningScheduler) -> String {
    let (request, signals) = LearningIngestion::new(Arc::clone(pool))
        .request("p", "s", "a", false, &str::to_owned)
        .expect("closed source");
    let LearningScheduleOutcome::Queued(job) = scheduler
        .schedule_post_turn(request, signals, 10)
        .expect("admit")
    else {
        panic!("eligible completed turn must be durably queued");
    };
    job.id
}

#[test]
fn closed_source_can_be_claimed_and_heartbeated_while_the_next_turn_is_busy() {
    let pool = pool();
    let scheduler = scheduler(&pool, 0);
    let id = schedule(&pool, &scheduler);
    pool.get()
        .unwrap()
        .execute_batch(
            r#"
        INSERT INTO message(id,session_id,time_created,time_updated,data)
        VALUES('next-u','s',20,20,'{"role":"user"}'),
              ('next-a','s',21,21,'{"role":"assistant","parentID":"next-u"}');
        UPDATE session SET time_updated=21 WHERE id='s';
    "#,
        )
        .unwrap();
    let job = scheduler
        .claim_due_for_project_excluding("p", "worker", 21, 100, &["s".to_owned()], None)
        .expect("claim closed source")
        .expect("a later live turn must not starve a closed snapshot");
    assert_eq!(job.id, id);
    pool.get()
        .unwrap()
        .execute("UPDATE session SET time_updated=22 WHERE id='s'", [])
        .unwrap();
    assert!(
        scheduler
            .heartbeat(&id, &job.lease().unwrap(), 22, 101)
            .unwrap()
    );
    let request = &job.payload.unwrap()["request"];
    assert!(!request.to_string().contains("next-u"));
    assert!(!request.to_string().contains("next-a"));
}

#[test]
fn source_capture_rejects_unfinished_failed_and_pending_tool_turns() {
    for mutation in [
        "UPDATE message SET data=json_remove(data,'$.finish','$.time.completed') WHERE id='a'",
        "UPDATE message SET data=json_set(data,'$.finish','length') WHERE id='a'",
        "UPDATE message SET data=json_set(data,'$.error',json('{\"name\":\"APIError\"}')) WHERE id='a'",
        "INSERT INTO part(id,message_id,session_id,time_created,time_updated,data) VALUES('pending','a','s',2,2,'{\"type\":\"tool\",\"tool\":\"shell\",\"state\":{\"status\":\"running\"}}')",
    ] {
        let pool = pool();
        pool.get().unwrap().execute_batch(mutation).unwrap();
        assert!(
            LearningIngestion::new(pool)
                .request("p", "s", "a", false, &str::to_owned)
                .is_err(),
            "source capture accepted an open or failed turn: {mutation}"
        );
    }
}

#[test]
fn explicit_nonzero_idle_delay_still_withholds_a_busy_session() {
    let pool = pool();
    let scheduler = scheduler(&pool, 25);
    let id = schedule(&pool, &scheduler);
    assert!(scheduler.claim(&id, "worker", 34, 100).unwrap().is_none());
    assert!(
        scheduler
            .claim_due_for_project_excluding("p", "worker", 100, 200, &["s".to_owned()], None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn current_closed_work_precedes_an_older_legacy_job() {
    let pool = pool();
    let scheduler = scheduler(&pool, 0);
    let jobs = LearningJobStore::new(Arc::clone(&pool));
    pool.get()
        .unwrap()
        .execute(
            "INSERT INTO message(id,session_id,time_created,time_updated,data)
         VALUES('legacy-a','s',0,0,'{\"role\":\"assistant\"}')",
            [],
        )
        .unwrap();
    jobs.enqueue(NewLearningJob::extraction(
        "legacy",
        "p",
        "s",
        "legacy-a",
        "extractor-v1",
        json!({"trigger":"automatic_post_turn","request":{"transcript":"unverified old blob"}}),
        1,
    ))
    .unwrap();
    let id = schedule(&pool, &scheduler);
    let job = scheduler
        .claim_due_for_project("p", "worker", 10, 100)
        .unwrap()
        .expect("current work");
    assert_eq!(
        job.id, id,
        "legacy backlog must not delay a new completed turn"
    );
}

#[test]
fn changed_sources_invalidate_the_snapshot_without_spending_an_attempt() {
    let pool = pool();
    let scheduler = scheduler(&pool, 0);
    let id = schedule(&pool, &scheduler);
    pool.get()
        .unwrap()
        .execute(
            "UPDATE part SET data=json_set(data,'$.text','Changed source') WHERE id='up'",
            [],
        )
        .unwrap();
    assert!(
        scheduler
            .claim_due_for_project_excluding("p", "worker", 30, 100, &["s".to_owned()], None)
            .unwrap()
            .is_none()
    );
    let job = scheduler.get(&id).unwrap();
    assert_eq!(
        job.status,
        zuno_db::learning_job::LearningJobStatus::Skipped
    );
    assert_eq!(job.attempt, 0);
}

#[test]
fn disabled_generation_withholds_already_queued_work() {
    let pool = pool();
    let enabled = scheduler(&pool, 0);
    let id = schedule(&pool, &enabled);
    let disabled = LearningScheduler::new(
        pool.clone(),
        ResolvedLearningConfig {
            generate: false,
            ..ResolvedLearningConfig::default()
        },
    );
    assert!(disabled.claim(&id, "worker", 30, 100).unwrap().is_none());
    assert!(
        disabled
            .claim_due_for_project("p", "worker", 30, 100)
            .unwrap()
            .is_none()
    );
    assert_eq!(enabled.get(&id).unwrap().attempt, 0);
    assert!(matches!(
        LearningIngestion::new(pool)
            .capture_post_turn("p", "s", "a", &disabled, 31, &str::to_owned)
            .unwrap(),
        LearningScheduleOutcome::Disabled
    ));
}

#[test]
fn repeated_post_turn_capture_is_durable_and_idempotent() {
    let pool = pool();
    let scheduler = scheduler(&pool, 0);
    let ingestion = LearningIngestion::new(pool);
    let LearningScheduleOutcome::Queued(first) = ingestion
        .capture_post_turn("p", "s", "a", &scheduler, 10, &str::to_owned)
        .unwrap()
    else {
        panic!("first completion must enqueue");
    };
    let LearningScheduleOutcome::Existing(second) = ingestion
        .capture_post_turn("p", "s", "a", &scheduler, 11, &str::to_owned)
        .unwrap()
    else {
        panic!("repeat must reuse the source identity");
    };
    assert_eq!(first.id, second.id);
    assert_eq!(first.scheduled_at, 10);
    assert_eq!(first.payload, second.payload);
    assert_eq!(first.payload.unwrap()["sourceSnapshot"]["version"], 1);
}
