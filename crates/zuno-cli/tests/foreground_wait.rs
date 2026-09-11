//! Real default-driver and process-service tests; every workspace and DB is a fixture.
#![cfg(unix)]

#[path = "../src/cmd/turn/foreground.rs"]
mod foreground;
#[path = "../../zuno-tools/tests/support/sandbox.rs"]
mod sandbox;

use async_trait::async_trait;
use foreground::{
    ForegroundError, ForegroundHookContext, ForegroundTurnBudget, ForegroundTurnHooks,
    ForegroundWaitCoordinator, ForegroundWaitResult, ForegroundWaitScope, ForegroundWake,
};
use serde_json::json;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zuno_db::completion_delivery::{CompletionDeliveryStore, CompletionOwner};
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::session_execution::SessionExecutionStore;
use zuno_db::{Pool, migration};
use zuno_engine::budget::{
    BudgetDecision, BudgetPolicyError, BudgetStop, BudgetStopKind, NoopBudgetPolicy,
    ProviderRequestUsage, TurnAllowance, TurnBudgetPolicy, TurnUsageSnapshot,
};
use zuno_engine::driver::{AgentDriver, DefaultAgentDriver};
use zuno_engine::hooks::TurnHooks;
use zuno_engine::interrupt::{
    HardInterruptReason, HardInterruptRequest, HardInterruptSource, SoftInterruptMessage,
    SoftInterruptSource,
};
use zuno_engine::r#loop::{
    AgentModelResolver, AvailableTools, DispatchRequest, PreparedToolDispatch, ResolvedAgent,
    ResolvedModel, RunTurnRequest, ToolDispatchResult, ToolDispatcher, TurnContext, TurnEvent,
    TurnEventSender, TurnOutcome, event_channel,
};
use zuno_engine::status::{AbortDisposition, SessionRunGuard, SessionRunRegistry, SessionStatus};
use zuno_llm::cache::{DynamicContext, McpToolStatus};
use zuno_llm::event::{FinishReason, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};
use zuno_paths::DbLocation;
use zuno_pty::{
    BackgroundExecutionId, BackgroundExecutionPurpose, BackgroundExecutionService,
    BackgroundExecutionStatus,
};
use zuno_tool::{
    AllowAll, NeverInterrupted, OutputLimits, ToolContext, ToolOutput, ToolOutputStore,
};
use zuno_tools::shell::ShellParams;
use zuno_types::execution::{CollaborationMode, SessionExecutionPhase};

const SESSION: &str = "ses_foreground_coordinator";
const CYCLE: &str = "cycle_foreground_coordinator";
const CALL: &str = "call_foreground";
const TURN: &str = "turn_model_stop";
const PART: &str = "part_foreground";

#[derive(Debug, Default)]
struct StopProvider {
    calls: AtomicUsize,
}

impl Provider for StopProvider {
    fn id(&self) -> &str {
        "foreground-test"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::text_only()
    }

    fn stream(&self, _request: CompletionRequest) -> ProviderStream<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::iter([
            Ok(StreamEvent::TextDelta(
                "The command is still running.".to_owned(),
            )),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some(FinishReason::Stop),
            }),
        ]))
    }
}

struct Resolver;

impl AgentModelResolver for Resolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| ResolvedAgent::new("build", "Foreground fixture"))
    }

    fn resolve_model(&self, provider: &str, model: &str) -> Option<ResolvedModel> {
        (provider == "foreground-test" && model == "model")
            .then(|| ResolvedModel::new(Spec::new("foreground-test"), "model", ApiSurface::Default))
    }
}

struct NoTools;

#[async_trait]
impl ToolDispatcher for NoTools {
    fn available_tools(&self) -> AvailableTools {
        AvailableTools::new(Vec::new(), McpToolStatus::Ready)
    }

    async fn prepare(&self, _request: DispatchRequest) -> PreparedToolDispatch {
        PreparedToolDispatch::ready(ToolDispatchResult::error(ToolOutput::text(
            "unexpected tool",
            "The fixture issues no further tools.",
        )))
    }
}

#[derive(Default)]
struct BoundarySignals {
    entered: tokio::sync::Notify,
    wake_returned: tokio::sync::Notify,
    release_wake: tokio::sync::Notify,
    pause_after_wake: AtomicBool,
}

/// Test-only scheduling at the actual native hook. The real coordinator still
/// makes every wait/continue decision; this exposes its return/cancellation race.
struct ObservedBoundary {
    inner: ForegroundTurnHooks,
    signals: Arc<BoundarySignals>,
}

#[async_trait]
impl TurnHooks for ObservedBoundary {
    async fn event(&self, event: &TurnEvent) -> Result<(), String> {
        self.inner.event(event).await
    }

    async fn before_provider_request(
        &self,
        session_id: &str,
        turn_id: &str,
        next_step: u32,
    ) -> Result<(), String> {
        self.signals.entered.notify_one();
        let result = self
            .inner
            .before_provider_request(session_id, turn_id, next_step)
            .await;
        if result.is_ok() && self.signals.pause_after_wake.swap(false, Ordering::AcqRel) {
            self.signals.wake_returned.notify_one();
            self.signals.release_wake.notified().await;
        }
        result
    }
}

struct Fixture {
    workspace: tempfile::TempDir,
    process_state: tempfile::TempDir,
    pool: Arc<Pool>,
    processes: Arc<BackgroundExecutionService>,
    runs: SessionRunRegistry,
    provider: Arc<StopProvider>,
    id: BackgroundExecutionId,
    boundary: Arc<BoundarySignals>,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_purpose(BackgroundExecutionPurpose::Command).await
    }

    async fn with_purpose(purpose: BackgroundExecutionPurpose) -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let process_state = tempfile::tempdir().expect("process state");
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("test DB"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection.execute_batch(
                "INSERT INTO project (id,worktree,time_created,time_updated,sandboxes)
                 VALUES ('project','/fixture',1,1,'[]');
                 INSERT INTO session
                   (id,project_id,slug,directory,title,version,time_created,time_updated)
                 VALUES ('ses_foreground_coordinator','project','fixture','/fixture','fixture','test',1,1);"
            ).expect("session");
            let store = MessageStore::new(&connection);
            store
                .put_message(
                    &MessageRecord::from_json(json!({
                        "id":"user", "sessionID":SESSION, "role":"user", "time":{"created":10},
                        "agent":"build", "model":{"providerID":"foreground-test","modelID":"model"}
                    }))
                    .expect("user"),
                )
                .expect("persist user");
            store
                .put_part(
                    &PartRecord::from_json(
                        json!({
                            "id":"user_text","messageID":"user","sessionID":SESSION,
                            "type":"text","text":"Wait for the existing command."
                        }),
                        10,
                    )
                    .expect("text"),
                )
                .expect("persist text");
        }
        let executions = SessionExecutionStore::new(Arc::clone(&pool));
        let mut state = executions
            .seed(SESSION, CollaborationMode::Work, None, 10)
            .expect("cycle");
        state.cycle_id = Some(CYCLE.to_owned());
        state.phase = SessionExecutionPhase::Running;
        executions
            .update(state.revision, state)
            .expect("bind cycle");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(workspace.path().join("release"))
                .status()
                .expect("fixture gate")
                .success()
        );
        std::fs::write(workspace.path().join("result"), b"foreground-final")
            .expect("fixture command output");
        let processes =
            Arc::new(BackgroundExecutionService::open(process_state.path()).expect("service"));
        let shell = sandbox::configured_shell_tool(workspace.path(), Some("/bin/bash"))
            .with_background_executions(Arc::clone(&processes))
            .with_execution_store(Arc::clone(&pool))
            .with_hard_ceiling(Duration::from_secs(10));
        let launched = shell
            .run(
                ShellParams {
                    command: "cat release >/dev/null && cat result".to_owned(),
                    timeout: Some(10),
                    workdir: None,
                    background: false,
                    background_purpose: purpose,
                    expected_git_head: None,
                    exit_policy: None,
                },
                ToolContext::new(
                    SESSION,
                    "origin",
                    CALL,
                    "build",
                    Arc::new(AllowAll),
                    Arc::new(NeverInterrupted),
                ),
            )
            .await
            .expect("foreground handle");
        let id =
            BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("handle"))
                .expect("id");
        {
            let connection = pool.get().expect("connection");
            let store = MessageStore::new(&connection);
            store
                .put_message(
                    &MessageRecord::from_json(json!({
                        "id":"origin","sessionID":SESSION,"role":"assistant",
                        "parentID":"user","agent":"build","mode":"build",
                        "providerID":"foreground-test","modelID":"model",
                        "time":{"created":20}
                    }))
                    .expect("assistant"),
                )
                .expect("persist assistant");
            store
                .put_part(
                    &PartRecord::from_json(
                        json!({
                            "id":PART,"messageID":"origin","sessionID":SESSION,"type":"tool",
                            "callID":CALL,"tool":"shell","displayName":"bash",
                            "state":{"status":"completed","input":{},"title":launched.title,
                                "output":launched.output,"metadata":launched.metadata,
                                "time":{"start":20,"end":21}}
                        }),
                        20,
                    )
                    .expect("tool part"),
                )
                .expect("persist yielded result");
        }
        Self {
            workspace,
            process_state,
            pool,
            processes,
            runs: SessionRunRegistry::new(),
            provider: Arc::new(StopProvider::default()),
            id,
            boundary: Arc::new(BoundarySignals::default()),
        }
    }

    fn coordinator(&self) -> ForegroundWaitCoordinator {
        ForegroundWaitCoordinator::new(
            Arc::clone(&self.processes),
            Arc::clone(&self.pool),
            ToolOutputStore::new(self.workspace.path().join("tool-output")),
            OutputLimits::default(),
            |session, call| format!("rcp_{session}_{call}"),
        )
    }

    fn budget(&self, allowance: TurnAllowance) -> Arc<ForegroundTurnBudget> {
        Arc::new(ForegroundTurnBudget::new(
            SESSION,
            CYCLE,
            allowance,
            Arc::new(NoopBudgetPolicy),
        ))
    }

    fn hooks(
        &self,
        guard: &SessionRunGuard,
        budget: Arc<ForegroundTurnBudget>,
        turn_id: &str,
        events: TurnEventSender,
    ) -> ForegroundTurnHooks {
        let instruction = budget.instruction();
        ForegroundTurnHooks::new(budget, turn_id.to_owned()).with_foreground(
            ForegroundHookContext {
                coordinator: self.coordinator(),
                interrupt: guard.interrupt_signal().clone(),
                steering: guard.soft_interrupt_signal().clone(),
                events,
                instruction,
            },
        )
    }

    fn count(&self, table: &str) -> i64 {
        assert!(matches!(
            table,
            "completion_delivery" | "verification_receipt" | "session_input"
        ));
        self.pool
            .get()
            .expect("connection")
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("fixture count")
    }

    fn completed_events(&self) -> usize {
        zuno_db::event_log::SessionEventLog::new(self.pool.clone())
            .read_of_type_after(SESSION, foreground::COMPLETED_EVENT, None)
            .expect("completion event count")
            .len()
    }

    async fn drive(&self) -> (TurnOutcome, ForegroundWaitResult) {
        let guard = self
            .runs
            .begin_turn(SESSION)
            .expect("logical operation lease");
        let budget = self.budget(TurnAllowance::UNLIMITED);
        let mut connection = self.pool.get().expect("connection");
        let mut providers = ProviderRegistry::new();
        let provider = Arc::clone(&self.provider);
        providers.register("foreground-test", move |_| provider.clone());
        let resolver = Resolver;
        let dispatcher = NoTools;
        let inbox = zuno_db::inbox::SessionInbox::new(Arc::clone(&self.pool));
        let context = TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            guard.interrupt_signal(),
        )
        .with_live_inputs(&guard, &inbox)
        .with_budget_policy(budget.clone());
        let (events, _receiver) = event_channel();
        let model_turn = DefaultAgentDriver.drive(
            RunTurnRequest::new(SESSION, TURN, DynamicContext::default())
                .with_deferred_success_terminal_event(true),
            context,
            events,
        );
        let outcome = model_turn.await.expect("default driver");
        let foreground = self
            .coordinator()
            .wait(&ForegroundWaitScope {
                guard: &guard,
                turn_id: TURN,
                budget: &budget,
            })
            .await
            .expect("foreground wait");
        (outcome, foreground)
    }

    async fn drive_at_request_boundary(&self) -> TurnOutcome {
        let guard = self
            .runs
            .begin_turn(SESSION)
            .expect("logical operation lease");
        let budget = self.budget(TurnAllowance::UNLIMITED);
        let mut connection = self.pool.get().expect("connection");
        let mut providers = ProviderRegistry::new();
        let provider = self.provider.clone();
        providers.register("foreground-test", move |_| provider.clone());
        let resolver = Resolver;
        let dispatcher = NoTools;
        let inbox = zuno_db::inbox::SessionInbox::new(self.pool.clone());
        let (events, _receiver) = event_channel();
        let hooks = Arc::new(ObservedBoundary {
            inner: self.hooks(&guard, budget.clone(), TURN, events.clone()),
            signals: self.boundary.clone(),
        });
        let context = TurnContext::new(
            &mut connection,
            &providers,
            &resolver,
            &dispatcher,
            guard.interrupt_signal(),
        )
        .with_live_inputs(&guard, &inbox)
        .with_budget_policy(budget)
        .with_hooks(hooks);
        DefaultAgentDriver
            .drive(
                RunTurnRequest::new(SESSION, TURN, DynamicContext::default())
                    .with_deferred_success_terminal_event(true),
                context,
                events,
            )
            .await
            .expect("native default-driver boundary")
    }

    fn release(&self) {
        std::fs::write(self.workspace.path().join("release"), b"finish").expect("release fixture");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _cancelled = self.processes.cancel(&self.id);
    }
}

#[tokio::test]
async fn model_stop_keeps_the_same_host_operation_busy_until_foreground_terminal() {
    let fixture = Arc::new(Fixture::new().await);
    let task_fixture = Arc::clone(&fixture);
    let mut operation = tokio::spawn(async move { task_fixture.drive().await });
    let early = tokio::time::timeout(Duration::from_millis(100), &mut operation).await;
    let finished_early = early.is_ok();
    let status_before_release = fixture.runs.status(SESSION);
    fixture.release();
    let result = match early {
        Ok(result) => result.expect("host task"),
        Err(_) => operation.await.expect("host task"),
    };
    fixture
        .processes
        .wait(&fixture.id, None)
        .await
        .expect("fixture process settles");
    assert!(
        !finished_early,
        "the host returned a model stop while its foreground process was still running"
    );
    assert_eq!(status_before_release, SessionStatus::Busy);
    assert_eq!(
        fixture.provider.calls.load(Ordering::SeqCst),
        1,
        "waiting must not poll the model"
    );
    assert_eq!(fixture.runs.status(SESSION), SessionStatus::Idle);
    assert!(matches!(result.0, TurnOutcome::Completed { .. }));
    assert_eq!(result.1.wake, ForegroundWake::Completed);
    assert_eq!(result.1.publications.len(), 1);
    assert!(result.1.pending.is_empty());
    let instruction = fixture
        .coordinator()
        .resume_instruction(SESSION, &result.1)
        .expect("instruction")
        .expect("new result");
    assert!(
        !instruction.contains("foreground-final"),
        "command stdout belongs to its tool part, not developer instructions"
    );
    assert!(
        result.1.publications[0]
            .output
            .output
            .contains("foreground-final")
    );
    assert!(fixture.processes.foreground_for_session(SESSION).is_empty());
    assert!(fixture.process_state.path().exists());
}

#[tokio::test]
async fn request_boundary_waits_on_the_same_handle_and_publishes_once() {
    let fixture = Fixture::new().await;
    let mut callbacks = fixture.processes.subscribe();
    let guard = fixture.runs.begin_turn(SESSION).expect("host lease");
    let identity = guard.mark_turn_started(TURN).expect("engine identity");
    let budget = fixture.budget(TurnAllowance::UNLIMITED);
    let (events, mut receiver) = event_channel();
    let hooks = fixture.hooks(&guard, budget.clone(), TURN, events);
    let mut boundary = Box::pin(hooks.before_provider_request(SESSION, TURN, 2));
    assert!(
        tokio::time::timeout(Duration::from_millis(60), &mut boundary)
            .await
            .is_err(),
        "attention expiry is not a reason to request the model again"
    );
    assert_eq!(fixture.runs.active_turn_id(SESSION).as_deref(), Some(TURN));
    assert_eq!(fixture.count("completion_delivery"), 0);
    assert_eq!(fixture.completed_events(), 0);
    fixture.release();
    tokio::time::timeout(Duration::from_secs(3), &mut boundary)
        .await
        .expect("terminal notification")
        .expect("publish original result");
    drop(boundary);
    assert!(hooks.take_failure().is_none());
    let event = receiver.recv().await.expect("terminal tool event");
    assert!(
        matches!(event, TurnEvent::ToolDispatchCompleted { ref call_id, .. } if call_id == CALL)
    );
    assert_eq!(fixture.runs.active_turn_id(SESSION).as_deref(), Some(TURN));
    assert!(
        fixture
            .processes
            .foreground(&fixture.id, SESSION)
            .expect("handle")
            .consumed
    );
    assert!(callbacks.try_recv().is_err(), "no detached callback");

    hooks
        .before_provider_request(SESSION, TURN, 2)
        .await
        .expect("repeat boundary");
    assert!(receiver.try_recv().is_err(), "output is committed once");
    assert_eq!(fixture.completed_events(), 1);
    assert_eq!(fixture.count("completion_delivery"), 1);
    assert_eq!(fixture.count("verification_receipt"), 1);
    assert_eq!(fixture.count("session_input"), 0);
    let rebound = BackgroundExecutionService::open(fixture.process_state.path()).expect("rebind");
    assert!(
        rebound
            .foreground(&fixture.id, SESSION)
            .expect("saved handle")
            .consumed
    );
    let instruction = budget.instruction();
    let instruction = instruction
        .lock()
        .expect("instruction")
        .clone()
        .expect("manifest");
    assert!(instruction.contains(CALL));
    assert!(!instruction.contains("foreground-final"));
    drop(identity);
    drop(guard);
    assert_eq!(fixture.runs.status(SESSION), SessionStatus::Idle);
}

#[tokio::test]
async fn real_default_driver_makes_no_request_until_native_process_terminal() {
    let fixture = Arc::new(Fixture::new().await);
    let task_fixture = fixture.clone();
    let mut drive = tokio::spawn(async move { task_fixture.drive_at_request_boundary().await });
    let early = tokio::time::timeout(Duration::from_millis(60), &mut drive).await;
    let requests_before_terminal = fixture.provider.calls.load(Ordering::SeqCst);
    fixture.release();
    let outcome = match early {
        Ok(outcome) => outcome.expect("driver task"),
        Err(_) => drive.await.expect("driver task"),
    };
    assert_eq!(
        requests_before_terminal, 0,
        "the native hook precedes the provider"
    );
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.completed_events(), 1);
}

#[tokio::test]
async fn a_cancelled_steer_wakeup_does_not_buy_an_unchanged_provider_request() {
    let fixture = Arc::new(Fixture::new().await);
    let task_fixture = fixture.clone();
    let mut drive = tokio::spawn(async move { task_fixture.drive_at_request_boundary().await });
    tokio::time::timeout(Duration::from_secs(3), fixture.boundary.entered.notified())
        .await
        .expect("the default driver entered the real native wait");
    let expected_turn = fixture
        .runs
        .active_turn_id(SESSION)
        .expect("native wait identity");
    let inbox = zuno_db::inbox::SessionInbox::new(fixture.pool.clone());
    let input = inbox
        .admit(zuno_db::inbox::NewSessionInput::new(
            "cancelled_steer",
            SESSION,
            json!({"kind":"user","text":"Withdrawn instruction"}),
            zuno_db::inbox::InputDelivery::Steer,
            zuno_db::message::now_millis(),
        ))
        .expect("admitted steer");
    fixture
        .runs
        .queue_soft_interrupt_for_turn(
            SESSION,
            &expected_turn,
            SoftInterruptMessage {
                input_id: Some(input.id.clone()),
                revision: Some(input.revision),
                content: "Withdrawn instruction".to_owned(),
                images: Vec::new(),
                attachments: Vec::new(),
                urgent: false,
                source: SoftInterruptSource::User,
            },
        )
        .expect("queued exact-turn signal");
    // A stale wake can remain after a durable cancellation wins the promotion race.
    inbox
        .cancel_pending(
            SESSION,
            &input.id,
            input.revision,
            zuno_db::message::now_millis(),
        )
        .expect("cancel before the next engine poll");
    let observed = tokio::time::timeout(Duration::from_millis(60), &mut drive).await;
    let requests_before_terminal = fixture.provider.calls.load(Ordering::SeqCst);
    fixture.release();
    match observed {
        Ok(outcome) => {
            outcome.expect("driver task");
        }
        Err(_) => {
            drive.await.expect("driver task");
        }
    }
    assert_eq!(
        requests_before_terminal, 0,
        "an unconsumed/cancelled steer is a native wake, not new model-visible work"
    );
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelling_the_queued_signal_after_the_native_hook_wakes_does_not_start_a_request() {
    let fixture = Arc::new(Fixture::new().await);
    fixture
        .boundary
        .pause_after_wake
        .store(true, Ordering::Release);
    let task_fixture = fixture.clone();
    let mut drive = tokio::spawn(async move { task_fixture.drive_at_request_boundary().await });
    tokio::time::timeout(Duration::from_secs(3), fixture.boundary.entered.notified())
        .await
        .expect("actual native request boundary");
    let expected_turn = fixture.runs.active_turn_id(SESSION).expect("turn identity");
    let inbox = zuno_db::inbox::SessionInbox::new(fixture.pool.clone());
    let admission =
        zuno_engine::admission::SessionInputAdmission::new(inbox.clone(), fixture.runs.clone());
    let input = admission
        .admit_steer(
            zuno_db::inbox::NewSessionInput::new(
                "withdrawn_after_wake",
                SESSION,
                json!({"kind":"user","text":"Withdraw this instruction before application."}),
                zuno_db::inbox::InputDelivery::Steer,
                zuno_db::message::now_millis(),
            ),
            &expected_turn,
            zuno_engine::admission::SteeringContent::user(
                "Withdraw this instruction before application.",
            ),
        )
        .expect("native exact-turn admission");
    tokio::time::timeout(
        Duration::from_secs(3),
        fixture.boundary.wake_returned.notified(),
    )
    .await
    .expect("foreground hook yielded to the queued steer");
    inbox
        .cancel_pending(
            SESSION,
            &input.id,
            input.revision,
            zuno_db::message::now_millis(),
        )
        .expect("withdraw durable input");
    assert!(
        fixture
            .runs
            .cancel_soft_interrupt(SESSION, &input.id)
            .expect("retire process-local message"),
        "the race must empty the actual queue, not just mark its durable row cancelled"
    );
    fixture.boundary.release_wake.notify_one();
    let observed = tokio::time::timeout(Duration::from_millis(60), &mut drive).await;
    let requests_before_terminal = fixture.provider.calls.load(Ordering::SeqCst);
    fixture.release();
    match observed {
        Ok(result) => {
            result.expect("driver task");
        }
        Err(_) => {
            drive.await.expect("driver task");
        }
    }
    assert_eq!(
        requests_before_terminal, 0,
        "a handled wake whose queue was cancelled is not new model-visible work"
    );
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        inbox
            .get(SESSION, &input.id)
            .expect("inbox")
            .expect("input")
            .state,
        zuno_db::inbox::SubmissionState::Cancelled
    );
    assert_eq!(fixture.completed_events(), 1);
}

#[tokio::test]
async fn hard_interrupt_cancels_and_records_unknown_without_detaching() {
    let fixture = Fixture::new().await;
    let mut callbacks = fixture.processes.subscribe();
    let guard = fixture.runs.begin_turn(SESSION).expect("host lease");
    let _identity = guard.mark_turn_started(TURN).expect("engine identity");
    let budget = fixture.budget(TurnAllowance::UNLIMITED);
    let coordinator = fixture.coordinator();
    let scope = ForegroundWaitScope {
        guard: &guard,
        turn_id: TURN,
        budget: &budget,
    };
    let request =
        HardInterruptRequest::new(HardInterruptSource::Acp, HardInterruptReason::UserCancel);
    let (result, ()) = tokio::join!(coordinator.wait_active(&scope), async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            fixture.runs.abort(SESSION, request),
            AbortDisposition::Active
        );
    });
    let result = result.expect("cancel and drain");
    assert_eq!(
        result.wake,
        ForegroundWake::Interrupted {
            request: Some(request)
        }
    );
    assert!(result.pending.is_empty());
    assert_eq!(result.publications.len(), 1);
    assert_eq!(fixture.runs.active_turn_id(SESSION).as_deref(), Some(TURN));
    let execution = fixture
        .processes
        .foreground(&fixture.id, SESSION)
        .expect("same handle");
    assert_eq!(execution.info.status, BackgroundExecutionStatus::Cancelled);
    assert_eq!(execution.info.cycle_id.as_deref(), Some(CYCLE));
    assert_eq!(execution.context.call_id, CALL);
    assert!(execution.consumed);
    let connection = fixture.pool.get().expect("connection");
    let part = MessageStore::new(&connection)
        .part(PART)
        .expect("original part");
    assert_eq!(part.data["state"]["outcome"], "uncertain");
    assert_eq!(part.data["state"]["uncertain"]["cause"], "interrupted");
    let receipts = zuno_db::verification::for_session(&connection, SESSION).expect("receipts");
    assert_eq!(receipts.len(), 1);
    assert!(!receipts[0].proves_success());
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 0);
    assert!(callbacks.try_recv().is_err());
    assert_eq!(fixture.count("session_input"), 0);
}

#[tokio::test]
async fn dropping_a_hard_interrupted_request_hook_cancels_but_defers_ack_to_durable_drain() {
    let fixture = Fixture::new().await;
    let guard = fixture.runs.begin_turn(SESSION).expect("lease");
    let _identity = guard.mark_turn_started(TURN).expect("identity");
    let budget = fixture.budget(TurnAllowance::UNLIMITED);
    let (events, _receiver) = event_channel();
    let hooks = fixture.hooks(&guard, budget.clone(), TURN, events);
    let mut boundary = Box::pin(hooks.before_provider_request(SESSION, TURN, 2));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut boundary)
            .await
            .is_err()
    );
    let request = HardInterruptRequest::new(
        HardInterruptSource::Lifecycle,
        HardInterruptReason::Shutdown,
    );
    fixture.runs.abort(SESSION, request);
    // The engine's biased select drops the hook when the hard signal fires.
    drop(boundary);
    fixture
        .processes
        .wait(&fixture.id, None)
        .await
        .expect("cancelled process");
    assert!(
        !fixture
            .processes
            .foreground(&fixture.id, SESSION)
            .expect("unacknowledged handle")
            .consumed
    );
    assert_eq!(fixture.completed_events(), 0);
    let result = fixture
        .coordinator()
        .stop(
            &ForegroundWaitScope {
                guard: &guard,
                turn_id: TURN,
                budget: &budget,
            },
            None,
        )
        .await
        .expect("host durable drain");
    assert_eq!(result.publications.len(), 1);
    assert!(result.pending.is_empty());
    assert_eq!(
        result.wake,
        ForegroundWake::Interrupted {
            request: Some(request)
        }
    );
    assert_eq!(fixture.completed_events(), 1);
}

#[tokio::test]
async fn a_committed_steer_permits_one_request_without_restarting_the_process() {
    let fixture = Fixture::new().await;
    let guard = fixture.runs.begin_turn(SESSION).expect("lease");
    let _identity = guard.mark_turn_started(TURN).expect("identity");
    let budget = fixture.budget(TurnAllowance::UNLIMITED);
    let (events, _receiver) = event_channel();
    let hooks = fixture.hooks(&guard, budget.clone(), TURN, events);
    let inbox = zuno_db::inbox::SessionInbox::new(fixture.pool.clone());
    let input = inbox
        .admit(
            zuno_db::inbox::NewSessionInput::new(
                "steer_foreground",
                SESSION,
                json!({"kind":"user","text":"Keep waiting; inspect the original result."}),
                zuno_db::inbox::InputDelivery::Steer,
                1_780_000_001_000,
            )
            .with_cycle_id(Some(CYCLE)),
        )
        .expect("durable steer");
    fixture
        .runs
        .queue_soft_interrupt_for_turn(
            SESSION,
            TURN,
            SoftInterruptMessage {
                input_id: Some(input.id.clone()),
                revision: Some(input.revision),
                content: "Keep waiting; inspect the original result.".to_owned(),
                images: Vec::new(),
                attachments: Vec::new(),
                urgent: false,
                source: SoftInterruptSource::User,
            },
        )
        .expect("same-turn steer");
    hooks
        .before_provider_request(SESSION, TURN, 2)
        .await
        .expect("wake for steer");
    assert_eq!(fixture.runs.active_turn_id(SESSION).as_deref(), Some(TURN));
    assert_eq!(
        inbox
            .get(SESSION, &input.id)
            .expect("inbox")
            .expect("input")
            .state,
        zuno_db::inbox::SubmissionState::Steering,
        "the coordinator must leave FIFO consumption to the engine"
    );
    let delivery = guard.take_soft_interrupts_at_safe_point();
    assert_eq!(delivery.messages.len(), 1);
    inbox
        .promote_revision(SESSION, &input.id, input.revision)
        .expect("promotion")
        .expect("claimed");
    inbox
        .mark_consumed(SESSION, &input.id)
        .expect("consumed")
        .expect("committed");
    hooks
        .event(&TurnEvent::InputConsumed {
            input_id: input.id,
            text: delivery.messages[0].content.clone(),
            attachments: Vec::new(),
            source: SoftInterruptSource::User,
        })
        .await
        .expect("native committed-input event");
    for _ in 0..2 {
        tokio::time::timeout(
            Duration::from_millis(100),
            hooks.before_provider_request(SESSION, TURN, 2),
        )
        .await
        .expect("permit survives request preparation")
        .expect("permit");
    }
    hooks
        .event(&TurnEvent::ProviderRequestStarted {
            step: 2,
            message_count: 3,
            estimated_prompt_tokens: 50,
        })
        .await
        .expect("request commits");
    let mut next = Box::pin(hooks.before_provider_request(SESSION, TURN, 3));
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut next)
            .await
            .is_err(),
        "one steer cannot fund repeated unchanged provider polling"
    );
    assert_eq!(
        fixture.processes.foreground_for_session(SESSION)[0].info.id,
        fixture.id
    );
    assert_eq!(fixture.completed_events(), 0);
    fixture.release();
    next.await.expect("same command terminal");
    assert_eq!(fixture.completed_events(), 1);
}

#[tokio::test]
async fn wall_budget_spans_recovery_and_stops_the_native_wait() {
    let fixture = Fixture::new().await;
    let guard = fixture.runs.begin_turn(SESSION).expect("lease");
    let budget = fixture.budget(TurnAllowance {
        max_duration: Some(Duration::from_millis(100)),
        ..TurnAllowance::UNLIMITED
    });
    let started = tokio::time::Instant::now();
    assert_eq!(
        budget
            .before_request(&usage_snapshot(TURN, 0))
            .await
            .expect("budget"),
        BudgetDecision::Continue
    );
    tokio::time::sleep(Duration::from_millis(40)).await;
    let recovery = "turn_recovery_same_cycle";
    let _identity = guard
        .mark_turn_started(recovery)
        .expect("recovery identity");
    let (events, _receiver) = event_channel();
    let hooks = fixture.hooks(&guard, budget.clone(), recovery, events);
    assert!(
        hooks
            .before_provider_request(SESSION, recovery, 1)
            .await
            .is_err()
    );
    assert!(matches!(
        hooks.take_failure(),
        Some(ForegroundError::BudgetStopped)
    ));
    assert_eq!(
        budget.current_stop().expect("latched").kind,
        BudgetStopKind::TimeBudget
    );
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "wait must not rely on Shell's hard ceiling"
    );
    assert!(fixture.processes.foreground_for_session(SESSION).is_empty());
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.completed_events(), 1);
}

#[tokio::test]
async fn explicit_host_budget_stop_cannot_renew_the_allowance_on_recovery() {
    let fixture = Fixture::new().await;
    let guard = fixture.runs.begin_turn(SESSION).expect("lease");
    let budget = fixture.budget(TurnAllowance::UNLIMITED);
    let stop = BudgetStop {
        kind: BudgetStopKind::TokenBudget,
        detail: "host token allowance spent".to_owned(),
    };
    let result = fixture
        .coordinator()
        .stop(
            &ForegroundWaitScope {
                guard: &guard,
                turn_id: TURN,
                budget: &budget,
            },
            Some(stop.clone()),
        )
        .await
        .expect("budget drain");
    assert_eq!(result.wake, ForegroundWake::BudgetLimited(stop.clone()));
    assert_eq!(
        budget
            .before_request(&usage_snapshot("recovery", 0))
            .await
            .expect("recovery decision"),
        BudgetDecision::Stop(stop),
        "an explicit host stop must latch on the logical budget"
    );
}

#[tokio::test]
async fn publication_failure_rolls_back_ownership_and_retries_the_same_result_once() {
    let fixture = Fixture::new().await;
    let original = {
        let connection = fixture.pool.get().expect("connection");
        let original = MessageStore::new(&connection).part(PART).expect("part");
        connection
            .execute("DELETE FROM part WHERE id=?1", [PART])
            .expect("fixture removes part");
        original
    };
    let guard = fixture.runs.begin_turn(SESSION).expect("lease");
    let budget = fixture.budget(TurnAllowance::UNLIMITED);
    let coordinator = fixture.coordinator();
    let scope = ForegroundWaitScope {
        guard: &guard,
        turn_id: TURN,
        budget: &budget,
    };
    fixture.release();
    let error = coordinator
        .wait(&scope)
        .await
        .expect_err("missing original result must fail closed");
    assert!(matches!(
        error,
        ForegroundError::Database(zuno_error::DbError::Conflict { .. })
    ));
    assert_eq!(fixture.count("completion_delivery"), 0);
    assert_eq!(fixture.count("verification_receipt"), 0);
    assert_eq!(fixture.completed_events(), 0);
    assert!(
        !fixture
            .processes
            .foreground(&fixture.id, SESSION)
            .expect("handle")
            .consumed
    );
    {
        let connection = fixture.pool.get().expect("connection");
        MessageStore::new(&connection)
            .put_part(&original)
            .expect("restore fixture part");
    }
    let result = coordinator
        .wait(&scope)
        .await
        .expect("retry observation only");
    assert_eq!(result.publications.len(), 1);
    assert_eq!(result.publications[0].execution_id, fixture.id);
    let delivery = CompletionDeliveryStore::new(fixture.pool.clone())
        .get(&result.publications[0].source_key)
        .expect("delivery")
        .expect("published");
    assert_eq!(delivery.owner, Some(CompletionOwner::Inline));
    assert!(delivery.input_id.is_none());
    assert_eq!(
        coordinator.wait(&scope).await.expect("repeat").wake,
        ForegroundWake::Idle
    );
    assert_eq!(fixture.completed_events(), 1);
    assert_eq!(fixture.count("verification_receipt"), 1);
    assert_eq!(fixture.count("session_input"), 0);
}

#[tokio::test]
async fn remote_observer_exit_is_not_authoritative_remote_success() {
    let fixture = Fixture::with_purpose(BackgroundExecutionPurpose::RemoteObserver).await;
    let guard = fixture.runs.begin_turn(SESSION).expect("lease");
    let budget = fixture.budget(TurnAllowance::UNLIMITED);
    fixture.release();
    let coordinator = fixture.coordinator();
    let result = coordinator
        .wait(&ForegroundWaitScope {
            guard: &guard,
            turn_id: TURN,
            budget: &budget,
        })
        .await
        .expect("observer terminal");
    let connection = fixture.pool.get().expect("connection");
    let receipts = zuno_db::verification::for_session(&connection, SESSION).expect("receipts");
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].outcome,
        zuno_db::verification::ReceiptOutcome::Unknown
    );
    assert_eq!(
        receipts[0].exit_authority,
        zuno_db::verification::ExitAuthority::Absent
    );
    assert!(!receipts[0].proves_success());
    let instruction = coordinator
        .resume_instruction(SESSION, &result)
        .expect("manifest")
        .expect("result");
    assert!(instruction.contains("remote-state recheck"));
    assert!(!instruction.contains("foreground-final"));
}

#[tokio::test]
async fn large_receipt_manifest_uses_an_artifact_without_promoting_stdout_to_instructions() {
    let fixture = Fixture::new().await;
    let guard = fixture.runs.begin_turn(SESSION).expect("lease");
    let budget = fixture.budget(TurnAllowance::UNLIMITED);
    fixture.release();
    let coordinator = fixture.coordinator();
    let mut result = coordinator
        .wait(&ForegroundWaitScope {
            guard: &guard,
            turn_id: TURN,
            budget: &budget,
        })
        .await
        .expect("terminal result");
    result.publications[0].output.metadata["verification"]["summary"] =
        json!("A long receipt note. ".repeat(1_000));
    let instruction = coordinator
        .resume_instruction(SESSION, &result)
        .expect("bounded manifest")
        .expect("manifest");
    assert!(instruction.len() <= 8 * 1024);
    assert!(!instruction.contains("foreground-final"));
    let locator = instruction
        .split_once("outputPath=")
        .expect("artifact pointer")
        .1
        .split_once(", cursor=")
        .expect("bounded read cursor")
        .0;
    let path: String = serde_json::from_str(locator).expect("quoted artifact path");
    assert!(std::path::Path::new(&path).starts_with(fixture.workspace.path()));
    let manifest = std::fs::read_to_string(path).expect("recorded receipt artifact");
    assert!(manifest.len() > 8 * 1024);
    assert!(manifest.contains(CALL));
    assert!(!manifest.contains("foreground-final"));
    assert!(
        result.publications[0]
            .output
            .output
            .contains("foreground-final")
    );
}

#[tokio::test]
async fn a_different_work_cycle_cannot_consume_or_cancel_the_original_foreground_handle() {
    let fixture = Fixture::new().await;
    let guard = fixture.runs.begin_turn(SESSION).expect("lease");
    let budget = ForegroundTurnBudget::new(
        SESSION,
        "another_cycle",
        TurnAllowance::UNLIMITED,
        Arc::new(NoopBudgetPolicy),
    );
    let coordinator = fixture.coordinator();
    let scope = ForegroundWaitScope {
        guard: &guard,
        turn_id: TURN,
        budget: &budget,
    };
    assert_eq!(
        coordinator
            .wait(&scope)
            .await
            .expect("ignore another cycle")
            .wake,
        ForegroundWake::Idle
    );
    let stopped = coordinator
        .stop(&scope, None)
        .await
        .expect("stop only this cycle");
    assert!(stopped.publications.is_empty());
    let original = fixture
        .processes
        .foreground(&fixture.id, SESSION)
        .expect("original handle");
    assert_eq!(original.info.status, BackgroundExecutionStatus::Running);
    assert!(!original.consumed);
    assert_eq!(fixture.completed_events(), 0);
    fixture
        .processes
        .cancel(&fixture.id)
        .expect("fixture cleanup");
    fixture
        .processes
        .wait(&fixture.id, None)
        .await
        .expect("fixture settles");
}

fn usage_snapshot(turn_id: &str, tool_calls: u32) -> TurnUsageSnapshot<'_> {
    TurnUsageSnapshot {
        session_id: SESSION,
        turn_id,
        step: 1,
        turn_usage: ProviderRequestUsage::default(),
        last_request: ProviderRequestUsage::default(),
        estimated_prompt_tokens: 10,
        context_limit: None,
        elapsed_seconds: 0,
        tool_calls_dispatched: tool_calls,
    }
}

#[derive(Default)]
struct RecordingPolicy {
    before_tools: Mutex<Vec<u32>>,
    charges: Mutex<Vec<ProviderRequestUsage>>,
}

#[async_trait]
impl TurnBudgetPolicy for RecordingPolicy {
    async fn before_request(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        self.before_tools
            .lock()
            .expect("observed")
            .push(snapshot.tool_calls_dispatched);
        Ok(BudgetDecision::Continue)
    }

    async fn after_response(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        self.charges
            .lock()
            .expect("charges")
            .push(snapshot.last_request);
        Ok(BudgetDecision::Continue)
    }
}

#[tokio::test]
async fn budget_counts_each_dispatched_call_once_across_engine_turns_and_charges_a_late_response() {
    let inner = Arc::new(RecordingPolicy::default());
    let budget = ForegroundTurnBudget::new(
        SESSION,
        CYCLE,
        TurnAllowance {
            max_tool_calls: NonZeroU32::new(3),
            ..TurnAllowance::UNLIMITED
        },
        inner.clone(),
    );
    for count in [1, 1, 2, 1] {
        assert_eq!(
            budget
                .before_request(&usage_snapshot("first", count))
                .await
                .expect("budget"),
            BudgetDecision::Continue
        );
    }
    assert_eq!(*inner.before_tools.lock().expect("observed"), [1, 1, 2, 2]);
    assert!(matches!(
        budget
            .before_request(&usage_snapshot("recovery", 1))
            .await
            .expect("stop"),
        BudgetDecision::Stop(BudgetStop {
            kind: BudgetStopKind::ToolCallBudget,
            ..
        })
    ));
    let mut paid = usage_snapshot("recovery", 1);
    paid.last_request = ProviderRequestUsage {
        input_tokens: 5,
        output_tokens: 2,
        accounted: true,
        ..ProviderRequestUsage::default()
    };
    assert!(matches!(
        budget
            .after_response(&paid)
            .await
            .expect("account late response"),
        BudgetDecision::Stop(_)
    ));
    assert_eq!(*inner.charges.lock().expect("charges"), [paid.last_request]);
}

#[tokio::test]
async fn final_tool_group_is_budgeted_even_without_a_followup_provider_snapshot() {
    let budget = Arc::new(ForegroundTurnBudget::new(
        SESSION,
        CYCLE,
        TurnAllowance {
            max_tool_calls: NonZeroU32::new(2),
            ..TurnAllowance::UNLIMITED
        },
        Arc::new(NoopBudgetPolicy),
    ));
    let hooks = ForegroundTurnHooks::new(budget.clone(), TURN.to_owned());
    let first = TurnEvent::ToolResultAppended {
        step: 1,
        call_id: "first".to_owned(),
        is_error: false,
    };
    hooks.event(&first).await.expect("first");
    hooks.event(&first).await.expect("replayed event");
    assert!(budget.current_stop().is_none());
    hooks
        .event(&TurnEvent::ToolResultAppended {
            step: 1,
            call_id: "second".to_owned(),
            is_error: true,
        })
        .await
        .expect("last dispatch");
    assert_eq!(
        budget.current_stop().expect("stop").kind,
        BudgetStopKind::ToolCallBudget
    );
}

#[test]
fn lost_process_observation_is_typed_uncertainty_not_permission_to_rerun() {
    let id = BackgroundExecutionId::parse("bg_00000000000000000000000000000000").expect("id");
    let error = ForegroundError::Process(zuno_pty::BackgroundExecutionError::NotFound(id));
    assert!(matches!(
        error.into_turn_error(),
        zuno_engine::r#loop::TurnError::BudgetLimited {
            kind: BudgetStopKind::UncertainSideEffect,
            ..
        }
    ));
}

#[tokio::test]
async fn budget_rejects_a_foreign_session_without_charging_it() {
    let inner = Arc::new(RecordingPolicy::default());
    let budget = ForegroundTurnBudget::new(SESSION, CYCLE, TurnAllowance::UNLIMITED, inner.clone());
    let mut snapshot = usage_snapshot(TURN, 0);
    snapshot.session_id = "another-session";
    assert!(matches!(
        budget.after_response(&snapshot).await,
        Err(BudgetPolicyError::Permanent(_))
    ));
    assert!(inner.charges.lock().expect("charges").is_empty());
    assert!(budget.current_stop().is_none());
}
