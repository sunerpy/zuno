//! Real TurnHost/default-driver regressions with deterministic provider streams.
//! Every database and workspace belongs to the fixture; no network provider runs.

use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;
use zuno_db::completion_delivery::{CompletionDeliveryStore, CompletionOwner};
use zuno_db::inbox::{InputDelivery, NewSessionInput, SubmissionState};
use zuno_engine::report::ReportBatch;
use zuno_llm::event::FinishReason;
use zuno_tool::question::QuestionPort as _;
use zuno_types::execution::{
    CompletionEnvelope, CompletionSource, DraftReviewRiskAcceptance, InputTriggerKind,
    SessionExecutionPhase, SessionExecutionState, SessionPauseReason,
};
use zuno_types::question::{
    PlanAuthorizationState, PlanQuestionDecision, QuestionAction, QuestionCommand, QuestionMode,
    QuestionOrigin, QuestionPrompt, QuestionPurpose, QuestionSpec, QuestionState,
};

#[derive(Debug)]
struct SchedulingProvider {
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl SchedulingProvider {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.requests.lock().expect("requests").len()
    }
}

impl Provider for SchedulingProvider {
    fn id(&self) -> &str {
        COMPATIBLE_PROVIDER
    }

    fn capabilities(&self) -> zuno_llm::registry::Capabilities {
        zuno_llm::registry::Capabilities {
            tool_calls: true,
            ..zuno_llm::registry::Capabilities::text_only()
        }
    }

    fn stream(&self, request: CompletionRequest) -> zuno_llm::registry::ProviderStream<'_> {
        self.requests.lock().expect("requests").push(request);
        let events = self
            .scripts
            .lock()
            .expect("provider script")
            .pop_front()
            .expect(
                "unexpected extra provider request: scheduling reopened completed or gated work",
            );
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }
}

fn final_response(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.to_owned()),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::Stop),
        },
    ]
}

fn plan_exit_request() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: "call_plan_exit".to_owned(),
            name: "plan_exit".to_owned(),
        },
        StreamEvent::ToolInputDelta {
            id: "call_plan_exit".to_owned(),
            delta: "{}".to_owned(),
        },
        StreamEvent::ToolUseEnd {
            id: "call_plan_exit".to_owned(),
        },
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
    ]
}

async fn mock_provider_host(
    agent_name: &str,
    scripts: Vec<Vec<StreamEvent>>,
) -> (
    tempfile::TempDir,
    TurnHost,
    Arc<SchedulingProvider>,
    zuno_tools::WorkStateStore,
) {
    let directory = tempfile::tempdir().expect("isolated workspace");
    let session_id = format!("ses_scheduling_{}", Uuid::now_v7().simple());
    let config = zuno_config::schema::Config::default();
    let mut entry = agent(agent_name);
    entry.tools = Some(vec![zuno_tools::plan_exit::WIRE_ID.to_owned()]);
    let profile = agent_profile(entry.clone(), directory.path(), &config);
    let mut turn_plan = plan_for(
        directory.path().to_str().expect("workspace"),
        SessionChoice::Existing(session_id.clone()),
        entry,
        profile,
        config,
    );
    let model = || {
        EngineModel::new(
            Spec::new(COMPATIBLE_PROVIDER)
                .with_surface(ApiSurface::Chat)
                .with_base_url("http://127.0.0.1:9/v1"),
            "model",
            ApiSurface::Chat,
        )
    };
    turn_plan.resolver.spec = model().provider;
    turn_plan.internals.title.model = model();
    turn_plan.internals.compaction.model = model();
    turn_plan.internals.summary.model = model();
    turn_plan.internals.council_synth.model = model();
    turn_plan.resolver.orchestration_seed = Some(Arc::new(AttemptSeed {
        capability: turn_plan.capability.as_ref().clone(),
        agent: agent_attempt_identity(&turn_plan.agent, turn_plan.tool_authority.as_deref())
            .expect("resolved Agent identity"),
        preset: None,
        subagent_model_policy_sha256: turn_plan.subagent_model_policy.digest().to_owned(),
        parent_attempt: None,
        workflow: None,
        workflow_node: None,
        parent_authority: None,
        cycle_id: None,
    }));
    turn_plan.credential = Some(zuno_auth::Credential::Api {
        key: zuno_auth::Secret::new("scheduling-test-key"),
        metadata: None,
    });
    let database =
        Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("test database"));
    {
        let mut connection = database.open_connection().expect("connection");
        zuno_db::migration::apply(&mut connection).expect("schema");
        let now = zuno_db::message::now_millis();
        ensure_project(&connection, &turn_plan.project, now).expect("project");
        let transaction = connection.transaction().expect("transaction");
        let mut session = zuno_db::session::SessionCreate::new(
            &session_id,
            &session_id,
            &turn_plan.project.id,
            directory.path().to_string_lossy(),
            directory.path().to_string_lossy(),
            "Scheduling regression",
            crate::RUST_PACKAGE_VERSION,
        )
        .at(now);
        session.agent = Some(agent_name.to_owned());
        session.model = Some(zuno_db::session::model_reference_with_variant(
            "provider", "model", None,
        ));
        zuno_db::session::create(&transaction, &session).expect("session");
        transaction.commit().expect("commit session");
    }
    let work = zuno_tools::WorkStateStore::new(Arc::clone(&database));
    let environment = crate::environment::StartupEnvironment::resolve(
        &Env::empty(),
        &crate::GlobalOptions::default(),
    );
    let runs = SessionRunRegistry::new();
    let question_port =
        Arc::new(QuestionService::new(Arc::clone(&database)).with_runs(runs.clone()));
    let mut host = TurnHost::open_with_dependencies(
        turn_plan,
        &environment,
        TurnHostDependencies {
            approval: Arc::new(zuno_tool::AllowAll),
            question: Some(question_port),
            runs,
            mcp: None,
            database,
            child_observer: None,
            detached_observer: None,
        },
    )
    .await
    .expect("native TurnHost");
    let provider = Arc::new(SchedulingProvider::new(scripts));
    let mut providers = ProviderRegistry::new();
    providers.register(COMPATIBLE_PROVIDER, {
        let provider = Arc::clone(&provider);
        move |_| provider.clone() as Arc<dyn Provider>
    });
    host.providers = providers;
    (directory, host, provider, work)
}

async fn drive_user(host: &mut TurnHost, prompt: &str) -> Vec<TurnEvent> {
    let (sender, receiver) = zuno_engine::r#loop::event_channel();
    let (result, events) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        tokio::join!(host.drive(prompt, sender), collect_turn_events(receiver))
    })
    .await
    .expect("the scripted user turn must terminate");
    result.expect("user turn");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnFailed { .. })),
        "{events:#?}"
    );
    events
}

fn execution(host: &TurnHost) -> SessionExecutionState {
    host.session_control
        .state(&host.session_id)
        .expect("read execution")
        .expect("execution state")
}

fn final_messages(host: &TurnHost) -> i64 {
    host.connection
        .query_row(
            "SELECT count(*) FROM message WHERE session_id=?1
         AND json_extract(data,'$.role')='assistant' AND json_extract(data,'$.finish')='stop'",
            [&host.session_id],
            |row| row.get(0),
        )
        .expect("durable final assistant messages")
}

fn turn_starts(host: &TurnHost) -> i64 {
    host.connection
        .query_row(
            "SELECT count(*) FROM event WHERE aggregate_id=?1 AND type='session.turn.started.1'",
            [&host.session_id],
            |row| row.get(0),
        )
        .expect("durable turn starts")
}

fn completed_events(events: &[TurnEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, TurnEvent::TurnCompleted { .. }))
        .count()
}

async fn deliver_exact_cycle_callback(host: &mut TurnHost) -> Vec<TurnEvent> {
    let cycle = execution(host).cycle_id.expect("original work cycle");
    let now = zuno_db::message::now_millis();
    let source_key = format!("background:bg_scheduling:{now}");
    let payload = json!({
        "kind":"backgroundExecutionReport",
        "executionID":"bg_scheduling",
        "status":"completed",
        "text":"The external observer completed."
    });
    let deliveries = CompletionDeliveryStore::new(Arc::clone(&host.database));
    deliveries
        .publish(
            CompletionEnvelope {
                source_key: source_key.clone(),
                source: CompletionSource::BackgroundExecution,
                terminal_revision: 1,
                parent_session_id: host.session_id.clone(),
                cycle_id: Some(cycle.clone()),
                payload: payload.clone(),
            },
            now,
        )
        .expect("persist completion receipt");
    let (receipt, admitted) = deliveries
        .claim_callback(
            &source_key,
            NewSessionInput::new(
                "callback-scheduling",
                &host.session_id,
                payload,
                InputDelivery::Queue,
                now,
            )
            .with_source_key(&source_key)
            .with_trigger_kind(InputTriggerKind::Automatic)
            .with_cycle_id(Some(cycle.as_str())),
            now,
        )
        .expect("callback admission")
        .expect("callback claims completion");
    assert_eq!(receipt.owner, Some(CompletionOwner::Callback));
    assert_eq!(admitted.state, SubmissionState::Queued);
    assert!(
        host.inbox
            .promote_id(&host.session_id, &admitted.id)
            .expect("promote callback")
            .is_none(),
        "paused callback stays durable but cannot be promoted"
    );
    let promoted = admitted.clone();
    assert_eq!(
        zuno_db::session_wake::signal_in(&host.connection, &promoted).expect("wake identity"),
        Some(SessionWakeSignal::ExternalCompletion {
            source_id: "bg_scheduling".to_owned(),
            origin_cycle_id: cycle,
        }),
        "the callback carries exact execution identity, independently of its receipt key"
    );
    let reports = ReportBatch::project(&[promoted]);
    assert!(reports.undecodable().is_empty());
    assert_eq!(reports.reports().len(), 1);
    let guard = host
        .runs
        .begin_turn(host.session_id.clone())
        .expect("callback lease");
    let (sender, receiver) = zuno_engine::r#loop::event_channel();
    let (result, events) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        tokio::join!(
            host.drive_promoted_reports_with_guard(reports.reports(), &guard, sender),
            collect_turn_events(receiver)
        )
    })
    .await
    .expect("suppressed callback terminates without a provider");
    result.expect("host records the callback without driving gated work");
    assert_eq!(
        host.inbox
            .get(&host.session_id, &admitted.id)
            .expect("input")
            .expect("durable input")
            .state,
        SubmissionState::Queued
    );
    events
}

#[tokio::test]
async fn ordinary_incomplete_plan_without_goal_pauses_after_one_provider_response() {
    let (_directory, mut host, provider, work) = mock_provider_host(
        "build",
        vec![
            final_response("The implementation summary is recorded."),
            final_response("The recorded status is unchanged."),
        ],
    )
    .await;
    let before = seed_scripted_plan(&work, &host.session_id, false);
    assert!(
        host.goal_store
            .goal(&host.session_id)
            .expect("Goal")
            .is_none()
    );
    assert!(before.items.is_empty());
    let events = drive_user(&mut host, "Summarize the current implementation.").await;
    assert_eq!(provider.calls(), 1);
    assert_eq!(completed_events(&events), 1);
    assert_eq!(final_messages(&host), 1);
    assert_eq!(turn_starts(&host), 1);
    assert!(
        host.goal_store
            .goal(&host.session_id)
            .expect("Goal")
            .is_none()
    );
    assert_eq!(work.snapshot(&host.session_id).expect("work"), before);
    let paused = execution(&host);
    assert_eq!(paused.mode, CollaborationMode::Work);
    assert_eq!(paused.phase, SessionExecutionPhase::Paused);
    let progress = paused.scheduling.as_ref().expect("scheduling");
    assert_eq!(
        progress.readiness,
        SessionReadiness::Paused {
            reason: SessionPauseReason::NoExecutableWork
        }
    );
    assert_eq!(progress.unchanged_progress_count, 1);
    assert!(progress.progress_fingerprint.is_some());
    let phase = host
        .plan_reconciliation
        .projection(&host.session_id)
        .expect("phase")
        .expect("driver");

    let callback_events = deliver_exact_cycle_callback(&mut host).await;
    assert_eq!(provider.calls(), 1);
    assert_eq!(completed_events(&callback_events), 0);
    assert_eq!(final_messages(&host), 1);
    assert_eq!(turn_starts(&host), 1);
    assert_eq!(execution(&host), paused);
    assert_eq!(
        host.plan_reconciliation
            .projection(&host.session_id)
            .expect("phase"),
        Some(phase.clone())
    );

    let query_events = drive_user(&mut host, "What is the current status?").await;
    assert_eq!(
        provider.calls(),
        2,
        "one explicit query, without an automatic follow-up"
    );
    assert_eq!(completed_events(&query_events), 1);
    assert_eq!(final_messages(&host), 2);
    let after_query = execution(&host);
    assert_eq!(after_query.phase, SessionExecutionPhase::Paused);
    assert_eq!(after_query.scheduling, paused.scheduling);
    assert_eq!(after_query.cycle_id, paused.cycle_id);
    assert_eq!(after_query.work_identity, paused.work_identity);
    assert_eq!(
        host.plan_reconciliation
            .projection(&host.session_id)
            .expect("phase"),
        Some(phase)
    );
    host.shutdown().await.expect("shutdown fixture");
}

#[tokio::test]
async fn plan_authorization_source_turn_finishes_summary_and_requires_matching_explicit_approval() {
    let (_directory, mut host, provider, work) = mock_provider_host(
        "plan",
        vec![
            plan_exit_request(),
            final_response("The plan summary is complete and its approval request is available."),
        ],
    )
    .await;
    let before = seed_scripted_plan(&work, &host.session_id, true);
    assert!(
        host.dispatcher
            .available_tools()
            .definitions
            .iter()
            .any(|tool| tool.id == "plan_exit")
    );
    let events = drive_user(
        &mut host,
        "Finish the implementation plan and request my approval.",
    )
    .await;
    assert_eq!(
        provider.calls(),
        2,
        "one plan_exit tool request and one final summary"
    );
    assert_eq!(completed_events(&events), 1);
    assert_eq!(final_messages(&host), 1);
    assert_eq!(turn_starts(&host), 1);
    assert_eq!(work.snapshot(&host.session_id).expect("work"), before);
    assert!(
        host.goal_store
            .goal(&host.session_id)
            .expect("Goal")
            .is_none()
    );
    let requests = host
        .questions
        .pending(&host.session_id)
        .await
        .expect("pending questions");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.purpose, QuestionPurpose::PlanAuthorization);
    assert_eq!(request.state, QuestionState::Pending);
    assert!(request.decision.is_none());
    let waiting = execution(&host);
    assert_eq!(waiting.mode, CollaborationMode::Plan);
    assert_eq!(waiting.phase, SessionExecutionPhase::Waiting);
    assert_eq!(
        waiting.scheduling.as_ref().expect("scheduling").readiness,
        SessionReadiness::WaitingHuman {
            request_id: request.id.clone()
        }
    );
    assert_eq!(waiting.authorized_plan_id, None);
    let plan = before.plan.as_ref().expect("Plan");
    assert_eq!(waiting.handoff_plan_id.as_deref(), Some(plan.id.as_str()));
    assert_eq!(waiting.handoff_plan_revision, Some(plan.revision));
    assert!(
        zuno_db::question::handoff_completed_in(&host.connection, &request.id)
            .expect("source turn handed off")
    );
    let phase = host
        .plan_reconciliation
        .projection(&host.session_id)
        .expect("phase")
        .expect("driver");
    assert_eq!(phase.phase, zuno_engine::plan_driver::DriverPhase::Terminal);
    assert_eq!(phase.reason.as_deref(), Some("planning_handoff_ready"));

    let callback_events = deliver_exact_cycle_callback(&mut host).await;
    assert_eq!(provider.calls(), 2);
    assert_eq!(completed_events(&callback_events), 0);
    assert_eq!(execution(&host), waiting);
    assert_eq!(turn_starts(&host), 1);
    assert!(
        host.questions
            .apply(
                &host.session_id,
                "unrelated-request",
                QuestionCommand {
                    command_id: "wrong-approval".to_owned(),
                    expected_revision: request.revision,
                    action: QuestionAction::PlanDecision {
                        decision: PlanQuestionDecision::Approve,
                        risk_reason: None
                    },
                },
            )
            .await
            .is_err()
    );
    assert_eq!(execution(&host), waiting);
    let deferred = host
        .questions
        .apply(
            &host.session_id,
            &request.id,
            QuestionCommand {
                command_id: "defer-approval".to_owned(),
                expected_revision: request.revision,
                action: QuestionAction::Defer {
                    draft_answers: Default::default(),
                },
            },
        )
        .await
        .expect("deferral is not approval");
    assert_eq!(execution(&host), waiting);
    let approved = host
        .questions
        .apply(
            &host.session_id,
            &request.id,
            QuestionCommand {
                command_id: "explicit-approval".to_owned(),
                expected_revision: deferred.question.revision,
                action: QuestionAction::PlanDecision {
                    decision: PlanQuestionDecision::Approve,
                    risk_reason: None,
                },
            },
        )
        .await
        .expect("explicit approval of the exact handed-off Plan");
    assert_eq!(
        approved.question.authorization,
        Some(PlanAuthorizationState::Applied)
    );
    let authorized = execution(&host);
    assert_eq!(authorized.mode, CollaborationMode::Work);
    assert_eq!(
        authorized.authorized_plan_id.as_deref(),
        Some(plan.id.as_str())
    );
    assert_eq!(authorized.authorized_plan_revision, Some(plan.revision));
    assert_eq!(
        authorized.scheduling.expect("scheduling").readiness,
        SessionReadiness::Ready
    );
    let control = host
        .inbox
        .get(
            &host.session_id,
            approved.input_id.as_deref().expect("queued Work control"),
        )
        .expect("control")
        .expect("durable control");
    assert_eq!(control.prompt["control"], "start_work");
    assert_eq!(control.state, SubmissionState::Queued);
    assert_eq!(
        provider.calls(),
        2,
        "approval queues Work; the originating Plan host does not execute it"
    );
    host.shutdown().await.expect("shutdown fixture");
}

#[tokio::test]
async fn goalless_pending_question_survives_durable_context_rebuild_without_assistant_prose() {
    let (_directory, mut host, provider, _work) = mock_provider_host("build", Vec::new()).await;
    let now = zuno_db::message::now_millis();
    host.session_control
        .record_continuation(
            &host.session_id,
            "question-context-cycle",
            host.current_turn_identity(),
            CollaborationMode::Work,
            None,
            None,
            None,
            now,
        )
        .expect("bind ordinary Work context");
    let mut current = execution(&host);
    let scheduling = current.scheduling.as_mut().expect("scheduling");
    scheduling.progress_fingerprint = Some("sha256:context-progress".to_owned());
    scheduling.unchanged_progress_count = 2;
    current.draft_review_risk = Some(DraftReviewRiskAcceptance {
        review_id: "review-context".to_owned(),
        review_revision: 1,
        reason: "FREEFORM_RISK_MUST_NOT_ENTER_BOUNDED_CONTEXT".repeat(400),
        time_accepted: now,
    });
    host.database
        .transaction(|tx| zuno_db::session_execution::update_in(tx, current.revision, current))
        .expect("durable execution metadata");
    let request = host
        .questions
        .open(QuestionSpec {
            origin: QuestionOrigin {
                session_id: host.session_id.clone(),
                message_id: None,
                call_id: None,
                turn_id: Some("question-context-turn".to_owned()),
                goal_id: None,
            },
            mode: QuestionMode::Blocking,
            purpose: QuestionPurpose::RequiredInput,
            questions: vec![
                QuestionPrompt::new("Which artifact should be used?", "Artifact", Vec::new())
                    .into_request(),
            ],
            expected_goal_revision: None,
            plan: None,
        })
        .await
        .expect("ordinary required input without a Goal");
    assert!(
        host.goal_store
            .goal(&host.session_id)
            .expect("Goal")
            .is_none()
    );
    assert_eq!(provider.calls(), 0);
    let rendered = durable_work_context(&host.connection, &host.session_id)
        .expect("render durable context")
        .expect("waiting state is model-visible");
    assert!(rendered.len() <= DURABLE_WORK_CONTEXT_MAX_BYTES);
    assert!(!rendered.contains("FREEFORM_RISK_MUST_NOT_ENTER_BOUNDED_CONTEXT"));
    let snapshot: Value = serde_json::from_str(rendered.lines().last().expect("snapshot JSON"))
        .expect("typed context");
    assert_eq!(snapshot["schemaVersion"], 3);
    assert_eq!(snapshot["execution"]["mode"], "work");
    assert_eq!(snapshot["execution"]["phase"], "waiting");
    assert_eq!(snapshot["execution"]["cycleId"], "question-context-cycle");
    assert_eq!(
        snapshot["execution"]["scheduling"]["readiness"]["kind"],
        "waiting_human"
    );
    assert_eq!(
        snapshot["execution"]["scheduling"]["readiness"]["requestId"],
        request.question.id
    );
    assert_eq!(
        snapshot["execution"]["scheduling"]["progressFingerprint"],
        "sha256:context-progress"
    );
    assert_eq!(
        snapshot["execution"]["scheduling"]["unchangedProgressCount"],
        2
    );
    assert_eq!(snapshot["questions"][0]["id"], request.question.id);
    assert_eq!(snapshot["questions"][0]["purpose"], "required_input");
    assert_eq!(snapshot["questions"][0]["state"], "pending");
    assert_eq!(snapshot["questions"][0]["header"], "Artifact");
    assert_eq!(snapshot["omittedQuestions"], 0);
    assert!(snapshot["plan"].is_null());
    let messages: i64 = host
        .connection
        .query_row(
            "SELECT count(*) FROM message WHERE session_id=?1",
            [&host.session_id],
            |row| row.get(0),
        )
        .expect("no assistant prose");
    assert_eq!(messages, 0);
    let reopened = host.database.open_connection().expect("fresh connection");
    assert_eq!(
        durable_work_context(&reopened, &host.session_id)
            .expect("rebuild")
            .as_deref(),
        Some(rendered.as_str()),
        "rebuild reads durable state without relying on a live question or prior prose"
    );
    host.shutdown().await.expect("shutdown fixture");
}
