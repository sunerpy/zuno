//! What delegation must do against a real database.
//!
//! These exercise the three things a [`ChildTurnHost`] owes
//! [`zuno_tools::task::TaskTool`] and that no test double can prove: the ancestry walk
//! the depth guard reads, the child session row a delegation creates, and the resume
//! check that keeps one parent out of another's children. Driving a full child turn
//! needs a provider, so that is covered by the registry membership assertions and the
//! real-binary check instead.

use super::*;
use crate::command::GlobalOptions;
use std::future::pending;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use zuno_engine::interrupt::InterruptSignal;
use zuno_paths::{DbLocation, Env};
use zuno_tool::{InterruptHandle, NeverInterrupted};
use zuno_tools::task::ReportDelivery;

fn no_interrupt() -> Arc<dyn InterruptHandle> {
    Arc::new(NeverInterrupted)
}

#[derive(Default)]
struct RecordingChildObserver {
    events: Mutex<Vec<(String, zuno_engine::r#loop::TurnEvent)>>,
}

impl ChildTurnObserver for RecordingChildObserver {
    fn opened(&self, _opened: ChildSessionOpened) {}

    fn event(&self, session_id: &str, event: &zuno_engine::r#loop::TurnEvent) {
        self.events
            .lock()
            .expect("observer event lock")
            .push((session_id.to_owned(), event.clone()));
    }
}

#[tokio::test]
async fn child_events_are_forwarded_before_the_child_channel_closes() {
    let observer = Arc::new(RecordingChildObserver::default());
    let erased: Arc<dyn ChildTurnObserver> = observer.clone();
    let (sender, receiver) = zuno_engine::r#loop::event_channel();
    let forwarding = tokio::spawn(forward_child_events(
        String::from("ses_child"),
        receiver,
        Some(erased),
    ));

    sender
        .publish(zuno_engine::r#loop::TurnEvent::TurnStarted {
            session_id: String::from("ses_child"),
        })
        .await
        .expect("publish the live event");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !observer
                .events
                .lock()
                .expect("observer event lock")
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the observer must see the event before completion");
    {
        let events = observer.events.lock().expect("observer event lock");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "ses_child");
        assert!(matches!(
            events[0].1,
            zuno_engine::r#loop::TurnEvent::TurnStarted { .. }
        ));
    }

    drop(sender);
    forwarding.await.expect("forwarder task exits cleanly");
}

struct Fixture {
    _root: tempfile::TempDir,
    host: ChildSessionHost,
    database: DbLocation,
    runner: Arc<RecordingRunner>,
    wake: Arc<RecordingWake>,
    jobs: BackgroundJobSupervisor,
}

impl Fixture {
    fn new() -> Self {
        Self::with_limit(8)
    }

    fn with_limit(limit: usize) -> Self {
        let root = tempfile::TempDir::new().expect("temporary delegation root");
        let database = DbLocation::File(root.path().join("delegation.db"));
        let mut connection = zuno_db::open::open(&database).expect("open the fixture database");
        zuno_db::migration::apply(&mut connection).expect("migrate the fixture database");
        drop(connection);

        let environment = StartupEnvironment::resolve(&Env::empty(), &GlobalOptions::default());
        let runner = Arc::new(RecordingRunner::default());
        let wake = Arc::new(RecordingWake::default());
        let jobs = BackgroundJobSupervisor::default();
        let delegation_limiter = jobs.delegation_limiter(
            NonZeroUsize::new(limit).expect("fixture delegation limit is non-zero"),
        );
        let host = ChildSessionHost::with_components(
            database.clone(),
            runner.clone(),
            wake.clone(),
            delegation_limiter,
            jobs.clone(),
        )
        .expect("build child host");
        let _ = environment;
        Self {
            _root: root,
            host,
            database,
            runner,
            wake,
            jobs,
        }
    }

    fn connection(&self) -> rusqlite::Connection {
        zuno_db::open::open(&self.database).expect("reopen the fixture database")
    }

    /// A session row, optionally a child of `parent`.
    fn session(&self, id: &str, parent: Option<&str>) {
        let mut connection = self.connection();
        connection
            .execute(
                "INSERT INTO project (id, worktree, vcs, time_created, time_updated, sandboxes) \
                 VALUES ('proj', '/tmp/proj', NULL, 1, 1, '[]') ON CONFLICT (id) DO NOTHING",
                (),
            )
            .expect("seed the project row");
        let mut input = zuno_db::session::SessionCreate::new(
            id,
            id,
            "proj",
            "/tmp/proj",
            "/tmp/proj",
            "fixture session",
            crate::RUST_PACKAGE_VERSION,
        )
        .at(1);
        if let Some(parent) = parent {
            input = input.with_parent(parent);
        }
        let transaction = connection.transaction().expect("begin");
        zuno_db::session::create(&transaction, &input).expect("create the session row");
        transaction.commit().expect("commit");
    }

    fn request(&self, parent: &str) -> ChildTurnRequest {
        ChildTurnRequest {
            parent_session_id: parent.to_owned(),
            parent_attempt: None,
            workflow: None,
            workflow_node: None,
            resume_session_id: None,
            logical_key: "delegation:v1:test".to_owned(),
            agent: "worker".to_owned(),
            description: Some("a delegated unit".to_owned()),
            prompt: "do the thing".to_owned(),
            model: None,
            effort: None,
            provider_options: serde_json::Map::new(),
            subagent_model_policy: zuno_tools::task::SubagentModelPolicy::default(),
            requested_model: None,
            requested_effort: None,
            background: false,
            report_delivery: ReportDelivery::NextStep,
        }
    }

    fn persist_child_for_request(&self, request: &ChildTurnRequest) -> String {
        let admission = self
            .host
            .session_admission_for(request)
            .expect("resolve child session admission");
        if let Some(child) = admission.create {
            self.host
                .database
                .transaction(|transaction| {
                    zuno_db::session::create(transaction, &child).map(|_| ())
                })
                .expect("persist child session");
        }
        admission.session_id
    }
}

fn parent_attempt(turn_id: &str, extension_revision: u64) -> AttemptSnapshot {
    serde_json::from_value(json!({
        "schemaVersion": 4,
        "turnId": turn_id,
        "step": 1,
        "capability": {
            "schemaVersion": 4,
            "pack": {"id":"test","version":"1","upstreamRevision":"test"},
            "extensionRevision": extension_revision,
            "permissionPolicySha256": "policy",
            "sandbox": {
                "mode": "workspace-write",
                "network": "deny",
                "writableRoots": [],
                "protectedPaths": []
            },
            "profiles": [], "presets": [], "councils": [], "workflows": [], "skills": []
        },
        "owner": {
            "sessionId":"ses_owner", "parentSessionId":null, "parentAttempt":null,
            "workflow":null, "workflowNode":null
        },
        "agent": {
            "name":"orchestrator", "sourceId":"test://orchestrator",
            "definitionSha256":"definition", "permissionSha256":"permission",
            "promptPolicySha256":"prompt"
        },
        "model": {
            "providerId":"fake", "modelId":"fake-model", "wireModelId":"fake-model",
            "surface":"responses", "reasoningSha256":"reasoning", "preset":null
        },
        "selectedSkills": [],
        "prompt": {"eventId":"evt-parent","assemblySha256":"assembly","actualSha256":"actual"},
        "tools": []
    }))
    .expect("parent attempt snapshot")
}

#[test]
fn child_continuation_preserves_exact_provider_options() {
    let fixture = Fixture::new();
    let mut request = fixture.request("ses_owner");
    request.provider_options = serde_json::from_value(json!({
        "thinking": {"mode": "enabled", "budget": 8192},
        "vendorExtension": {"nested": ["kept", 7]}
    }))
    .expect("provider options object");

    let spec = ChildSessionSpec::resolved(
        &request,
        "worker",
        "provider/model",
        zuno_llm::effort::ReasoningEffort::High.into(),
    );

    assert_eq!(spec.provider_options, request.provider_options);
    let encoded = serde_json::to_value(ChildSessionMetadata {
        kind: CHILD_SESSION_METADATA_KIND.to_owned(),
        schema_version: CHILD_SESSION_METADATA_SCHEMA_VERSION,
        continuation: spec,
    })
    .expect("serialize child continuation");
    assert_eq!(
        encoded["continuation"]["providerOptions"]["thinking"]["budget"],
        8192
    );
    assert_eq!(
        encoded["continuation"]["providerOptions"]["vendorExtension"]["nested"][0],
        "kept"
    );
}

#[test]
fn child_continuation_freezes_model_effort_and_policy_but_reads_legacy_specs() {
    let fixture = Fixture::new();
    let mut request = fixture.request("ses_owner");
    request.effort = Some(zuno_llm::effort::ReasoningEffort::High);
    let original = ChildSessionSpec::resolved(
        &request,
        "worker",
        "provider/model",
        Some(zuno_llm::effort::ReasoningEffort::High),
    );

    original
        .validate_continuation(&original)
        .expect("an exact explicit model and effort pair may continue");

    let mut changed_model = original.clone();
    changed_model.model = "provider/other".to_owned();
    assert!(
        original
            .validate_continuation(&changed_model)
            .expect_err("continuation may not switch models")
            .contains("effective provider/model")
    );

    let mut changed_effort = original.clone();
    changed_effort.effort = None;
    assert!(
        original
            .validate_continuation(&changed_effort)
            .expect_err("an explicit model may not silently omit its frozen effort")
            .contains("reasoning")
    );

    let mut changed_policy = original.clone();
    changed_policy.subagent_model_policy_sha256 = Some("changed-policy".to_owned());
    assert!(
        original
            .validate_continuation(&changed_policy)
            .expect_err("continuation may not gain different model authority")
            .contains("subagent model policy")
    );

    let mut legacy = original.clone();
    legacy.subagent_model_policy_sha256 = None;
    legacy
        .validate_continuation(&original)
        .expect("historical child metadata without the new digest remains readable");
}

#[derive(Default)]
struct RecordingRunner {
    result: Mutex<Option<Result<String, String>>>,
    started: AtomicUsize,
}

impl RecordingRunner {
    fn complete_with(&self, result: Result<&str, &str>) {
        *self.result.lock().expect("runner result lock") =
            Some(result.map(str::to_owned).map_err(str::to_owned));
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::Acquire)
    }

    async fn wait_for_starts(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while self.started() < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delegated runner reached the expected start count");
    }
}

#[async_trait]
impl DelegatedTurnRunner for RecordingRunner {
    async fn run(
        &self,
        _session_id: &str,
        _request: &ChildTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<String, String> {
        self.started.fetch_add(1, Ordering::AcqRel);
        loop {
            if cancellation.is_cancelled() {
                return Err("cancelled".to_owned());
            }
            if let Some(result) = self.result.lock().expect("runner result lock").take() {
                return result;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[derive(Default)]
struct StubbornRunner {
    started: tokio::sync::Notify,
}

#[async_trait]
impl DelegatedTurnRunner for StubbornRunner {
    async fn run(
        &self,
        _session_id: &str,
        _request: &ChildTurnRequest,
        _cancellation: CancellationToken,
    ) -> Result<String, String> {
        self.started.notify_one();
        pending().await
    }
}

#[derive(Default)]
struct SuccessfulCancellationRunner {
    started: tokio::sync::Notify,
}

#[async_trait]
impl DelegatedTurnRunner for SuccessfulCancellationRunner {
    async fn run(
        &self,
        _session_id: &str,
        _request: &ChildTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<String, String> {
        self.started.notify_one();
        cancellation.cancelled().await;
        Ok(String::from("cleanup completed"))
    }
}

#[test]
fn task_report_metadata_collects_only_typed_durable_evidence() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.session("ses_child", Some("ses_owner"));
    let connection = fixture.connection();
    let store = zuno_db::message::MessageStore::new(&connection);
    let historical_assistant = zuno_db::message::MessageRecord::from_json(json!({
        "id": "msg_child_historical",
        "sessionID": "ses_child",
        "role": "assistant",
        "time": {"created": 8, "completed": 9},
        "parentID": "msg_parent",
        "modelID": "model",
        "providerID": "provider",
        "mode": "worker",
        "agent": "worker",
        "path": {"cwd": "/tmp/proj", "root": "/tmp/proj"},
        "cost": 0,
        "tokens": {
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache": {"read": 0, "write": 0}
        },
        "finish": "stop"
    }))
    .expect("historical assistant message");
    let historical_tool = zuno_db::message::PartRecord::from_json(
        json!({
            "id": "prt_child_historical",
            "sessionID": "ses_child",
            "messageID": historical_assistant.id,
            "type": "tool",
            "callID": "call_historical",
            "tool": "shell",
            "state": {
                "status": "completed",
                "input": {"command": "cargo test -p previous"},
                "output": "ok",
                "metadata": {
                    "writtenPaths": ["/tmp/proj/src/previous.rs"],
                    "taskVerification": {
                        "name": "previous tests",
                        "status": "passed",
                        "evidence": "cargo test -p previous"
                    },
                    "uncertainSideEffects": ["historical uncertainty"]
                }
            }
        }),
        9,
    )
    .expect("historical tool part");
    store
        .put_message(&historical_assistant)
        .expect("persist historical assistant");
    store
        .put_part_at(&historical_tool, 9)
        .expect("persist historical tool part");
    let evidence_start_rowid = store
        .latest_part_rowid_for_session("ses_child")
        .expect("capture delegation evidence boundary");

    let assistant = zuno_db::message::MessageRecord::from_json(json!({
        "id": "msg_child_report",
        "sessionID": "ses_child",
        "role": "assistant",
        "time": {"created": 10, "completed": 11},
        "parentID": "msg_parent",
        "modelID": "model",
        "providerID": "provider",
        "mode": "worker",
        "agent": "worker",
        "path": {"cwd": "/tmp/proj", "root": "/tmp/proj"},
        "cost": 0,
        "tokens": {
            "input": 3,
            "output": 2,
            "reasoning": 1,
            "cache": {"read": 0, "write": 0}
        },
        "finish": "stop"
    }))
    .expect("assistant message");
    let tool = zuno_db::message::PartRecord::from_json(
        json!({
            "id": "prt_child_report",
            "sessionID": "ses_child",
            "messageID": assistant.id,
            "type": "tool",
            "callID": "call_verify",
            "tool": "shell",
            "state": {
                "status": "completed",
                "input": {"command": "cargo test -p changed"},
                "output": "ok",
                "metadata": {
                    "writtenPaths": ["/tmp/proj/src/b.rs", "/tmp/proj/src/a.rs"],
                    "taskVerification": {
                        "name": "changed-crate tests",
                        "status": "passed",
                        "evidence": "cargo test -p changed"
                    },
                    "uncertainSideEffects": ["remote acknowledgement was not observed"]
                }
            }
        }),
        11,
    )
    .expect("tool part");
    store.put_message(&assistant).expect("persist assistant");
    store.put_part_at(&tool, 11).expect("persist tool part");

    let metadata = task_report_metadata(
        fixture.host.database.as_ref(),
        &fixture.request("ses_owner"),
        TaskReportBuild {
            job_id: Some("job_report"),
            work_context: None,
            child_session_id: "ses_child",
            evidence_start_rowid,
            status: "completed",
            final_text: "done",
            uncertain_side_effects: Vec::new(),
        },
    );
    let metadata = serde_json::to_value(metadata).expect("serialize report metadata");

    assert_eq!(metadata["jobId"], "job_report");
    assert_eq!(
        metadata["changedPaths"],
        json!(["/tmp/proj/src/a.rs", "/tmp/proj/src/b.rs"])
    );
    assert_eq!(
        metadata["verificationRecords"],
        json!([{
            "name": "changed-crate tests",
            "status": "passed",
            "evidence": "cargo test -p changed"
        }])
    );
    assert_eq!(
        metadata["uncertainSideEffects"],
        json!(["remote acknowledgement was not observed"])
    );
    assert_eq!(metadata["evidenceErrors"], json!([]));
}

#[test]
fn recovered_task_report_uses_the_child_agent_not_the_parent_attempt_agent() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.session("ses_child", Some("ses_owner"));
    fixture
        .connection()
        .execute(
            "UPDATE session SET agent = 'deep' WHERE id = 'ses_child'",
            (),
        )
        .expect("record the effective child agent");
    let parent_attempt: AttemptSnapshot = serde_json::from_value(json!({
        "schemaVersion": 4,
        "turnId": "turn-parent",
        "step": 1,
        "capability": {
            "schemaVersion": 4,
            "pack": {"id":"test","version":"1","upstreamRevision":"test"},
            "extensionRevision": 0,
            "permissionPolicySha256": "policy",
            "sandbox": {
                "mode": "workspace-write",
                "network": "deny",
                "writableRoots": [],
                "protectedPaths": []
            },
            "profiles": [], "presets": [], "councils": [], "workflows": [], "skills": []
        },
        "owner": {
            "sessionId":"ses_owner", "parentSessionId":null, "parentAttempt":null,
            "workflow":null, "workflowNode":null
        },
        "agent": {
            "name":"orchestrator", "sourceId":"test://orchestrator",
            "definitionSha256":"definition", "permissionSha256":"permission",
            "promptPolicySha256":"prompt"
        },
        "model": {
            "providerId":"fake", "modelId":"fake-model", "wireModelId":"fake-model",
            "surface":"responses", "reasoningSha256":"reasoning", "preset":null
        },
        "selectedSkills": [],
        "prompt": {"eventId":"evt-parent","assemblySha256":"assembly","actualSha256":"actual"},
        "tools": []
    }))
    .expect("parent attempt snapshot");
    let job = fixture
        .host
        .job_store
        .create(
            NewAgentJob::new(
                "job_recovered_agent",
                "ses_owner",
                JobSubject::child_session("ses_child"),
                DbReportDelivery::NextStep,
                10,
            )
            .queued()
            .with_orchestration_snapshot(Some(parent_attempt)),
        )
        .expect("create child job");

    let metadata = task_report_metadata_for_job(
        fixture.host.database.as_ref(),
        &job,
        "cancelled",
        "recovered",
        Vec::new(),
    )
    .expect("child report metadata");

    assert_eq!(metadata.agent, "deep");
}

#[derive(Default)]
struct RecordingWake {
    reports: Mutex<Vec<zuno_db::inbox::SessionInput>>,
    failure: Mutex<Option<String>>,
    transient_failures: Mutex<Vec<String>>,
    attempts: AtomicUsize,
}

#[derive(Default)]
struct PromotingInputDriver {
    inbox: Option<zuno_db::inbox::SessionInbox>,
    seen: Mutex<Vec<zuno_db::inbox::SessionInput>>,
}

impl PromotingInputDriver {
    fn new(inbox: zuno_db::inbox::SessionInbox) -> Self {
        Self {
            inbox: Some(inbox),
            seen: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl zuno_engine::wake::PendingInputDriver for PromotingInputDriver {
    async fn drive(
        &self,
        input: zuno_db::inbox::SessionInput,
        _guard: zuno_engine::status::SessionRunGuard,
    ) -> Result<(), String> {
        self.seen
            .lock()
            .expect("seen input lock")
            .push(input.clone());
        self.inbox
            .as_ref()
            .expect("fixture inbox")
            .promote_id(&input.session_id, &input.id)
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn interactive_input_fixture() -> (
    Arc<zuno_db::pool::Pool>,
    zuno_db::inbox::SessionInbox,
    BackgroundJobSupervisor,
) {
    let pool = Arc::new(
        zuno_db::pool::Pool::open(&DbLocation::Memory).expect("open interactive input database"),
    );
    {
        let mut connection = pool.get().expect("interactive input connection");
        zuno_db::migration::apply(&mut connection).expect("migrate interactive input database");
        connection
            .execute_batch(
                "INSERT INTO project \
                   (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-interactive-child', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, \
                    time_created, time_updated) \
                 VALUES ('ses_parent', 'project-interactive-child', 'parent', \
                         '/workspace', 'parent', '1', 1, 1);
                 INSERT INTO session \
                   (id, project_id, parent_id, slug, directory, title, version, \
                    time_created, time_updated) \
                 VALUES ('ses_child', 'project-interactive-child', 'ses_parent', 'child', \
                         '/workspace', 'child', '1', 1, 1);",
            )
            .expect("seed interactive child sessions");
    }
    let inbox = zuno_db::inbox::SessionInbox::new(Arc::clone(&pool));
    (pool, inbox, BackgroundJobSupervisor::default())
}

#[tokio::test]
async fn interactive_child_input_targets_only_the_child_and_steers_an_active_turn() {
    let (pool, inbox, jobs) = interactive_input_fixture();
    let runs = zuno_engine::status::SessionRunRegistry::new();
    let guard = runs
        .begin_turn("ses_child")
        .expect("the child turn is active");
    let driver = Arc::new(PromotingInputDriver::new(inbox.clone()));
    let input = InteractiveChildInput::with_driver(pool, runs, jobs.clone(), driver);

    let input_id = input
        .submit_text(
            "ses_child",
            serde_json::json!({
                "kind": "tuiPrompt",
                "submission": {
                    "kind": "steer",
                    "data": {"kind": "text", "data": "change direction"}
                }
            }),
            "change direction".to_owned(),
            zuno_db::inbox::InputDelivery::Steer,
        )
        .expect("admit interactive child input");

    assert!(
        inbox
            .pending("ses_parent")
            .expect("parent inbox")
            .is_empty(),
        "the child composer leaked its message into the parent inbox"
    );
    let pending = inbox.pending("ses_child").expect("child inbox");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, input_id);
    assert_eq!(pending[0].delivery, zuno_db::inbox::InputDelivery::Steer);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !guard.soft_interrupt_signal().is_set() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the supervised delivery task must steer the active child");
    let delivered = guard.take_soft_interrupts_at_safe_point();
    assert_eq!(delivered.messages.len(), 1);
    assert_eq!(delivered.messages[0].content, "change direction");
    assert_eq!(
        delivered.messages[0].source,
        zuno_engine::interrupt::SoftInterruptSource::User
    );
    drop(guard);
    jobs.wait_all().await;
}

#[tokio::test]
async fn interactive_child_input_reopens_an_idle_child_through_the_pending_driver() {
    let (pool, inbox, jobs) = interactive_input_fixture();
    let runs = zuno_engine::status::SessionRunRegistry::new();
    let driver = Arc::new(PromotingInputDriver::new(inbox.clone()));
    let input = InteractiveChildInput::with_driver(pool, runs, jobs.clone(), driver.clone());

    let input_id = input
        .submit_text(
            "ses_child",
            serde_json::json!({
                "kind": "tuiPrompt",
                "submission": {
                    "kind": "steer",
                    "data": {"kind": "text", "data": "continue from here"}
                }
            }),
            "continue from here".to_owned(),
            zuno_db::inbox::InputDelivery::Queue,
        )
        .expect("admit idle child input");
    jobs.wait_all().await;

    let seen = driver.seen.lock().expect("seen input lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, input_id);
    assert_eq!(seen[0].session_id, "ses_child");
    drop(seen);
    assert!(inbox.pending("ses_child").expect("child inbox").is_empty());
    assert_eq!(
        inbox
            .get("ses_child", &input_id)
            .expect("stored input")
            .expect("input exists")
            .state,
        zuno_db::inbox::SubmissionState::Promoted
    );
}

impl RecordingWake {
    fn fail_with(&self, error: &str) {
        *self.failure.lock().expect("wake failure lock") = Some(error.to_owned());
    }

    fn fail_once_with(&self, error: &str) {
        self.transient_failures
            .lock()
            .expect("transient wake failure lock")
            .push(error.to_owned());
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Acquire)
    }

    async fn wait_for_attempts(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while self.attempts() < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("parent wake reached the expected attempt count");
    }
}

#[async_trait]
impl ParentReportWake for RecordingWake {
    async fn wake(&self, report: zuno_db::inbox::SessionInput) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        if let Some(error) = self
            .transient_failures
            .lock()
            .expect("transient wake failure lock")
            .pop()
        {
            return Err(error);
        }
        if let Some(error) = self.failure.lock().expect("wake failure lock").clone() {
            return Err(error);
        }
        self.reports.lock().expect("wake reports lock").push(report);
        Ok(())
    }
}

/// The measure the depth guard reads must come from the real `parent_id` chain.
///
/// `TaskTool` refuses at `depth >= subagent_depth`, so these exact numbers are what
/// makes the default one-hop bound one hop. A host answering `0` for everything would
/// pass every `zuno-tools` test and allow unbounded delegation in production.
#[tokio::test]
async fn delegation_depth_counts_real_parent_hops() {
    let fixture = Fixture::new();
    fixture.session("ses_root", None);
    fixture.session("ses_child", Some("ses_root"));
    fixture.session("ses_grandchild", Some("ses_child"));

    for (session, expected) in [("ses_root", 0), ("ses_child", 1), ("ses_grandchild", 2)] {
        assert_eq!(
            fixture
                .host
                .delegation_depth(session)
                .await
                .expect("the ancestry walk reads the real chain"),
            expected,
            "{session}"
        );
    }
}

/// `parent_id` has no foreign key, so a cycle is representable and must terminate.
#[tokio::test]
async fn a_cyclic_parent_chain_is_refused_rather_than_walked_forever() {
    let fixture = Fixture::new();
    fixture.session("ses_a", None);
    fixture.session("ses_b", Some("ses_a"));
    fixture
        .connection()
        .execute(
            "UPDATE session SET parent_id = 'ses_b' WHERE id = 'ses_a'",
            (),
        )
        .expect("forge a cycle");

    let error = fixture
        .host
        .delegation_depth("ses_a")
        .await
        .expect_err("a cycle cannot yield a depth");

    assert!(format!("{error}").contains("cycle"), "{error}");
}

/// A delegation creates a child row the parent owns, carrying the requested agent.
///
/// The `parent_id` is what makes the depth guard work on the next hop, and the title
/// is what makes a delegated session findable; both are set here or nowhere.
#[tokio::test]
async fn a_fresh_delegation_creates_a_child_session_owned_by_its_parent() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.runner.complete_with(Ok("done"));

    let child = fixture
        .host
        .dispatch(fixture.request("ses_owner"), no_interrupt())
        .await
        .expect("a fresh delegation creates a child")
        .session_id;

    let stored = zuno_db::session::get(&fixture.connection(), &child).expect("the child persisted");
    assert_eq!(stored.parent_id.as_deref(), Some("ses_owner"));
    assert_eq!(stored.agent.as_deref(), Some("worker"));
    assert_eq!(stored.title, "a delegated unit");
    assert_eq!(
        fixture
            .host
            .delegation_depth(&child)
            .await
            .expect("the child's own depth is readable"),
        1,
        "the row a delegation writes must be the row the depth guard then reads"
    );
}

#[tokio::test]
async fn a_child_continuation_identity_survives_process_local_cache_loss() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let request = fixture.request("ses_owner");
    let child = fixture.persist_child_for_request(&request);
    let spec = ChildSessionSpec::resolved(
        &request,
        "worker",
        "test-provider/test-model",
        Some(zuno_llm::effort::ReasoningEffort::High),
    );
    persist_child_session_spec(
        &fixture.host.database,
        &child,
        &spec,
        &CancellationToken::new(),
    )
    .await
    .expect("persist the continuation identity");

    let restarted = ChildSessionSpecs::default();
    let restored = restarted
        .get_or_restore(&fixture.host.database, &child)
        .expect("a new process restores the child identity from SQLite");

    assert_eq!(restored, spec);
    assert_eq!(restored.parent_session_id, "ses_owner");
    assert_eq!(restored.agent, "worker");
    assert_eq!(restored.model, "test-provider/test-model");
}

#[tokio::test]
async fn resumed_child_rejects_identity_drift_without_rewriting_durable_metadata() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.parent_attempt = Some(Arc::new(parent_attempt("turn-original", 7)));
    request.effort = Some(zuno_llm::effort::ReasoningEffort::High);
    request.provider_options = serde_json::from_value(json!({
        "vendor": {"mode": "stable"}
    }))
    .expect("provider options");
    let child = fixture.persist_child_for_request(&request);
    let original = ChildSessionSpec::resolved(
        &request,
        "worker",
        "provider-a/model-a",
        Some(zuno_llm::effort::ReasoningEffort::High),
    );
    persist_child_session_spec(
        &fixture.host.database,
        &child,
        &original,
        &CancellationToken::new(),
    )
    .await
    .expect("persist the original continuation identity");
    let baseline = zuno_db::session::get(&fixture.connection(), &child)
        .expect("stored child")
        .metadata
        .expect("continuation metadata");

    let mut candidates = Vec::new();
    let mut changed_parent = original.clone();
    changed_parent.parent_session_id = "ses_other".to_owned();
    candidates.push(("parent", changed_parent));
    let mut changed_agent = original.clone();
    changed_agent.agent = "deep".to_owned();
    candidates.push(("agent", changed_agent));
    let mut changed_model = original.clone();
    changed_model.model = "provider-b/model-b".to_owned();
    candidates.push(("effective provider/model", changed_model));
    let mut changed_reasoning = original.clone();
    changed_reasoning.effort = Some(zuno_llm::effort::ReasoningEffort::Low);
    candidates.push(("reasoning", changed_reasoning));
    let mut changed_policy = original.clone();
    changed_policy.subagent_model_policy_sha256 = Some("changed-policy".to_owned());
    candidates.push(("subagent model policy", changed_policy));
    let mut changed_capability = original.clone();
    changed_capability
        .parent_attempt
        .as_mut()
        .expect("parent attempt")
        .capability
        .extension_revision = 8;
    candidates.push(("parent capability generation", changed_capability));
    let mut changed_parent_attempt = original.clone();
    changed_parent_attempt
        .parent_attempt
        .as_mut()
        .expect("parent attempt")
        .agent
        .permission_sha256 = "changed-parent-authority".to_owned();
    candidates.push(("parent Attempt", changed_parent_attempt));

    for (field, candidate) in candidates {
        let error = checkpoint_child_session_spec(
            &fixture.host.database,
            &fixture.host.supervisor.children,
            &child,
            &candidate,
            true,
            &CancellationToken::new(),
        )
        .await
        .expect_err("identity drift must be rejected");
        assert!(error.contains(field), "{field}: {error}");

        let stored = zuno_db::session::get(&fixture.connection(), &child).expect("stored child");
        assert_eq!(
            stored.metadata.as_deref(),
            Some(baseline.as_str()),
            "{field} drift must not rewrite durable metadata"
        );
        assert_eq!(stored.agent.as_deref(), Some("worker"));
    }
}

#[test]
fn child_continuation_checkpoint_backoff_is_positive_capped_and_honors_retry_after() {
    assert_eq!(
        child_session_metadata_retry_delay(1, None),
        Duration::from_millis(25)
    );
    assert_eq!(
        child_session_metadata_retry_delay(2, None),
        Duration::from_millis(50)
    );
    assert_eq!(
        child_session_metadata_retry_delay(u32::MAX, None),
        Duration::from_millis(250)
    );
    assert_eq!(
        child_session_metadata_retry_delay(1, Some(Duration::ZERO)),
        Duration::from_millis(25)
    );
    assert_eq!(
        child_session_metadata_retry_delay(1, Some(Duration::from_millis(90))),
        Duration::from_millis(90)
    );
    assert_eq!(
        child_session_metadata_retry_delay(1, Some(Duration::from_secs(30))),
        Duration::from_millis(250)
    );
}

/// `task_id` must name a child of *this* session, not any session at all.
///
/// Accepting a stranger's child would let one delegation continue a session its caller
/// was never handed, which is both a confusing transcript and a write into someone
/// else's history.
#[test]
fn resuming_another_parents_child_is_refused_by_name() {
    let fixture = Fixture::new();
    fixture.session("ses_mine", None);
    fixture.session("ses_theirs", None);
    fixture.session("ses_their_child", Some("ses_theirs"));

    let mut request = fixture.request("ses_mine");
    request.resume_session_id = Some("ses_their_child".to_owned());
    let error = fixture
        .host
        .session_admission_for(&request)
        .expect_err("another parent's child is not resumable here");

    assert!(
        matches!(error, ChildTurnError::UnknownSession(ref id) if id == "ses_their_child"),
        "{error}"
    );
    assert!(
        format!("{error}").contains("drop `task_id`"),
        "the refusal must name the fix: {error}"
    );
}

#[test]
fn resuming_an_absent_session_is_refused_rather_than_silently_creating_one() {
    let fixture = Fixture::new();
    fixture.session("ses_mine", None);

    let mut request = fixture.request("ses_mine");
    request.resume_session_id = Some("ses_never_existed".to_owned());
    let error = fixture
        .host
        .session_admission_for(&request)
        .expect_err("an absent session is not resumable");

    assert!(
        matches!(error, ChildTurnError::UnknownSession(_)),
        "{error}"
    );
}

#[test]
fn resuming_this_parents_own_child_reuses_it_rather_than_forking() {
    let fixture = Fixture::new();
    fixture.session("ses_mine", None);
    fixture.session("ses_my_child", Some("ses_mine"));

    let mut request = fixture.request("ses_mine");
    request.resume_session_id = Some("ses_my_child".to_owned());

    assert_eq!(
        fixture
            .host
            .session_admission_for(&request)
            .expect("this parent's own child resumes")
            .session_id,
        "ses_my_child"
    );
}

#[tokio::test]
async fn foreground_resume_refuses_a_child_with_an_unreconciled_background_job() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.session("ses_child", Some("ses_owner"));
    fixture
        .host
        .job_store
        .create_child_if_reconciled(NewAgentJob::new(
            "job_live",
            "ses_owner",
            JobSubject::child_session("ses_child"),
            DbReportDelivery::NextStep,
            1,
        ))
        .expect("create live child job");
    let mut request = fixture.request("ses_owner");
    request.resume_session_id = Some("ses_child".to_owned());
    fixture.runner.complete_with(Ok("must not run"));

    let error = fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect_err("the same child cannot run before reconciliation");

    assert!(format!("{error}").contains("job_live"), "{error}");
    assert_eq!(fixture.runner.started(), 0);
}

#[tokio::test]
async fn foreground_dispatch_returns_the_same_host_generated_report_shape_as_background() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.runner.complete_with(Ok("foreground answer"));

    let turn = fixture
        .host
        .dispatch(fixture.request("ses_owner"), no_interrupt())
        .await
        .expect("foreground delegation completes");
    let report = turn
        .report_metadata
        .expect("foreground terminal report metadata");

    assert_eq!(turn.job_id, None);
    assert_eq!(turn.output, "foreground answer");
    assert_eq!(report["schemaVersion"], 2);
    assert!(
        report["jobId"]
            .as_str()
            .is_some_and(|job_id| job_id.starts_with("job_")),
        "{report}"
    );
    assert_eq!(report["sessionId"], turn.session_id);
    assert_eq!(report["parentSessionId"], "ses_owner");
    assert_eq!(report["agent"], "worker");
    assert_eq!(report["status"], "completed");
    assert_eq!(report["finalText"], "foreground answer");
    assert_eq!(report["changedPaths"], json!([]));
    assert_eq!(report["verificationRecords"], json!([]));
    assert_eq!(report["uncertainSideEffects"], json!([]));
}

#[tokio::test]
async fn child_admission_links_the_job_and_report_to_the_active_plan_step() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let plan = zuno_tools::WorkStateStore::new(Arc::clone(&fixture.host.database))
        .update_plan(
            "ses_owner",
            zuno_tools::PlanUpdateParams {
                expected_revision: None,
                goal_id: None,
                title: "Repair release automation".to_owned(),
                steps: vec![
                    zuno_tools::PlanStep {
                        id: "diagnose-actions".to_owned(),
                        title: "Diagnose GitHub Actions".to_owned(),
                        status: zuno_tools::PlanStepStatus::InProgress,
                    },
                    zuno_tools::PlanStep {
                        id: "verify-release".to_owned(),
                        title: "Verify the release".to_owned(),
                        status: zuno_tools::PlanStepStatus::Pending,
                    },
                ],
            },
        )
        .expect("seed active plan");
    fixture.runner.complete_with(Ok("diagnosis"));

    let turn = fixture
        .host
        .dispatch(fixture.request("ses_owner"), no_interrupt())
        .await
        .expect("foreground delegation completes");
    let report = turn
        .report_metadata
        .expect("foreground terminal report metadata");
    let jobs = fixture
        .host
        .job_store
        .list_for_parent("ses_owner")
        .expect("list parent jobs");
    let context = jobs[0]
        .work_context
        .as_ref()
        .expect("job has plan work context");

    assert_eq!(context.plan_id, plan.id);
    assert_eq!(context.plan_revision, plan.revision);
    assert_eq!(context.plan_step_id, "diagnose-actions");
    assert_eq!(
        report["workContext"],
        json!({
            "schemaVersion": 1,
            "goalId": null,
            "planId": context.plan_id,
            "planRevision": context.plan_revision,
            "planStepId": "diagnose-actions"
        })
    );
}

#[tokio::test]
async fn identical_foreground_delegations_run_once_and_create_one_child_session() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);

    let first_host = fixture.host.clone();
    let first_request = fixture.request("ses_owner");
    let first =
        tokio::spawn(async move { first_host.dispatch(first_request, no_interrupt()).await });
    fixture.runner.wait_for_starts(1).await;

    let duplicate = tokio::time::timeout(
        Duration::from_millis(100),
        fixture
            .host
            .dispatch(fixture.request("ses_owner"), no_interrupt()),
    )
    .await
    .expect("an active logical duplicate must be rejected without starting a child")
    .expect_err("the second foreground delegation is the same logical task");
    assert!(
        duplicate
            .to_string()
            .contains("logical task `delegation:v1:test`"),
        "{duplicate}"
    );
    assert_eq!(fixture.runner.started(), 1);
    assert_eq!(
        zuno_db::session::children(&fixture.connection(), "ses_owner")
            .expect("read children")
            .len(),
        1
    );

    fixture.runner.complete_with(Ok("foreground answer"));
    first
        .await
        .expect("foreground task joined")
        .expect("first logical delegation completes");
}

#[tokio::test]
async fn sequential_foreground_duplicates_in_one_provider_attempt_run_once() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let attempt = Arc::new(parent_attempt("turn-one-provider-response", 7));
    let mut request = fixture.request("ses_owner");
    request.parent_attempt = Some(Arc::clone(&attempt));

    fixture.runner.complete_with(Ok("foreground answer"));
    fixture
        .host
        .dispatch(request.clone(), no_interrupt())
        .await
        .expect("the first foreground delegation completes");

    fixture.runner.complete_with(Ok("must not run twice"));
    let duplicate = fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect_err("the same provider attempt cannot repeat a completed logical task");
    assert!(
        duplicate
            .to_string()
            .contains("logical task `delegation:v1:test`"),
        "{duplicate}"
    );
    assert_eq!(fixture.runner.started(), 1);
    assert_eq!(
        zuno_db::session::children(&fixture.connection(), "ses_owner")
            .expect("read children")
            .len(),
        1
    );
}

#[tokio::test]
async fn foreground_dispatch_propagates_parent_interrupt_and_waits_for_runner_exit() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let interrupt = Arc::new(InterruptSignal::new());
    let fire = Arc::clone(&interrupt);
    let host = fixture.host.clone();
    let request = fixture.request("ses_owner");
    let task =
        tokio::spawn(async move { ChildTurnHost::dispatch(&host, request, interrupt).await });

    fixture.runner.wait_for_starts(1).await;
    fire.fire();

    let turn = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("foreground cancellation must settle")
        .expect("dispatch task remains attached")
        .expect("an interrupted child returns its durable terminal report");
    assert_eq!(turn.state, zuno_tools::task::ChildTurnState::Cancelled);
    assert!(turn.output.contains("cancelled"), "{}", turn.output);
    assert_eq!(
        turn.report_metadata
            .as_ref()
            .and_then(|report| report["status"].as_str()),
        Some("cancelled")
    );
}

#[tokio::test]
async fn foreground_cancellation_cannot_be_reclassified_as_successful_completion() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let runner = Arc::new(SuccessfulCancellationRunner::default());
    let host = ChildSessionHost::with_components(
        fixture.database.clone(),
        runner.clone(),
        fixture.wake.clone(),
        fixture.jobs.delegation_limiter(
            NonZeroUsize::new(8).expect("fixture delegation limit is non-zero"),
        ),
        fixture.jobs.clone(),
    )
    .expect("build cancellation-success child host");
    let interrupt = Arc::new(InterruptSignal::new());
    let fire = Arc::clone(&interrupt);
    let task_host = host.clone();
    let request = fixture.request("ses_owner");
    let task =
        tokio::spawn(async move { ChildTurnHost::dispatch(&task_host, request, interrupt).await });

    runner.started.notified().await;
    fire.fire();
    let turn = task
        .await
        .expect("foreground task joined")
        .expect("cancellation returns its durable terminal report");

    assert_eq!(turn.state, zuno_tools::task::ChildTurnState::Cancelled);
    assert_eq!(turn.output, "cleanup completed");
    let jobs = host
        .job_store
        .list_for_parent("ses_owner")
        .expect("list foreground child jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, zuno_db::job::JobStatus::Cancelled);
}

#[tokio::test]
async fn foreground_child_failure_remains_a_tool_failure_after_job_settlement() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let host = fixture.host.clone();
    let request = fixture.request("ses_owner");
    let task =
        tokio::spawn(async move { ChildTurnHost::dispatch(&host, request, no_interrupt()).await });

    fixture.runner.wait_for_starts(1).await;
    fixture.runner.complete_with(Err("provider failed"));
    let error = task
        .await
        .expect("foreground task joined")
        .expect_err("a failed child must remain a failed task tool");
    assert!(error.to_string().contains("provider failed"), "{error}");
    let jobs = fixture
        .host
        .job_store
        .list_for_parent("ses_owner")
        .expect("list foreground child jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, zuno_db::job::JobStatus::Failed);
    assert_eq!(jobs[0].error.as_deref(), Some("provider failed"));
}

#[tokio::test]
async fn dropping_the_outer_task_future_still_settles_the_foreground_child_job() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let host = fixture.host.clone();
    let request = fixture.request("ses_owner");
    let task =
        tokio::spawn(async move { ChildTurnHost::dispatch(&host, request, no_interrupt()).await });

    fixture.runner.wait_for_starts(1).await;
    let jobs = fixture
        .host
        .job_store
        .list_for_parent("ses_owner")
        .expect("list foreground child jobs");
    assert_eq!(jobs.len(), 1, "one foreground child job");
    let job_id = jobs[0].id.clone();
    task.abort();
    let _aborted = task.await;

    tokio::time::timeout(Duration::from_secs(1), fixture.jobs.wait_all())
        .await
        .expect("the independent child supervisor must settle after its caller is dropped");
    assert_eq!(
        fixture
            .host
            .job_store
            .get(&job_id)
            .expect("settled foreground job")
            .status,
        zuno_db::job::JobStatus::Cancelled,
        "dropping TaskTool left a durable job permanently running"
    );
}

#[tokio::test(start_paused = true)]
async fn an_unresponsive_foreground_child_becomes_uncertain_after_the_cancel_deadline() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let runner = Arc::new(StubbornRunner::default());
    let host = ChildSessionHost::with_components(
        fixture.database.clone(),
        runner.clone(),
        fixture.wake.clone(),
        fixture.jobs.delegation_limiter(
            NonZeroUsize::new(8).expect("fixture delegation limit is non-zero"),
        ),
        fixture.jobs.clone(),
    )
    .expect("build stubborn child host");
    let task_host = host.clone();
    let request = fixture.request("ses_owner");
    let task =
        tokio::spawn(
            async move { ChildTurnHost::dispatch(&task_host, request, no_interrupt()).await },
        );

    runner.started.notified().await;
    let jobs = host
        .job_store
        .list_for_parent("ses_owner")
        .expect("list foreground child jobs");
    assert_eq!(jobs.len(), 1, "one foreground child job");
    let job_id = jobs[0].id.clone();
    task.abort();
    let _aborted = task.await;

    tokio::time::advance(Duration::from_secs(10) + Duration::from_millis(1)).await;
    fixture.jobs.wait_all().await;
    assert_eq!(
        host.job_store
            .get(&job_id)
            .expect("terminal foreground job")
            .status,
        zuno_db::job::JobStatus::Uncertain,
        "an unresponsive child remained running after the cancellation safety deadline"
    );
}

#[tokio::test]
async fn background_supervisor_reports_a_live_writer_until_it_finishes() {
    let jobs = BackgroundJobSupervisor::default();
    let (release, waiting) = tokio::sync::oneshot::channel();
    jobs.spawn(
        "job_test",
        "ses_test",
        CancellationToken::new(),
        async move {
            let _released = waiting.await;
        },
    );
    tokio::task::yield_now().await;

    assert!(jobs.has_running_tasks("ses_test"));
    release.send(()).expect("release background task");
    jobs.wait_all().await;
    assert!(!jobs.has_running_tasks("ses_test"));
}

#[tokio::test]
async fn background_supervisor_cancels_every_owned_task_before_waiting() {
    let jobs = BackgroundJobSupervisor::default();
    for (job, session) in [("job_one", "ses_one"), ("job_two", "ses_two")] {
        let cancellation = CancellationToken::new();
        let cancelled = cancellation.clone();
        jobs.spawn(job, session, cancellation, async move {
            cancelled.cancelled().await;
        });
    }
    tokio::task::yield_now().await;

    jobs.cancel_all();
    jobs.wait_all().await;

    assert!(!jobs.has_running_tasks("ses_one"));
    assert!(!jobs.has_running_tasks("ses_two"));
}

#[tokio::test]
async fn background_supervisor_cancels_and_joins_only_one_parent_session() {
    let jobs = BackgroundJobSupervisor::default();
    for (job, session) in [("job_one", "ses_one"), ("job_two", "ses_two")] {
        let cancellation = CancellationToken::new();
        let cancelled = cancellation.clone();
        jobs.spawn(job, session, cancellation, async move {
            cancelled.cancelled().await;
        });
    }
    tokio::task::yield_now().await;

    jobs.cancel_for_parent("ses_one");
    jobs.wait_for_parent("ses_one").await;

    assert!(!jobs.has_running_tasks("ses_one"));
    assert!(
        jobs.has_running_tasks("ses_two"),
        "closing one ACP session must not cancel another root session's work"
    );

    jobs.cancel_all();
    jobs.wait_all().await;
}

struct TaskDropFlag(Arc<AtomicBool>);

impl Drop for TaskDropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn supervised_handle_is_aborted_and_joined_on_process_shutdown() {
    let jobs = BackgroundJobSupervisor::default();
    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let task_entered = Arc::clone(&entered);
    let task_dropped = Arc::clone(&dropped);
    let task = tokio::spawn(async move {
        let _drop = TaskDropFlag(task_dropped);
        task_entered.notify_one();
        pending::<()>().await;
    });
    jobs.supervise_handle(
        "reflection_test",
        "ses_test",
        CancellationToken::new(),
        task,
    );
    entered.notified().await;

    jobs.cancel_all();
    jobs.wait_all().await;

    assert!(dropped.load(Ordering::Acquire));
    assert!(!jobs.has_running_tasks("ses_test"));
}

#[tokio::test]
async fn background_dispatch_returns_a_durable_active_job_before_the_child_finishes() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;

    let turn = fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect("background delegation is admitted");
    let job_id = turn.job_id.expect("background job id");
    assert_ne!(job_id, turn.session_id);
    assert!(
        matches!(
            fixture
                .host
                .job_store
                .get(&job_id)
                .expect("active job")
                .status,
            zuno_db::job::JobStatus::Queued | zuno_db::job::JobStatus::Running
        ),
        "the durable job must exist before the child finishes"
    );
    assert_eq!(
        zuno_db::session::children(&fixture.connection(), "ses_owner")
            .expect("read children")
            .len(),
        1
    );

    fixture.runner.wait_for_starts(1).await;
    assert_eq!(
        fixture
            .host
            .job_store
            .get(&job_id)
            .expect("started job")
            .status,
        zuno_db::job::JobStatus::Running
    );

    fixture.runner.complete_with(Ok("child answer"));
    fixture.jobs.wait_all().await;
    let settled = fixture.host.job_store.get(&job_id).expect("settled job");
    assert_eq!(settled.status, zuno_db::job::JobStatus::Completed);
    assert_eq!(
        settled
            .result
            .as_ref()
            .and_then(|value| value["finalText"].as_str()),
        Some("child answer")
    );
    let result = settled.result.as_ref().expect("task report metadata");
    assert_eq!(result["schemaVersion"], 2);
    assert_eq!(result["jobId"], job_id);
    assert_eq!(result["sessionId"], turn.session_id);
    assert_eq!(result["parentSessionId"], "ses_owner");
    assert_eq!(result["agent"], "worker");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["changedPaths"], json!([]));
    assert_eq!(result["verificationRecords"], json!([]));
    assert_eq!(result["uncertainSideEffects"], json!([]));
    let reports = fixture.wake.reports.lock().expect("wake reports");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].prompt["metadata"], *result);
    drop(reports);
    assert!(
        fixture
            .host
            .inbox
            .pending("ses_owner")
            .expect("pending report")
            .iter()
            .any(|input| input.id == settled.report_input_id.as_deref().expect("report input id"))
    );
}

#[tokio::test]
async fn an_admitted_background_child_outlives_a_parent_turn_interrupt() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;
    request.report_delivery = ReportDelivery::Quiet;
    let interrupt = Arc::new(InterruptSignal::new());

    let turn = fixture
        .host
        .dispatch(request, interrupt.clone())
        .await
        .expect("admit background child");
    let job_id = turn.job_id.expect("background job id");
    fixture.runner.wait_for_starts(1).await;
    interrupt.fire();
    tokio::task::yield_now().await;

    assert_eq!(
        fixture
            .host
            .job_store
            .get(&job_id)
            .expect("background job remains durable")
            .status,
        zuno_db::job::JobStatus::Running,
        "a parent turn interrupt cancelled independent background work"
    );
    fixture.runner.complete_with(Ok("background answer"));
    fixture.jobs.wait_all().await;
    assert_eq!(
        fixture
            .host
            .job_store
            .get(&job_id)
            .expect("settled background job")
            .status,
        zuno_db::job::JobStatus::Completed
    );
}

#[tokio::test]
async fn rejected_background_logical_duplicate_leaves_no_orphan_child_session() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.session("ses_existing_child", Some("ses_owner"));
    fixture
        .host
        .job_store
        .create_child_if_reconciled(
            zuno_db::job::NewAgentJob::new(
                "job_existing",
                "ses_owner",
                zuno_db::job::JobSubject::child_session("ses_existing_child"),
                zuno_db::job::ReportDelivery::NextStep,
                10,
            )
            .with_logical_key("delegation:v1:test"),
        )
        .expect("seed active logical task");
    let before = zuno_db::session::children(&fixture.connection(), "ses_owner")
        .expect("read children before duplicate")
        .len();

    let mut request = fixture.request("ses_owner");
    request.background = true;
    let duplicate = fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect_err("the active logical task blocks a fresh background child");
    assert!(
        duplicate
            .to_string()
            .contains("logical task `delegation:v1:test`"),
        "{duplicate}"
    );
    assert_eq!(
        zuno_db::session::children(&fixture.connection(), "ses_owner")
            .expect("read children after duplicate")
            .len(),
        before,
        "a rejected admission must roll back its speculative child session"
    );
    assert_eq!(fixture.runner.started(), 0);
}

#[tokio::test]
async fn background_children_share_the_workspace_delegation_bound() {
    let fixture = Fixture::with_limit(1);
    fixture.session("ses_owner", None);
    let mut first = fixture.request("ses_owner");
    first.background = true;
    first.report_delivery = ReportDelivery::Quiet;
    let mut second = fixture.request("ses_owner");
    second.logical_key = "delegation:v1:test-second".to_owned();
    second.background = true;
    second.report_delivery = ReportDelivery::Quiet;

    let first = fixture
        .host
        .dispatch(first, no_interrupt())
        .await
        .expect("first dispatch");
    let second = fixture
        .host
        .dispatch(second, no_interrupt())
        .await
        .expect("second dispatch");
    fixture.runner.wait_for_starts(1).await;
    assert_eq!(
        fixture
            .host
            .job_store
            .get(first.job_id.as_deref().expect("first job id"))
            .expect("first job")
            .status,
        zuno_db::job::JobStatus::Running
    );
    assert_eq!(
        fixture
            .host
            .job_store
            .get(second.job_id.as_deref().expect("second job id"))
            .expect("second job")
            .status,
        zuno_db::job::JobStatus::Queued
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), fixture.runner.wait_for_starts(2))
            .await
            .is_err(),
        "a second child started while the only delegation slot was occupied"
    );

    fixture.runner.complete_with(Ok("first answer"));
    fixture.runner.wait_for_starts(2).await;
    fixture.runner.complete_with(Ok("second answer"));
    fixture.jobs.wait_all().await;

    for turn in [first, second] {
        let job = fixture
            .host
            .job_store
            .get(turn.job_id.as_deref().expect("job id"))
            .expect("settled job");
        assert_eq!(job.status, zuno_db::job::JobStatus::Completed);
    }
}

#[tokio::test]
async fn a_queued_background_child_can_be_cancelled_without_starting() {
    let fixture = Fixture::with_limit(1);
    fixture.session("ses_owner", None);
    let mut first = fixture.request("ses_owner");
    first.background = true;
    first.report_delivery = ReportDelivery::Quiet;
    let mut queued = fixture.request("ses_owner");
    queued.logical_key = "delegation:v1:test-queued".to_owned();
    queued.background = true;

    let first = fixture
        .host
        .dispatch(first, no_interrupt())
        .await
        .expect("first dispatch");
    let queued = fixture
        .host
        .dispatch(queued, no_interrupt())
        .await
        .expect("queued dispatch");
    fixture.runner.wait_for_starts(1).await;
    let queued_job_id = queued.job_id.expect("queued job id");
    assert_eq!(
        fixture
            .host
            .job_store
            .get(&queued_job_id)
            .expect("queued job")
            .status,
        zuno_db::job::JobStatus::Queued
    );

    assert!(fixture.jobs.cancel("ses_owner", &queued_job_id));
    fixture.runner.complete_with(Ok("first answer"));
    fixture.jobs.wait_all().await;

    assert_eq!(
        fixture.runner.started(),
        1,
        "the queued child must never run"
    );
    assert_eq!(
        fixture
            .host
            .job_store
            .get(&queued_job_id)
            .expect("cancelled queued job")
            .status,
        zuno_db::job::JobStatus::Cancelled
    );
    assert_eq!(
        fixture
            .host
            .job_store
            .get(first.job_id.as_deref().expect("first job id"))
            .expect("first job")
            .status,
        zuno_db::job::JobStatus::Completed
    );
}

#[tokio::test]
async fn quiet_background_dispatch_persists_the_result_without_waking_the_parent() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;
    request.report_delivery = ReportDelivery::Quiet;

    let turn = fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect("dispatch");
    fixture.runner.complete_with(Ok("quiet answer"));
    fixture.jobs.wait_all().await;

    let job = fixture
        .host
        .job_store
        .get(turn.job_id.as_deref().expect("job id"))
        .expect("settled job");
    assert_eq!(job.status, zuno_db::job::JobStatus::Completed);
    assert_eq!(job.report_input_id, None);
    assert!(
        fixture
            .wake
            .reports
            .lock()
            .expect("wake reports")
            .is_empty()
    );
    assert!(
        fixture
            .host
            .inbox
            .pending("ses_owner")
            .expect("pending")
            .is_empty()
    );
}

#[tokio::test]
async fn cancelling_a_native_background_job_keeps_the_host_alive_and_settles_cancelled() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;

    let turn = fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect("dispatch");
    let job_id = turn.job_id.expect("job id");
    assert!(fixture.jobs.cancel("ses_owner", &job_id));
    fixture.jobs.wait_all().await;

    let job = fixture.host.job_store.get(&job_id).expect("cancelled job");
    assert_eq!(job.status, zuno_db::job::JobStatus::Cancelled);
    assert!(!fixture.jobs.has_running_tasks("ses_owner"));
    let reports = fixture.wake.reports.lock().expect("wake reports");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].prompt["status"], "cancelled");
}

#[tokio::test]
async fn a_failed_background_child_is_persisted_and_reported() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;

    let turn = fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect("dispatch");
    fixture.runner.complete_with(Err("provider failed"));
    fixture.jobs.wait_all().await;

    let job = fixture
        .host
        .job_store
        .get(turn.job_id.as_deref().expect("job id"))
        .expect("settled job");
    assert_eq!(job.status, zuno_db::job::JobStatus::Failed);
    assert_eq!(job.error.as_deref(), Some("provider failed"));
    let reports = fixture.wake.reports.lock().expect("wake reports");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].prompt["status"], "failed");
}

#[tokio::test]
async fn a_transient_parent_wake_failure_is_retried_in_the_same_process() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.wake.fail_once_with("temporary run-registry race");
    let mut request = fixture.request("ses_owner");
    request.background = true;

    fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect("dispatch");
    fixture.runner.complete_with(Ok("child answer"));
    fixture.jobs.wait_all().await;

    assert_eq!(fixture.wake.attempts(), 2);
    assert_eq!(fixture.wake.reports.lock().expect("wake reports").len(), 1);
}

#[tokio::test]
async fn parent_wake_is_bounded_and_the_same_report_can_be_recovered_later() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    for _ in 0..3 {
        fixture.wake.fail_once_with("temporary parent wake outage");
    }
    let mut request = fixture.request("ses_owner");
    request.background = true;

    fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect("dispatch");
    fixture.runner.complete_with(Ok("child answer"));
    fixture.jobs.wait_all().await;

    assert_eq!(fixture.wake.attempts(), 3);
    assert!(
        fixture
            .wake
            .reports
            .lock()
            .expect("wake reports")
            .is_empty()
    );
    assert_eq!(
        fixture
            .host
            .recover_pending_reports("ses_owner")
            .await
            .expect("schedule report recovery"),
        1
    );
    fixture.jobs.wait_all().await;

    assert_eq!(fixture.wake.attempts(), 4);
    assert_eq!(
        fixture.wake.reports.lock().expect("wake reports").len(),
        1,
        "the durable report id must be delivered only once"
    );
}

#[tokio::test]
async fn a_report_left_pending_by_process_loss_is_recovered_when_the_parent_reopens() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.wake.fail_with("process is stopping");
    let mut request = fixture.request("ses_owner");
    request.background = true;

    let turn = fixture
        .host
        .dispatch(request, no_interrupt())
        .await
        .expect("dispatch");
    fixture.runner.complete_with(Ok("survives restart"));
    fixture.wake.wait_for_attempts(1).await;
    let task = {
        let mut tasks = fixture
            .jobs
            .tasks
            .lock()
            .expect("background supervisor tasks");
        tasks
            .iter_mut()
            .find(|job| job.id == turn.job_id.as_deref().expect("job id"))
            .and_then(|job| job.task.take())
            .expect("live wake retry task")
    };
    task.abort();
    let _ = task.await;
    let job = fixture
        .host
        .job_store
        .get(turn.job_id.as_deref().expect("job id"))
        .expect("settled job");
    assert!(job.report_input_id.is_some());

    let recovered_wake = Arc::new(RecordingWake::default());
    let recovered_jobs = BackgroundJobSupervisor::default();
    let recovered = ChildSessionHost::with_components(
        fixture.database.clone(),
        Arc::new(RecordingRunner::default()),
        recovered_wake.clone(),
        recovered_jobs.delegation_limiter(
            NonZeroUsize::new(8).expect("default delegation limit is non-zero"),
        ),
        recovered_jobs.clone(),
    )
    .expect("reopen child host")
    .recover_pending_reports("ses_owner")
    .await
    .expect("recover reports");
    recovered_jobs.wait_all().await;

    assert_eq!(recovered, 1);
    assert_eq!(
        recovered_wake
            .reports
            .lock()
            .expect("recovered reports")
            .len(),
        1
    );
}

#[tokio::test]
async fn recovery_keeps_a_process_owned_background_child_running() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;

    let turn = ChildTurnHost::dispatch(&fixture.host, request, no_interrupt())
        .await
        .expect("dispatch background child");
    let job_id = turn.job_id.expect("background child job");
    fixture.runner.wait_for_starts(1).await;

    assert_eq!(
        fixture
            .host
            .recover_interrupted("ses_owner")
            .expect("inspect process-owned child jobs"),
        0
    );
    assert_eq!(
        fixture
            .host
            .job_store
            .get(&job_id)
            .expect("running job")
            .status,
        zuno_db::job::JobStatus::Running
    );

    fixture.jobs.cancel_all();
    fixture.jobs.wait_all().await;
}

#[tokio::test]
async fn peer_recovery_respects_a_live_owner_and_recovers_after_owner_loss() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;

    let turn = ChildTurnHost::dispatch(&fixture.host, request, no_interrupt())
        .await
        .expect("dispatch background child");
    let job_id = turn.job_id.expect("background child job");
    fixture.runner.wait_for_starts(1).await;

    let peer_jobs = BackgroundJobSupervisor::default();
    let peer_host = ChildSessionHost::with_components(
        fixture.database.clone(),
        Arc::new(RecordingRunner::default()),
        Arc::new(RecordingWake::default()),
        peer_jobs
            .delegation_limiter(NonZeroUsize::new(8).expect("peer delegation limit is non-zero")),
        peer_jobs,
    )
    .expect("open a second process-equivalent child host");

    assert_eq!(
        peer_host
            .recover_interrupted("ses_owner")
            .expect("a peer inspects active child jobs"),
        0,
        "a peer process must not reconcile work still owned by a live executor"
    );
    assert_eq!(
        peer_host.job_store.get(&job_id).expect("live job").status,
        zuno_db::job::JobStatus::Running
    );

    let task = {
        let mut tasks = fixture
            .jobs
            .tasks
            .lock()
            .expect("background supervisor tasks");
        tasks
            .iter_mut()
            .find(|job| job.id == job_id)
            .and_then(|job| job.task.take())
            .expect("live executor task")
    };
    task.abort();
    let _ = task.await;

    assert_eq!(
        peer_host
            .recover_interrupted("ses_owner")
            .expect("recover after the owning executor disappears"),
        1,
        "process loss must release ownership so durable recovery can settle the job"
    );
    assert_eq!(
        peer_host
            .job_store
            .get(&job_id)
            .expect("recovered job")
            .status,
        zuno_db::job::JobStatus::Uncertain
    );
}

#[tokio::test]
async fn restart_reconciliation_cancels_queued_children_and_marks_running_children_uncertain() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.session("ses_queued", Some("ses_owner"));
    fixture.session("ses_running", Some("ses_owner"));
    fixture
        .host
        .job_store
        .create(
            zuno_db::job::NewAgentJob::new(
                "job_queued",
                "ses_owner",
                zuno_db::job::JobSubject::child_session("ses_queued"),
                zuno_db::job::ReportDelivery::NextStep,
                10,
            )
            .queued(),
        )
        .expect("create queued child job");
    fixture
        .host
        .job_store
        .create(zuno_db::job::NewAgentJob::new(
            "job_running",
            "ses_owner",
            zuno_db::job::JobSubject::child_session("ses_running"),
            zuno_db::job::ReportDelivery::NextStep,
            11,
        ))
        .expect("create running child job");

    assert_eq!(
        fixture
            .host
            .recover_interrupted("ses_owner")
            .expect("reconcile child jobs"),
        2
    );
    assert_eq!(
        fixture
            .host
            .job_store
            .get("job_queued")
            .expect("queued job")
            .status,
        zuno_db::job::JobStatus::Cancelled
    );
    assert_eq!(
        fixture
            .host
            .job_store
            .get("job_running")
            .expect("running job")
            .status,
        zuno_db::job::JobStatus::Uncertain
    );
    assert_eq!(
        fixture
            .host
            .recover_pending_reports("ses_owner")
            .await
            .expect("wake recovered reports"),
        2
    );
    fixture.jobs.wait_all().await;
    let statuses = fixture
        .wake
        .reports
        .lock()
        .expect("wake reports")
        .iter()
        .map(|report| {
            report.prompt["status"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(statuses, ["cancelled", "uncertain"]);
}
