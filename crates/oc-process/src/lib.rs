//! Process-tree containment shared by every resident external host.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

const GUARD_MARKER: &str = "__oc_child_guard";
const SUPERVISE_MODE: &str = "supervise";
const MONITOR_MODE: &str = "monitor";
const EXEC_MODE: &str = "exec";
const EXEC_FOREGROUND_MODE: &str = "exec-foreground";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
static GUARD_EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();

/// Enables guarded child launches in the current process.
///
/// The executable must call [`run_guard_from_args`] before parsing its ordinary
/// command line. Tests may supply a dedicated guard binary; production supplies
/// the current `opencode-rust` executable.
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

/// Rewrites one program and argument vector through the active containment guard.
///
/// Before activation this is an identity operation, which keeps library-only
/// consumers and unit-test binaries independent of the CLI executable.
pub fn guarded_argv<I, S>(program: impl AsRef<OsStr>, arguments: I) -> (OsString, Vec<OsString>)
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
    guarded.push(OsString::from(SUPERVISE_MODE));
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

struct GuardRequest {
    mode: &'static str,
    expected_parent: u32,
    program: OsString,
    arguments: Vec<OsString>,
}

fn parse_guard(arguments: &[OsString]) -> io::Result<GuardRequest> {
    let mode = match arguments.get(2).and_then(|value| value.to_str()) {
        Some(SUPERVISE_MODE) => SUPERVISE_MODE,
        Some(MONITOR_MODE) => MONITOR_MODE,
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
        SUPERVISE_MODE => supervise(request),
        MONITOR_MODE => monitor(request),
        EXEC_MODE | EXEC_FOREGROUND_MODE => exec_guarded(request),
        _ => unreachable!("guard mode was validated while parsing"),
    }
}

#[cfg(unix)]
fn supervise(request: GuardRequest) -> io::Result<ExitStatus> {
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
        .arg(MONITOR_MODE)
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
fn monitor(request: GuardRequest) -> io::Result<ExitStatus> {
    #[cfg(target_os = "linux")]
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))?;
    let terminate = termination_flag()?;
    if parent_pid() != Some(request.expected_parent) {
        return Err(io::Error::other(
            "guard supervisor exited before monitor was armed",
        ));
    }

    let transfer_foreground = terminal_foreground_is(request.expected_parent)?;
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
        terminate_process_group(child_pid);
        let _status = child.wait();
        return Err(error);
    }

    loop {
        if let Some(status) = child.try_wait()? {
            terminate_process_group(child_pid);
            return Ok(status);
        }
        if terminate.load(std::sync::atomic::Ordering::Acquire)
            || parent_pid() != Some(request.expected_parent)
        {
            terminate_process_group(child_pid);
            return child.wait();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn exec_guarded(request: GuardRequest) -> io::Result<ExitStatus> {
    use std::os::unix::process::CommandExt as _;

    #[cfg(target_os = "linux")]
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
    if parent_pid() != Some(request.expected_parent) {
        return Err(io::Error::other(
            "guard supervisor exited before exec was armed",
        ));
    }
    rustix::process::setpgid(None, None)?;
    if request.mode == EXEC_FOREGROUND_MODE {
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::STOP)?;
    }
    let error = Command::new(request.program).args(request.arguments).exec();
    Err(error)
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
fn parent_pid() -> Option<u32> {
    rustix::process::getppid().map(|pid| pid.as_raw_nonzero().get() as u32)
}

#[cfg(unix)]
fn terminal_foreground_is(process_group: u32) -> io::Result<bool> {
    use std::io::IsTerminal as _;

    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let process_group = rustix::process::Pid::from_raw(process_group as i32)
        .ok_or_else(|| io::Error::other("terminal foreground process group is invalid"))?;
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
    if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
        let _result = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(windows)]
fn supervise(request: GuardRequest) -> io::Result<ExitStatus> {
    use process_wrap::std::{ChildWrapper as _, CommandWrap, JobObject};

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
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn monitor(_request: GuardRequest) -> io::Result<ExitStatus> {
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
