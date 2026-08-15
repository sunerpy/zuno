use crate::output_policy::OutputPolicy;
use crate::risk::{GateOutcome, Justification, RiskContext, assess_and_gate};
use crate::timeout::{
    BackgroundAdoption, BackgroundManager, ForegroundTask, LocalBackgroundManager,
    background_started_output, normalize_foreground_timeout, wait_or_promote,
};
use async_trait::async_trait;
use oc_error::ToolError;
use oc_tool::{OutputLimits, PermissionAsk, Tool, ToolContext, ToolOutput, ToolOutputStore};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, Command};
use tree_sitter::{Node, Parser};

const TOOL_ID: &str = "bash";
const BACKGROUND_DIRECTORY: &str = "background";
const TERMINATE_GRACE: Duration = Duration::from_millis(200);
const DESCRIPTION: &str = "Executes a command with the configured shell. Commands are parsed with tree-sitter before execution so each constituent command is permission-checked independently. A deterministic destructive-command gate runs before every foreground or background spawn. This is not a sandbox: commands retain the user's full filesystem, network, and credentials; confinement is a future decision, not an implied guarantee. Use workdir instead of changing directories inside the command.";

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
    /// Only for resubmitting a reflected command; identify the actual user request it serves.
    #[serde(default)]
    pub justification: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Posix,
    PowerShell,
    Cmd,
}

#[derive(Debug, Clone)]
struct SelectedShell {
    path: PathBuf,
    kind: ShellKind,
}

pub struct ShellTool {
    workspace: PathBuf,
    shell: SelectedShell,
    env_hook: Arc<dyn ShellEnvHook>,
    output_store: ToolOutputStore,
    output_limits: OutputLimits,
    hard_ceiling: Duration,
    background_manager: Arc<dyn BackgroundManager>,
}

impl ShellTool {
    pub fn new(workspace: &Path) -> io::Result<Self> {
        Self::with_configured_shell(workspace, None)
    }

    pub fn with_configured_shell(workspace: &Path, configured: Option<&Path>) -> io::Result<Self> {
        let workspace = workspace.canonicalize()?;
        let shell = discover_shell(configured)?;
        let output_store = ToolOutputStore::new(
            workspace
                .join(oc_paths::PROJECT_DIRECTORY)
                .join(oc_paths::TOOL_OUTPUT_DIRECTORY),
        );
        let background_manager = Arc::new(LocalBackgroundManager::new(
            workspace
                .join(oc_paths::PROJECT_DIRECTORY)
                .join(BACKGROUND_DIRECTORY),
        ));
        Ok(Self {
            workspace,
            shell,
            env_hook: Arc::new(NoopShellEnv),
            output_store,
            output_limits: OutputLimits::default(),
            hard_ceiling: crate::timeout::DEFAULT_HARD_CEILING,
            background_manager,
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
    pub fn with_background_manager(mut self, manager: Arc<dyn BackgroundManager>) -> Self {
        self.background_manager = manager;
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
        let justification = Justification {
            text: params.justification.clone(),
        };
        match assess_and_gate(
            &params.command,
            self.syntax(),
            &risk_context,
            &justification,
        )? {
            GateOutcome::Allow => {}
            GateOutcome::Reflect { prompt } => {
                return Err(ToolError::InvalidArgs {
                    tool: TOOL_ID.to_owned(),
                    source: Box::new(io::Error::new(io::ErrorKind::InvalidInput, prompt)),
                });
            }
            GateOutcome::Deny { reason } => {
                return Err(ToolError::Failed {
                    tool: TOOL_ID.to_owned(),
                    source: Box::new(io::Error::new(io::ErrorKind::PermissionDenied, reason)),
                });
            }
        }
        self.authorize(&params.command, &cwd, &analysis, &ctx)
            .await?;
        if ctx.is_interrupted() {
            return Err(interrupted());
        }
        let env = self.environment(&cwd, &ctx).await?;
        let child = self.spawn(&params.command, &cwd, &env)?;
        let pid = child.id();
        let command = params.command.clone();
        let session_id = ctx.session_id.clone();
        let foreground_timeout_ms = normalize_foreground_timeout(params.timeout);
        let execution = ChildExecution {
            command: command.clone(),
            session_id: session_id.clone(),
            ctx,
            output_policy: OutputPolicy::new(self.output_store.clone(), self.output_limits),
            hard_ceiling: self.hard_ceiling,
            foreground_timeout_ms,
            accept_large_output,
        };
        let work = tokio::spawn(async move { complete_child(child, execution).await });

        if params.background {
            let handle = self.background_manager.adopt(BackgroundAdoption {
                tool_name: TOOL_ID.to_owned(),
                display_name: command.clone(),
                session_id,
                work,
            })?;
            return Ok(background_started_output(command, pid, handle));
        }

        wait_or_promote(
            self.background_manager.as_ref(),
            ForegroundTask {
                tool_name: TOOL_ID.to_owned(),
                display_name: command,
                session_id,
                foreground_timeout_ms,
                hard_ceiling: self.hard_ceiling,
                work,
            },
        )
        .await
    }

    fn syntax(&self) -> ShellSyntax {
        match self.shell.kind {
            ShellKind::PowerShell => ShellSyntax::PowerShell,
            ShellKind::Posix | ShellKind::Cmd => ShellSyntax::Bash,
        }
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
        ctx: &ToolContext,
    ) -> Result<(), ToolError> {
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
                },
            )
            .await?;
        }

        let resources: Vec<&CommandResource> = analysis
            .commands
            .iter()
            .filter(|resource| !resource.changes_directory)
            .collect();
        if resources.is_empty() {
            return Ok(());
        }
        let mut metadata = Map::new();
        metadata.insert("command".to_owned(), Value::String(command.to_owned()));
        ctx.ask(
            TOOL_ID,
            PermissionAsk {
                permission: TOOL_ID.to_owned(),
                patterns: resources
                    .iter()
                    .map(|resource| resource.source.clone())
                    .collect(),
                metadata,
                always: resources
                    .iter()
                    .map(|resource| resource.always.clone())
                    .collect(),
            },
        )
        .await
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

    fn spawn(
        &self,
        command: &str,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<Child, ToolError> {
        let mut process = Command::new(&self.shell.path);
        match self.shell.kind {
            ShellKind::PowerShell => {
                process.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    command,
                ]);
            }
            ShellKind::Cmd => {
                process.args(["/c", command]);
            }
            ShellKind::Posix => {
                process.args(["-lc", command]);
            }
        }
        process
            .current_dir(cwd)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        process.process_group(0);
        process.spawn().map_err(failed)
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn id(&self) -> &str {
        TOOL_ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn raw_parameters_schema(&self) -> Value {
        oc_tool::schema::derive_params_schema::<ShellParams>()
    }

    async fn execute(&self, mut args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let accept_large_output = oc_tool::guard::accepts_large_output(&args);
        oc_tool::guard::strip_cross_cutting(&mut args);
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

struct ChildExecution {
    command: String,
    session_id: String,
    ctx: ToolContext,
    output_policy: OutputPolicy,
    hard_ceiling: Duration,
    foreground_timeout_ms: u64,
    accept_large_output: bool,
}

async fn complete_child(
    mut child: Child,
    execution: ChildExecution,
) -> Result<ToolOutput, ToolError> {
    let mut process_tree = ProcessTreeGuard::new(child.id());
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(read_pipe(stdout));
    let stderr_task = tokio::spawn(read_pipe(stderr));
    let started = Instant::now();

    let status = tokio::select! {
        result = child.wait() => Some(result.map_err(failed)?),
        () = execution.ctx.interrupt.notified() => {
            terminate_process_tree(&mut child).await;
            let _status = child.wait().await;
            None
        }
        () = tokio::time::sleep(execution.hard_ceiling) => {
            terminate_process_tree(&mut child).await;
            let _status = child.wait().await;
            process_tree.disarm();
            let _stdout = join_pipe(stdout_task).await;
            let _stderr = join_pipe(stderr_task).await;
            return Err(ToolError::Timeout {
                tool: TOOL_ID.to_owned(),
                elapsed: started.elapsed(),
            });
        }
    };

    let stdout = join_pipe(stdout_task).await?;
    let stderr = join_pipe(stderr_task).await?;
    if status.is_none() {
        process_tree.disarm();
        return Err(interrupted());
    }
    process_tree.disarm();
    let status = status.expect("checked above");
    let mut full = String::from_utf8_lossy(&stdout).into_owned();
    full.push_str(&String::from_utf8_lossy(&stderr));
    if full.is_empty() {
        full = "(no output)".to_owned();
    }
    let output = ToolOutput::text(&execution.command, full)
        .with_metadata("exit", json!(status.code()))
        .with_metadata("truncated", false)
        .with_metadata("background", false)
        .with_metadata("timeout", json!(execution.foreground_timeout_ms));
    execution
        .output_policy
        .apply(
            TOOL_ID,
            &execution.session_id,
            output,
            execution.accept_large_output,
        )
        .map_err(|error| ToolError::Failed {
            tool: TOOL_ID.to_owned(),
            source: Box::new(error),
        })
}

struct ProcessTreeGuard {
    pid: Option<u32>,
}

impl ProcessTreeGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        let Some(pid) = self.pid else {
            return;
        };
        #[cfg(unix)]
        {
            let _status = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(windows)]
        {
            let _status = std::process::Command::new("taskkill")
                .args(["/pid", &pid.to_string(), "/f", "/t"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

async fn read_pipe(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> io::Result<Vec<u8>> {
    let Some(mut pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

async fn join_pipe(
    task: tokio::task::JoinHandle<io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, ToolError> {
    task.await
        .map_err(|error| failed(io::Error::other(error)))?
        .map_err(failed)
}

#[cfg(unix)]
async fn terminate_process_tree(child: &mut Child) {
    let Some(pid) = child.id() else {
        return;
    };
    let group = format!("-{pid}");
    let terminated = Command::new("kill")
        .args(["-TERM", "--", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success());
    if !terminated {
        let _result = child.start_kill();
    }
    tokio::time::sleep(TERMINATE_GRACE).await;
    let _status = Command::new("kill")
        .args(["-KILL", "--", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    let _result = child.start_kill();
}

#[cfg(windows)]
async fn terminate_process_tree(child: &mut Child) {
    if let Some(pid) = child.id() {
        let _status = Command::new("taskkill")
            .args(["/pid", &pid.to_string(), "/f", "/t"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _result = child.start_kill();
}

fn discover_shell(configured: Option<&Path>) -> io::Result<SelectedShell> {
    if let Some(configured) = configured {
        return resolve_shell(configured).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("configured shell {} was not found", configured.display()),
            )
        });
    }

    if let Some(shell) = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .as_deref()
        .and_then(resolve_shell)
        .filter(acceptable)
    {
        return Ok(shell);
    }

    #[cfg(windows)]
    let candidates = ["pwsh.exe", "powershell.exe", "bash.exe", "cmd.exe"];
    #[cfg(target_os = "macos")]
    let candidates = ["/bin/zsh", "/bin/bash", "/bin/sh"];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates = ["bash", "/bin/bash", "/bin/sh"];

    candidates
        .iter()
        .find_map(|candidate| resolve_shell(Path::new(candidate)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no acceptable shell was found"))
}

fn resolve_shell(candidate: &Path) -> Option<SelectedShell> {
    let path = if candidate.components().count() > 1 || candidate.is_absolute() {
        candidate.is_file().then(|| candidate.to_owned())?
    } else {
        which::which(candidate).ok()?
    };
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())?
        .to_ascii_lowercase();
    let kind = match name.as_str() {
        "pwsh" | "powershell" => ShellKind::PowerShell,
        "cmd" => ShellKind::Cmd,
        _ => ShellKind::Posix,
    };
    Some(SelectedShell { path, kind })
}

fn acceptable(shell: &SelectedShell) -> bool {
    shell
        .path
        .file_stem()
        .and_then(|name| name.to_str())
        .is_none_or(|name| !matches!(name.to_ascii_lowercase().as_str(), "fish" | "nu"))
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
