//! Actual TurnHost regressions: the default driver, real Shell and no live provider.
#![cfg(unix)]

use super::*;
use serde_json::Map;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use zuno_catalog::agent::{Agent, AgentMode, AgentSource};
use zuno_llm::event::FinishReason;
use zuno_llm::registry::{Capabilities, CompletionRequest, ProviderStream};
use zuno_paths::Env;

fn config_json(source: &str) -> zuno_config::schema::Config {
    zuno_config::schema::Config::from_json_str(Path::new("zuno.json"), source).expect("test config")
}

fn agent(name: &str) -> Agent {
    Agent {
        name: name.to_owned(),
        description: None,
        mode: AgentMode::All,
        hidden: None,
        model: None,
        variant: None,
        reasoning: None,
        temperature: None,
        top_p: None,
        color: None,
        prompt: None,
        steps: None,
        tools: None,
        delegates: None,
        required_skills: None,
        options: Map::new(),
        permission: None,
        source: AgentSource::Native,
    }
}

fn agent_profile(
    entry: Agent,
    directory: &Path,
    config: &zuno_config::schema::Config,
) -> zuno_agent::profile::AgentProfile {
    let dynamic = crate::cmd::agent::DynamicRules::resolve(directory, None, &Env::empty(), config);
    crate::cmd::agent::resolved_profile(entry, config, &dynamic, false)
}

fn plan_for(
    directory: &str,
    session: SessionChoice,
    agent: Agent,
    profile: zuno_agent::profile::AgentProfile,
    config: zuno_config::schema::Config,
) -> TurnPlan {
    let directory = PathBuf::from(directory);
    let skills = Arc::new(zuno_catalog::skill::Skills::default());
    let internal = |name: &str| InternalAgent {
        name: name.to_owned(),
        prompt: String::new(),
        model: EngineModel::new(
            Spec::new(COMPATIBLE_PROVIDER)
                .with_surface(ApiSurface::Chat)
                .with_base_url("http://127.0.0.1:9/v1"),
            "model",
            ApiSurface::Chat,
        ),
    };
    TurnPlan {
        profile: zuno_harness::default_profile(),
        resolver: Resolver {
            requested_agent: agent.name.clone(),
            system_prompt: String::new(),
            prompt_assembly: None,
            runtime_prompt_policy: RuntimePromptPolicy::default(),
            max_steps: None,
            requested_provider: "provider".to_owned(),
            requested_model: "model".to_owned(),
            wire_model: "model".to_owned(),
            spec: Spec::new(COMPATIBLE_PROVIDER).with_surface(ApiSurface::Chat),
            reasoning_options: Map::new(),
            orchestration_seed: None,
        },
        catalog_models: Vec::new(),
        reasoning_efforts: BTreeMap::new(),
        subagent_model_policy: zuno_tools::task::SubagentModelPolicy::default(),
        skills: skills.clone(),
        skill_catalog: zuno_catalog::skill::catalog::SkillCatalogService::fixed(skills),
        required_skill_names: Vec::new(),
        capability: Arc::new(CapabilitySnapshot::new(
            PackIdentity {
                id: zuno_orchestration::PACK_ID.to_owned(),
                version: zuno_orchestration::PACK_VERSION.to_owned(),
                upstream_revision: zuno_orchestration::CAPABILITY_REVIEW_REVISION.to_owned(),
            },
            0,
            zuno_orchestration::sha256_text("foreground fixture"),
            CapabilityContents::default(),
        )),
        tool_authority: None,
        agents: vec![agent],
        extensions: zuno_extension::ResolvedExtensions::default(),
        configured_extension_tool_ids: Vec::new(),
        extension_scope: zuno_extension::Scope::new(&directory),
        extension_revision: 0,
        extension_transaction: None,
        extension_prepared: None,
        instructions: zuno_config::LoadedInstructions::default(),
        delegation_facts: Arc::new(zuno_tools::task::FixedFacts::new()),
        vision_available: false,
        reasoning_supported: false,
        is_delegated: false,
        effort: None,
        effective_variant: None,
        effort_override: None,
        variant_override: None,
        thinking_override: false,
        goal_retry_policy: GoalRetryPolicy::default(),
        project: zuno_paths::project::ResolvedProject {
            previous: None,
            id: "foreground-project".to_owned(),
            directory: directory.clone(),
            vcs: None,
        },
        env: Env::empty(),
        config,
        agent: profile,
        provider_id: "provider".to_owned(),
        model_id: "model".to_owned(),
        model_override: None,
        preset_override: None,
        auth_store: AuthStore::new(directory.join(".fixture-auth")),
        credential: None,
        session,
        title: None,
        internals: Internals {
            title: internal("title"),
            compaction: internal("compaction"),
            summary: internal("summary"),
            council_synth: internal("council-synth"),
        },
        presets: PresetLibrary::new(),
        learning_model: None,
        window: TokenWindow {
            context: 0,
            max_output: 0,
        },
        notes: Vec::new(),
        directory,
    }
}

#[derive(Debug)]
struct ForegroundProvider {
    calls: AtomicUsize,
    requests: Mutex<Vec<CompletionRequest>>,
    max_requests: usize,
    release_gate: std::fs::File,
}

impl ForegroundProvider {
    fn release(&self) {
        // A fixture-owned read/write FIFO keeps a failed assertion from blocking
        // forever while opening a writer. The command consumes one newline.
        (&self.release_gate)
            .write_all(b"finish\n")
            .expect("release fixture command");
    }
}

impl Drop for ForegroundProvider {
    fn drop(&mut self) {
        let _released = (&self.release_gate).write_all(b"finish\n");
    }
}

impl Provider for ForegroundProvider {
    fn id(&self) -> &str {
        COMPATIBLE_PROVIDER
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().expect("requests").push(request);
        let response = if call == 0 {
            vec![
                StreamEvent::ToolUseStart {
                    id: "host_foreground_call".to_owned(),
                    name: "shell".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "host_foreground_call".to_owned(),
                    delta: json!({
                        "command":"IFS= read -r line <release && cat result",
                        "timeout":10,"exitPolicy":"all"
                    })
                    .to_string(),
                },
                StreamEvent::ToolUseEnd {
                    id: "host_foreground_call".to_owned(),
                },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
            ]
        } else {
            assert!(
                call < self.max_requests,
                "an unchanged foreground wait must never poll the model"
            );
            vec![
                StreamEvent::TextDelta(
                    "The recorded command outcome has been inspected.".to_owned(),
                ),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ]
        };
        Box::pin(futures::stream::iter(response.into_iter().map(Ok)))
    }
}

async fn foreground_host() -> (tempfile::TempDir, TurnHost, Arc<ForegroundProvider>) {
    foreground_host_with_requests(2).await
}

async fn foreground_host_with_requests(
    max_requests: usize,
) -> (tempfile::TempDir, TurnHost, Arc<ForegroundProvider>) {
    let directory = tempfile::tempdir().expect("foreground host workspace");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(directory.path().join("release"))
            .status()
            .expect("fixture gate")
            .success()
    );
    let release_gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.path().join("release"))
        .expect("fixture gate owner");
    std::fs::write(directory.path().join("result"), b"foreground-final")
        .expect("fixture command output");
    let session_id = "ses_foreground_host";
    let config = config_json(
        r#"{
        "shell":"/bin/bash","sandbox":{"mode":"danger-full-access"},
        "learning":{"generate":false,"use":false}
    }"#,
    );
    let mut entry = agent("build");
    entry.tools = Some(vec!["shell".to_owned(), "bg".to_owned()]);
    let profile = agent_profile(entry.clone(), directory.path(), &config);
    let mut plan = plan_for(
        directory.path().to_str().expect("path"),
        SessionChoice::Existing(session_id.to_owned()),
        entry,
        profile,
        config,
    );
    plan.resolver.spec = Spec::new(COMPATIBLE_PROVIDER)
        .with_surface(ApiSurface::Chat)
        .with_base_url("http://127.0.0.1:9/v1");
    plan.credential = Some(Credential::Api {
        key: zuno_auth::Secret::new("foreground-test-key"),
        metadata: None,
    });
    let database = Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("test DB"));
    {
        let mut connection = database.open_connection().expect("connection");
        zuno_db::migration::apply(&mut connection).expect("schema");
        ensure_project(&connection, &plan.project, 1_780_000_000_000).expect("project");
        let transaction = connection.transaction().expect("transaction");
        let mut session = zuno_db::session::SessionCreate::new(
            session_id,
            session_id,
            &plan.project.id,
            directory.path().to_string_lossy(),
            directory.path().to_string_lossy(),
            "Foreground host fixture",
            crate::RUST_PACKAGE_VERSION,
        )
        .at(1_780_000_000_000);
        session.agent = Some("build".to_owned());
        session.model = Some(zuno_db::session::model_reference_with_variant(
            "provider", "model", None,
        ));
        zuno_db::session::create(&transaction, &session).expect("session");
        transaction.commit().expect("commit");
    }
    let environment = crate::environment::StartupEnvironment::resolve(
        &Env::empty(),
        &crate::GlobalOptions::default(),
    );
    let mut host = TurnHost::open_with_dependencies(
        plan,
        &environment,
        TurnHostDependencies {
            approval: Arc::new(zuno_tool::AllowAll),
            question: None,
            runs: SessionRunRegistry::new(),
            mcp: None,
            database,
            child_observer: None,
            detached_observer: None,
        },
    )
    .await
    .expect("actual TurnHost");
    let provider = Arc::new(ForegroundProvider {
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
        max_requests,
        release_gate,
    });
    let mut providers = ProviderRegistry::new();
    let injected = Arc::clone(&provider);
    providers.register(COMPATIBLE_PROVIDER, move |_| injected.clone());
    host.providers = providers;
    (directory, host, provider)
}

#[tokio::test]
async fn actual_host_waits_before_a_second_paid_request_and_defers_success() {
    let (_directory, mut host, provider) = foreground_host().await;
    let terminal_events = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&terminal_events);
    let (sender, mut receiver) = zuno_engine::r#loop::event_channel();
    let events = tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            if matches!(event, TurnEvent::TurnCompleted { .. }) {
                observed.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    let mut drive = Box::pin(host.drive("Run the command and wait for its result.", sender));
    let early = tokio::time::timeout(Duration::from_millis(150), &mut drive).await;
    let returned_early = early.is_ok();
    let calls_before_release = provider.calls.load(Ordering::SeqCst);
    let terminals_before_release = terminal_events.load(Ordering::SeqCst);
    provider.release();
    let result = match early {
        Ok(result) => result,
        Err(_) => drive.as_mut().await,
    };
    drop(drive);
    result.expect("actual host completes");
    events.await.expect("event reader");
    assert_eq!(
        calls_before_release, 1,
        "foreground yield must stop the default driver's paid tool loop"
    );
    assert!(
        !returned_early,
        "the logical host operation must still own the foreground wait"
    );
    assert_eq!(
        terminals_before_release, 0,
        "a model/tool yield is not a successful host completion"
    );
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "one native wait and one real result continuation"
    );
    assert_eq!(terminal_events.load(Ordering::SeqCst), 1);
    assert!(
        host.background_executions
            .foreground_for_session(&host.session_id)
            .is_empty()
    );
    let receipts =
        zuno_db::verification::for_session(&host.connection, &host.session_id).expect("receipts");
    assert!(
        receipts
            .iter()
            .any(|receipt| receipt.tool_call_id == "host_foreground_call"
                && receipt.proves_success())
    );
    {
        let requests = provider.requests.lock().expect("provider requests");
        let resumed = &requests[1];
        assert!(
            resumed
                .messages
                .iter()
                .any(|message| message.content.iter().any(|block| {
                    matches!(block, zuno_llm::event::RequestContentBlock::ToolResult {
                    tool_use_id, content, ..
                } if tool_use_id == "host_foreground_call"
                    && content.contains("foreground-final")
                    && content.contains("Verification receipt:"))
                })),
            "the next actual request must see the final output on the original tool call"
        );
        assert!(
            resumed
                .developer_context
                .iter()
                .all(|section| !section.contains("foreground-final")),
            "untrusted command stdout is not developer policy"
        );
    }
    host.shutdown().await.expect("shutdown fixture");
}

#[tokio::test]
async fn actual_host_accepts_one_durable_steer_then_waits_after_model_stop() {
    let (_directory, mut host, provider) = foreground_host_with_requests(3).await;
    let control = host.control();
    let admission = host.input_admission();
    let inbox = host.session_inbox();
    let session_id = host.session_id.clone();
    let processes = host.background_executions.clone();
    let terminal_events = Arc::new(AtomicUsize::new(0));
    let observed = terminal_events.clone();
    let (sender, mut receiver) = zuno_engine::r#loop::event_channel();
    let events = tokio::spawn(async move {
        let mut consumed = Vec::new();
        while let Some(event) = receiver.recv().await {
            match event {
                TurnEvent::TurnCompleted { .. } => {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                TurnEvent::InputConsumed { input_id, .. } => consumed.push(input_id),
                _ => {}
            }
        }
        consumed
    });
    let mut drive = Box::pin(host.drive("Run the command and inspect its result.", sender));
    let original = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(original) = processes.foreground_for_session(&session_id).into_iter().next() {
                break original;
            }
            tokio::select! {
                result = &mut drive => panic!("turn ended before foreground registration: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {},
            }
        }
    }).await.expect("foreground execution must be durably registered");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let original_turn = control.active_turn_id().expect("steerable foreground turn");
    let input = admission
        .admit_steer(
            zuno_db::inbox::NewSessionInput::new(
                "host_foreground_steer",
                &session_id,
                json!({"kind":"user","text":"Keep waiting on the same command; do not restart it."}),
                zuno_db::inbox::InputDelivery::Steer,
                zuno_db::message::now_millis(),
            )
            .with_cycle_id(original.info.cycle_id.clone()),
            &original_turn,
            zuno_engine::admission::SteeringContent::user(
                "Keep waiting on the same command; do not restart it.",
            ),
        )
        .expect("native exact-turn durable admission");
    let waiting = tokio::time::timeout(Duration::from_millis(150), &mut drive).await;
    let calls_after_steer = provider.calls.load(Ordering::SeqCst);
    let terminals_after_steer = terminal_events.load(Ordering::SeqCst);
    let same = processes.foreground_for_session(&session_id);
    provider.release();
    let result = match waiting {
        Ok(result) => result,
        Err(_) => tokio::time::timeout(Duration::from_secs(4), &mut drive)
            .await
            .expect("terminal continuation"),
    };
    drop(drive);
    result.expect("actual host completion");
    let consumed = events.await.expect("event reader");
    assert_eq!(
        calls_after_steer, 2,
        "one real steer buys one provider request"
    );
    assert_eq!(
        terminals_after_steer, 0,
        "the second model stop is still provisional"
    );
    assert_eq!(same.len(), 1);
    assert_eq!(same[0].info.id, original.info.id);
    assert_eq!(same[0].info.cycle_id, original.info.cycle_id);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert_eq!(terminal_events.load(Ordering::SeqCst), 1);
    assert_eq!(consumed.iter().filter(|id| **id == input.id).count(), 1);
    assert_eq!(
        inbox
            .get(&session_id, &input.id)
            .expect("inbox")
            .expect("input")
            .state,
        zuno_db::inbox::SubmissionState::Consumed
    );
    assert!(processes.foreground_for_session(&session_id).is_empty());
    assert!(
        processes.list_for_session(&session_id).is_empty(),
        "no detached task"
    );
    let recorded = zuno_db::event_log::SessionEventLog::new(host.database.clone())
        .read_of_type_after(&session_id, foreground::COMPLETED_EVENT, None)
        .expect("foreground events");
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].properties["cycleId"],
        json!(original.info.cycle_id)
    );
    host.shutdown().await.expect("shutdown fixture");
}

#[tokio::test]
async fn actual_host_hard_interrupt_during_native_wait_never_reports_success() {
    let (_directory, mut host, provider) = foreground_host().await;
    let control = host.control();
    let processes = host.background_executions.clone();
    let session_id = host.session_id.clone();
    let (sender, mut receiver) = zuno_engine::r#loop::event_channel();
    let events = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        events
    });
    let mut drive = Box::pin(host.drive("Run the command and inspect its result.", sender));
    let original = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(original) = processes.foreground_for_session(&session_id).into_iter().next() {
                break original;
            }
            tokio::select! {
                result = &mut drive => panic!("turn ended before foreground registration: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {},
            }
        }
    }).await.expect("foreground execution must be durably registered");

    let request = zuno_engine::interrupt::HardInterruptRequest::new(
        zuno_engine::interrupt::HardInterruptSource::Acp,
        zuno_engine::interrupt::HardInterruptReason::UserCancel,
    );
    control.abort(request);
    let result = tokio::time::timeout(Duration::from_secs(4), &mut drive)
        .await
        .expect("hard interruption remains live");
    drop(drive);
    result.expect("interruption is a typed normal host outcome");
    let events = events.await.expect("event reader");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnCompleted { .. }))
    );
    assert!(events.iter().any(|event| matches!(event,
        TurnEvent::TurnInterrupted { request: Some(observed), .. } if *observed == request
    )));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let execution = processes
        .foreground(&original.info.id, &session_id)
        .expect("original handle");
    assert_eq!(
        execution.info.status,
        zuno_pty::BackgroundExecutionStatus::Cancelled
    );
    assert!(execution.consumed);
    assert!(processes.list_for_session(&session_id).is_empty());
    let receipts =
        zuno_db::verification::for_session(&host.connection, &session_id).expect("receipts");
    assert_eq!(receipts.len(), 1);
    assert!(!receipts[0].proves_success());
    host.shutdown().await.expect("shutdown fixture");
}

#[tokio::test]
async fn actual_host_wall_budget_ends_native_wait_without_a_second_request() {
    let (_directory, mut host, provider) = foreground_host().await;
    host.turn_allowance = zuno_engine::budget::TurnAllowance {
        max_duration: Some(Duration::from_millis(200)),
        ..zuno_engine::budget::TurnAllowance::UNLIMITED
    };
    let (sender, mut receiver) = zuno_engine::r#loop::event_channel();
    let events = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        events
    });
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        host.drive("Run the command and inspect its result.", sender),
    )
    .await
    .expect("logical wall budget stops the native wait");
    let events = events.await.expect("event reader");
    assert!(
        result.is_err(),
        "a turn-budget stop must not be presented as successful completion"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnCompleted { .. }))
    );
    assert!(
        host.background_executions
            .foreground_for_session(&host.session_id)
            .is_empty(),
        "the bounded stop must drain the same foreground handle"
    );
    let receipts =
        zuno_db::verification::for_session(&host.connection, &host.session_id).expect("receipts");
    assert_eq!(receipts.len(), 1);
    assert!(!receipts[0].proves_success());
    host.shutdown().await.expect("shutdown fixture");
}
