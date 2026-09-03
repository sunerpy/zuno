//! Process-owned command execution with bounded live output and durable background retention.
//!
//! A command enters this service before it is spawned. Foreground callers may
//! wait for it, detach after an attention deadline, or cancel it, but they never
//! transfer an already-running [`tokio::task::JoinHandle`] between owners. That
//! single-owner shape is what makes cancellation and at-most-once execution hold
//! across explicit background mode and foreground timeout promotion.
//!
//! Foreground commands are ephemeral and disappear after their caller consumes
//! the terminal output. Explicit or timeout-promoted background commands persist
//! status/output for restart reconciliation, with terminal history bounded by
//! [`MAX_RETAINED_TERMINAL_EXECUTIONS`].

use crate::{BUFFER_LIMIT, ReplayCursor, ScrollbackBuffer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, watch};
use uuid::Uuid;
use zuno_sandbox::{ExecutionAuthority, PreparedCommand};

const STATE_FORMAT: u32 = 3;
const OUTPUT_SUFFIX: &str = ".output";
const STATUS_SUFFIX: &str = ".status.json";
const OUTPUT_CHUNK: usize = 8 * 1024;

/// Completed background executions retained per workspace.
///
/// Running commands are never evicted. The terminal cap bounds the durable
/// store at two files per retained execution while preserving a useful recent
/// history for `/ps` and `bg`.
pub const MAX_RETAINED_TERMINAL_EXECUTIONS: usize = 32;

/// Lifecycle events shared by TUI, server, and future clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundExecutionEvent {
    /// A command was spawned and is now observable.
    Created(BackgroundExecutionInfo),
    /// A running command reached a terminal state.
    Settled(BackgroundExecutionInfo),
}

/// Stable identifier returned to model tools and client surfaces.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackgroundExecutionId(String);

impl BackgroundExecutionId {
    fn mint() -> Self {
        Self(format!("bg_{}", Uuid::new_v4().simple()))
    }

    /// Builds an identifier at a wire or test boundary.
    ///
    /// # Errors
    ///
    /// Rejects values that could escape the service's state directory.
    pub fn parse(value: impl Into<String>) -> Result<Self, BackgroundExecutionError> {
        let value = value.into();
        if value.strip_prefix("bg_").is_some_and(|tail| {
            tail.len() == 32 && tail.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            Ok(Self(value))
        } else {
            Err(BackgroundExecutionError::InvalidId(value))
        }
    }

    /// Wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BackgroundExecutionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Authoritative lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundExecutionStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    /// The prior process disappeared without an authoritative terminal result.
    Uncertain,
}

impl BackgroundExecutionStatus {
    /// Whether no more output or state transitions are expected.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    /// Stable client-facing spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Uncertain => "uncertain",
        }
    }
}

/// What a background process can prove when it reaches terminal state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BackgroundExecutionPurpose {
    /// The process outcome is the requested local command outcome.
    #[default]
    Command,
    /// The process only observes work owned by a remote system.
    RemoteObserver,
}

impl BackgroundExecutionPurpose {
    /// Stable client- and model-facing spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::RemoteObserver => "remoteObserver",
        }
    }

    /// Whether the resumed Agent must refresh another system before claiming completion.
    #[must_use]
    pub const fn requires_authoritative_refresh(self) -> bool {
        matches!(self, Self::RemoteObserver)
    }
}

/// One command's durable metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundExecutionInfo {
    pub id: BackgroundExecutionId,
    pub session_id: String,
    pub title: String,
    pub command: String,
    /// The completion authority selected at the tool boundary.
    #[serde(default)]
    pub purpose: BackgroundExecutionPurpose,
    pub cwd: PathBuf,
    pub status: BackgroundExecutionStatus,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub time_created: i64,
    pub time_updated: i64,
    pub time_completed: Option<i64>,
    pub error: Option<String>,
    pub output_file: PathBuf,
    pub status_file: PathBuf,
    /// Exact OS-sandbox authority compiled before this process was spawned.
    pub authority: ExecutionAuthority,
}

/// Frontend-neutral snapshot consumed by TUI, server, and future clients.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundExecutionProjection {
    pub executions: Vec<BackgroundExecutionInfo>,
}

/// Whether one execution is an implementation detail of a foreground call or a
/// user-visible background job that must survive session/TUI rebinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundExecutionRetention {
    /// Keep process ownership in memory and remove its output after the caller
    /// consumes the terminal result.
    Ephemeral,
    /// Persist status and output so `/ps`, `bg`, and restart reconciliation can
    /// continue observing the command.
    Durable,
}

impl BackgroundExecutionRetention {
    const fn is_durable(self) -> bool {
        matches!(self, Self::Durable)
    }
}

/// Command launch input. Environment values are never persisted.
#[derive(Debug)]
pub struct BackgroundExecutionInput {
    /// Opaque launch produced by a sandbox backend.
    pub prepared: PreparedCommand,
    pub session_id: String,
    pub title: String,
    pub command: String,
    pub purpose: BackgroundExecutionPurpose,
    pub hard_ceiling: Duration,
    pub retention: BackgroundExecutionRetention,
}

/// Bounded output replay for a running or retained command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundExecutionOutput {
    pub bytes: Vec<u8>,
    /// Absolute cursor just past [`Self::bytes`]: where the next window starts.
    pub cursor: u64,
    pub retained_from: u64,
    pub total_written: u64,
    pub discarded: u64,
    pub output_file: PathBuf,
    /// Whether these bytes came from the persisted file rather than the retained ring,
    /// because the requested cursor predated [`Self::retained_from`].
    pub from_disk: bool,
}

/// Result of waiting for a terminal state or one caller attention deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundWaitOutcome {
    pub info: BackgroundExecutionInfo,
    pub timed_out: bool,
}

/// Failures at the process/service boundary.
#[derive(Debug, thiserror::Error)]
pub enum BackgroundExecutionError {
    #[error("invalid background execution id `{0}`")]
    InvalidId(String),
    #[error("background execution `{0}` does not exist")]
    NotFound(BackgroundExecutionId),
    #[error("foreground execution `{0}` is still running")]
    ForegroundStillRunning(BackgroundExecutionId),
    #[error("background execution `{0}` is durable and cannot be consumed as foreground output")]
    DurableForeground(BackgroundExecutionId),
    #[error("could not create background execution state at `{path}`")]
    State {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not decode background execution state at `{path}`")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not spawn background command `{command}`")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
}

/// Cancellation lease held by a foreground Shell future.
///
/// Dropping an armed lease requests cancellation synchronously. This closes the
/// race where the dispatcher aborts the tool future after spawn but before the
/// Shell tool enters its own cancellation `select!`.
#[derive(Debug)]
pub struct BackgroundExecutionLease {
    service: BackgroundExecutionService,
    id: BackgroundExecutionId,
    armed: bool,
}

impl BackgroundExecutionLease {
    /// Stops drop-driven cancellation after ownership was deliberately transferred
    /// to durable background execution or after the command settled.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BackgroundExecutionLease {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.service.cancel(&self.id);
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedExecution {
    format: u32,
    info: BackgroundExecutionInfo,
}

#[derive(Debug)]
struct ExecutionState {
    info: watch::Sender<BackgroundExecutionInfo>,
    output: Mutex<ScrollbackBuffer>,
    cancel: watch::Sender<bool>,
    durable: AtomicBool,
    transition: Mutex<()>,
}

impl ExecutionState {
    fn info(&self) -> BackgroundExecutionInfo {
        self.info.borrow().clone()
    }

    fn output(&self) -> MutexGuard<'_, ScrollbackBuffer> {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn is_durable(&self) -> bool {
        self.durable.load(Ordering::Acquire)
    }

    fn transition(&self) -> MutexGuard<'_, ()> {
        self.transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug)]
struct ServiceInner {
    root: PathBuf,
    executions: Mutex<HashMap<BackgroundExecutionId, Arc<ExecutionState>>>,
    events: broadcast::Sender<BackgroundExecutionEvent>,
}

/// Shared owner of every background command in one workspace and process.
#[derive(Debug, Clone)]
pub struct BackgroundExecutionService {
    inner: Arc<ServiceInner>,
}

impl BackgroundExecutionService {
    /// Opens the workspace's execution store and reconciles interrupted commands.
    ///
    /// A persisted `running` row cannot prove whether its old process completed,
    /// failed, or performed a side effect before the Zuno process disappeared. It
    /// is therefore changed to `uncertain` and is never replayed.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, BackgroundExecutionError> {
        let root = root.into();
        let (events, _) = broadcast::channel(256);
        let service = Self {
            inner: Arc::new(ServiceInner {
                root: root.clone(),
                executions: Mutex::new(HashMap::new()),
                events,
            }),
        };
        if !root.exists() {
            return Ok(service);
        }

        let entries = std::fs::read_dir(&root).map_err(|source| state_error(&root, source))?;
        let mut status_files = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(STATUS_SUFFIX))
            })
            .collect::<Vec<_>>();
        status_files.sort();

        for status_file in status_files {
            let bytes =
                std::fs::read(&status_file).map_err(|source| state_error(&status_file, source))?;
            let mut persisted: PersistedExecution =
                serde_json::from_slice(&bytes).map_err(|source| {
                    BackgroundExecutionError::Decode {
                        path: status_file.clone(),
                        source,
                    }
                })?;
            if persisted.format != STATE_FORMAT {
                return Err(BackgroundExecutionError::State {
                    path: status_file,
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "unsupported background state format {}; expected {STATE_FORMAT}",
                            persisted.format
                        ),
                    ),
                });
            }
            let id = BackgroundExecutionId::parse(persisted.info.id.0)?;
            let output_file = service.output_path(&id);
            let canonical_status_file = service.status_path(&id);
            persisted.info.id = id.clone();
            persisted.info.output_file = output_file.clone();
            persisted.info.status_file = canonical_status_file;
            if persisted.info.status == BackgroundExecutionStatus::Running {
                let now = now_millis();
                persisted.info.status = BackgroundExecutionStatus::Uncertain;
                persisted.info.pid = None;
                persisted.info.time_updated = now;
                persisted.info.time_completed = Some(now);
                persisted.info.error = Some(
                    "the previous Zuno process exited before recording an authoritative outcome; \
                     this command was not replayed"
                        .to_owned(),
                );
                persist_info(&persisted.info)?;
            }

            let total_written = std::fs::metadata(&output_file)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let tail = read_window(
                &output_file,
                total_written.saturating_sub(BUFFER_LIMIT as u64),
                None,
            )
            .map_err(|source| state_error(&output_file, source))?;
            let (info, _) = watch::channel(persisted.info);
            let (cancel, _) = watch::channel(false);
            service.executions().insert(
                id,
                Arc::new(ExecutionState {
                    info,
                    output: Mutex::new(ScrollbackBuffer::restore_tail(
                        BUFFER_LIMIT,
                        total_written,
                        &tail,
                    )),
                    cancel,
                    durable: AtomicBool::new(true),
                    transition: Mutex::new(()),
                }),
            );
        }
        service.prune_retained();
        Ok(service)
    }

    /// Subscribe to lifecycle changes. A lagging client should call [`Self::list`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<BackgroundExecutionEvent> {
        self.inner.events.subscribe()
    }

    /// Starts one command under process-tree containment.
    ///
    /// The returned row is already registered and persisted. A caller may begin a
    /// foreground wait or return it to the user immediately without an adoption
    /// race.
    pub fn start(
        &self,
        input: BackgroundExecutionInput,
    ) -> Result<BackgroundExecutionInfo, BackgroundExecutionError> {
        std::fs::create_dir_all(&self.inner.root)
            .map_err(|source| state_error(&self.inner.root, source))?;
        let id = BackgroundExecutionId::mint();
        let output_file = self.output_path(&id);
        let status_file = self.status_path(&id);
        std::fs::File::create(&output_file).map_err(|source| state_error(&output_file, source))?;

        let prepared = input.prepared.into_parts();
        let (program, arguments) =
            zuno_process::guarded_argv(&prepared.program, prepared.arguments.iter());
        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(&prepared.cwd)
            .env_clear()
            .envs(&prepared.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|source| {
            let _removed = remove_if_exists(&output_file);
            BackgroundExecutionError::Spawn {
                command: input.command.clone(),
                source,
            }
        })?;
        let pid = child.id();
        let now = now_millis();
        let info = BackgroundExecutionInfo {
            id: id.clone(),
            session_id: input.session_id,
            title: input.title,
            command: input.command,
            purpose: input.purpose,
            cwd: prepared.cwd,
            status: BackgroundExecutionStatus::Running,
            pid,
            exit_code: None,
            timed_out: false,
            time_created: now,
            time_updated: now,
            time_completed: None,
            error: None,
            output_file,
            status_file,
            authority: prepared.authority,
        };
        let durable = input.retention.is_durable();
        if durable && let Err(error) = persist_info(&info) {
            if let Some(pid) = pid {
                let _terminated = zuno_process::request_contained_process_shutdown(pid);
            }
            let _removed = remove_execution_files(&info);
            let _reaper = tokio::spawn(async move {
                let _status = child.wait().await;
            });
            return Err(error);
        }

        let (info_sender, _) = watch::channel(info.clone());
        let (cancel_sender, cancel_receiver) = watch::channel(false);
        let state = Arc::new(ExecutionState {
            info: info_sender,
            output: Mutex::new(ScrollbackBuffer::new()),
            cancel: cancel_sender,
            durable: AtomicBool::new(durable),
            transition: Mutex::new(()),
        });
        self.executions().insert(id, Arc::clone(&state));
        if durable {
            let _delivered = self
                .inner
                .events
                .send(BackgroundExecutionEvent::Created(info.clone()));
        }

        tokio::spawn(run_execution(
            child,
            state,
            cancel_receiver,
            input.hard_ceiling,
            self.clone(),
        ));
        if durable {
            self.prune_retained();
        }
        Ok(info)
    }

    /// Starts one foreground-owned command and returns a drop cancellation lease.
    pub fn start_leased(
        &self,
        input: BackgroundExecutionInput,
    ) -> Result<(BackgroundExecutionInfo, BackgroundExecutionLease), BackgroundExecutionError> {
        let info = self.start(input)?;
        let lease = BackgroundExecutionLease {
            service: self.clone(),
            id: info.id.clone(),
            armed: true,
        };
        Ok((info, lease))
    }

    /// Every durable command in creation order.
    #[must_use]
    pub fn list(&self) -> Vec<BackgroundExecutionInfo> {
        let mut values = self
            .executions()
            .values()
            .filter(|state| state.is_durable())
            .map(|state| state.info())
            .collect::<Vec<_>>();
        values.sort_by(|left, right| {
            left.time_created
                .cmp(&right.time_created)
                .then_with(|| left.id.cmp(&right.id))
        });
        values
    }

    /// Current client projection in deterministic creation order.
    #[must_use]
    pub fn projection(&self) -> BackgroundExecutionProjection {
        BackgroundExecutionProjection {
            executions: self.list(),
        }
    }

    /// Commands owned by one durable or prepared session.
    #[must_use]
    pub fn list_for_session(&self, session_id: &str) -> Vec<BackgroundExecutionInfo> {
        self.list()
            .into_iter()
            .filter(|info| info.session_id == session_id)
            .collect()
    }

    /// One command's current metadata.
    pub fn get(
        &self,
        id: &BackgroundExecutionId,
    ) -> Result<BackgroundExecutionInfo, BackgroundExecutionError> {
        self.state(id).map(|state| state.info())
    }

    /// Makes a live foreground execution durable after its attention deadline.
    ///
    /// The status snapshot is written before the execution becomes observable,
    /// so a failed promotion cannot invite the caller to rerun a command whose
    /// durable ownership is ambiguous.
    pub fn promote(
        &self,
        id: &BackgroundExecutionId,
    ) -> Result<BackgroundExecutionInfo, BackgroundExecutionError> {
        let state = self.state(id)?;
        let info = {
            let _transition = state.transition();
            if state.is_durable() {
                return Ok(state.info());
            }
            let info = state.info();
            persist_info(&info)?;
            state.durable.store(true, Ordering::Release);
            let _created = self
                .inner
                .events
                .send(BackgroundExecutionEvent::Created(info.clone()));
            if info.status.is_terminal() {
                let _settled = self
                    .inner
                    .events
                    .send(BackgroundExecutionEvent::Settled(info.clone()));
            }
            info
        };
        self.prune_retained();
        Ok(info)
    }

    /// Replays one bounded window of output from an absolute cursor.
    ///
    /// `limit` caps the bytes returned; `None` returns everything from the cursor to the
    /// current end, which is what a client rendering a tail wants and what a
    /// model-facing caller must not ask for. The returned
    /// [`BackgroundExecutionOutput::cursor`] is where the next window begins, so a
    /// caller pages by handing it back.
    ///
    /// [`ReplayCursor::From`] naming a cursor older than what the ring still retains is
    /// served from the persisted output file instead of being clamped forward. The ring
    /// is bounded at [`crate::BUFFER_LIMIT`] and the file is not, so clamping left the
    /// discarded prefix permanently unreachable through this service while it sat
    /// complete on disk, and the only way to see it was a shell command slicing the file
    /// by hand. [`ReplayCursor::Full`] still means "everything still retained" and never
    /// reaches for the file: it is the tail request every client surface makes, and a
    /// caller that wants the beginning says `From(0)`.
    ///
    /// A file that is gone — an ephemeral foreground command cleans up after itself —
    /// replays as no bytes rather than as a failure, the same tolerance restoring a
    /// retained tail applies.
    pub fn output(
        &self,
        id: &BackgroundExecutionId,
        cursor: ReplayCursor,
        limit: Option<usize>,
    ) -> Result<BackgroundExecutionOutput, BackgroundExecutionError> {
        let state = self.state(id)?;
        let info = state.info();
        let output = state.output();
        let retained_from = output.start_cursor();
        let total_written = output.total_written();
        let discarded = output.discarded();
        let (bytes, next, from_disk) = match cursor {
            ReplayCursor::From(requested) if requested < retained_from => {
                drop(output);
                let bytes = read_window(&info.output_file, requested, limit)
                    .map_err(|source| state_error(&info.output_file, source))?;
                let next = requested.saturating_add(bytes.len() as u64);
                (bytes, next, true)
            }
            cursor => {
                let replay = output.replay_window(cursor, limit);
                (replay.bytes, replay.cursor, false)
            }
        };
        Ok(BackgroundExecutionOutput {
            bytes,
            cursor: next,
            retained_from,
            total_written,
            discarded,
            output_file: info.output_file,
            from_disk,
        })
    }

    /// Reads the complete persisted output after a foreground wait.
    pub fn complete_output(
        &self,
        id: &BackgroundExecutionId,
    ) -> Result<Vec<u8>, BackgroundExecutionError> {
        let info = self.get(id)?;
        std::fs::read(&info.output_file).map_err(|source| state_error(&info.output_file, source))
    }

    /// Consumes one terminal foreground execution and removes every transient
    /// artifact it created.
    pub fn finish_foreground(
        &self,
        id: &BackgroundExecutionId,
    ) -> Result<Vec<u8>, BackgroundExecutionError> {
        let state = self.state(id)?;
        let _transition = state.transition();
        if state.is_durable() {
            return Err(BackgroundExecutionError::DurableForeground(id.clone()));
        }
        let info = state.info();
        if !info.status.is_terminal() {
            return Err(BackgroundExecutionError::ForegroundStillRunning(id.clone()));
        }
        let output = std::fs::read(&info.output_file)
            .map_err(|source| state_error(&info.output_file, source))?;
        remove_execution_files(&info)?;
        let mut executions = self.executions();
        if executions
            .get(id)
            .is_some_and(|candidate| Arc::ptr_eq(candidate, &state))
        {
            executions.remove(id);
        }
        Ok(output)
    }

    /// Waits until the command settles, or returns a non-terminal progress
    /// checkpoint when `timeout` elapses.
    pub async fn wait(
        &self,
        id: &BackgroundExecutionId,
        timeout: Option<Duration>,
    ) -> Result<BackgroundWaitOutcome, BackgroundExecutionError> {
        let state = self.state(id)?;
        let mut receiver = state.info.subscribe();
        let wait = async {
            loop {
                let info = receiver.borrow().clone();
                if info.status.is_terminal() {
                    return info;
                }
                if receiver.changed().await.is_err() {
                    return receiver.borrow().clone();
                }
            }
        };
        match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, wait).await {
                Ok(info) => Ok(BackgroundWaitOutcome {
                    info,
                    timed_out: false,
                }),
                Err(_) => Ok(BackgroundWaitOutcome {
                    info: state.info(),
                    timed_out: true,
                }),
            },
            None => Ok(BackgroundWaitOutcome {
                info: wait.await,
                timed_out: false,
            }),
        }
    }

    /// Requests cancellation of a running command. Terminal calls are idempotent.
    pub fn cancel(&self, id: &BackgroundExecutionId) -> Result<bool, BackgroundExecutionError> {
        let state = self.state(id)?;
        if state.info().status.is_terminal() {
            return Ok(false);
        }
        Ok(!state.cancel.send_replace(true))
    }

    /// Directory containing durable status/output and active foreground output.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    fn state(
        &self,
        id: &BackgroundExecutionId,
    ) -> Result<Arc<ExecutionState>, BackgroundExecutionError> {
        self.executions()
            .get(id)
            .cloned()
            .ok_or_else(|| BackgroundExecutionError::NotFound(id.clone()))
    }

    fn output_path(&self, id: &BackgroundExecutionId) -> PathBuf {
        self.inner.root.join(format!("{id}{OUTPUT_SUFFIX}"))
    }

    fn status_path(&self, id: &BackgroundExecutionId) -> PathBuf {
        self.inner.root.join(format!("{id}{STATUS_SUFFIX}"))
    }

    fn executions(&self) -> MutexGuard<'_, HashMap<BackgroundExecutionId, Arc<ExecutionState>>> {
        self.inner
            .executions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn prune_retained(&self) {
        let mut terminal = self
            .executions()
            .iter()
            .filter_map(|(id, state)| {
                let info = state.info();
                (state.is_durable() && info.status.is_terminal())
                    .then(|| (id.clone(), Arc::clone(state), info))
            })
            .collect::<Vec<_>>();
        if terminal.len() <= MAX_RETAINED_TERMINAL_EXECUTIONS {
            return;
        }
        terminal.sort_by(|left, right| {
            left.2
                .time_completed
                .unwrap_or(left.2.time_updated)
                .cmp(&right.2.time_completed.unwrap_or(right.2.time_updated))
                .then_with(|| left.2.time_created.cmp(&right.2.time_created))
                .then_with(|| left.0.cmp(&right.0))
        });
        let remove = terminal.len() - MAX_RETAINED_TERMINAL_EXECUTIONS;
        for (id, state, info) in terminal.into_iter().take(remove) {
            if let Err(error) = remove_execution_files(&info) {
                tracing::warn!(
                    execution_id = %id,
                    error = %error,
                    "could not prune retained background execution"
                );
                continue;
            }
            let mut executions = self.executions();
            if executions
                .get(&id)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &state))
            {
                executions.remove(&id);
            }
        }
    }
}

async fn run_execution(
    mut child: Child,
    state: Arc<ExecutionState>,
    mut cancel: watch::Receiver<bool>,
    hard_ceiling: Duration,
    service: BackgroundExecutionService,
) {
    let output_file = state.info().output_file;
    let (chunks, receiver) = mpsc::channel::<Vec<u8>>(32);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(pump(stdout, chunks.clone()));
    let stderr_task = tokio::spawn(pump(stderr, chunks.clone()));
    drop(chunks);
    let output_state = Arc::clone(&state);
    let output_task = tokio::spawn(collect_output(output_file, output_state, receiver));

    enum Outcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        Cancelled,
        TimedOut,
    }

    let outcome = tokio::select! {
        biased;
        result = child.wait() => Outcome::Exited(result),
        changed = cancel.changed() => {
            if changed.is_ok() && *cancel.borrow() {
                terminate(&mut child).await;
                let _status = child.wait().await;
                Outcome::Cancelled
            } else {
                Outcome::Exited(child.wait().await)
            }
        }
        () = tokio::time::sleep(hard_ceiling) => {
            terminate(&mut child).await;
            let _status = child.wait().await;
            Outcome::TimedOut
        }
    };

    let stdout_result = join_reader(stdout_task).await;
    let stderr_result = join_reader(stderr_task).await;
    let output_result = match output_task.await {
        Ok(result) => result,
        Err(error) => Err(std::io::Error::other(format!(
            "background output task failed to join: {error}"
        ))),
    };

    let now = now_millis();
    let mut info = state.info();
    info.pid = None;
    info.time_updated = now;
    info.time_completed = Some(now);
    match outcome {
        Outcome::Exited(Ok(status)) => {
            info.status = BackgroundExecutionStatus::Completed;
            info.exit_code = status.code();
        }
        Outcome::Exited(Err(error)) => {
            info.status = BackgroundExecutionStatus::Failed;
            info.error = Some(format!("waiting for the command failed: {error}"));
        }
        Outcome::Cancelled => {
            info.status = BackgroundExecutionStatus::Cancelled;
            info.error = Some("cancelled by request".to_owned());
        }
        Outcome::TimedOut => {
            info.status = BackgroundExecutionStatus::Failed;
            info.timed_out = true;
            info.error = Some(format!(
                "command exceeded its hard ceiling after {:.1}s",
                hard_ceiling.as_secs_f64()
            ));
        }
    }
    for result in [stdout_result, stderr_result, output_result] {
        if let Err(error) = result {
            info.status = BackgroundExecutionStatus::Failed;
            info.error = Some(format!("capturing command output failed: {error}"));
            break;
        }
    }
    let durable = {
        let _transition = state.transition();
        let durable = state.is_durable();
        if durable && let Err(error) = persist_info(&info) {
            info.status = BackgroundExecutionStatus::Failed;
            info.error = Some(error.to_string());
        }
        state.info.send_replace(info.clone());
        if durable {
            let _delivered = service
                .inner
                .events
                .send(BackgroundExecutionEvent::Settled(info));
        }
        durable
    };
    if durable {
        service.prune_retained();
    }
}

async fn pump(
    pipe: Option<impl AsyncRead + Unpin>,
    sender: mpsc::Sender<Vec<u8>>,
) -> std::io::Result<()> {
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut buffer = vec![0u8; OUTPUT_CHUNK];
    loop {
        let read = pipe.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        if sender.send(buffer[..read].to_vec()).await.is_err() {
            return Ok(());
        }
    }
}

async fn collect_output(
    path: PathBuf,
    state: Arc<ExecutionState>,
    mut receiver: mpsc::Receiver<Vec<u8>>,
) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await?;
    while let Some(chunk) = receiver.recv().await {
        state.output().push(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await
}

async fn join_reader(task: tokio::task::JoinHandle<std::io::Result<()>>) -> std::io::Result<()> {
    task.await
        .map_err(|error| std::io::Error::other(format!("output reader failed to join: {error}")))?
}

async fn terminate(child: &mut Child) {
    let Some(pid) = child.id() else {
        return;
    };
    if zuno_process::request_contained_process_shutdown(pid).is_err() {
        let _kill = child.start_kill();
    }
}

fn persist_info(info: &BackgroundExecutionInfo) -> Result<(), BackgroundExecutionError> {
    let encoded = serde_json::to_vec_pretty(&PersistedExecution {
        format: STATE_FORMAT,
        info: info.clone(),
    })
    .expect("background execution state is serializable");
    let temporary = info.status_file.with_extension("json.tmp");
    {
        let mut file =
            std::fs::File::create(&temporary).map_err(|source| state_error(&temporary, source))?;
        file.write_all(&encoded)
            .map_err(|source| state_error(&temporary, source))?;
        file.sync_all()
            .map_err(|source| state_error(&temporary, source))?;
    }
    std::fs::rename(&temporary, &info.status_file)
        .map_err(|source| state_error(&info.status_file, source))
}

fn remove_execution_files(info: &BackgroundExecutionInfo) -> Result<(), BackgroundExecutionError> {
    remove_if_exists(&info.status_file)?;
    remove_if_exists(&info.status_file.with_extension("json.tmp"))?;
    remove_if_exists(&info.output_file)
}

fn remove_if_exists(path: &Path) -> Result<(), BackgroundExecutionError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(state_error(path, source)),
    }
}

/// One window of a persisted output file, read by seeking rather than from the start.
///
/// The only seek reader in this crate. It answers both questions asked of the persisted
/// output — the last [`BUFFER_LIMIT`] bytes when a restart restores a retained tail, and
/// an arbitrary window when a caller asks for a prefix the ring has discarded — because
/// two readers over the same file is two places for an off-by-one to live.
///
/// An absent file reads as no bytes: a foreground command removes its own output once
/// the caller has consumed it, and a restart is entitled to find the file gone.
fn read_window(path: &Path, offset: u64, limit: Option<usize>) -> std::io::Result<Vec<u8>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    let start = offset.min(length);
    file.seek(std::io::SeekFrom::Start(start))?;
    let remaining = usize::try_from(length - start).unwrap_or(usize::MAX);
    let take = limit.map_or(remaining, |limit| limit.min(remaining));
    let mut bytes = Vec::with_capacity(take);
    file.take(take as u64).read_to_end(&mut bytes)?;
    if start.saturating_add(bytes.len() as u64) < length {
        crate::buffer::trim_incomplete_tail(&mut bytes);
    }
    Ok(bytes)
}

fn state_error(path: &Path, source: std::io::Error) -> BackgroundExecutionError {
    BackgroundExecutionError::State {
        path: path.to_owned(),
        source,
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_reject_path_material() {
        assert!(BackgroundExecutionId::parse("../escape").is_err());
        assert!(BackgroundExecutionId::parse("bg_0123456789abcdef0123456789abcdef").is_ok());
    }
}
