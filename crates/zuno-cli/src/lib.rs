//! Command-line policy: startup flags, version identities, command registration,
//! and the dispatch seam used by the command-owning todos.
//!
//! This crate deliberately keeps policy separate from behavior. Adding a command
//! handler must not be able to change the package identity, forget startup
//! environment markers, or make another upstream command disappear. The frozen
//! `index.ts` fixture and [`validate_upstream_surface`] guard that boundary.

mod cmd;
mod command;
mod disposition;
mod environment;
pub mod startup;
mod version;

pub use command::{
    AcpArgs, Action, Cli, CliLogLevel, CliSandboxMode, CliSandboxUnavailableAction, Command,
    CommandDispatcher, CompletionArgs, DispatchArguments, DispatchError, DispatchRequest,
    ExportArgs, GlobalOptions, ImplementedCommand, ImportArgs, SelfUpdateArgs,
};
pub use disposition::{
    CommandDisposition, Disposition, SurfaceError, disposition_for, dispositions,
    validate_upstream_surface,
};
pub use environment::{
    AGENT, StartupEnvironment, ZUNO, ZUNO_FLAG_NAMES, ZUNO_LOG_LEVEL, ZUNO_PID, ZUNO_PRINT_LOGS,
    ZUNO_SANDBOX_MODE, ZUNO_SANDBOX_ON_UNAVAILABLE, ZunoFlags,
};
pub use version::{BUILD_ID, RUST_PACKAGE_VERSION, long_version, user_agent, version};

use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use clap::{CommandFactory as _, Parser as _, error::ErrorKind};
use zuno_observability::LogConfig;
use zuno_paths::Env;

/// Set in the replacement image so the Unix bootstrap restart happens exactly once.
#[cfg(unix)]
const BOOTSTRAP_MARKER: &str = "ZUNO_RUST_CLI_BOOTSTRAPPED";

/// Runs the process boundary with the default command dispatcher.
///
/// Rust 2024 correctly makes global environment mutation unsafe and this workspace
/// forbids unsafe code, so startup resolves the environment into a
/// [`StartupEnvironment`] value that command-owned code reads instead of writing
/// `AGENT`, `ZUNO`, PID, and logging values back into the process. Unix
/// additionally replaces this image once so its command process owns those
/// variables for real and every process it launches inherits them;
/// [`bootstrap_restart`] records why no other platform buys that with a second
/// process.
#[must_use]
pub fn run_process() -> ExitCode {
    // The guard work `main` did before calling in is already behind us, so the
    // first mark closes it rather than opening the profile.
    let mut profile = startup::StartupProfile::new();
    profile.mark(startup::StartupPhase::ProcessGuard);

    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            // `--help` and `--version` both leave through here, and both are
            // budgeted startup paths, so this arm has to emit or they would be the
            // two invocations the profile cannot describe.
            profile.mark(startup::StartupPhase::Parse);
            let code = render_clap_error(error);
            profile.emit(startup::StartupPhase::Dispatch);
            return code;
        }
    };
    profile.mark(startup::StartupPhase::Parse);

    let base = Env::from_process();
    let action = cli.action(&base);
    profile.mark(startup::StartupPhase::Environment);

    if let Action::Version { long } = action {
        let output = if long {
            long_version()
        } else {
            version().to_owned()
        };
        println!("{output}");
        profile.emit(startup::StartupPhase::Dispatch);
        return ExitCode::SUCCESS;
    }

    let environment = match &action {
        Action::Dispatch(request) => Some(&request.environment),
        Action::Rejected { environment, .. } => Some(environment),
        Action::Version { .. } => None,
    };

    if let Some(code) = bootstrap_restart(&mut profile, &args, environment) {
        return code;
    }

    let _logging = match init_logging(&action) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("failed to initialize logging: {error}");
            return ExitCode::FAILURE;
        }
    };
    profile.mark(startup::StartupPhase::Logging);

    // Started after logging so its reports have a sink, and only on the dispatch
    // path: the fast paths return before here, where a watchdog thread would be
    // spawned to observe a `println!`. The guard is taken only for commands whose
    // silence really is a stall — `DispatchArguments::silence_is_a_stall` records
    // why holding one across `tui` or `serve` would defeat the BUSY gate.
    let watchdog = zuno_observability::watchdog::Watchdog::spawn(
        zuno_observability::watchdog::WatchdogConfig::default(),
    );
    let dispatch_phase = watchdog.phase(WATCHDOG_DISPATCH_PHASE);
    let work = match &action {
        Action::Dispatch(request) if request.args.silence_is_a_stall() => {
            Some(watchdog.begin_work(dispatch_phase))
        }
        Action::Dispatch(_) | Action::Rejected { .. } | Action::Version { .. } => None,
    };

    // Only for the commands that live long enough for growth to mean something —
    // `DispatchArguments::deserves_a_memory_sampler` decides, exhaustively. Started here
    // rather than inside `tui` and `serve` so both are covered by one wiring: the four
    // attribution levels in `zuno_observability::memory` were implemented, tested, and
    // reachable from nothing, and two separate call sites are how the next one gets missed.
    let memory = match &action {
        Action::Dispatch(request) if request.args.deserves_a_memory_sampler() => {
            Some(zuno_observability::memory::MemorySampler::spawn(
                std::sync::Arc::clone(zuno_observability::memory::active_sessions()),
            ))
        }
        Action::Dispatch(_) | Action::Rejected { .. } | Action::Version { .. } => None,
    };

    let code = {
        let progress = || watchdog.beat(dispatch_phase);
        let progress = work.as_ref().map(|_| &progress as &dyn Fn());
        let mut dispatcher = cmd::HeadlessCommandDispatcher::new(progress);
        execute_action(action, &mut dispatcher)
    };

    drop(work);
    // Before the watchdog, so the sampler's last report still has a logging sink.
    if let Some(memory) = memory {
        memory.shutdown();
    }
    watchdog.shutdown();
    profile.emit(startup::StartupPhase::Dispatch);
    code
}

const WATCHDOG_DISPATCH_PHASE: &str = "cli.dispatch";

/// Hands the command process its bootstrap environment, where that is free.
///
/// Unix only, and deliberately so. `exec` *replaces* this image: one process, one
/// pid, and the handle a supervisor holds still names the process doing the work.
/// `Command::status` is the only thing a platform without `exec` could use, and it
/// is not the same thing at all — it starts a second process and leaves this one a
/// waiter holding the inherited stdio write ends. `Child::kill` is
/// `TerminateProcess` on Windows and ended only that waiter, so the command process
/// kept running, kept the pipes open, and a supervisor's `wait_with_output` never
/// reached end of file; an editor, an ACP client, or a test runner leaked one
/// `zuno.exe` per invocation. Platforms without `exec` therefore dispatch in the
/// process that parsed the arguments and read the same values from
/// [`StartupEnvironment`], which already carries every override.
///
/// Routing the restart through `zuno_process::guarded_argv` is not an alternative:
/// its Windows guard arms a Windows PowerShell parent watcher, and PowerShell is a
/// backend dependency of that guard, not of starting `zuno`.
#[cfg(unix)]
fn bootstrap_restart(
    profile: &mut startup::StartupProfile,
    args: &[OsString],
    environment: Option<&StartupEnvironment>,
) -> Option<ExitCode> {
    let environment = environment?;
    if std::env::var_os(BOOTSTRAP_MARKER).is_some() {
        return None;
    }
    profile.emit(startup::StartupPhase::BootstrapRestart);
    Some(restart_with_environment(args, environment))
}

/// Dispatch happens in this process; the Unix arm records why.
#[cfg(not(unix))]
fn bootstrap_restart(
    _profile: &mut startup::StartupProfile,
    _args: &[OsString],
    _environment: Option<&StartupEnvironment>,
) -> Option<ExitCode> {
    None
}

/// Replaces this image with one that owns the resolved bootstrap environment.
///
/// Returns only on failure: a successful `exec` continues in [`run_process`] with
/// the marker set. Stdio is inherited by default and `exec` keeps the descriptors,
/// so a supervisor's pipes stay attached to the same pid.
#[cfg(unix)]
fn restart_with_environment(args: &[OsString], environment: &StartupEnvironment) -> ExitCode {
    use std::os::unix::process::CommandExt as _;

    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("failed to locate zuno for environment bootstrap: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut command = std::process::Command::new(executable);
    command
        .args(args.iter().skip(1))
        .env(BOOTSTRAP_MARKER, "1")
        .envs(environment.overrides());

    let error = command.exec();
    eprintln!("failed to replace zuno with its command process: {error}");
    ExitCode::FAILURE
}

fn init_logging(
    action: &Action,
) -> Result<zuno_observability::LogHandle, zuno_observability::LogInitError> {
    let environment = match action {
        Action::Dispatch(request) => &request.environment,
        Action::Rejected { environment, .. } => environment,
        Action::Version { .. } => {
            return zuno_observability::init(LogConfig::from_env(zuno_paths::log()));
        }
    };
    let mut config = LogConfig::from_env(zuno_paths::log());
    if let Some(raw) = environment.flags.value(ZUNO_LOG_LEVEL)
        && let Some(level) = zuno_observability::LogLevel::parse(raw)
    {
        config = config.with_level(level);
    }
    if environment.flags.value(ZUNO_PRINT_LOGS) == Some("1") {
        config = config.with_print_logs(true);
    }
    zuno_observability::init(config)
}

/// Executes an action with a caller-supplied command implementation.
#[must_use]
pub fn execute_action(action: Action, dispatcher: &mut dyn CommandDispatcher) -> ExitCode {
    match action {
        Action::Version { long } => {
            let output = if long {
                long_version()
            } else {
                version().to_owned()
            };
            println!("{output}");
            ExitCode::SUCCESS
        }
        Action::Dispatch(request) => match dispatcher.dispatch(*request) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        Action::Rejected {
            command, message, ..
        } => {
            eprintln!("`{command}` is not available: {message}");
            ExitCode::FAILURE
        }
    }
}

fn render_clap_error(error: clap::Error) -> ExitCode {
    let code = match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
        _ => 2,
    };
    if let Err(write_error) = error.print() {
        let _ = writeln!(
            std::io::stderr(),
            "failed to render CLI error: {write_error}"
        );
        return ExitCode::FAILURE;
    }
    exit_code(code)
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// The root clap command, useful to completion generators and surface tests.
#[must_use]
pub fn clap_command() -> clap::Command {
    Cli::command()
}
