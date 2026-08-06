//! Command-line policy: startup flags, version identities, command registration,
//! and the dispatch seam used by the command-owning todos.
//!
//! This crate deliberately keeps policy separate from behavior. Adding a command
//! handler must not be able to change the compatibility version, forget startup
//! environment markers, or make another upstream command disappear. The frozen
//! `index.ts` fixture and [`validate_upstream_surface`] guard that boundary.

mod command;
mod disposition;
mod environment;
mod version;

pub use command::{
    Action, Cli, CliLogLevel, Command, CommandDispatcher, DispatchError, DispatchRequest,
    GlobalOptions, ImplementedCommand, PendingCommandDispatcher,
};
pub use disposition::{
    CommandDisposition, Disposition, SurfaceError, disposition_for, dispositions,
    validate_upstream_surface,
};
pub use environment::{
    AGENT, OPENCODE, OPENCODE_FLAG_NAMES, OPENCODE_LOG_LEVEL, OPENCODE_PID, OPENCODE_PRINT_LOGS,
    OPENCODE_PURE, OpenCodeFlags, StartupEnvironment,
};
pub use version::{
    BUILD_ID, COMPATIBILITY_VERSION, RUST_PACKAGE_VERSION, compatibility_version, long_version,
    user_agent,
};

use std::ffi::OsString;
use std::io::Write as _;
use std::process::{ExitCode, Stdio};

use clap::{CommandFactory as _, Parser as _, error::ErrorKind};
use oc_observability::LogConfig;
use oc_paths::Env;

const BOOTSTRAP_MARKER: &str = "OC_RUST_CLI_BOOTSTRAPPED";

/// Runs the process boundary with the default command dispatcher.
///
/// Startup uses one child process because Rust 2024 correctly makes global
/// environment mutation unsafe. `Command::env` is safe and gives command-owned
/// code the real `AGENT`, `OPENCODE`, PID, logging, and pure values it expects.
#[must_use]
pub fn run_process() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => return render_clap_error(error),
    };
    let base = Env::from_process();
    let action = cli.action(&base);

    if let Action::Version { long } = action {
        let output = if long {
            long_version()
        } else {
            compatibility_version().to_owned()
        };
        println!("{output}");
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
        return restart_with_environment(&args, environment);
    }

    let _logging = match init_logging(&action) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("failed to initialize logging: {error}");
            return ExitCode::FAILURE;
        }
    };

    execute_action(action, &mut PendingCommandDispatcher)
}

fn restart_with_environment(args: &[OsString], environment: &StartupEnvironment) -> ExitCode {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("failed to locate opencode-rust for environment bootstrap: {error}");
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
        eprintln!("failed to replace opencode-rust with its command process: {error}");
        ExitCode::FAILURE
    }

    #[cfg(not(unix))]
    match command.status() {
        Ok(status) => exit_code(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("failed to start opencode-rust command process: {error}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(
    action: &Action,
) -> Result<oc_observability::LogHandle, oc_observability::LogInitError> {
    let environment = match action {
        Action::Dispatch(request) => &request.environment,
        Action::Rejected { environment, .. } => environment,
        Action::Version { .. } => {
            return oc_observability::init(LogConfig::from_env(oc_paths::log()));
        }
    };
    let mut config = LogConfig::from_env(oc_paths::log());
    if let Some(raw) = environment.flags.value(OPENCODE_LOG_LEVEL)
        && let Some(level) = oc_observability::LogLevel::parse(raw)
    {
        config = config.with_level(level);
    }
    if environment.flags.value(OPENCODE_PRINT_LOGS) == Some("1") {
        config = config.with_print_logs(true);
    }
    oc_observability::init(config)
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
        Action::Dispatch(request) => match dispatcher.dispatch(request) {
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
