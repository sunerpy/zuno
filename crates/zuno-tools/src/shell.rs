use crate::output_policy::OutputPolicy;
use crate::risk::{GateOutcome, RiskContext, assess_and_gate};
use crate::timeout::{
    background_started_output, normalize_foreground_timeout, timeout_promoted_output,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tree_sitter::{Node, Parser};
use zuno_error::ToolError;
use zuno_pty::{
    BackgroundExecutionInfo, BackgroundExecutionInput, BackgroundExecutionRetention,
    BackgroundExecutionService, BackgroundExecutionStatus, CommandShell, CommandShellKind,
};
use zuno_sandbox::{NetworkAccess, PrepareRequest, SandboxBackend, SandboxMode, SandboxPolicy};
use zuno_tool::{OutputLimits, PermissionAsk, Tool, ToolContext, ToolOutput, ToolOutputStore};

const TOOL_ID: &str = "shell";
const BACKGROUND_DIRECTORY: &str = "background";
/// The description the model reads.
pub const DESCRIPTION: &str = include_str!("description/shell.txt");

const CWD_COMMANDS: &[&str] = &[
    "cd",
    "chdir",
    "popd",
    "pop-location",
    "pushd",
    "push-location",
    "set-location",
];
const FILE_COMMANDS: &[&str] = &[
    "cd",
    "chdir",
    "popd",
    "pushd",
    "push-location",
    "set-location",
    "rm",
    "cp",
    "mv",
    "mkdir",
    "touch",
    "chmod",
    "chown",
    "cat",
    "get-content",
    "set-content",
    "add-content",
    "copy-item",
    "move-item",
    "remove-item",
    "new-item",
    "rename-item",
];

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShellParams {
    /// The command to execute.
    pub command: String,
    /// The caller's foreground deadline in milliseconds; todo 72 owns promotion at this deadline.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// The command's working directory, relative to the workspace when not absolute.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Start the command and return immediately while its lifecycle continues asynchronously.
    #[serde(default)]
    pub background: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSyntax {
    Bash,
    PowerShell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResource {
    pub source: String,
    pub tokens: Vec<String>,
    pub always: String,
    pub changes_directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellAnalysis {
    pub commands: Vec<CommandResource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellAuthorization {
    writable_roots: Vec<PathBuf>,
    git_metadata_writable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellEnvInput {
    pub cwd: PathBuf,
    pub session_id: String,
    pub call_id: String,
}

#[async_trait]
pub trait ShellEnvHook: Send + Sync {
    async fn env(&self, input: ShellEnvInput) -> Result<BTreeMap<String, String>, ToolError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopShellEnv;

#[async_trait]
impl ShellEnvHook for NoopShellEnv {
    async fn env(&self, _input: ShellEnvInput) -> Result<BTreeMap<String, String>, ToolError> {
        Ok(BTreeMap::new())
    }
}

pub struct ShellTool {
    workspace: PathBuf,
    shell: CommandShell,
    env_hook: Arc<dyn ShellEnvHook>,
    output_store: ToolOutputStore,
    output_limits: OutputLimits,
    hard_ceiling: Duration,
    background_executions: Arc<BackgroundExecutionService>,
    sandbox: Arc<dyn SandboxBackend>,
    sandbox_policy: SandboxPolicy,
}

impl ShellTool {
    pub fn new(workspace: &Path) -> io::Result<Self> {
        Self::with_configured_shell(workspace, None)
    }

    pub fn with_configured_shell(workspace: &Path, configured: Option<&str>) -> io::Result<Self> {
        let sandbox: Arc<dyn SandboxBackend> = Arc::from(
            zuno_sandbox::system_backend(workspace, SandboxMode::WorkspaceWrite)
                .map_err(io::Error::other)?,
        );
        let policy = SandboxPolicy::new(
            workspace,
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
        )
        .map_err(io::Error::other)?;
        Self::with_sandbox_backend(workspace, configured, sandbox, policy)
    }

    pub fn with_sandbox_backend(
        workspace: &Path,
        configured: Option<&str>,
        sandbox: Arc<dyn SandboxBackend>,
        sandbox_policy: SandboxPolicy,
    ) -> io::Result<Self> {
        let workspace = workspace.canonicalize()?;
        let shell = zuno_pty::shells::command(configured)?;
        let output_store = ToolOutputStore::new(
            workspace
                .join(zuno_paths::PROJECT_DIRECTORY)
                .join(zuno_paths::TOOL_OUTPUT_DIRECTORY),
        );
        let background_executions = Arc::new(
            BackgroundExecutionService::open(
                workspace
                    .join(zuno_paths::PROJECT_DIRECTORY)
                    .join(BACKGROUND_DIRECTORY),
            )
            .map_err(io::Error::other)?,
        );
        Ok(Self {
            workspace,
            shell,
            env_hook: Arc::new(NoopShellEnv),
            output_store,
            output_limits: OutputLimits::default(),
            hard_ceiling: crate::timeout::DEFAULT_HARD_CEILING,
            background_executions,
            sandbox,
            sandbox_policy,
        })
    }

    #[must_use]
    pub fn with_env_hook(mut self, hook: Arc<dyn ShellEnvHook>) -> Self {
        self.env_hook = hook;
        self
    }

    #[must_use]
    pub fn with_output_store(mut self, store: ToolOutputStore) -> Self {
        self.output_store = store;
        self
    }

    #[must_use]
    pub fn with_output_limits(mut self, limits: OutputLimits) -> Self {
        self.output_limits = limits;
        self
    }

    #[must_use]
    pub fn with_hard_ceiling(mut self, ceiling: Duration) -> Self {
        self.hard_ceiling = ceiling;
        self
    }

    #[must_use]
    pub fn with_background_executions(mut self, service: Arc<BackgroundExecutionService>) -> Self {
        self.background_executions = service;
        self
    }

    pub async fn run(
        &self,
        params: ShellParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        self.run_with_output_acceptance(params, ctx, false).await
    }

    async fn run_with_output_acceptance(
        &self,
        params: ShellParams,
        ctx: ToolContext,
        accept_large_output: bool,
    ) -> Result<ToolOutput, ToolError> {
        if params.command.trim().is_empty() {
            return Err(invalid("command must not be empty"));
        }
        if params.timeout == Some(0) {
            return Err(invalid("timeout must be a positive number"));
        }
        if ctx.is_interrupted() {
            return Err(interrupted());
        }

        let cwd = self.resolve_workdir(params.workdir.as_deref())?;
        let analysis = analyze_command(&params.command, self.syntax())?;
        let risk_context = RiskContext::from_env(Some(cwd.clone()));
        let risk_confirmation =
            match assess_and_gate(&params.command, self.syntax(), &risk_context)? {
                GateOutcome::Allow => None,
                GateOutcome::Confirm { reason, target } => Some((reason, target)),
                GateOutcome::Deny { reason } => {
                    return Err(ToolError::Failed {
                        tool: TOOL_ID.to_owned(),
                        source: Box::new(io::Error::new(io::ErrorKind::PermissionDenied, reason)),
                    });
                }
            };
        let authorization = self
            .authorize(
                &params.command,
                &cwd,
                &analysis,
                risk_confirmation.as_ref(),
                &ctx,
            )
            .await?;
        if ctx.is_interrupted() {
            return Err(interrupted());
        }
        let env = self.environment(&cwd, &ctx).await?;
        let command = params.command.clone();
        let foreground_timeout_ms = normalize_foreground_timeout(params.timeout);
        let retention = if params.background {
            BackgroundExecutionRetention::Durable
        } else {
            BackgroundExecutionRetention::Ephemeral
        };
        let input = self.execution_input(&command, &cwd, env, &ctx, retention, &authorization)?;
        let (execution, mut lease) = self
            .background_executions
            .start_leased(input)
            .map_err(failed)?;

        if params.background {
            lease.disarm();
            return Ok(
                background_started_output(self.display_command(&command), &execution)
                    .with_metadata("sandboxBackend", execution.authority.backend.clone())
                    .with_metadata("sandboxMode", json!(execution.authority.mode))
                    .with_metadata("sandboxNetwork", json!(execution.authority.network)),
            );
        }

        let foreground_timeout = Duration::from_millis(foreground_timeout_ms);
        let wait_timeout = (foreground_timeout < self.hard_ceiling).then_some(foreground_timeout);
        let waited = tokio::select! {
            result = self.background_executions.wait(&execution.id, wait_timeout) => {
                result.map_err(failed)?
            }
            () = ctx.interrupt.notified() => {
                let _cancelled = self.background_executions.cancel(&execution.id);
                let _settled = self.background_executions.wait(&execution.id, None).await;
                if let Err(error) = self.background_executions.finish_foreground(&execution.id) {
                    tracing::warn!(
                        execution_id = %execution.id,
                        error = %error,
                        "could not remove interrupted foreground execution"
                    );
                }
                lease.disarm();
                return Err(interrupted());
            }
        };
        if waited.timed_out {
            let promoted = self
                .background_executions
                .promote(&execution.id)
                .map_err(failed)?;
            lease.disarm();
            return Ok(timeout_promoted_output(
                self.display_command(&command),
                foreground_timeout_ms,
                &promoted,
            )
            .with_metadata("sandboxBackend", promoted.authority.backend.clone())
            .with_metadata("sandboxMode", json!(promoted.authority.mode))
            .with_metadata("sandboxNetwork", json!(promoted.authority.network)));
        }
        let full = self
            .background_executions
            .finish_foreground(&execution.id)
            .map_err(failed)?;
        lease.disarm();
        self.completed_output(
            &command,
            foreground_timeout_ms,
            waited.info,
            full,
            &ctx.session_id,
            accept_large_output,
        )
    }

    fn syntax(&self) -> ShellSyntax {
        match self.shell.kind() {
            CommandShellKind::PowerShell => ShellSyntax::PowerShell,
            CommandShellKind::Posix => ShellSyntax::Bash,
        }
    }

    fn display_command(&self, command: &str) -> String {
        format!("{} {command}", self.shell.name())
    }

    fn resolve_workdir(&self, requested: Option<&str>) -> Result<PathBuf, ToolError> {
        let path = requested.map_or_else(
            || self.workspace.clone(),
            |value| {
                let path = Path::new(value);
                if path.is_absolute() {
                    path.to_owned()
                } else {
                    self.workspace.join(path)
                }
            },
        );
        path.canonicalize().map_err(failed)
    }

    async fn authorize(
        &self,
        command: &str,
        cwd: &Path,
        analysis: &ShellAnalysis,
        risk_confirmation: Option<&(String, Option<String>)>,
        ctx: &ToolContext,
    ) -> Result<ShellAuthorization, ToolError> {
        let mut directories = external_directories(analysis, cwd, &self.workspace);
        if !cwd.starts_with(&self.workspace) {
            directories.insert(cwd.to_owned());
        }
        if !directories.is_empty() {
            let patterns: Vec<String> = directories
                .iter()
                .map(|directory| format!("{}{}*", directory.display(), std::path::MAIN_SEPARATOR))
                .collect();
            let mut metadata = Map::new();
            metadata.insert("command".to_owned(), Value::String(command.to_owned()));
            metadata.insert(
                "directories".to_owned(),
                Value::Array(
                    directories
                        .iter()
                        .map(|path| Value::String(path.to_string_lossy().into_owned()))
                        .collect(),
                ),
            );
            ctx.ask(
                TOOL_ID,
                PermissionAsk {
                    permission: "external_directory".to_owned(),
                    patterns: patterns.clone(),
                    metadata,
                    always: patterns,
                    ..PermissionAsk::default()
                },
            )
            .await?;
        }

        let resources: Vec<&CommandResource> = analysis
            .commands
            .iter()
            .filter(|resource| !resource.changes_directory)
            .collect();
        if resources.is_empty() && risk_confirmation.is_none() {
            return Ok(ShellAuthorization {
                writable_roots: directories.into_iter().collect(),
                git_metadata_writable: false,
            });
        }
        let mut metadata = Map::new();
        metadata.insert("command".to_owned(), Value::String(command.to_owned()));
        if let Some((reason, target)) = risk_confirmation {
            metadata.insert("reason".to_owned(), Value::String(reason.clone()));
            if let Some(target) = target {
                metadata.insert("target".to_owned(), Value::String(target.clone()));
            }
        }
        let ask = PermissionAsk {
            permission: TOOL_ID.to_owned(),
            patterns: if resources.is_empty() {
                vec![command.to_owned()]
            } else {
                resources
                    .iter()
                    .map(|resource| resource.source.clone())
                    .collect()
            },
            metadata,
            always: resources
                .iter()
                .map(|resource| resource.always.clone())
                .collect(),
            ..PermissionAsk::default()
        };
        let ask = if risk_confirmation.is_some() {
            ask.require_manual()
        } else {
            ask
        };
        ctx.ask(TOOL_ID, ask).await?;

        let git_metadata_writable = mutates_git_metadata(analysis);
        if git_metadata_writable {
            if self.sandbox_policy.mode() == SandboxMode::ReadOnly {
                return Err(failed(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "this Agent's Shell policy is read-only and cannot modify Git metadata",
                )));
            }
            let git_pattern = format!(
                "{}{}*",
                self.workspace.join(".git").display(),
                std::path::MAIN_SEPARATOR
            );
            let mut metadata = Map::new();
            metadata.insert("command".to_owned(), Value::String(command.to_owned()));
            metadata.insert(
                "workspace".to_owned(),
                Value::String(self.workspace.to_string_lossy().into_owned()),
            );
            ctx.ask(
                TOOL_ID,
                PermissionAsk {
                    permission: "git_metadata".to_owned(),
                    patterns: vec![git_pattern.clone()],
                    metadata,
                    always: vec![git_pattern],
                    ..PermissionAsk::default()
                },
            )
            .await?;
        }
        Ok(ShellAuthorization {
            writable_roots: directories.into_iter().collect(),
            git_metadata_writable,
        })
    }

    async fn environment(
        &self,
        cwd: &Path,
        ctx: &ToolContext,
    ) -> Result<BTreeMap<String, String>, ToolError> {
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        let extra = self
            .env_hook
            .env(ShellEnvInput {
                cwd: cwd.to_owned(),
                session_id: ctx.session_id.clone(),
                call_id: ctx.call_id.clone(),
            })
            .await?;
        env.extend(extra);
        Ok(env)
    }

    fn execution_input(
        &self,
        command: &str,
        cwd: &Path,
        env: BTreeMap<String, String>,
        ctx: &ToolContext,
        retention: BackgroundExecutionRetention,
        authorization: &ShellAuthorization,
    ) -> Result<BackgroundExecutionInput, ToolError> {
        let arguments = match self.shell.kind() {
            CommandShellKind::PowerShell => vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-Command"),
                OsString::from(command),
            ],
            CommandShellKind::Posix => vec![OsString::from("-lc"), OsString::from(command)],
        };
        let environment = env
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        let mut policy = self.sandbox_policy.clone();
        if policy.mode() == SandboxMode::WorkspaceWrite {
            policy = policy
                .with_writable_roots(authorization.writable_roots.clone())
                .map_err(failed)?;
        }
        policy = policy.with_git_metadata_writable(authorization.git_metadata_writable);
        let prepared = self
            .sandbox
            .prepare(PrepareRequest {
                program: self.shell.path().as_os_str().to_owned(),
                arguments,
                cwd: cwd.to_owned(),
                environment,
                policy,
            })
            .map_err(failed)?;
        Ok(BackgroundExecutionInput {
            prepared,
            session_id: ctx.session_id.clone(),
            title: self.display_command(command),
            command: command.to_owned(),
            hard_ceiling: self.hard_ceiling,
            retention,
        })
    }

    fn completed_output(
        &self,
        command: &str,
        foreground_timeout_ms: u64,
        execution: BackgroundExecutionInfo,
        bytes: Vec<u8>,
        session_id: &str,
        accept_large_output: bool,
    ) -> Result<ToolOutput, ToolError> {
        match execution.status {
            BackgroundExecutionStatus::Completed => {}
            BackgroundExecutionStatus::Cancelled => return Err(interrupted()),
            BackgroundExecutionStatus::Failed if execution.timed_out => {
                return Err(ToolError::Timeout {
                    tool: TOOL_ID.to_owned(),
                    elapsed: self.hard_ceiling,
                });
            }
            BackgroundExecutionStatus::Failed | BackgroundExecutionStatus::Uncertain => {
                return Err(failed(io::Error::other(execution.error.unwrap_or_else(
                    || {
                        format!(
                            "background execution {} ended as {}",
                            execution.id,
                            execution.status.as_str()
                        )
                    },
                ))));
            }
            BackgroundExecutionStatus::Running => {
                return Err(failed(io::Error::other(format!(
                    "background execution {} returned from a terminal wait while still running",
                    execution.id
                ))));
            }
        }

        let mut full = String::from_utf8_lossy(&bytes).into_owned();
        if full.is_empty() {
            full = "(no output)".to_owned();
        }
        let output = ToolOutput::text(self.display_command(command), full)
            .with_metadata("exit", json!(execution.exit_code))
            .with_metadata("truncated", false)
            .with_metadata("background", false)
            .with_metadata("task_id", execution.id.as_str())
            .with_metadata("shell", self.shell.name())
            .with_metadata("sandboxBackend", execution.authority.backend)
            .with_metadata("sandboxMode", json!(execution.authority.mode))
            .with_metadata("sandboxNetwork", json!(execution.authority.network))
            .with_metadata("timeout", json!(foreground_timeout_ms));
        OutputPolicy::new(self.output_store.clone(), self.output_limits)
            .apply(TOOL_ID, session_id, output, accept_large_output)
            .map_err(|error| ToolError::Failed {
                tool: TOOL_ID.to_owned(),
                source: Box::new(error),
            })
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn id(&self) -> &str {
        TOOL_ID
    }

    fn display_name(&self) -> &str {
        self.shell.name()
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn raw_parameters_schema(&self) -> Value {
        zuno_tool::schema::derive_params_schema::<ShellParams>()
    }

    /// Claimed because this tool decides for itself whether an oversized result is
    /// returned; the invocation boundary would otherwise remove the opt-in first.
    fn consumed_injected_keys(&self) -> &'static [&'static str] {
        &[zuno_tool::ACCEPT_LARGE_OUTPUT_KEY]
    }

    async fn execute(&self, mut args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let accept_large_output = zuno_tool::guard::accepts_large_output(&args);
        zuno_tool::guard::strip_cross_cutting(&mut args);
        let params = serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
            tool: TOOL_ID.to_owned(),
            source: Box::new(error),
        })?;
        self.run_with_output_acceptance(params, ctx, accept_large_output)
            .await
    }
}

pub fn analyze_command(command: &str, syntax: ShellSyntax) -> Result<ShellAnalysis, ToolError> {
    let mut parser = Parser::new();
    let language = match syntax {
        ShellSyntax::Bash => tree_sitter_bash::LANGUAGE.into(),
        ShellSyntax::PowerShell => tree_sitter_powershell::LANGUAGE.into(),
    };
    parser.set_language(&language).map_err(failed)?;
    let tree = parser
        .parse(command, None)
        .ok_or_else(|| failed(io::Error::other("tree-sitter returned no syntax tree")))?;
    let mut nodes = Vec::new();
    collect_commands(tree.root_node(), &mut nodes);
    let commands = nodes
        .into_iter()
        .filter_map(|node| command_resource(node, command.as_bytes()))
        .collect();
    Ok(ShellAnalysis { commands })
}

fn collect_commands<'tree>(node: Node<'tree>, commands: &mut Vec<Node<'tree>>) {
    if node.kind() == "command" {
        commands.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_commands(child, commands);
    }
}

fn command_resource(node: Node<'_>, source_bytes: &[u8]) -> Option<CommandResource> {
    let source_node = node
        .parent()
        .filter(|parent| parent.kind() == "redirected_statement")
        .unwrap_or(node);
    let source = source_node.utf8_text(source_bytes).ok()?.trim().to_owned();
    if source.is_empty() {
        return None;
    }
    let mut tokens = command_parts(node, source_bytes);
    if tokens.is_empty() {
        tokens = lexical_tokens(&source);
    }
    let command = tokens
        .first()
        .map(|token| unquote(token).to_ascii_lowercase());
    let changes_directory = command
        .as_deref()
        .is_some_and(|command| CWD_COMMANDS.contains(&command));
    let prefix = arity_prefix(&tokens);
    let always = if prefix.is_empty() {
        "*".to_owned()
    } else {
        format!("{} *", prefix.join(" "))
    };
    Some(CommandResource {
        source,
        tokens,
        always,
        changes_directory,
    })
}

fn command_parts(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "command_elements" {
            let mut elements_cursor = child.walk();
            for item in child.children(&mut elements_cursor) {
                if matches!(item.kind(), "command_argument_sep" | "redirection") {
                    continue;
                }
                if let Ok(text) = item.utf8_text(source) {
                    let text = text.trim();
                    if !text.is_empty() {
                        parts.push(text.to_owned());
                    }
                }
            }
            continue;
        }
        if matches!(
            child.kind(),
            "command_name"
                | "command_name_expr"
                | "word"
                | "string"
                | "raw_string"
                | "concatenation"
        ) && let Ok(text) = child.utf8_text(source)
        {
            let text = text.trim();
            if !text.is_empty() {
                parts.push(text.to_owned());
            }
        }
    }
    parts
}

fn lexical_tokens(source: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in source.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            current.push(character);
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            current.push(character);
            continue;
        }
        if character.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(character);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn arity_prefix(tokens: &[String]) -> Vec<String> {
    let lowered: Vec<String> = tokens
        .iter()
        .map(|token| unquote(token).to_ascii_lowercase())
        .collect();
    let arity = match lowered.as_slice() {
        [first, ..] if ONE_TOKEN_COMMANDS.contains(&first.as_str()) => 1,
        [first, second, ..]
            if matches!(first.as_str(), "git")
                && matches!(second.as_str(), "config" | "remote" | "stash") =>
        {
            3
        }
        [first, second, ..]
            if matches!(
                (first.as_str(), second.as_str()),
                (
                    "npm" | "pnpm" | "yarn" | "bun",
                    "run" | "exec" | "dlx" | "x"
                ) | (
                    "docker" | "podman",
                    "builder" | "compose" | "container" | "image" | "network" | "volume"
                ) | ("terraform", "workspace")
            ) =>
        {
            3
        }
        [first, ..] if THREE_TOKEN_COMMANDS.contains(&first.as_str()) => 3,
        [first, ..] if TWO_TOKEN_COMMANDS.contains(&first.as_str()) => 2,
        [] => 0,
        _ => 1,
    };
    tokens.iter().take(arity).cloned().collect()
}

const ONE_TOKEN_COMMANDS: &[&str] = &[
    "cat", "cd", "chmod", "chown", "cp", "echo", "env", "export", "grep", "kill", "killall", "ln",
    "ls", "mkdir", "mv", "ps", "pwd", "rm", "rmdir", "sleep", "source", "tail", "touch", "unset",
    "which",
];
const TWO_TOKEN_COMMANDS: &[&str] = &[
    "bazel",
    "brew",
    "bun",
    "cargo",
    "cdk",
    "cf",
    "cmake",
    "composer",
    "consul",
    "crictl",
    "deno",
    "docker",
    "eksctl",
    "firebase",
    "flyctl",
    "git",
    "go",
    "gradle",
    "helm",
    "heroku",
    "hugo",
    "ip",
    "kind",
    "kubectl",
    "kustomize",
    "make",
    "mc",
    "minikube",
    "mongosh",
    "mysql",
    "mvn",
    "ng",
    "npm",
    "nvm",
    "nx",
    "openssl",
    "pip",
    "pipenv",
    "pnpm",
    "poetry",
    "podman",
    "psql",
    "pulumi",
    "pyenv",
    "python",
    "rake",
    "rbenv",
    "redis-cli",
    "rustup",
    "serverless",
    "skaffold",
    "sls",
    "sst",
    "swift",
    "systemctl",
    "terraform",
    "tmux",
    "turbo",
    "ufw",
    "vault",
    "vercel",
    "volta",
    "wp",
    "yarn",
];
const THREE_TOKEN_COMMANDS: &[&str] = &["aws", "az", "doctl", "gcloud", "gh", "sfdx"];

fn external_directories(
    analysis: &ShellAnalysis,
    cwd: &Path,
    workspace: &Path,
) -> BTreeSet<PathBuf> {
    let mut directories = BTreeSet::new();
    for resource in &analysis.commands {
        let Some(command) = resource.tokens.first() else {
            continue;
        };
        let command = unquote(command).to_ascii_lowercase();
        if !FILE_COMMANDS.contains(&command.as_str()) {
            continue;
        }
        for argument in path_arguments(&resource.tokens, &command) {
            let argument = unquote(argument);
            if argument.is_empty() || is_dynamic_path(&argument) {
                continue;
            }
            let candidate = if Path::new(&argument).is_absolute() {
                PathBuf::from(&argument)
            } else {
                cwd.join(&argument)
            };
            let resolved = candidate.canonicalize().unwrap_or(candidate);
            if resolved.starts_with(workspace) {
                continue;
            }
            let directory = if resolved.is_dir() {
                resolved
            } else {
                resolved
                    .parent()
                    .map_or_else(|| cwd.to_owned(), Path::to_owned)
            };
            directories.insert(directory);
        }
    }
    directories
}

fn mutates_git_metadata(analysis: &ShellAnalysis) -> bool {
    analysis.commands.iter().any(|resource| {
        let Some(first) = resource.tokens.first() else {
            return false;
        };
        if !unquote(first).eq_ignore_ascii_case("git") {
            return false;
        }
        let mut index = 1;
        while let Some(argument) = resource.tokens.get(index) {
            let argument = unquote(argument).to_ascii_lowercase();
            if matches!(
                argument.as_str(),
                "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace"
            ) {
                index = index.saturating_add(2);
                continue;
            }
            if argument.starts_with('-') {
                index = index.saturating_add(1);
                continue;
            }
            let remaining = &resource.tokens[index + 1..];
            return !git_subcommand_is_read_only(&argument, remaining);
        }
        false
    })
}

fn git_subcommand_is_read_only(subcommand: &str, arguments: &[String]) -> bool {
    if matches!(
        subcommand,
        "annotate"
            | "blame"
            | "cat-file"
            | "diff"
            | "diff-files"
            | "diff-index"
            | "diff-tree"
            | "for-each-ref"
            | "grep"
            | "log"
            | "ls-files"
            | "ls-remote"
            | "ls-tree"
            | "merge-base"
            | "name-rev"
            | "rev-list"
            | "rev-parse"
            | "shortlog"
            | "show"
            | "show-ref"
            | "status"
            | "verify-commit"
            | "verify-tag"
            | "version"
            | "whatchanged"
    ) {
        return true;
    }
    if subcommand == "branch" {
        return arguments.is_empty()
            || arguments.iter().all(|argument| {
                matches!(
                    unquote(argument).as_str(),
                    "--all"
                        | "-a"
                        | "--list"
                        | "-l"
                        | "--show-current"
                        | "--verbose"
                        | "-v"
                        | "-vv"
                        | "--no-color"
                )
            });
    }
    if subcommand == "config" {
        if arguments.iter().any(|argument| {
            matches!(
                unquote(argument).as_str(),
                "--add"
                    | "--edit"
                    | "-e"
                    | "--rename-section"
                    | "--remove-section"
                    | "--replace-all"
                    | "--unset"
                    | "--unset-all"
            )
        }) {
            return false;
        }
        let positionals = arguments
            .iter()
            .filter(|argument| !unquote(argument).starts_with('-'))
            .count();
        return positionals <= 1;
    }
    false
}

fn path_arguments<'a>(tokens: &'a [String], command: &str) -> Vec<&'a str> {
    tokens
        .iter()
        .skip(1)
        .map(String::as_str)
        .filter(|argument| {
            !(argument.starts_with('-') || command == "chmod" && argument.starts_with('+'))
        })
        .collect()
}

fn unquote(text: &str) -> String {
    if text.len() >= 2 {
        let first = text.as_bytes()[0];
        let last = text.as_bytes()[text.len() - 1];
        if matches!(first, b'\'' | b'"') && first == last {
            return text[1..text.len() - 1].to_owned();
        }
    }
    text.to_owned()
}

fn is_dynamic_path(path: &str) -> bool {
    path.starts_with('(')
        || path.starts_with("@(")
        || path.contains("$(")
        || path.contains("${")
        || path.contains('`')
        || path.contains('$')
        || path.contains('*')
        || path.contains('?')
        || path.contains('[')
}

fn invalid(message: &'static str) -> ToolError {
    ToolError::InvalidArgs {
        tool: TOOL_ID.to_owned(),
        source: Box::new(io::Error::new(io::ErrorKind::InvalidInput, message)),
    }
}

fn failed(error: impl std::error::Error + Send + Sync + 'static) -> ToolError {
    ToolError::Failed {
        tool: TOOL_ID.to_owned(),
        source: Box::new(error),
    }
}

fn interrupted() -> ToolError {
    failed(io::Error::new(
        io::ErrorKind::Interrupted,
        "shell command was interrupted",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutates(command: &str) -> bool {
        mutates_git_metadata(&analyze_command(command, ShellSyntax::Bash).expect("analysis"))
    }

    #[test]
    fn git_read_commands_keep_metadata_read_only() {
        for command in [
            "git status --short",
            "git diff --stat",
            "git -C repo log -1",
            "git branch --show-current",
            "git config --get user.name",
        ] {
            assert!(!mutates(command), "{command}");
        }
    }

    #[test]
    fn git_mutations_require_a_per_call_metadata_grant() {
        for command in [
            "git add src/lib.rs",
            "git commit -m test",
            "git checkout main",
            "git config user.name zuno",
            "git branch feature",
        ] {
            assert!(mutates(command), "{command}");
        }
    }
}
