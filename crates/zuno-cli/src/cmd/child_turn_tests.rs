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
use zuno_paths::{DbLocation, Env};
use zuno_tools::task::ReportDelivery;

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
            resume_session_id: None,
            agent: "worker".to_owned(),
            description: Some("a delegated unit".to_owned()),
            prompt: "do the thing".to_owned(),
            model: None,
            effort: None,
            provider_options: serde_json::Map::new(),
            background: false,
            report_delivery: ReportDelivery::NextStep,
        }
    }
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
struct RecordingWake {
    reports: Mutex<Vec<zuno_db::inbox::SessionInput>>,
    failure: Mutex<Option<String>>,
}

impl RecordingWake {
    fn fail_with(&self, error: &str) {
        *self.failure.lock().expect("wake failure lock") = Some(error.to_owned());
    }
}

#[async_trait]
impl ParentReportWake for RecordingWake {
    async fn wake(&self, report: zuno_db::inbox::SessionInput) -> Result<(), String> {
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

    let child = fixture
        .host
        .session_for(&fixture.request("ses_owner"))
        .expect("a fresh delegation creates a child");

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
        .session_for(&request)
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
        .session_for(&request)
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
            .session_for(&request)
            .expect("this parent's own child resumes"),
        "ses_my_child"
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
async fn background_dispatch_returns_a_durable_job_before_the_child_finishes() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;

    let turn = fixture
        .host
        .dispatch(request)
        .await
        .expect("background delegation is admitted");
    let job_id = turn.job_id.expect("background job id");
    assert_ne!(job_id, turn.session_id);
    assert_eq!(
        fixture
            .host
            .job_store
            .get(&job_id)
            .expect("running job")
            .status,
        zuno_db::job::JobStatus::Running
    );
    assert_eq!(
        zuno_db::session::children(&fixture.connection(), "ses_owner")
            .expect("read children")
            .len(),
        1
    );

    fixture.runner.complete_with(Ok("child answer"));
    fixture.jobs.wait_all().await;
    let settled = fixture.host.job_store.get(&job_id).expect("settled job");
    assert_eq!(settled.status, zuno_db::job::JobStatus::Completed);
    assert_eq!(
        settled
            .result
            .as_ref()
            .and_then(|value| value["text"].as_str()),
        Some("child answer")
    );
    assert_eq!(fixture.wake.reports.lock().expect("wake reports").len(), 1);
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
async fn background_children_share_the_workspace_delegation_bound() {
    let fixture = Fixture::with_limit(1);
    fixture.session("ses_owner", None);
    let mut first = fixture.request("ses_owner");
    first.background = true;
    first.report_delivery = ReportDelivery::Quiet;
    let mut second = fixture.request("ses_owner");
    second.background = true;
    second.report_delivery = ReportDelivery::Quiet;

    let first = fixture.host.dispatch(first).await.expect("first dispatch");
    let second = fixture
        .host
        .dispatch(second)
        .await
        .expect("second dispatch");
    fixture.runner.wait_for_starts(1).await;
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
async fn quiet_background_dispatch_persists_the_result_without_waking_the_parent() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    let mut request = fixture.request("ses_owner");
    request.background = true;
    request.report_delivery = ReportDelivery::Quiet;

    let turn = fixture.host.dispatch(request).await.expect("dispatch");
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

    let turn = fixture.host.dispatch(request).await.expect("dispatch");
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

    let turn = fixture.host.dispatch(request).await.expect("dispatch");
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
async fn a_report_left_pending_by_process_loss_is_recovered_when_the_parent_reopens() {
    let fixture = Fixture::new();
    fixture.session("ses_owner", None);
    fixture.wake.fail_with("process is stopping");
    let mut request = fixture.request("ses_owner");
    request.background = true;

    let turn = fixture.host.dispatch(request).await.expect("dispatch");
    fixture.runner.complete_with(Ok("survives restart"));
    fixture.jobs.wait_all().await;
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
        recovered_jobs,
    )
    .expect("reopen child host")
    .recover_pending_reports("ses_owner")
    .await
    .expect("recover reports");

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
