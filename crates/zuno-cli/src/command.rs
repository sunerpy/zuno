//! clap registration and the stable hand-off point for command implementations.
//!
//! Todo 55 owns names and routing, not command behavior. The registered headless
//! commands retain their remaining argv and become a [`DispatchRequest`]. Todo 56
//! implements [`CommandDispatcher`] without replacing startup, version, or
//! disposition policy; todos 80-85 extend the same request path for maintenance.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use zuno_observability::LogLevel;
use zuno_paths::Env;

use crate::{StartupEnvironment, disposition_for};

/// Operational log levels accepted by the Zuno CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliLogLevel {
    /// Maximum tracing detail.
    #[value(name = "TRACE")]
    Trace,
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
    /// Uppercase spelling written to `ZUNO_LOG_LEVEL`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
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
            Self::Trace => LogLevel::Trace,
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
    /// Add logs to stderr while retaining the structured local store.
    pub print_logs: bool,
    /// Override the environment's log level.
    pub log_level: Option<CliLogLevel>,
    /// Override sandbox authority for this invocation.
    pub sandbox: Option<CliSandboxMode>,
}

/// The root command parser.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "zuno",
    about = "Zuno — Zero code. Any task.",
    disable_version_flag = true,
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    /// Show the Zuno package version.
    #[arg(short = 'v', long, global = true, action = ArgAction::SetTrue)]
    pub version: bool,

    /// Include the build identity with the package version.
    #[arg(long, global = true, hide = true, requires = "version", action = ArgAction::SetTrue)]
    pub long: bool,

    /// Print logs to stderr in addition to the structured local log store.
    #[arg(long, global = true, action = ArgAction::SetTrue)]
    pub print_logs: bool,

    /// Set the minimum log level.
    #[arg(long, global = true, value_enum)]
    pub log_level: Option<CliLogLevel>,

    /// Select Shell confinement for this invocation.
    #[arg(long, global = true, value_enum)]
    pub sandbox: Option<CliSandboxMode>,

    /// The default command's own options, accepted without naming it.
    #[command(flatten)]
    pub tui: TuiArgs,

    /// The selected command. Absent means the interactive TUI, as upstream's `$0`.
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
            sandbox: self.sandbox,
        }
    }

    /// Converts parsed syntax into a policy action.
    #[must_use]
    pub fn action(self, base: &Env) -> Action {
        if self.version {
            return Action::Version { long: self.long };
        }

        let environment = StartupEnvironment::resolve(base, &self.globals());
        let root_tui = self.tui;
        match self.command {
            Some(command) => command.action(environment),
            // Upstream's default command is the TUI, so a bare invocation dispatches
            // exactly what `tui` does rather than explaining an absence — including
            // the options it was given without the subcommand's name.
            None => dispatch(DispatchArguments::Tui(root_tui), environment),
        }
    }
}

/// Public Shell sandbox modes accepted by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliSandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CliSandboxMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
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

/// The interactive terminal application, which takes no arguments of its own.
///
/// A struct rather than a unit variant so the dispatch seam keeps one shape for
/// every command, and so a later flag does not change the variant's arity.
///
/// These are upstream's `tui` options (`cli/cmd/tui.ts:81-113`), and they are the
/// invocation an unattended caller uses: `--prompt` submits a turn without a
/// keystroke. They are flattened onto the root command as well, because upstream's
/// default command *is* the TUI and a bare `opencode --prompt …` has to reach it.
#[derive(Debug, Clone, Default, Args)]
pub struct TuiArgs {
    /// Submit this prompt on start, as though it had been typed and sent.
    #[arg(long)]
    pub prompt: Option<String>,
    /// The model to use, as `provider/model`.
    #[arg(short = 'm', long)]
    pub model: Option<String>,
    /// The agent to use.
    #[arg(long)]
    pub agent: Option<String>,
    /// Continue the most recent session in this directory.
    #[arg(short = 'c', long)]
    pub r#continue: bool,
    /// Talk in this exact session.
    #[arg(short = 's', long)]
    pub session: Option<String>,
    /// Approve every permission that is not explicitly denied, without asking.
    ///
    /// Upstream's own description ends in "(dangerous!)" and it means it: this
    /// replaces the human at the permission prompt, so a tool call the default
    /// ruleset would have stopped to ask about proceeds unattended.
    #[arg(long)]
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
    #[arg(long, default_value = "zuno.local")]
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
    /// List the login methods implemented for a provider.
    Methods {
        /// Provider id or display name.
        provider: String,
    },
    /// Authenticate a provider with one of its implemented login methods.
    Login {
        /// Provider id/name, or an HTTPS URL implementing `/.well-known/zuno`.
        /// Omit in a terminal to choose interactively.
        target: Option<String>,
        /// Provider id or display name, as an alternative to the positional target.
        #[arg(short = 'p', long)]
        provider: Option<String>,
        /// Method id shown by `zuno auth methods <provider>`.
        /// Omit in a terminal to choose when several methods are available.
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

#[derive(Debug, Clone, Args)]
#[command(subcommand_required = true)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: Option<PluginCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum PluginCommand {
    /// List packages active for one directory.
    #[command(alias = "ls")]
    List {
        /// Directory whose project configuration chain should be inspected.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Install a new local package.
    Add(PluginInstallArgs),
    /// Transactionally replace an installed local package.
    Update(PluginInstallArgs),
    /// Remove an installed package.
    Remove(PluginRemoveArgs),
}

#[derive(Debug, Clone, Args)]
pub struct PluginInstallArgs {
    /// Package directory or its extension.json manifest.
    pub source: PathBuf,
    /// Install below the selected project's `.zuno` directory instead of globally.
    #[arg(long)]
    pub project: bool,
    /// Directory used to select the project target.
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct PluginRemoveArgs {
    /// Stable package id.
    pub id: String,
    /// Remove from the selected project's `.zuno` directory instead of globally.
    #[arg(long)]
    pub project: bool,
    /// Directory used to select the project target.
    #[arg(long)]
    pub dir: Option<PathBuf>,
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

/// Export the portable Zuno user environment.
#[derive(Debug, Clone, Args)]
pub struct ExportArgs {
    /// Bundle path; defaults to `zuno-export-<UTC timestamp>.zuno-bundle`.
    #[arg(value_name = "bundle")]
    pub output: Option<PathBuf>,
    /// Include provider and MCP credential stores in the unencrypted bundle.
    #[arg(long)]
    pub include_credentials: bool,
    /// Replace an existing output file.
    #[arg(long)]
    pub force: bool,
}

/// Import a portable Zuno user environment.
#[derive(Debug, Clone, Args)]
pub struct ImportArgs {
    /// Path to a `.zuno-bundle` produced by `zuno export`.
    #[arg(value_name = "bundle")]
    pub file: PathBuf,
    /// Transactionally replace non-empty target roots.
    #[arg(long)]
    pub replace: bool,
    /// Validate and report the import without changing files.
    #[arg(long)]
    pub dry_run: bool,
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
    Prompt(DebugPromptArgs),
    Permissions,
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
pub struct DebugPromptArgs {
    /// Session whose prompt receipt should be shown; defaults to the latest receipt.
    #[arg(value_name = "session")]
    pub session_id: Option<String>,
    /// One-based model step within the session; defaults to its latest receipt.
    #[arg(value_name = "turn")]
    pub turn: Option<u32>,
    /// Include model-visible instruction, AGENTS, skill, and memory content.
    #[arg(long)]
    pub show_sensitive: bool,
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

/// Generate a shell completion script from Zuno's current clap command tree.
#[derive(Debug, Clone, Args)]
pub struct CompletionArgs {
    /// Shell whose completion syntax should be emitted.
    #[arg(value_enum, value_name = "SHELL")]
    pub shell: clap_complete::Shell,
}

/// Update the running executable from a verified GitHub release.
#[derive(Debug, Clone, Args)]
pub struct SelfUpdateArgs {
    /// Report whether a newer release exists without changing the executable.
    #[arg(long, conflicts_with_all = ["force", "tag", "yes"])]
    pub check: bool,
    /// Reinstall the selected release even when it is not newer.
    #[arg(long)]
    pub force: bool,
    /// Install one explicit release tag instead of the latest release.
    #[arg(long, value_name = "vX.Y.Z")]
    pub tag: Option<String>,
    /// Replace the executable without an interactive confirmation.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Serve Agent Client Protocol over stdin/stdout for editor integrations.
#[derive(Debug, Clone, Args)]
pub struct AcpArgs {
    /// Validate that the production ACP adapter is available, then exit.
    #[arg(long)]
    pub check: bool,
}

/// Every command intentionally registered by this skeleton.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Serve Agent Client Protocol over stdin/stdout.
    Acp(AcpArgs),
    /// Run Zuno with a message.
    Run(RunArgs),
    /// Start the interactive terminal application. Also the default with no command.
    Tui(TuiArgs),
    /// Start the headless server.
    ///
    /// Its owner wraps [`zuno_server::ServerBuilder`] through a dependency added by
    /// todo 56; it must not spawn `zuno-server` or duplicate listener behavior.
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
    /// Install and inspect Zuno extension plugins.
    Plugin(PluginArgs),
    /// Database tools.
    Db(DbArgs),
    /// Diagnostics and introspection.
    Debug(DebugArgs),
    /// Generate shell completion output.
    Completion(CompletionArgs),
    /// Update Zuno in place from a checksum-verified GitHub release.
    SelfUpdate(SelfUpdateArgs),
    /// Export Zuno configuration, Skills, extensions, Agents, and other user assets.
    Export(ExportArgs),
    /// Import a portable Zuno user-environment bundle.
    Import(ImportArgs),

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
    /// Explain how this artifact is removed.
    Uninstall(RejectedArgs),
    /// Explain how to consume the runtime OpenAPI document.
    Generate(RejectedArgs),
}

impl Command {
    fn action(self, environment: StartupEnvironment) -> Action {
        match self {
            Self::Acp(args) => dispatch(DispatchArguments::Acp(args), environment),
            Self::Run(args) => dispatch(DispatchArguments::Run(args), environment),
            Self::Tui(args) => dispatch(DispatchArguments::Tui(args), environment),
            Self::Serve(args) => dispatch(DispatchArguments::Serve(args), environment),
            Self::Session(args) => dispatch(DispatchArguments::Session(args), environment),
            Self::Agent(args) => dispatch(DispatchArguments::Agent(args), environment),
            Self::Models(args) => dispatch(DispatchArguments::Models(args), environment),
            Self::Providers(args) => dispatch(DispatchArguments::Providers(args), environment),
            Self::Mcp(args) => dispatch(DispatchArguments::Mcp(args), environment),
            Self::Plugin(args) => dispatch(DispatchArguments::Plugin(args), environment),
            Self::Db(args) => dispatch(DispatchArguments::Db(args), environment),
            Self::Debug(args) => dispatch(DispatchArguments::Debug(args), environment),
            Self::Completion(args) => dispatch(DispatchArguments::Completion(args), environment),
            Self::SelfUpdate(args) => dispatch(DispatchArguments::SelfUpdate(args), environment),
            Self::Export(args) => dispatch(DispatchArguments::Export(args), environment),
            Self::Import(args) => dispatch(DispatchArguments::Import(args), environment),
            Self::Console(_) => reject("console", environment),
            Self::Web(_) => reject("web", environment),
            Self::Stats(_) => reject("stats", environment),
            Self::Github(_) => reject("github", environment),
            Self::Pr(_) => reject("pr", environment),
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
    /// `acp`.
    Acp,
    /// `run`.
    Run,
    /// `tui`, and the bare invocation upstream registers as `$0`.
    Tui,
    /// `serve`; wraps the `zuno-server` library rather than its standalone binary.
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
    /// `plugin`.
    Plugin,
    /// `db`.
    Db,
    /// `debug`.
    Debug,
    /// `completion`.
    Completion,
    /// `self-update`.
    SelfUpdate,
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
            Self::Acp => "acp",
            Self::Run => "run",
            Self::Tui => "tui",
            Self::Serve => "serve",
            Self::Session => "session",
            Self::Agent => "agent",
            Self::Models => "models",
            Self::Providers => "providers",
            Self::Mcp => "mcp",
            Self::Plugin => "plugin",
            Self::Db => "db",
            Self::Debug => "debug",
            Self::Completion => "completion",
            Self::SelfUpdate => "self-update",
            Self::Export => "export",
            Self::Import => "import",
        }
    }
}

#[derive(Debug, Clone)]
pub enum DispatchArguments {
    Acp(AcpArgs),
    Run(RunArgs),
    Tui(TuiArgs),
    Serve(ServeArgs),
    Session(SessionArgs),
    Agent(AgentArgs),
    Models(ModelsArgs),
    Providers(ProvidersArgs),
    Mcp(McpArgs),
    Plugin(PluginArgs),
    Db(DbArgs),
    Debug(DebugArgs),
    Completion(CompletionArgs),
    SelfUpdate(SelfUpdateArgs),
    Export(ExportArgs),
    Import(ImportArgs),
}

impl DispatchArguments {
    #[must_use]
    pub const fn command(&self) -> ImplementedCommand {
        match self {
            Self::Acp(_) => ImplementedCommand::Acp,
            Self::Run(_) => ImplementedCommand::Run,
            Self::Tui(_) => ImplementedCommand::Tui,
            Self::Serve(_) => ImplementedCommand::Serve,
            Self::Session(_) => ImplementedCommand::Session,
            Self::Agent(_) => ImplementedCommand::Agent,
            Self::Models(_) => ImplementedCommand::Models,
            Self::Providers(_) => ImplementedCommand::Providers,
            Self::Mcp(_) => ImplementedCommand::Mcp,
            Self::Plugin(_) => ImplementedCommand::Plugin,
            Self::Db(_) => ImplementedCommand::Db,
            Self::Debug(_) => ImplementedCommand::Debug,
            Self::Completion(_) => ImplementedCommand::Completion,
            Self::SelfUpdate(_) => ImplementedCommand::SelfUpdate,
            Self::Export(_) => ImplementedCommand::Export,
            Self::Import(_) => ImplementedCommand::Import,
        }
    }

    /// Whether this command's silence is evidence of a stall.
    ///
    /// The liveness watchdog holds a `WorkGuard` for the whole of a command that
    /// answers this `true`, so a stall in one is reported. `false` means the
    /// command legitimately blocks on something outside the process — a key
    /// press, an inbound request — and holding a guard across it would report a
    /// stall every stall interval while the user reads the screen. That is the
    /// exact false positive `zuno_observability::watchdog`'s BUSY gate exists to
    /// prevent, so getting this classification wrong defeats the gate rather than
    /// merely being noisy.
    ///
    /// Exhaustive, so a new command has to decide which it is. `Run` is bounded
    /// because it takes one turn and exits; `Tui` and `Serve` are not.
    /// An interactive command that wants coverage should take its own guards
    /// around its bounded segments rather than being reclassified here.
    #[must_use]
    pub const fn silence_is_a_stall(&self) -> bool {
        match self {
            Self::Run(_)
            | Self::Session(_)
            | Self::Agent(_)
            | Self::Models(_)
            | Self::Providers(_)
            | Self::Mcp(_)
            | Self::Plugin(_)
            | Self::Db(_)
            | Self::Debug(_)
            | Self::Completion(_)
            | Self::SelfUpdate(_)
            | Self::Export(_)
            | Self::Import(_) => true,
            Self::Acp(_) | Self::Tui(_) | Self::Serve(_) => false,
        }
    }

    /// Whether this command runs long enough for resident growth to mean anything.
    ///
    /// A memory sampler is started for the whole of a command that answers this `true`.
    /// It is a separate question from [`Self::silence_is_a_stall`] and not its negation:
    /// that one asks whether *silence* is a fault, this one asks whether the process lives
    /// long enough to leak. `Pending` answers `false` to both — it prints and exits — so
    /// the two lists differ by exactly that arm, which is why deriving one from the other
    /// would be wrong.
    ///
    /// Exhaustive, so a new long-running command cannot be added without deciding. That
    /// matters here more than most: [`zuno_observability::memory`]'s four attribution
    /// levels were fully implemented and tested, and shipped with **no** production
    /// sampler at all, so the alert could not fire however far memory grew. A predicate
    /// the compiler forces a choice on is what keeps the next command from re-creating
    /// that hole silently.
    #[must_use]
    pub const fn deserves_a_memory_sampler(&self) -> bool {
        match self {
            Self::Acp(_) | Self::Tui(_) | Self::Serve(_) => true,
            Self::Run(_)
            | Self::Session(_)
            | Self::Agent(_)
            | Self::Models(_)
            | Self::Providers(_)
            | Self::Mcp(_)
            | Self::Plugin(_)
            | Self::Db(_)
            | Self::Debug(_)
            | Self::Completion(_)
            | Self::SelfUpdate(_)
            | Self::Export(_)
            | Self::Import(_) => false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zuno_binary_name_is_pinned_in_manifest() {
        let manifest = include_str!("../Cargo.toml");
        let binary = manifest
            .split_once("[[bin]]")
            .map(|(_, binary)| binary)
            .expect("zuno-cli must declare its executable explicitly");
        let binary = binary
            .split_once("[dependencies]")
            .map_or(binary, |(binary, _)| binary);

        assert!(
            binary.lines().any(|line| line.trim() == "name = \"zuno\""),
            "the shipped executable must be named `zuno`; reverting the binary rename is a user-facing regression"
        );
    }

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
            &["plugin", "list"],
            &["db"],
            &["debug", "paths"],
            &["tui"],
            &["completion", "bash"],
            &["self-update", "--check"],
            &["export"],
            &["import", "environment.zuno-bundle"],
        ] {
            let cli = Cli::try_parse_from(std::iter::once("zuno").chain(args.iter().copied()))
                .expect("registered command");
            let action = cli.action(&Env::empty());
            assert!(matches!(action, Action::Dispatch(_)), "{}", args[0]);
        }
    }

    #[test]
    fn a_bare_invocation_dispatches_the_tui_like_upstreams_default_command() {
        // Upstream registers the TUI as `$0`, so a bare invocation must reach the
        // same handler as `tui` rather than explain that no command was given.
        let cli = Cli::try_parse_from(["zuno"]).expect("a bare invocation parses");
        let Action::Dispatch(request) = cli.action(&Env::empty()) else {
            panic!("a bare invocation must dispatch");
        };
        assert_eq!(request.command, ImplementedCommand::Tui);
        assert!(matches!(request.args, DispatchArguments::Tui(_)));
    }

    #[test]
    fn every_rejected_command_reaches_a_deliberate_message() {
        for name in [
            "console",
            "web",
            "stats",
            "github",
            "pr",
            "uninstall",
            "generate",
        ] {
            let cli = Cli::try_parse_from(["zuno", name]).expect("rejected command");
            let action = cli.action(&Env::empty());
            assert!(
                matches!(action, Action::Rejected { command, .. } if command == name),
                "{name}"
            );
        }
    }

    #[test]
    fn self_update_parses_checked_and_explicit_install_modes() {
        let checked =
            Cli::try_parse_from(["zuno", "self-update", "--check"]).expect("check parses");
        let Some(Command::SelfUpdate(checked)) = checked.command else {
            panic!("expected self-update command");
        };
        assert!(checked.check);
        assert!(!checked.force);
        assert!(checked.tag.is_none());
        assert!(!checked.yes);

        let install =
            Cli::try_parse_from(["zuno", "self-update", "--tag", "v0.2.0", "--force", "--yes"])
                .expect("explicit install parses");
        let Some(Command::SelfUpdate(install)) = install.command else {
            panic!("expected self-update command");
        };
        assert!(!install.check);
        assert!(install.force);
        assert_eq!(install.tag.as_deref(), Some("v0.2.0"));
        assert!(install.yes);
    }

    #[test]
    fn self_update_check_rejects_mutating_options() {
        for conflicting in ["--force", "--tag", "--yes"] {
            let mut args = vec!["zuno", "self-update", "--check", conflicting];
            if conflicting == "--tag" {
                args.push("v0.2.0");
            }
            assert!(
                Cli::try_parse_from(args).is_err(),
                "--check must conflict with {conflicting}"
            );
        }
    }

    #[test]
    fn global_options_parse_before_and_after_a_command() {
        for args in [
            vec!["zuno", "--print-logs", "run"],
            vec!["zuno", "run", "--print-logs"],
        ] {
            let cli = Cli::try_parse_from(args).expect("global flags");
            assert!(cli.print_logs);
        }
    }

    #[test]
    fn sandbox_mode_is_a_global_trusted_invocation_override() {
        for (spelling, expected) in [
            ("read-only", "read-only"),
            ("workspace-write", "workspace-write"),
            ("danger-full-access", "danger-full-access"),
        ] {
            let cli = Cli::try_parse_from(["zuno", "--sandbox", spelling, "run"])
                .unwrap_or_else(|error| panic!("{spelling} must parse: {error}"));
            let Action::Dispatch(request) = cli.action(&Env::empty()) else {
                panic!("sandbox override must still dispatch");
            };
            assert_eq!(
                request.environment.flags.value(crate::ZUNO_SANDBOX_MODE),
                Some(expected)
            );
        }
    }

    #[test]
    fn removed_pure_flag_is_rejected() {
        assert!(Cli::try_parse_from(["zuno", "--pure"]).is_err());
    }

    #[test]
    fn serve_uses_the_zuno_mdns_domain_by_default() {
        let cli = Cli::try_parse_from(["zuno", "serve"]).expect("serve parses");
        let Some(Command::Serve(args)) = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(args.mdns_domain, "zuno.local");
    }

    #[test]
    fn log_level_accepts_zunos_five_spellings() {
        for level in ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"] {
            let cli =
                Cli::try_parse_from(["zuno", "--log-level", level, "run"]).expect("zuno log level");
            assert!(cli.log_level.is_some());
        }
        assert!(Cli::try_parse_from(["zuno", "--log-level", "VERBOSE", "run"]).is_err());
    }

    #[test]
    fn session_prune_defaults_to_preview_for_the_current_project() {
        let cli = Cli::try_parse_from(["zuno", "session", "prune", "--older-than", "90"])
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
            "zuno",
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
    fn debug_prompt_accepts_optional_session_turn_and_sensitive_flag() {
        let cli = Cli::try_parse_from([
            "zuno",
            "debug",
            "prompt",
            "ses_example",
            "7",
            "--show-sensitive",
        ])
        .expect("debug prompt parses");
        let Some(Command::Debug(DebugArgs {
            command: Some(DebugCommand::Prompt(args)),
        })) = cli.command
        else {
            panic!("expected debug prompt");
        };
        assert_eq!(args.session_id.as_deref(), Some("ses_example"));
        assert_eq!(args.turn, Some(7));
        assert!(args.show_sensitive);

        let latest =
            Cli::try_parse_from(["zuno", "debug", "prompt"]).expect("latest prompt receipt parses");
        let Some(Command::Debug(DebugArgs {
            command: Some(DebugCommand::Prompt(args)),
        })) = latest.command
        else {
            panic!("expected latest debug prompt");
        };
        assert!(args.session_id.is_none());
        assert!(args.turn.is_none());
        assert!(!args.show_sensitive);
    }

    #[test]
    fn session_prune_rejects_conflicting_scopes_and_actions() {
        assert!(
            Cli::try_parse_from([
                "zuno",
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
                "zuno",
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
            Cli::try_parse_from(["zuno", "session", "prune", "--older-than", "90", "--yes",])
                .is_err()
        );
    }
}
