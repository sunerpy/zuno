use serde_json::json;
use std::sync::Arc;
use zuno_db::event_log::SessionEventLog;
use zuno_db::experience::{
    ExperienceEvidenceKind, ExperienceStore, NewExperience, NewExperienceEvidence,
};
use zuno_db::learning_job::{LearningJobStatus, LearningJobStore, LearningLease, NewLearningJob};
use zuno_db::learning_source::{LearningSource, LearningSourceStore, digest};
use zuno_db::{Pool, migration};
use zuno_error::DbError;
use zuno_paths::DbLocation;
use zuno_types::{ExperienceKind, ExperienceStatus};

struct Fixture {
    pool: Arc<Pool>,
    jobs: LearningJobStore,
    experiences: ExperienceStore,
    source: LearningSource,
    lease: LearningLease,
}

#[derive(Debug, Clone, Copy)]
enum Mutation {
    Source,
    Forget,
}

impl Fixture {
    fn new() -> Self {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).unwrap());
        {
            let mut connection = pool.get().unwrap();
            migration::apply(&mut connection).unwrap();
            connection.execute_batch(r#"
                INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
                VALUES('p','/workspace',1,1,'[]');
                INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
                VALUES('s','p','s','/workspace','source fence','test',1,10);
                INSERT INTO message(id,session_id,time_created,time_updated,data)
                VALUES('u','s',1,1,'{"role":"user"}'),
                      ('a','s',2,10,'{"role":"assistant","parentID":"u","finish":"stop","time":{"completed":10}}');
                INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
                VALUES('up','u','s',1,1,'{"type":"text","text":"Correction: prefer concise reports."}');
            "#).unwrap();
        }
        let source_store = LearningSourceStore::new(pool.clone());
        let sources = source_store
            .for_turn("s", "a", &str::to_owned)
            .unwrap()
            .sources;
        let snapshot = source_store
            .snapshot("p", "s", "a", &sources, false, true)
            .unwrap();
        let source = sources[0].clone();
        let jobs = LearningJobStore::new(pool.clone());
        jobs.enqueue(NewLearningJob::extraction(
            "job",
            "p",
            "s",
            "a",
            "source-fence-test",
            json!({
                "trigger":"automatic_post_turn",
                "sourceSnapshot":snapshot,
                "request":{
                    "project_id":"p","session_id":"s","source_message_id":"a",
                    "sources":sources,"transcript":"","sources_truncated":false,
                    "had_tool_calls":false,"had_artifacts":false,"user_corrected":true,
                    "recovered_from_error":false,"explicit_feedback":false
                }
            }),
            10,
        ))
        .unwrap();
        let lease = jobs
            .claim("job", "worker", 11, 1_000)
            .unwrap()
            .unwrap()
            .lease()
            .unwrap();
        let fixture = Self {
            experiences: ExperienceStore::new(pool.clone()),
            pool,
            jobs,
            source,
            lease,
        };
        let mut prior = fixture.experience("prior", true);
        prior.extraction_job_id = None;
        prior.extraction_ordinal = None;
        fixture.experiences.create_manual(prior).unwrap();
        fixture
    }

    fn experience(&self, id: &str, verified: bool) -> NewExperience {
        NewExperience {
            id: id.to_owned(),
            project_id: "p".to_owned(),
            session_id: Some("s".to_owned()),
            source_message_id: Some("a".to_owned()),
            extraction_job_id: Some("job".to_owned()),
            extraction_ordinal: Some(0),
            kind: ExperienceKind::UserCorrection,
            title: "Concise reports".to_owned(),
            summary: "Prefer concise reports.".to_owned(),
            resolution: None,
            confidence: 9_500,
            fingerprint: digest("user_correction:prefer concise reports"),
            evidence: vec![NewExperienceEvidence {
                id: format!("evidence-{id}"),
                kind: ExperienceEvidenceKind::User,
                source_id: Some(self.source.source_id.clone()),
                excerpt: "prefer concise reports.".to_owned(),
                digest: digest("prefer concise reports."),
                source_digest: verified.then(|| self.source.source_digest.clone()),
                verified,
                promotion_eligible: false,
            }],
            time_created: 20,
        }
    }

    fn mutate_after_preflight(&self, mutation: Mutation) {
        assert!(
            self.jobs.sources_current("job").unwrap(),
            "preflight must really pass"
        );
        match mutation {
            Mutation::Source => {
                // A different connection commits between application preflight
                // and the later extraction writer transaction.
                self.pool
                    .open_connection()
                    .unwrap()
                    .execute(
                        "UPDATE part SET data=json_set(data,'$.text',
                     'Correction: prefer concise reports. Source changed after preflight.')
                     WHERE id='up'",
                        [],
                    )
                    .unwrap();
            }
            Mutation::Forget => {
                self.experiences.forget("prior", 21).unwrap();
            }
        }
        assert!(!self.jobs.sources_current("job").unwrap());
    }

    fn inject_write_race(&self, mutation: Mutation) {
        assert!(self.jobs.sources_current("job").unwrap());
        let statement = match mutation {
            Mutation::Source => {
                "UPDATE part SET data=json_set(data,'$.text','source changed inside writer') WHERE id='up';"
            }
            Mutation::Forget => "UPDATE experience_record SET status='forgotten' WHERE id='prior';",
        };
        self.pool
            .get()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER race_after_extraction_insert AFTER INSERT ON experience_record
             WHEN new.extraction_job_id='job' BEGIN {statement} END;"
            ))
            .unwrap();
    }

    fn assert_rejected_without_orphans(&self, record_only: bool) {
        let before_job = self.jobs.get("job").unwrap();
        let events = SessionEventLog::new(self.pool.clone());
        let before_events = events.read_after("s", None).unwrap();
        let result = if record_only {
            self.experiences.record_extraction(
                "job",
                &self.lease,
                &[self.experience("new", false)],
                30,
            )
        } else {
            self.experiences.complete_extraction(
                "job",
                &self.lease,
                &[self.experience("new", false)],
                &json!({"unverifiedExperienceIds":["new"]}),
                30,
            )
        };
        let error = result.expect_err("a stale source must not commit even an unverified row");
        assert!(
            matches!(&error, DbError::Conflict { table, id, .. } if table=="learning_job" && id=="job"),
            "the source fence must report a typed conflict: {error:?}"
        );
        let counts: (i64, i64) = self
            .pool
            .get()
            .unwrap()
            .query_row(
                "SELECT
              (SELECT count(*) FROM experience_record WHERE extraction_job_id='job'),
              (SELECT count(*) FROM experience_evidence WHERE experience_id='new')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (0, 0), "no experience or evidence orphans");
        assert_eq!(events.read_after("s", None).unwrap(), before_events);
        assert_eq!(self.jobs.get("job").unwrap(), before_job);
    }
}

#[test]
fn source_drift_after_preflight_blocks_atomic_unverified_extraction() {
    let fixture = Fixture::new();
    fixture.mutate_after_preflight(Mutation::Source);
    fixture.assert_rejected_without_orphans(false);
    assert!(
        !fixture.jobs.sources_current("job").unwrap(),
        "the external edit must remain committed"
    );
}

#[test]
fn forgetting_after_preflight_blocks_atomic_unverified_extraction() {
    let fixture = Fixture::new();
    fixture.mutate_after_preflight(Mutation::Forget);
    fixture.assert_rejected_without_orphans(false);
    assert_eq!(
        fixture.experiences.get("prior").unwrap().projection.status,
        ExperienceStatus::Forgotten
    );
}

#[test]
fn source_drift_after_preflight_blocks_record_only_writer() {
    let fixture = Fixture::new();
    fixture.mutate_after_preflight(Mutation::Source);
    fixture.assert_rejected_without_orphans(true);
}

#[test]
fn forgetting_after_preflight_blocks_record_only_writer() {
    let fixture = Fixture::new();
    fixture.mutate_after_preflight(Mutation::Forget);
    fixture.assert_rejected_without_orphans(true);
}

#[test]
fn source_drift_during_atomic_write_rolls_back_rows_events_and_completion() {
    let fixture = Fixture::new();
    fixture.inject_write_race(Mutation::Source);
    fixture.assert_rejected_without_orphans(false);
    assert!(
        fixture.jobs.sources_current("job").unwrap(),
        "the injected edit must also roll back"
    );
}

#[test]
fn forgetting_during_atomic_write_rolls_back_rows_events_and_completion() {
    let fixture = Fixture::new();
    fixture.inject_write_race(Mutation::Forget);
    fixture.assert_rejected_without_orphans(false);
    assert_eq!(
        fixture.experiences.get("prior").unwrap().projection.status,
        ExperienceStatus::Active
    );
}

#[test]
fn record_only_writer_revalidates_after_inserts_before_committing() {
    for mutation in [Mutation::Source, Mutation::Forget] {
        let fixture = Fixture::new();
        fixture.inject_write_race(mutation);
        fixture.assert_rejected_without_orphans(true);
        assert!(fixture.jobs.sources_current("job").unwrap());
    }
}

#[test]
fn source_drift_after_job_settlement_still_rolls_back_the_whole_transaction() {
    let fixture = Fixture::new();
    assert!(fixture.jobs.sources_current("job").unwrap());
    fixture
        .pool
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER race_after_settlement AFTER UPDATE OF status ON learning_job
         WHEN new.id='job' AND new.status='completed' BEGIN
           UPDATE part SET data=json_set(data,'$.text','changed during settlement') WHERE id='up';
         END;",
        )
        .unwrap();
    fixture.assert_rejected_without_orphans(false);
    assert!(fixture.jobs.sources_current("job").unwrap());
}

#[test]
fn forgetting_after_job_settlement_still_rolls_back_the_whole_transaction() {
    let fixture = Fixture::new();
    assert!(fixture.jobs.sources_current("job").unwrap());
    fixture
        .pool
        .get()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER forget_after_settlement AFTER UPDATE OF status ON learning_job
         WHEN new.id='job' AND new.status='completed' BEGIN
           UPDATE experience_record SET status='forgotten' WHERE id='prior';
         END;",
        )
        .unwrap();
    fixture.assert_rejected_without_orphans(false);
    assert_eq!(
        fixture.experiences.get("prior").unwrap().projection.status,
        ExperienceStatus::Active
    );
}

#[test]
fn a_current_closed_snapshot_commits_while_the_next_turn_is_live() {
    let fixture = Fixture::new();
    fixture
        .pool
        .get()
        .unwrap()
        .execute_batch(
            r#"
        INSERT INTO message(id,session_id,time_created,time_updated,data)
        VALUES('next-u','s',21,21,'{"role":"user"}'),
              ('next-a','s',22,22,'{"role":"assistant","parentID":"next-u"}');
        UPDATE session SET time_updated=22 WHERE id='s';
    "#,
        )
        .unwrap();
    assert!(fixture.jobs.sources_current("job").unwrap());
    let stored = fixture
        .experiences
        .complete_extraction(
            "job",
            &fixture.lease,
            &[fixture.experience("new", false)],
            &json!({}),
            30,
        )
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].verified_sources());
    assert_eq!(
        fixture.jobs.get("job").unwrap().status,
        LearningJobStatus::Completed
    );
}
