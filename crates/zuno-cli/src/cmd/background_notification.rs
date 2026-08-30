//! Durable completion notifications for process-owned background commands.
//!
//! A background process event is only a wakeup hint. The model-visible fact is a
//! deterministic [`zuno_db::inbox::SessionInput`] admitted before a parent turn is
//! requested. Reopening the workspace scans terminal process records and pending
//! asynchronous inputs, so a crash between settlement, admission, and turn claim
//! cannot lose the continuation or replay the command.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SessionInput, SubmissionState};
use zuno_db::job::AgentJobStore;
use zuno_engine::status::SessionRunRegistry;
use zuno_pty::{BackgroundExecutionEvent, BackgroundExecutionInfo, BackgroundExecutionService};

use super::child_turn::{ParentReportWake, wake_parent_report};

const RECOVERY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct NotificationTarget {
    inbox: SessionInbox,
    jobs: AgentJobStore,
    runs: SessionRunRegistry,
    wake: Arc<dyn ParentReportWake>,
}

pub(super) struct BackgroundNotificationRegistration {
    pub(super) service: Arc<BackgroundExecutionService>,
    pub(super) session_id: String,
    pub(super) inbox: SessionInbox,
    pub(super) jobs: AgentJobStore,
    pub(super) runs: SessionRunRegistry,
    pub(super) wake: Arc<dyn ParentReportWake>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WatcherKey {
    directory: PathBuf,
    session_id: String,
}

struct Watcher {
    target: Option<watch::Sender<NotificationTarget>>,
    task: Option<JoinHandle<()>>,
}

impl Watcher {
    fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn replace_target(&self, target: NotificationTarget) {
        self.target
            .as_ref()
            .expect("live watcher retains its target sender")
            .send_replace(target);
    }

    /// Close the target channel without aborting a detached continuation already
    /// in flight. The caller must interrupt the session and await this task.
    fn stop(mut self) -> JoinHandle<()> {
        self.target.take();
        self.task
            .take()
            .expect("registered watcher retains its task")
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Process-lifetime registry for one watcher per workspace session.
#[derive(Clone, Default)]
pub(crate) struct BackgroundNotificationRegistry {
    watchers: Arc<Mutex<HashMap<WatcherKey, Watcher>>>,
}

impl std::fmt::Debug for BackgroundNotificationRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackgroundNotificationRegistry")
            .field("watchers", &self.lock().len())
            .finish()
    }
}

impl BackgroundNotificationRegistry {
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.watchers, &other.watchers)
    }

    /// Attach the latest session driver and begin restart reconciliation.
    ///
    /// Re-registering the same session replaces only the live target. The watcher
    /// and process service remain stable across host replacement.
    pub(super) fn register(
        &self,
        runtime: &tokio::runtime::Handle,
        directory: &Path,
        registration: BackgroundNotificationRegistration,
    ) {
        let BackgroundNotificationRegistration {
            service,
            session_id,
            inbox,
            jobs,
            runs,
            wake,
        } = registration;
        let key = WatcherKey {
            directory: directory.to_path_buf(),
            session_id: session_id.clone(),
        };
        let target = NotificationTarget {
            inbox,
            jobs,
            runs,
            wake,
        };
        let mut watchers = self.lock();
        if let Some(watcher) = watchers.get_mut(&key)
            && !watcher.is_finished()
        {
            watcher.replace_target(target);
            return;
        }
        watchers.remove(&key);

        let (target_sender, target_receiver) = watch::channel(target);
        let task = runtime.spawn(run_watcher(service, session_id, target_receiver));
        watchers.insert(
            key,
            Watcher {
                target: Some(target_sender),
                task: Some(task),
            },
        );
    }

    /// Stop accepting notifications for an explicitly closed session.
    ///
    /// Dropping the target sender asks an idle watcher to exit. If it is currently
    /// driving a detached turn, the caller must interrupt that session and await
    /// the returned task so host shutdown can finish normally instead of being
    /// aborted mid-cleanup.
    pub(crate) fn unregister(&self, directory: &Path, session_id: &str) -> Option<JoinHandle<()>> {
        self.lock()
            .remove(&WatcherKey {
                directory: directory.to_path_buf(),
                session_id: session_id.to_owned(),
            })
            .map(Watcher::stop)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<WatcherKey, Watcher>> {
        self.watchers.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

async fn run_watcher(
    service: Arc<BackgroundExecutionService>,
    session_id: String,
    mut target: watch::Receiver<NotificationTarget>,
) {
    if target.has_changed().is_err() {
        return;
    }
    let mut events = service.subscribe();
    let mut recovery = tokio::time::interval(RECOVERY_INTERVAL);
    recovery.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    recovery.tick().await;
    let current = target.borrow().clone();
    reconcile(&service, &session_id, &current).await;

    loop {
        tokio::select! {
            changed = target.changed() => {
                if changed.is_err() {
                    return;
                }
                let current = target.borrow().clone();
                reconcile(&service, &session_id, &current).await;
            }
            event = events.recv() => match event {
                Ok(BackgroundExecutionEvent::Settled(info)) if info.session_id == session_id => {
                    let current = target.borrow().clone();
                    deliver_execution(&current, &info).await;
                }
                Ok(BackgroundExecutionEvent::Created(_)
                    | BackgroundExecutionEvent::Settled(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let current = target.borrow().clone();
                    reconcile(&service, &session_id, &current).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = recovery.tick() => {
                let current = target.borrow().clone();
                reconcile(&service, &session_id, &current).await;
            }
        }
    }
}

async fn reconcile(
    service: &BackgroundExecutionService,
    session_id: &str,
    target: &NotificationTarget,
) {
    let has_promoted_reports = match target.jobs.has_promoted_reports_for(session_id) {
        Ok(present) => present,
        Err(error) => {
            tracing::error!(
                session_id,
                %error,
                "could not inspect promoted asynchronous reports"
            );
            return;
        }
    };
    if has_promoted_reports
        && let Ok(_recovery) = target.runs.begin_recovery(session_id)
        && let Err(error) = target.jobs.pending_reports_for(session_id)
    {
        tracing::error!(
            session_id,
            %error,
            "could not recover promoted asynchronous reports"
        );
        return;
    }
    for info in service
        .list_for_session(session_id)
        .into_iter()
        .filter(|info| info.status.is_terminal())
    {
        deliver_execution(target, &info).await;
    }
    deliver_pending_inputs(session_id, target).await;
}

async fn deliver_execution(target: &NotificationTarget, info: &BackgroundExecutionInfo) {
    let input_id = format!("msg_{}", info.id.as_str());
    let input = match target.inbox.get(&info.session_id, &input_id) {
        Ok(Some(input)) => match validate_execution_input(&input, info.id.as_str()) {
            Ok(()) => input,
            Err(error) => {
                tracing::error!(
                    execution_id = %info.id,
                    input_id,
                    %error,
                    "background completion input identity conflicts with durable state"
                );
                return;
            }
        },
        Ok(None) => {
            let candidate = execution_input(info);
            match target.inbox.admit(candidate) {
                Ok(input) => input,
                Err(error) => match target.inbox.get(&info.session_id, &input_id) {
                    Ok(Some(input))
                        if validate_execution_input(&input, info.id.as_str()).is_ok() =>
                    {
                        input
                    }
                    _ => {
                        tracing::error!(
                            execution_id = %info.id,
                            input_id,
                            %error,
                            "could not admit durable background completion input"
                        );
                        return;
                    }
                },
            }
        }
        Err(error) => {
            tracing::error!(
                execution_id = %info.id,
                input_id,
                %error,
                "could not inspect durable background completion input"
            );
            return;
        }
    };

    let input = if input.state == SubmissionState::Promoted {
        let _recovery = match target.runs.begin_recovery(&input.session_id) {
            Ok(recovery) => recovery,
            Err(_) => return,
        };
        match target.inbox.recover_promoted(&input.session_id, &input.id) {
            Ok(Some(input)) => input,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(
                    execution_id = %info.id,
                    input_id = %input.id,
                    %error,
                    "could not recover promoted background completion input"
                );
                return;
            }
        }
    } else {
        input
    };
    if input.state.is_pending() {
        wake_with_retry(target.wake.as_ref(), input).await;
    }
}

async fn deliver_pending_inputs(session_id: &str, target: &NotificationTarget) {
    let pending = match target.inbox.pending(session_id) {
        Ok(pending) => pending,
        Err(error) => {
            tracing::error!(
                session_id,
                %error,
                "could not inspect pending asynchronous session inputs"
            );
            return;
        }
    };
    for input in pending
        .into_iter()
        .filter(|input| is_async_notification(&input.prompt))
    {
        wake_with_retry(target.wake.as_ref(), input).await;
    }
}

pub(super) fn is_async_notification(prompt: &Value) -> bool {
    matches!(
        prompt.get("kind").and_then(Value::as_str),
        Some(
            "subagentReport"
                | "productAgentReport"
                | "workflowReport"
                | "councilReport"
                | "backgroundExecutionReport"
        )
    )
}

async fn wake_with_retry(wake: &dyn ParentReportWake, input: SessionInput) {
    if let Err(error) = wake_parent_report(
        wake,
        input.clone(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    {
        tracing::error!(
            input_id = %input.id,
            %error,
            "durable asynchronous input remains pending for periodic or restart recovery"
        );
    }
}

fn execution_input(info: &BackgroundExecutionInfo) -> NewSessionInput {
    let text = execution_report_text(info);
    NewSessionInput::new(
        format!("msg_{}", info.id.as_str()),
        info.session_id.clone(),
        json!({
            "kind": "backgroundExecutionReport",
            "executionID": info.id.as_str(),
            "status": info.status.as_str(),
            "title": info.title,
            "command": info.command,
            "exitCode": info.exit_code,
            "timedOut": info.timed_out,
            "error": info.error,
            "text": text,
        }),
        InputDelivery::Steer,
        info.time_completed
            .unwrap_or(info.time_updated)
            .max(info.time_created),
    )
}

fn execution_report_text(info: &BackgroundExecutionInfo) -> String {
    let exit = info
        .exit_code
        .map(|code| format!(", exit code {code}"))
        .unwrap_or_default();
    let error = info
        .error
        .as_deref()
        .map(|error| format!("\nRecorded error: {error}"))
        .unwrap_or_default();
    format!(
        "Background command `{}` ({}) reached terminal status `{}`{exit}.{error}\n\
         The earlier assistant turn may have ended while it was running. Inspect the durable \
         output with the `bg` tool when needed, then continue the parent task. Do not rerun a \
         command with possible side effects unless authoritative state proves that replay is safe.",
        info.id,
        info.title,
        info.status.as_str()
    )
}

fn validate_execution_input(input: &SessionInput, execution_id: &str) -> Result<(), &'static str> {
    if input.prompt.get("kind").and_then(Value::as_str) != Some("backgroundExecutionReport") {
        return Err("the deterministic input id belongs to another input kind");
    }
    if input.prompt.get("executionID").and_then(Value::as_str) != Some(execution_id) {
        return Err("the deterministic input id belongs to another background execution");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use zuno_db::job::{JobSettlement, JobSubject, NewAgentJob, ReportDelivery};
    use zuno_pty::{
        BackgroundExecutionId, BackgroundExecutionInput, BackgroundExecutionRetention,
        BackgroundExecutionStatus,
    };
    use zuno_sandbox::{
        ExecutionAuthority, NetworkAccess, PrepareRequest, PreparedCommand, SandboxCapabilities,
        SandboxMode, SandboxPolicy, SandboxResolutionKind,
    };

    use super::*;

    struct ClaimingWake {
        inbox: SessionInbox,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ParentReportWake for ClaimingWake {
        async fn wake(&self, input: SessionInput) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self
                .inbox
                .promote_id(&input.session_id, &input.id)
                .map_err(|error| error.to_string())?
                .is_some()
            {
                self.inbox
                    .mark_consumed(&input.session_id, &input.id)
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        }
    }

    fn fixture() -> (
        SessionInbox,
        AgentJobStore,
        SessionRunRegistry,
        Arc<ClaimingWake>,
    ) {
        let pool = Arc::new(
            zuno_db::pool::Pool::open(&zuno_paths::DbLocation::Memory)
                .expect("open notification database"),
        );
        let mut connection = pool.get().expect("open notification connection");
        zuno_db::migration::apply(&mut connection).expect("initialize notification schema");
        connection
            .execute(
                "INSERT INTO project \
                 (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES (?1, '/workspace', 1, 1, '[]')",
                [zuno_paths::GLOBAL_PROJECT_ID],
            )
            .expect("insert project");
        connection
            .execute(
                "INSERT INTO session \
                 (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('ses_parent', ?1, 'parent', '/workspace', 'parent', 'test', 1, 1)",
                [zuno_paths::GLOBAL_PROJECT_ID],
            )
            .expect("insert session");
        drop(connection);
        let inbox = SessionInbox::new(Arc::clone(&pool));
        let jobs = AgentJobStore::new(pool);
        let runs = SessionRunRegistry::new();
        let wake = Arc::new(ClaimingWake {
            inbox: inbox.clone(),
            calls: AtomicUsize::new(0),
        });
        (inbox, jobs, runs, wake)
    }

    fn info() -> BackgroundExecutionInfo {
        let directory = PathBuf::from("/workspace");
        BackgroundExecutionInfo {
            id: BackgroundExecutionId::parse("bg_0123456789abcdef0123456789abcdef")
                .expect("valid execution id"),
            session_id: "ses_parent".to_owned(),
            title: "tests".to_owned(),
            command: "cargo test".to_owned(),
            cwd: directory.clone(),
            status: BackgroundExecutionStatus::Completed,
            pid: None,
            exit_code: Some(0),
            timed_out: false,
            time_created: 10,
            time_updated: 20,
            time_completed: Some(20),
            error: None,
            output_file: directory.join("output"),
            status_file: directory.join("status"),
            authority: ExecutionAuthority {
                schema_version: 3,
                backend: "test".to_owned(),
                backend_executable: None,
                workspace: directory.clone(),
                mode: SandboxMode::WorkspaceWrite,
                network: NetworkAccess::Denied,
                requested_mode: Some(SandboxMode::WorkspaceWrite),
                requested_network: Some(NetworkAccess::Denied),
                resolution_kind: SandboxResolutionKind::Confined,
                fallback_reason: None,
                writable_roots: vec![directory.clone()],
                protected_paths: Vec::new(),
                cwd: directory,
                command_sha256: "digest".to_owned(),
                environment_keys: Vec::new(),
                approval_mode: "test".to_owned(),
                reviewer_policy_sha256: "policy".to_owned(),
            },
        }
    }

    #[test]
    fn asynchronous_report_kinds_share_one_delivery_classifier() {
        for kind in [
            "subagentReport",
            "productAgentReport",
            "workflowReport",
            "councilReport",
            "backgroundExecutionReport",
        ] {
            assert!(
                is_async_notification(&json!({"kind":kind,"text":"done"})),
                "{kind} was omitted from durable background delivery"
            );
        }
        assert!(!is_async_notification(
            &json!({"kind":"tuiPrompt","text":"user input"})
        ));
        assert!(!is_async_notification(
            &json!({"kind":"humanRequestAnswer","text":"answer"})
        ));
    }

    #[tokio::test]
    async fn terminal_execution_admits_one_input_and_does_not_redrive_after_consumption() {
        let (inbox, jobs, runs, wake) = fixture();
        let target = NotificationTarget {
            inbox: inbox.clone(),
            jobs,
            runs,
            wake: Arc::clone(&wake) as Arc<dyn ParentReportWake>,
        };
        let info = info();

        deliver_execution(&target, &info).await;
        deliver_execution(&target, &info).await;

        let input = inbox
            .get("ses_parent", "msg_bg_0123456789abcdef0123456789abcdef")
            .expect("read completion input")
            .expect("completion input");
        assert_eq!(input.state, SubmissionState::Consumed);
        assert_eq!(input.prompt["kind"], "backgroundExecutionReport");
        assert_eq!(wake.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn orphaned_promoted_completion_returns_to_its_lane_before_wake() {
        let (inbox, jobs, runs, wake) = fixture();
        let info = info();
        let admitted = inbox
            .admit(execution_input(&info))
            .expect("admit completion");
        inbox
            .promote_id(&admitted.session_id, &admitted.id)
            .expect("promote completion")
            .expect("completion was pending");
        let target = NotificationTarget {
            inbox: inbox.clone(),
            jobs,
            runs,
            wake: Arc::clone(&wake) as Arc<dyn ParentReportWake>,
        };

        deliver_execution(&target, &info).await;

        assert_eq!(
            inbox
                .get(&admitted.session_id, &admitted.id)
                .expect("read recovered completion")
                .expect("recovered completion")
                .state,
            SubmissionState::Consumed
        );
        assert_eq!(wake.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn live_turn_ownership_prevents_promoted_completion_recovery() {
        let (inbox, jobs, runs, wake) = fixture();
        let info = info();
        let admitted = inbox
            .admit(execution_input(&info))
            .expect("admit completion");
        inbox
            .promote_id(&admitted.session_id, &admitted.id)
            .expect("promote completion")
            .expect("completion was pending");
        let guard = runs
            .begin_turn(&admitted.session_id)
            .expect("live turn owns the promoted completion");
        let target = NotificationTarget {
            inbox: inbox.clone(),
            jobs,
            runs: runs.clone(),
            wake: Arc::clone(&wake) as Arc<dyn ParentReportWake>,
        };

        deliver_execution(&target, &info).await;

        assert_eq!(wake.calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            inbox
                .get(&admitted.session_id, &admitted.id)
                .expect("read live completion")
                .expect("live completion")
                .state,
            SubmissionState::Promoted
        );
        drop(guard);

        deliver_execution(&target, &info).await;

        assert_eq!(wake.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            inbox
                .get(&admitted.session_id, &admitted.id)
                .expect("read recovered completion")
                .expect("recovered completion")
                .state,
            SubmissionState::Consumed
        );
    }

    #[tokio::test]
    async fn pending_async_reports_redrive_but_ordinary_user_queue_does_not() {
        let (inbox, jobs, runs, wake) = fixture();
        inbox
            .admit(NewSessionInput::new(
                "msg_report",
                "ses_parent",
                json!({"kind":"workflowReport","text":"workflow finished"}),
                InputDelivery::Steer,
                2,
            ))
            .expect("admit report");
        inbox
            .admit(NewSessionInput::new(
                "msg_council",
                "ses_parent",
                json!({"kind":"councilReport","text":"council finished"}),
                InputDelivery::Queue,
                3,
            ))
            .expect("admit council report");
        inbox
            .admit(NewSessionInput::new(
                "msg_user",
                "ses_parent",
                json!({"kind":"tuiPrompt","text":"user waits"}),
                InputDelivery::Queue,
                4,
            ))
            .expect("admit user input");
        let target = NotificationTarget {
            inbox: inbox.clone(),
            jobs,
            runs,
            wake: Arc::clone(&wake) as Arc<dyn ParentReportWake>,
        };

        deliver_pending_inputs("ses_parent", &target).await;

        assert_eq!(wake.calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            inbox
                .get("ses_parent", "msg_report")
                .expect("read report")
                .expect("report")
                .state,
            SubmissionState::Consumed
        );
        assert_eq!(
            inbox
                .get("ses_parent", "msg_council")
                .expect("read council report")
                .expect("council report")
                .state,
            SubmissionState::Consumed
        );
        assert_eq!(
            inbox
                .get("ses_parent", "msg_user")
                .expect("read user")
                .expect("user")
                .state,
            SubmissionState::Queued
        );
    }

    #[tokio::test]
    async fn restart_scan_recovers_a_promoted_child_report_and_redrives_it() {
        let directory = tempfile::tempdir().expect("notification workspace");
        let (inbox, jobs, runs, wake) = fixture();
        jobs.create(NewAgentJob::new(
            "job_child",
            "ses_parent",
            JobSubject::child_session("ses_child"),
            ReportDelivery::NextStep,
            2,
        ))
        .expect("create child job");
        let settled = jobs
            .settle(
                "job_child",
                JobSettlement::completed(
                    json!({"text":"child finished"}),
                    3,
                    Some(NewSessionInput::new(
                        "msg_child",
                        "ses_parent",
                        json!({"kind":"subagentReport","text":"child finished"}),
                        InputDelivery::Queue,
                        3,
                    )),
                ),
            )
            .expect("settle child job");
        let report = settled.report.expect("next-step report");
        inbox
            .promote_id(&report.session_id, &report.id)
            .expect("promote child report")
            .expect("report was pending");
        let service = Arc::new(
            BackgroundExecutionService::open(directory.path().join("background"))
                .expect("background service"),
        );
        let registry = BackgroundNotificationRegistry::default();

        registry.register(
            &tokio::runtime::Handle::current(),
            directory.path(),
            BackgroundNotificationRegistration {
                service,
                session_id: "ses_parent".to_owned(),
                inbox: inbox.clone(),
                jobs,
                runs,
                wake: Arc::clone(&wake) as Arc<dyn ParentReportWake>,
            },
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while wake.calls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("restart scan redrives the child report");

        let task = registry
            .unregister(directory.path(), "ses_parent")
            .expect("registered watcher");
        task.await.expect("watcher stops cleanly");
        assert_eq!(
            inbox
                .get("ses_parent", "msg_child")
                .expect("read child report")
                .expect("child report")
                .state,
            SubmissionState::Consumed
        );
    }

    #[test]
    fn explicit_runtime_handle_registers_from_a_synchronous_surface() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("notification runtime");
        let directory = tempfile::tempdir().expect("notification workspace");
        let (inbox, jobs, runs, wake) = fixture();
        inbox
            .admit(NewSessionInput::new(
                "msg_sync_surface",
                "ses_parent",
                json!({"kind":"workflowReport","text":"workflow finished"}),
                InputDelivery::Queue,
                2,
            ))
            .expect("admit report");
        let service = Arc::new(
            BackgroundExecutionService::open(directory.path().join("background"))
                .expect("background service"),
        );
        let registry = BackgroundNotificationRegistry::default();

        registry.register(
            runtime.handle(),
            directory.path(),
            BackgroundNotificationRegistration {
                service,
                session_id: "ses_parent".to_owned(),
                inbox,
                jobs,
                runs,
                wake: Arc::clone(&wake) as Arc<dyn ParentReportWake>,
            },
        );
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), async {
                while wake.calls.load(Ordering::Relaxed) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("synchronous surface watcher redrives its report");
            let task = registry
                .unregister(directory.path(), "ses_parent")
                .expect("registered watcher");
            task.await.expect("watcher stops cleanly");
        });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_settlement_event_resumes_an_idle_session() {
        let directory = tempfile::tempdir().expect("notification workspace");
        let (inbox, jobs, runs, wake) = fixture();
        let service = Arc::new(
            BackgroundExecutionService::open(directory.path().join("background"))
                .expect("background service"),
        );
        let registry = BackgroundNotificationRegistry::default();
        registry.register(
            &tokio::runtime::Handle::current(),
            directory.path(),
            BackgroundNotificationRegistration {
                service: Arc::clone(&service),
                session_id: "ses_parent".to_owned(),
                inbox: inbox.clone(),
                jobs,
                runs,
                wake: Arc::clone(&wake) as Arc<dyn ParentReportWake>,
            },
        );
        tokio::task::yield_now().await;

        let info = service
            .start(background_input(
                directory.path(),
                "sleep 0.05; printf complete",
            ))
            .expect("start background command");
        service
            .wait(&info.id, None)
            .await
            .expect("background command settles");
        tokio::time::timeout(Duration::from_secs(1), async {
            while wake.calls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("settlement event resumes the session");

        let task = registry
            .unregister(directory.path(), "ses_parent")
            .expect("registered watcher");
        task.await.expect("watcher stops cleanly");
        let input_id = format!("msg_{}", info.id);
        assert_eq!(
            inbox
                .get("ses_parent", &input_id)
                .expect("read command report")
                .expect("command report")
                .state,
            SubmissionState::Consumed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unregister_prevents_explicitly_closed_session_from_reopening() {
        let directory = tempfile::tempdir().expect("notification workspace");
        let (inbox, jobs, runs, wake) = fixture();
        let service = Arc::new(
            BackgroundExecutionService::open(directory.path().join("background"))
                .expect("background service"),
        );
        let registry = BackgroundNotificationRegistry::default();
        registry.register(
            &tokio::runtime::Handle::current(),
            directory.path(),
            BackgroundNotificationRegistration {
                service: Arc::clone(&service),
                session_id: "ses_parent".to_owned(),
                inbox: inbox.clone(),
                jobs,
                runs,
                wake: Arc::clone(&wake) as Arc<dyn ParentReportWake>,
            },
        );
        tokio::task::yield_now().await;
        let info = service
            .start(background_input(
                directory.path(),
                "sleep 0.1; printf complete",
            ))
            .expect("start background command");

        let task = registry
            .unregister(directory.path(), "ses_parent")
            .expect("registered watcher");
        task.await.expect("watcher stops cleanly");
        service
            .wait(&info.id, None)
            .await
            .expect("background command settles");
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(wake.calls.load(Ordering::Relaxed), 0);
        assert!(
            inbox
                .get("ses_parent", &format!("msg_{}", info.id))
                .expect("inspect command report")
                .is_none()
        );
    }

    #[cfg(unix)]
    fn background_input(directory: &Path, command: &str) -> BackgroundExecutionInput {
        let arguments = vec![OsString::from("-c"), OsString::from(command)];
        let request = PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: arguments.clone(),
            cwd: directory.to_owned(),
            environment: std::env::vars_os().collect::<BTreeMap<_, _>>(),
            policy: SandboxPolicy::new(
                directory,
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Allowed,
            )
            .expect("test sandbox policy"),
        };
        let prepared = PreparedCommand::from_backend(
            request,
            OsString::from("/bin/sh"),
            arguments,
            &SandboxCapabilities {
                backend: "test_direct".to_owned(),
                executable: Some(Path::new("/bin/sh").to_owned()),
                read_only: true,
                workspace_write: true,
                danger_full_access: false,
                network_isolation: true,
            },
            vec![directory.to_owned()],
            Vec::new(),
        );
        BackgroundExecutionInput {
            prepared,
            session_id: "ses_parent".to_owned(),
            title: "notification test".to_owned(),
            command: command.to_owned(),
            hard_ceiling: Duration::from_secs(2),
            retention: BackgroundExecutionRetention::Durable,
        }
    }
}
