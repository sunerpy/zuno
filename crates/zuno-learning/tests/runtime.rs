use async_trait::async_trait;
use futures::stream;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use zuno_config::ResolvedLearningConfig;
use zuno_db::{Pool, migration};
use zuno_eval::{AttemptSnapshot, OfflineCaseEvaluator, OfflineCaseRequest};
use zuno_learning::{
    ConsolidatedPattern, Consolidation, ConsolidationRequest, ExperienceRetriever,
    ExperienceService, ExtractionRequest, LearningExtractor, LearningModel, LearningModelClient,
    LearningProjectionService, ManualExperienceRequest, PatternConsolidator, PatternMiner,
    ProviderSkillEvaluator,
};
use zuno_llm::{
    event::{FinishReason, StreamEvent},
    registry::{ApiSurface, Capabilities, CompletionRequest, Provider, ProviderStream, generation},
};
use zuno_paths::DbLocation;
use zuno_types::ExperienceKind;

fn pool() -> Arc<Pool> {
    let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
    {
        let mut db = pool.get().expect("connection");
        migration::apply(&mut db).expect("schema");
        db.execute_batch(r#"
          INSERT INTO project(id,worktree,time_created,time_updated,sandboxes) VALUES('p','/work',1,1,'[]');
          INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
          VALUES('s','p','s','/work','test','1',1,1),('s2','p','s2','/work','test two','1',1,1);
          INSERT INTO message(id,session_id,time_created,time_updated,data)
          VALUES('m','s',2,2,'{"role":"assistant"}');
        "#).expect("fixture");
    }
    pool
}

#[derive(Debug)]
enum Reply {
    Events(Vec<StreamEvent>),
    Pending,
}
#[derive(Debug)]
struct ScriptedProvider {
    replies: Mutex<VecDeque<Reply>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "test"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }
    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        self.requests.lock().expect("requests").push(request);
        match self
            .replies
            .lock()
            .expect("replies")
            .pop_front()
            .expect("unexpected model request")
        {
            Reply::Pending => Box::pin(stream::pending()),
            Reply::Events(events) => Box::pin(stream::iter(events.into_iter().map(Ok))),
        }
    }
}

fn answer(text: &str) -> Reply {
    Reply::Events(vec![
        StreamEvent::TextDelta(text.to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])
}
fn client(replies: Vec<Reply>) -> (LearningModelClient, Arc<Mutex<Vec<CompletionRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    (
        LearningModelClient {
            provider: Arc::new(ScriptedProvider {
                replies: Mutex::new(replies.into()),
                requests: requests.clone(),
            }),
            model: LearningModel {
                provider_id: "test".to_owned(),
                model_id: "model".to_owned(),
                wire_id: "model".to_owned(),
                surface: ApiSurface::Chat,
            },
            events: zuno_db::event_log::SessionEventLog::new(pool()),
            limits: ResolvedLearningConfig::default(),
        },
        requests,
    )
}
fn request() -> ExtractionRequest {
    ExtractionRequest {
        project_id: "p".to_owned(),
        session_id: "s".to_owned(),
        source_message_id: "m".to_owned(),
        transcript: "legacy transcript must not reach the model".repeat(10_000),
        sources: Vec::new(),
        sources_truncated: false,
        had_tool_calls: true,
        had_artifacts: false,
        explicit_feedback: false,
        user_corrected: false,
        recovered_from_error: false,
    }
}

#[tokio::test]
async fn extraction_bounds_legacy_input_repairs_once_and_audits_the_real_requests() {
    let (mut client, requests) = client(vec![
        answer("invalid JSON"),
        answer(r#"{"experiences":[],"memories":[]}"#),
    ]);
    client.limits.execution_structured_output = true;
    client.limits.execution_max_output_tokens = 512;
    client.extract(request()).await.expect("bounded repair");
    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert!(request.tools.is_empty());
        assert_eq!(request.parameters[generation::MAX_TOKENS], 512);
        assert_eq!(
            request.parameters["response_format"]["json_schema"]["strict"],
            true
        );
        let body = serde_json::to_string(&request.messages).expect("messages");
        assert!(!body.contains("legacy transcript must not reach the model"));
        assert!(body.len() < 131_072);
    }
    let events = client.events.read_after("s", None).expect("receipts");
    assert_eq!(events.len(), 4);
    assert_eq!(
        events[0].properties["request"]["parameters"][generation::MAX_TOKENS],
        512
    );
    assert_eq!(events[1].properties["output"], "invalid JSON");
    assert!(
        events[2].properties["request"]["messages"]
            .to_string()
            .contains("invalid JSON")
    );
}

#[tokio::test(start_paused = true)]
async fn auxiliary_stream_deadline_terminates_a_hung_provider_and_records_failure() {
    let (mut client, _) = client(vec![Reply::Pending]);
    client.limits.execution_timeout_ms = 50;
    let error = client.extract(request()).await.expect_err("deadline");
    assert!(matches!(
        error.recovery(),
        zuno_error::Recovery::Retry { .. }
    ));
    let events = client.events.read_after("s", None).expect("events");
    assert_eq!(
        events.last().expect("outcome").properties["status"],
        "failed"
    );
}

#[tokio::test]
async fn candidate_execution_reads_cassette_results_before_a_blind_grading_request() {
    let (client, requests) = client(vec![
        Reply::Events(vec![
            StreamEvent::ToolUseStart {
                id: "call".to_owned(),
                name: "shell".to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: "call".to_owned(),
                delta: r#"{"command":"cargo test"}"#.to_owned(),
            },
            StreamEvent::ToolUseEnd {
                id: "call".to_owned(),
            },
            StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::ToolCalls),
            },
        ]),
        answer("The recorded cargo test result passed."),
        answer(
            r#"{"score":95,"passed":true,"criticalFailure":false,"explanation":"The actual trace proves the result."}"#,
        ),
    ]);
    let evaluator = ProviderSkillEvaluator {
        client,
        session_id: "s".to_owned(),
    };
    let result = evaluator
        .evaluate(OfflineCaseRequest {
            case_id: "case".to_owned(),
            skill_content: "Verify before claiming success.".to_owned(),
            prompt: "Run the project test command.".to_owned(),
            expected: "GRADER_ONLY_EXPECTATION".to_owned(),
            tool_cassette: json!({"calls":[{"name":"shell","arguments":{"command":"cargo test"},
            "output":"RECORDED_RESULT passed","is_error":false}]}),
            attempt: AttemptSnapshot {
                model: "test/model".to_owned(),
                toolset_digest: "cassette".to_owned(),
                max_output_tokens: 4096,
                max_steps: 8,
                temperature_millis: 0,
                seed: 0,
            },
        })
        .await
        .expect("real offline attempt");
    assert!(result.passed);
    assert_eq!(result.details["trace"][0]["result"]["matched"], true);
    assert_eq!(result.details["liveTools"], false);
    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    let first = serde_json::to_string(&requests[0].messages).expect("request");
    assert!(!first.contains("GRADER_ONLY_EXPECTATION"));
    assert!(!first.contains("RECORDED_RESULT"));
    assert_eq!(requests[0].tools[0].name, "shell");
    assert!(
        serde_json::to_string(&requests[1].messages)
            .expect("continuation")
            .contains("RECORDED_RESULT")
    );
    assert!(requests[2].tools.is_empty());
    assert!(
        serde_json::to_string(&requests[2].messages)
            .expect("grading")
            .contains("GRADER_ONLY_EXPECTATION")
    );
}

fn record(service: &ExperienceService, session: &str, text: &str, now: i64) {
    service
        .record_manual(ManualExperienceRequest {
            project_id: "p".to_owned(),
            session_id: Some(session.to_owned()),
            source_message_id: None,
            kind: ExperienceKind::Procedure,
            title: text.to_owned(),
            summary: text.to_owned(),
            resolution: Some(text.to_owned()),
            time_created: now,
        })
        .expect("manual evidence");
}
struct Consolidator;
#[async_trait]
impl PatternConsolidator for Consolidator {
    async fn consolidate(
        &self,
        request: ConsolidationRequest,
    ) -> zuno_learning::Result<Consolidation> {
        assert_ne!(
            request.experiences[0].summary,
            request.experiences[1].summary
        );
        Ok(Consolidation {
            groups: vec![ConsolidatedPattern {
                existing_pattern_id: request.patterns.first().map(|pattern| pattern.id.clone()),
                title: "Run repository tests".to_owned(),
                learned_rules: vec!["Use cargo test before publishing.".to_owned()],
                evidence_ids: request
                    .experiences
                    .iter()
                    .map(|experience| experience.id.clone())
                    .collect(),
            }],
        })
    }
}

#[tokio::test]
async fn differently_worded_verified_experiences_share_a_stable_reviewed_pattern() {
    let pool = pool();
    let service = ExperienceService::new(pool.clone(), None);
    record(&service, "s", "Run cargo test before a release.", 10);
    record(
        &service,
        "s2",
        "Test the workspace with Cargo prior to publishing.",
        20,
    );
    let miner = PatternMiner::new(
        pool,
        ResolvedLearningConfig {
            aggregation_min_new_records: 2,
            ..ResolvedLearningConfig::default()
        },
    )
    .with_consolidator(Arc::new(Consolidator));
    let first = miner
        .mine_project("p", 0, 30)
        .await
        .expect("semantic grouping");
    let zuno_db::learning_pattern::PatternProposal::Proposed { record, .. } = &first[0] else {
        panic!("new pattern");
    };
    assert_eq!(record.projection.independent_sessions, 2);
    let approved = miner.promote(&record.projection.id, 40).expect("review");
    miner.mine_project("p", 0, 50).await.expect("repeat");
    assert_eq!(miner.get(&record.projection.id).expect("pattern"), approved);
}

#[test]
fn pure_retrieval_and_explicit_selection_accounting_have_distinct_effects() {
    let pool = pool();
    let service = ExperienceService::new(pool.clone(), None);
    record(&service, "s", "cargo test", 10);
    record(&service, "s2", "cargo check", 20);
    record(&service, "s", "cargo fmt", 30);
    let retriever = ExperienceRetriever::new(pool.clone(), &ResolvedLearningConfig::default());
    let selected = retriever.retrieve("p", "cargo test").expect("recall");
    assert!(!selected.items.is_empty());
    assert_eq!(
        pool.get()
            .expect("connection")
            .query_row("SELECT sum(use_count) FROM experience_record", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("usage"),
        0
    );
    retriever
        .record_selection("s", "p", "cargo test", &selected)
        .expect("explicit boundary");
    let page = LearningProjectionService::new(pool.clone())
        .page("s", "p", 1, 1)
        .expect("status");
    assert_eq!(page.experience_page.total, 3);
    assert_eq!(page.experience_page.next_offset, Some(2));
    assert_eq!(page.experiences.len(), 1);
    assert_eq!(
        page.retrieval.expect("recall receipt").selected_ids.len(),
        selected.items.len()
    );
    let usage = pool
        .get()
        .expect("connection")
        .query_row("SELECT sum(use_count) FROM experience_record", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("usage");
    retriever
        .retrieve("p", "cargo test")
        .expect("another pure read");
    assert_eq!(
        pool.get()
            .expect("connection")
            .query_row("SELECT sum(use_count) FROM experience_record", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("usage"),
        usage
    );
}

struct QueueWork(zuno_learning::ProjectLearningService);
#[async_trait]
impl zuno_learning::LearningWork for QueueWork {
    async fn tick(&self, cancel: tokio_util::sync::CancellationToken) {
        if let Some(job) = self.0.claim("worker", &[]).expect("claim") {
            self.0.execute(job, &cancel).await.expect("execute");
        }
    }
}

#[tokio::test]
async fn startup_catchup_is_idempotent_and_project_work_outlives_the_registering_session() {
    let pool = pool();
    let now = zuno_db::message::now_millis();
    let completed = now - 7 * 3_600_000;
    {
        let db = pool.get().expect("connection");
        db.execute(
            "UPDATE message SET time_created=?1,time_updated=?1,data=?2 WHERE id='m'",
            rusqlite::params![
                completed,
                json!({"role":"assistant","finish":"stop","time":{"completed":completed}})
                    .to_string()
            ],
        )
        .expect("completed turn");
        db.execute(
            "UPDATE session SET time_updated=?1 WHERE id='s'",
            [completed],
        )
        .expect("idle session");
        db.execute("INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
            VALUES('tool','m','s',?1,?1,?2)",
            rusqlite::params![completed,json!({"type":"tool","callID":"call","tool":"shell",
                "state":{"status":"completed","input":{"command":"cargo test"},"output":"observed"}}).to_string()])
            .expect("tool source");
    }
    let settings = ResolvedLearningConfig::default();
    let scheduler = zuno_learning::LearningScheduler::new(pool.clone(), settings.clone());
    let ingestion = zuno_learning::LearningIngestion::new(pool.clone());
    assert_eq!(
        ingestion
            .catch_up("p", &scheduler, now, &str::to_owned)
            .expect("missed admission"),
        1
    );
    assert_eq!(
        ingestion
            .catch_up("p", &scheduler, now, &str::to_owned)
            .expect("idempotency"),
        0
    );
    let (mut client, requests) = client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
    client.events = zuno_db::event_log::SessionEventLog::new(pool.clone());
    let service = zuno_learning::ProjectLearningService {
        scheduler,
        extractor: Arc::new(client),
        experiences: ExperienceService::new(pool.clone(), None),
        patterns: PatternMiner::new(pool.clone(), settings.clone()),
        skills: zuno_learning::SkillCandidateService::new(pool.clone(), settings),
        project_id: "p".to_owned(),
        project_root: std::path::PathBuf::from("/work"),
    };
    let supervisor = zuno_learning::LearningSupervisor::default();
    {
        let session_owner = supervisor.clone();
        session_owner.ensure_project(
            "p".to_owned(),
            Arc::new(QueueWork(service)),
            std::time::Duration::from_millis(10),
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let completed: bool = pool
                .get()
                .expect("connection")
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM learning_job WHERE status='completed')",
                    [],
                    |row| row.get(0),
                )
                .expect("job");
            if completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("project worker remains alive");
    supervisor.shutdown(std::time::Duration::from_secs(1)).await;
    assert_eq!(requests.lock().expect("requests").len(), 1);
}

#[test]
fn a_legacy_request_gets_bounded_sources_and_cancelled_manual_work_is_not_replayed() {
    let pool = pool();
    pool.get()
        .expect("connection")
        .execute_batch(
            r#"INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
         VALUES('text','m','s',2,2,'{"type":"text","text":"Recorded assistant observation."}');"#,
        )
        .expect("source");
    let now = zuno_db::message::now_millis();
    let scheduler =
        zuno_learning::LearningScheduler::new(pool.clone(), ResolvedLearningConfig::default());
    let jobs = zuno_db::learning_job::LearningJobStore::new(pool.clone());
    jobs.enqueue(zuno_db::learning_job::NewLearningJob::extraction(
        "legacy",
        "p",
        "s",
        "m",
        "old-extractor",
        serde_json::to_value(zuno_learning::ExtractionJobPayload::manual(request()))
            .expect("input"),
        now,
    ))
    .expect("job");
    let job = scheduler
        .claim("legacy", "worker", now, now + 60_000)
        .expect("claim")
        .expect("job");
    let job = zuno_learning::LearningIngestion::new(pool)
        .upgrade_legacy_job(job, &scheduler, &str::to_owned)
        .expect("source refresh");
    let input = zuno_learning::decode_extraction_job_payload(job.payload.clone().expect("payload"))
        .expect("typed payload")
        .into_request();
    assert!(input.transcript.is_empty());
    assert_eq!(input.sources.len(), 1);
    {
        let _guard = zuno_learning::ManualReflectionGuard::new(
            scheduler.clone(),
            job.id.clone(),
            job.lease().expect("lease"),
        );
    }
    assert_eq!(
        scheduler.get("legacy").expect("cancelled job").status,
        zuno_db::learning_job::LearningJobStatus::Skipped
    );
    assert!(
        scheduler
            .claim("legacy", "worker", now + 1, now + 60_000)
            .expect("no automatic replay")
            .is_none()
    );
}

struct GlobalConsolidator;
#[async_trait]
impl PatternConsolidator for GlobalConsolidator {
    async fn consolidate(
        &self,
        request: ConsolidationRequest,
    ) -> zuno_learning::Result<Consolidation> {
        assert_eq!(request.scope, zuno_learning::ConsolidationScope::Global);
        assert!(request.experiences.is_empty());
        Ok(Consolidation {
            groups: vec![ConsolidatedPattern {
                existing_pattern_id: request
                    .patterns
                    .iter()
                    .find(|pattern| pattern.project_id.is_none())
                    .map(|pattern| pattern.id.clone()),
                title: "Verify a release".to_owned(),
                learned_rules: vec!["Run Cargo tests.".to_owned()],
                evidence_ids: request
                    .patterns
                    .iter()
                    .filter(|pattern| pattern.project_id.is_some())
                    .map(|pattern| pattern.id.clone())
                    .collect(),
            }],
        })
    }
}

#[tokio::test]
async fn global_consolidation_matches_reviewed_rules_with_different_fingerprints() {
    use zuno_db::learning_pattern::{
        LearningPatternStore, NewLearningPattern, PatternProposal, PatternScope,
    };
    let pool = pool();
    pool.get().expect("connection").execute_batch(
        "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes) VALUES('q','/other',1,1,'[]');
         UPDATE session SET project_id='q' WHERE id='s2';"
    ).expect("second project");
    let service = ExperienceService::new(pool.clone(), None);
    let patterns = LearningPatternStore::new(pool.clone());
    for (project, session, rule) in [
        ("p", "s", "Run cargo test."),
        ("q", "s2", "Validate with Cargo before publishing."),
    ] {
        let evidence = service
            .record_manual(ManualExperienceRequest {
                project_id: project.to_owned(),
                session_id: Some(session.to_owned()),
                source_message_id: None,
                kind: ExperienceKind::Procedure,
                title: rule.to_owned(),
                summary: rule.to_owned(),
                resolution: Some(rule.to_owned()),
                time_created: 10,
            })
            .expect("explicit evidence");
        let PatternProposal::Proposed { record, .. } = patterns
            .propose(NewLearningPattern {
                id: format!("pattern-{project}"),
                scope: PatternScope::Project,
                project_id: Some(project.to_owned()),
                fingerprint: format!("different-{project}"),
                title: rule.to_owned(),
                summary: rule.to_owned(),
                learned_rules: vec![rule.to_owned()],
                evidence_ids: vec![evidence.projection.id],
                evidence_digest: format!("evidence-{project}"),
                evidence_version: 1,
                independent_sessions: 1,
                project_count: 1,
                time_created: 11,
            })
            .expect("project pattern")
        else {
            panic!("new pattern");
        };
        patterns
            .promote(&record.projection.id, 12)
            .expect("human review");
    }
    let miner = PatternMiner::new(pool, ResolvedLearningConfig::default())
        .with_consolidator(Arc::new(GlobalConsolidator));
    assert!(
        miner
            .global_evidence_digest()
            .expect("independent input identity")
            .is_some()
    );
    let output = miner.mine_global(20).await.expect("semantic global group");
    let PatternProposal::Proposed { record, .. } = &output[0] else {
        panic!("new global pattern");
    };
    assert_eq!(record.projection.project_count, 2);
    let reviewed = miner
        .promote(&record.projection.id, 21)
        .expect("review global pattern");
    miner.mine_global(30).await.expect("unchanged rerun");
    assert_eq!(
        miner.get(&record.projection.id).expect("global pattern"),
        reviewed
    );
}
