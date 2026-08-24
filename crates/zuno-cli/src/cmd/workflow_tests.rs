use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use zuno_db::job::{JobStatus, ReportDelivery as DbReportDelivery};
use zuno_paths::DbLocation;
use zuno_tools::work_state::{WorkItemStatus, WorkStateStore};
use zuno_tools::workflow::WorkflowNodeRequest;

struct Fixture {
    _root: tempfile::TempDir,
    host: NativeWorkflowHost,
    supervisor: BackgroundJobSupervisor,
    jobs: AgentJobStore,
    work: WorkStateStore,
    runner: Arc<RecordingRunner>,
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
        };
        Self {
            _root: root,
            host,
            supervisor,
            jobs: AgentJobStore::new(pool),
            work,
            runner,
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
}

#[async_trait]
impl WorkflowNodeRunner for RecordingRunner {
    async fn run(
        &self,
        request: ChildTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<ChildTurn, String> {
        let label = request.description.unwrap_or_else(|| request.agent.clone());
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

        let delay = if request.prompt == "slow" { 60 } else { 10 };
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
        Ok(ChildTurn {
            session_id: format!("ses_{label}"),
            job_id: None,
            output: format!("result:{label}"),
        })
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
    let turn = fixture
        .host
        .dispatch(request, CancellationToken::new())
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
    let turn = fixture
        .host
        .dispatch(
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
