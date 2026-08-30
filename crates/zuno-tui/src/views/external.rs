//! External `$EDITOR` and clipboard, both behind seams.
//!
//! # Why these are traits and not functions
//!
//! Both spawn a process, and one of them takes over the terminal. Neither may run in
//! a test: a real `$EDITOR` would block on a human, and a real clipboard read depends
//! on which of `wl-paste`/`xclip`/`pbpaste` happens to exist on the machine running
//! the suite. So [`ExternalEditor`] and [`Clipboard`] are traits with a real
//! implementation and a recording double, exactly as todo 73 did for the terminal
//! lifecycle.
//!
//! # Opening an editor is a terminal lease, not a suspend
//!
//! Upstream calls `renderer.suspend()` around the child process
//! (`packages/tui/src/editor.ts:32-53`). This crate already has the right mechanism
//! for that — todo 97's [`zuno_engine::terminal_lease::TerminalBroker`], driven by
//! todo 73's [`crate::app::TerminalLeaseOwner`] — so [`EditorRequest`] carries the
//! lease reason and the caller acquires a lease before invoking the editor. That
//! keeps one exclusion policy in the process instead of two that can disagree, which
//! is what would deadlock against a plugin's OAuth prompt.
//!
//! # The clipboard's fallback ladder is data, not control flow
//!
//! `copy_command` (`packages/tui/src/clipboard.ts:75-91`) picks a program by platform
//! and by what is installed. It is ported as a pure function over
//! `(platform, wayland, has)` so the whole ladder is testable without any of those
//! programs being present — which is the only way to test it at all.
//!
//! # OSC 52 wins; the native tool is the fallback
//!
//! OSC 52 is tried first because, over SSH or inside tmux, the native tool copies into
//! the remote machine's clipboard rather than the one the user is looking at. Once the
//! terminal accepts and flushes that sequence, one successful delivery is enough and
//! spawning another process would add latency and a failure surface without improving
//! the result. The native ladder remains available when no terminal sink exists or its
//! write fails.
//!
//! # Where the OSC 52 bytes go, and why that cannot corrupt a frame
//!
//! [`TerminalSink`] writes straight to stdout, which ratatui also owns while the
//! alternate screen is up. That is safe here, and provably so rather than by
//! convention, because of how [`crate::app::App`]'s loop is built:
//!
//! * Every production frame is painted by `UiState::draw` (`app.rs:407-409`), which
//!   is the only route to [`crate::app::CrosstermDrawTarget::draw`]
//!   (`app.rs:386-390`). All four of its callers — `app.rs:500`, `:620`, `:680`,
//!   `:696` — take the `Mutex<UiState>` guard before calling it.
//! * A copy runs inside `SessionScreen::handle_action`, which
//!   [`crate::keybind::KeyDispatcher`] calls from `Component::handle_event`, which
//!   `App::handle_terminal` invokes at `app.rs:678` — *while holding that same
//!   guard*, taken at `app.rs:671`.
//!
//! So the write lands strictly between two frames, on the thread that draws them. No
//! other thread can be inside `Terminal::draw` concurrently, because
//! [`crate::app::TerminalLeaseOwner::reclaim_terminal`] — the only other painter —
//! would block on the guard at `app.rs:499`. The frame for this keystroke is painted
//! afterwards at `app.rs:680`, and ratatui flushes at the end of every `draw`, so
//! stdout is never left mid-sequence when the emit begins.
//!
//! The payload is inert besides: OSC 52 is an operating-system command a terminal
//! consumes without moving the cursor or touching a cell, so even a reader who
//! doubted the argument above would find no cell on screen disturbed.
//!
//! Routing the bytes through [`crate::app::DrawTarget`] instead was rejected, and not
//! for taste: [`Clipboard::write`] takes `&self` on a collaborator the screen owns,
//! and the screen holds no handle on the draw target. Giving it one means either
//! widening that trait — `app.rs`, outside this surface — or handing the view layer
//! the terminal it deliberately does not own. Queueing the sequence for the loop to
//! flush after the next frame needs the same new seam, and buys nothing over a write
//! that is already frame-safe.
//!
//! # A clipboard exists because a human is at a terminal
//!
//! [`SystemClipboard::host`] resolves *both* mechanisms once, at construction, and
//! yields a clipboard with neither when stdout is not a terminal. Two independent
//! reasons, either sufficient: an OSC 52 sequence written to a pipe or a redirected
//! file reaches no terminal and corrupts that stream instead, and shelling out to
//! `xclip` from a process nobody is watching would mutate the clipboard of whichever
//! X session happens to be around — during a test run, the author's own.

use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Notify;
use zuno_engine::terminal_lease::TerminalLeaseCleanup;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[path = "external_tests.rs"]
mod tests;

/// Environment variables consulted for the editor, in order
/// (`packages/tui/src/editor.ts:27`).
pub const EDITOR_VARIABLES: [&str; 2] = ["VISUAL", "EDITOR"];

/// A request to edit text in an external editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorRequest {
    /// The text to open with.
    pub value: String,
    /// The working directory for the child, when one is wanted.
    pub cwd: Option<String>,
}

impl EditorRequest {
    /// A request carrying `value`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            cwd: None,
        }
    }

    /// The lease reason a caller should acquire before invoking the editor.
    ///
    /// Named here rather than at the call site so every path that opens an editor
    /// declares the same reason with the same spelling. `LeaseReason::plugin` names
    /// the culprit in a forced-reclaim diagnostic, and for this path the culprit is
    /// the TUI itself, so it says so instead of borrowing a plugin's name.
    #[must_use]
    pub fn lease_reason(&self) -> zuno_engine::terminal_lease::LeaseReason {
        zuno_engine::terminal_lease::LeaseReason::new("tui", "external editor")
    }
}

/// An external editor could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ExternalError {
    /// Neither `VISUAL` nor `EDITOR` is set.
    #[error("no external editor is configured; set $VISUAL or $EDITOR")]
    NoEditor,
    /// The child failed to start, or exited non-zero.
    #[error("the external editor failed: {0}")]
    Failed(String),
    /// The editor was terminated because its terminal lease ended.
    #[error("the external editor was cancelled")]
    Cancelled,
    /// The scratch file could not be written or read.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// No clipboard mechanism is available on this host.
    #[error("no clipboard program is available on this host")]
    NoClipboard,
}

/// The `$EDITOR` round trip.
#[async_trait]
pub trait ExternalEditor: Send + Sync {
    /// Open an editor on `request.value` and return the edited text.
    ///
    /// `None` means the user made no change worth taking — an empty file, which
    /// upstream also treats as "no result" (`editor.ts:48`) rather than as an
    /// instruction to clear the prompt.
    ///
    /// # Errors
    ///
    /// [`ExternalError`] when no editor is configured or the child fails.
    async fn edit(
        &self,
        request: &EditorRequest,
        cancellation: EditorCancellation,
    ) -> Result<Option<String>, ExternalError>;
}

const EDITOR_CHILD_POLL_INTERVAL: Duration = Duration::from_millis(5);
const EDITOR_CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// One spawned editor process as the terminal-lease cleanup path controls it.
pub trait EditorProcess: Send {
    /// Poll and reap the process when it has exited.
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;

    /// Begin termination without waiting for completion.
    fn request_termination(&mut self) -> io::Result<()>;
}

/// Platform boundary for spawning an editor inside process-tree containment.
pub trait EditorProcessLauncher: Send + Sync {
    /// Spawn `invocation`, inheriting terminal stdio and optionally changing directory.
    fn spawn(
        &self,
        invocation: &EditorInvocation,
        cwd: Option<&str>,
    ) -> io::Result<Box<dyn EditorProcess>>;
}

struct DirectEditorProcess(tokio::process::Child);

impl EditorProcess for DirectEditorProcess {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }

    fn request_termination(&mut self) -> io::Result<()> {
        self.0.start_kill()
    }
}

#[derive(Debug, Default)]
struct DirectEditorLauncher;

impl EditorProcessLauncher for DirectEditorLauncher {
    fn spawn(
        &self,
        invocation: &EditorInvocation,
        cwd: Option<&str>,
    ) -> io::Result<Box<dyn EditorProcess>> {
        let mut command = tokio::process::Command::new(&invocation.program);
        command.args(&invocation.args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        command
            .spawn()
            .map(|child| Box::new(DirectEditorProcess(child)) as Box<dyn EditorProcess>)
    }
}

enum EditorProcessSlot {
    Empty,
    Spawning,
    Running(Box<dyn EditorProcess>),
}

struct EditorInvocationProcess {
    cancelled: AtomicBool,
    child: Mutex<EditorProcessSlot>,
    spawned: Condvar,
    changed: Notify,
}

/// Shared cancellation and child-reaping state for one editor invocation.
#[derive(Clone)]
pub struct EditorCancellation {
    process: Arc<EditorInvocationProcess>,
}

impl EditorCancellation {
    /// Creates an uncancelled editor invocation with no child yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            process: Arc::new(EditorInvocationProcess {
                cancelled: AtomicBool::new(false),
                child: Mutex::new(EditorProcessSlot::Empty),
                spawned: Condvar::new(),
                changed: Notify::new(),
            }),
        }
    }

    /// Requests cancellation. Lease cleanup performs the synchronous kill and reap.
    pub fn cancel(&self) {
        self.process.cancelled.store(true, Ordering::SeqCst);
        self.process.changed.notify_waiters();
    }

    /// Resolves once cancellation has been requested.
    pub async fn cancelled(&self) {
        loop {
            if self.process.cancelled.load(Ordering::SeqCst) {
                return;
            }
            self.process.changed.notified().await;
        }
    }

    fn spawn(
        &self,
        launcher: &dyn EditorProcessLauncher,
        invocation: &EditorInvocation,
        cwd: Option<&str>,
    ) -> io::Result<()> {
        {
            let mut child = locked(&self.process.child);
            if self.process.cancelled.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "the editor invocation was cancelled before spawn",
                ));
            }
            if !matches!(*child, EditorProcessSlot::Empty) {
                return Err(io::Error::other(
                    "the editor invocation already has a child",
                ));
            }
            *child = EditorProcessSlot::Spawning;
        }
        let spawned = launcher.spawn(invocation, cwd);
        let mut child = locked(&self.process.child);
        match spawned {
            Ok(running) => {
                *child = EditorProcessSlot::Running(running);
                self.process.spawned.notify_all();
                Ok(())
            }
            Err(error) => {
                *child = EditorProcessSlot::Empty;
                self.process.spawned.notify_all();
                Err(error)
            }
        }
    }

    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        let mut child = locked(&self.process.child);
        let EditorProcessSlot::Running(running) = &mut *child else {
            return Err(io::Error::other("the editor child is not running"));
        };
        let status = running.try_wait()?;
        if status.is_some() {
            *child = EditorProcessSlot::Empty;
        }
        Ok(status)
    }

    fn terminate_and_reap(&self) -> Result<(), String> {
        self.cancel();
        let started = Instant::now();
        let mut kill_started = false;
        let mut child = locked(&self.process.child);
        loop {
            match &mut *child {
                EditorProcessSlot::Empty => return Ok(()),
                EditorProcessSlot::Spawning => {
                    child = self
                        .process
                        .spawned
                        .wait(child)
                        .unwrap_or_else(PoisonError::into_inner);
                    continue;
                }
                EditorProcessSlot::Running(running) => match running.try_wait() {
                    Ok(Some(_status)) => {
                        *child = EditorProcessSlot::Empty;
                        return Ok(());
                    }
                    Ok(None) if !kill_started => {
                        running
                            .request_termination()
                            .map_err(|error| format!("killing the editor failed: {error}"))?;
                        kill_started = true;
                    }
                    Ok(None) => {}
                    Err(error) => return Err(format!("reaping the editor failed: {error}")),
                },
            }
            if started.elapsed() >= EDITOR_CHILD_REAP_TIMEOUT {
                return Err(format!(
                    "the editor did not exit within {} ms after kill",
                    EDITOR_CHILD_REAP_TIMEOUT.as_millis()
                ));
            }
            drop(child);
            thread::sleep(EDITOR_CHILD_POLL_INTERVAL);
            child = locked(&self.process.child);
        }
    }
}

impl Default for EditorCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalLeaseCleanup for EditorCancellation {
    fn before_reclaim(&self) -> Result<(), String> {
        self.terminate_and_reap()
    }
}

/// The command an editor invocation would run, and the temporary file it would use.
///
/// Split out from the spawn so the argument construction — the part with the quoting
/// and the `.md` suffix that gives the editor its syntax mode — is testable without a
/// child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorInvocation {
    /// The program.
    pub program: String,
    /// Its arguments, ending with the scratch file.
    pub args: Vec<String>,
}

/// Build the invocation for `spec` (the value of `$VISUAL`/`$EDITOR`) over `file`.
///
/// The spec is split on spaces because a user's `EDITOR` is frequently
/// `code --wait` or `nvim -u NONE`; treating the whole string as a program name
/// would fail for those with a confusing "not found".
#[must_use]
pub fn invocation(spec: &str, file: &str) -> Option<EditorInvocation> {
    let mut parts = spec.split_whitespace();
    let program = parts.next()?.to_owned();
    let mut args = parts.map(str::to_owned).collect::<Vec<_>>();
    args.push(file.to_owned());
    Some(EditorInvocation { program, args })
}

/// The editor spec from the environment, `VISUAL` first.
#[must_use]
pub fn editor_spec(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    EDITOR_VARIABLES
        .iter()
        .find_map(|name| lookup(name).filter(|value| !value.trim().is_empty()))
}

/// The host's `$VISUAL` or `$EDITOR` implementation.
#[derive(Clone)]
pub struct SystemEditor {
    spec: Option<String>,
    launcher: Arc<dyn EditorProcessLauncher>,
}

impl SystemEditor {
    /// Resolve the editor command from the process environment.
    #[must_use]
    pub fn host() -> Self {
        Self::host_with_launcher(Arc::new(DirectEditorLauncher))
    }

    /// Resolve the host editor and launch it through `launcher`.
    #[must_use]
    pub fn host_with_launcher(launcher: Arc<dyn EditorProcessLauncher>) -> Self {
        Self {
            spec: editor_spec(|name| std::env::var(name).ok()),
            launcher,
        }
    }

    /// Uses an explicit editor specification instead of consulting the environment.
    #[must_use]
    pub fn configured(spec: impl Into<String>) -> Self {
        Self::configured_with_launcher(spec, Arc::new(DirectEditorLauncher))
    }

    /// Uses an explicit editor specification and process launcher.
    #[must_use]
    pub fn configured_with_launcher(
        spec: impl Into<String>,
        launcher: Arc<dyn EditorProcessLauncher>,
    ) -> Self {
        Self {
            spec: Some(spec.into()),
            launcher,
        }
    }
}

#[async_trait]
impl ExternalEditor for SystemEditor {
    async fn edit(
        &self,
        request: &EditorRequest,
        cancellation: EditorCancellation,
    ) -> Result<Option<String>, ExternalError> {
        let spec = self.spec.as_deref().ok_or(ExternalError::NoEditor)?;
        let scratch = ScratchFile::create(&request.value)?;
        let path = scratch.path().to_string_lossy();
        let invocation = invocation(spec, &path).ok_or(ExternalError::NoEditor)?;
        cancellation
            .spawn(self.launcher.as_ref(), &invocation, request.cwd.as_deref())
            .map_err(|error| ExternalError::Failed(error.to_string()))?;
        let status = loop {
            if let Some(status) = cancellation
                .try_wait()
                .map_err(|error| ExternalError::Failed(error.to_string()))?
            {
                break status;
            }
            tokio::select! {
                () = tokio::time::sleep(EDITOR_CHILD_POLL_INTERVAL) => {}
                () = cancellation.cancelled() => {
                    let cleanup = cancellation.clone();
                    tokio::task::spawn_blocking(move || cleanup.terminate_and_reap())
                        .await
                        .map_err(|error| ExternalError::Failed(error.to_string()))?
                        .map_err(ExternalError::Failed)?;
                    return Err(ExternalError::Cancelled);
                }
            }
        };
        if !status.success() {
            return Err(ExternalError::Failed(format!(
                "{} exited with {status}",
                invocation.program
            )));
        }
        let edited = std::fs::read_to_string(scratch.path())?;
        Ok((!edited.trim().is_empty()).then_some(edited))
    }
}

static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct ScratchFile {
    path: PathBuf,
}

impl ScratchFile {
    fn create(value: &str) -> io::Result<Self> {
        for _ in 0..32 {
            let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("zuno-editor-{}-{sequence}.md", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(value.as_bytes())?;
                    file.flush()?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate an external-editor scratch file",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _removed = std::fs::remove_file(&self.path);
    }
}

/// A recording double.
///
/// Not `#[cfg(test)]`: the CLI's `--no-editor` mode and the ACP host both need an
/// editor that answers without a terminal, and one double is better than three.
#[derive(Debug, Default)]
pub struct ScriptedEditor {
    /// What [`ExternalEditor::edit`] returns.
    pub result: Option<String>,
    /// Whether it fails instead.
    pub fail: bool,
    requests: std::sync::Mutex<Vec<EditorRequest>>,
}

impl ScriptedEditor {
    /// An editor that returns `result`.
    #[must_use]
    pub fn returning(result: impl Into<String>) -> Self {
        Self {
            result: Some(result.into()),
            fail: false,
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// An editor that fails.
    #[must_use]
    pub fn failing() -> Self {
        Self {
            result: None,
            fail: true,
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// What it was asked to edit.
    #[must_use]
    pub fn requests(&self) -> Vec<EditorRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ExternalEditor for ScriptedEditor {
    async fn edit(
        &self,
        request: &EditorRequest,
        _cancellation: EditorCancellation,
    ) -> Result<Option<String>, ExternalError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        if self.fail {
            return Err(ExternalError::Failed(String::from("scripted failure")));
        }
        Ok(self.result.clone())
    }
}

/// What the clipboard holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardContent {
    /// The payload: text, or base64 for an image.
    pub data: String,
    /// Its MIME type.
    pub mime: String,
}

impl ClipboardContent {
    /// Plain text.
    #[must_use]
    pub fn text(data: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            mime: String::from("text/plain"),
        }
    }

    /// Whether this is an image, which the prompt turns into an attachment rather
    /// than into typed characters.
    #[must_use]
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// The clipboard round trip.
pub trait Clipboard: Send + Sync {
    /// Read the clipboard.
    ///
    /// # Errors
    ///
    /// [`ExternalError::NoClipboard`] when the host offers no mechanism.
    fn read(&self) -> Result<Option<ClipboardContent>, ExternalError>;

    /// Write text to the clipboard.
    ///
    /// # Errors
    ///
    /// [`ExternalError`] when every mechanism failed.
    fn write(&self, text: &str) -> Result<(), ExternalError>;
}

/// An in-memory clipboard.
#[derive(Debug, Default)]
pub struct MemoryClipboard {
    content: std::sync::Mutex<Option<ClipboardContent>>,
}

impl MemoryClipboard {
    /// A clipboard holding `content`.
    #[must_use]
    pub fn holding(content: ClipboardContent) -> Self {
        Self {
            content: std::sync::Mutex::new(Some(content)),
        }
    }
}

impl Clipboard for MemoryClipboard {
    fn read(&self) -> Result<Option<ClipboardContent>, ExternalError> {
        Ok(self
            .content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    fn write(&self, text: &str) -> Result<(), ExternalError> {
        *self
            .content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(ClipboardContent::text(text));
        Ok(())
    }
}

/// What a [`SystemClipboard`]'s mechanisms did, in the order they were tried.
///
/// Shared by [`RecordingSink`] and [`ScriptedRunner`] rather than kept one per double,
/// because the property worth asserting is an *ordering* — OSC 52 before the native
/// program — and two separate logs can only ever show that both of them ran.
#[derive(Debug, Default)]
pub struct CopyLog {
    entries: Mutex<Vec<String>>,
}

impl CopyLog {
    /// A shared, empty log.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Every entry, oldest first.
    #[must_use]
    pub fn entries(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn push(&self, entry: String) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
    }
}

/// Where an OSC 52 sequence is written.
///
/// A seam and not a bare `write!` for the reason [`Clipboard`] is one: the production
/// destination is the terminal ratatui is painting, which no test has.
pub trait Osc52Sink: Send + Sync {
    /// Emit `sequence` verbatim, then flush.
    ///
    /// # Errors
    ///
    /// [`ExternalError`] when the destination refused the write.
    fn emit(&self, sequence: &str) -> Result<(), ExternalError>;
}

/// The real sink: stdout, flushed immediately.
///
/// See the module header for why writing here cannot interleave with a frame.
#[derive(Debug, Default)]
pub struct TerminalSink;

impl Osc52Sink for TerminalSink {
    fn emit(&self, sequence: &str) -> Result<(), ExternalError> {
        let mut output = io::stdout();
        output.write_all(sequence.as_bytes())?;
        // Flushed here rather than left for ratatui's next frame to carry: that frame
        // arrives only when something else asks for a repaint, and a copy the user has
        // to wait on is one they will read as having failed.
        output.flush()?;
        Ok(())
    }
}

/// A recording sink.
///
/// Not `#[cfg(test)]`, for the reason [`ScriptedEditor`] and [`MemoryClipboard`] are
/// not: a host that wants a clipboard without a terminal needs one too.
pub struct RecordingSink {
    log: Arc<CopyLog>,
    fail: bool,
}

impl RecordingSink {
    /// A sink that records into `log` and succeeds.
    #[must_use]
    pub const fn new(log: Arc<CopyLog>) -> Self {
        Self { log, fail: false }
    }

    /// A sink that records into `log` and then fails.
    ///
    /// It records *before* failing so a test can still prove the attempt was made in
    /// the right order — a sink that recorded nothing on failure could not tell
    /// "tried OSC 52 first and it failed" from "never tried OSC 52".
    #[must_use]
    pub const fn failing(log: Arc<CopyLog>) -> Self {
        Self { log, fail: true }
    }
}

impl Osc52Sink for RecordingSink {
    fn emit(&self, sequence: &str) -> Result<(), ExternalError> {
        self.log.push(format!("osc52:{sequence}"));
        if self.fail {
            return Err(ExternalError::Failed(String::from("no terminal attached")));
        }
        Ok(())
    }
}

/// Runs one clipboard program with the text on its stdin.
///
/// Separated from [`SystemClipboard`] so the ladder can be exercised without any of
/// `wl-copy`, `xclip`, `xsel` or `powershell.exe` being installed — and, more to the
/// point, without a test mutating the clipboard of whoever is running the suite.
pub trait CommandRunner: Send + Sync {
    /// Run `argv`, feeding `input` to the child's stdin.
    ///
    /// # Errors
    ///
    /// [`ExternalError::Failed`] when the program is missing, the write to its stdin
    /// fails, it exits non-zero, or it exceeds the runner's deadline.
    fn run(&self, argv: &[String], input: &str) -> Result<(), ExternalError>;
}

/// Maximum time the component path waits for a native clipboard worker result.
///
/// Fifty milliseconds is long enough for the tiny local helpers in the ladder but
/// short enough to present as one delayed keypress rather than a frozen TUI. Every child
/// operation happens on a detached worker, so a failed kill, an inherited stdin pipe or
/// a non-returning wait can outlive this deadline without retaining the UI-state lock or
/// joining application shutdown. OSC 52 handles the normal interactive path without
/// submitting any work.
const CLIPBOARD_COMMAND_TIMEOUT: Duration = Duration::from_millis(50);
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(1);

trait ClipboardChild {
    fn take_stdin(&mut self) -> Option<Box<dyn io::Write + Send>>;
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
}

impl ClipboardChild for std::process::Child {
    fn take_stdin(&mut self) -> Option<Box<dyn io::Write + Send>> {
        self.stdin
            .take()
            .map(|stdin| Box::new(stdin) as Box<dyn io::Write + Send>)
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.try_wait()
    }

    fn kill(&mut self) -> io::Result<()> {
        self.kill()
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait()
    }
}

/// The real runner: a child process with a piped stdin.
///
/// This runs only on [`NativeClipboardWorker`]'s detached thread. The cleanup below is
/// deliberately thorough but is not itself the component-path bound: an operating-system
/// wait can ignore every deadline. The worker result handoff supplies that hard boundary.
#[derive(Debug, Default)]
pub struct ChildProcessRunner;

impl CommandRunner for ChildProcessRunner {
    fn run(&self, argv: &[String], input: &str) -> Result<(), ExternalError> {
        let Some((program, arguments)) = argv.split_first() else {
            return Err(ExternalError::Failed(String::from(
                "the clipboard command is empty",
            )));
        };
        let mut child = Command::new(program)
            .args(arguments)
            .stdin(Stdio::piped())
            // Discarded rather than inherited: these programs say nothing useful on
            // success, and anything they did print would land on the alternate screen
            // as unpainted characters the next frame would not know to clear.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                ExternalError::Failed(format!("{program} could not start: {error}"))
            })?;

        run_clipboard_child(program, &mut child, input, CLIPBOARD_COMMAND_TIMEOUT)
    }
}

fn run_clipboard_child(
    program: &str,
    child: &mut dyn ClipboardChild,
    input: &str,
    timeout: Duration,
) -> Result<(), ExternalError> {
    let started = Instant::now();
    let Some(stdin) = child.take_stdin() else {
        let cleanup = terminate_and_reap(child);
        return Err(ExternalError::Failed(with_cleanup(
            format!("{program} exposed no stdin to write to"),
            cleanup,
        )));
    };

    // The writer has to be independently interruptible: a helper that never consumes
    // stdin can fill the pipe before the parent ever reaches `try_wait`. The child stays
    // on this thread so a deadline can force-kill it, which closes the pipe and releases
    // the writer. Stdin is still taken, fully written, and dropped before every `wait`;
    // that ordering is load-bearing because the clipboard helpers read until EOF.
    let payload = input.as_bytes().to_vec();
    let writer = match thread::Builder::new()
        .name(String::from("zuno-clipboard-stdin"))
        .spawn(move || {
            let mut stdin = stdin;
            let result = stdin.write_all(&payload).and_then(|()| stdin.flush());
            drop(stdin);
            result
        }) {
        Ok(writer) => writer,
        Err(error) => {
            let cleanup = terminate_and_reap(child);
            return Err(ExternalError::Failed(with_cleanup(
                format!("starting the {program} stdin writer failed: {error}"),
                cleanup,
            )));
        }
    };
    let mut exited = None;

    while !writer.is_finished() {
        if exited.is_none() {
            match child.try_wait() {
                Ok(status) => exited = status,
                Err(error) => {
                    return Err(abort_child_with_writer(
                        child,
                        writer,
                        format!("checking {program} failed: {error}"),
                    ));
                }
            }
        }
        if started.elapsed() >= timeout {
            return Err(timeout_child(program, child, writer, timeout));
        }
        thread::sleep(CLIPBOARD_POLL_INTERVAL);
    }

    let write = match join_writer(writer) {
        Ok(write) => write,
        Err(error) => {
            let cleanup = if exited.is_some() {
                None
            } else {
                terminate_and_reap(child)
            };
            return Err(ExternalError::Failed(with_cleanup(
                error.to_owned(),
                cleanup,
            )));
        }
    };
    if let Err(error) = write {
        let cleanup = if exited.is_some() {
            None
        } else {
            terminate_and_reap(child)
        };
        return Err(ExternalError::Failed(with_cleanup(
            format!("writing to {program} failed: {error}"),
            cleanup,
        )));
    }

    if let Some(status) = exited {
        return status_result(program, status);
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status_result(program, status),
            Ok(None) => {}
            Err(error) => {
                let cleanup = terminate_and_reap(child);
                return Err(ExternalError::Failed(with_cleanup(
                    format!("checking {program} failed: {error}"),
                    cleanup,
                )));
            }
        }
        if started.elapsed() >= timeout {
            return Err(timeout_child_without_writer(program, child, timeout));
        }
        thread::sleep(CLIPBOARD_POLL_INTERVAL);
    }
}

fn join_writer(writer: JoinHandle<io::Result<()>>) -> Result<io::Result<()>, &'static str> {
    writer
        .join()
        .map_err(|_| "the clipboard stdin writer panicked")
}

fn timeout_child(
    program: &str,
    child: &mut dyn ClipboardChild,
    writer: JoinHandle<io::Result<()>>,
    timeout: Duration,
) -> ExternalError {
    abort_child_with_writer(
        child,
        writer,
        format!("{program} timed out after {} ms", timeout.as_millis()),
    )
}

fn timeout_child_without_writer(
    program: &str,
    child: &mut dyn ClipboardChild,
    timeout: Duration,
) -> ExternalError {
    let kill_error = child.kill().err();
    let wait_error = child.wait().err();
    ExternalError::Failed(cleanup_message(
        format!("{program} timed out after {} ms", timeout.as_millis()),
        kill_error,
        None,
        wait_error,
    ))
}

fn abort_child_with_writer(
    child: &mut dyn ClipboardChild,
    writer: JoinHandle<io::Result<()>>,
    message: String,
) -> ExternalError {
    let kill_error = child.kill().err();
    let writer_error = join_writer(writer).err();
    let wait_error = child.wait().err();
    ExternalError::Failed(cleanup_message(
        message,
        kill_error,
        writer_error,
        wait_error,
    ))
}

fn cleanup_message(
    mut message: String,
    kill_error: Option<io::Error>,
    writer_error: Option<&str>,
    wait_error: Option<io::Error>,
) -> String {
    if kill_error.is_none() && wait_error.is_none() {
        message.push_str("; child was killed and reaped");
    }
    if let Some(error) = kill_error {
        message.push_str(&format!("; killing it failed: {error}"));
    }
    if let Some(error) = writer_error {
        message.push_str(&format!("; {error}"));
    }
    if let Some(error) = wait_error {
        message.push_str(&format!("; reaping it failed: {error}"));
    }
    message
}

fn terminate_and_reap(child: &mut dyn ClipboardChild) -> Option<String> {
    let kill_error = child.kill().err();
    let wait_error = child.wait().err();
    match (kill_error, wait_error) {
        (None, None) => None,
        (Some(kill), None) => Some(format!("killing the child failed: {kill}")),
        (None, Some(wait)) => Some(format!("reaping the child failed: {wait}")),
        (Some(kill), Some(wait)) => Some(format!(
            "killing the child failed: {kill}; reaping it failed: {wait}"
        )),
    }
}

fn with_cleanup(message: String, cleanup: Option<String>) -> String {
    match cleanup {
        Some(cleanup) => format!("{message}; {cleanup}"),
        None => message,
    }
}

fn status_result(program: &str, status: ExitStatus) -> Result<(), ExternalError> {
    if status.success() {
        Ok(())
    } else {
        Err(ExternalError::Failed(format!(
            "{program} exited unsuccessfully: {status}"
        )))
    }
}

/// A recording runner.
pub struct ScriptedRunner {
    log: Arc<CopyLog>,
    fail: bool,
}

impl ScriptedRunner {
    /// A runner that records into `log` and succeeds.
    #[must_use]
    pub const fn new(log: Arc<CopyLog>) -> Self {
        Self { log, fail: false }
    }

    /// A runner that records into `log` and then fails, as a missing program would.
    #[must_use]
    pub const fn failing(log: Arc<CopyLog>) -> Self {
        Self { log, fail: true }
    }
}

impl CommandRunner for ScriptedRunner {
    fn run(&self, argv: &[String], input: &str) -> Result<(), ExternalError> {
        self.log.push(format!("run:{}:{input}", argv.join(" ")));
        if self.fail {
            return Err(ExternalError::Failed(format!(
                "{} could not start: no such file or directory",
                argv.first().map_or("", String::as_str)
            )));
        }
        Ok(())
    }
}

struct ClipboardJob {
    argv: Vec<String>,
    input: String,
    outcome: Arc<ClipboardOutcome>,
}

#[derive(Default)]
struct ClipboardOutcome {
    result: Mutex<Option<Result<(), ExternalError>>>,
    ready: Condvar,
}

impl ClipboardOutcome {
    fn finish(&self, result: Result<(), ExternalError>) {
        *locked(&self.result) = Some(result);
        self.ready.notify_one();
    }

    fn wait(&self, timeout: Duration) -> Option<Result<(), ExternalError>> {
        let result = locked(&self.result);
        let (mut result, _) = self
            .ready
            .wait_timeout_while(result, timeout, |value| value.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        result.take()
    }
}

#[derive(Default)]
struct ClipboardMailboxState {
    pending: Option<ClipboardJob>,
    closed: bool,
}

#[derive(Default)]
struct ClipboardMailbox {
    state: Mutex<ClipboardMailboxState>,
    ready: Condvar,
}

impl ClipboardMailbox {
    fn submit(&self, job: ClipboardJob) -> Result<(), ExternalError> {
        let mut state = locked(&self.state);
        if state.closed {
            return Err(ExternalError::Failed(String::from(
                "the native clipboard worker stopped unexpectedly",
            )));
        }
        if state.pending.is_some() {
            return Err(ExternalError::Failed(String::from(
                "the native clipboard worker is still busy with an earlier copy",
            )));
        }
        state.pending = Some(job);
        self.ready.notify_one();
        Ok(())
    }

    fn receive(&self) -> Option<ClipboardJob> {
        let mut state = locked(&self.state);
        while state.pending.is_none() && !state.closed {
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.pending.take()
    }

    fn close(&self) {
        locked(&self.state).closed = true;
        self.ready.notify_one();
    }
}

enum NativeClipboardWorker {
    Absent,
    Ready(Arc<ClipboardMailbox>),
    Failed(String),
}

impl NativeClipboardWorker {
    fn spawn(enabled: bool, runner: Box<dyn CommandRunner>) -> Self {
        if !enabled {
            return Self::Absent;
        }
        let mailbox = Arc::new(ClipboardMailbox::default());
        let source = Arc::clone(&mailbox);
        match thread::Builder::new()
            .name(String::from("zuno-clipboard-native"))
            .spawn(move || {
                while let Some(job) = source.receive() {
                    let result = runner.run(&job.argv, &job.input);
                    job.outcome.finish(result);
                }
            }) {
            Ok(_worker) => Self::Ready(mailbox),
            Err(error) => Self::Failed(format!(
                "starting the native clipboard worker failed: {error}"
            )),
        }
    }

    fn run(&self, argv: &[String], input: &str) -> Result<(), ExternalError> {
        let sender = match self {
            Self::Absent => return Err(ExternalError::NoClipboard),
            Self::Ready(sender) => sender,
            Self::Failed(message) => return Err(ExternalError::Failed(message.clone())),
        };
        let outcome = Arc::new(ClipboardOutcome::default());
        let job = ClipboardJob {
            argv: argv.to_vec(),
            input: input.to_owned(),
            outcome: Arc::clone(&outcome),
        };
        sender.submit(job)?;
        match outcome.wait(CLIPBOARD_COMMAND_TIMEOUT) {
            Some(result) => result,
            None => Err(ExternalError::Failed(format!(
                "{} did not finish within {} ms; cleanup continues outside the UI event path",
                argv.first().map_or("clipboard helper", String::as_str),
                CLIPBOARD_COMMAND_TIMEOUT.as_millis()
            ))),
        }
    }
}

impl Drop for NativeClipboardWorker {
    fn drop(&mut self) {
        if let Self::Ready(mailbox) = self {
            mailbox.close();
        }
    }
}

/// The platforms the clipboard ladder distinguishes.
///
/// Its own enum rather than `cfg!` so every branch is reachable from a test on any
/// host — the whole point of making the ladder a pure function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS.
    Macos,
    /// Linux, and WSL.
    Linux,
    /// Windows.
    Windows,
}

impl Platform {
    /// The platform this binary was built for.
    #[must_use]
    pub const fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// The copy command for a host, or `None` when nothing is available.
///
/// Verbatim from `clipboard.ts:75-91`, including the order: Wayland before X11 on
/// Linux, and `xclip` before `xsel`.
#[must_use]
pub fn copy_command(
    platform: Platform,
    wayland: bool,
    has: impl Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let owned = |parts: &[&str]| Some(parts.iter().map(|part| (*part).to_owned()).collect());
    match platform {
        Platform::Macos if has("osascript") => owned(&["osascript"]),
        Platform::Linux if wayland && has("wl-copy") => owned(&["wl-copy"]),
        Platform::Linux if has("xclip") => owned(&["xclip", "-selection", "clipboard"]),
        Platform::Linux if has("xsel") => owned(&["xsel", "--clipboard", "--input"]),
        Platform::Windows if has("powershell.exe") => owned(&[
            "powershell.exe",
            "-NonInteractive",
            "-NoProfile",
            "-Command",
            "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
        ]),
        _ => None,
    }
}

/// The read command for a host, when an image-capable one exists.
///
/// `clipboard.ts:31-72` reaches for an image first on every platform and falls back
/// to text. Only the Linux arms are expressible as a plain command; macOS needs an
/// AppleScript that writes a file and Windows a PowerShell script, so those return
/// `None` here and the real implementation handles them.
#[must_use]
pub fn image_read_command(
    platform: Platform,
    wayland: bool,
    has: impl Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let owned = |parts: &[&str]| Some(parts.iter().map(|part| (*part).to_owned()).collect());
    match platform {
        Platform::Linux if wayland && has("wl-paste") => owned(&["wl-paste", "-t", "image/png"]),
        Platform::Linux if has("xclip") => {
            owned(&["xclip", "-selection", "clipboard", "-t", "image/png", "-o"])
        }
        _ => None,
    }
}

/// The OSC 52 sequence that copies `text` through the terminal itself.
///
/// `multiplexed` wraps the sequence for tmux or GNU screen, which otherwise consume
/// it instead of forwarding it (`clipboard.ts:24-28`).
#[must_use]
pub fn osc52(text: &str, multiplexed: bool) -> String {
    let encoded = base64(text.as_bytes());
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    if multiplexed {
        format!("\x1bPtmux;\x1b{sequence}\x1b\\")
    } else {
        sequence
    }
}

/// Where `program` would be found on `path`, in search order.
///
/// Split from the filesystem check so the joining and the platform's separator are
/// testable without planting executables, the same trade [`copy_command`] makes.
///
/// No `PATHEXT` handling, and that is not an omission: the only Windows arm of the
/// ladder names `powershell.exe` with its extension already.
#[must_use]
pub fn path_candidates(program: &str, path: &str, platform: Platform) -> Vec<PathBuf> {
    let separator = if matches!(platform, Platform::Windows) {
        ';'
    } else {
        ':'
    };
    path.split(separator)
        .filter(|entry| !entry.is_empty())
        .map(|entry| Path::new(entry).join(program))
        .collect()
}

/// The clipboard a real host has: OSC 52 first, then the native fallback.
///
/// # Why OSC 52 goes first
///
/// It is the only mechanism that reaches the machine the user is looking at. Over SSH,
/// or inside tmux, every program in [`copy_command`]'s ladder copies into the *remote*
/// host's clipboard — technically a success, and useless. The escape sequence travels
/// back down the same connection the user's keystrokes came up.
///
/// A successful, flushed OSC 52 write ends the operation. That is a deliberate safety
/// policy: one successful delivery is enough, and avoiding an unnecessary helper keeps
/// the event loop out of a process that could hang. The native ladder is still used
/// when the terminal sink is absent or reports a write failure.
pub struct SystemClipboard {
    /// Absent when this process has no terminal to write an escape sequence to.
    sink: Option<Box<dyn Osc52Sink>>,
    multiplexed: bool,
    /// Absent when no clipboard program is installed.
    command: Option<Vec<String>>,
    native: NativeClipboardWorker,
}

impl SystemClipboard {
    /// A clipboard with exactly the mechanisms given.
    ///
    /// Both mechanisms are resolved by the caller rather than probed in here, which is
    /// what keeps every branch of [`Self::write`] reachable from a test on any host.
    #[must_use]
    pub fn new(
        sink: Option<Box<dyn Osc52Sink>>,
        multiplexed: bool,
        command: Option<Vec<String>>,
        runner: Box<dyn CommandRunner>,
    ) -> Self {
        let native = NativeClipboardWorker::spawn(command.is_some(), runner);
        Self {
            sink,
            multiplexed,
            command,
            native,
        }
    }

    /// The clipboard this host actually has.
    ///
    /// The boundary, and the only function in this module that reads the real world:
    /// [`Platform::host`], the environment and the terminal check all happen here so
    /// that nothing below has to consult `cfg!` or `std::env` to decide what it does.
    #[must_use]
    pub fn host() -> Self {
        Self::for_environment(
            Platform::host(),
            |name| std::env::var(name).ok(),
            terminal_destination(io::stdout().is_terminal()),
            Box::new(ChildProcessRunner),
        )
    }

    /// The clipboard `platform` and `environment` describe, writing OSC 52 to `sink`.
    ///
    /// `sink` is a parameter rather than built in here so that a test can observe the
    /// sequence instead of painting it into the terminal running the suite — the same
    /// reason `runner` is one.
    #[must_use]
    pub fn for_environment(
        platform: Platform,
        environment: impl Fn(&str) -> Option<String>,
        sink: Option<Box<dyn Osc52Sink>>,
        runner: Box<dyn CommandRunner>,
    ) -> Self {
        let path = environment("PATH").unwrap_or_default();
        let wayland = environment("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
        let command = copy_command(platform, wayland, |program| {
            path_candidates(program, &path, platform)
                .iter()
                .any(|candidate| candidate.is_file())
        });
        Self::new(sink, is_multiplexed(&environment), command, runner)
    }

    /// Whether this clipboard has any mechanism at all.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.sink.is_some() || self.command.is_some()
    }
}

impl Clipboard for SystemClipboard {
    fn read(&self) -> Result<Option<ClipboardContent>, ExternalError> {
        // Deliberately not `Ok(None)`. An empty answer would read as "the clipboard is
        // empty", which is precisely the silent no-op this type exists to stop happening
        // on the write side — and this error is now *shown*: `EditorSignal::Paste` is
        // routed, so `SessionScreen::paste_from_clipboard` prints whatever comes back.
        //
        // Still refused rather than implemented, because a native read means another
        // child process, and the write side needed a bounded mailbox on a detached
        // thread to stop a hung `xclip` from freezing the render loop. Reading needs the
        // same treatment and does not have it. The supported path is bracketed paste,
        // which the terminal delivers as an event and which needs no subprocess at all.
        Err(ExternalError::Failed(String::from(
            "reading the clipboard is not wired yet; only copying is. Use the terminal's \
             own paste, which arrives as a bracketed paste",
        )))
    }

    fn write(&self, text: &str) -> Result<(), ExternalError> {
        if !self.is_available() {
            return Err(ExternalError::NoClipboard);
        }
        let mut failures = Vec::new();
        if let Some(sink) = self.sink.as_ref() {
            match sink.emit(&osc52(text, self.multiplexed)) {
                Ok(()) => return Ok(()),
                Err(error) => failures.push(format!("OSC 52: {error}")),
            }
        }
        if let Some(command) = self.command.as_ref() {
            match self.native.run(command, text) {
                Ok(()) => return Ok(()),
                Err(error) => failures.push(error.to_string()),
            }
        }
        // Every failure, joined, rather than the first: the two mechanisms fail for
        // unrelated reasons, and a report naming only one sends the user looking in the
        // wrong place.
        Err(ExternalError::Failed(failures.join("; ")))
    }
}

/// The OSC 52 destination for a process whose stdout `attached_to_terminal` describes.
///
/// Its own function so the decision is assertable without a terminal: `None` is the
/// whole reason a test run neither paints escape sequences into captured output nor
/// spawns `xclip`. See the module header for why a redirected stdout means no
/// clipboard rather than a clipboard writing into a file.
#[must_use]
pub fn terminal_destination(attached_to_terminal: bool) -> Option<Box<dyn Osc52Sink>> {
    attached_to_terminal.then(|| Box::new(TerminalSink) as Box<dyn Osc52Sink>)
}

/// Whether the host is inside tmux or GNU screen (`clipboard.ts:27` — `TMUX`/`STY`).
#[must_use]
pub fn is_multiplexed(lookup: impl Fn(&str) -> Option<String>) -> bool {
    ["TMUX", "STY"]
        .iter()
        .any(|name| lookup(name).is_some_and(|value| !value.is_empty()))
}

/// Standard base64, no line breaks.
///
/// Hand-rolled rather than a dependency: this is the only base64 in the crate, and
/// adding an encoder to the render stack for twenty lines is a poor trade.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}
