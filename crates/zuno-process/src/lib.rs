//! Process-tree containment shared by every resident external host.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

/// First argv token marking an invocation as a hidden guard request.
///
/// Public only so tests build guard argv from this definition rather than
/// repeating the literal.
pub const GUARD_MARKER: &str = "__zuno_child_guard";
const SUPERVISE_MODE: &str = "supervise";
const SUPERVISE_FOREGROUND_MODE: &str = "supervise-foreground";
const MONITOR_FOREGROUND_MODE: &str = "monitor-foreground";
const EXEC_MODE: &str = "exec";
const EXEC_FOREGROUND_MODE: &str = "exec-foreground";
#[cfg(unix)]
const POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(any(windows, unix))]
const PARENT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long a teardown waits for the contained group to stop being able to run.
///
/// The wait begins only after `SIGKILL` has reached the whole group, so what remains is
/// scheduler latency rather than cooperation, and it returns the moment the condition
/// holds — the ordinary cost is one syscall. The bound covers the case that is not
/// scheduler latency, a member stuck in uninterruptible I/O, where hanging would be
/// worse than the hazard because a TUI with a waiting user sits above this.
#[cfg(unix)]
const GROUP_QUIET_BUDGET: Duration = Duration::from_millis(500);
static GUARD_EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();

/// Enables guarded child launches in the current process.
///
/// The executable must call [`run_guard_from_args`] before parsing its ordinary
/// command line. Tests may supply a dedicated guard binary; production supplies
/// the current `zuno` executable.
pub fn activate_guard_executable(executable: impl Into<PathBuf>) -> io::Result<()> {
    let executable = executable.into();
    match GUARD_EXECUTABLE.set(executable.clone()) {
        Ok(()) => Ok(()),
        Err(_) if GUARD_EXECUTABLE.get() == Some(&executable) => Ok(()),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a different child-process guard executable is already active",
        )),
    }
}

/// One directly spawned process group owned by the current process.
///
/// This is the zero-helper topology used by local stdio MCP: the configured
/// command is the direct child and process-group leader. The owner is responsible
/// for TERM/KILL escalation and reaping during normal shutdown.
#[derive(Debug, Clone, Copy)]
pub struct DirectProcessGroup {
    pid: u32,
}

impl DirectProcessGroup {
    /// Validate and retain a directly spawned process-group leader.
    pub fn register(pid: u32) -> io::Result<Self> {
        validate_process_id(pid)?;
        validate_process_group_leader(pid)?;
        Ok(Self { pid })
    }

    /// Requests cooperative shutdown of the complete process group.
    pub fn request_termination(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            signal_direct_process_group(self.pid, rustix::process::Signal::TERM)
        }
        #[cfg(windows)]
        {
            terminate_windows_process_tree(self.pid)
        }
    }

    /// Forces every remaining member of the process group to stop.
    pub fn force_kill(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            signal_direct_process_group(self.pid, rustix::process::Signal::KILL)
        }
        #[cfg(windows)]
        {
            terminate_windows_process_tree(self.pid)
        }
    }
}

/// Rewrites one program and argument vector through the active containment guard.
///
/// Before activation this is an identity operation, which keeps library-only
/// consumers and unit-test binaries independent of the CLI executable.
pub fn guarded_argv<I, S>(program: impl AsRef<OsStr>, arguments: I) -> (OsString, Vec<OsString>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    guarded_argv_for_mode(program, arguments, SUPERVISE_MODE)
}

/// Rewrites one interactive program through containment with terminal foreground ownership.
///
/// This mode is reserved for a short-lived child that must read the terminal, such as an
/// external editor. Resident background hosts must use [`guarded_argv`] so they cannot take
/// foreground ownership merely because their parent currently owns the terminal.
pub fn guarded_foreground_argv<I, S>(
    program: impl AsRef<OsStr>,
    arguments: I,
) -> (OsString, Vec<OsString>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    guarded_argv_for_mode(program, arguments, SUPERVISE_FOREGROUND_MODE)
}

/// Rewrites a command for execution inside a native pseudoterminal.
///
/// Unix PTYs retain the foreground guard because the guard owns process-group
/// foreground transfer and parent-death cleanup. Windows ConPTY is itself the
/// terminal containment boundary: inserting the resident Job Object guard as
/// the ConPTY child prevents interactive input and natural exit from settling.
/// The Windows route therefore launches the requested program directly and the
/// PTY owner terminates its tree through [`request_contained_process_shutdown`].
pub fn guarded_terminal_argv<I, S>(
    program: impl AsRef<OsStr>,
    arguments: I,
) -> (OsString, Vec<OsString>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    #[cfg(windows)]
    {
        (
            program.as_ref().to_os_string(),
            arguments
                .into_iter()
                .map(|argument| argument.as_ref().to_os_string())
                .collect(),
        )
    }

    #[cfg(not(windows))]
    {
        guarded_foreground_argv(program, arguments)
    }
}

/// Requests shutdown from a direct child returned by a guarded launch helper.
///
/// With an active resident guard, the guard receives a catchable signal so it can
/// stop and reap the process group it owns. Before guard activation, library-only
/// consumers launch the program directly; an isolated process-group leader is then
/// killed as a group, while a non-leader is killed individually so Zuno never
/// signals its own process group. Windows terminal launches are deliberately
/// direct even after guard activation, and this function terminates their complete
/// process tree. The request is idempotent when the process already exited.
pub fn request_contained_process_shutdown(pid: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        let Some(process) = rustix::process::Pid::from_raw(pid as i32) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process id must be positive",
            ));
        };
        let result = if GUARD_EXECUTABLE.get().is_some() {
            rustix::process::kill_process(process, rustix::process::Signal::TERM)
        } else {
            match rustix::process::getpgid(Some(process)) {
                Ok(group) if group == process => {
                    rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
                }
                Ok(_) => rustix::process::kill_process(process, rustix::process::Signal::KILL),
                Err(rustix::io::Errno::SRCH) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        };
        match result {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(windows)]
    {
        terminate_windows_process_tree(pid)
    }
}

#[cfg(windows)]
fn terminate_windows_process_tree(pid: u32) -> io::Result<()> {
    let status = Command::new("taskkill")
        .args(["/pid", &pid.to_string(), "/f", "/t"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        return Ok(());
    }
    // `taskkill` uses a non-zero status for a process that already exited.
    // Checking again makes cancellation idempotent without hiding a live
    // process that genuinely refused termination.
    if windows_process_exists(pid) {
        Err(io::Error::other(format!(
            "taskkill failed for process tree {pid} with status {status}"
        )))
    } else {
        Ok(())
    }
}

fn guarded_argv_for_mode<I, S>(
    program: impl AsRef<OsStr>,
    arguments: I,
    mode: &'static str,
) -> (OsString, Vec<OsString>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = program.as_ref().to_os_string();
    let arguments: Vec<OsString> = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_os_string())
        .collect();
    let Some(guard) = GUARD_EXECUTABLE.get() else {
        return (program, arguments);
    };
    let mut guarded = Vec::with_capacity(arguments.len() + 5);
    guarded.push(OsString::from(GUARD_MARKER));
    guarded.push(OsString::from(mode));
    guarded.push(OsString::from(std::process::id().to_string()));
    guarded.push(OsString::from("--"));
    guarded.push(program);
    guarded.extend(arguments);
    (guard.as_os_str().to_os_string(), guarded)
}

/// Executes the hidden guard protocol when the process was launched in guard mode.
///
/// Call this before any ordinary CLI parsing. `None` means the invocation is not
/// a guard request and the caller should continue normal startup.
#[must_use]
pub fn run_guard_from_args() -> Option<ExitCode> {
    let arguments: Vec<OsString> = std::env::args_os().collect();
    if arguments.get(1).and_then(|value| value.to_str()) != Some(GUARD_MARKER) {
        return None;
    }
    Some(match parse_guard(&arguments).and_then(run_guard) {
        Ok(status) => exit_code(status),
        Err(error) => {
            eprintln!("child-process guard failed: {error}");
            ExitCode::FAILURE
        }
    })
}

fn validate_process_id(pid: u32) -> io::Result<u32> {
    if pid == 0 {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process id must be positive",
        ))
    } else {
        Ok(pid)
    }
}

fn validate_process_group_leader(pid: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        let process = process_pid(pid)?;
        match rustix::process::getpgid(Some(process)) {
            Ok(group) if group == process => Ok(()),
            Ok(group) => Err(io::Error::other(format!(
                "process {pid} belongs to group {} instead of leading its own group",
                group.as_raw_nonzero()
            ))),
            Err(error) => Err(error.into()),
        }
    }
    #[cfg(windows)]
    {
        let _pid = pid;
        Ok(())
    }
}

#[cfg(unix)]
fn process_pid(pid: u32) -> io::Result<rustix::process::Pid> {
    let pid = validate_process_id(pid)?;
    rustix::process::Pid::from_raw(pid as i32)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "process id must be positive"))
}

#[cfg(unix)]
fn signal_direct_process_group(pid: u32, signal: rustix::process::Signal) -> io::Result<()> {
    let group = process_pid(pid)?;
    match rustix::process::kill_process_group(group, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

struct GuardRequest {
    mode: &'static str,
    expected_parent: u32,
    program: OsString,
    arguments: Vec<OsString>,
}

fn parse_guard(arguments: &[OsString]) -> io::Result<GuardRequest> {
    let mode = match arguments.get(2).and_then(|value| value.to_str()) {
        Some(SUPERVISE_MODE) => SUPERVISE_MODE,
        Some(SUPERVISE_FOREGROUND_MODE) => SUPERVISE_FOREGROUND_MODE,
        Some(MONITOR_FOREGROUND_MODE) => MONITOR_FOREGROUND_MODE,
        Some(EXEC_MODE) => EXEC_MODE,
        Some(EXEC_FOREGROUND_MODE) => EXEC_FOREGROUND_MODE,
        _ => return Err(io::Error::other("invalid child-process guard mode")),
    };
    let expected_parent = arguments
        .get(3)
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::other("child-process guard parent PID is missing"))?
        .parse::<u32>()
        .map_err(|error| io::Error::other(format!("invalid guard parent PID: {error}")))?;
    if arguments.get(4).and_then(|value| value.to_str()) != Some("--") {
        return Err(io::Error::other("child-process guard separator is missing"));
    }
    let program = arguments
        .get(5)
        .cloned()
        .ok_or_else(|| io::Error::other("guarded program is missing"))?;
    Ok(GuardRequest {
        mode,
        expected_parent,
        program,
        arguments: arguments[6..].to_vec(),
    })
}

fn run_guard(request: GuardRequest) -> io::Result<ExitStatus> {
    match request.mode {
        SUPERVISE_MODE => supervise_resident(request),
        SUPERVISE_FOREGROUND_MODE => supervise_foreground(request),
        MONITOR_FOREGROUND_MODE => monitor_foreground(request),
        EXEC_MODE | EXEC_FOREGROUND_MODE => exec_guarded(request),
        _ => unreachable!("guard mode was validated while parsing"),
    }
}

#[cfg(unix)]
fn supervise_resident(request: GuardRequest) -> io::Result<ExitStatus> {
    #[cfg(target_os = "linux")]
    {
        supervise_resident_linux(request)
    }
    #[cfg(not(target_os = "linux"))]
    {
        supervise_resident_with_parent_poll(request)
    }
}

#[cfg(target_os = "linux")]
fn supervise_resident_linux(request: GuardRequest) -> io::Result<ExitStatus> {
    use std::sync::atomic::Ordering;

    use rustix::event::{PollFd, PollFlags, Timespec};
    use rustix::process::{PidfdFlags, pidfd_open};

    let terminate = termination_flag()?;
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))?;
    if parent_pid() != Some(request.expected_parent) {
        return Err(io::Error::other(
            "guard parent exited before containment was armed",
        ));
    }

    let mut child = spawn_resident_payload(&request)?;
    let child_pid = child.id();
    let child_process = rustix::process::Pid::from_raw(child_pid as i32)
        .ok_or_else(|| io::Error::other("resident payload PID is invalid"))?;
    let pidfd = pidfd_open(child_process, PidfdFlags::empty()).ok();
    let wait_timeout = Timespec::try_from(PARENT_POLL_INTERVAL)
        .map_err(|_| io::Error::other("resident process poll interval is invalid"))?;
    loop {
        if let Some(status) = observe_resident_payload(&mut child, child_pid)? {
            return Ok(status);
        }
        if terminate.load(Ordering::Acquire) || parent_pid() != Some(request.expected_parent) {
            return terminate_guarded_process_group(&mut child, child_pid, false);
        }

        let Some(pidfd) = pidfd.as_ref() else {
            std::thread::sleep(PARENT_POLL_INTERVAL);
            continue;
        };
        let mut poll_fd = [PollFd::new(pidfd, PollFlags::IN)];
        match rustix::event::poll(&mut poll_fd, Some(&wait_timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => {
                return Err(with_cleanup_error(
                    error.into(),
                    terminate_guarded_process_group(&mut child, child_pid, false),
                ));
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn supervise_resident_with_parent_poll(request: GuardRequest) -> io::Result<ExitStatus> {
    let terminate = termination_flag()?;
    if parent_pid() != Some(request.expected_parent) {
        return Err(io::Error::other(
            "guard parent exited before containment was armed",
        ));
    }

    let mut child = spawn_resident_payload(&request)?;
    let child_pid = child.id();
    loop {
        if let Some(status) = observe_resident_payload(&mut child, child_pid)? {
            return Ok(status);
        }
        if terminate.load(std::sync::atomic::Ordering::Acquire)
            || parent_pid() != Some(request.expected_parent)
        {
            return terminate_guarded_process_group(&mut child, child_pid, false);
        }
        std::thread::sleep(PARENT_POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn observe_resident_payload(
    child: &mut std::process::Child,
    child_pid: u32,
) -> io::Result<Option<ExitStatus>> {
    match child.try_wait() {
        Ok(Some(status)) => {
            terminate_process_group(child_pid);
            settle_contained_process_group(child_pid);
            Ok(Some(status))
        }
        Ok(None) => Ok(None),
        Err(error) => Err(with_cleanup_error(
            error,
            terminate_guarded_process_group(child, child_pid, false),
        )),
    }
}

#[cfg(unix)]
fn spawn_resident_payload(request: &GuardRequest) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new(&request.program);
    command
        .args(&request.arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0);
    command.spawn()
}

#[cfg(unix)]
fn supervise_foreground(request: GuardRequest) -> io::Result<ExitStatus> {
    #[cfg(target_os = "linux")]
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))?;
    let terminate = termination_flag()?;
    if parent_pid() != Some(request.expected_parent) {
        return Err(io::Error::other(
            "guard parent exited before containment was armed",
        ));
    }

    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .arg(GUARD_MARKER)
        .arg(MONITOR_FOREGROUND_MODE)
        .arg(std::process::id().to_string())
        .arg("--")
        .arg(&request.program)
        .args(&request.arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let child_pid = child.id();

    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if terminate.load(std::sync::atomic::Ordering::Acquire)
            || parent_pid() != Some(request.expected_parent)
        {
            terminate_process(child_pid);
            return child.wait();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn monitor_foreground(request: GuardRequest) -> io::Result<ExitStatus> {
    use std::io::IsTerminal as _;

    #[cfg(target_os = "linux")]
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))?;
    let terminate = termination_flag()?;
    if parent_pid() != Some(request.expected_parent) {
        return Err(io::Error::other(
            "guard supervisor exited before monitor was armed",
        ));
    }

    let transfer_foreground = std::io::stdin().is_terminal();
    if transfer_foreground && !terminal_foreground_is_process_group_of(request.expected_parent)? {
        return Err(io::Error::other(
            "guard supervisor does not own the terminal foreground process group",
        ));
    }
    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .arg(GUARD_MARKER)
        .arg(if transfer_foreground {
            EXEC_FOREGROUND_MODE
        } else {
            EXEC_MODE
        })
        .arg(std::process::id().to_string())
        .arg("--")
        .arg(&request.program)
        .args(&request.arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let child_pid = child.id();
    if transfer_foreground && let Err(error) = transfer_terminal_foreground(child_pid) {
        return Err(with_cleanup_error(
            error,
            terminate_guarded_process_group(&mut child, child_pid, true),
        ));
    }

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                terminate_process_group(child_pid);
                settle_contained_process_group(child_pid);
                return Ok(status);
            }
            Ok(None) => {}
            Err(error) => {
                return Err(with_cleanup_error(
                    error,
                    terminate_guarded_process_group(&mut child, child_pid, transfer_foreground),
                ));
            }
        }
        if terminate.load(std::sync::atomic::Ordering::Acquire)
            || parent_pid() != Some(request.expected_parent)
        {
            return terminate_guarded_process_group(&mut child, child_pid, transfer_foreground);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn exec_guarded(request: GuardRequest) -> io::Result<ExitStatus> {
    use std::os::unix::process::CommandExt as _;

    let foreground = request.mode == EXEC_FOREGROUND_MODE;
    let terminate = foreground.then(foreground_termination_flag).transpose()?;
    #[cfg(target_os = "linux")]
    rustix::process::set_parent_process_death_signal(Some(if foreground {
        rustix::process::Signal::TERM
    } else {
        rustix::process::Signal::KILL
    }))?;
    if parent_pid() != Some(request.expected_parent) {
        return Err(io::Error::other(
            "guard supervisor exited before exec was armed",
        ));
    }
    if foreground {
        return run_foreground_guard(request, terminate.expect("foreground guard has a flag"));
    }
    rustix::process::setpgid(None, None)?;
    let error = Command::new(request.program).args(request.arguments).exec();
    Err(error)
}

#[cfg(unix)]
fn run_foreground_guard(
    request: GuardRequest,
    terminate: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<ExitStatus> {
    let original_process_group = rustix::process::getpgrp();
    rustix::process::setpgid(None, None)?;
    let guarded_process_group = rustix::process::getpgrp();
    let restoration = TerminalForegroundRestoration::new(original_process_group);
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::STOP)?;

    let result = if rustix::termios::tcgetpgrp(std::io::stdin())? != guarded_process_group {
        Err(io::Error::other(
            "guarded child resumed before terminal foreground handoff",
        ))
    } else {
        run_foreground_payload(request, terminate)
    };
    restoration.finish(result)
}

#[cfg(unix)]
fn run_foreground_payload(
    request: GuardRequest,
    terminate: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<ExitStatus> {
    let mut child = Command::new(request.program)
        .args(request.arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if terminate.load(std::sync::atomic::Ordering::Acquire)
            || parent_pid() != Some(request.expected_parent)
        {
            terminate_process(child.id());
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(100) {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "guarded foreground child did not exit after termination",
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
struct TerminalForegroundRestoration {
    process_group: rustix::process::Pid,
    restored: bool,
}

#[cfg(unix)]
impl TerminalForegroundRestoration {
    fn new(process_group: rustix::process::Pid) -> Self {
        Self {
            process_group,
            restored: false,
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        rustix::termios::tcsetpgrp(std::io::stdin(), self.process_group)?;
        self.restored = true;
        Ok(())
    }

    fn finish<T>(mut self, result: io::Result<T>) -> io::Result<T> {
        match (result, self.restore()) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(restore_error)) => Err(io::Error::other(format!(
                "{error}; restoring the terminal foreground process group failed: {restore_error}"
            ))),
        }
    }
}

#[cfg(unix)]
impl Drop for TerminalForegroundRestoration {
    fn drop(&mut self) {
        if !self.restored {
            let _restored = rustix::termios::tcsetpgrp(std::io::stdin(), self.process_group);
        }
    }
}

#[cfg(unix)]
fn termination_flag() -> io::Result<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let terminate = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&terminate))?;
    }
    Ok(terminate)
}

#[cfg(unix)]
fn foreground_termination_flag() -> io::Result<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let terminate = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&terminate))?;
    }
    let terminal_interrupt = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGQUIT] {
        signal_hook::flag::register(signal, Arc::clone(&terminal_interrupt))?;
    }
    Ok(terminate)
}

#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    rustix::process::getppid().map(|pid| pid.as_raw_nonzero().get() as u32)
}

#[cfg(unix)]
fn terminal_foreground_is_process_group_of(process: u32) -> io::Result<bool> {
    use std::io::IsTerminal as _;

    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let process = rustix::process::Pid::from_raw(process as i32)
        .ok_or_else(|| io::Error::other("guard supervisor PID is invalid"))?;
    // `expected_parent` identifies the supervisor process, but `tcgetpgrp`
    // returns a process-group ID. The supervisor inherited the launching TUI's
    // group, so its PGID is the group that legitimately owned the terminal
    // before the contained editor gets a new one.
    let process_group = rustix::process::getpgid(Some(process))?;
    Ok(rustix::termios::tcgetpgrp(std::io::stdin())? == process_group)
}

#[cfg(unix)]
fn transfer_terminal_foreground(child_pid: u32) -> io::Result<()> {
    let child_pid = rustix::process::Pid::from_raw(child_pid as i32)
        .ok_or_else(|| io::Error::other("guarded child PID is invalid"))?;
    let (_, status) =
        rustix::process::waitpid(Some(child_pid), rustix::process::WaitOptions::UNTRACED)?
            .ok_or_else(|| io::Error::other("guarded child did not stop for terminal handoff"))?;
    if !status.stopped() {
        return Err(io::Error::other(format!(
            "guarded child exited before terminal handoff: {status:?}"
        )));
    }
    rustix::termios::tcsetpgrp(std::io::stdin(), child_pid)?;
    rustix::process::kill_process(child_pid, rustix::process::Signal::CONT)?;
    Ok(())
}

#[cfg(unix)]
fn terminate_process(pid: u32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
        let _result = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
}

#[cfg(unix)]
fn terminate_process_group(pid: u32) {
    signal_process_group(pid, rustix::process::Signal::KILL);
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum GroupQuiet {
    Reached,
    /// Empty means "some, but this platform cannot name them" — never "none".
    Refused(Vec<u32>),
}

/// Drives process group `pgid` until no member can execute another instruction.
///
/// A zombie counts as quiet. It has no address space and no file descriptors, so it
/// cannot write to the terminal, and the only transition left for it — being reaped —
/// belongs to whichever process inherits the orphan. Waiting for the group to be
/// *empty* would therefore make every editor exit depend on a third party's
/// scheduling, and never complete at all under an init that does not reap orphans.
///
/// Each pass re-signals before it observes, so this converges rather than merely
/// watching. The already-pending `SIGKILL` on a member the caller's sweep reached
/// makes the repeat redundant for that member, but a member that forked into the group
/// while the kernel was walking it never received that signal, and only a later pass
/// can reach it.
#[cfg(unix)]
fn wait_for_process_group_to_go_quiet(pgid: u32, budget: Duration) -> GroupQuiet {
    drive_until_quiet(budget, || {
        signal_process_group(pgid, rustix::process::Signal::KILL);
        process_group_quiet(pgid)
    })
}

/// The bound, apart from what it is bounding.
///
/// Split out because a group that refuses `SIGKILL` cannot be constructed — the kernel
/// does not offer one — so the only way to prove the budget is honoured is to hand the
/// loop an observation that never succeeds.
#[cfg(unix)]
fn drive_until_quiet(budget: Duration, mut pass: impl FnMut() -> GroupQuiet) -> GroupQuiet {
    let started = Instant::now();
    loop {
        let quiet = pass();
        if quiet == GroupQuiet::Reached || started.elapsed() >= budget {
            return quiet;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One observation of what group `pgid` can still do.
///
/// `SIGnal 0` answers the common case — an empty group — in a single syscall, which
/// matters because this runs on every editor exit. The `/proc` walk is only paid for
/// when somebody is still there.
#[cfg(unix)]
fn process_group_quiet(pgid: u32) -> GroupQuiet {
    let Some(group) = rustix::process::Pid::from_raw(pgid as i32) else {
        return GroupQuiet::Reached;
    };
    // `ESRCH` — the group is gone — is the only answer that lets the terminal go back.
    // A present group and an unanswerable one both keep waiting until the budget stops.
    if rustix::process::test_kill_process_group(group) == Err(rustix::io::Errno::SRCH) {
        return GroupQuiet::Reached;
    }
    let live = live_process_group_members(pgid);
    if live.is_empty() {
        GroupQuiet::Reached
    } else {
        GroupQuiet::Refused(live)
    }
}

#[cfg(target_os = "linux")]
fn live_process_group_members(pgid: u32) -> Vec<u32> {
    let mut live = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return live;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if process_is_live_member_of(pid, pgid) {
            live.push(pid);
        }
    }
    live
}

#[cfg(target_os = "linux")]
fn process_is_live_member_of(pid: u32, pgid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `comm` is parenthesised and may itself hold spaces and parentheses, so the fixed
    // fields — state, ppid, pgrp — start after its last `)`.
    let Some(fields) = stat.rfind(')').and_then(|end| stat.get(end + 1..)) else {
        return false;
    };
    let mut fields = fields.split_whitespace();
    let Some(state) = fields.next() else {
        return false;
    };
    fields
        .nth(1)
        .and_then(|group| group.parse::<u32>().ok())
        .is_some_and(|group| group == pgid && state != "Z")
}

/// Every other Unix lacks a `/proc` state field, so membership is the whole answer.
///
/// The caller has already established that the group is not empty, which on this
/// platform is as much as can be known: the wait then holds out for an empty group,
/// which is stricter than necessary but still bounded.
#[cfg(all(unix, not(target_os = "linux")))]
fn live_process_group_members(_pgid: u32) -> Vec<u32> {
    Vec::new()
}

/// Waits for the contained group and reports on the guard's stderr if it refuses.
///
/// The report is not promoted to a failing exit status on purpose. This runs on the
/// ordinary path too, where the payload succeeded, and turning a stray background job
/// into a failure would throw away the text the user just wrote — a worse outcome than
/// a repaint artefact. The guard's stderr is the terminal the editor was using and is
/// where every other containment failure is already reported.
#[cfg(unix)]
fn settle_contained_process_group(pgid: u32) {
    if let GroupQuiet::Refused(members) =
        wait_for_process_group_to_go_quiet(pgid, GROUP_QUIET_BUDGET)
    {
        eprintln!("{}", group_refused_to_go_quiet(pgid, &members));
    }
}

#[cfg(unix)]
fn group_refused_to_go_quiet(pgid: u32, members: &[u32]) -> String {
    let who = if members.is_empty() {
        "members this platform cannot name".to_owned()
    } else {
        format!("{members:?}")
    };
    format!(
        "child-process guard: process group {pgid} still had {who} able to run \
         {} ms after SIGKILL; the terminal is being handed back while they could still \
         write to it",
        GROUP_QUIET_BUDGET.as_millis()
    )
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: rustix::process::Signal) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
        let _result = rustix::process::kill_process_group(pid, signal);
    }
}

#[cfg(unix)]
fn terminate_guarded_process_group(
    child: &mut std::process::Child,
    child_pid: u32,
    restore_foreground: bool,
) -> io::Result<ExitStatus> {
    if restore_foreground {
        signal_process_group(child_pid, rustix::process::Signal::TERM);
        terminate_process_group_member(child_pid, rustix::process::Signal::CONT);
    } else {
        terminate_process_group(child_pid);
    }
    let status = child.wait()?;
    terminate_process_group(child_pid);
    settle_contained_process_group(child_pid);
    Ok(status)
}

#[cfg(unix)]
fn terminate_process_group_member(pid: u32, signal: rustix::process::Signal) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
        let _result = rustix::process::kill_process(pid, signal);
    }
}

#[cfg(unix)]
fn with_cleanup_error(error: io::Error, cleanup: io::Result<ExitStatus>) -> io::Error {
    match cleanup {
        Ok(_) => error,
        Err(cleanup_error) => io::Error::other(format!(
            "{error}; guarded process cleanup failed: {cleanup_error}"
        )),
    }
}

#[cfg(windows)]
fn supervise_resident(request: GuardRequest) -> io::Result<ExitStatus> {
    supervise_windows(request)
}

#[cfg(windows)]
fn supervise_foreground(request: GuardRequest) -> io::Result<ExitStatus> {
    supervise_windows(request)
}

#[cfg(windows)]
fn supervise_windows(request: GuardRequest) -> io::Result<ExitStatus> {
    use process_wrap::std::{CommandWrap, JobObject};

    let mut command = Command::new(request.program);
    command
        .args(request.arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut command = CommandWrap::from(command);
    command.wrap(JobObject);
    let mut child = command.spawn()?;
    loop {
        if let Some(status) = child.try_wait()? {
            // process-wrap 9.0.1's std JobObject is not kill-on-close. The
            // top-level process may be gone while descendants still own the job.
            child.start_kill()?;
            let _reaped_status = child.wait()?;
            return Ok(status);
        }
        if !windows_process_exists(request.expected_parent) {
            child.start_kill()?;
            return child.wait();
        }
        std::thread::sleep(PARENT_POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn monitor_foreground(_request: GuardRequest) -> io::Result<ExitStatus> {
    Err(io::Error::other("monitor guard mode is Unix-only"))
}

#[cfg(windows)]
fn exec_guarded(_request: GuardRequest) -> io::Result<ExitStatus> {
    Err(io::Error::other("exec guard mode is Unix-only"))
}

#[cfg(windows)]
fn windows_process_exists(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        })
}

fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

/// Returns whether `path` names the active guard executable.
#[must_use]
pub fn is_active_guard(path: &Path) -> bool {
    GUARD_EXECUTABLE.get().is_some_and(|active| active == path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};

    /// A live process group in its own group, so signalling it cannot reach the suite.
    fn sleeping_group() -> std::process::Child {
        Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a sleeping process group")
    }

    #[test]
    fn a_group_nobody_else_will_kill_is_driven_quiet_by_the_wait_itself() {
        let mut group = sleeping_group();
        let pgid = group.id();

        let quiet = wait_for_process_group_to_go_quiet(pgid, Duration::from_secs(5));

        let reaped = group.wait().expect("the group leader is reapable");
        assert_eq!(
            quiet,
            GroupQuiet::Reached,
            "nothing else in this test signals the group, so a wait that only observes \
             can never see it go quiet — it must drive the group there"
        );
        assert_eq!(
            reaped.signal(),
            Some(rustix::process::Signal::KILL.as_raw()),
            "the group leader ended some other way than the wait's own signal: {reaped}"
        );
    }

    #[test]
    fn a_live_process_group_is_never_reported_as_unable_to_run() {
        let mut group = sleeping_group();
        let pgid = group.id();

        let observed = process_group_quiet(pgid);

        terminate_process_group(pgid);
        let _reaped = group.wait();
        assert_eq!(
            observed,
            GroupQuiet::Refused(vec![pgid]),
            "a sleeping process was treated as unable to run, yet it can write to the \
             terminal the moment it is scheduled"
        );
    }

    #[test]
    fn a_zombie_is_quiet_because_it_can_no_longer_execute_anything() {
        let mut group = sleeping_group();
        let pgid = group.id();
        terminate_process_group(pgid);
        // Deliberately unreaped: this is the state an orphaned descendant sits in until
        // init gets to it, and it must not hold the terminal hostage.
        wait_until_zombie(pgid);

        let observed = process_group_quiet(pgid);

        let _reaped = group.wait();
        assert_eq!(
            observed,
            GroupQuiet::Reached,
            "an unreaped zombie was treated as able to run, which would make every \
             editor exit wait on whoever inherits the orphan"
        );
    }

    #[test]
    fn an_observation_that_never_succeeds_still_ends_at_the_budget() {
        let budget = Duration::from_millis(50);
        let mut passes = 0_u32;

        let started = Instant::now();
        let quiet = drive_until_quiet(budget, || {
            passes += 1;
            GroupQuiet::Refused(vec![7])
        });
        let waited = started.elapsed();

        assert_eq!(quiet, GroupQuiet::Refused(vec![7]));
        assert!(
            passes > 1,
            "the wait gave up after {passes} pass(es), so a group that needs a moment \
             to die is reported as holding the terminal"
        );
        assert!(
            waited < budget * 40,
            "the wait ran for {waited:?} against a {budget:?} budget, which is a hang \
             rather than a bounded wait"
        );
    }

    /// The shipped budget must allow the drain to poll more than once.
    ///
    /// Every other test here passes its own budget, so the constant production actually
    /// uses was for a while pinned by nothing: setting it to zero left the whole crate
    /// green while collapsing the drain to one signal and one observation. That
    /// degradation is close to invisible in the integration fixtures — a single `/proc`
    /// walk is enough delay for an ordinary descendant — and it removes precisely the
    /// convergence the drain exists for.
    #[test]
    fn the_shipped_budget_leaves_room_for_more_than_one_pass() {
        assert!(
            GROUP_QUIET_BUDGET > POLL_INTERVAL,
            "the drain polls every {POLL_INTERVAL:?} but is allowed only \
             {GROUP_QUIET_BUDGET:?}, so it gives up after its first observation and a \
             member that joined the group during the kernel's kill walk is never \
             reached again"
        );
    }

    /// Both routes out of `monitor` must settle the group before the guard exits.
    ///
    /// Behaviour cannot assert this: whether a member is still runnable at the instant
    /// the guard returns is a scheduling outcome, so removing either call leaves the
    /// suite green on an idle host and red a few times in sixty under load. The wiring
    /// is what has to be pinned, and only the source states it.
    #[test]
    fn every_route_out_of_the_guard_settles_the_contained_group() {
        let source = include_str!("lib.rs");
        let ordinary = "                terminate_process_group(child_pid);\n                \
                        settle_contained_process_group(child_pid);";
        let terminated = "    terminate_process_group(child_pid);\n    \
                          settle_contained_process_group(child_pid);";

        assert!(
            source.contains(ordinary),
            "the route taken when the editor exits by itself kills the group and then \
             hands the terminal back without waiting, so a descendant it orphaned can \
             still write over the first frame the TUI paints"
        );
        assert!(
            source.contains(terminated),
            "the route taken when the editor is cancelled kills the group and then hands \
             the terminal back without waiting, so a descendant it orphaned can still \
             write over the first frame the TUI paints"
        );
    }

    fn wait_until_zombie(pid: u32) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if live_process_group_members(pid).is_empty() {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        panic!("the killed process never became a zombie");
    }

    #[test]
    fn a_group_with_no_members_is_quiet_without_spending_the_budget() {
        let mut group = sleeping_group();
        let pgid = group.id();
        terminate_process_group(pgid);
        let _reaped = group.wait();

        let started = Instant::now();
        let quiet = wait_for_process_group_to_go_quiet(pgid, Duration::from_secs(5));
        let waited = started.elapsed();

        assert_eq!(quiet, GroupQuiet::Reached);
        assert!(
            waited < Duration::from_millis(500),
            "an empty group cost {waited:?}, so every editor exit would pay for it"
        );
    }

    #[test]
    fn the_refusal_says_which_members_held_the_terminal() {
        let named = group_refused_to_go_quiet(4321, &[91, 92]);

        assert!(
            named.contains("4321") && named.contains("[91, 92]"),
            "the diagnostic names neither the group nor its members: {named}"
        );
        assert!(
            named.contains("SIGKILL") && named.contains("write to it"),
            "the diagnostic does not say what the hazard is: {named}"
        );
    }

    #[test]
    fn a_refusal_this_platform_cannot_attribute_still_reports_the_hazard() {
        let unnamed = group_refused_to_go_quiet(4321, &[]);

        assert!(
            unnamed.contains("cannot name"),
            "an unattributable refusal must not read as an empty one: {unnamed}"
        );
    }
}
