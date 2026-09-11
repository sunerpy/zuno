use super::*;
use zuno_config::ResolvedLearningConfig;
use zuno_db::learning_job::{LearningJobStore, NewLearningJob};
use zuno_db::{Pool, migration};
use zuno_paths::DbLocation;

fn fixture() -> (Arc<Pool>, LearningIngestion, LearningScheduler) {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).unwrap());
    {
        let mut connection = pool.get().unwrap();
        migration::apply(&mut connection).unwrap();
        connection.execute_batch(r#"
            INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
            VALUES('p','/work',1,1,'[]');
            INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
            VALUES('s','p','s','/work','learning command','test',1,10);
            INSERT INTO message(id,session_id,time_created,time_updated,data)
            VALUES('u','s',1,1,'{"role":"user"}'),
                  ('a','s',2,10,'{"role":"assistant","finish":"stop","time":{"completed":10}}');
            INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
            VALUES('up','u','s',1,1,
              '{"type":"text","text":"Correction: this source text must not appear in command status."}');
        "#).unwrap();
    }
    (
        pool.clone(),
        LearningIngestion::new(pool.clone()),
        LearningScheduler::new(pool, ResolvedLearningConfig::default())
            .with_extractor_version(zuno_learning::LEARNING_EXTRACTOR_VERSION),
    )
}

#[test]
fn reprocess_command_admits_one_background_job_and_no_foreground_input() {
    let (pool, ingestion, scheduler) = fixture();
    let first = reprocess(&ingestion, &scheduler, "p", "a", &str::to_owned).unwrap();
    assert!(runnable(&first));
    let first = admission_value(first, &str::to_owned);
    assert_eq!(first["admission"], "queued");
    assert_eq!(first["attempt"], 0);
    assert!(!first.to_string().contains("source text"));
    let second = admission_value(
        reprocess(&ingestion, &scheduler, "p", "a", &str::to_owned).unwrap(),
        &str::to_owned,
    );
    assert_eq!(second["admission"], "existing");
    assert_eq!(first["jobID"], second["jobID"]);
    assert_eq!(
        LearningJobStore::new(pool.clone())
            .list_for_project("p", 10)
            .unwrap()
            .len(),
        1
    );
    let inputs: i64 = pool
        .get()
        .unwrap()
        .query_row("SELECT count(*) FROM session_input", [], |row| row.get(0))
        .unwrap();
    assert_eq!(inputs, 0);
}

#[test]
fn repair_preview_is_read_only_and_apply_remains_bounded_and_idempotent() {
    let (pool, ingestion, scheduler) = fixture();
    let jobs = LearningJobStore::new(pool);
    jobs.enqueue(NewLearningJob::extraction(
        "old",
        "p",
        "s",
        "a",
        "legacy",
        json!({"transcript":"legacy source"}),
        10,
    ))
    .unwrap();
    let before = jobs.get("old").unwrap();
    let preview = repair_history(
        &ingestion,
        &scheduler,
        "p",
        parse_dry_run("--dry-run").unwrap(),
        &str::to_owned,
    )
    .unwrap();
    assert!(preview.dry_run);
    assert_eq!(preview.would_queue, 1);
    assert_eq!(jobs.get("old").unwrap(), before);
    assert_eq!(jobs.list_for_project("p", 10).unwrap().len(), 1);
    let applied = repair_history(&ingestion, &scheduler, "p", false, &str::to_owned).unwrap();
    assert_eq!(applied.queued, 1);
    assert_eq!(
        repair_history(&ingestion, &scheduler, "p", false, &str::to_owned)
            .unwrap()
            .examined,
        0
    );
}

#[test]
fn command_arguments_and_project_scope_cannot_select_unrelated_sources() {
    let (pool, ingestion, scheduler) = fixture();
    for value in ["", "a b", "a\nb", "a\0b"] {
        assert!(reprocess(&ingestion, &scheduler, "p", value, &str::to_owned).is_err());
    }
    assert!(
        reprocess(
            &ingestion,
            &scheduler,
            "another-project",
            "a",
            &str::to_owned
        )
        .is_err()
    );
    assert!(parse_dry_run("--dry-run --force").is_err());
    assert_eq!(
        LearningJobStore::new(pool)
            .list_for_project("p", 10)
            .unwrap()
            .len(),
        0
    );
}
