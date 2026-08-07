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
    #[arg(long, global = true, hide = true, requires = "version", action = ArgAction::SetTrue)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum RunFormat {
    #[default]
    Default,
    Json,
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    #[arg(value_name = "message")]
    pub message: Vec<String>,
    #[arg(long)]
    pub command: Option<String>,
    #[arg(short = 'c', long)]
    pub r#continue: bool,
    #[arg(short = 's', long)]
    pub session: Option<String>,
    #[arg(long)]
    pub fork: bool,
    #[arg(long)]
    pub share: bool,
    #[arg(short = 'm', long)]
    pub model: Option<String>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long, value_enum, default_value_t)]
    pub format: RunFormat,
    #[arg(short = 'f', long)]
    pub file: Vec<String>,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub attach: Option<String>,
    #[arg(short = 'p', long)]
    pub password: Option<String>,
    #[arg(short = 'u', long)]
    pub username: Option<String>,
    #[arg(long)]
    pub dir: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub variant: Option<String>,
    #[arg(long)]
    pub thinking: bool,
    #[arg(short = 'i', long, default_value_t = false)]
    pub interactive: bool,
    #[arg(long, default_value_t = false)]
    pub auto: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    #[arg(long, default_value_t = 0)]
    pub port: u16,
    #[arg(long, default_value = "127.0.0.1")]
    pub hostname: String,
    #[arg(long, default_value_t = false)]
    pub mdns: bool,
    #[arg(long, default_value = "opencode.local")]
    pub mdns_domain: String,
    #[arg(long)]
    pub cors: Vec<String>,
}

#[derive(Debug, Clone, Args)]
#[command(subcommand_required = true)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: Option<SessionCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum SessionCommand {
    List(SessionListArgs),
    Prune(SessionPruneArgs),
    Delete { session_id: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum SessionFormat {
    #[default]
    Table,
    Json,
}

/// Which timestamp `session list` orders on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum SessionSortKey {
    /// `time_updated` — last activity. Upstream's `listGlobal` order.
    #[default]
    Updated,
    /// `time_created`.
    Created,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum SessionPruneKey {
    #[default]
    Updated,
    Created,
}

#[derive(Debug, Clone, Args)]
pub struct SessionPruneArgs {
    #[arg(long, value_name = "DAYS")]
    pub older_than: u64,
    #[arg(long, conflicts_with = "project")]
    pub all_projects: bool,
    #[arg(long, value_name = "PATH|ID")]
    pub project: Option<String>,
    #[arg(long, value_enum, default_value_t)]
    pub by: SessionPruneKey,
    #[arg(long, conflicts_with = "delete")]
    pub archive: bool,
    #[arg(long, conflicts_with = "archive")]
    pub delete: bool,
    #[arg(long)]
    pub include_shared: bool,
    #[arg(long)]
    pub include_recent: bool,
    #[arg(long)]
    pub force: bool,
    #[arg(long, requires = "delete")]
    pub yes: bool,
    #[arg(long, value_enum, default_value_t)]
    pub format: SessionFormat,
}

#[derive(Debug, Clone, Args)]
pub struct SessionListArgs {
    /// List sessions from every project, not just this checkout.
    #[arg(long, conflicts_with = "project")]
    pub all_projects: bool,
    /// List one project, named by its id or its worktree path.
    #[arg(long, value_name = "PATH|ID")]
    pub project: Option<String>,
    /// Include archived sessions alongside the live ones.
    #[arg(long)]
    pub archived: bool,
    /// Only root sessions. This is the default; pass `--no-roots` for children.
    #[arg(long, overrides_with = "no_roots")]
    pub roots: bool,
    /// Include child sessions, which are hidden by default.
    #[arg(long = "no-roots", overrides_with = "roots")]
    pub no_roots: bool,
    /// Order by last activity or by creation time.
    #[arg(long, value_enum, default_value_t)]
    pub sort: SessionSortKey,
    /// Limit to N sessions, most recent first. Defaults to 100.
    #[arg(short = 'n', long, visible_alias = "max-count")]
    pub limit: Option<u32>,
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    pub format: SessionFormat,
}

impl SessionListArgs {
    /// Whether the listing shows root sessions only.
    ///
    /// Roots-only is the default because that is what upstream's `session list`
    /// does — `svc.list({ roots: true, … })` with no way to turn it off
    /// (`cli/cmd/session.ts:87`). `--roots` therefore names the default rather
    /// than changing it, and `--no-roots` is the escape hatch upstream lacks.
    #[must_use]
    pub fn roots_only(&self) -> bool {
        self.roots || !self.no_roots
    }
}

#[derive(Debug, Clone, Args)]
#[command(subcommand_required = true)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub command: Option<AgentCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommand {
    Create(AgentCreateArgs),
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AgentMode {
    All,
    Primary,
    Subagent,
}

#[derive(Debug, Clone, Args)]
pub struct AgentCreateArgs {
    #[arg(long)]
    pub path: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long, value_enum)]
    pub mode: Option<AgentMode>,
    #[arg(long, visible_alias = "tools")]
    pub permissions: Option<String>,
    #[arg(short = 'm', long)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct ModelsArgs {
    pub provider: Option<String>,
    #[arg(long)]
    pub verbose: bool,
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Clone, Args)]
#[command(subcommand_required = true)]
pub struct ProvidersArgs {
    #[command(subcommand)]
    pub command: Option<ProvidersCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ProvidersCommand {
    #[command(alias = "ls")]
    List,
    Login {
        url: Option<String>,
        #[arg(short = 'p', long)]
        provider: Option<String>,
        #[arg(short = 'm', long)]
        method: Option<String>,
    },
    Logout {
        provider: Option<String>,
    },
}

#[derive(Debug, Clone, Args)]
#[command(subcommand_required = true)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: Option<McpCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum McpCommand {
    Add(McpAddArgs),
    #[command(alias = "ls")]
    List,
    Auth(McpAuthArgs),
    Logout {
        name: Option<String>,
    },
    Debug {
        name: String,
    },
}

#[derive(Debug, Clone, Args)]
pub struct McpAddArgs {
    pub name: Option<String>,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long)]
    pub env: Vec<String>,
    #[arg(long)]
    pub header: Vec<String>,
    #[arg(last = true)]
    pub server_command: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct McpAuthArgs {
    pub name: Option<String>,
    #[command(subcommand)]
    pub command: Option<McpAuthCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum McpAuthCommand {
    #[command(alias = "ls")]
    List,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum DbFormat {
    Json,
    #[default]
    Tsv,
}

#[derive(Debug, Clone, Args)]
pub struct DbArgs {
    pub query: Option<String>,
    #[arg(long, value_enum, default_value_t)]
    pub format: DbFormat,
}

#[derive(Debug, Clone, Args)]
#[command(subcommand_required = true)]
pub struct DebugArgs {
    #[command(subcommand)]
    pub command: Option<DebugCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DebugCommand {
    Paths,
    Config,
    Agent(DebugAgentArgs),
    Skill,
    Rg(DebugRgArgs),
    Lsp(DebugLspArgs),
    Snapshot(DebugSnapshotArgs),
}

#[derive(Debug, Clone, Args)]
pub struct DebugAgentArgs {
    pub name: String,
    #[arg(long)]
    pub tool: Option<String>,
    #[arg(long)]
    pub params: Option<String>,
}

#[derive(Debug, Clone, Args)]
#[command(subcommand_required = true)]
pub struct DebugRgArgs {
    #[command(subcommand)]
    pub command: Option<DebugRgCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DebugRgCommand {
    Files {
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        glob: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    Search {
        pattern: String,
        #[arg(long)]
        glob: Vec<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Debug, Clone, Args)]
#[command(subcommand_required = true)]
pub struct DebugLspArgs {
    #[command(subcommand)]
    pub command: Option<DebugLspCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DebugLspCommand {
    Diagnostics { file: String },
    Symbols { query: String },
    DocumentSymbols { uri: String },
}

#[derive(Debug, Clone, Args)]
#[command(subcommand_required = true)]
pub struct DebugSnapshotArgs {
    #[command(subcommand)]
    pub command: Option<DebugSnapshotCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DebugSnapshotCommand {
    Track,
    Patch { hash: String },
    Diff { hash: String },
}

/// Every command intentionally registered by this skeleton.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Run OpenCode with a message.
    Run(RunArgs),
    /// Start the headless server.
    ///
    /// Its owner wraps [`oc_server::ServerBuilder`] through a dependency added by
    /// todo 56; it must not spawn `oc-server` or duplicate listener behavior.
    Serve(ServeArgs),
    /// Manage sessions.
    Session(SessionArgs),
    /// Manage agents.
    Agent(AgentArgs),
    /// List available models.
    Models(ModelsArgs),
    /// Manage providers and credentials.
    #[command(alias = "auth")]
    Providers(ProvidersArgs),
    /// Manage Model Context Protocol servers.
    Mcp(McpArgs),
    /// Database tools.
    Db(DbArgs),
    /// Diagnostics and introspection.
    Debug(DebugArgs),
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
            Self::Run(args) => dispatch(DispatchArguments::Run(args), environment),
            Self::Serve(args) => dispatch(DispatchArguments::Serve(args), environment),
            Self::Session(args) => dispatch(DispatchArguments::Session(args), environment),
            Self::Agent(args) => dispatch(DispatchArguments::Agent(args), environment),
            Self::Models(args) => dispatch(DispatchArguments::Models(args), environment),
            Self::Providers(args) => dispatch(DispatchArguments::Providers(args), environment),
            Self::Mcp(args) => dispatch(DispatchArguments::Mcp(args), environment),
            Self::Db(args) => dispatch(DispatchArguments::Db(args), environment),
            Self::Debug(args) => dispatch(DispatchArguments::Debug(args), environment),
            Self::Completion(args) => dispatch(
                DispatchArguments::Pending(ImplementedCommand::Completion, args.args),
                environment,
            ),
            Self::Export(args) => dispatch(
                DispatchArguments::Pending(ImplementedCommand::Export, args.args),
                environment,
            ),
            Self::Import(args) => dispatch(
                DispatchArguments::Pending(ImplementedCommand::Import, args.args),
                environment,
            ),
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

fn dispatch(args: DispatchArguments, environment: StartupEnvironment) -> Action {
    Action::Dispatch(Box::new(DispatchRequest {
        command: args.command(),
        args,
        environment,
    }))
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

#[derive(Debug, Clone)]
pub enum DispatchArguments {
    Run(RunArgs),
    Serve(ServeArgs),
    Session(SessionArgs),
    Agent(AgentArgs),
    Models(ModelsArgs),
    Providers(ProvidersArgs),
    Mcp(McpArgs),
    Db(DbArgs),
    Debug(DebugArgs),
    Pending(ImplementedCommand, Vec<OsString>),
}

impl DispatchArguments {
    #[must_use]
    pub const fn command(&self) -> ImplementedCommand {
        match self {
            Self::Run(_) => ImplementedCommand::Run,
            Self::Serve(_) => ImplementedCommand::Serve,
            Self::Session(_) => ImplementedCommand::Session,
            Self::Agent(_) => ImplementedCommand::Agent,
            Self::Models(_) => ImplementedCommand::Models,
            Self::Providers(_) => ImplementedCommand::Providers,
            Self::Mcp(_) => ImplementedCommand::Mcp,
            Self::Db(_) => ImplementedCommand::Db,
            Self::Debug(_) => ImplementedCommand::Debug,
            Self::Pending(command, _) => *command,
        }
    }
}

/// Stable boundary between CLI policy and command behavior.
#[derive(Debug, Clone)]
pub struct DispatchRequest {
    /// Command implementation to invoke.
    pub command: ImplementedCommand,
    pub args: DispatchArguments,
    /// Resolved startup environment and all known flags.
    pub environment: StartupEnvironment,
}

/// Result of parsing and applying command-surface policy.
#[derive(Debug, Clone)]
pub enum Action {
    /// Print the requested version identity.
    Version { long: bool },
    /// Hand a registered command to its implementation.
    Dispatch(Box<DispatchRequest>),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchError {
    /// Canonical CLI spelling.
    pub command: &'static str,
    /// Todo that owns the implementation.
    pub owner: &'static str,
    pub detail: Option<String>,
}

impl DispatchError {
    #[must_use]
    pub fn command(command: ImplementedCommand, detail: impl Into<String>) -> Self {
        Self {
            command: command.as_str(),
            owner: "todo 56",
            detail: Some(detail.into()),
        }
    }
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(detail) => formatter.write_str(detail),
            None => write!(
                formatter,
                "`{}` is registered, but its handler is pending {}",
                self.command, self.owner
            ),
        }
    }
}

impl std::error::Error for DispatchError {}

/// Honest default while todo 56 has not supplied the command handlers.
#[derive(Debug, Default)]
pub struct PendingCommandDispatcher;

impl CommandDispatcher for PendingCommandDispatcher {
    fn dispatch(&mut self, request: DispatchRequest) -> Result<(), DispatchError> {
        Err(DispatchError {
            command: request.command.as_str(),
            owner: "todo 56",
            detail: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_registered_headless_commands_reach_the_dispatch_seam() {
        for args in [
            &["run"][..],
            &["serve"],
            &["session", "list"],
            &["agent", "list"],
            &["models"],
            &["providers", "list"],
            &["auth", "list"],
            &["mcp", "list"],
            &["db"],
            &["debug", "paths"],
            &["completion"],
            &["export"],
            &["import"],
        ] {
            let cli =
                Cli::try_parse_from(std::iter::once("opencode-rust").chain(args.iter().copied()))
                    .expect("registered command");
            let action = cli.action(&Env::empty());
            assert!(matches!(action, Action::Dispatch(_)), "{}", args[0]);
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

    #[test]
    fn session_prune_defaults_to_preview_for_the_current_project() {
        let cli = Cli::try_parse_from(["opencode-rust", "session", "prune", "--older-than", "90"])
            .expect("session prune parses");
        let Some(Command::Session(SessionArgs {
            command: Some(SessionCommand::Prune(args)),
        })) = cli.command
        else {
            panic!("expected session prune");
        };
        assert_eq!(args.older_than, 90);
        assert!(!args.all_projects);
        assert!(args.project.is_none());
        assert_eq!(args.by, SessionPruneKey::Updated);
        assert!(!args.archive);
        assert!(!args.delete);
        assert!(!args.yes);
        assert_eq!(args.format, SessionFormat::Table);
    }

    #[test]
    fn session_prune_accepts_the_complete_destructive_shape() {
        let cli = Cli::try_parse_from([
            "opencode-rust",
            "session",
            "prune",
            "--older-than",
            "30",
            "--all-projects",
            "--by",
            "created",
            "--delete",
            "--yes",
            "--include-shared",
            "--include-recent",
            "--force",
            "--format",
            "json",
        ])
        .expect("destructive session prune parses");
        let Some(Command::Session(SessionArgs {
            command: Some(SessionCommand::Prune(args)),
        })) = cli.command
        else {
            panic!("expected session prune");
        };
        assert!(args.all_projects);
        assert_eq!(args.by, SessionPruneKey::Created);
        assert!(args.delete);
        assert!(args.yes);
        assert!(args.include_shared);
        assert!(args.include_recent);
        assert!(args.force);
        assert_eq!(args.format, SessionFormat::Json);
    }

    #[test]
    fn session_prune_rejects_conflicting_scopes_and_actions() {
        assert!(
            Cli::try_parse_from([
                "opencode-rust",
                "session",
                "prune",
                "--older-than",
                "90",
                "--all-projects",
                "--project",
                "prj_a",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "opencode-rust",
                "session",
                "prune",
                "--older-than",
                "90",
                "--archive",
                "--delete",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "opencode-rust",
                "session",
                "prune",
                "--older-than",
                "90",
                "--yes",
            ])
            .is_err()
        );
    }
}
