//! Process-owned background command execution with bounded live output.
//!
//! A command enters this service before it is spawned. Foreground callers may
//! wait for it, detach after an attention deadline, or cancel it, but they never
//! transfer an already-running [`tokio::task::JoinHandle`] between owners. That
//! single-owner shape is what makes cancellation and at-most-once execution hold
//! across explicit background mode and foreground timeout promotion.

use crate::{BUFFER_LIMIT, ReplayCursor, ScrollbackBuffer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, watch};
use uuid::Uuid;

const STATE_FORMAT: u32 = 1;
const OUTPUT_SUFFIX: &str = ".output";
const STATUS_SUFFIX: &str = ".status.json";
const OUTPUT_CHUNK: usize = 8 * 1024;

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

/// One command's durable metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundExecutionInfo {
    pub id: BackgroundExecutionId,
    pub session_id: String,
    pub title: String,
    pub command: String,
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
}

/// Frontend-neutral snapshot consumed by TUI, server, and future clients.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundExecutionProjection {
    pub executions: Vec<BackgroundExecutionInfo>,
}

/// Command launch input. Environment values are never persisted.
#[derive(Debug)]
pub struct BackgroundExecutionInput {
    pub program: OsString,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub session_id: String,
    pub title: String,
    pub command: String,
    pub hard_ceiling: Duration,
}

/// Bounded output replay for a running or retained command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundExecutionOutput {
    pub bytes: Vec<u8>,
    pub cursor: u64,
    pub retained_from: u64,
    pub total_written: u64,
    pub discarded: u64,
    pub output_file: PathBuf,
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
            let tail = read_tail(&output_file, BUFFER_LIMIT)
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
                }),
            );
        }
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

        let (program, arguments) =
            zuno_process::guarded_argv(&input.program, input.arguments.iter());
        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(&input.cwd)
            .env_clear()
            .envs(&input.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|source| BackgroundExecutionError::Spawn {
                command: input.command.clone(),
                source,
            })?;
        let pid = child.id();
        let now = now_millis();
        let info = BackgroundExecutionInfo {
            id: id.clone(),
            session_id: input.session_id,
            title: input.title,
            command: input.command,
            cwd: input.cwd,
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
        };
        if let Err(error) = persist_info(&info) {
            if let Some(pid) = pid {
                let _terminated = zuno_process::terminate_process_tree(pid);
            }
            let _kill = child.start_kill();
            return Err(error);
        }

        let (info_sender, _) = watch::channel(info.clone());
        let (cancel_sender, cancel_receiver) = watch::channel(false);
        let state = Arc::new(ExecutionState {
            info: info_sender,
            output: Mutex::new(ScrollbackBuffer::new()),
            cancel: cancel_sender,
        });
        self.executions().insert(id, Arc::clone(&state));
        let _delivered = self
            .inner
            .events
            .send(BackgroundExecutionEvent::Created(info.clone()));

        let events = self.inner.events.clone();
        tokio::spawn(run_execution(
            child,
            state,
            cancel_receiver,
            input.hard_ceiling,
            events,
        ));
        Ok(info)
    }

    /// Every command in creation order.
    #[must_use]
    pub fn list(&self) -> Vec<BackgroundExecutionInfo> {
        let mut values = self
            .executions()
            .values()
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

    /// Replays bounded output from an absolute cursor.
    pub fn output(
        &self,
        id: &BackgroundExecutionId,
        cursor: ReplayCursor,
    ) -> Result<BackgroundExecutionOutput, BackgroundExecutionError> {
        let state = self.state(id)?;
        let output = state.output();
        let replay = output.replay(cursor);
        Ok(BackgroundExecutionOutput {
            bytes: replay.bytes,
            cursor: replay.cursor,
            retained_from: output.start_cursor(),
            total_written: output.total_written(),
            discarded: output.discarded(),
            output_file: state.info().output_file,
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

    /// Directory containing durable status and complete output files.
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
}

async fn run_execution(
    mut child: Child,
    state: Arc<ExecutionState>,
    mut cancel: watch::Receiver<bool>,
    hard_ceiling: Duration,
    events: broadcast::Sender<BackgroundExecutionEvent>,
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
    if let Err(error) = persist_info(&info) {
        info.status = BackgroundExecutionStatus::Failed;
        info.error = Some(error.to_string());
    }
    state.info.send_replace(info.clone());
    let _delivered = events.send(BackgroundExecutionEvent::Settled(info));
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
    let terminated = tokio::task::spawn_blocking(move || zuno_process::terminate_process_tree(pid))
        .await
        .is_ok_and(|result| result.is_ok());
    if !terminated {
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

fn read_tail(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    let start = length.saturating_sub(limit as u64);
    file.seek(std::io::SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((length - start) as usize);
    file.read_to_end(&mut bytes)?;
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
