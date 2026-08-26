//! Production child-session delegation and durable background delivery.
//!
//! Foreground and background calls use the same child runner. A foreground call
//! carries the parent turn's interrupt into the runner and drains shutdown before
//! returning cancellation. A background call first creates a durable queued job,
//! returns its independent job id, and starts only after fair delegation admission.
//! Terminal state and the optional parent report commit in one SQLite transaction.
//! Parent wake-up happens after that commit, so a process loss can delay delivery but
//! cannot erase the report.

use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SessionInput};
use zuno_db::job::{
    AgentJob, AgentJobStore, JobSettlement, JobStatus, JobSubject, NewAgentJob,
    ReportDelivery as DbReportDelivery,
};
use zuno_engine::interrupt::{SoftInterruptMessage, SoftInterruptSource};
use zuno_engine::r#loop::{TurnEvent, event_channel};
use zuno_engine::status::{SessionRunGuard, SessionRunRegistry};
use zuno_engine::wake::{PendingInputDriver, SessionWakeCoordinator};
use zuno_tool::{InterruptHandle, PermissionAsker};
use zuno_tools::question::QuestionAsker;
use zuno_tools::task::{
    ChildTurn, ChildTurnError, ChildTurnHost, ChildTurnRequest,
    ReportDelivery as ToolReportDelivery,
};

use super::delegation::DelegationLimiter;
use super::turn::{SessionChoice, TurnHost, TurnHostDependencies, TurnOptions, TurnPlan};
use crate::environment::StartupEnvironment;

/// How deep a delegation chain may be walked before the walk is called cyclic.
///
/// `session.parent_id` has no foreign key, so nothing in the schema prevents an
/// `a -> b -> a` pair; `zuno-db`'s own subtree walk keeps a visited set for the same
/// reason. A bound is enough here because any real chain is bounded by
/// `subagent_depth`, which is single digits.
const MAX_ANCESTRY_WALK: u32 = 64;

/// One process-local generation shared by every host observing the same work.
#[derive(Debug, Clone)]
pub(crate) struct ChangeNotifier {
    sender: watch::Sender<u64>,
}

impl Default for ChangeNotifier {
    fn default() -> Self {
        let (sender, _receiver) = watch::channel(0);
        Self { sender }
    }
}

impl ChangeNotifier {
    pub(crate) fn changed(&self) {
        self.sender.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.sender.subscribe()
    }
}

/// Owns background tasks started in one workspace process.
///
/// A host only borrows this owner. Session switches therefore cannot detach a task,
/// while the process surface can cancel or drain the complete set before its Tokio
/// runtime disappears.
#[derive(Debug, Clone)]
pub(crate) struct BackgroundJobSupervisor {
    tasks: Arc<Mutex<Vec<ManagedJob>>>,
    next_task: Arc<AtomicU64>,
    waiter: Arc<tokio::sync::Mutex<()>>,
    changed: ChangeNotifier,
    delegations: DelegationLimiter,
    children: ChildSessionSpecs,
}

impl Default for BackgroundJobSupervisor {
    fn default() -> Self {
        let default_delegations =
            zuno_config::schema::ResolvedConcurrencyConfig::default().delegations;
        Self {
            tasks: Arc::new(Mutex::new(Vec::new())),
            next_task: Arc::new(AtomicU64::new(0)),
            waiter: Arc::new(tokio::sync::Mutex::new(())),
            changed: ChangeNotifier::default(),
            delegations: DelegationLimiter::new(
                NonZeroUsize::new(usize::from(default_delegations))
                    .expect("the config default delegation limit is non-zero"),
            ),
            children: ChildSessionSpecs::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct ChildSessionSpec {
    request: ChildTurnRequest,
    agent: String,
    model: String,
    effort: Option<zuno_llm::effort::ReasoningEffort>,
}

#[derive(Debug, Clone, Default)]
struct ChildSessionSpecs(Arc<Mutex<BTreeMap<String, ChildSessionSpec>>>);

impl ChildSessionSpecs {
    fn remember(
        &self,
        session_id: &str,
        request: &ChildTurnRequest,
        agent: &str,
        model: &str,
        effort: Option<zuno_llm::effort::ReasoningEffort>,
    ) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                session_id.to_owned(),
                ChildSessionSpec {
                    request: request.clone(),
                    agent: agent.to_owned(),
                    model: model.to_owned(),
                    effort,
                },
            );
    }

    fn get(&self, session_id: &str) -> Option<ChildSessionSpec> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned()
    }
}

#[derive(Debug)]
struct ManagedJob {
    internal_id: u64,
    id: String,
    parent_session_id: String,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl BackgroundJobSupervisor {
    /// Return the workspace-wide delegation budget after applying current config.
    pub(crate) fn delegation_limiter(&self, limit: NonZeroUsize) -> DelegationLimiter {
        self.delegations.set_limit(limit);
        self.delegations.clone()
    }

    pub(crate) fn notifier(&self) -> ChangeNotifier {
        self.changed.clone()
    }

    fn notify_changed(&self) {
        self.changed.changed();
    }

    pub(crate) fn spawn(
        &self,
        id: impl Into<String>,
        parent_session_id: impl Into<String>,
        cancellation: CancellationToken,
        task: impl Future<Output = ()> + Send + 'static,
    ) {
        let internal_id = self.next_task.fetch_add(1, Ordering::Relaxed);
        let changed = self.changed.clone();
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(ManagedJob {
                internal_id,
                id: id.into(),
                parent_session_id: parent_session_id.into(),
                cancellation,
                task: Some(tokio::spawn(async move {
                    task.await;
                    changed.changed();
                })),
            });
        self.notify_changed();
    }

    /// Adopt an already-spawned task and make cancellation abort-and-join it.
    pub(crate) fn supervise_handle(
        &self,
        id: impl Into<String>,
        parent_session_id: impl Into<String>,
        cancellation: CancellationToken,
        mut task: JoinHandle<()>,
    ) {
        let task_cancellation = cancellation.clone();
        self.spawn(id, parent_session_id, cancellation, async move {
            tokio::select! {
                outcome = &mut task => {
                    if let Err(error) = outcome {
                        tracing::error!(%error, "owned background task panicked");
                    }
                }
                () = task_cancellation.cancelled() => {
                    task.abort();
                    let _cancelled = task.await;
                }
            }
        });
    }

    /// Request cancellation without replaying or directly settling the job.
    pub(crate) fn cancel(&self, parent_session_id: &str, job_id: &str) -> bool {
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(job) = tasks.iter().find(|job| {
            job.id == job_id
                && job.parent_session_id == parent_session_id
                && job.task.as_ref().is_none_or(|task| !task.is_finished())
        }) else {
            return false;
        };
        job.cancellation.cancel();
        true
    }

    /// Request cancellation for every task without claiming it has stopped yet.
    pub(crate) fn cancel_all(&self) {
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for job in tasks
            .iter()
            .filter(|job| job.task.as_ref().is_none_or(|task| !task.is_finished()))
        {
            job.cancellation.cancel();
        }
    }

    /// Whether this process still owns a task that can write one session's state.
    pub(crate) fn has_running_tasks(&self, parent_session_id: &str) -> bool {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|job| job.parent_session_id == parent_session_id)
            .any(|job| job.task.as_ref().is_none_or(|task| !task.is_finished()))
    }

    /// Wait for every task this supervisor owns.
    pub(crate) async fn wait_all(&self) {
        let _waiter = self.waiter.lock().await;
        loop {
            let next = {
                let mut tasks = self
                    .tasks
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                tasks
                    .iter_mut()
                    .find_map(|job| job.task.take().map(|task| (job.internal_id, task)))
            };
            let Some((internal_id, task)) = next else {
                return;
            };
            if let Err(error) = task.await {
                tracing::error!(%error, "background subagent task panicked");
            }
            self.tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|job| job.internal_id != internal_id);
        }
    }
}

#[async_trait]
impl zuno_tools::job_cancel::JobController for BackgroundJobSupervisor {
    async fn cancel(
        &self,
        parent_session_id: &str,
        job_id: &str,
    ) -> Result<zuno_tools::job_cancel::CancelOutcome, String> {
        let requested = self.cancel(parent_session_id, job_id);
        Ok(zuno_tools::job_cancel::CancelOutcome {
            requested,
            message: if requested {
                "the live executor accepted the cancellation request".to_owned()
            } else {
                "this process has no running executor for the job; inspect durable state before \
                 retrying or reconciling"
                    .to_owned()
            },
        })
    }
}

#[async_trait]
trait DelegatedTurnRunner: Send + Sync + 'static {
    async fn run(
        &self,
        session_id: &str,
        request: &ChildTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<String, String>;
}

/// Durable replay and identity published before a child emits its first live event.
#[derive(Debug, Clone)]
pub(crate) struct ChildSessionOpened {
    pub(crate) session_id: String,
    pub(crate) parent_session_id: String,
    pub(crate) title: String,
    pub(crate) agent: String,
    pub(crate) model: String,
    pub(crate) effort: Option<zuno_llm::effort::ReasoningEffort>,
    pub(crate) messages: Vec<zuno_tui::views::message::Message>,
    pub(crate) usage: Option<zuno_types::UsageSnapshot>,
}

/// Optional process surface that observes independently running child sessions.
///
/// The observer is synchronous by design: implementations update a short in-memory
/// projection and nudge their own event loop. A child turn never waits on terminal
/// rendering, and a non-interactive surface simply supplies no observer.
pub(crate) trait ChildTurnObserver: Send + Sync + 'static {
    fn opened(&self, opened: ChildSessionOpened);
    fn event(&self, session_id: &str, event: &TurnEvent);
}

#[async_trait]
pub(crate) trait ParentReportWake: Send + Sync + 'static {
    async fn wake(&self, report: SessionInput) -> Result<(), String>;
}

/// Everything a child host inherits from its parent composition.
pub(crate) struct ChildSessionContext {
    pub(crate) database: Arc<zuno_db::pool::Pool>,
    pub(crate) environment: StartupEnvironment,
    pub(crate) directory: PathBuf,
    pub(crate) approval: Arc<dyn PermissionAsker>,
    pub(crate) question: Option<Arc<dyn QuestionAsker>>,
    pub(crate) runs: SessionRunRegistry,
    pub(crate) mcp: Option<zuno_mcp::Catalog>,
    pub(crate) observer: Option<Arc<dyn ChildTurnObserver>>,
    pub(crate) parent_agent: String,
    pub(crate) parent_model: String,
    pub(crate) parent_effort: Option<zuno_llm::effort::ReasoningEffort>,
    pub(crate) delegation_limiter: DelegationLimiter,
    pub(crate) supervisor: BackgroundJobSupervisor,
}

/// Dependencies for user-authored input sent directly to an observed child session.
pub(crate) struct InteractiveChildInputContext {
    pub(crate) database: Arc<zuno_db::pool::Pool>,
    pub(crate) environment: StartupEnvironment,
    pub(crate) directory: PathBuf,
    pub(crate) approval: Arc<dyn PermissionAsker>,
    pub(crate) question: Option<Arc<dyn QuestionAsker>>,
    pub(crate) runs: SessionRunRegistry,
    pub(crate) mcp: Option<zuno_mcp::Catalog>,
    pub(crate) observer: Option<Arc<dyn ChildTurnObserver>>,
    pub(crate) supervisor: BackgroundJobSupervisor,
}

/// Delegation backed by a real child session and a real turn.
#[derive(Clone)]
pub(crate) struct ChildSessionHost {
    database: Arc<zuno_db::pool::Pool>,
    runner: Arc<dyn DelegatedTurnRunner>,
    wake: Arc<dyn ParentReportWake>,
    delegation_limiter: DelegationLimiter,
    supervisor: BackgroundJobSupervisor,
    job_store: AgentJobStore,
    inbox: SessionInbox,
}

impl ChildSessionHost {
    pub(crate) fn new(context: ChildSessionContext) -> Result<Self, String> {
        let pool = Arc::clone(&context.database);
        let inbox = SessionInbox::new(Arc::clone(&pool));
        let runner: Arc<dyn DelegatedTurnRunner> = Arc::new(ProductionDelegatedTurnRunner {
            database: Arc::clone(&pool),
            environment: context.environment.clone(),
            directory: context.directory.clone(),
            approval: Arc::clone(&context.approval),
            question: context.question.clone(),
            runs: context.runs.clone(),
            mcp: context.mcp.clone(),
            observer: context.observer.clone(),
            children: context.supervisor.children.clone(),
        });
        let parent_driver: Arc<dyn PendingInputDriver> = Arc::new(ParentReportDriver {
            database: Arc::clone(&pool),
            environment: context.environment,
            directory: context.directory,
            approval: context.approval,
            question: context.question,
            runs: context.runs.clone(),
            mcp: context.mcp,
            observer: context.observer,
            inbox: inbox.clone(),
            agent: context.parent_agent,
            model: context.parent_model,
            effort: context.parent_effort,
        });
        let wake: Arc<dyn ParentReportWake> = Arc::new(CoordinatedParentWake {
            coordinator: SessionWakeCoordinator::new(inbox.clone(), context.runs, parent_driver),
        });
        Ok(Self {
            database: pool,
            runner,
            wake,
            delegation_limiter: context.delegation_limiter,
            supervisor: context.supervisor,
            job_store: AgentJobStore::new(context.database),
            inbox,
        })
    }

    #[cfg(test)]
    fn with_components(
        database: zuno_paths::DbLocation,
        runner: Arc<dyn DelegatedTurnRunner>,
        wake: Arc<dyn ParentReportWake>,
        delegation_limiter: DelegationLimiter,
        supervisor: BackgroundJobSupervisor,
    ) -> Result<Self, String> {
        let pool = Arc::new(zuno_db::pool::Pool::open(&database).map_err(to_string)?);
        Ok(Self {
            database: Arc::clone(&pool),
            runner,
            wake,
            delegation_limiter,
            supervisor,
            job_store: AgentJobStore::new(Arc::clone(&pool)),
            inbox: SessionInbox::new(pool),
        })
    }

    /// Re-deliver this parent's committed reports that no driver has claimed.
    pub(crate) async fn recover_pending_reports(
        &self,
        parent_session_id: &str,
    ) -> Result<usize, String> {
        let jobs = self
            .job_store
            .pending_reports_for(parent_session_id)
            .map_err(to_string)?;
        if jobs.is_empty() {
            return Ok(0);
        }
        let pending = self.inbox.pending(parent_session_id).map_err(to_string)?;
        let mut recovered = 0_usize;
        for job in jobs {
            let Some(input_id) = job.report_input_id.as_deref() else {
                continue;
            };
            let Some(report) = pending.iter().find(|input| input.id == input_id).cloned() else {
                continue;
            };
            self.wake.wake(report).await?;
            recovered = recovered.saturating_add(1);
        }
        Ok(recovered)
    }

    /// Reconcile process-owned native child jobs without replaying their work.
    pub(crate) fn recover_interrupted(&self, parent_session_id: &str) -> Result<usize, String> {
        let active = self
            .job_store
            .active_child_sessions_for(parent_session_id)
            .map_err(to_string)?;
        let mut recovered = 0_usize;
        for job in active {
            let completed = zuno_db::message::now_millis();
            let (status, message, settlement) = match job.status {
                JobStatus::Queued => {
                    let message = format!(
                        "Background subagent job `{}` was cancelled because the Zuno process \
                         restarted before execution capacity was admitted; no child turn was run",
                        job.id
                    );
                    let report = report_for_job(&job, "cancelled", &message, completed);
                    (
                        "cancelled",
                        message.clone(),
                        JobSettlement::cancelled(message, completed, report),
                    )
                }
                JobStatus::Running => {
                    let message = format!(
                        "Background subagent job `{}` has an uncertain outcome because the Zuno \
                         process lost its child-turn executor; completed side effects are not replayed",
                        job.id
                    );
                    let report = report_for_job(&job, "uncertain", &message, completed);
                    (
                        "uncertain",
                        message.clone(),
                        JobSettlement::uncertain(message, completed, report),
                    )
                }
                JobStatus::Completed
                | JobStatus::Failed
                | JobStatus::Cancelled
                | JobStatus::Uncertain => continue,
            };
            self.job_store
                .settle(&job.id, settlement)
                .map_err(to_string)?;
            tracing::info!(job_id = %job.id, %status, %message, "reconciled interrupted child job");
            recovered = recovered.saturating_add(1);
        }
        Ok(recovered)
    }

    /// Share the parent's durable report coordinator with another job provider.
    pub(crate) fn wake_handle(&self) -> Arc<dyn ParentReportWake> {
        Arc::clone(&self.wake)
    }

    /// Open a connection of this host's own.
    ///
    /// Not the parent's: `run_turn` holds that one mutably for the whole turn, and a
    /// tool has no way to reach it. See the module docs on why that is sound.
    fn connect(&self) -> Result<rusqlite::Connection, ChildTurnError> {
        self.database
            .open_connection()
            .map_err(|error| ChildTurnError::Host(error.to_string()))
    }

    /// The child session to run in: `task_id`'s, or a fresh one.
    ///
    /// A resumed session must be a child **of this parent**. Accepting any session id
    /// would let one delegation continue another session's child, which is both a
    /// confusing transcript and a way to write into a session the caller was never
    /// given.
    fn session_for(&self, request: &ChildTurnRequest) -> Result<String, ChildTurnError> {
        let mut connection = self.connect()?;
        if let Some(resume) = &request.resume_session_id {
            let existing = zuno_db::session::get(&connection, resume)
                .map_err(|_error| ChildTurnError::UnknownSession(resume.clone()))?;
            if existing.parent_id.as_deref() != Some(request.parent_session_id.as_str()) {
                return Err(ChildTurnError::UnknownSession(resume.clone()));
            }
            return Ok(existing.id);
        }

        let parent = zuno_db::session::get(&connection, &request.parent_session_id)
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        let child_id = crate::cmd::turn::prefixed_id("ses");
        let title = request
            .description
            .clone()
            .unwrap_or_else(|| format!("Delegated to {}", request.agent));
        let mut input = zuno_db::session::SessionCreate::new(
            &child_id,
            Uuid::new_v4().simple().to_string(),
            &parent.project_id,
            parent.directory.clone(),
            parent.directory.clone(),
            title,
            crate::RUST_PACKAGE_VERSION,
        )
        .with_parent(&request.parent_session_id);
        input.agent = Some(request.agent.clone());
        if let Some(workspace) = parent.workspace_id.clone() {
            input = input.with_workspace(workspace);
        }
        let transaction = connection
            .transaction()
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        zuno_db::session::create(&transaction, &input)
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        transaction
            .commit()
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        Ok(child_id)
    }

    /// Drive one child turn with cancellation owned by a larger orchestration.
    pub(crate) async fn dispatch_foreground(
        &self,
        request: ChildTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<ChildTurn, ChildTurnError> {
        if request.background {
            return Err(ChildTurnError::Host(
                "dispatch_foreground cannot admit a background child".to_owned(),
            ));
        }
        let _permit = self
            .delegation_limiter
            .acquire(&cancellation)
            .await
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        let session_id = self.session_for(&request)?;
        let output = self
            .runner
            .run(&session_id, &request, cancellation)
            .await
            .map_err(ChildTurnError::Host)?;
        Ok(ChildTurn {
            session_id,
            job_id: None,
            output,
        })
    }
}

struct ProductionDelegatedTurnRunner {
    database: Arc<zuno_db::pool::Pool>,
    environment: StartupEnvironment,
    directory: PathBuf,
    approval: Arc<dyn PermissionAsker>,
    question: Option<Arc<dyn QuestionAsker>>,
    runs: SessionRunRegistry,
    mcp: Option<zuno_mcp::Catalog>,
    observer: Option<Arc<dyn ChildTurnObserver>>,
    children: ChildSessionSpecs,
}

#[async_trait]
impl DelegatedTurnRunner for ProductionDelegatedTurnRunner {
    async fn run(
        &self,
        session_id: &str,
        request: &ChildTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<String, String> {
        if cancellation.is_cancelled() {
            return Err("child turn was cancelled before it started".to_owned());
        }
        let options = TurnOptions {
            directory: Some(self.directory.clone()),
            model: request.model.as_ref().map(|model| model.model.clone()),
            agent: Some(request.agent.clone()),
            preset: None,
            session: SessionChoice::Existing(session_id.to_owned()),
            title: request.description.clone(),
            effort: request.effort,
            extension_composition: super::turn::ExtensionComposition::Active,
        };
        let mut plan = TurnPlan::resolve(&options, &self.environment).await?;
        self.children.remember(
            session_id,
            request,
            plan.agent_name(),
            &plan.qualified_model(),
            plan.effort(),
        );
        let parent_attempt = request.parent_attempt.as_deref().ok_or_else(|| {
            "delegated child turn is missing the immutable parent Attempt snapshot".to_owned()
        })?;
        plan.inherit_orchestration(
            parent_attempt,
            request.workflow.as_deref(),
            request.workflow_node.as_deref(),
        )?;
        let mut host = TurnHost::open_with_dependencies(
            plan,
            &self.environment,
            TurnHostDependencies {
                approval: Arc::clone(&self.approval),
                question: self.question.clone(),
                runs: self.runs.clone(),
                mcp: self.mcp.clone(),
                database: Arc::clone(&self.database),
                child_observer: self.observer.clone(),
            },
        )
        .await?;
        host.activate_extension_composition()?;
        if let Some(observer) = self.observer.as_ref() {
            let messages = match host.resumed_history() {
                Ok(history) => {
                    let replay = super::tui_replay::project(history);
                    let omission = replay.omission_notice();
                    let mut messages = replay.messages;
                    if let Some(notice) = omission {
                        messages.push(notice);
                    }
                    messages.push(zuno_tui::views::message::Message::user(
                        request.prompt.clone(),
                    ));
                    messages
                }
                Err(error) => vec![super::tui_replay::failure_notice(session_id, &error)],
            };
            observer.opened(ChildSessionOpened {
                session_id: session_id.to_owned(),
                parent_session_id: request.parent_session_id.clone(),
                title: host
                    .session_title()
                    .map(str::to_owned)
                    .or_else(|| request.description.clone())
                    .unwrap_or_else(|| session_id.to_owned()),
                agent: host.agent_name().to_owned(),
                model: host.qualified_model(),
                effort: host.effort_override(),
                messages,
                usage: Some(host.session_usage().snapshot()),
            });
        }
        let guard = self
            .runs
            .begin_turn(session_id.to_owned())
            .map_err(to_string)?;
        let control = self.runs.control(session_id.to_owned());
        let outcome = {
            let drive = drive_and_drain(
                &mut host,
                &request.prompt,
                None,
                Some(guard),
                session_id,
                self.observer.clone(),
            );
            tokio::pin!(drive);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    let _aborted = control.abort();
                    let _drained = drive.await;
                    Err("child turn was cancelled".to_owned())
                }
                outcome = &mut drive => outcome,
            }
        };
        let shutdown = host.shutdown().await;
        super::turn::finish_with_shutdown(outcome, shutdown)?;
        child_answer(&self.database, session_id)
    }
}

/// Durable direct-input path for a child session shown by the TUI.
#[derive(Clone)]
pub(crate) struct InteractiveChildInput {
    database: Arc<zuno_db::pool::Pool>,
    inbox: SessionInbox,
    runs: SessionRunRegistry,
    coordinator: SessionWakeCoordinator,
    supervisor: BackgroundJobSupervisor,
    observer: Option<Arc<dyn ChildTurnObserver>>,
}

impl InteractiveChildInput {
    pub(crate) fn new(context: InteractiveChildInputContext) -> Self {
        let inbox = SessionInbox::new(Arc::clone(&context.database));
        let driver: Arc<dyn PendingInputDriver> = Arc::new(InteractiveChildInputDriver {
            database: Arc::clone(&context.database),
            environment: context.environment,
            directory: context.directory,
            approval: context.approval,
            question: context.question,
            runs: context.runs.clone(),
            mcp: context.mcp,
            observer: context.observer.clone(),
            inbox: inbox.clone(),
            children: context.supervisor.children.clone(),
        });
        let coordinator = SessionWakeCoordinator::new(inbox.clone(), context.runs.clone(), driver);
        Self {
            database: context.database,
            inbox,
            runs: context.runs,
            coordinator,
            supervisor: context.supervisor,
            observer: context.observer,
        }
    }

    #[cfg(test)]
    fn with_driver(
        database: Arc<zuno_db::pool::Pool>,
        runs: SessionRunRegistry,
        supervisor: BackgroundJobSupervisor,
        driver: Arc<dyn PendingInputDriver>,
    ) -> Self {
        let inbox = SessionInbox::new(Arc::clone(&database));
        let coordinator = SessionWakeCoordinator::new(inbox.clone(), runs.clone(), driver);
        Self {
            database,
            inbox,
            runs,
            coordinator,
            supervisor,
            observer: None,
        }
    }

    /// Admit one plain-text user message and arrange active steering or idle continuation.
    pub(crate) fn submit_text(
        &self,
        session_id: &str,
        mut prompt: Value,
        text: String,
    ) -> Result<String, String> {
        if text.trim().is_empty() {
            return Err("interactive child input cannot be empty".to_owned());
        }
        let connection = self.database.open_connection().map_err(to_string)?;
        let session = zuno_db::session::get(&connection, session_id).map_err(to_string)?;
        if session.parent_id.is_none() {
            return Err(format!(
                "session `{session_id}` is not a child session and cannot receive child input"
            ));
        }
        let object = prompt.as_object_mut().ok_or_else(|| {
            "interactive child input must persist a structured prompt object".to_owned()
        })?;
        object.insert("text".to_owned(), Value::String(text.clone()));
        let input_id = format!("msg_{}", Uuid::new_v4().simple());
        let input = self
            .inbox
            .admit(NewSessionInput::new(
                input_id.clone(),
                session_id,
                prompt,
                InputDelivery::Steer,
                zuno_db::message::now_millis(),
            ))
            .map_err(to_string)?;

        let coordinator = self.coordinator.clone();
        let inbox = self.inbox.clone();
        let control = self.runs.control(session_id.to_owned());
        let observer = self.observer.as_ref().map(Arc::clone);
        let task_session_id = session_id.to_owned();
        let task_input_id = input_id.clone();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        self.supervisor.spawn(
            format!("interactive-{input_id}"),
            session_id.to_owned(),
            cancellation,
            async move {
                let delivery = coordinator.deliver(
                    &task_session_id,
                    &task_input_id,
                    SoftInterruptMessage {
                        input_id: Some(task_input_id.clone()),
                        content: text,
                        images: Vec::new(),
                        urgent: false,
                        source: SoftInterruptSource::User,
                    },
                );
                tokio::pin!(delivery);
                let outcome = tokio::select! {
                    biased;
                    () = task_cancellation.cancelled() => {
                        let _aborted = control.abort();
                        delivery.await
                    }
                    outcome = &mut delivery => outcome,
                };
                if let Err(error) = outcome {
                    let _failed =
                        inbox.mark_failed(&task_session_id, &task_input_id, error.clone());
                    if let Some(observer) = observer.as_ref() {
                        observer.event(
                            &task_session_id,
                            &TurnEvent::Provider {
                                step: 0,
                                event: zuno_llm::event::StreamEvent::Error {
                                    message: format!(
                                        "interactive child input `{task_input_id}` failed: {error}"
                                    ),
                                    retry_after: None,
                                },
                            },
                        );
                    }
                    tracing::error!(
                        session_id = %task_session_id,
                        input_id = %task_input_id,
                        %error,
                        "interactive child input failed"
                    );
                }
            },
        );
        Ok(input.id)
    }
}

struct InteractiveChildInputDriver {
    database: Arc<zuno_db::pool::Pool>,
    environment: StartupEnvironment,
    directory: PathBuf,
    approval: Arc<dyn PermissionAsker>,
    question: Option<Arc<dyn QuestionAsker>>,
    runs: SessionRunRegistry,
    mcp: Option<zuno_mcp::Catalog>,
    observer: Option<Arc<dyn ChildTurnObserver>>,
    inbox: SessionInbox,
    children: ChildSessionSpecs,
}

#[async_trait]
impl PendingInputDriver for InteractiveChildInputDriver {
    async fn drive(&self, input: SessionInput, guard: SessionRunGuard) -> Result<(), String> {
        let text = input
            .prompt
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "interactive child input `{}` has no string `text` field",
                    input.id
                )
            })?
            .to_owned();
        let spec = self.children.get(&input.session_id).ok_or_else(|| {
            format!(
                "child session `{}` has no process-owned continuation identity",
                input.session_id
            )
        })?;
        let options = TurnOptions {
            directory: Some(self.directory.clone()),
            model: Some(spec.model.clone()),
            agent: Some(spec.agent.clone()),
            preset: None,
            session: SessionChoice::Existing(input.session_id.clone()),
            title: spec.request.description.clone(),
            effort: spec.effort,
            extension_composition: super::turn::ExtensionComposition::Active,
        };
        let mut plan = TurnPlan::resolve(&options, &self.environment).await?;
        if let Some(parent_attempt) = spec.request.parent_attempt.as_deref() {
            plan.inherit_orchestration(
                parent_attempt,
                spec.request.workflow.as_deref(),
                spec.request.workflow_node.as_deref(),
            )?;
        }
        let mut host = TurnHost::open_with_dependencies(
            plan,
            &self.environment,
            TurnHostDependencies {
                approval: Arc::clone(&self.approval),
                question: self.question.clone(),
                runs: self.runs.clone(),
                mcp: self.mcp.clone(),
                database: Arc::clone(&self.database),
                child_observer: self.observer.clone(),
            },
        )
        .await?;
        host.activate_extension_composition()?;
        if let Some(observer) = self.observer.as_ref() {
            let messages = match host.resumed_history() {
                Ok(history) => {
                    let replay = super::tui_replay::project(history);
                    let omission = replay.omission_notice();
                    let mut messages = replay.messages;
                    if let Some(notice) = omission {
                        messages.push(notice);
                    }
                    messages.push(zuno_tui::views::message::Message::user(text.clone()));
                    messages
                }
                Err(error) => {
                    vec![super::tui_replay::failure_notice(&input.session_id, &error)]
                }
            };
            observer.opened(ChildSessionOpened {
                session_id: input.session_id.clone(),
                parent_session_id: spec.request.parent_session_id.clone(),
                title: host
                    .session_title()
                    .map(str::to_owned)
                    .or_else(|| spec.request.description.clone())
                    .unwrap_or_else(|| input.session_id.clone()),
                agent: host.agent_name().to_owned(),
                model: host.qualified_model(),
                effort: host.effort_override(),
                messages,
                usage: Some(host.session_usage().snapshot()),
            });
        }
        let promoted = self
            .inbox
            .promote_id(&input.session_id, &input.id)
            .map_err(to_string)?;
        if promoted.is_none() {
            return host.shutdown().await;
        }
        let outcome = drive_and_drain(
            &mut host,
            &text,
            Some(input.id.as_str()),
            Some(guard),
            input.session_id.as_str(),
            self.observer.clone(),
        )
        .await;
        let shutdown = host.shutdown().await;
        super::turn::finish_with_shutdown(outcome, shutdown)
    }
}

struct ParentReportDriver {
    database: Arc<zuno_db::pool::Pool>,
    environment: StartupEnvironment,
    directory: PathBuf,
    approval: Arc<dyn PermissionAsker>,
    question: Option<Arc<dyn QuestionAsker>>,
    runs: SessionRunRegistry,
    mcp: Option<zuno_mcp::Catalog>,
    observer: Option<Arc<dyn ChildTurnObserver>>,
    inbox: SessionInbox,
    agent: String,
    model: String,
    effort: Option<zuno_llm::effort::ReasoningEffort>,
}

#[async_trait]
impl PendingInputDriver for ParentReportDriver {
    async fn drive(&self, input: SessionInput, guard: SessionRunGuard) -> Result<(), String> {
        let text = input
            .prompt
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "background report input `{}` has no string `text` field",
                    input.id
                )
            })?
            .to_owned();
        let options = TurnOptions {
            directory: Some(self.directory.clone()),
            model: Some(self.model.clone()),
            agent: Some(self.agent.clone()),
            preset: None,
            session: SessionChoice::Existing(input.session_id.clone()),
            title: None,
            effort: self.effort,
            extension_composition: super::turn::ExtensionComposition::Active,
        };
        let plan = TurnPlan::resolve(&options, &self.environment).await?;
        let mut host = TurnHost::open_with_dependencies(
            plan,
            &self.environment,
            TurnHostDependencies {
                approval: Arc::clone(&self.approval),
                question: self.question.clone(),
                runs: self.runs.clone(),
                mcp: self.mcp.clone(),
                database: Arc::clone(&self.database),
                child_observer: self.observer.clone(),
            },
        )
        .await?;
        host.activate_extension_composition()?;
        let promoted = self
            .inbox
            .promote_id(&input.session_id, &input.id)
            .map_err(to_string)?;
        if promoted.is_none() {
            return host.shutdown().await;
        }
        let outcome = drive_and_drain(
            &mut host,
            &text,
            Some(input.id.as_str()),
            Some(guard),
            input.session_id.as_str(),
            self.observer.clone(),
        )
        .await;
        let shutdown = host.shutdown().await;
        super::turn::finish_with_shutdown(outcome, shutdown)
    }
}

struct CoordinatedParentWake {
    coordinator: SessionWakeCoordinator,
}

#[async_trait]
impl ParentReportWake for CoordinatedParentWake {
    async fn wake(&self, report: SessionInput) -> Result<(), String> {
        let content = report
            .prompt
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "background report input `{}` has no string `text` field",
                    report.id
                )
            })?
            .to_owned();
        self.coordinator
            .deliver(
                &report.session_id,
                &report.id,
                SoftInterruptMessage {
                    input_id: Some(report.id.clone()),
                    content,
                    images: Vec::new(),
                    urgent: false,
                    source: SoftInterruptSource::BackgroundTask,
                },
            )
            .await?;
        Ok(())
    }
}

async fn drive_and_drain(
    host: &mut TurnHost,
    prompt: &str,
    message_id: Option<&str>,
    guard: Option<SessionRunGuard>,
    session_id: &str,
    observer: Option<Arc<dyn ChildTurnObserver>>,
) -> Result<(), String> {
    let (sender, receiver) = event_channel();
    let drive = async {
        let outcome = match guard {
            Some(guard) => {
                host.drive_with_message_id_and_guard(prompt, message_id, guard, sender.clone())
                    .await
            }
            None => {
                host.drive_with_message_id(prompt, message_id, sender.clone())
                    .await
            }
        };
        drop(sender);
        outcome
    };
    let drain = forward_child_events(session_id.to_owned(), receiver, observer);
    let (outcome, ()) = tokio::join!(drive, drain);
    outcome
}

async fn forward_child_events(
    session_id: String,
    mut receiver: mpsc::Receiver<TurnEvent>,
    observer: Option<Arc<dyn ChildTurnObserver>>,
) {
    while let Some(event) = receiver.recv().await {
        if let Some(observer) = observer.as_ref() {
            observer.event(&session_id, &event);
        }
    }
}

fn child_answer(database: &zuno_db::pool::Pool, session_id: &str) -> Result<String, String> {
    let connection = database.open_connection().map_err(to_string)?;
    let store = zuno_db::message::MessageStore::new(&connection);
    let messages = store.messages_for_session(session_id).map_err(to_string)?;
    let Some(last) = messages
        .iter()
        .rev()
        .find(|message| message.role == zuno_db::message::MessageRole::Assistant)
    else {
        return Ok(String::new());
    };
    let parts = store
        .parts_by_message_kind(
            std::slice::from_ref(&last.id),
            zuno_db::message::PartKind::Text,
        )
        .map_err(to_string)?;
    Ok(parts
        .get(&last.id)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.data.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default())
}

#[async_trait]
impl ChildTurnHost for ChildSessionHost {
    async fn delegation_depth(&self, session_id: &str) -> Result<u32, ChildTurnError> {
        let connection = self.connect()?;
        let mut depth = 0_u32;
        let mut current = session_id.to_owned();
        while depth < MAX_ANCESTRY_WALK {
            let session = zuno_db::session::get(&connection, &current)
                .map_err(|error| ChildTurnError::Host(error.to_string()))?;
            match session.parent_id {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                }
                None => return Ok(depth),
            }
        }
        Err(ChildTurnError::Host(format!(
            "session `{session_id}` has more than {MAX_ANCESTRY_WALK} ancestors, which \
             means its `parent_id` chain contains a cycle; delegation depth cannot be \
             established"
        )))
    }

    async fn dispatch(
        &self,
        request: ChildTurnRequest,
        interrupt: Arc<dyn InterruptHandle>,
    ) -> Result<ChildTurn, ChildTurnError> {
        if !request.background {
            let cancellation = CancellationToken::new();
            let dispatch = self.dispatch_foreground(request, cancellation.clone());
            tokio::pin!(dispatch);
            return tokio::select! {
                biased;
                () = interrupt.notified() => {
                    cancellation.cancel();
                    match dispatch.await {
                        Ok(_) => Err(ChildTurnError::Host("child turn was cancelled".to_owned())),
                        Err(error) => Err(error),
                    }
                }
                result = &mut dispatch => result,
            };
        }
        if interrupt.is_set() {
            return Err(ChildTurnError::Host(
                "background child was cancelled before admission".to_owned(),
            ));
        }
        let session_id = self.session_for(&request)?;

        let job_id = crate::cmd::turn::prefixed_id("job");
        let delivery = match request.report_delivery {
            ToolReportDelivery::NextStep => DbReportDelivery::NextStep,
            ToolReportDelivery::Quiet => DbReportDelivery::Quiet,
        };
        self.job_store
            .create(
                NewAgentJob::new(
                    job_id.clone(),
                    request.parent_session_id.clone(),
                    JobSubject::child_session(session_id.clone()),
                    delivery,
                    zuno_db::message::now_millis(),
                )
                .queued()
                .with_orchestration_snapshot(request.parent_attempt.as_deref().cloned()),
            )
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;

        let runner = Arc::clone(&self.runner);
        let wake = Arc::clone(&self.wake);
        let delegation_limiter = self.delegation_limiter.clone();
        let job_store = self.job_store.clone();
        let background_job_id = job_id.clone();
        let background_session_id = session_id.clone();
        let parent_session_id = request.parent_session_id.clone();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        self.supervisor.spawn(
            job_id.clone(),
            parent_session_id,
            cancellation,
            async move {
                let outcome = match delegation_limiter.acquire(&task_cancellation).await {
                    Ok(_permit) => {
                        match job_store.start(&background_job_id, zuno_db::message::now_millis()) {
                            Ok(_) => {
                                runner
                                    .run(
                                        &background_session_id,
                                        &request,
                                        task_cancellation.clone(),
                                    )
                                    .await
                            }
                            Err(error) => Err(error.to_string()),
                        }
                    }
                    Err(error) => Err(error.to_string()),
                };
                let completed = zuno_db::message::now_millis();
                let (settlement, report_text) = if task_cancellation.is_cancelled() {
                    let text = format!(
                        "Background subagent `{background_session_id}` cancelled job \
                     `{background_job_id}`."
                    );
                    (
                        JobSettlement::cancelled(
                            "cancelled by user",
                            completed,
                            report_input(
                                &request,
                                &background_job_id,
                                &background_session_id,
                                "cancelled",
                                &text,
                                completed,
                            ),
                        ),
                        text,
                    )
                } else {
                    match outcome {
                        Ok(output) => {
                            let text = format!(
                                "Background subagent `{background_session_id}` completed job \
                         `{background_job_id}`.\n\n{output}"
                            );
                            (
                                JobSettlement::completed(
                                    json!({"text": output}),
                                    completed,
                                    report_input(
                                        &request,
                                        &background_job_id,
                                        &background_session_id,
                                        "completed",
                                        &text,
                                        completed,
                                    ),
                                ),
                                text,
                            )
                        }
                        Err(error) => {
                            let text = format!(
                                "Background subagent `{background_session_id}` failed job \
                         `{background_job_id}`: {error}"
                            );
                            (
                                JobSettlement::failed(
                                    error,
                                    completed,
                                    report_input(
                                        &request,
                                        &background_job_id,
                                        &background_session_id,
                                        "failed",
                                        &text,
                                        completed,
                                    ),
                                ),
                                text,
                            )
                        }
                    }
                };
                match job_store.settle(&background_job_id, settlement) {
                    Ok(settled) => {
                        if let Some(report) = settled.report
                            && let Err(error) = wake.wake(report).await
                        {
                            tracing::error!(
                                job_id = %background_job_id,
                                %error,
                                "background report remains pending after wake failure"
                            );
                        }
                    }
                    Err(error) => tracing::error!(
                        job_id = %background_job_id,
                        %error,
                        report = %report_text,
                        "background job settlement failed"
                    ),
                }
            },
        );

        Ok(ChildTurn {
            session_id,
            job_id: Some(job_id),
            output: "Background subagent started. Its terminal state will be delivered according \
                     to `reportDelivery`."
                .to_owned(),
        })
    }
}

fn report_input(
    request: &ChildTurnRequest,
    job_id: &str,
    child_session_id: &str,
    status: &str,
    text: &str,
    created: i64,
) -> Option<NewSessionInput> {
    (request.report_delivery == ToolReportDelivery::NextStep).then(|| {
        NewSessionInput::new(
            crate::cmd::turn::prefixed_id("input"),
            request.parent_session_id.clone(),
            json!({
                "kind": "subagentReport",
                "jobID": job_id,
                "childSessionID": child_session_id,
                "status": status,
                "text": text,
            }),
            InputDelivery::Queue,
            created,
        )
    })
}

fn report_for_job(
    job: &AgentJob,
    status: &str,
    text: &str,
    created: i64,
) -> Option<NewSessionInput> {
    if job.report_delivery != DbReportDelivery::NextStep {
        return None;
    }
    let JobSubject::ChildSession { session_id } = &job.subject else {
        return None;
    };
    Some(NewSessionInput::new(
        crate::cmd::turn::prefixed_id("input"),
        job.parent_session_id.clone(),
        json!({
            "kind": "subagentReport",
            "jobID": job.id,
            "childSessionID": session_id,
            "status": status,
            "text": text,
        }),
        InputDelivery::Queue,
        created,
    ))
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
#[path = "child_turn_tests.rs"]
mod tests;
