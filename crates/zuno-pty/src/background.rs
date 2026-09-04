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
const STATUS_TEMP_SUFFIX: &str = ".status.json.tmp";
const LOCK_SUFFIX: &str = ".lock";
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
    #[error("background execution `{0}` is owned by another live Zuno process in this workspace")]
    Foreign(BackgroundExecutionId),
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

/// One row exactly as it exists on disk.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedExecution {
    format: u32,
    /// Whether the process that wrote this row held the execution's ownership claim.
    ///
    /// Three states, all of them explicit:
    ///
    /// * `Some(true)` — a claim backed this row, so a peer that later acquires that claim
    ///   has proven the writer is gone.
    /// * `Some(false)` — the writer could not take a claim (an unlockable filesystem, or a
    ///   transient open/lock failure) and ran the command anyway. The absence of a live
    ///   claim then proves nothing, so no peer may settle the row or delete its files.
    /// * `None` — the row predates the claim protocol (Zuno 0.6.6 and earlier wrote
    ///   format 3 with no marker and no `<id>.lock` at all). Absent is *unproven by this
    ///   build*, which is not the same as unowned: the writer may be a released Zuno
    ///   running that command right now. Such a row is settled only when the recorded
    ///   process is provably gone — see [`Self::claim_evidence`] and [`settlable`].
    ///
    /// The field is additive inside format [`STATE_FORMAT`]: an older Zuno ignores it and a
    /// row without it still decodes, so no state file has to be rewritten to be readable.
    #[serde(default)]
    claimed: Option<bool>,
    info: BackgroundExecutionInfo,
}

/// What one row says about the claim that backed the process which wrote it.
///
/// Typed rather than a `bool` because the three states need three different decisions and
/// the two that are not `Backed` are the two a boolean silently merges: an absent marker
/// used to read as "provable", which settled a released build's live command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimEvidence {
    /// `claimed: true` — a claim backed the row, so acquiring that claim proves the
    /// writer is gone.
    Backed,
    /// `claimed: false` — the writer never held a claim, so no claim can prove anything
    /// about it and nothing here may settle it.
    Unbacked,
    /// No marker at all: a row from a build that had no claim protocol.
    PreProtocol,
}

impl PersistedExecution {
    /// What the row's marker proves about the process that wrote it.
    const fn claim_evidence(&self) -> ClaimEvidence {
        match self.claimed {
            Some(true) => ClaimEvidence::Backed,
            Some(false) => ClaimEvidence::Unbacked,
            None => ClaimEvidence::PreProtocol,
        }
    }
}

/// Cross-process proof that one execution still has a living owner.
///
/// The claim is an advisory lock held by an open handle, so the OS releases it
/// when the owning process exits however it exits. A peer that acquires the claim
/// has therefore proven that no process still holds it; a peer that cannot acquire it
/// has proven nothing and must leave the execution's row and files alone.
///
/// Acquisition is proof about the *claim*, not about the row on its own: a row whose
/// writer could not take a claim ([`PersistedExecution::claimed`] `== Some(false)`) has no
/// claim to acquire, so nothing about it may be inferred from an empty lock file.
#[derive(Debug)]
struct OwnershipClaim {
    file: Option<std::fs::File>,
    path: PathBuf,
}

impl OwnershipClaim {
    /// `Ok(None)` means a live owner holds the claim.
    fn acquire(path: &Path) -> std::io::Result<Option<Self>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                file: Some(file),
                path: path.to_owned(),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

impl Drop for OwnershipClaim {
    fn drop(&mut self) {
        // Closing the handle before unlinking keeps Windows from leaving the name in a
        // pending-delete state that a peer's own claim attempt cannot open.
        //
        // Unlinking a name this handle still owns is only safe because of what every
        // holder does under the claim: a peer that opened the same inode before the
        // unlink can lock it afterwards, so two claims on one id can briefly coexist,
        // one of them on an unlinked inode. `restore` and `refresh_foreign` therefore
        // re-read the status file inside the claim and only ever move a row toward
        // terminal, and `reclaim_orphan` only ever removes artifacts of an id that has
        // no row at all. A future settle path that skips that re-read would make this
        // window observable.
        drop(self.file.take());
        let _removed = remove_if_exists(&self.path);
    }
}

#[derive(Debug)]
struct ExecutionState {
    info: watch::Sender<BackgroundExecutionInfo>,
    output: Mutex<ScrollbackBuffer>,
    cancel: watch::Sender<bool>,
    durable: AtomicBool,
    transition: Mutex<()>,
    /// Held while this process owns the execution's row and files.
    claim: Mutex<Option<OwnershipClaim>>,
    /// Whether another process owns this row, so nothing here may settle, cancel, or
    /// reclaim it. Cleared only by [`BackgroundExecutionService::refresh_foreign`], which
    /// adopts the owner's persisted outcome once one exists.
    foreign: AtomicBool,
}

impl ExecutionState {
    fn info(&self) -> BackgroundExecutionInfo {
        self.info.borrow().clone()
    }

    /// Publishes that this process no longer owns the execution's files.
    fn release_claim(&self) {
        let released = self
            .claim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(released);
    }

    /// Whether this process currently backs the row with an ownership claim.
    fn holds_claim(&self) -> bool {
        self.claim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn is_foreign(&self) -> bool {
        self.foreign.load(Ordering::Acquire)
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
    /// Opens the workspace's execution store, reconciles interrupted commands, and
    /// reclaims the artifacts of executions no process owns any more.
    ///
    /// A persisted `running` row whose owner is gone cannot prove whether that owner
    /// completed, failed, or performed a side effect first, so it is changed to
    /// `uncertain` and is never replayed. "Whose owner is gone" is a fact this
    /// service establishes by taking the execution's ownership claim, and only for a row
    /// that says a claim backed it: a second Zuno process in the same workspace is an
    /// expected mode, and settling a command that is still running would tell the model a
    /// side effect never happened while it is happening, then let retention delete the
    /// output it is still writing. A row a peer still owns stays visible and readable and
    /// converges through [`Self::refresh_foreign`] once its owner records an outcome.
    ///
    /// A row this build cannot interpret is skipped and left on disk untouched. It is
    /// the only evidence of what a previous process was doing, and one such file must
    /// not stop every session in the worktree from opening.
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
        let mut persisted = Vec::new();
        let mut artifacts = Vec::new();
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let (stem, owned) = if let Some(stem) = name.strip_suffix(STATUS_SUFFIX) {
                (stem, &mut persisted)
            } else if let Some(stem) = name
                .strip_suffix(OUTPUT_SUFFIX)
                .or_else(|| name.strip_suffix(LOCK_SUFFIX))
                .or_else(|| name.strip_suffix(STATUS_TEMP_SUFFIX))
            {
                (stem, &mut artifacts)
            } else {
                continue;
            };
            // An unrecognizable name is left alone: this directory is Zuno's, but a
            // file no id can explain is not Zuno's to delete.
            if let Ok(id) = BackgroundExecutionId::parse(stem) {
                owned.push(id);
            }
        }
        persisted.sort();
        persisted.dedup();
        artifacts.sort();
        artifacts.dedup();

        for id in &persisted {
            service.restore(id);
        }
        for id in &artifacts {
            if persisted.binary_search(id).is_err() {
                service.reclaim_orphan(id);
            }
        }
        service.prune_retained();
        Ok(service)
    }

    /// Registers one persisted row, reconciling it only when its owner is provably gone.
    fn restore(&self, id: &BackgroundExecutionId) {
        let status_file = self.status_path(id);
        let row = match read_persisted(&status_file, id) {
            Ok(row) => row,
            Err(error) => {
                tracing::warn!(
                    path = %status_file.display(),
                    error = %error,
                    "skipping unreadable background execution state"
                );
                return;
            }
        };
        let evidence = row.claim_evidence();
        let mut info = row.info;
        let output_file = self.output_path(id);
        info.output_file = output_file.clone();
        info.status_file = status_file.clone();

        let mut foreign = false;
        if info.status == BackgroundExecutionStatus::Running {
            match self.settle_unowned(id, &mut info, evidence) {
                Ok(()) => {}
                Err(Unsettled::LivePeer) => foreign = true,
                Err(Unsettled::Unclaimed) => {
                    tracing::warn!(
                        path = %status_file.display(),
                        "leaving a running background execution alone because it was recorded \
                         without an ownership claim; nothing can prove its owner exited"
                    );
                    foreign = true;
                }
                Err(Unsettled::RecordedProcessAlive(pid)) => {
                    tracing::info!(
                        path = %status_file.display(),
                        pid,
                        "leaving a background execution from a build without the ownership \
                         claim protocol alone because the process it recorded is still running"
                    );
                    foreign = true;
                }
                Err(Unsettled::RecordedProcessUnresolvable) => {
                    tracing::warn!(
                        path = %status_file.display(),
                        "leaving a background execution from a build without the ownership \
                         claim protocol alone because this platform cannot prove whether the \
                         process it recorded is still running"
                    );
                    foreign = true;
                }
                Err(Unsettled::Unprovable(error)) => {
                    tracing::warn!(
                        path = %status_file.display(),
                        error = %error,
                        "leaving a running background execution alone because its ownership \
                         could not be established"
                    );
                    foreign = true;
                }
                Err(Unsettled::NotRecorded(error)) => {
                    tracing::warn!(
                        path = %status_file.display(),
                        error = %error,
                        "could not record an interrupted background execution as uncertain"
                    );
                    return;
                }
            }
        } else {
            // A process that crashed after settling a row leaves its claim file behind,
            // and no other path would ever reclaim it.
            let claim_path = self.claim_path(id);
            if claim_path.exists() {
                let _released = OwnershipClaim::acquire(&claim_path);
            }
        }

        let total_written = std::fs::metadata(&output_file)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        // A tail that cannot be read is a cold cache, not a reason to refuse the row:
        // the file is untouched and `output` reports the real failure to whoever asks
        // for those bytes.
        let tail = read_window(
            &output_file,
            total_written.saturating_sub(BUFFER_LIMIT as u64),
            None,
        )
        .unwrap_or_else(|error| {
            tracing::warn!(
                path = %output_file.display(),
                error = %error,
                "could not restore the retained output tail of a background execution"
            );
            Vec::new()
        });
        let (info, _) = watch::channel(info);
        let (cancel, _) = watch::channel(false);
        self.executions().insert(
            id.clone(),
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
                claim: Mutex::new(None),
                foreign: AtomicBool::new(foreign),
            }),
        );
    }

    /// Settles one `running` row whose owner is provably gone.
    ///
    /// The claim is taken first and held across the re-read and the write. An owner
    /// releases its claim by settling *or* by dying, so the status file is re-read inside
    /// the claim rather than trusted from before: a recorded outcome wins, and only a row
    /// still marked `running` becomes `uncertain`. `evidence` is what the caller's read of
    /// the row said about [`PersistedExecution::claimed`]; the re-read replaces it when it
    /// succeeds, because that is the value written closest to the decision.
    ///
    /// [`settlable`] is consulted *before* the claim as well as after it. A claim proves
    /// nothing about a row no claim ever backed, and creating, locking and unlinking
    /// `<id>.lock` only to refuse is churn in every `get`, `list`, `wait`, `cancel` and
    /// `output` that touches such a row.
    fn settle_unowned(
        &self,
        id: &BackgroundExecutionId,
        info: &mut BackgroundExecutionInfo,
        evidence: ClaimEvidence,
    ) -> Result<(), Unsettled> {
        settlable(evidence, info.pid)?;
        let claim = match OwnershipClaim::acquire(&self.claim_path(id)) {
            Ok(Some(claim)) => claim,
            Ok(None) => return Err(Unsettled::LivePeer),
            Err(error) => return Err(Unsettled::Unprovable(error)),
        };
        let mut evidence = evidence;
        if let Ok(current) = read_persisted(&self.status_path(id), id) {
            evidence = current.claim_evidence();
            let recorded = current.info;
            info.status = recorded.status;
            info.pid = recorded.pid;
            info.exit_code = recorded.exit_code;
            info.timed_out = recorded.timed_out;
            info.time_updated = recorded.time_updated;
            info.time_completed = recorded.time_completed;
            info.error = recorded.error;
        }
        if info.status == BackgroundExecutionStatus::Running {
            if let Err(refusal) = settlable(evidence, info.pid) {
                drop(claim);
                return Err(refusal);
            }
            settle_as_uncertain(info);
            if let Err(error) = persist_info(info, true) {
                drop(claim);
                return Err(Unsettled::NotRecorded(error));
            }
        }
        drop(claim);
        Ok(())
    }

    /// Re-derives a row this process does not own, so a peer's command converges here.
    ///
    /// A `foreign` row is a snapshot of someone else's execution, and nothing local will
    /// ever move it: this process holds no child, subscribes to no exit, and must not
    /// settle it. Without this refresh a finished command stays `running` with a stale pid
    /// for the life of the process, `wait` refuses forever, and replay keeps serving the
    /// tail captured at open. Consulting the store on demand is what turns "not ours" into
    /// "not ours *yet*".
    ///
    /// Returns whether the row is still registered: a row its owner has retired is dropped
    /// here, and every caller must then report it as missing rather than as running.
    fn refresh_foreign(&self, id: &BackgroundExecutionId, state: &Arc<ExecutionState>) -> bool {
        if !state.is_foreign() {
            return true;
        }
        let status_file = self.status_path(id);
        let row = match read_persisted(&status_file, id) {
            Ok(row) => row,
            Err(error) => {
                // A publish replaces the status file by rename, so it is never absent
                // mid-write: absent means the owner's retention retired this row. Keeping a
                // registration whose store entry is gone would report a `running` command
                // that no longer exists anywhere.
                if retired(&error) {
                    self.forget(id, state);
                    return false;
                }
                tracing::debug!(
                    path = %status_file.display(),
                    error = %error,
                    "could not re-read a background execution owned by another process"
                );
                return true;
            }
        };
        let _transition = state.transition();
        if !state.is_foreign() {
            return true;
        }
        let evidence = row.claim_evidence();
        let mut info = row.info;
        info.output_file = self.output_path(id);
        info.status_file = status_file;
        if info.status == BackgroundExecutionStatus::Running {
            // No terminal outcome is recorded yet, but the owner's capture file may have
            // grown, and that much is safe to read: it is append-only until it is removed.
            self.sync_foreign_output(state, &info.output_file);
            if let Err(reason) = self.settle_unowned(id, &mut info, evidence) {
                tracing::debug!(
                    execution_id = %id,
                    reason = reason.as_str(),
                    "a background execution owned by another process is still running"
                );
                return true;
            }
        }
        self.sync_foreign_output(state, &info.output_file);
        // Ordered so a concurrent `prune_retained` can only ever see this row as
        // "foreign" (skipped) or as "terminal and adopted" (prunable), never as a
        // terminal row whose files another process is still writing.
        state.info.send_replace(info.clone());
        state.foreign.store(false, Ordering::Release);
        let _delivered = self
            .inner
            .events
            .send(BackgroundExecutionEvent::Settled(info));
        true
    }

    /// Catches a peer-owned row's retained ring up with what its owner appended.
    ///
    /// Bounded by [`BUFFER_LIMIT`] per call however far the owner has run ahead: a gap
    /// larger than the ring can retain is served as a tail, which is exactly what the ring
    /// would have kept anyway. The requested window is capped at the gap measured before
    /// the read, so an owner writing during the read cannot widen it.
    fn sync_foreign_output(&self, state: &Arc<ExecutionState>, output_file: &Path) {
        let Ok(metadata) = std::fs::metadata(output_file) else {
            return;
        };
        let length = metadata.len();
        let mut retained = state.output();
        let seen = retained.total_written();
        if length == seen {
            return;
        }
        let gap = length.saturating_sub(seen);
        if length < seen || gap > BUFFER_LIMIT as u64 {
            let start = length.saturating_sub(BUFFER_LIMIT as u64);
            let tail = read_window(output_file, start, Some(BUFFER_LIMIT)).unwrap_or_default();
            // `length` is deliberately not passed on: see `rebase_ring`.
            *retained = rebase_ring(start, &tail);
            return;
        }
        let limit = usize::try_from(gap)
            .unwrap_or(BUFFER_LIMIT)
            .min(BUFFER_LIMIT);
        if let Ok(appended) = read_window(output_file, seen, Some(limit)) {
            retained.push(&appended);
        }
    }

    /// Removes the artifacts of an execution that has no state row and no live owner.
    ///
    /// A foreground command's capture file is created before it runs and removed by
    /// the caller that consumes it, so a killed process leaves one behind that no
    /// later `running` row explains. Only the claim separates that dead file from a
    /// peer process's live capture.
    ///
    /// The claim file must already exist to be evidence of anything. Every capture file
    /// this build creates is preceded by its `<id>.lock` in [`Self::claim_capture`], and
    /// a process killed before it could clean up leaves both behind — so a lock file that
    /// is *not* there means the capture came from a writer that never took one: a build
    /// from before the claim protocol, or a filesystem that refused the lock. Creating a
    /// fresh lock for such an id and reading "nobody holds it" would prove nothing about
    /// that writer and would delete a live command's output from under it.
    fn reclaim_orphan(&self, id: &BackgroundExecutionId) {
        let claim_path = self.claim_path(id);
        if !claim_path.exists() {
            tracing::debug!(
                path = %claim_path.display(),
                "leaving background execution artifacts alone because no ownership claim was \
                 ever recorded for them"
            );
            return;
        }
        match OwnershipClaim::acquire(&claim_path) {
            Ok(Some(claim)) => {
                let output_file = self.output_path(id);
                for path in [&output_file, &temp_status_path(&self.status_path(id))] {
                    if let Err(error) = remove_if_exists(path) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            "could not reclaim an orphaned background execution artifact"
                        );
                    }
                }
                drop(claim);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    path = %claim_path.display(),
                    error = %error,
                    "leaving background execution artifacts alone because their ownership \
                     could not be established"
                );
            }
        }
    }

    /// Subscribe to lifecycle changes. A lagging client should call [`Self::list`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<BackgroundExecutionEvent> {
        self.inner.events.subscribe()
    }

    /// Takes ownership of one new execution id, then creates its capture file.
    ///
    /// The order is load-bearing and is what
    /// `a_capture_file_is_never_created_before_its_ownership_claim` pins. `<id>.output` is
    /// the artifact a peer's [`Self::reclaim_orphan`] sweep looks at, and the claim is the
    /// only thing that tells that sweep the file is alive, so the file must not exist for
    /// even one instant while no claim for `id` is held. Creating it first left a window in
    /// which a peer deleted a live capture and the owner's own append then failed.
    ///
    /// # Errors
    ///
    /// A claim another live process already holds means that id's files are not this
    /// process's to write, so the start is refused instead of sharing them. A claim the
    /// filesystem refuses is only survivable when a peer asking the same question is
    /// refused the same way; when it is not, the start is refused rather than run with a
    /// capture file no claim protects.
    fn claim_capture(
        &self,
        id: &BackgroundExecutionId,
    ) -> Result<(Option<OwnershipClaim>, PathBuf), BackgroundExecutionError> {
        let claim_path = self.claim_path(id);
        let claim = match OwnershipClaim::acquire(&claim_path) {
            Ok(claim @ Some(_)) => claim,
            Ok(None) => return Err(BackgroundExecutionError::Foreign(id.clone())),
            Err(source) => {
                // Running unclaimed is only safe while a peer asking this same question
                // gets this same refusal: the peer must not be able to create and lock a
                // fresh `<id>.lock`, read "no owner", and reclaim a capture file this
                // process is still writing. A name that is still there refuses every
                // process identically (a directory, or an ACL none of them can open), so
                // the answer is symmetric and the command may run with the row recording
                // that no claim backs it. A name that is *not* there is not resolvable
                // from anything this process trusts, so it fails closed instead - nothing
                // has run yet, and for the usual causes (a missing or unwritable state
                // directory, exhausted descriptors, a full disk) creating the capture file
                // on the very next line would fail with the same error anyway.
                if !claim_path.exists() {
                    return Err(state_error(&claim_path, source));
                }
                tracing::warn!(
                    path = %claim_path.display(),
                    error = %source,
                    "could not claim background execution ownership on this filesystem; no \
                     other process will be able to reconcile this command"
                );
                None
            }
        };
        let output_file = self.output_path(id);
        // Dropping `claim` on this failure removes the claim file too.
        std::fs::File::create(&output_file).map_err(|source| state_error(&output_file, source))?;
        Ok((claim, output_file))
    }

    /// Starts one command under process-tree containment.
    ///
    /// The returned row is already registered and persisted. A caller may begin a
    /// foreground wait or return it to the user immediately without an adoption
    /// race.
    ///
    /// # Errors
    ///
    /// [`BackgroundExecutionError::Foreign`] when another live process already claims the
    /// minted id, and [`BackgroundExecutionError::State`] when the execution's ownership
    /// cannot be resolved at all; see [`Self::claim_capture`]. Both refuse before anything
    /// is spawned, so a refused start has run nothing.
    pub fn start(
        &self,
        input: BackgroundExecutionInput,
    ) -> Result<BackgroundExecutionInfo, BackgroundExecutionError> {
        std::fs::create_dir_all(&self.inner.root)
            .map_err(|source| state_error(&self.inner.root, source))?;
        let id = BackgroundExecutionId::mint();
        let status_file = self.status_path(&id);
        // Every early return below drops the claim, which removes its file.
        let (claim, output_file) = self.claim_capture(&id)?;

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
        // `claim.is_some()` is the honest answer, not the intended one: a row a peer could
        // later find unclaimed must say so, or that peer will read the missing lock file as
        // proof this process died and settle a command that is still running.
        if durable && let Err(error) = persist_info(&info, claim.is_some()) {
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
            claim: Mutex::new(claim),
            foreign: AtomicBool::new(false),
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
    ///
    /// Rows another process owns are refreshed first, so a client listing this workspace
    /// sees a peer's command settle instead of a frozen `running` row with a stale pid.
    #[must_use]
    pub fn list(&self) -> Vec<BackgroundExecutionInfo> {
        let durable = self
            .executions()
            .iter()
            .filter(|(_, state)| state.is_durable())
            .map(|(id, state)| (id.clone(), Arc::clone(state)))
            .collect::<Vec<_>>();
        let mut values = durable
            .into_iter()
            .filter_map(|(id, state)| self.refresh_foreign(&id, &state).then(|| state.info()))
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
            persist_info(&info, state.holds_claim())?;
            state.durable.store(true, Ordering::Release);
            let _created = self
                .inner
                .events
                .send(BackgroundExecutionEvent::Created(info.clone()));
            if info.status.is_terminal() {
                // A command that settled before its promotion never reached the release
                // in `run_execution`, and its outcome is now persisted.
                state.release_claim();
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
    /// caller that wants the beginning says `From(0)`. Bounded, it stays a tail request —
    /// the newest `limit` bytes, ending at [`BackgroundExecutionOutput::total_written`] —
    /// so the window's first byte is `cursor - bytes.len()` however it was served.
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
        // Removed under the claim, released after. A capture file with no claim file beside
        // it is what a build from before the claim protocol leaves, and `reclaim_orphan`
        // deliberately leaves those alone - so releasing first would put this execution's
        // capture into exactly the state nothing sweeps, for as long as it takes to unlink
        // it, and permanently if this process is killed in between.
        remove_execution_files(&info)?;
        state.release_claim();
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
        // Only the owning process learns that a command settled, so waiting on a peer's
        // running command would wait forever. Say so instead.
        if state.is_foreign() && !state.info().status.is_terminal() {
            return Err(BackgroundExecutionError::Foreign(id.clone()));
        }
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
    ///
    /// # Errors
    ///
    /// A command another live Zuno process owns is refused rather than silently
    /// reported as cancelled: the process holding the child is the only one that can
    /// signal it.
    pub fn cancel(&self, id: &BackgroundExecutionId) -> Result<bool, BackgroundExecutionError> {
        let state = self.state(id)?;
        if state.info().status.is_terminal() {
            return Ok(false);
        }
        if state.is_foreign() {
            return Err(BackgroundExecutionError::Foreign(id.clone()));
        }
        Ok(!state.cancel.send_replace(true))
    }

    /// Directory containing durable status/output and active foreground output.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// One registered row, brought up to date if another process owns it.
    ///
    /// Every single-row entry point resolves through here, so `get`, `output`, `wait` and
    /// `cancel` all observe a peer-owned row's current state rather than the snapshot taken
    /// when this service opened. The lookup guard is released before the refresh, which
    /// reads the store and may settle an abandoned row.
    fn state(
        &self,
        id: &BackgroundExecutionId,
    ) -> Result<Arc<ExecutionState>, BackgroundExecutionError> {
        let state = self
            .executions()
            .get(id)
            .cloned()
            .ok_or_else(|| BackgroundExecutionError::NotFound(id.clone()))?;
        if !self.refresh_foreign(id, &state) {
            return Err(BackgroundExecutionError::NotFound(id.clone()));
        }
        Ok(state)
    }

    /// Drops one registration whose store entry another process has already retired.
    fn forget(&self, id: &BackgroundExecutionId, state: &Arc<ExecutionState>) {
        let mut executions = self.executions();
        if executions
            .get(id)
            .is_some_and(|candidate| Arc::ptr_eq(candidate, state))
        {
            executions.remove(id);
        }
    }

    fn output_path(&self, id: &BackgroundExecutionId) -> PathBuf {
        self.inner.root.join(format!("{id}{OUTPUT_SUFFIX}"))
    }

    fn status_path(&self, id: &BackgroundExecutionId) -> PathBuf {
        self.inner.root.join(format!("{id}{STATUS_SUFFIX}"))
    }

    fn claim_path(&self, id: &BackgroundExecutionId) -> PathBuf {
        self.inner.root.join(format!("{id}{LOCK_SUFFIX}"))
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
                // A row another process owns is retained by that process; deleting its
                // files from here would race the owner's own capture. This term is
                // load-bearing exactly once a row can be both terminal and foreign:
                // `refresh_foreign` publishes the adopted terminal row before it clears
                // the flag, and a prune interleaved between the two must skip it.
                (state.is_durable() && !state.is_foreign() && info.status.is_terminal())
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
        if durable && let Err(error) = persist_info(&info, state.holds_claim()) {
            info.status = BackgroundExecutionStatus::Failed;
            info.error = Some(error.to_string());
        }
        if durable {
            // The persisted outcome is now authoritative, so a peer that takes the
            // claim from here reads a terminal row and has nothing to reconcile. An
            // ephemeral execution keeps its claim until its caller consumes the
            // capture file, which is the only thing still protecting that file.
            state.release_claim();
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

/// Whether a failed read means the row is gone rather than unreadable.
fn retired(error: &BackgroundExecutionError) -> bool {
    matches!(
        error,
        BackgroundExecutionError::State { source, .. } if source.kind() == std::io::ErrorKind::NotFound
    )
}

/// Why a row this process does not own could not be settled from here.
#[derive(Debug)]
enum Unsettled {
    /// A live process holds the execution's ownership claim.
    LivePeer,
    /// The row was written without a claim, so no claim can prove anything about it.
    Unclaimed,
    /// The row predates the claim protocol and the process it recorded is still running.
    RecordedProcessAlive(u32),
    /// The row predates the claim protocol and this platform cannot say whether the
    /// process it recorded is still running.
    RecordedProcessUnresolvable,
    /// The filesystem could not prove ownership in either direction.
    Unprovable(std::io::Error),
    /// Ownership was proven, but the reconciled row could not be persisted.
    NotRecorded(BackgroundExecutionError),
}

impl Unsettled {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::LivePeer => "a live process holds its ownership claim",
            Self::Unclaimed => "it was recorded without an ownership claim",
            Self::RecordedProcessAlive(_) => {
                "it predates the ownership claim protocol and the process it recorded is alive"
            }
            Self::RecordedProcessUnresolvable => {
                "it predates the ownership claim protocol and this platform cannot probe the \
                 process it recorded"
            }
            Self::Unprovable(_) => "its ownership could not be established",
            Self::NotRecorded(_) => "the reconciled row could not be persisted",
        }
    }
}

/// Whether a still-`running` row may be rewritten as `uncertain` from this process.
///
/// The claim answers this for every row a build with the claim protocol wrote. For a row
/// from before it there is no claim to ask, and the released build that wrote it creates
/// no lock file at all — so "no lock file" is not evidence there, and the only other thing
/// the row states about its owner is the pid it recorded. A live pid contradicts "the
/// owner is gone" outright; a pid this platform cannot probe leaves the question
/// unanswered, and an unanswered question fails closed. Refusing keeps the row visible,
/// readable and replayable and keeps retention away from its files; settling it would tell
/// the model a command that is still running never reported, then delete its output.
///
/// This can only ever refuse more than the released build did: every path that reaches it
/// settled unconditionally before, and the probe never widens what is settled or deleted.
fn settlable(evidence: ClaimEvidence, pid: Option<u32>) -> Result<(), Unsettled> {
    match evidence {
        ClaimEvidence::Backed => Ok(()),
        ClaimEvidence::Unbacked => Err(Unsettled::Unclaimed),
        ClaimEvidence::PreProtocol => match recorded_process(pid) {
            RecordedProcess::Gone => Ok(()),
            RecordedProcess::Alive => Err(Unsettled::RecordedProcessAlive(pid.unwrap_or_default())),
            RecordedProcess::Unresolvable => Err(Unsettled::RecordedProcessUnresolvable),
        },
    }
}

/// What this platform can prove about a process another process recorded.
///
/// Deliberately three-valued on every platform even though only `unix` can currently
/// construct the first two: the decision in [`settlable`] is the same everywhere, and
/// adding a Windows probe has to change one function rather than this contract.
#[cfg_attr(
    not(unix),
    expect(
        dead_code,
        reason = "off unix `recorded_process` can only answer `Unresolvable`, so nothing \
                  constructs `Alive` or `Gone` until a probe for that platform exists"
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordedProcess {
    /// A process with that identifier exists right now.
    Alive,
    /// No process with that identifier exists.
    Gone,
    /// This platform cannot answer, so nothing may be concluded from the answer.
    Unresolvable,
}

/// Probes a recorded pid without signalling it.
///
/// `kill(pid, 0)` is the POSIX existence check and needs no privilege beyond the caller's
/// own: `ESRCH` is the only answer that proves absence, `EPERM` proves the opposite (the
/// process exists and belongs to somebody else), and every other errno is an answer this
/// process cannot interpret. `nix` performs the call, so this crate keeps
/// `unsafe_code = "forbid"`.
///
/// Two answers are deliberately conservative. A pid this platform has no process table
/// for — Windows, where the equivalent query needs `unsafe` or a subprocess — is
/// `Unresolvable`, never `Gone`. So is a recorded pid that cannot be a child of the
/// writer (absent, zero, or wider than `pid_t`), because `kill` reads those as the caller's
/// own process group rather than as one process.
///
/// The remaining imprecision is one-directional by construction: pid reuse and a pid
/// recorded in another pid namespace can both report `Alive` for a process that is not the
/// recorded one, which leaves a row visible that could have been reconciled. Neither can
/// report `Gone` for a process that exists in this namespace.
#[cfg(unix)]
fn recorded_process(pid: Option<u32>) -> RecordedProcess {
    let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) else {
        return RecordedProcess::Unresolvable;
    };
    if pid <= 0 {
        return RecordedProcess::Unresolvable;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => RecordedProcess::Alive,
        Err(nix::errno::Errno::ESRCH) => RecordedProcess::Gone,
        Err(_) => RecordedProcess::Unresolvable,
    }
}

/// Windows has no process query this crate can make without `unsafe` or a subprocess, so
/// a row from before the claim protocol is never settled from here. It stays visible and
/// readable instead; see [`settlable`].
#[cfg(not(unix))]
const fn recorded_process(_pid: Option<u32>) -> RecordedProcess {
    RecordedProcess::Unresolvable
}

/// Marks one row whose owner disappeared without recording an authoritative outcome.
fn settle_as_uncertain(info: &mut BackgroundExecutionInfo) {
    let now = now_millis();
    info.status = BackgroundExecutionStatus::Uncertain;
    info.pid = None;
    info.time_updated = now;
    info.time_completed = Some(now);
    info.error = Some(
        "the previous Zuno process exited before recording an authoritative outcome; this \
         command was not replayed"
            .to_owned(),
    );
}

/// Reads one persisted row, refusing anything this build cannot interpret.
///
/// The row's identity comes from its file name, which is the name this service
/// derives every path from. A body that names a different execution is refused
/// rather than trusted, because trusting it would point one execution's state at
/// another execution's files.
fn read_persisted(
    status_file: &Path,
    id: &BackgroundExecutionId,
) -> Result<PersistedExecution, BackgroundExecutionError> {
    let bytes = std::fs::read(status_file).map_err(|source| state_error(status_file, source))?;
    let persisted: PersistedExecution =
        serde_json::from_slice(&bytes).map_err(|source| BackgroundExecutionError::Decode {
            path: status_file.to_owned(),
            source,
        })?;
    if persisted.format != STATE_FORMAT {
        return Err(state_error(
            status_file,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "unsupported background state format {}; expected {STATE_FORMAT}",
                    persisted.format
                ),
            ),
        ));
    }
    if &persisted.info.id != id {
        return Err(state_error(
            status_file,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("background state names execution `{}`", persisted.info.id),
            ),
        ));
    }
    Ok(persisted)
}

/// Publishes one row atomically.
///
/// `claimed` records whether the writing process holds this execution's ownership claim.
/// It is the only thing that tells a reader apart "no lock file because the owner is gone"
/// from "no lock file because the owner never managed to take one", so it must be the
/// truth about *this* process at *this* moment rather than an intent.
fn persist_info(
    info: &BackgroundExecutionInfo,
    claimed: bool,
) -> Result<(), BackgroundExecutionError> {
    // A workspace path that is not valid UTF-8 makes this encoding fail. It must reach
    // the caller as an error: the child is already running by the time a durable row is
    // written, and an abort here would leave that side effect with no record.
    let encoded = serde_json::to_vec_pretty(&PersistedExecution {
        format: STATE_FORMAT,
        claimed: Some(claimed),
        info: info.clone(),
    })
    .map_err(|source| {
        state_error(
            &info.status_file,
            std::io::Error::new(std::io::ErrorKind::InvalidData, source),
        )
    })?;
    let temporary = temp_status_path(&info.status_file);
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

/// Sibling of a status file that [`persist_info`] publishes from.
fn temp_status_path(status_file: &Path) -> PathBuf {
    status_file.with_extension("json.tmp")
}

fn remove_execution_files(info: &BackgroundExecutionInfo) -> Result<(), BackgroundExecutionError> {
    remove_if_exists(&info.status_file)?;
    remove_if_exists(&temp_status_path(&info.status_file))?;
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

/// Rebases a peer-owned ring onto a window read out of that peer's output file.
///
/// The end cursor is derived from the bytes that were actually read, never from the length
/// measured before the read. Those two disagree whenever the owner appends in between:
/// `read_window` then sees a longer file than its caller did, finds its window is no longer
/// the end of the file, and trims a split code point off it — so the window is shorter than
/// the measured length claims. Publishing the measured length over that shorter window would
/// leave every later cursor in this ring `length - (start + window.len())` bytes out, and
/// `sync_foreign_output` would keep reading from past the end. Understating the total
/// instead costs one extra catch-up read, which the next call already performs.
///
/// The measured length is not a parameter so that it cannot be used here by mistake.
fn rebase_ring(window_start: u64, window: &[u8]) -> ScrollbackBuffer {
    ScrollbackBuffer::restore_tail(
        BUFFER_LIMIT,
        window_start.saturating_add(window.len() as u64),
        window,
    )
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
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    /// The program a fixture records. It is never executed, but a row that encoded a
    /// POSIX path would not describe a Windows execution.
    fn fixture_program() -> OsString {
        if cfg!(windows) {
            OsString::from("cmd.exe")
        } else {
            OsString::from("/bin/sh")
        }
    }

    fn authority(directory: &Path) -> ExecutionAuthority {
        let program = fixture_program();
        let request = zuno_sandbox::PrepareRequest {
            program: program.clone(),
            arguments: Vec::new(),
            cwd: directory.to_owned(),
            environment: BTreeMap::new(),
            policy: zuno_sandbox::SandboxPolicy::new(
                directory,
                zuno_sandbox::SandboxMode::WorkspaceWrite,
                zuno_sandbox::NetworkAccess::Allowed,
            )
            .expect("fixture policy"),
        };
        PreparedCommand::from_backend(
            request,
            program,
            Vec::new(),
            &zuno_sandbox::SandboxCapabilities {
                backend: "test_direct".to_owned(),
                executable: None,
                read_only: true,
                workspace_write: true,
                danger_full_access: false,
                network_isolation: true,
            },
            vec![directory.to_owned()],
            Vec::new(),
        )
        .authority()
        .clone()
    }

    fn running_row(
        service: &BackgroundExecutionService,
        id: &BackgroundExecutionId,
        directory: &Path,
    ) -> BackgroundExecutionInfo {
        BackgroundExecutionInfo {
            id: id.clone(),
            session_id: "ses_claim".to_owned(),
            title: "fixture".to_owned(),
            command: "fixture".to_owned(),
            purpose: BackgroundExecutionPurpose::Command,
            cwd: directory.to_owned(),
            status: BackgroundExecutionStatus::Running,
            pid: Some(4242),
            exit_code: None,
            timed_out: false,
            time_created: 1,
            time_updated: 1,
            time_completed: None,
            error: None,
            output_file: service.output_path(id),
            status_file: service.status_path(id),
            authority: authority(directory),
        }
    }

    #[test]
    fn ids_reject_path_material() {
        assert!(BackgroundExecutionId::parse("../escape").is_err());
        assert!(BackgroundExecutionId::parse("bg_0123456789abcdef0123456789abcdef").is_ok());
    }

    /// The ordering inside `claim_capture` is the whole protection for an in-flight
    /// capture file: `reclaim_orphan` deletes any `<id>.output` whose claim it can take,
    /// so an `<id>.output` that exists before the claim is a live file a peer sweep is
    /// entitled to destroy. Creating the file first left exactly that window open.
    #[test]
    fn a_capture_file_is_never_created_before_its_ownership_claim() {
        let directory = tempfile::tempdir().expect("workspace");
        let service = BackgroundExecutionService::open(directory.path()).expect("service opens");
        let id = BackgroundExecutionId::parse("bg_1123456789abcdef0123456789abcdef").expect("id");
        let claim_path = service.claim_path(&id);
        let output_path = service.output_path(&id);
        // What a live peer that already owns this id looks like from here.
        let held = OwnershipClaim::acquire(&claim_path)
            .expect("the claim is takeable")
            .expect("nothing else holds it");

        let refused = service
            .claim_capture(&id)
            .expect_err("an id another process claims is not this one's to write");

        assert!(
            matches!(&refused, BackgroundExecutionError::Foreign(conflict) if conflict == &id),
            "{refused}"
        );
        assert!(
            !output_path.exists(),
            "a capture file must not exist while no claim for its id is held: that is the \
             exact state a peer's orphan sweep deletes"
        );
        drop(held);

        let (claim, output_file) = service.claim_capture(&id).expect("a free id is claimable");

        assert!(claim.is_some(), "the claim precedes the file");
        assert!(output_file.exists(), "the capture file follows the claim");
        assert!(
            matches!(OwnershipClaim::acquire(&claim_path), Ok(None)),
            "the capture file only became observable behind a held claim"
        );
    }

    /// A writer that cannot take a claim must say so in the row, because the reader's only
    /// other evidence is the missing lock file - which in that case means nothing.
    #[test]
    fn a_row_written_without_a_claim_records_that_no_claim_backs_it() {
        let directory = tempfile::tempdir().expect("workspace");
        let service = BackgroundExecutionService::open(directory.path()).expect("service opens");
        let id = BackgroundExecutionId::parse("bg_2223456789abcdef0123456789abcdef").expect("id");
        // A claim name that is present and refuses every process identically, which is the
        // one shape of claim failure a command may still run under: a directory cannot be
        // opened as a file on Unix (`EISDIR`) or on Windows (no backup semantics), so a
        // peer's own claim attempt fails here exactly as this one does and no peer can read
        // this id's lock file as proof of anything.
        std::fs::create_dir(service.claim_path(&id)).expect("blocked claim name");

        let (claim, output_file) = service
            .claim_capture(&id)
            .expect("an unclaimable filesystem still runs the command");

        assert!(claim.is_none(), "the claim could not be taken");
        assert!(
            output_file.exists(),
            "the command still gets its capture file"
        );
        let info = running_row(&service, &id, directory.path());
        persist_info(&info, claim.is_some()).expect("the row is published");
        let unclaimed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(service.status_path(&id)).expect("row"))
                .expect("row JSON");
        assert_eq!(
            unclaimed["claimed"],
            serde_json::json!(false),
            "a peer must be able to see that no claim backs this running row"
        );

        persist_info(&info, true).expect("the row is republished");
        let claimed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(service.status_path(&id)).expect("row"))
                .expect("row JSON");

        assert_eq!(claimed["claimed"], serde_json::json!(true));
        assert_eq!(claimed["format"], serde_json::json!(STATE_FORMAT));
    }

    /// The other shape of claim failure: the claim name is not there afterwards, so a peer
    /// *can* create and lock one, read "no owner", and delete this execution's capture file
    /// while it is being written. Nothing has run at that point, so the start must fail
    /// closed rather than run a command whose output a peer may destroy.
    #[test]
    fn a_claim_a_peer_could_still_take_fails_the_start_closed() {
        let directory = tempfile::tempdir().expect("workspace");
        // A state directory that is not there models the causes that leave no claim file
        // behind - an unusable `.zuno/background`, exhausted descriptors, a full disk, a
        // policy that blocks this one name - and does so identically on Unix and Windows.
        // `start` creates the root first, so reaching this through `start` needs the
        // directory to be unusable rather than merely absent.
        let root = directory.path().join("missing");
        let service = BackgroundExecutionService::open(&root).expect("service opens");
        let id = BackgroundExecutionId::parse("bg_4423456789abcdef0123456789abcdef").expect("id");

        let refused = service
            .claim_capture(&id)
            .expect_err("an unprotectable capture file is not worth starting");

        assert!(
            matches!(&refused, BackgroundExecutionError::State { path, .. } if path == &service.claim_path(&id)),
            "the refusal names the claim that could not be resolved: {refused}"
        );
        assert!(
            !service.output_path(&id).exists(),
            "no capture file may be left behind by a start that failed closed"
        );
        assert!(
            !service.status_path(&id).exists(),
            "and no row may claim the command is running"
        );
    }

    /// A row from a build that had no claim protocol carries no marker at all. Absent is
    /// its own state: neither "a claim backed this" (which would settle a released build's
    /// live command) nor "no claim backed this" (which would make every row interrupted by
    /// an upgrade permanently unsettleable).
    #[test]
    fn a_row_from_before_the_claim_protocol_is_its_own_evidence() {
        let directory = tempfile::tempdir().expect("workspace");
        let service = BackgroundExecutionService::open(directory.path()).expect("service opens");
        let id = BackgroundExecutionId::parse("bg_3323456789abcdef0123456789abcdef").expect("id");
        let legacy: PersistedExecution = serde_json::from_value(serde_json::json!({
            "format": STATE_FORMAT,
            "info": serde_json::to_value(running_row(&service, &id, directory.path()))
                .expect("info JSON"),
        }))
        .expect("a format-3 row without the marker still decodes");

        assert_eq!(legacy.claimed, None, "absent is its own state, not `false`");
        assert_eq!(legacy.claim_evidence(), ClaimEvidence::PreProtocol);
        assert_ne!(
            legacy.claim_evidence(),
            ClaimEvidence::Backed,
            "an absent marker must not be merged with a claim that was actually held"
        );
    }

    /// A marker that is present decides on its own, on every platform: the claim is the
    /// proof, so no process table is consulted for either answer.
    #[test]
    fn a_recorded_claim_marker_decides_without_probing_anything() {
        assert!(settlable(ClaimEvidence::Backed, Some(std::process::id())).is_ok());
        assert!(matches!(
            settlable(ClaimEvidence::Unbacked, None),
            Err(Unsettled::Unclaimed)
        ));
    }

    /// The gate the released format needs. `claimed` is optional, so the row Zuno 0.6.6
    /// wrote for a command it is running *right now* carries no marker - and settling that
    /// row tells the model a live command never reported, then lets retention delete the
    /// output it is still writing. This process's own pid is the cheapest process that is
    /// certainly alive; a spawned and reaped child is one that is certainly gone.
    #[cfg(unix)]
    #[test]
    fn a_pre_protocol_row_is_settled_only_when_its_recorded_process_is_gone() {
        let mine = std::process::id();

        assert_eq!(recorded_process(Some(mine)), RecordedProcess::Alive);
        assert!(
            matches!(
                settlable(ClaimEvidence::PreProtocol, Some(mine)),
                Err(Unsettled::RecordedProcessAlive(pid)) if pid == mine
            ),
            "a live recorded process contradicts `the owner is gone`"
        );
        // A pid that cannot name one process is not resolvable, and an unresolvable
        // question fails closed rather than settling. `0` and a value wider than `pid_t`
        // matter because `kill` reads them as this process's own group, not as one process.
        for unresolvable in [None, Some(0), Some(u32::MAX)] {
            assert_eq!(
                recorded_process(unresolvable),
                RecordedProcess::Unresolvable,
                "{unresolvable:?}"
            );
            assert!(matches!(
                settlable(ClaimEvidence::PreProtocol, unresolvable),
                Err(Unsettled::RecordedProcessUnresolvable)
            ));
        }
        // And the ordinary upgrade case still converges: the recorded process is gone, so
        // the row is reconciled exactly as the released build reconciled it.
        let gone = reaped_pid();
        assert_eq!(recorded_process(Some(gone)), RecordedProcess::Gone);
        assert!(settlable(ClaimEvidence::PreProtocol, Some(gone)).is_ok());
    }

    /// Where no process table is reachable without `unsafe` or a subprocess, the answer is
    /// "not resolvable" for every pid and the row is left alone rather than settled.
    #[cfg(not(unix))]
    #[test]
    fn a_pre_protocol_row_is_never_settled_where_liveness_cannot_be_probed() {
        assert_eq!(
            recorded_process(Some(std::process::id())),
            RecordedProcess::Unresolvable
        );
        assert!(matches!(
            settlable(ClaimEvidence::PreProtocol, Some(std::process::id())),
            Err(Unsettled::RecordedProcessUnresolvable)
        ));
    }

    /// The window `read_window` hands back is shorter than the length its caller measured
    /// whenever the owner appended in between. This is that window.
    #[test]
    fn a_rebased_foreign_ring_counts_only_the_bytes_it_actually_read() {
        // What the caller measured, and where its window therefore started.
        let measured_length = 4096u64;
        let start = measured_length - 64;
        // What the read returned: the owner appended during it, so the window is no
        // longer the end of the file and a split code point came off its tail.
        let window = vec![b'x'; 61];

        let ring = rebase_ring(start, &window);

        assert_eq!(
            ring.total_written(),
            start + window.len() as u64,
            "the end cursor must count the bytes this ring holds, not the length measured \
             before they were read"
        );
        assert_ne!(
            ring.total_written(),
            measured_length,
            "publishing the measured length over a trimmed window is the defect: every \
             later cursor in this ring would be 3 bytes past the bytes it can serve"
        );
        assert_eq!(
            ring.end_cursor() - ring.start_cursor(),
            window.len() as u64,
            "cursor space and retained bytes have to agree, or a replay reads past the end"
        );
    }

    /// A pid no process can be using: spawned, waited for, and therefore reaped.
    #[cfg(unix)]
    fn reaped_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("a shell runs");
        let pid = child.id();
        let status = child.wait().expect("the child is reaped");
        assert!(status.success());
        pid
    }
}
