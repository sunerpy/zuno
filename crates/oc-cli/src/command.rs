//! clap registration and the stable hand-off point for command implementations.
//!
//! Todo 55 owns names and routing, not command behavior. The registered headless
//! commands retain their remaining argv and become a [`DispatchRequest`]. Todo 56
//! implements [`CommandDispatcher`] without replacing startup, version, or
//! disposition policy; todos 80-85 extend the same request path for maintenance.

use std::ffi::OsString;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use oc_observability::LogLevel;
use oc_paths::Env;

use crate::{StartupEnvironment, disposition_for};

/// The four log levels accepted by upstream's CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliLogLevel {
    /// Verbose diagnostic events.
    #[value(name = "DEBUG")]
    Debug,
    /// Normal operational events.
    #[value(name = "INFO")]
    Info,
    /// Warnings and errors.
    #[value(name = "WARN")]
    Warn,
    /// Errors only.
    #[value(name = "ERROR")]
    Error,
}

impl CliLogLevel {
    /// Uppercase spelling written to `OPENCODE_LOG_LEVEL`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    /// The logger's typed level, avoiding a second string parse.
    #[must_use]
    pub const fn log_level(self) -> LogLevel {
        match self {
            Self::Debug => LogLevel::Debug,
            Self::Info => LogLevel::Info,
            Self::Warn => LogLevel::Warn,
            Self::Error => LogLevel::Error,
        }
    }
}

/// Global settings applied before any command handler is reached.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalOptions {
    /// Add logs to stderr while retaining file logs.
    pub print_logs: bool,
    /// Override the environment's log level.
    pub log_level: Option<CliLogLevel>,
    /// Disable external plugins.
    pub pure: bool,
}

/// The root command parser.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "opencode-rust",
    about = "A Rust reimplementation of the OpenCode agent",
    disable_version_flag = true,
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    /// Show the plugin-compatible version number.
    #[arg(short = 'v', long, global = true, action = ArgAction::SetTrue)]
    pub version: bool,

    /// Include the real Rust build identity with the compatibility version.
    #[arg(long, global = true, requires = "version", action = ArgAction::SetTrue)]
    pub long: bool,

    /// Print logs to stderr in addition to the rolling log file.
    #[arg(long, global = true, action = ArgAction::SetTrue)]
    pub print_logs: bool,

    /// Set the minimum log level.
    #[arg(long, global = true, value_enum)]
    pub log_level: Option<CliLogLevel>,

    /// Run without external plugins.
    #[arg(long, global = true, action = ArgAction::SetTrue)]
    pub pure: bool,

    /// The selected command. No command is explicit while the TUI owner is pending.
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// The global values independent command implementations consume.
    #[must_use]
    pub const fn globals(&self) -> GlobalOptions {
        GlobalOptions {
            print_logs: self.print_logs,
            log_level: self.log_level,
            pure: self.pure,
        }
    }

    /// Converts parsed syntax into a policy action.
    #[must_use]
    pub fn action(self, base: &Env) -> Action {
        if self.version {
            return Action::Version { long: self.long };
        }

        let environment = StartupEnvironment::resolve(base, &self.globals());
        match self.command {
            Some(command) => command.action(environment),
            None => Action::Rejected {
                command: "$0",
                message: disposition_for("$0").map_or(
                    "the default TUI command is not registered yet; use a headless command",
                    |entry| entry.reason,
                ),
                environment,
            },
        }
    }
}

/// Remaining syntax owned by a later command implementation.
///
/// Keeping it opaque prevents this skeleton from copying per-command semantics.
/// Todo 56 replaces each use with typed arguments while preserving
/// [`DispatchRequest`] as the hand-off point.
#[derive(Debug, Clone, Default, Args)]
pub struct PendingArgs {
    /// Command-specific arguments.
    #[arg(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub args: Vec<OsString>,
}

/// Arguments accepted only so a rejected command can explain its replacement.
#[derive(Debug, Clone, Default, Args)]
pub struct RejectedArgs {
    /// Ignored legacy arguments.
    #[arg(
        value_name = "ARGS",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub args: Vec<OsString>,
}

/// Every command intentionally registered by this skeleton.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Run OpenCode with a message.
    Run(PendingArgs),
    /// Start the headless server.
    ///
    /// Its owner wraps [`oc_server::ServerBuilder`] through a dependency added by
    /// todo 56; it must not spawn `oc-server` or duplicate listener behavior.
    Serve(PendingArgs),
    /// Manage sessions.
    Session(PendingArgs),
    /// Manage agents.
    Agent(PendingArgs),
    /// List available models.
    Models(PendingArgs),
    /// Manage providers and credentials.
    #[command(alias = "auth")]
    Providers(PendingArgs),
    /// Manage Model Context Protocol servers.
    Mcp(PendingArgs),
    /// Database tools.
    Db(PendingArgs),
    /// Diagnostics and introspection.
    Debug(PendingArgs),
    /// Generate shell completion output.
    Completion(PendingArgs),
    /// Export session data.
    Export(PendingArgs),
    /// Import session data.
    Import(PendingArgs),

    /// Explain why the hosted Console is excluded.
    Console(RejectedArgs),
    /// Explain why the bundled web application is excluded.
    Web(RejectedArgs),
    /// Explain the supported statistics replacement.
    Stats(RejectedArgs),
    /// Explain the CI replacement for the hosted GitHub agent.
    Github(RejectedArgs),
    /// Explain the explicit GitHub CLI workflow.
    Pr(RejectedArgs),
    /// Explain how Rust releases are installed.
    Upgrade(RejectedArgs),
    /// Explain how this artifact is removed.
    Uninstall(RejectedArgs),
    /// Explain how to consume the runtime OpenAPI document.
    Generate(RejectedArgs),
}

impl Command {
    fn action(self, environment: StartupEnvironment) -> Action {
        match self {
            Self::Run(args) => dispatch(ImplementedCommand::Run, args, environment),
            Self::Serve(args) => dispatch(ImplementedCommand::Serve, args, environment),
            Self::Session(args) => dispatch(ImplementedCommand::Session, args, environment),
            Self::Agent(args) => dispatch(ImplementedCommand::Agent, args, environment),
            Self::Models(args) => dispatch(ImplementedCommand::Models, args, environment),
            Self::Providers(args) => dispatch(ImplementedCommand::Providers, args, environment),
            Self::Mcp(args) => dispatch(ImplementedCommand::Mcp, args, environment),
            Self::Db(args) => dispatch(ImplementedCommand::Db, args, environment),
            Self::Debug(args) => dispatch(ImplementedCommand::Debug, args, environment),
            Self::Completion(args) => dispatch(ImplementedCommand::Completion, args, environment),
            Self::Export(args) => dispatch(ImplementedCommand::Export, args, environment),
            Self::Import(args) => dispatch(ImplementedCommand::Import, args, environment),
            Self::Console(_) => reject("console", environment),
            Self::Web(_) => reject("web", environment),
            Self::Stats(_) => reject("stats", environment),
            Self::Github(_) => reject("github", environment),
            Self::Pr(_) => reject("pr", environment),
            Self::Upgrade(_) => reject("upgrade", environment),
            Self::Uninstall(_) => reject("uninstall", environment),
            Self::Generate(_) => reject("generate", environment),
        }
    }
}

fn dispatch(
    command: ImplementedCommand,
    args: PendingArgs,
    environment: StartupEnvironment,
) -> Action {
    Action::Dispatch(DispatchRequest {
        command,
        args: args.args,
        environment,
    })
}

fn reject(command: &'static str, environment: StartupEnvironment) -> Action {
    Action::Rejected {
        command,
        message: disposition_for(command).map_or(
            "this command is deliberately rejected by the Rust CLI",
            |entry| entry.reason,
        ),
        environment,
    }
}

/// Commands whose syntax is registered and whose behavior has a named owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplementedCommand {
    /// `run`.
    Run,
    /// `serve`; wraps the `oc-server` library rather than its standalone binary.
    Serve,
    /// `session`.
    Session,
    /// `agent`.
    Agent,
    /// `models`.
    Models,
    /// `providers`, including its `auth` alias.
    Providers,
    /// `mcp`.
    Mcp,
    /// `db`.
    Db,
    /// `debug`.
    Debug,
    /// `completion`.
    Completion,
    /// `export`.
    Export,
    /// `import`.
    Import,
}

impl ImplementedCommand {
    /// Canonical command spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Serve => "serve",
            Self::Session => "session",
            Self::Agent => "agent",
            Self::Models => "models",
            Self::Providers => "providers",
            Self::Mcp => "mcp",
            Self::Db => "db",
            Self::Debug => "debug",
            Self::Completion => "completion",
            Self::Export => "export",
            Self::Import => "import",
        }
    }
}

/// Stable boundary between CLI policy and command behavior.
#[derive(Debug, Clone)]
pub struct DispatchRequest {
    /// Command implementation to invoke.
    pub command: ImplementedCommand,
    /// Remaining arguments, to be replaced by typed command arguments in todo 56.
    pub args: Vec<OsString>,
    /// Resolved startup environment and all known flags.
    pub environment: StartupEnvironment,
}

/// Result of parsing and applying command-surface policy.
#[derive(Debug, Clone)]
pub enum Action {
    /// Print the requested version identity.
    Version { long: bool },
    /// Hand a registered command to its implementation.
    Dispatch(DispatchRequest),
    /// Fail with the deliberate scope or migration decision.
    Rejected {
        command: &'static str,
        message: &'static str,
        environment: StartupEnvironment,
    },
}

/// Behavior supplied by todo 56 and later command-owning todos.
pub trait CommandDispatcher {
    /// Execute one already-parsed command under the resolved environment.
    fn dispatch(&mut self, request: DispatchRequest) -> Result<(), DispatchError>;
}

/// A registered command reached a handler that has not landed yet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{command}` is registered, but its handler is pending {owner}")]
pub struct DispatchError {
    /// Canonical CLI spelling.
    pub command: &'static str,
    /// Todo that owns the implementation.
    pub owner: &'static str,
}

/// Honest default while todo 56 has not supplied the command handlers.
#[derive(Debug, Default)]
pub struct PendingCommandDispatcher;

impl CommandDispatcher for PendingCommandDispatcher {
    fn dispatch(&mut self, request: DispatchRequest) -> Result<(), DispatchError> {
        Err(DispatchError {
            command: request.command.as_str(),
            owner: "todo 56",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_registered_headless_commands_reach_the_dispatch_seam() {
        for name in [
            "run",
            "serve",
            "session",
            "agent",
            "models",
            "providers",
            "auth",
            "mcp",
            "db",
            "debug",
            "completion",
            "export",
            "import",
        ] {
            let cli = Cli::try_parse_from(["opencode-rust", name]).expect("registered command");
            let action = cli.action(&Env::empty());
            assert!(matches!(action, Action::Dispatch(_)), "{name}");
        }
    }

    #[test]
    fn every_rejected_command_reaches_a_deliberate_message() {
        for name in [
            "console",
            "web",
            "stats",
            "github",
            "pr",
            "upgrade",
            "uninstall",
            "generate",
        ] {
            let cli = Cli::try_parse_from(["opencode-rust", name]).expect("rejected command");
            let action = cli.action(&Env::empty());
            assert!(
                matches!(action, Action::Rejected { command, .. } if command == name),
                "{name}"
            );
        }
    }

    #[test]
    fn global_options_parse_before_and_after_a_command() {
        for args in [
            vec!["opencode-rust", "--print-logs", "--pure", "run"],
            vec!["opencode-rust", "run", "--print-logs", "--pure"],
        ] {
            let cli = Cli::try_parse_from(args).expect("global flags");
            assert!(cli.print_logs);
            assert!(cli.pure);
        }
    }

    #[test]
    fn log_level_accepts_only_the_four_upstream_spellings() {
        for level in ["DEBUG", "INFO", "WARN", "ERROR"] {
            let cli = Cli::try_parse_from(["opencode-rust", "--log-level", level, "run"])
                .expect("upstream log level");
            assert!(cli.log_level.is_some());
        }
        assert!(Cli::try_parse_from(["opencode-rust", "--log-level", "TRACE", "run"]).is_err());
    }
}
