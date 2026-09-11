use serde_json::json;
use std::sync::Arc;
use zuno_config::ResolvedLearningConfig;
use zuno_db::{
    Pool,
    experience::ExperienceStore,
    learning_job::{LearningJobStatus, LearningJobStore, NewLearningJob},
    memory_evidence::MemoryEvidenceStore,
    migration,
};
use zuno_learning::{
    ExperienceService, ExtractionJobPayload, ExtractionRequest, LearningExtraction,
    LearningHistoryAction, LearningIngestion, LearningScheduleOutcome, LearningScheduler,
};
use zuno_paths::DbLocation;

fn fixture(address: &str) -> (Arc<Pool>, LearningScheduler, String) {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).unwrap());
    {
        let mut db = pool.get().unwrap();
        migration::apply(&mut db).unwrap();
        db.execute_batch(r#"
            INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
            VALUES('p','/workspace',1,1,'[]');
            INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
            VALUES('s','p','s','/workspace','legacy source','test',1,10);
            INSERT INTO message(id,session_id,time_created,time_updated,data)
            VALUES('u','s',1,1,'{"role":"user"}'),
                  ('a','s',2,10,'{"role":"assistant","finish":"stop","time":{"completed":10}}');
            INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
            VALUES('up','u','s',1,1,'{"type":"text","text":"Correction: prefer concise reports."}'),
                  ('ap','a','s',2,10,'{"type":"text","text":"Understood."}');
        "#).unwrap();
    }
    let jobs = LearningJobStore::new(pool.clone());
    let request = ExtractionRequest {
        project_id: "p".to_owned(),
        session_id: "s".to_owned(),
        source_message_id: "a".to_owned(),
        transcript: "old extraction input".to_owned(),
        sources: vec![],
        sources_truncated: false,
        had_tool_calls: false,
        had_artifacts: false,
        recovered_from_error: false,
        user_corrected: true,
        explicit_feedback: false,
    };
    jobs.enqueue(NewLearningJob::extraction(
        "old",
        "p",
        "s",
        "a",
        "extractor-v1",
        serde_json::to_value(ExtractionJobPayload::automatic_post_turn(request)).unwrap(),
        10,
    ))
    .unwrap();
    let lease = jobs
        .claim("old", "worker", 11, 1000)
        .unwrap()
        .unwrap()
        .lease()
        .unwrap();
    let extraction: LearningExtraction = serde_json::from_value(json!({
        "experiences":[{
            "kind":"user_correction","title":"Concise reports","summary":"Prefer concise reports.",
            "resolution":null,"confidence":0.99,
            "evidence":[{"kind":"user","source_id":address,"excerpt":"prefer concise reports."}]
        }],
        "memories":[]
    }))
    .unwrap();
    let record = ExperienceService::new(pool.clone(), None)
        .persist_extraction("old", &lease, extraction, 12)
        .unwrap()
        .experiences
        .remove(0);
    assert!(!record.verified_sources());
    let scheduler = LearningScheduler::new(pool.clone(), ResolvedLearningConfig::default())
        .with_extractor_version(zuno_learning::LEARNING_EXTRACTOR_VERSION);
    (pool, scheduler, record.projection.id)
}

#[test]
fn legacy_exact_citation_revalidates_without_paid_reprocessing_and_keeps_original_input() {
    let (pool, scheduler, id) = fixture("part:up");
    let jobs = LearningJobStore::new(pool.clone());
    let original = jobs.get("old").unwrap().payload.unwrap()["request"].clone();
    let ingestion = LearningIngestion::new(pool.clone());
    let report = ingestion
        .repair_history("p", &scheduler, 30, false, &str::to_owned)
        .unwrap();
    assert_eq!(report.examined, 1);
    assert_eq!(report.revalidated_experiences, 1);
    assert_eq!(report.queued, 0);
    assert_eq!(report.items[0].action, LearningHistoryAction::Revalidated);
    assert!(
        MemoryEvidenceStore::new(pool.clone())
            .get(&id)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        jobs.get("old").unwrap().payload.unwrap()["request"],
        original
    );
    assert_eq!(
        ingestion
            .repair_history("p", &scheduler, 31, false, &str::to_owned)
            .unwrap()
            .examined,
        0
    );
}

#[test]
fn dry_run_reports_exact_repair_without_changing_evidence_or_jobs() {
    let (pool, scheduler, id) = fixture("part:up");
    let jobs = LearningJobStore::new(pool.clone());
    let before_job = jobs.get("old").unwrap();
    let experiences = ExperienceStore::new(pool.clone());
    let before = experiences.get(&id).unwrap();
    let report = LearningIngestion::new(pool.clone())
        .repair_history("p", &scheduler, 30, true, &str::to_owned)
        .unwrap();
    assert_eq!(
        report.items[0].action,
        LearningHistoryAction::WouldRevalidate
    );
    assert_eq!(report.revalidated_experiences, 1);
    assert_eq!(jobs.get("old").unwrap(), before_job);
    assert_eq!(experiences.get(&id).unwrap(), before);
    assert!(MemoryEvidenceStore::new(pool).get(&id).unwrap().is_none());
}

#[test]
fn unavailable_legacy_citations_remain_unverified_and_reprocess_is_idempotent_and_bounded() {
    let (pool, scheduler, id) = fixture("part:missing");
    let ingestion = LearningIngestion::new(pool.clone());
    let report = ingestion
        .repair_history("p", &scheduler, 30, false, &str::to_owned)
        .unwrap();
    assert_eq!(report.revalidated_experiences, 0);
    assert_eq!(report.items[0].unverified_experiences, 1);
    assert_eq!(report.queued, 1);
    assert!(
        MemoryEvidenceStore::new(pool.clone())
            .get(&id)
            .unwrap()
            .is_none()
    );
    let LearningScheduleOutcome::Existing(job) = ingestion
        .reprocess("p", "a", &scheduler, 31, &str::to_owned)
        .unwrap()
    else {
        panic!("repair must reuse the same durable job");
    };
    let jobs = LearningJobStore::new(pool.clone());
    for attempt in 1..=3 {
        let running = jobs
            .claim(&job.id, "worker", 40 + attempt, 1000)
            .unwrap()
            .unwrap();
        assert_eq!(running.attempt, attempt as u32);
        jobs.retry(
            &job.id,
            &running.lease().unwrap(),
            "scripted transient",
            41 + attempt,
            40 + attempt,
        )
        .unwrap();
    }
    assert_eq!(jobs.get(&job.id).unwrap().status, LearningJobStatus::Failed);
    assert!(jobs.claim(&job.id, "worker", 100, 1000).unwrap().is_none());
    let LearningScheduleOutcome::Existing(same) = ingestion
        .reprocess("p", "a", &scheduler, 101, &str::to_owned)
        .unwrap()
    else {
        panic!("a repeated repair must not reset paid attempts");
    };
    assert_eq!(same.id, job.id);
    assert_eq!(same.attempt, 3);
    assert_eq!(
        ingestion
            .repair_history("p", &scheduler, 102, false, &str::to_owned)
            .unwrap()
            .examined,
        0
    );
}

#[test]
fn missing_source_history_and_forgotten_sources_never_regain_verification() {
    let (pool, scheduler, id) = fixture("part:up");
    pool.get().unwrap().execute("DELETE FROM part", []).unwrap();
    let ingestion = LearningIngestion::new(pool.clone());
    let report = ingestion
        .repair_history("p", &scheduler, 30, false, &str::to_owned)
        .unwrap();
    assert_eq!(report.unavailable, 1);
    assert_eq!(report.queued, 0);
    assert!(
        !ExperienceStore::new(pool.clone())
            .get(&id)
            .unwrap()
            .verified_sources()
    );
    assert!(MemoryEvidenceStore::new(pool).get(&id).unwrap().is_none());

    let (pool, scheduler, id) = fixture("part:up");
    pool.get()
        .unwrap()
        .execute(
            "UPDATE experience_record SET status='forgotten' WHERE id=?1",
            [&id],
        )
        .unwrap();
    let ingestion = LearningIngestion::new(pool.clone());
    assert!(matches!(
        ingestion
            .reprocess("p", "a", &scheduler, 30, &str::to_owned)
            .unwrap(),
        LearningScheduleOutcome::Excluded
    ));
    assert_eq!(
        ingestion
            .repair_history("p", &scheduler, 31, false, &str::to_owned)
            .unwrap()
            .examined,
        0
    );
    assert!(
        !ExperienceStore::new(pool)
            .get(&id)
            .unwrap()
            .verified_sources()
    );
}

#[test]
fn history_repair_is_project_scoped_and_has_a_hard_batch_limit() {
    let (pool, scheduler, _) = fixture("part:up");
    let jobs = LearningJobStore::new(pool.clone());
    for index in 0..40 {
        jobs.enqueue(NewLearningJob::extraction(
            format!("backlog-{index}"),
            "p",
            "s",
            "a",
            format!("legacy-{index}"),
            json!({"request":{"transcript":"legacy"}}),
            15 + index,
        ))
        .unwrap();
    }
    let ingestion = LearningIngestion::new(pool.clone());
    assert!(
        ingestion
            .reprocess("another-project", "a", &scheduler, 100, &str::to_owned)
            .is_err()
    );
    assert_eq!(
        ingestion
            .repair_history("another-project", &scheduler, 100, true, &str::to_owned)
            .unwrap()
            .examined,
        0
    );
    let report = ingestion
        .repair_history("p", &scheduler, 100, true, &str::to_owned)
        .unwrap();
    assert_eq!(report.examined, 32);
    assert!(report.has_more);
    assert_eq!(jobs.list_for_project("p", 100).unwrap().len(), 41);
}

#[test]
fn an_old_source_digest_is_not_upgraded_after_source_drift_even_if_the_quote_survives() {
    let (pool, scheduler, id) = fixture("part:up");
    let sources = zuno_db::learning_source::LearningSourceStore::new(pool.clone())
        .for_turn("s", "a", &str::to_owned)
        .unwrap()
        .sources;
    let source = sources
        .iter()
        .find(|source| source.source_id == "up")
        .unwrap();
    pool.get()
        .unwrap()
        .execute(
            "UPDATE experience_evidence SET source_digest=?2 WHERE experience_id=?1",
            rusqlite::params![id, source.source_digest],
        )
        .unwrap();
    pool.get().unwrap().execute(
        "UPDATE part SET data=json_set(data,'$.text','Correction: prefer concise reports. Added content.') WHERE id='up'",[],
    ).unwrap();
    let report = LearningIngestion::new(pool.clone())
        .repair_history("p", &scheduler, 30, false, &str::to_owned)
        .unwrap();
    assert_eq!(report.revalidated_experiences, 0);
    assert_eq!(report.items[0].unverified_experiences, 1);
    assert!(
        !ExperienceStore::new(pool.clone())
            .get(&id)
            .unwrap()
            .verified_sources()
    );
    assert!(MemoryEvidenceStore::new(pool).get(&id).unwrap().is_none());
}

#[test]
fn an_old_failed_job_gets_one_version_upgrade_without_rewriting_the_original_failure() {
    let (pool, scheduler, _) = fixture("part:up");
    let original_error = "HTTP 400: upstream rejected an unspecified request option";
    pool.get()
        .unwrap()
        .execute(
            "UPDATE learning_job SET status='failed',error=?1 WHERE id='old'",
            [original_error],
        )
        .unwrap();
    let ingestion = LearningIngestion::new(pool.clone());
    let report = ingestion
        .repair_history("p", &scheduler, 30, false, &str::to_owned)
        .unwrap();
    assert_eq!(report.queued, 1);
    let jobs = LearningJobStore::new(pool);
    assert_eq!(
        jobs.get("old").unwrap().error.as_deref(),
        Some(original_error)
    );
    let LearningScheduleOutcome::Existing(upgraded) = ingestion
        .reprocess("p", "a", &scheduler, 31, &str::to_owned)
        .unwrap()
    else {
        panic!("the explicit command must reuse the admitted version upgrade");
    };
    assert_eq!(
        upgraded.extractor_version.as_deref(),
        Some(zuno_learning::LEARNING_EXTRACTOR_VERSION)
    );
    assert_eq!(upgraded.attempt, 0);
    assert_eq!(
        ingestion
            .repair_history("p", &scheduler, 32, false, &str::to_owned)
            .unwrap()
            .examined,
        0
    );
}

#[test]
fn excluded_legacy_sources_do_not_keep_a_completed_repair_batch_pending() {
    let (pool, _, id) = fixture("part:up");
    pool.get().unwrap().execute_batch(r#"
        INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
        VALUES('external','a','s',2,2,
          '{"type":"tool","tool":"web_search","state":{"status":"completed","output":"external context","metadata":{"externalContext":true}}}');
    "#).unwrap();
    let scheduler = LearningScheduler::new(
        pool.clone(),
        ResolvedLearningConfig {
            disable_on_external_context: true,
            ..Default::default()
        },
    )
    .with_extractor_version(zuno_learning::LEARNING_EXTRACTOR_VERSION);
    let ingestion = LearningIngestion::new(pool.clone());
    let first = ingestion
        .repair_history("p", &scheduler, 30, false, &str::to_owned)
        .unwrap();
    assert_eq!(first.excluded, 1);
    assert_eq!(first.queued, 0);
    assert!(
        !ExperienceStore::new(pool)
            .get(&id)
            .unwrap()
            .verified_sources()
    );
    assert_eq!(
        ingestion
            .repair_history("p", &scheduler, 31, false, &str::to_owned)
            .unwrap()
            .examined,
        0,
        "a terminal exclusion must advance the bounded history scan"
    );
}
