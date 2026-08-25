use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use zuno_db::job::{JobStatus, ReportDelivery as DbReportDelivery};
use zuno_paths::DbLocation;
use zuno_tools::council::{CouncilRequest, CouncilSeatRequest};
use zuno_tools::work_state::{WorkItemStatus, WorkStateStore};
use zuno_tools::workflow::WorkflowNodeRequest;

struct Fixture {
    _root: tempfile::TempDir,
    host: NativeWorkflowHost,
    supervisor: BackgroundJobSupervisor,
    jobs: AgentJobStore,
    work: WorkStateStore,
    runner: Arc<RecordingRunner>,
    synth: Arc<RecordingSynthesizer>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::TempDir::new().expect("temporary workflow root");
        let location = DbLocation::File(root.path().join("workflow.db"));
        let mut connection = zuno_db::open::open(&location).expect("open workflow database");
        zuno_db::migration::apply(&mut connection).expect("migrate workflow database");
        connection
            .execute(
                "INSERT INTO project (id, worktree, vcs, time_created, time_updated, sandboxes) \
                 VALUES ('proj', '/tmp/proj', NULL, 1, 1, '[]')",
                (),
            )
            .expect("seed project");
        let transaction = connection.transaction().expect("begin session seed");
        zuno_db::session::create(
            &transaction,
            &zuno_db::session::SessionCreate::new(
                "ses_parent",
                "ses_parent",
                "proj",
                "/tmp/proj",
                "/tmp/proj",
                "workflow fixture",
                crate::RUST_PACKAGE_VERSION,
            )
            .at(1),
        )
        .expect("seed parent session");
        transaction.commit().expect("commit session seed");
        drop(connection);

        let pool = Arc::new(zuno_db::Pool::open(&location).expect("workflow pool"));
        let runner = Arc::new(RecordingRunner::default());
        let synth = Arc::new(RecordingSynthesizer::default());
        let wake = Arc::new(RecordingWake::default());
        let supervisor = BackgroundJobSupervisor::default();
        let work = WorkStateStore::new(Arc::clone(&pool));
        let host = NativeWorkflowHost {
            runner: runner.clone(),
            database: Arc::clone(&pool),
            jobs: AgentJobStore::new(Arc::clone(&pool)),
            work: work.clone(),
            changes: supervisor.notifier(),
            wake,
            supervisor: supervisor.clone(),
            council_synth: synth.clone(),
        };
        Self {
            _root: root,
            host,
            supervisor,
            jobs: AgentJobStore::new(pool),
            work,
            runner,
            synth,
        }
    }

    fn request(&self, background: bool, nodes: Vec<WorkflowNodeRequest>) -> WorkflowRequest {
        WorkflowRequest {
            parent_session_id: "ses_parent".to_owned(),
            parent_attempt: None,
            workflow: "release".to_owned(),
            description: Some("release fixture".to_owned()),
            nodes,
            max_parallel: 2,
            background,
            report_delivery: ReportDelivery::Quiet,
        }
    }
}

#[derive(Default)]
struct RecordingSynthesizer {
    payloads: Mutex<Vec<String>>,
}

#[async_trait]
impl CouncilSynthesizer for RecordingSynthesizer {
    async fn synthesize(&self, _session_id: &str, payload: String) -> Result<String, String> {
        self.payloads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(payload);
        Ok("Decision\nShip with the recorded dissent.\n\nAgreement\nThe evidence is sufficient.\n\nDissent\nOne seat remains cautious.\n\nRisks\nSee the seat ledger.\n\nRecommendation\nProceed with verification."
            .to_owned())
    }
}

#[derive(Default)]
struct RecordingWake {
    reports: Mutex<Vec<zuno_db::inbox::SessionInput>>,
}

#[async_trait]
impl ParentReportWake for RecordingWake {
    async fn wake(&self, report: zuno_db::inbox::SessionInput) -> Result<(), String> {
        self.reports
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(report);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingRunner {
    active: AtomicUsize,
    max_active: AtomicUsize,
    events: Mutex<Vec<String>>,
    entered: tokio::sync::Notify,
    attempts: Mutex<BTreeMap<String, usize>>,
}

#[async_trait]
impl WorkflowNodeRunner for RecordingRunner {
    async fn run(
        &self,
        request: ChildTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<ChildTurn, String> {
        let label = request.description.unwrap_or_else(|| request.agent.clone());
        let attempt = {
            let mut attempts = self
                .attempts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let attempt = attempts.entry(label.clone()).or_insert(0);
            *attempt = attempt.saturating_add(1);
            *attempt
        };
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!("start:{label}"));
        self.entered.notify_one();

        if request.prompt == "wait" {
            cancellation.cancelled().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(format!("cancel:{label}"));
            return Err("cancelled".to_owned());
        }

        if request.prompt == "fail" {
            self.active.fetch_sub(1, Ordering::SeqCst);
            return Err("seat failed".to_owned());
        }

        let delay = if request.prompt.starts_with("slow") {
            60
        } else {
            10
        };
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(delay)) => {}
            () = cancellation.cancelled() => {
                self.active.fetch_sub(1, Ordering::SeqCst);
                return Err("cancelled".to_owned());
            }
        }
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!("end:{label}"));
        self.active.fetch_sub(1, Ordering::SeqCst);
        let output = match request.prompt.as_str() {
            "agree" | "slow-agree" => json!({
                "verdict":"approve",
                "confidence":0.9,
                "evidence":[format!("evidence from {label}")],
                "risks":["bounded risk"],
                "recommendation":"ship after verification"
            })
            .to_string(),
            "dissent" => json!({
                "verdict":"hold",
                "confidence":0.7,
                "evidence":["a dissenting constraint remains"],
                "risks":["compatibility uncertainty"],
                "recommendation":"resolve the dissent before release"
            })
            .to_string(),
            "invalid-once" if attempt > 1 => json!({
                "verdict":"approve after retry",
                "confidence":0.8,
                "evidence":["second attempt was structured"],
                "risks":[],
                "recommendation":"accept the recovered seat"
            })
            .to_string(),
            "invalid" | "invalid-once" => "not-json".to_owned(),
            _ => format!("result:{label}"),
        };
        Ok(ChildTurn {
            session_id: format!("ses_{label}_{attempt}"),
            job_id: None,
            output,
        })
    }
}

fn council_seat(id: &str, agent: &str, prompt: &str) -> CouncilSeatRequest {
    CouncilSeatRequest {
        id: id.to_owned(),
        turn: ChildTurnRequest {
            parent_session_id: "ses_parent".to_owned(),
            parent_attempt: None,
            workflow: Some("council:balanced-review".to_owned()),
            workflow_node: Some(id.to_owned()),
            resume_session_id: None,
            agent: agent.to_owned(),
            description: Some(id.to_owned()),
            prompt: prompt.to_owned(),
            model: None,
            effort: None,
            provider_options: serde_json::Map::new(),
            background: false,
            report_delivery: ReportDelivery::Quiet,
        },
    }
}

fn council_request(
    background: bool,
    seats: Vec<CouncilSeatRequest>,
    quorum: usize,
    max_parallel: usize,
    max_retries: usize,
    deadline: Duration,
) -> CouncilRequest {
    CouncilRequest {
        parent_session_id: "ses_parent".to_owned(),
        parent_attempt: None,
        preset: "balanced-review".to_owned(),
        description: Some("Council fixture".to_owned()),
        question: "Should this change ship?".to_owned(),
        seats,
        quorum,
        max_parallel,
        deadline,
        max_retries,
        seat_output_bytes: 16_384,
        synthesis_input_bytes: 32_768,
        background,
        report_delivery: ReportDelivery::Quiet,
    }
}

fn node(id: &str, depends_on: &[&str], prompt: &str) -> WorkflowNodeRequest {
    WorkflowNodeRequest {
        id: id.to_owned(),
        depends_on: depends_on.iter().map(|value| (*value).to_owned()).collect(),
        turn: ChildTurnRequest {
            parent_session_id: "ses_parent".to_owned(),
            parent_attempt: None,
            workflow: None,
            workflow_node: None,
            resume_session_id: None,
            agent: format!("agent-{id}"),
            description: Some(id.to_owned()),
            prompt: prompt.to_owned(),
            model: None,
            effort: None,
            provider_options: serde_json::Map::new(),
            background: false,
            report_delivery: ReportDelivery::Quiet,
        },
    }
}

#[tokio::test]
async fn independent_nodes_overlap_and_dependents_wait_with_stable_results() {
    let fixture = Fixture::new();
    let request = fixture.request(
        false,
        vec![
            node("scan", &[], "slow"),
            node("review", &[], "fast"),
            node("implement", &["scan", "review"], "fast"),
        ],
    );
    let turn = WorkflowHost::dispatch(&fixture.host, request, CancellationToken::new())
        .await
        .expect("workflow completes");

    assert_eq!(fixture.runner.max_active.load(Ordering::SeqCst), 2);
    let events = fixture
        .runner
        .events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let implement_start = events
        .iter()
        .position(|event| event == "start:implement")
        .expect("implement starts");
    assert!(
        events[..implement_start]
            .iter()
            .any(|event| event == "end:scan")
    );
    assert!(
        events[..implement_start]
            .iter()
            .any(|event| event == "end:review")
    );
    assert!(
        turn.output.find("### scan").expect("scan output")
            < turn.output.find("### review").expect("review output")
    );
    assert!(
        turn.output.find("### review").expect("review output")
            < turn.output.find("### implement").expect("implement output")
    );

    let items = fixture.work.items("ses_parent").expect("workflow items");
    assert_eq!(items.len(), 4, "one root plus three node WorkItems");
    let root = items
        .iter()
        .find(|item| item.parent_id.is_none())
        .expect("workflow root item");
    assert_eq!(root.status, WorkItemStatus::Completed);
    assert!(root.time_used_ms > 0);
    let nodes = items
        .iter()
        .filter(|item| item.parent_id.as_deref() == Some(root.id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(nodes.len(), 3);
    assert!(
        nodes
            .iter()
            .all(|item| item.status == WorkItemStatus::Completed)
    );
    assert!(nodes.iter().all(|item| item.time_used_ms > 0));
    assert!(
        nodes.iter().all(|item| !item.usage_known),
        "the fixture emits no provider usage, which must remain unknown rather than zero"
    );
    let implement = nodes
        .iter()
        .find(|item| item.subject == "implement")
        .expect("implement item");
    assert_eq!(implement.dependencies.len(), 2);
}

#[tokio::test]
async fn background_cancellation_propagates_and_settles_the_parent_job() {
    let fixture = Fixture::new();
    let turn = WorkflowHost::dispatch(
        &fixture.host,
        fixture.request(true, vec![node("wait", &[], "wait")]),
        CancellationToken::new(),
    )
    .await
    .expect("background workflow admitted");
    let job_id = turn.job_id.expect("durable workflow job");
    fixture.runner.entered.notified().await;
    assert!(fixture.supervisor.cancel("ses_parent", &job_id));
    fixture.supervisor.wait_all().await;

    let job = fixture.jobs.get(&job_id).expect("settled workflow job");
    assert_eq!(job.status, JobStatus::Cancelled);
    let items = fixture.work.items("ses_parent").expect("cancelled items");
    assert!(
        items
            .iter()
            .all(|item| item.status == WorkItemStatus::Cancelled),
        "cancellation must leave no pending or running WorkItem"
    );
}

#[tokio::test]
async fn restart_reconciliation_marks_running_workflows_uncertain_without_replay() {
    let fixture = Fixture::new();
    fixture
        .host
        .admit_work_items(
            &fixture.request(false, vec![node("orphan", &[], "wait")]),
            "run_orphan",
        )
        .expect("admit orphan workflow items");
    fixture
        .jobs
        .create(NewAgentJob::new(
            "job_orphan",
            "ses_parent",
            JobSubject::workflow("run_orphan", "release"),
            DbReportDelivery::Quiet,
            2,
        ))
        .expect("running workflow job");

    assert_eq!(
        fixture
            .host
            .recover_uncertain("ses_parent")
            .await
            .expect("reconcile workflow"),
        1
    );
    let job = fixture.jobs.get("job_orphan").expect("reconciled job");
    assert_eq!(job.status, JobStatus::Uncertain);
    let items = fixture.work.items("ses_parent").expect("reconciled items");
    assert!(
        items
            .iter()
            .all(|item| item.status == WorkItemStatus::Blocked),
        "lost schedulers leave unfinished WorkItems visibly blocked"
    );
    assert!(fixture.runner.events.lock().expect("events").is_empty());
}

#[tokio::test]
async fn council_seats_overlap_keep_stable_order_and_preserve_dissent() {
    let fixture = Fixture::new();
    let request = council_request(
        false,
        vec![
            council_seat("implementation", "explorer", "slow-agree"),
            council_seat("dissent", "oracle", "dissent"),
            council_seat("contract", "librarian", "agree"),
        ],
        2,
        3,
        0,
        Duration::from_secs(2),
    );
    let turn = CouncilHost::dispatch(&fixture.host, request, CancellationToken::new())
        .await
        .expect("Council reaches quorum");

    assert_eq!(fixture.runner.max_active.load(Ordering::SeqCst), 3);
    assert!(
        turn.output.find("### implementation").expect("first seat")
            < turn.output.find("### dissent").expect("second seat")
    );
    assert!(
        turn.output.find("### dissent").expect("second seat")
            < turn.output.find("### contract").expect("third seat")
    );
    assert!(turn.output.contains("Verdict: hold"));
    assert!(turn.output.contains("### Synthesis"));

    let payloads = fixture.synth.payloads.lock().expect("payloads");
    assert_eq!(payloads.len(), 1);
    assert!(payloads[0].len() <= 32_768);
    let payload: Value = serde_json::from_str(&payloads[0]).expect("structured synthesis input");
    let seats = payload["seats"].as_array().expect("seat array");
    assert_eq!(
        seats
            .iter()
            .map(|seat| seat["id"].as_str().expect("seat id"))
            .collect::<Vec<_>>(),
        vec!["implementation", "dissent", "contract"]
    );
    assert_eq!(seats[1]["verdict"], json!("hold"));

    let jobs = fixture
        .jobs
        .list_for_parent("ses_parent")
        .expect("Council job");
    let job = jobs
        .iter()
        .find(|job| matches!(&job.subject, JobSubject::Workflow { workflow, .. } if workflow == "council:balanced-review"))
        .expect("Council workflow job");
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(
        job.result
            .as_ref()
            .and_then(|result| result["status"].as_str()),
        Some("completed")
    );
}

#[tokio::test]
async fn council_retries_invalid_structured_output_once_then_synthesizes() {
    let fixture = Fixture::new();
    let request = council_request(
        false,
        vec![
            council_seat("recovered", "explorer", "invalid-once"),
            council_seat("steady", "oracle", "agree"),
        ],
        2,
        2,
        1,
        Duration::from_secs(2),
    );
    CouncilHost::dispatch(&fixture.host, request, CancellationToken::new())
        .await
        .expect("retry reaches quorum");

    let attempts = fixture.runner.attempts.lock().expect("attempts");
    assert_eq!(attempts.get("recovered"), Some(&2));
    assert_eq!(attempts.get("steady"), Some(&1));
    drop(attempts);
    let payloads = fixture.synth.payloads.lock().expect("payloads");
    let payload: Value = serde_json::from_str(&payloads[0]).expect("payload");
    assert_eq!(payload["seats"][0]["attempts"], json!(2));
    assert_eq!(payload["seats"][0]["status"], json!("completed"));
}

#[tokio::test]
async fn council_below_quorum_keeps_typed_partial_results_on_failed_job() {
    let fixture = Fixture::new();
    let request = council_request(
        false,
        vec![
            council_seat("invalid", "explorer", "invalid"),
            council_seat("failed", "oracle", "fail"),
            council_seat("valid", "librarian", "agree"),
        ],
        2,
        3,
        0,
        Duration::from_secs(2),
    );
    let error = CouncilHost::dispatch(&fixture.host, request, CancellationToken::new())
        .await
        .expect_err("one valid seat cannot reach quorum two");
    assert!(error.contains("quorum was not reached"));
    assert!(fixture.synth.payloads.lock().expect("payloads").is_empty());

    let jobs = fixture
        .jobs
        .list_for_parent("ses_parent")
        .expect("Council job");
    let job = jobs
        .iter()
        .find(|job| matches!(&job.subject, JobSubject::Workflow { workflow, .. } if workflow == "council:balanced-review"))
        .expect("failed Council job");
    assert_eq!(job.status, JobStatus::Failed);
    let result = job.result.as_ref().expect("partial Council result");
    assert_eq!(result["status"], json!("failed"));
    assert_eq!(result["seats"][0]["status"], json!("invalid"));
    assert_eq!(result["seats"][1]["status"], json!("failed"));
    assert_eq!(result["seats"][2]["status"], json!("completed"));
}

#[tokio::test]
async fn council_deadline_marks_the_seat_timed_out_without_synthesis() {
    let fixture = Fixture::new();
    let request = council_request(
        false,
        vec![council_seat("slow", "explorer", "wait")],
        1,
        1,
        0,
        Duration::from_millis(20),
    );
    let error = CouncilHost::dispatch(&fixture.host, request, CancellationToken::new())
        .await
        .expect_err("deadline prevents quorum");
    assert!(error.contains("quorum was not reached"));
    assert!(fixture.synth.payloads.lock().expect("payloads").is_empty());
    let jobs = fixture
        .jobs
        .list_for_parent("ses_parent")
        .expect("Council job");
    let job = jobs
        .iter()
        .find(|job| matches!(&job.subject, JobSubject::Workflow { workflow, .. } if workflow == "council:balanced-review"))
        .expect("timed-out Council job");
    assert_eq!(
        job.result.as_ref().expect("result")["seats"][0]["status"],
        json!("timed_out")
    );
}

#[tokio::test]
async fn background_council_cancellation_settles_job_and_work_items() {
    let fixture = Fixture::new();
    let request = council_request(
        true,
        vec![council_seat("wait", "explorer", "wait")],
        1,
        1,
        0,
        Duration::from_secs(30),
    );
    let turn = CouncilHost::dispatch(&fixture.host, request, CancellationToken::new())
        .await
        .expect("background Council admitted");
    let job_id = turn.job_id.expect("durable Council job");
    fixture.runner.entered.notified().await;
    assert!(fixture.supervisor.cancel("ses_parent", &job_id));
    fixture.supervisor.wait_all().await;

    let job = fixture.jobs.get(&job_id).expect("settled Council job");
    assert_eq!(job.status, JobStatus::Cancelled);
    assert_eq!(
        job.result.as_ref().expect("cancelled result")["seats"][0]["status"],
        json!("cancelled")
    );
    let items = fixture.work.items("ses_parent").expect("cancelled items");
    assert!(
        items
            .iter()
            .all(|item| item.status == WorkItemStatus::Cancelled)
    );
}
