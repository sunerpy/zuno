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
    registry::{
        ApiSurface, Capabilities, CompletionRequest, Provider, ProviderStream, Spec, generation,
        model_capabilities,
    },
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
          VALUES('m','s',2,2,'{"role":"assistant","finish":"stop","time":{"completed":2}}');
        "#).expect("fixture");
    }
    pool
}

#[derive(Debug)]
enum Reply {
    Events(Vec<StreamEvent>),
    Failure(zuno_error::ProviderError),
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
            Reply::Failure(error) => Box::pin(stream::once(async move { Err(error) })),
            Reply::Events(events) => Box::pin(stream::iter(events.into_iter().map(Ok))),
        }
    }
}

#[tokio::test]
async fn provider_diagnostics_keep_the_bounded_reason_and_redact_credentials() {
    let (client, _) = client(vec![Reply::Failure(zuno_error::ProviderError::Fatal {
        status: Some(400),
        source: Some(Box::new(std::io::Error::other(
            "code=unsupported_parameter requestID=req_test reason=unsupported setting; \
             Authorization: Bearer sk-secret-credential-for-test",
        ))),
    })]);
    let error = client
        .extract(request())
        .await
        .expect_err("invalid request");
    assert_eq!(error.recovery(), zuno_error::Recovery::Fail);
    let events = client
        .events
        .read_after("s", None)
        .expect("durable outcome");
    let diagnostic = events.last().unwrap().properties["error"].as_str().unwrap();
    assert!(diagnostic.contains("unsupported_parameter"), "{diagnostic}");
    assert!(diagnostic.contains("req_test"), "{diagnostic}");
    assert!(diagnostic.contains("unsupported setting"), "{diagnostic}");
    assert!(
        !diagnostic.contains("sk-secret-credential-for-test"),
        "{diagnostic}"
    );
    assert!(diagnostic.len() <= 4096);
}

fn answer(text: &str) -> Reply {
    Reply::Events(vec![
        StreamEvent::TextDelta(text.to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])
}

#[tokio::test]
async fn learning_usage_is_durable_and_partial_reports_do_not_count_cache_twice() {
    use zuno_llm::event::PromptAccounting;
    let (client, _) = client(vec![Reply::Events(vec![
        StreamEvent::TokenUsage {
            input_tokens: Some(100),
            output_tokens: None,
            reasoning_tokens: None,
            cache_read_input_tokens: Some(30),
            cache_write_input_tokens: Some(10),
            accounting: PromptAccounting::CacheInsideInput,
        },
        StreamEvent::TextDelta(r#"{"experiences":[],"memories":[]}"#.to_owned()),
        StreamEvent::TokenUsage {
            input_tokens: None,
            output_tokens: Some(20),
            reasoning_tokens: Some(5),
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        },
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]);
    client.extract(request()).await.unwrap();
    let events = client.events.read_after("s", None).unwrap();
    let usage = &events.last().unwrap().properties["usage"];
    assert_eq!(usage["inputTokens"], 60);
    assert_eq!(usage["cacheReadInputTokens"], 30);
    assert_eq!(usage["cacheWriteInputTokens"], 10);
    assert_eq!(usage["outputTokens"], 20);
    assert_eq!(usage["reasoningTokens"], 5);
    assert_eq!(usage["totalTokens"], 120);
    assert_eq!(usage["accounted"], true);
}

#[tokio::test]
async fn a_rejected_model_journal_prevents_the_provider_request() {
    struct Denied;
    #[async_trait]
    impl zuno_learning::LearningModelJournal for Denied {
        async fn record(&self, _: zuno_learning::LearningModelRecord) -> zuno_learning::Result<()> {
            Err(zuno_memory::MemoryServiceError::Denied.into())
        }
    }
    let (mut client, requests) = client(Vec::new());
    client.journal = Arc::new(Denied);
    assert!(matches!(
        client.extract(request()).await,
        Err(zuno_learning::LearningServiceError::Memory(
            zuno_memory::MemoryServiceError::Denied
        ))
    ));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_stalled_model_journal_cannot_hold_the_worker_forever() {
    struct Stalled;
    #[async_trait]
    impl zuno_learning::LearningModelJournal for Stalled {
        async fn record(&self, _: zuno_learning::LearningModelRecord) -> zuno_learning::Result<()> {
            std::future::pending().await
        }
    }
    let (mut client, requests) = client(Vec::new());
    client.journal = Arc::new(Stalled);
    client.limits.execution_timeout_ms = 50;
    assert!(matches!(
        client.extract(request()).await,
        Err(zuno_learning::LearningServiceError::Journal { .. })
    ));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn learning_outcomes_can_be_journaled_without_a_sqlite_dependency() {
    struct Records(Mutex<Vec<zuno_learning::LearningModelRecord>>);
    #[async_trait]
    impl zuno_learning::LearningModelJournal for Records {
        async fn record(
            &self,
            record: zuno_learning::LearningModelRecord,
        ) -> zuno_learning::Result<()> {
            self.0.lock().unwrap().push(record);
            Ok(())
        }
    }
    let records = Arc::new(Records(Mutex::new(Vec::new())));
    let (mut client, requests) = client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
    client.model.headers.insert(
        "Authorization".to_owned(),
        "Bearer never-log-this-key".to_owned(),
    );
    client.journal = records.clone();
    client.extract(request()).await.unwrap();
    let recorded = records.0.lock().unwrap();
    assert_eq!(recorded.len(), 2);
    assert!(matches!(
        recorded[0].event,
        zuno_learning::LearningModelEvent::Request { .. }
    ));
    assert!(matches!(&recorded[1].event,
        zuno_learning::LearningModelEvent::Outcome { usage, .. } if !usage.accounted && usage.provider_attempts==1));
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(client.events.read_after("s", None).unwrap().is_empty());
    assert!(
        !serde_json::to_string(&*recorded)
            .unwrap()
            .contains("never-log-this-key")
    );
}

#[tokio::test]
async fn an_unrecorded_model_outcome_is_not_returned_as_success() {
    struct RefuseOutcome;
    #[async_trait]
    impl zuno_learning::LearningModelJournal for RefuseOutcome {
        async fn record(
            &self,
            record: zuno_learning::LearningModelRecord,
        ) -> zuno_learning::Result<()> {
            if matches!(
                record.event,
                zuno_learning::LearningModelEvent::Outcome { .. }
            ) {
                Err(zuno_memory::MemoryServiceError::Unavailable.into())
            } else {
                Ok(())
            }
        }
    }
    let (mut client, requests) = client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
    client.journal = Arc::new(RefuseOutcome);
    assert!(matches!(
        client.extract(request()).await,
        Err(zuno_learning::LearningServiceError::OutcomeJournal { provider: None, .. })
    ));
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "a journal failure must not replay the model"
    );
}

#[tokio::test]
async fn journal_failure_preserves_provider_retry_after_and_observed_failure_usage() {
    struct RefuseOutcome;
    #[async_trait]
    impl zuno_learning::LearningModelJournal for RefuseOutcome {
        async fn record(
            &self,
            record: zuno_learning::LearningModelRecord,
        ) -> zuno_learning::Result<()> {
            if matches!(
                record.event,
                zuno_learning::LearningModelEvent::Outcome { .. }
            ) {
                Err(zuno_memory::MemoryServiceError::Unavailable.into())
            } else {
                Ok(())
            }
        }
    }
    let delay = std::time::Duration::from_secs(37);
    let (mut denied, requests) = client(vec![Reply::Failure(
        zuno_error::ProviderError::RateLimited {
            retry_after: Some(delay),
        },
    )]);
    denied.journal = Arc::new(RefuseOutcome);
    let error = denied.extract(request()).await.unwrap_err();
    assert_eq!(
        error.recovery(),
        zuno_error::Recovery::Retry { after: Some(delay) }
    );
    assert_eq!(requests.lock().unwrap().len(), 1);

    let (client, _) = client(vec![Reply::Events(vec![
        StreamEvent::TokenUsage {
            input_tokens: Some(10),
            output_tokens: None,
            reasoning_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            accounting: zuno_llm::event::PromptAccounting::CacheInsideInput,
        },
        StreamEvent::Error {
            message: "rate limit".to_owned(),
            retry_after: Some(delay),
        },
    ])]);
    assert_eq!(
        client.extract(request()).await.unwrap_err().recovery(),
        zuno_error::Recovery::Retry { after: Some(delay) }
    );
    let events = client.events.read_after("s", None).unwrap();
    let last = &events.last().unwrap().properties;
    assert_eq!(last["status"], "failed");
    assert_eq!(last["usage"]["inputTokens"], 10);
    assert_eq!(last["usage"]["accounted"], false);
}

#[tokio::test]
async fn provider_rollback_discards_text_but_retains_observed_learning_usage() {
    use zuno_llm::event::PromptAccounting;
    let usage = |input, output, accounting| StreamEvent::TokenUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        reasoning_tokens: None,
        cache_read_input_tokens: Some(5),
        cache_write_input_tokens: Some(0),
        accounting,
    };
    let (client, _) = client(vec![Reply::Events(vec![
        usage(10, 2, PromptAccounting::CacheBesideInput),
        StreamEvent::TextDelta("discarded".to_owned()),
        StreamEvent::RetryRollback { attempt: 1, max: 2 },
        usage(20, 3, PromptAccounting::CacheInsideInput),
        StreamEvent::TextDelta(r#"{"experiences":[],"memories":[]}"#.to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ])]);
    client.extract(request()).await.unwrap();
    let events = client.events.read_after("s", None).unwrap();
    let outcome = &events.last().unwrap().properties;
    assert!(!outcome["output"].as_str().unwrap().contains("discarded"));
    assert_eq!(outcome["usage"]["inputTokens"], 25);
    assert_eq!(outcome["usage"]["cacheReadInputTokens"], 10);
    assert_eq!(outcome["usage"]["outputTokens"], 5);
    assert_eq!(outcome["usage"]["totalTokens"], 40);
    assert_eq!(outcome["usage"]["providerAttempts"], 2);
    assert_eq!(
        outcome["usage"]["accounted"], false,
        "the discarded attempt has observed counts but no terminal measurement"
    );
}

struct FixtureClient {
    inner: LearningModelClient,
    events: zuno_db::event_log::SessionEventLog,
}
impl std::ops::Deref for FixtureClient {
    type Target = LearningModelClient;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for FixtureClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
impl FixtureClient {
    fn set_events(&mut self, events: zuno_db::event_log::SessionEventLog) {
        self.inner.journal = Arc::new(zuno_learning::SqliteLearningModelJournal::new(
            events.clone(),
        ));
        self.events = events;
    }
}

fn client(replies: Vec<Reply>) -> (FixtureClient, Arc<Mutex<Vec<CompletionRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let events = zuno_db::event_log::SessionEventLog::new(pool());
    (
        FixtureClient {
            inner: LearningModelClient {
                provider: Arc::new(ScriptedProvider {
                    replies: Mutex::new(replies.into()),
                    requests: requests.clone(),
                }),
                model: LearningModel {
                    provider_id: "test".to_owned(),
                    model_id: "model".to_owned(),
                    wire_id: "model".to_owned(),
                    surface: ApiSurface::Chat,
                    parameters: Default::default(),
                    headers: Default::default(),
                    sampling_params: true,
                },
                journal: Arc::new(zuno_learning::SqliteLearningModelJournal::new(
                    events.clone(),
                )),
                limits: ResolvedLearningConfig::default(),
            },
            events,
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
async fn copying_the_model_visible_source_id_preserves_verified_evidence() {
    use zuno_db::{
        learning_job::{LearningJobStore, NewLearningJob},
        learning_source::LearningSourceStore,
    };
    use zuno_llm::event::RequestContentBlock;

    #[derive(Debug)]
    struct CopyingProvider {
        requests: Arc<Mutex<Vec<CompletionRequest>>>,
    }
    impl Provider for CopyingProvider {
        fn id(&self) -> &str {
            "test"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::text_only()
        }
        fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
            let RequestContentBlock::Text { text } = &request.messages[1].message().content[0]
            else {
                panic!("plain JSON input");
            };
            let input: serde_json::Value = serde_json::from_str(text).expect("source input");
            let evidence: Vec<_> = input["sources"]
                .as_array()
                .expect("sources")
                .iter()
                .map(|source| {
                    json!({
                        "kind": source["kind"],
                        "source_id": source["source_id"],
                        "excerpt": source["content"],
                    })
                })
                .collect();
            let output = json!({
                "experiences": [{
                    "kind":"user_correction", "title":"Concise reports",
                    "summary":"The user requested concise reports.",
                    "resolution":"Keep reports concise.", "confidence":0.9,
                    "evidence": evidence,
                }],
                "memories": [{
                    "experience_ordinal":0, "scope":"project", "action":"add",
                    "content":"Keep reports concise.", "old_text":null,
                    "reason":"An explicit user correction.", "confidence":0.9,
                }],
            });
            self.requests.lock().expect("requests").push(request);
            Box::pin(stream::iter([
                Ok(StreamEvent::TextDelta(output.to_string())),
                Ok(StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                }),
            ]))
        }
    }

    let pool = pool();
    pool.get()
        .expect("connection")
        .execute_batch(
            r#"INSERT INTO message(id,session_id,time_created,time_updated,data)
           VALUES('user','s',1,1,'{"role":"user"}');
           INSERT INTO part(id,message_id,session_id,time_created,time_updated,data)
           VALUES('user-part','user','s',1,1,
                  '{"type":"text","text":"Correction: keep reports concise."}'),
                 ('assistant-part','m','s',2,2,'{"type":"text","text":"Understood."}');"#,
        )
        .expect("source rows");
    let sources = LearningSourceStore::new(pool.clone())
        .for_turn("s", "m", &str::to_owned)
        .expect("closed sources")
        .sources;
    assert_eq!(sources.len(), 2);
    let mut input = request();
    input.sources = sources.clone();
    input.had_tool_calls = false;
    input.user_corrected = true;
    let jobs = LearningJobStore::new(pool.clone());
    jobs.enqueue(NewLearningJob::extraction(
        "citation-job",
        "p",
        "s",
        "m",
        zuno_learning::LEARNING_EXTRACTOR_VERSION,
        json!({"request": input}),
        10,
    ))
    .expect("admit");
    let lease = jobs
        .claim_due("worker", 11, 1000)
        .expect("claim")
        .expect("job")
        .lease()
        .expect("lease");
    let (mut client, _) = client(Vec::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    client.provider = Arc::new(CopyingProvider {
        requests: requests.clone(),
    });
    client.set_events(zuno_db::event_log::SessionEventLog::new(pool.clone()));
    let output = client.extract(input).await.expect("extract");
    let events = client.events.read_after("s", None).expect("receipts");
    let stored = ExperienceService::new(pool.clone(), None)
        .persist_extraction("citation-job", &lease, output, 20)
        .expect("persist");
    assert!(
        stored.experiences[0].verified_sources(),
        "copying the supplied source_id must cite the exact admitted source"
    );
    assert!(
        zuno_db::memory_evidence::MemoryEvidenceStore::new(pool)
            .get(&stored.experiences[0].projection.id)
            .expect("evidence")
            .is_some(),
        "the verified user correction can enter the independent Memory stage"
    );

    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    let RequestContentBlock::Text { text } = &requests[0].messages[1].message().content[0] else {
        panic!("plain JSON input");
    };
    let projected: serde_json::Value = serde_json::from_str(text).expect("projection");
    for (source, original) in projected["sources"]
        .as_array()
        .unwrap()
        .iter()
        .zip(&sources)
    {
        assert_eq!(source["source_id"], original.reference_id);
        for private_field in [
            "reference_id",
            "message_id",
            "source_digest",
            "content_digest",
        ] {
            assert!(source.get(private_field).is_none(), "{private_field}");
        }
    }
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].properties["request"]["messages"],
        serde_json::to_value(&requests[0].messages).expect("actual input"),
        "the durable receipt records the projection actually sent to the provider"
    );
    let manifest = jobs.get("citation-job").expect("job").payload.unwrap();
    assert_eq!(
        manifest["request"]["sources"],
        serde_json::to_value(&sources).expect("original manifest"),
        "model projection must not change the durable storage identities or digests"
    );
}

#[tokio::test]
async fn memory_consolidation_uses_the_audited_no_tools_provider_path() {
    use zuno_learning::{MemoryConsolidationRequest, MemoryConsolidator};
    let (mut client, requests) = client(vec![answer(r#"{"updates":[]}"#)]);
    client
        .model
        .parameters
        .insert(generation::MAX_TOKENS.to_owned(), json!(0));
    client.limits.execution_max_output_tokens = 512;
    let result = client
        .consolidate_memory(MemoryConsolidationRequest {
            project_id: "p".to_owned(),
            session_id: "s".to_owned(),
            scopes: vec![json!({"scope":"project","revision":1,"entries":[]})],
            experiences: vec![json!({"id":"evidence","summary":"Prefer concise reports."})],
            user_changes: Vec::new(),
            correction: None,
        })
        .await
        .expect("no-op consolidation");
    assert!(result.updates.is_empty());
    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].parameters[generation::MAX_TOKENS], 512);
    let events = client
        .events
        .read_after("s", None)
        .expect("durable request/outcome");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].properties["tools"], json!([]));
    assert!(
        events[0].properties["request"]
            .to_string()
            .contains("Prefer concise reports.")
    );
    assert_eq!(events[1].properties["output"], r#"{"updates":[]}"#);
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

#[tokio::test]
async fn learning_preserves_resolved_model_controls_headers_and_its_own_output_bound() {
    let (mut client, requests) = client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
    client.model.surface = ApiSurface::Responses;
    client.model.parameters = serde_json::from_value(json!({
        "reasoning":{"effort":"high"},"text":{"verbosity":"low"},
        "temperature":0.7,"max_output_tokens":9000
    }))
    .unwrap();
    client
        .model
        .headers
        .insert("x-model-revision".to_owned(), "chosen-revision".to_owned());
    client.model.sampling_params = false;
    client.limits.execution_structured_output = true;
    client.limits.execution_max_output_tokens = 512;
    client.extract(request()).await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.model_id, "model");
    assert_eq!(sent.surface, ApiSurface::Responses);
    assert_eq!(sent.parameters["reasoning"]["effort"], "high");
    assert_eq!(sent.parameters["text"]["verbosity"], "low");
    assert_eq!(sent.parameters["text"]["format"]["type"], "json_schema");
    assert_eq!(sent.parameters[generation::MAX_TOKENS], 512);
    assert!(sent.parameters.get("max_output_tokens").is_none());
    assert!(sent.parameters.get("temperature").is_none());
    assert_eq!(sent.headers["x-model-revision"], "chosen-revision");
    assert!(sent.tools.is_empty());
    assert_eq!(
        sent.request_context(),
        Some(&zuno_llm::registry::ProviderRequestContext::Learning)
    );
}

#[tokio::test]
async fn a_supported_explicit_sampling_setting_and_smaller_output_limit_are_preserved() {
    let (mut client, requests) = client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
    client.model.parameters =
        serde_json::from_value(json!({"temperature":0.65,"maxTokens":128})).unwrap();
    client.extract(request()).await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0].parameters["temperature"], 0.65);
    assert_eq!(requests[0].parameters[generation::MAX_TOKENS], 128);
}

#[tokio::test]
async fn learning_output_limit_zero_is_unspecified_but_the_captured_request_stays_bounded() {
    for alias in [
        "maxTokens",
        "max_tokens",
        "max_output_tokens",
        "max_completion_tokens",
    ] {
        for surface in [ApiSurface::Chat, ApiSurface::Responses] {
            for sampling_supported in [false, true] {
                let (mut client, requests) =
                    client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
                let spec = Spec::new(&client.model.provider_id)
                    .with_option(
                        "capabilities",
                        json!({"sampling_params": !sampling_supported}),
                    )
                    .with_option(
                        "modelCapabilities",
                        json!({(client.model.wire_id.clone()): {
                            "sampling_params": sampling_supported
                        }}),
                    );
                let capabilities = model_capabilities(
                    &spec,
                    &client.model.wire_id,
                    client.provider.capabilities(),
                );
                assert_eq!(capabilities.sampling_params, sampling_supported);
                client.model.sampling_params = capabilities.sampling_params;
                client.model.surface = surface;
                client.model.parameters.insert(alias.to_owned(), json!(0));
                client
                    .model
                    .parameters
                    .insert(generation::TEMPERATURE.to_owned(), json!(0.7));
                client.limits.execution_max_output_tokens = 512;

                let result = client.extract(request()).await;
                assert!(
                    result.is_ok(),
                    "{alias}=0 must mean no additional model cap: {result:?}"
                );
                let requests = requests.lock().expect("captured requests");
                assert_eq!(requests.len(), 1, "zero must not cause a paid repair");
                let sent = &requests[0];
                assert_eq!(sent.model_id, client.model.wire_id);
                assert_eq!(sent.surface, surface);
                assert_eq!(sent.parameters[generation::MAX_TOKENS], 512);
                assert!(sent.tools.is_empty());
                assert_eq!(
                    sent.parameters.get(generation::TEMPERATURE),
                    sampling_supported.then_some(&json!(0.7)),
                );

                let events = client.events.read_after("s", None).expect("audit events");
                assert_eq!(events.len(), 2);
                let recorded = &events[0].properties["request"]["parameters"];
                assert_eq!(recorded[generation::MAX_TOKENS], 512);
                for other in ["max_tokens", "max_output_tokens", "max_completion_tokens"] {
                    assert!(sent.parameters.get(other).is_none());
                    assert!(recorded.get(other).is_none());
                }
            }
        }
    }
}

#[tokio::test]
async fn learning_output_limit_reaches_the_wire_under_the_resolved_surface_key() {
    for (surface, wire_key) in [
        (ApiSurface::Chat, "max_tokens"),
        (ApiSurface::Responses, "max_output_tokens"),
        (ApiSurface::Messages, "max_tokens"),
    ] {
        let (mut client, requests) = client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
        client.model.surface = surface;
        client
            .model
            .parameters
            .insert(generation::MAX_TOKENS.to_owned(), json!(0));
        client.limits.execution_max_output_tokens = 512;
        client.extract(request()).await.expect("bounded extraction");
        let requests = requests.lock().expect("captured request");
        assert_eq!(requests.len(), 1);
        // The compatible provider calls this same lowering after assembling its body.
        let mut body = json!({});
        requests[0].apply_parameters(&mut body, surface);
        assert_eq!(
            body[wire_key], 512,
            "wire output must keep the execution cap"
        );
        assert!(body.get(generation::MAX_TOKENS).is_none());
        for other in ["max_tokens", "max_output_tokens", "max_completion_tokens"] {
            if other != wire_key {
                assert!(body.get(other).is_none(), "unexpected wire alias {other}");
            }
        }
    }
}

#[tokio::test]
async fn learning_output_limit_positive_caps_still_win_over_unspecified_aliases() {
    let (mut client, requests) = client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
    client.model.parameters = serde_json::from_value(json!({
        "maxTokens": 0, "max_tokens": 128,
        "max_output_tokens": 4096, "max_completion_tokens": 0
    }))
    .expect("model options");
    client.limits.execution_max_output_tokens = 512;
    let result = client.extract(request()).await;
    assert!(
        result.is_ok(),
        "zero must not override a positive cap: {result:?}"
    );
    let requests = requests.lock().expect("captured request");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].parameters[generation::MAX_TOKENS], 128);
    for alias in ["max_tokens", "max_output_tokens", "max_completion_tokens"] {
        assert!(requests[0].parameters.get(alias).is_none());
    }
}

#[tokio::test]
async fn learning_output_limit_rejects_negative_fractional_boolean_and_non_numeric_values() {
    for alias in [
        "maxTokens",
        "max_tokens",
        "max_output_tokens",
        "max_completion_tokens",
    ] {
        for value in [
            json!(-1),
            json!(0.5),
            json!(1.5),
            json!(true),
            json!(false),
            json!("0"),
            json!(null),
        ] {
            let (mut client, requests) =
                client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
            client
                .model
                .parameters
                .insert(alias.to_owned(), value.clone());
            let result = client.extract(request()).await;
            assert!(result.is_err(), "{alias}={value} must remain invalid");
            assert_eq!(result.unwrap_err().recovery(), zuno_error::Recovery::Fail);
            assert!(requests.lock().expect("captured requests").is_empty());
            assert!(
                client
                    .events
                    .read_after("s", None)
                    .expect("events")
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn learning_output_limit_requires_a_positive_execution_bound_before_requesting() {
    let (mut client, requests) = client(vec![answer(r#"{"experiences":[],"memories":[]}"#)]);
    client.limits.execution_max_output_tokens = 0;
    let result = client.extract(request()).await;
    assert!(
        result.is_err(),
        "execution bound zero must not reach the provider"
    );
    assert_eq!(result.unwrap_err().recovery(), zuno_error::Recovery::Fail);
    assert!(requests.lock().expect("captured requests").is_empty());
    assert!(
        client
            .events
            .read_after("s", None)
            .expect("events")
            .is_empty()
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
        client: client.inner,
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
    client.set_events(zuno_db::event_log::SessionEventLog::new(pool.clone()));
    let service = zuno_learning::ProjectLearningService {
        scheduler,
        extractor: Arc::new(client.inner),
        experiences: ExperienceService::new(pool.clone(), None),
        patterns: PatternMiner::new(pool.clone(), settings.clone()),
        skills: zuno_learning::SkillCandidateService::new(pool.clone(), settings),
        memory: None,
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
