//! Command-line policy: startup flags, version identities, command registration,
//! and the dispatch seam used by the command-owning todos.
//!
//! This crate deliberately keeps policy separate from behavior. Adding a command
//! handler must not be able to change the compatibility version, forget startup
//! environment markers, or make another upstream command disappear. The frozen
//! `index.ts` fixture and [`validate_upstream_surface`] guard that boundary.

mod cmd;
mod command;
mod disposition;
mod environment;
pub mod startup;
mod version;

pub use command::{
    Action, Cli, CliLogLevel, Command, CommandDispatcher, DispatchArguments, DispatchError,
    DispatchRequest, ExportArgs, GlobalOptions, ImplementedCommand, ImportArgs, PENDING_COMMANDS,
    PendingCommandDispatcher, pending_reason,
};
pub use disposition::{
    CommandDisposition, Disposition, SurfaceError, disposition_for, dispositions,
    validate_upstream_surface,
};
pub use environment::{
    AGENT, OpenCodeFlags, StartupEnvironment, ZUNO, ZUNO_ENABLE_JS_PLUGINS, ZUNO_FLAG_NAMES,
    ZUNO_LOG_LEVEL, ZUNO_PID, ZUNO_PRINT_LOGS, ZUNO_PURE,
};
pub use version::{
    BUILD_ID, COMPATIBILITY_VERSION, RUST_PACKAGE_VERSION, compatibility_version, long_version,
    user_agent,
};

use std::ffi::OsString;
use std::io::Write as _;
use std::process::{ExitCode, Stdio};

use clap::{CommandFactory as _, Parser as _, error::ErrorKind};
use zuno_observability::LogConfig;
use zuno_paths::Env;

const BOOTSTRAP_MARKER: &str = "ZUNO_RUST_CLI_BOOTSTRAPPED";

/// Runs the process boundary with the default command dispatcher.
///
/// Startup uses one child process because Rust 2024 correctly makes global
/// environment mutation unsafe. `Command::env` is safe and gives command-owned
/// code the real `AGENT`, `OPENCODE`, PID, logging, and pure values it expects.
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
            compatibility_version().to_owned()
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

    if std::env::var_os(BOOTSTRAP_MARKER).is_none()
        && let Some(environment) = environment
    {
        profile.emit(startup::StartupPhase::BootstrapRestart);
        return restart_with_environment(&args, environment);
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
    let work = match &action {
        Action::Dispatch(request) if request.args.silence_is_a_stall() => {
            Some(watchdog.begin_work(watchdog.phase(WATCHDOG_DISPATCH_PHASE)))
        }
        Action::Dispatch(_) | Action::Rejected { .. } | Action::Version { .. } => None,
    };

    let code = execute_action(action, &mut cmd::HeadlessCommandDispatcher);

    drop(work);
    watchdog.shutdown();
    profile.emit(startup::StartupPhase::Dispatch);
    code
}

const WATCHDOG_DISPATCH_PHASE: &str = "cli.dispatch";

fn restart_with_environment(args: &[OsString], environment: &StartupEnvironment) -> ExitCode {
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
        .envs(environment.overrides())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        let error = command.exec();
        eprintln!("failed to replace zuno with its command process: {error}");
        ExitCode::FAILURE
    }

    #[cfg(not(unix))]
    match command.status() {
        Ok(status) => exit_code(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("failed to start zuno command process: {error}");
            ExitCode::FAILURE
        }
    }
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
                compatibility_version().to_owned()
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
