use super::*;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use zuno_config::schema::product_agent::ProductAgentKind;
use zuno_db::job::{JobStatus, ReportDelivery as DbReportDelivery};
use zuno_paths::DbLocation;
use zuno_product_agent::ProductAgentResult;

struct Fixture {
    _root: tempfile::TempDir,
    host: NativeProductAgentHost,
    pool: Arc<zuno_db::Pool>,
    wake: Arc<RecordingWake>,
    supervisor: BackgroundJobSupervisor,
}

impl Fixture {
    fn new(runner: Arc<dyn ProductAgent>) -> Self {
        Self::with_limit(runner, 8)
    }

    fn with_limit(runner: Arc<dyn ProductAgent>, limit: usize) -> Self {
        let root = tempfile::TempDir::new().expect("temporary product-agent root");
        let pool = Arc::new(zuno_db::Pool::open(&DbLocation::Memory).expect("open database"));
        {
            let mut connection = pool.get().expect("database connection");
            zuno_db::migration::apply(&mut connection).expect("migrate database");
            connection
                .execute(
                    "INSERT INTO project \
                     (id, worktree, time_created, time_updated, sandboxes) \
                     VALUES ('project', '/workspace', 1, 1, '[]')",
                    [],
                )
                .expect("create project");
        }
        pool.transaction(|transaction| {
            zuno_db::session::create(
                transaction,
                &zuno_db::session::SessionCreate::new(
                    "ses_parent",
                    "parent",
                    "project",
                    "/workspace",
                    "/workspace",
                    "parent",
                    crate::RUST_PACKAGE_VERSION,
                )
                .at(1),
            )
        })
        .expect("create parent session");
        let wake = Arc::new(RecordingWake::default());
        let supervisor = BackgroundJobSupervisor::default();
        let delegation_limiter = supervisor.delegation_limiter(
            NonZeroUsize::new(limit).expect("fixture delegation limit is non-zero"),
        );
        let agents = BTreeMap::from([(
            "reviewer".to_owned(),
            ConfiguredProduct {
                product: "codex".to_owned(),
                tool: "subagent_codex".to_owned(),
                runner,
            },
        )]);
        let host = NativeProductAgentHost {
            agents: Arc::new(agents),
            directory: root.path().to_owned(),
            jobs: AgentJobStore::new(Arc::clone(&pool)),
            wake: Arc::clone(&wake) as Arc<dyn ParentReportWake>,
            delegation_limiter,
            supervisor: supervisor.clone(),
        };
        Self {
            _root: root,
            host,
            pool,
            wake,
            supervisor,
        }
    }

    fn request(&self, background: bool, report_delivery: ReportDelivery) -> ProductAgentRequest {
        ProductAgentRequest {
            parent_session_id: "ses_parent".to_owned(),
            instance: "reviewer".to_owned(),
            product: "codex".to_owned(),
            tool: "subagent_codex".to_owned(),
            prompt: "review the change".to_owned(),
            description: Some("review".to_owned()),
            background,
            report_delivery,
        }
    }
}

struct ImmediateAgent {
    requests: Mutex<Vec<NativeRequest>>,
    outcome: Mutex<Option<Result<ProductAgentResult, ProductAgentError>>>,
}

impl ImmediateAgent {
    fn returning(outcome: Result<ProductAgentResult, ProductAgentError>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            outcome: Mutex::new(Some(outcome)),
        }
    }
}

#[async_trait]
impl ProductAgent for ImmediateAgent {
    fn kind(&self) -> ProductAgentKind {
        ProductAgentKind::Codex
    }

    async fn run(
        &self,
        request: NativeRequest,
        _cancellation: CancellationToken,
    ) -> Result<ProductAgentResult, ProductAgentError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        self.outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("one configured outcome")
    }
}

struct CancellingAgent;

#[async_trait]
impl ProductAgent for CancellingAgent {
    fn kind(&self) -> ProductAgentKind {
        ProductAgentKind::Codex
    }

    async fn run(
        &self,
        _request: NativeRequest,
        cancellation: CancellationToken,
    ) -> Result<ProductAgentResult, ProductAgentError> {
        cancellation.cancelled().await;
        Err(ProductAgentError::Cancelled { product: "Codex" })
    }
}

struct BlockingAgent {
    started: AtomicUsize,
    release: tokio::sync::Semaphore,
}

impl BlockingAgent {
    fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }

    fn release_one(&self) {
        self.release.add_permits(1);
    }

    async fn wait_for_starts(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while self.started.load(Ordering::Acquire) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("product agent reached the expected start count");
    }
}

#[async_trait]
impl ProductAgent for BlockingAgent {
    fn kind(&self) -> ProductAgentKind {
        ProductAgentKind::Codex
    }

    async fn run(
        &self,
        _request: NativeRequest,
        cancellation: CancellationToken,
    ) -> Result<ProductAgentResult, ProductAgentError> {
        self.started.fetch_add(1, Ordering::AcqRel);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                Err(ProductAgentError::Cancelled { product: "Codex" })
            }
            permit = self.release.acquire() => {
                permit.expect("test release semaphore remains open").forget();
                success("released")
            }
        }
    }
}

#[derive(Default)]
struct RecordingWake(Mutex<Vec<zuno_db::inbox::SessionInput>>);

#[async_trait]
impl ParentReportWake for RecordingWake {
    async fn wake(&self, report: zuno_db::inbox::SessionInput) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(report);
        Ok(())
    }
}

fn success(text: &str) -> Result<ProductAgentResult, ProductAgentError> {
    Ok(ProductAgentResult {
        text: text.to_owned(),
    })
}

#[tokio::test]
async fn foreground_runs_in_the_session_directory_without_creating_a_job() {
    let agent = Arc::new(ImmediateAgent::returning(success("review complete")));
    let fixture = Fixture::new(Arc::clone(&agent) as Arc<dyn ProductAgent>);

    let turn = fixture
        .host
        .dispatch(
            fixture.request(false, ReportDelivery::NextStep),
            CancellationToken::new(),
        )
        .await
        .expect("foreground dispatch");

    assert_eq!(turn.job_id, None);
    assert_eq!(turn.output, "review complete");
    assert!(turn.run_id.starts_with("run_"));
    let requests = agent
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(requests[0].directory, fixture._root.path());
    assert_eq!(requests[0].prompt, "review the change");
    assert!(
        AgentJobStore::new(Arc::clone(&fixture.pool))
            .list_for_parent("ses_parent")
            .expect("list jobs")
            .is_empty()
    );
}

#[tokio::test]
async fn background_next_step_settles_and_wakes_with_product_identity() {
    let fixture = Fixture::new(Arc::new(ImmediateAgent::returning(success(
        "background answer",
    ))));

    let turn = fixture
        .host
        .dispatch(
            fixture.request(true, ReportDelivery::NextStep),
            CancellationToken::new(),
        )
        .await
        .expect("background dispatch");
    let job_id = turn.job_id.expect("job id");
    fixture.supervisor.wait_all().await;

    let job = AgentJobStore::new(Arc::clone(&fixture.pool))
        .get(&job_id)
        .expect("settled job");
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(
        job.result
            .as_ref()
            .and_then(|result| result["text"].as_str()),
        Some("background answer")
    );
    let reports = fixture
        .wake
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].prompt["kind"], "productAgentReport");
    assert_eq!(reports[0].prompt["jobID"], job_id);
    assert_eq!(reports[0].prompt["product"], "codex");
    assert_eq!(reports[0].prompt["instance"], "reviewer");
    assert_eq!(reports[0].prompt["status"], "completed");
}

#[tokio::test]
async fn background_product_agents_share_the_workspace_delegation_bound() {
    let agent = Arc::new(BlockingAgent::new());
    let fixture = Fixture::with_limit(Arc::clone(&agent) as Arc<dyn ProductAgent>, 1);

    let first = fixture
        .host
        .dispatch(
            fixture.request(true, ReportDelivery::Quiet),
            CancellationToken::new(),
        )
        .await
        .expect("first background dispatch");
    let second = fixture
        .host
        .dispatch(
            fixture.request(true, ReportDelivery::Quiet),
            CancellationToken::new(),
        )
        .await
        .expect("second background dispatch");
    agent.wait_for_starts(1).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), agent.wait_for_starts(2))
            .await
            .is_err(),
        "a second product agent started while the only delegation slot was occupied"
    );

    agent.release_one();
    agent.wait_for_starts(2).await;
    agent.release_one();
    fixture.supervisor.wait_all().await;

    let store = AgentJobStore::new(Arc::clone(&fixture.pool));
    for turn in [first, second] {
        assert_eq!(
            store
                .get(turn.job_id.as_deref().expect("job id"))
                .expect("settled product job")
                .status,
            JobStatus::Completed
        );
    }
}

#[tokio::test]
async fn quiet_background_result_is_durable_without_parent_input() {
    let fixture = Fixture::new(Arc::new(ImmediateAgent::returning(success("quiet answer"))));

    let turn = fixture
        .host
        .dispatch(
            fixture.request(true, ReportDelivery::Quiet),
            CancellationToken::new(),
        )
        .await
        .expect("background dispatch");
    fixture.supervisor.wait_all().await;

    let job = AgentJobStore::new(Arc::clone(&fixture.pool))
        .get(turn.job_id.as_deref().expect("job id"))
        .expect("settled job");
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.report_delivery, DbReportDelivery::Quiet);
    assert_eq!(job.report_input_id, None);
    assert!(
        fixture
            .wake
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[tokio::test]
async fn protocol_loss_settles_uncertain_and_is_never_replayed() {
    let fixture = Fixture::new(Arc::new(ImmediateAgent::returning(Err(
        ProductAgentError::Uncertain {
            product: "Codex",
            message: "stdio disappeared".to_owned(),
        },
    ))));

    let turn = fixture
        .host
        .dispatch(
            fixture.request(true, ReportDelivery::NextStep),
            CancellationToken::new(),
        )
        .await
        .expect("background dispatch");
    fixture.supervisor.wait_all().await;

    let job = AgentJobStore::new(Arc::clone(&fixture.pool))
        .get(turn.job_id.as_deref().expect("job id"))
        .expect("settled job");
    assert_eq!(job.status, JobStatus::Uncertain);
    assert!(
        job.error
            .as_deref()
            .is_some_and(|error| error.contains("stdio disappeared"))
    );
}

#[tokio::test]
async fn live_cancellation_settles_only_after_the_product_observes_it() {
    let fixture = Fixture::new(Arc::new(CancellingAgent));

    let turn = fixture
        .host
        .dispatch(
            fixture.request(true, ReportDelivery::NextStep),
            CancellationToken::new(),
        )
        .await
        .expect("background dispatch");
    let job_id = turn.job_id.expect("job id");
    assert!(fixture.supervisor.cancel("ses_parent", &job_id));
    fixture.supervisor.wait_all().await;

    let job = AgentJobStore::new(Arc::clone(&fixture.pool))
        .get(&job_id)
        .expect("cancelled job");
    assert_eq!(job.status, JobStatus::Cancelled);
    assert_eq!(
        fixture
            .wake
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[0]
            .prompt["status"],
        "cancelled"
    );
}

#[tokio::test]
async fn restart_reconciliation_marks_only_product_jobs_uncertain() {
    let fixture = Fixture::new(Arc::new(ImmediateAgent::returning(success("unused"))));
    let store = AgentJobStore::new(Arc::clone(&fixture.pool));
    store
        .create(NewAgentJob::new(
            "job_orphaned_product",
            "ses_parent",
            JobSubject::product_agent("run_orphaned", "codex", "reviewer", "subagent_codex"),
            DbReportDelivery::NextStep,
            10,
        ))
        .expect("create orphaned product job");

    let recovered = fixture
        .host
        .recover_uncertain("ses_parent")
        .await
        .expect("recover product jobs");

    assert_eq!(recovered, 1);
    let job = store.get("job_orphaned_product").expect("reconciled job");
    assert_eq!(job.status, JobStatus::Uncertain);
    assert!(
        job.error
            .as_deref()
            .is_some_and(|error| error.contains("will not be replayed"))
    );
    let reports = fixture
        .wake
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(reports.len(), 1);
    assert!(
        reports[0].prompt["text"]
            .as_str()
            .is_some_and(|text| text.contains("job `job_orphaned_product`"))
    );
}
