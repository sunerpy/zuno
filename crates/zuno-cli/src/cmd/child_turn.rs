//! Production child-session delegation and durable background delivery.
//!
//! Foreground and background calls use the same child runner. A foreground call
//! carries the parent turn's interrupt into the runner and drains shutdown before
//! returning cancellation. A background call first creates a durable queued job,
//! returns its independent job id, and starts only after fair delegation admission.
//! Terminal state and the optional parent report commit in one SQLite transaction.
//! Parent wake-up happens after that commit, so a process loss can delay delivery but
//! cannot erase the report.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SessionInput};
use zuno_db::job::{
    AgentJob, AgentJobStore, JobSettlement, JobStatus, JobSubject, JobWorkContext, NewAgentJob,
    ReportDelivery as DbReportDelivery,
};
use zuno_engine::interrupt::{SoftInterruptMessage, SoftInterruptSource};
use zuno_engine::r#loop::{TurnEvent, event_channel};
use zuno_engine::planning::PlanningInputSource;
use zuno_engine::status::{SessionRunGuard, SessionRunRegistry};
use zuno_engine::wake::{PendingInputDriver, SessionWakeCoordinator};
use zuno_orchestration::AttemptSnapshot;
use zuno_tool::{InterruptHandle, PermissionAsker};
use zuno_tools::question::QuestionAsker;
use zuno_tools::task::{
    ChildTurn, ChildTurnError, ChildTurnHost, ChildTurnRequest, ChildTurnState,
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
const CHILD_SESSION_METADATA_KIND: &str = "zuno.child";
const CHILD_SESSION_METADATA_SCHEMA_VERSION: u32 = 2;
const CHILD_SESSION_METADATA_MAX_ATTEMPTS: u32 = 3;
const CHILD_SESSION_METADATA_INITIAL_DELAY: Duration = Duration::from_millis(25);
const CHILD_SESSION_METADATA_MAX_DELAY: Duration = Duration::from_millis(250);
const PARENT_WAKE_INITIAL_DELAY: Duration = Duration::from_millis(10);
const PARENT_WAKE_MAX_DELAY: Duration = Duration::from_millis(100);
const PARENT_WAKE_ATTEMPTS: usize = 3;
const FOREGROUND_CHILD_CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
const TASK_REPORT_METADATA_SCHEMA_VERSION: u32 = 2;
const TASK_VERIFICATION_METADATA_KEY: &str = "taskVerification";
const UNCERTAIN_SIDE_EFFECTS_METADATA_KEY: &str = "uncertainSideEffects";

/// Host-generated terminal metadata for one native delegated task.
///
/// The child model supplies only its final text. Durable identity, usage, changed
/// paths, verification evidence, and uncertain side effects are reconstructed from
/// the child session and its typed tool results after execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskReportMetadata {
    schema_version: u32,
    job_id: Option<String>,
    work_context: Option<JobWorkContext>,
    session_id: String,
    parent_session_id: String,
    agent: String,
    status: String,
    final_text: String,
    usage: TaskReportUsage,
    changed_paths: Vec<String>,
    verification_records: Vec<TaskVerificationRecord>,
    uncertain_side_effects: Vec<String>,
    evidence_errors: Vec<String>,
}

struct TaskReportBuild<'a> {
    job_id: Option<&'a str>,
    work_context: Option<JobWorkContext>,
    child_session_id: &'a str,
    evidence_start_rowid: i64,
    status: &'a str,
    final_text: &'a str,
    uncertain_side_effects: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskReportUsage {
    confirmed: TaskReportTokenUsage,
    last_prompt_tokens: Option<u64>,
    estimated_pending_prompt_tokens: Option<u64>,
    context_limit: Option<u64>,
    accounting: Option<String>,
    confirmed_known: bool,
    last_confirmed_at: Option<i64>,
    failed_turns: u64,
    last_failed_at: Option<i64>,
}

impl From<zuno_types::UsageSnapshot> for TaskReportUsage {
    fn from(snapshot: zuno_types::UsageSnapshot) -> Self {
        Self {
            confirmed: snapshot.confirmed.into(),
            last_prompt_tokens: snapshot.last_prompt_tokens,
            estimated_pending_prompt_tokens: snapshot.estimated_pending_prompt_tokens,
            context_limit: snapshot.context_limit,
            accounting: snapshot.accounting.as_str().map(str::to_owned),
            confirmed_known: snapshot.confirmed_known,
            last_confirmed_at: snapshot.last_confirmed_at,
            failed_turns: snapshot.failed_turns,
            last_failed_at: snapshot.last_failed_at,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskReportTokenUsage {
    input: u64,
    output: u64,
    reasoning: u64,
    cache_read: u64,
    cache_write: u64,
    unclassified: u64,
}

impl From<zuno_types::TokenUsage> for TaskReportTokenUsage {
    fn from(usage: zuno_types::TokenUsage) -> Self {
        Self {
            input: usage.input,
            output: usage.output,
            reasoning: usage.reasoning,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            unclassified: usage.unclassified,
        }
    }
}

/// Verification evidence emitted by a host tool, never parsed from child prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TaskVerificationRecord {
    name: String,
    status: String,
    #[serde(default)]
    evidence: Option<String>,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChildSessionSpec {
    parent_session_id: String,
    parent_attempt: Option<AttemptSnapshot>,
    workflow: Option<String>,
    workflow_node: Option<String>,
    description: Option<String>,
    agent: String,
    model: String,
    effort: Option<zuno_llm::effort::ReasoningEffort>,
    provider_options: Map<String, Value>,
    background: bool,
}

impl ChildSessionSpec {
    fn resolved(
        request: &ChildTurnRequest,
        agent: &str,
        model: &str,
        effort: Option<zuno_llm::effort::ReasoningEffort>,
    ) -> Self {
        Self {
            parent_session_id: request.parent_session_id.clone(),
            parent_attempt: request.parent_attempt.as_deref().cloned(),
            workflow: request.workflow.clone(),
            workflow_node: request.workflow_node.clone(),
            description: request.description.clone(),
            agent: agent.to_owned(),
            model: model.to_owned(),
            effort,
            provider_options: request.provider_options.clone(),
            background: request.background,
        }
    }

    fn validate_continuation(&self, candidate: &Self) -> Result<(), String> {
        if self.parent_session_id != candidate.parent_session_id {
            return Err("child continuation `parent` identity changed".to_owned());
        }
        if self.agent != candidate.agent {
            return Err("child continuation `agent` identity changed".to_owned());
        }
        if self.model != candidate.model {
            return Err(
                "child continuation `effective provider/model` identity changed".to_owned(),
            );
        }
        if self.effort != candidate.effort {
            return Err("child continuation `reasoning` identity changed".to_owned());
        }
        if self.provider_options != candidate.provider_options {
            return Err("child continuation provider options changed".to_owned());
        }
        if self.workflow != candidate.workflow || self.workflow_node != candidate.workflow_node {
            return Err("child continuation workflow identity changed".to_owned());
        }
        validate_parent_attempt_authority(
            self.parent_attempt.as_ref(),
            candidate.parent_attempt.as_ref(),
        )
    }
}

fn validate_parent_attempt_authority(
    stored: Option<&AttemptSnapshot>,
    candidate: Option<&AttemptSnapshot>,
) -> Result<(), String> {
    let (Some(stored), Some(candidate)) = (stored, candidate) else {
        return if stored.is_none() && candidate.is_none() {
            Ok(())
        } else {
            Err("child continuation parent Attempt identity changed".to_owned())
        };
    };
    let stored_capability = stored.capability.identity().map_err(to_string)?;
    let candidate_capability = candidate.capability.identity().map_err(to_string)?;
    if stored_capability != candidate_capability {
        return Err("child continuation parent capability generation changed".to_owned());
    }
    if stored.schema_version != candidate.schema_version
        || stored.owner.session_id != candidate.owner.session_id
        || stored.owner.parent_session_id != candidate.owner.parent_session_id
        || stored.owner.parent_attempt != candidate.owner.parent_attempt
        || stored.agent != candidate.agent
        || stored.model != candidate.model
        || stored.tools != candidate.tools
    {
        return Err("child continuation parent Attempt authority changed".to_owned());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChildSessionMetadata {
    kind: String,
    schema_version: u32,
    continuation: ChildSessionSpec,
}

#[derive(Debug, Clone, Default)]
struct ChildSessionSpecs(Arc<Mutex<BTreeMap<String, ChildSessionSpec>>>);

impl ChildSessionSpecs {
    fn remember(&self, session_id: &str, spec: ChildSessionSpec) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_owned(), spec);
    }

    fn get(&self, session_id: &str) -> Option<ChildSessionSpec> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned()
    }

    fn get_or_restore(
        &self,
        database: &Arc<zuno_db::pool::Pool>,
        session_id: &str,
    ) -> Result<ChildSessionSpec, String> {
        if let Some(spec) = self.get(session_id) {
            return Ok(spec);
        }
        let connection = database.open_connection().map_err(to_string)?;
        let session = zuno_db::session::get(&connection, session_id).map_err(to_string)?;
        let encoded = session.metadata.as_deref().ok_or_else(|| {
            format!(
                "child session `{session_id}` has no durable continuation identity; start a new delegation"
            )
        })?;
        let metadata: ChildSessionMetadata = serde_json::from_str(encoded).map_err(|error| {
            format!("child session `{session_id}` has invalid continuation metadata: {error}")
        })?;
        if metadata.kind != CHILD_SESSION_METADATA_KIND
            || metadata.schema_version != CHILD_SESSION_METADATA_SCHEMA_VERSION
        {
            return Err(format!(
                "child session `{session_id}` has unsupported continuation metadata"
            ));
        }
        if session.parent_id.as_deref() != Some(metadata.continuation.parent_session_id.as_str()) {
            return Err(format!(
                "child session `{session_id}` continuation identity does not match its durable parent"
            ));
        }
        if session.agent.as_deref() != Some(metadata.continuation.agent.as_str()) {
            return Err(format!(
                "child session `{session_id}` continuation identity does not match its durable agent"
            ));
        }
        self.remember(session_id, metadata.continuation.clone());
        Ok(metadata.continuation)
    }
}

async fn checkpoint_child_session_spec(
    database: &Arc<zuno_db::pool::Pool>,
    children: &ChildSessionSpecs,
    session_id: &str,
    spec: &ChildSessionSpec,
    resumed: bool,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    if resumed {
        let stored = children.get_or_restore(database, session_id)?;
        return stored.validate_continuation(spec);
    }
    persist_child_session_spec(database, session_id, spec, cancellation).await?;
    children.remember(session_id, spec.clone());
    Ok(())
}

async fn persist_child_session_spec(
    database: &Arc<zuno_db::pool::Pool>,
    session_id: &str,
    spec: &ChildSessionSpec,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let metadata = serde_json::to_string(&ChildSessionMetadata {
        kind: CHILD_SESSION_METADATA_KIND.to_owned(),
        schema_version: CHILD_SESSION_METADATA_SCHEMA_VERSION,
        continuation: spec.clone(),
    })
    .map_err(to_string)?;
    for attempt in 1..=CHILD_SESSION_METADATA_MAX_ATTEMPTS {
        match zuno_db::session::Store::new(database).set_metadata(session_id, &metadata) {
            Ok(_) => return Ok(()),
            Err(zuno_error::DbError::Busy { retry_after })
                if attempt < CHILD_SESSION_METADATA_MAX_ATTEMPTS =>
            {
                let delay = child_session_metadata_retry_delay(attempt, retry_after);
                tracing::warn!(
                    session_id,
                    attempt,
                    max_attempts = CHILD_SESSION_METADATA_MAX_ATTEMPTS,
                    ?delay,
                    "retrying child continuation checkpoint after SQLite contention"
                );
                tokio::select! {
                    () = cancellation.cancelled() => {
                        return Err(
                            "child turn was cancelled while waiting to persist its continuation identity"
                                .to_owned(),
                        );
                    }
                    () = tokio::time::sleep(delay) => {}
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!("the bounded metadata checkpoint loop returns on every terminal attempt")
}

fn child_session_metadata_retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(retry_after) = retry_after.filter(|delay| !delay.is_zero()) {
        return retry_after
            .min(CHILD_SESSION_METADATA_MAX_DELAY)
            .max(Duration::from_millis(1));
    }
    let exponent = attempt.saturating_sub(1).min(31);
    let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
    CHILD_SESSION_METADATA_INITIAL_DELAY
        .saturating_mul(multiplier)
        .min(CHILD_SESSION_METADATA_MAX_DELAY)
}

#[derive(Debug)]
struct ManagedJob {
    internal_id: u64,
    id: String,
    parent_session_id: String,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
}

/// Cross-process ownership for one native child job.
///
/// The lock is acquired before the durable job row is inserted and held for the
/// complete executor future. OS file locks disappear when a process exits, so a
/// peer can distinguish a live executor from a genuinely abandoned job without a
/// timeout that could expire during legitimate long-running work.
#[derive(Debug)]
struct ChildJobLease {
    _file: Option<File>,
}

impl ChildJobLease {
    fn try_acquire(database: &zuno_db::pool::Pool, job_id: &str) -> Result<Option<Self>, String> {
        let Some(path) = child_job_lease_path(database, job_id) else {
            // An in-memory database cannot be shared by another process.
            return Ok(Some(Self { _file: None }));
        };
        let parent = path.parent().ok_or_else(|| {
            format!(
                "child job lease `{}` has no parent directory",
                path.display()
            )
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "create child job lease directory `{}`: {error}",
                parent.display()
            )
        })?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| format!("open child job lease `{}`: {error}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: Some(file) })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(format!(
                "acquire child job lease `{}`: {error}",
                path.display()
            )),
        }
    }
}

struct ForegroundDispatchLease {
    cancellation: CancellationToken,
    armed: bool,
}

impl ForegroundDispatchLease {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ForegroundDispatchLease {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

fn child_job_lease_path(database: &zuno_db::pool::Pool, job_id: &str) -> Option<PathBuf> {
    let database_path = database.path()?;
    let resolved_path =
        fs::canonicalize(database_path).unwrap_or_else(|_| database_path.to_path_buf());
    let database_path = resolved_path.as_path();
    let parent = database_path.parent().unwrap_or_else(|| Path::new("."));
    let database_name = database_path
        .file_name()
        .map_or_else(|| "zuno.db".into(), |name| name.to_string_lossy());
    let digest = zuno_orchestration::sha256_text(job_id);
    Some(
        parent
            .join(format!(".{database_name}.child-job-leases"))
            .join(format!("{digest}.lock")),
    )
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

    fn spawn_unique(
        &self,
        id: impl Into<String>,
        parent_session_id: impl Into<String>,
        cancellation: CancellationToken,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> bool {
        let id = id.into();
        let parent_session_id = parent_session_id.into();
        let internal_id = self.next_task.fetch_add(1, Ordering::Relaxed);
        let changed = self.changed.clone();
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if tasks.iter().any(|job| {
            job.id == id
                && job.parent_session_id == parent_session_id
                && job.task.as_ref().is_none_or(|task| !task.is_finished())
        }) {
            return false;
        }
        tasks.push(ManagedJob {
            internal_id,
            id,
            parent_session_id,
            cancellation,
            task: Some(tokio::spawn(async move {
                task.await;
                changed.changed();
            })),
        });
        drop(tasks);
        self.notify_changed();
        true
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

    /// Request cancellation for work owned by one root or child session.
    pub(crate) fn cancel_for_parent(&self, parent_session_id: &str) {
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for job in tasks.iter().filter(|job| {
            job.parent_session_id == parent_session_id
                && job.task.as_ref().is_none_or(|task| !task.is_finished())
        }) {
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

    fn owns_running_task(&self, parent_session_id: &str, job_id: &str) -> bool {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|job| {
                job.id == job_id
                    && job.parent_session_id == parent_session_id
                    && job.task.as_ref().is_none_or(|task| !task.is_finished())
            })
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

    /// Join only work owned by one session, leaving peer roots untouched.
    pub(crate) async fn wait_for_parent(&self, parent_session_id: &str) {
        let _waiter = self.waiter.lock().await;
        loop {
            let next = {
                let mut tasks = self
                    .tasks
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                tasks.iter_mut().find_map(|job| {
                    (job.parent_session_id == parent_session_id)
                        .then(|| job.task.take().map(|task| (job.internal_id, task)))
                        .flatten()
                })
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
            self.notify_changed();
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
    pub(crate) prompt: String,
    pub(crate) background: bool,
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

/// Surface projection for a turn started by a durable wake after its caller returned.
///
/// Unlike child projection, this path may need ordered async delivery to a root TUI,
/// ACP connection, or durable HTTP event service. Projection failure never owns the
/// turn's durable state, so implementations report locally and return.
#[async_trait]
pub(crate) trait DetachedTurnObserver: Send + Sync + 'static {
    async fn event(&self, session_id: &str, event: &TurnEvent);

    /// Publishes the authoritative durable work projection after every detached
    /// event has drained. Projection remains best-effort and never owns the turn.
    async fn work_state(&self, _session_id: &str, _work: &zuno_types::WorkStateProjection) {}
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
    pub(crate) detached_observer: Option<Arc<dyn DetachedTurnObserver>>,
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
    pub(crate) detached_observer: Option<Arc<dyn DetachedTurnObserver>>,
    pub(crate) supervisor: BackgroundJobSupervisor,
}

#[derive(Debug)]
struct ChildSessionAdmission {
    session_id: String,
    create: Option<zuno_db::session::SessionCreate>,
    evidence_start_rowid: i64,
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
            detached_observer: context.detached_observer.clone(),
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
            child_observer: context.observer,
            detached_observer: context.detached_observer,
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
            self.schedule_parent_report_wake(report);
            recovered = recovered.saturating_add(1);
        }
        Ok(recovered)
    }

    fn schedule_parent_report_wake(&self, report: SessionInput) {
        let wake = Arc::clone(&self.wake);
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let parent_session_id = report.session_id.clone();
        let input_id = report.id.clone();
        let retry_input_id = input_id.clone();
        let retry_id = format!("report-wake:{input_id}");
        let spawned =
            self.supervisor
                .spawn_unique(retry_id, parent_session_id, cancellation, async move {
                    if let Err(error) =
                        wake_parent_report(wake.as_ref(), report, task_cancellation).await
                    {
                        tracing::warn!(
                            input_id = retry_input_id,
                            %error,
                            "durable parent report remains pending after wake retry stopped"
                        );
                    }
                });
        if !spawned {
            tracing::debug!(
                input_id,
                "durable parent report already has a process-local wake retry"
            );
        }
    }

    /// Reconcile process-owned native child jobs without replaying their work.
    pub(crate) fn recover_interrupted(&self, parent_session_id: &str) -> Result<usize, String> {
        let active = self
            .job_store
            .active_child_sessions_for(parent_session_id)
            .map_err(to_string)?;
        let mut recovered = 0_usize;
        for job in active {
            if self
                .supervisor
                .owns_running_task(parent_session_id, &job.id)
            {
                continue;
            }
            let Some(_recovery_lease) =
                ChildJobLease::try_acquire(self.database.as_ref(), &job.id)?
            else {
                tracing::debug!(
                    job_id = %job.id,
                    "another process still owns the native child executor"
                );
                continue;
            };
            let job = self.job_store.get(&job.id).map_err(to_string)?;
            if !matches!(job.status, JobStatus::Queued | JobStatus::Running) {
                continue;
            }
            let completed = zuno_db::message::now_millis();
            let (status, message, settlement) = match job.status {
                JobStatus::Queued => {
                    let message = format!(
                        "Background subagent job `{}` was cancelled because the Zuno process \
                         restarted before execution capacity was admitted; no child turn was run",
                        job.id
                    );
                    let metadata = task_report_metadata_for_job(
                        &self.database,
                        &job,
                        "cancelled",
                        &message,
                        Vec::new(),
                    );
                    let report =
                        report_for_job(&job, "cancelled", &message, metadata.as_ref(), completed);
                    let settlement = JobSettlement::cancelled(message.clone(), completed, report);
                    (
                        "cancelled",
                        message.clone(),
                        metadata.map_or(settlement.clone(), |metadata| {
                            settlement.with_result(
                                serde_json::to_value(metadata)
                                    .expect("task report metadata is serializable"),
                            )
                        }),
                    )
                }
                JobStatus::Running => {
                    let message = format!(
                        "Background subagent job `{}` has an uncertain outcome because the Zuno \
                         process lost its child-turn executor; completed side effects are not replayed",
                        job.id
                    );
                    let metadata = task_report_metadata_for_job(
                        &self.database,
                        &job,
                        "uncertain",
                        &message,
                        vec![
                            "The process lost the child-turn executor before an authoritative \
                             terminal acknowledgement; inspect durable state before retrying."
                                .to_owned(),
                        ],
                    );
                    let report =
                        report_for_job(&job, "uncertain", &message, metadata.as_ref(), completed);
                    let settlement = JobSettlement::uncertain(message.clone(), completed, report);
                    (
                        "uncertain",
                        message.clone(),
                        metadata.map_or(settlement.clone(), |metadata| {
                            settlement.with_result(
                                serde_json::to_value(metadata)
                                    .expect("task report metadata is serializable"),
                            )
                        }),
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
    fn session_admission_for(
        &self,
        request: &ChildTurnRequest,
    ) -> Result<ChildSessionAdmission, ChildTurnError> {
        if let Some(resume) = &request.resume_session_id {
            let connection = self.connect()?;
            let existing = zuno_db::session::get(&connection, resume)
                .map_err(|_error| ChildTurnError::UnknownSession(resume.clone()))?;
            if existing.parent_id.as_deref() != Some(request.parent_session_id.as_str()) {
                return Err(ChildTurnError::UnknownSession(resume.clone()));
            }
            let evidence_start_rowid = zuno_db::message::MessageStore::new(&connection)
                .latest_part_rowid_for_session(&existing.id)
                .map_err(|error| ChildTurnError::Host(zuno_error::source::describe(&error)))?;
            return Ok(ChildSessionAdmission {
                session_id: existing.id,
                create: None,
                evidence_start_rowid,
            });
        }

        let child_id = crate::cmd::turn::prefixed_id("ses");
        let title = request
            .description
            .clone()
            .unwrap_or_else(|| format!("Delegated to {}", request.agent));
        let connection = self.connect()?;
        let parent = zuno_db::session::get(&connection, &request.parent_session_id)
            .map_err(|error| ChildTurnError::Host(zuno_error::source::describe(&error)))?;
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
        if let Some(workspace) = parent.workspace_id {
            input = input.with_workspace(workspace);
        }
        Ok(ChildSessionAdmission {
            session_id: child_id,
            create: Some(input),
            evidence_start_rowid: 0,
        })
    }

    fn admit_child_job(
        &self,
        request: &ChildTurnRequest,
        job_id: String,
        delivery: DbReportDelivery,
        queued: bool,
    ) -> Result<zuno_db::job::AgentJob, ChildTurnError> {
        let admission = self.session_admission_for(request)?;
        let work_context = self.current_job_work_context(&request.parent_session_id)?;
        let mut job = NewAgentJob::new(
            job_id,
            request.parent_session_id.clone(),
            JobSubject::child_session(admission.session_id),
            delivery,
            zuno_db::message::now_millis(),
        )
        .with_logical_key(request.logical_key.clone())
        .with_work_context(work_context)
        .with_orchestration_snapshot(request.parent_attempt.as_deref().cloned())
        .with_evidence_start_rowid(admission.evidence_start_rowid);
        if queued {
            job = job.queued();
        }
        let admitted = match admission.create {
            Some(child) => self
                .job_store
                .create_child_session_if_reconciled(child, job),
            None => self.job_store.create_child_if_reconciled(job),
        }
        .map_err(|error| ChildTurnError::Host(zuno_error::source::describe(&error)))?;
        Ok(admitted)
    }

    fn current_job_work_context(
        &self,
        parent_session_id: &str,
    ) -> Result<Option<JobWorkContext>, ChildTurnError> {
        let plan = zuno_tools::WorkStateStore::new(Arc::clone(&self.database))
            .plan(parent_session_id)
            .map_err(|error| ChildTurnError::Host(error.to_string()))?;
        let Some(plan) = plan else {
            return Ok(None);
        };
        let mut active = plan
            .steps
            .iter()
            .filter(|step| step.status == zuno_tools::PlanStepStatus::InProgress);
        let Some(step) = active.next() else {
            if plan.steps.iter().any(|step| !step.status.is_terminal()) {
                return Err(ChildTurnError::Host(format!(
                    "plan `{}` has unfinished work but no in-progress step during child admission",
                    plan.id
                )));
            }
            return Ok(None);
        };
        if active.next().is_some() {
            return Err(ChildTurnError::Host(format!(
                "plan `{}` has multiple in-progress steps during child admission",
                plan.id
            )));
        }
        Ok(Some(JobWorkContext::new(
            plan.goal_id,
            plan.id,
            plan.revision,
            step.id.clone(),
        )))
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
        let job_id = crate::cmd::turn::prefixed_id("job");
        let execution_lease = ChildJobLease::try_acquire(self.database.as_ref(), &job_id)
            .map_err(ChildTurnError::Host)?
            .ok_or_else(|| {
                ChildTurnError::Host(format!(
                    "new foreground job `{job_id}` unexpectedly already has a live executor"
                ))
            })?;
        let admitted =
            self.admit_child_job(&request, job_id.clone(), DbReportDelivery::Quiet, false)?;
        let JobSubject::ChildSession { session_id } = &admitted.subject else {
            unreachable!("native child admission always stores a child-session subject")
        };
        let session_id = session_id.clone();
        let evidence_start_rowid = admitted.evidence_start_rowid;
        let parent_session_id = request.parent_session_id.clone();
        let runner = Arc::clone(&self.runner);
        let database = Arc::clone(&self.database);
        let job_store = self.job_store.clone();
        let task_cancellation = cancellation.clone();
        let task_job_id = job_id.clone();
        let task_session_id = session_id.clone();
        let (result_sender, result_receiver) = oneshot::channel();

        self.supervisor.spawn(
            job_id,
            parent_session_id,
            cancellation.clone(),
            async move {
                let _permit = _permit;
                let _execution_lease = execution_lease;
                let outcome = run_foreground_child(
                    runner,
                    task_session_id.clone(),
                    request.clone(),
                    task_cancellation,
                )
                .await;
                let result = settle_foreground_child(
                    &database,
                    &job_store,
                    &request,
                    &task_job_id,
                    &task_session_id,
                    evidence_start_rowid,
                    outcome,
                );
                if result_sender.send(result).is_err() {
                    tracing::debug!(
                        job_id = %task_job_id,
                        session_id = %task_session_id,
                        "foreground child settled after its caller detached"
                    );
                }
            },
        );

        let mut caller_lease = ForegroundDispatchLease::new(cancellation);
        let result = result_receiver.await.map_err(|_| {
            ChildTurnError::Host(format!(
                "foreground child supervisor for `{session_id}` stopped before publishing its terminal result"
            ))
        });
        caller_lease.disarm();
        result?
    }
}

enum ForegroundChildOutcome {
    Completed(String),
    Failed(String),
    Cancelled(String),
    Uncertain(String),
}

async fn run_foreground_child(
    runner: Arc<dyn DelegatedTurnRunner>,
    session_id: String,
    request: ChildTurnRequest,
    cancellation: CancellationToken,
) -> ForegroundChildOutcome {
    let runner_cancellation = cancellation.clone();
    let mut runner_task =
        tokio::spawn(async move { runner.run(&session_id, &request, runner_cancellation).await });
    tokio::select! {
        biased;
        joined = &mut runner_task => foreground_child_outcome(joined, cancellation.is_cancelled()),
        () = cancellation.cancelled() => {
            match tokio::time::timeout(
                FOREGROUND_CHILD_CANCEL_SETTLE_TIMEOUT,
                &mut runner_task,
            )
            .await
            {
                Ok(joined) => foreground_child_outcome(joined, true),
                Err(_elapsed) => {
                    runner_task.abort();
                    ForegroundChildOutcome::Uncertain(format!(
                        "child did not acknowledge cancellation within {} seconds; execution was force-aborted and its side effects require inspection",
                        FOREGROUND_CHILD_CANCEL_SETTLE_TIMEOUT.as_secs()
                    ))
                }
            }
        }
    }
}

fn foreground_child_outcome(
    joined: Result<Result<String, String>, tokio::task::JoinError>,
    cancellation_requested: bool,
) -> ForegroundChildOutcome {
    match joined {
        Ok(Ok(output)) if cancellation_requested => ForegroundChildOutcome::Cancelled(output),
        Ok(Ok(output)) => ForegroundChildOutcome::Completed(output),
        Ok(Err(error)) if cancellation_requested => ForegroundChildOutcome::Cancelled(error),
        Ok(Err(error)) => ForegroundChildOutcome::Failed(error),
        Err(error) => ForegroundChildOutcome::Uncertain(format!(
            "child execution ended without an authoritative result: {error}"
        )),
    }
}

fn settle_foreground_child(
    database: &zuno_db::pool::Pool,
    job_store: &AgentJobStore,
    request: &ChildTurnRequest,
    job_id: &str,
    session_id: &str,
    evidence_start_rowid: i64,
    outcome: ForegroundChildOutcome,
) -> Result<ChildTurn, ChildTurnError> {
    let (status, final_text, uncertain_side_effects) = match &outcome {
        ForegroundChildOutcome::Completed(output) => ("completed", output.as_str(), Vec::new()),
        ForegroundChildOutcome::Failed(error) => ("failed", error.as_str(), Vec::new()),
        ForegroundChildOutcome::Cancelled(error) => ("cancelled", error.as_str(), Vec::new()),
        ForegroundChildOutcome::Uncertain(error) => {
            ("uncertain", error.as_str(), vec![error.clone()])
        }
    };
    let work_context = job_store
        .get(job_id)
        .map_err(|error| ChildTurnError::Host(zuno_error::source::describe(&error)))?
        .work_context;
    let report_metadata = serde_json::to_value(task_report_metadata(
        database,
        request,
        TaskReportBuild {
            job_id: Some(job_id),
            work_context,
            child_session_id: session_id,
            evidence_start_rowid,
            status,
            final_text,
            uncertain_side_effects,
        },
    ))
    .map_err(|error| ChildTurnError::Host(error.to_string()))?;
    let completed = zuno_db::message::now_millis();
    let settlement = match &outcome {
        ForegroundChildOutcome::Completed(_) => {
            JobSettlement::completed(report_metadata.clone(), completed, None)
        }
        ForegroundChildOutcome::Failed(error) => {
            JobSettlement::failed(error.clone(), completed, None)
                .with_result(report_metadata.clone())
        }
        ForegroundChildOutcome::Cancelled(error) => {
            JobSettlement::cancelled(error.clone(), completed, None)
                .with_result(report_metadata.clone())
        }
        ForegroundChildOutcome::Uncertain(error) => {
            JobSettlement::uncertain(error.clone(), completed, None)
                .with_result(report_metadata.clone())
        }
    };
    job_store
        .settle(job_id, settlement)
        .map_err(|error| ChildTurnError::Host(zuno_error::source::describe(&error)))?;
    match outcome {
        ForegroundChildOutcome::Completed(output) => Ok(ChildTurn {
            session_id: session_id.to_owned(),
            job_id: None,
            state: ChildTurnState::Completed,
            output,
            report_metadata: Some(report_metadata),
        }),
        ForegroundChildOutcome::Failed(error) => Err(ChildTurnError::Host(error)),
        ForegroundChildOutcome::Cancelled(output) => Ok(ChildTurn {
            session_id: session_id.to_owned(),
            job_id: None,
            state: ChildTurnState::Cancelled,
            output,
            report_metadata: Some(report_metadata),
        }),
        ForegroundChildOutcome::Uncertain(error) => Ok(ChildTurn {
            session_id: session_id.to_owned(),
            job_id: None,
            state: ChildTurnState::Uncertain,
            output: error,
            report_metadata: Some(report_metadata),
        }),
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
    detached_observer: Option<Arc<dyn DetachedTurnObserver>>,
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
        let parent_attempt = request.parent_attempt.as_deref().ok_or_else(|| {
            "delegated child turn is missing the immutable parent Attempt snapshot".to_owned()
        })?;
        let options = TurnOptions {
            directory: Some(self.directory.clone()),
            model: request.model.as_ref().map(|model| model.model.clone()),
            agent: Some(request.agent.clone()),
            preset: None,
            session: SessionChoice::Existing(session_id.to_owned()),
            title: request.description.clone(),
            effort: request.effort,
            variant: None,
            thinking: false,
            tool_authority: Some(Arc::from(parent_attempt.tools.clone())),
            extension_composition: super::turn::ExtensionComposition::Active,
        };
        let mut plan = TurnPlan::resolve(&options, &self.environment).await?;
        plan.inherit_request_parameters(request.provider_options.clone());
        let spec = ChildSessionSpec::resolved(
            request,
            plan.agent_name(),
            &plan.qualified_model(),
            plan.effort(),
        );
        let resumed = request.resume_session_id.is_some();
        if resumed {
            checkpoint_child_session_spec(
                &self.database,
                &self.children,
                session_id,
                &spec,
                true,
                &cancellation,
            )
            .await?;
        }
        plan.inherit_orchestration(
            parent_attempt,
            spec.workflow.as_deref(),
            spec.workflow_node.as_deref(),
        )?;
        if !resumed {
            checkpoint_child_session_spec(
                &self.database,
                &self.children,
                session_id,
                &spec,
                false,
                &cancellation,
            )
            .await?;
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
                detached_observer: self.detached_observer.clone(),
            },
        )
        .await?;
        host.activate_extension_composition()?;
        host.activate_background_notifications(&tokio::runtime::Handle::current());
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
                prompt: request.prompt.clone(),
                background: request.background,
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
                PlanningInputSource::User,
                session_id,
                self.observer.clone(),
            );
            tokio::pin!(drive);
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    let _aborted = control.abort(zuno_engine::interrupt::HardInterruptRequest::new(
                        zuno_engine::interrupt::HardInterruptSource::Lifecycle,
                        zuno_engine::interrupt::HardInterruptReason::SessionClose,
                    ));
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
            detached_observer: context.detached_observer.clone(),
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
        delivery: InputDelivery,
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
                delivery,
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
                        let _aborted = control.abort(
                            zuno_engine::interrupt::HardInterruptRequest::new(
                                zuno_engine::interrupt::HardInterruptSource::Lifecycle,
                                zuno_engine::interrupt::HardInterruptReason::Shutdown,
                            ),
                        );
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
    detached_observer: Option<Arc<dyn DetachedTurnObserver>>,
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
        let spec = self
            .children
            .get_or_restore(&self.database, &input.session_id)?;
        let parent_attempt = spec.parent_attempt.as_ref().ok_or_else(|| {
            "interactive child session is missing the immutable parent Attempt snapshot".to_owned()
        })?;
        let options = TurnOptions {
            directory: Some(self.directory.clone()),
            model: Some(spec.model.clone()),
            agent: Some(spec.agent.clone()),
            preset: None,
            session: SessionChoice::Existing(input.session_id.clone()),
            title: spec.description.clone(),
            effort: spec.effort,
            variant: None,
            thinking: false,
            tool_authority: Some(Arc::from(parent_attempt.tools.clone())),
            extension_composition: super::turn::ExtensionComposition::Active,
        };
        let mut plan = TurnPlan::resolve(&options, &self.environment).await?;
        plan.inherit_request_parameters(spec.provider_options.clone());
        plan.inherit_orchestration(
            parent_attempt,
            spec.workflow.as_deref(),
            spec.workflow_node.as_deref(),
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
                detached_observer: self.detached_observer.clone(),
            },
        )
        .await?;
        host.activate_extension_composition()?;
        host.activate_background_notifications(&tokio::runtime::Handle::current());
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
                parent_session_id: spec.parent_session_id.clone(),
                title: host
                    .session_title()
                    .map(str::to_owned)
                    .or_else(|| spec.description.clone())
                    .unwrap_or_else(|| input.session_id.clone()),
                agent: host.agent_name().to_owned(),
                model: host.qualified_model(),
                effort: host.effort_override(),
                prompt: text.clone(),
                background: spec.background,
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
            PlanningInputSource::User,
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
    child_observer: Option<Arc<dyn ChildTurnObserver>>,
    detached_observer: Option<Arc<dyn DetachedTurnObserver>>,
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
            variant: None,
            thinking: false,
            tool_authority: None,
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
                child_observer: self.child_observer.clone(),
                detached_observer: self.detached_observer.clone(),
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
        let planning_source = detached_planning_source(&input.prompt);
        let outcome = drive_detached_and_drain(
            &mut host,
            &text,
            Some(input.id.as_str()),
            Some(guard),
            planning_source,
            input.session_id.as_str(),
            self.detached_observer.clone(),
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

pub(super) async fn wake_parent_report(
    wake: &dyn ParentReportWake,
    report: SessionInput,
    cancellation: CancellationToken,
) -> Result<(), String> {
    let mut delay = PARENT_WAKE_INITIAL_DELAY;
    for attempt in 1..=PARENT_WAKE_ATTEMPTS {
        let error = match wake.wake(report.clone()).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if attempt == PARENT_WAKE_ATTEMPTS {
            return Err(format!(
                "parent report wake stopped after {attempt} attempt(s): {error}"
            ));
        }
        tracing::warn!(
            input_id = %report.id,
            attempt,
            ?delay,
            %error,
            "retrying durable parent report wake"
        );
        tokio::select! {
            () = cancellation.cancelled() => {
                return Err(format!(
                    "parent report wake stopped after {attempt} attempt(s): {error}"
                ));
            }
            () = tokio::time::sleep(delay) => {}
        }
        delay = delay.saturating_mul(2).min(PARENT_WAKE_MAX_DELAY);
    }
    unreachable!("the bounded parent wake loop always returns")
}

async fn drive_and_drain(
    host: &mut TurnHost,
    prompt: &str,
    message_id: Option<&str>,
    guard: Option<SessionRunGuard>,
    planning_source: PlanningInputSource,
    session_id: &str,
    observer: Option<Arc<dyn ChildTurnObserver>>,
) -> Result<(), String> {
    let (sender, receiver) = event_channel();
    let drive = async {
        let outcome = match (guard, message_id, planning_source) {
            (Some(guard), Some(message_id), _) => {
                host.drive_promoted_with_guard(prompt, message_id, guard, sender.clone())
                    .await
            }
            (Some(guard), None, _) => {
                host.drive_with_message_id_and_guard(prompt, None, guard, sender.clone())
                    .await
            }
            (None, Some(message_id), _) => {
                host.drive_promoted(prompt, message_id, sender.clone())
                    .await
            }
            (None, None, _) => {
                host.drive_with_message_id(prompt, None, sender.clone())
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

async fn drive_detached_and_drain(
    host: &mut TurnHost,
    prompt: &str,
    message_id: Option<&str>,
    guard: Option<SessionRunGuard>,
    planning_source: PlanningInputSource,
    session_id: &str,
    observer: Option<Arc<dyn DetachedTurnObserver>>,
) -> Result<(), String> {
    let (sender, receiver) = event_channel();
    let drive = async {
        let outcome = match (guard, message_id) {
            (Some(guard), Some(message_id)) => {
                host.drive_promoted_report_with_guard(
                    prompt,
                    message_id,
                    planning_source,
                    guard,
                    sender.clone(),
                )
                .await
            }
            (None, Some(message_id)) => {
                host.drive_promoted_report(prompt, message_id, planning_source, sender.clone())
                    .await
            }
            (Some(guard), None) => {
                host.drive_with_message_id_and_guard(prompt, None, guard, sender.clone())
                    .await
            }
            (None, None) => {
                host.drive_with_message_id(prompt, None, sender.clone())
                    .await
            }
        };
        if outcome.is_ok() {
            while host
                .continue_goal_if_idle(zuno_goal::QueuedUserInput::Absent, sender.clone())
                .await?
            {}
        }
        drop(sender);
        outcome
    };
    let drain = forward_detached_events(session_id.to_owned(), receiver, observer.clone());
    let (outcome, ()) = tokio::join!(drive, drain);
    if let Some(observer) = observer {
        match host.work_state() {
            Ok(work) => observer.work_state(session_id, &work).await,
            Err(error) => {
                tracing::debug!(
                    session_id,
                    %error,
                    "failed to read final detached turn work state for projection"
                );
            }
        }
    }
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

async fn forward_detached_events(
    session_id: String,
    mut receiver: mpsc::Receiver<TurnEvent>,
    observer: Option<Arc<dyn DetachedTurnObserver>>,
) {
    while let Some(event) = receiver.recv().await {
        if let Some(observer) = observer.as_ref() {
            observer.event(&session_id, &event).await;
        }
    }
}

fn detached_planning_source(prompt: &Value) -> PlanningInputSource {
    if prompt.get("kind").and_then(Value::as_str) == Some("backgroundExecutionReport") {
        PlanningInputSource::BackgroundReport
    } else {
        PlanningInputSource::ChildReport
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

fn task_report_metadata(
    database: &zuno_db::pool::Pool,
    request: &ChildTurnRequest,
    build: TaskReportBuild<'_>,
) -> TaskReportMetadata {
    let TaskReportBuild {
        job_id,
        work_context,
        child_session_id,
        evidence_start_rowid,
        status,
        final_text,
        mut uncertain_side_effects,
    } = build;
    let mut evidence_errors = Vec::new();
    let mut usage = TaskReportUsage::default();
    let mut changed_paths = Vec::new();
    let mut verification_records = Vec::new();

    match database.open_connection() {
        Ok(connection) => {
            match zuno_db::session::get(&connection, child_session_id) {
                Ok(session) => usage = session.usage.snapshot().into(),
                Err(error) => evidence_errors.push(format!("usage: {error}")),
            }
            let store = zuno_db::message::MessageStore::new(&connection);
            match store.parts_for_session_by_kind_after_rowid(
                child_session_id,
                zuno_db::message::PartKind::Tool,
                evidence_start_rowid,
            ) {
                Ok(parts) => {
                    let mut paths = BTreeSet::new();
                    for part in parts {
                        let metadata = part
                            .data
                            .get("state")
                            .and_then(Value::as_object)
                            .and_then(|state| state.get("metadata"))
                            .and_then(Value::as_object);
                        let Some(metadata) = metadata else {
                            continue;
                        };
                        paths.extend(
                            metadata
                                .get(zuno_tool::METADATA_WRITTEN_PATHS_KEY)
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .filter(|path| !path.is_empty())
                                .map(str::to_owned),
                        );
                        collect_verification_records(
                            metadata.get(TASK_VERIFICATION_METADATA_KEY),
                            &mut verification_records,
                            &mut evidence_errors,
                        );
                        uncertain_side_effects.extend(
                            metadata
                                .get(UNCERTAIN_SIDE_EFFECTS_METADATA_KEY)
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .filter(|detail| !detail.is_empty())
                                .map(str::to_owned),
                        );
                    }
                    changed_paths = paths.into_iter().collect();
                }
                Err(error) => evidence_errors.push(format!("tool evidence: {error}")),
            }
        }
        Err(error) => evidence_errors.push(format!("session evidence: {error}")),
    }
    uncertain_side_effects.sort();
    uncertain_side_effects.dedup();

    TaskReportMetadata {
        schema_version: TASK_REPORT_METADATA_SCHEMA_VERSION,
        job_id: job_id.map(str::to_owned),
        work_context,
        session_id: child_session_id.to_owned(),
        parent_session_id: request.parent_session_id.clone(),
        agent: request.agent.clone(),
        status: status.to_owned(),
        final_text: final_text.to_owned(),
        usage,
        changed_paths,
        verification_records,
        uncertain_side_effects,
        evidence_errors,
    }
}

fn task_report_metadata_for_job(
    database: &zuno_db::pool::Pool,
    job: &AgentJob,
    status: &str,
    final_text: &str,
    uncertain_side_effects: Vec<String>,
) -> Option<TaskReportMetadata> {
    let JobSubject::ChildSession { session_id } = &job.subject else {
        return None;
    };
    let agent = database
        .open_connection()
        .ok()
        .and_then(|connection| zuno_db::session::get(&connection, session_id).ok())
        .and_then(|session| session.agent)
        .unwrap_or_else(|| "subagent".to_owned());
    let request = ChildTurnRequest {
        parent_session_id: job.parent_session_id.clone(),
        parent_attempt: job.orchestration_snapshot.clone().map(Arc::new),
        workflow: None,
        workflow_node: None,
        resume_session_id: Some(session_id.clone()),
        logical_key: job.logical_key.clone(),
        agent,
        description: None,
        prompt: String::new(),
        model: None,
        effort: None,
        provider_options: Map::new(),
        background: true,
        report_delivery: match job.report_delivery {
            DbReportDelivery::NextStep => ToolReportDelivery::NextStep,
            DbReportDelivery::Quiet => ToolReportDelivery::Quiet,
        },
    };
    Some(task_report_metadata(
        database,
        &request,
        TaskReportBuild {
            job_id: Some(&job.id),
            work_context: job.work_context.clone(),
            child_session_id: session_id,
            evidence_start_rowid: job.evidence_start_rowid,
            status,
            final_text,
            uncertain_side_effects,
        },
    ))
}

fn collect_verification_records(
    value: Option<&Value>,
    records: &mut Vec<TaskVerificationRecord>,
    errors: &mut Vec<String>,
) {
    let Some(value) = value else {
        return;
    };
    let values = value
        .as_array()
        .map_or_else(|| vec![value.clone()], |values| values.clone());
    for value in values {
        match serde_json::from_value(value) {
            Ok(record) => records.push(record),
            Err(error) => errors.push(format!("verification record: {error}")),
        }
    }
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
                    dispatch.await
                }
                result = &mut dispatch => result,
            };
        }
        if interrupt.is_set() {
            return Err(ChildTurnError::Host(
                "background child was cancelled before admission".to_owned(),
            ));
        }
        let job_id = crate::cmd::turn::prefixed_id("job");
        let execution_lease = ChildJobLease::try_acquire(self.database.as_ref(), &job_id)
            .map_err(ChildTurnError::Host)?
            .ok_or_else(|| {
                ChildTurnError::Host(format!(
                    "new background job `{job_id}` unexpectedly already has a live executor"
                ))
            })?;
        let delivery = match request.report_delivery {
            ToolReportDelivery::NextStep => DbReportDelivery::NextStep,
            ToolReportDelivery::Quiet => DbReportDelivery::Quiet,
        };
        let admitted = self.admit_child_job(&request, job_id.clone(), delivery, true)?;
        let JobSubject::ChildSession { session_id } = &admitted.subject else {
            unreachable!("native child admission always stores a child-session subject")
        };
        let session_id = session_id.clone();
        let evidence_start_rowid = admitted.evidence_start_rowid;
        let work_context = admitted.work_context.clone();

        let runner = Arc::clone(&self.runner);
        let wake = Arc::clone(&self.wake);
        let delegation_limiter = self.delegation_limiter.clone();
        let job_store = self.job_store.clone();
        let database = Arc::clone(&self.database);
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
                let _execution_lease = execution_lease;
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
                    let metadata = task_report_metadata(
                        &database,
                        &request,
                        TaskReportBuild {
                            job_id: Some(&background_job_id),
                            work_context: work_context.clone(),
                            child_session_id: &background_session_id,
                            evidence_start_rowid,
                            status: "cancelled",
                            final_text: &text,
                            uncertain_side_effects: Vec::new(),
                        },
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
                                &metadata,
                                completed,
                            ),
                        )
                        .with_result(
                            serde_json::to_value(metadata)
                                .expect("task report metadata is serializable"),
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
                            let metadata = task_report_metadata(
                                &database,
                                &request,
                                TaskReportBuild {
                                    job_id: Some(&background_job_id),
                                    work_context: work_context.clone(),
                                    child_session_id: &background_session_id,
                                    evidence_start_rowid,
                                    status: "completed",
                                    final_text: &output,
                                    uncertain_side_effects: Vec::new(),
                                },
                            );
                            (
                                JobSettlement::completed(
                                    serde_json::to_value(&metadata)
                                        .expect("task report metadata is serializable"),
                                    completed,
                                    report_input(
                                        &request,
                                        &background_job_id,
                                        &background_session_id,
                                        "completed",
                                        &text,
                                        &metadata,
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
                            let metadata = task_report_metadata(
                                &database,
                                &request,
                                TaskReportBuild {
                                    job_id: Some(&background_job_id),
                                    work_context: work_context.clone(),
                                    child_session_id: &background_session_id,
                                    evidence_start_rowid,
                                    status: "failed",
                                    final_text: &error,
                                    uncertain_side_effects: Vec::new(),
                                },
                            );
                            (
                                JobSettlement::failed(
                                    error.clone(),
                                    completed,
                                    report_input(
                                        &request,
                                        &background_job_id,
                                        &background_session_id,
                                        "failed",
                                        &text,
                                        &metadata,
                                        completed,
                                    ),
                                )
                                .with_result(
                                    serde_json::to_value(metadata)
                                        .expect("task report metadata is serializable"),
                                ),
                                text,
                            )
                        }
                    }
                };
                match job_store.settle(&background_job_id, settlement) {
                    Ok(settled) => {
                        if let Some(report) = settled.report
                            && let Err(error) =
                                wake_parent_report(wake.as_ref(), report, task_cancellation.clone())
                                    .await
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
            state: ChildTurnState::Running,
            output: "Background subagent started. Its terminal state will be delivered according \
                     to `reportDelivery`."
                .to_owned(),
            report_metadata: None,
        })
    }
}

fn report_input(
    request: &ChildTurnRequest,
    job_id: &str,
    child_session_id: &str,
    status: &str,
    text: &str,
    metadata: &TaskReportMetadata,
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
                "metadata": metadata,
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
    metadata: Option<&TaskReportMetadata>,
    created: i64,
) -> Option<NewSessionInput> {
    if job.report_delivery != DbReportDelivery::NextStep {
        return None;
    }
    let JobSubject::ChildSession { session_id } = &job.subject else {
        return None;
    };
    let mut prompt = json!({
        "kind": "subagentReport",
        "jobID": job.id,
        "childSessionID": session_id,
        "status": status,
        "text": text,
    });
    if let Some(metadata) = metadata {
        prompt["metadata"] =
            serde_json::to_value(metadata).expect("task report metadata is serializable");
    }
    Some(NewSessionInput::new(
        crate::cmd::turn::prefixed_id("input"),
        job.parent_session_id.clone(),
        prompt,
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
