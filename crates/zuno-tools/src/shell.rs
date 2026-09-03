mod authority;

use crate::output_policy::OutputPolicy;
use crate::risk::{
    GIT_REPOSITORY_ENVIRONMENT_VARIABLES, GateOutcome, RiskAssessment, RiskContext, assess_and_gate,
};
use crate::search_common::directory_grant_pattern;
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
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tree_sitter::{Node, Parser};
use zuno_error::ToolError;
use zuno_paths::GeneratedDirectory;
use zuno_process::GuardExit;
use zuno_pty::{
    BackgroundExecutionInfo, BackgroundExecutionInput, BackgroundExecutionPurpose,
    BackgroundExecutionRetention, BackgroundExecutionService, BackgroundExecutionStatus,
    CommandShell, CommandShellKind,
};
use zuno_sandbox::{
    ExecutionAuthority, NetworkAccess, PrepareRequest, SandboxBackend, SandboxMode, SandboxPolicy,
    SandboxResolutionKind,
};
use zuno_tool::{
    ExitAuthority, OutputLimits, PermissionAsk, ReceiptOutcome, Tool, ToolContext, ToolOutput,
    ToolOutputStore, VerificationReceipt,
};

const TOOL_ID: &str = "shell";
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

/// The single-line length a receipt summary is cut to.
///
/// A receipt is read as one checklist entry, so a heredoc or a long compound
/// command has to collapse to one line of bounded length. The value is a display
/// budget rather than a protocol limit; it only has to leave the interesting head
/// of a command legible.
const SUMMARY_MAX_BYTES: usize = 160;

/// POSIX interpreters known to implement `set -o pipefail`.
///
/// `pipefail` is a ksh extension, not POSIX. `dash` — Debian's `/bin/sh` —
/// rejects it, and because `set` is a special builtin the rejection is fatal: a
/// non-interactive `dash` exits 2 without running a line of the caller's command.
/// The prologue is therefore emitted only for an interpreter listed here, and the
/// same list decides whether the resulting exit status may be called
/// authoritative.
///
/// `sh` is deliberately absent even though it is `bash` in POSIX mode on macOS.
/// The name does not say which interpreter is behind it, and the two ways of
/// being wrong are not symmetric: guessing that `sh` honours `pipefail` risks a
/// receipt claiming authority the shell never provided, while declining to guess
/// only costs a `Derived` receipt on a shell that could have done better.
const PIPEFAIL_INTERPRETERS: &[&str] = &["bash", "ksh", "zsh"];

/// How much of a command's failure its reported exit status has to reflect.
///
/// Neither a POSIX shell nor PowerShell propagates a mid-pipeline failure on its
/// own: `cargo test | tail -5` exits zero when the tests fail. This selects the
/// shell configuration a command runs under, and with it how much authority the
/// status carries in the verification receipt attached to the result.
//
// These doc comments are the wire schema every request carries, so they stay at
// the length of the guidance a caller needs, and the reasoning a maintainer
// needs sits in plain comments like this one. Two parts of that reasoning matter
// most. First, `Pipefail` is the default even though it changes the observable
// exit code of an existing POSIX pipeline — `false | true` now reports 1 where it
// reported 0 — because the old status was not a weaker signal but a wrong one,
// and turning a silent false pass into a visible failure costs a caller one extra
// look where the reverse costs a session its conclusion. Second, `Last` is what
// an unconfigured shell already does, kept as the explicit opt-out for a command
// that is meant to tolerate a failing stage: a probe whose non-zero status is the
// answer, or a pipeline whose reader closes the stream early.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ExitPolicy {
    /// A failure at any stage of a pipeline is the whole command's failure.
    #[default]
    Pipefail,
    /// Only the last stage of a pipeline decides the status.
    Last,
    /// Also stop at the first failing command in a sequence: POSIX `set -e`.
    All,
}

impl ExitPolicy {
    /// The wire spelling, so a receipt can name the policy it ran under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pipefail => "pipefail",
            Self::Last => "last",
            Self::All => "all",
        }
    }
}

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
    /// Mark a command that only observes remote work so completion triggers an authoritative refresh.
    #[serde(default)]
    pub background_purpose: BackgroundExecutionPurpose,
    /// Exact full object id expected at `HEAD` before rewriting local Git history.
    #[serde(default)]
    pub expected_git_head: Option<String>,
    /// How much of a pipeline's failure the exit status must reflect; unset means
    /// `pipefail`, a status that covers the whole command.
    #[serde(default)]
    pub exit_policy: Option<ExitPolicy>,
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
    /// True when this command is a later stage of a pipeline, so its standard input
    /// is the previous stage's output rather than the filesystem.
    ///
    /// Recorded here because it is a fact about the syntax tree that the flat resource
    /// list otherwise loses, and a consumer that needed it would have to reparse the
    /// command with a second tokenizer. [`crate::navigation`] uses it to tell
    /// `cargo test | grep FAILED`, which filters a result, from `grep -rn FAILED .`,
    /// which searches the tree.
    pub stdin_from_pipeline: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShellExecutionLifecycle {
    purpose: BackgroundExecutionPurpose,
    retention: BackgroundExecutionRetention,
}

/// One command as it will be executed: the caller's text, the resolved directory,
/// and the exit policy that decides how the interpreter is invoked.
#[derive(Debug, Clone, Copy)]
struct ShellRequest<'a> {
    command: &'a str,
    cwd: &'a Path,
    exit_policy: ExitPolicy,
}

/// What a completed command's exit status is worth under one shell configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExitContract {
    /// How much of the command the status covers.
    authority: ExitAuthority,
    /// Why it covers less than the whole command, for the receipt's `detail`.
    limitation: Option<String>,
}

/// What a receipt needs to say about one call, gathered before the command runs.
///
/// Held for the whole call because none of it is recoverable from the result: the
/// caller's own command text, the directory the tool resolved, the `HEAD` this
/// call already read for its history-rewrite guard, and the contract the
/// interpreter agreed to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellVerification {
    /// The caller's command, verbatim: the transcript title and the summary source.
    command: String,
    /// The resolved working directory the command runs in.
    workdir: String,
    /// `HEAD` as already resolved for this call, absent when nothing resolved it.
    git_head: Option<String>,
    /// The exit contract of the configuration this command runs under.
    contract: ExitContract,
}

impl ShellVerification {
    /// A receipt for a run that produced no exit status at all.
    ///
    /// Used for a background launch, a promotion at the foreground deadline, and a
    /// command killed by a signal. Outcome and authority keep their defaults —
    /// `Unknown` and `Absent` — so [`VerificationReceipt::proves_success`] is false
    /// no matter what a reader does with it.
    fn unresolved(&self, detail: impl Into<String>) -> VerificationReceipt {
        VerificationReceipt {
            workdir: Some(self.workdir.clone()),
            git_head: self.git_head.clone(),
            ..VerificationReceipt::unknown(summarize_command(&self.command), detail)
        }
    }

    /// The receipt for a run that finished, from its status and captured output.
    ///
    /// An absent exit code on a completed run means the process was killed by a
    /// signal rather than exiting, which decides nothing about the work, so it
    /// degrades to [`Self::unresolved`] and keeps only the output digest.
    fn settled(&self, exit_code: Option<i32>, output: &[u8]) -> VerificationReceipt {
        let output_digest = Some(crate::read::digest_bytes(output));
        let Some(code) = exit_code else {
            return VerificationReceipt {
                output_digest,
                ..self.unresolved(
                    "the command reported no exit status, which means it was killed by a signal \
                     rather than deciding an outcome of its own",
                )
            };
        };
        VerificationReceipt {
            summary: summarize_command(&self.command),
            workdir: Some(self.workdir.clone()),
            exit_code: Some(i64::from(code)),
            exit_authority: self.contract.authority,
            outcome: if code == 0 {
                ReceiptOutcome::Passed
            } else {
                ReceiptOutcome::Failed
            },
            git_head: self.git_head.clone(),
            output_digest,
            detail: self.contract.limitation.clone(),
        }
    }

    /// The receipt for a run whose program never started.
    ///
    /// The child-process guard reports its own reserved code in place of a payload
    /// code it never got, so the interpreter's exit contract does not apply here:
    /// nothing ran that could have decided anything. The code is still recorded,
    /// because it is what the caller would see on a terminal, but the authority is
    /// [`ExitAuthority::Absent`] so no reader can cite it as the command's verdict.
    fn never_ran(&self, exit_code: i32, output: &[u8], detail: &str) -> VerificationReceipt {
        VerificationReceipt {
            summary: summarize_command(&self.command),
            workdir: Some(self.workdir.clone()),
            exit_code: Some(i64::from(exit_code)),
            exit_authority: ExitAuthority::Absent,
            outcome: ReceiptOutcome::Failed,
            git_head: self.git_head.clone(),
            output_digest: Some(crate::read::digest_bytes(output)),
            detail: Some(detail.to_owned()),
        }
    }
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
    /// The generated directory the background service writes into, when its root is
    /// one. `None` for a service a caller rooted somewhere of its own choosing, which
    /// is not Zuno's generated state and not Zuno's to exclude.
    background_directory: Option<GeneratedDirectory>,
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
        // Generated state is rooted at the worktree, because that is where the exclude
        // patterns are anchored and where `classify` looks; joining the project
        // directory onto a session's own directory put it somewhere nothing covered.
        // Resolved once because it spawns git, and only these two roots move: the
        // workspace itself still decides the sandbox boundary, the default working
        // directory, and what a relative `workdir` resolves against.
        let generated_root = zuno_paths::generated_root(&workspace);
        let output_store = ToolOutputStore::in_worktree(&generated_root);
        let background_directory = GeneratedDirectory::in_worktree(
            &generated_root,
            &zuno_paths::generated::BACKGROUND_EXECUTIONS,
        );
        let background_executions = Arc::new(
            BackgroundExecutionService::open(background_directory.path())
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
            background_directory: Some(background_directory),
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

    /// Uses a background service the process already owns, instead of the one this tool
    /// opened.
    ///
    /// The exclusion follows the service, because the directory that must stay out of a
    /// commit is the one the service actually creates.
    #[must_use]
    pub fn with_background_executions(mut self, service: Arc<BackgroundExecutionService>) -> Self {
        self.background_directory = GeneratedDirectory::claim(
            service.root(),
            &zuno_paths::generated::BACKGROUND_EXECUTIONS,
        );
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
        let (risk_assessment, risk_gate) =
            assess_and_gate(&params.command, self.syntax(), &risk_context)?;
        let risk_confirmation = match risk_gate {
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
        let git_head = validate_expected_git_head(
            &risk_assessment,
            params.expected_git_head.as_deref(),
            &cwd,
            &env,
        )?;
        refuse_generated_delivery(&analysis, &cwd, &env)?;
        if ctx.is_interrupted() {
            return Err(interrupted());
        }
        let exit_policy = params.exit_policy.unwrap_or_default();
        let verification = ShellVerification {
            command: params.command.clone(),
            workdir: cwd.to_string_lossy().into_owned(),
            git_head,
            contract: exit_contract(
                self.shell.kind(),
                self.shell.name(),
                exit_policy,
                &params.command,
            ),
        };
        let foreground_timeout_ms = normalize_foreground_timeout(params.timeout);
        let retention = if params.background {
            BackgroundExecutionRetention::Durable
        } else {
            BackgroundExecutionRetention::Ephemeral
        };
        let lifecycle = ShellExecutionLifecycle {
            purpose: params.background_purpose,
            retention,
        };
        let input = self.execution_input(
            ShellRequest {
                command: &verification.command,
                cwd: &cwd,
                exit_policy,
            },
            env,
            &ctx,
            lifecycle,
            &authorization,
        )?;
        if let Some(directory) = &self.background_directory {
            // Republished on every start, not once when the service was opened: the
            // service creates its root whenever it is missing, so a root deleted
            // mid-session has to come back excluded rather than come back bare.
            directory.ensure().map_err(failed)?;
        }
        let (execution, mut lease) = self
            .background_executions
            .start_leased(input)
            .map_err(failed)?;

        if params.background {
            lease.disarm();
            let receipt = verification.unresolved(
                "the command was launched in the background and has not finished, so no exit \
                 status exists and this result proves nothing about its outcome",
            );
            return Ok(with_sandbox_metadata(
                background_started_output(verification.command.clone(), &execution)
                    .with_metadata("shell", self.shell.name())
                    .with_verification(&receipt),
                &execution.authority,
            ));
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
            let receipt = verification.unresolved(format!(
                "the command was still running at its {foreground_timeout_ms}ms foreground \
                 deadline and continues in the background, so no exit status exists yet"
            ));
            return Ok(with_sandbox_metadata(
                timeout_promoted_output(
                    verification.command.clone(),
                    foreground_timeout_ms,
                    &promoted,
                )
                .with_metadata("shell", self.shell.name())
                .with_verification(&receipt),
                &promoted.authority,
            ));
        }
        let full = self
            .background_executions
            .finish_foreground(&execution.id)
            .map_err(failed)?;
        lease.disarm();
        self.completed_output(
            &verification,
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
                .map(|directory| directory_grant_pattern(directory))
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
            let git_pattern = directory_grant_pattern(&self.workspace.join(".git"));
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
        // Zuno's own secrets are removed before the hook runs, so a host that wants a
        // credential in the tool environment can still put one back deliberately.
        let mut env = withhold_zuno_secrets(std::env::vars());
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
        request: ShellRequest<'_>,
        env: BTreeMap<String, String>,
        ctx: &ToolContext,
        lifecycle: ShellExecutionLifecycle,
        authorization: &ShellAuthorization,
    ) -> Result<BackgroundExecutionInput, ToolError> {
        let arguments = shell_arguments(
            self.shell.kind(),
            self.shell.name(),
            request.exit_policy,
            request.command,
        );
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
                cwd: request.cwd.to_owned(),
                environment,
                policy,
            })
            .map_err(failed)?;
        Ok(BackgroundExecutionInput {
            prepared,
            session_id: ctx.session_id.clone(),
            title: request.command.to_owned(),
            command: request.command.to_owned(),
            purpose: lifecycle.purpose,
            hard_ceiling: self.hard_ceiling,
            retention: lifecycle.retention,
        })
    }

    fn completed_output(
        &self,
        verification: &ShellVerification,
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

        // The digest covers the bytes the command produced, not the rendered
        // output: the placeholder below and any later size policy are presentation,
        // while a citation is checked for drift against what actually ran.
        let mut full = String::from_utf8_lossy(&bytes).into_owned();
        let receipt = Self::guard_aware_receipt(verification, execution.exit_code, &bytes, &full)?;
        if full.is_empty() {
            full = "(no output)".to_owned();
        }
        let output = with_sandbox_metadata(
            ToolOutput::text(verification.command.as_str(), full)
                .with_metadata("exit", json!(execution.exit_code))
                .with_metadata("truncated", false)
                .with_metadata("background", false)
                .with_metadata("task_id", execution.id.as_str())
                .with_metadata("shell", self.shell.name())
                .with_metadata("timeout", json!(foreground_timeout_ms))
                .with_verification(&receipt),
            &execution.authority,
        );
        OutputPolicy::new(self.output_store.clone(), self.output_limits)
            .apply(TOOL_ID, session_id, output, accept_large_output)
            .map_err(|error| ToolError::Failed {
                tool: TOOL_ID.to_owned(),
                source: Box::new(error),
            })
    }

    /// The receipt for a finished run, reading a guard verdict as the guard's.
    ///
    /// Every background execution is launched behind [`zuno_process::guarded_argv`],
    /// so three exit codes may belong to the guard rather than to the command:
    ///
    /// - `125` says the guard's own machinery failed, and says nothing about whether
    ///   the command ran or what it changed. That is an uncertain outcome, not a
    ///   failure: it must never be replayed mechanically, so it leaves as
    ///   [`ToolError::Uncertain`] instead of a receipt claiming `Failed exit 125`.
    /// - `126` and `127` say the program was never started, so the code is not the
    ///   command's verdict and is recorded without authority.
    ///
    /// [`GuardExit::from_reported_run`] only reads a reserved code as the guard's when
    /// the guard's diagnostic is in the captured output, which both streams of a
    /// background execution carry, so a command that exits 125 of its own accord keeps
    /// its ordinary authoritative receipt.
    fn guard_aware_receipt(
        verification: &ShellVerification,
        exit_code: Option<i32>,
        bytes: &[u8],
        text: &str,
    ) -> Result<VerificationReceipt, ToolError> {
        let Some(code) = exit_code else {
            return Ok(verification.settled(None, bytes));
        };
        match GuardExit::from_reported_run(code, text) {
            GuardExit::GuardFailed => Err(ToolError::Uncertain {
                tool: TOOL_ID.to_owned(),
                applied_paths: Vec::new(),
                source: Box::new(io::Error::other(format!(
                    "the child-process guard failed around `{}`, so whether the command ran, and \
                     what it changed, is unknown from its exit status; inspect the authoritative \
                     state the command would have changed before deciding what to do next",
                    summarize_command(&verification.command)
                ))),
            }),
            GuardExit::NotFound => Ok(verification.never_ran(
                code,
                bytes,
                "the program was never started because it could not be found, so this exit code \
                 is the guard's and decides nothing about the command",
            )),
            GuardExit::NotExecutable => Ok(verification.never_ran(
                code,
                bytes,
                "the program exists but could not be executed, so it never ran and this exit code \
                 is the guard's",
            )),
            GuardExit::Exited(_) | GuardExit::Signaled(_) => {
                Ok(verification.settled(Some(code), bytes))
            }
        }
    }
}

/// Whether `interpreter` accepts the `set -o pipefail` prologue.
///
/// See [`PIPEFAIL_INTERPRETERS`] for why this is a name table rather than a probe
/// and why an unrecognised name answers `false`.
fn honours_pipefail(interpreter: &str) -> bool {
    PIPEFAIL_INTERPRETERS.contains(&interpreter)
}

/// The `set` prologue one POSIX policy needs, or `None` when it sets nothing.
///
/// A `pipefail` request on an interpreter that cannot honour it sets nothing at
/// all rather than emitting a line that would abort the shell; [`exit_contract`]
/// reports the resulting status as [`ExitAuthority::Derived`] so the silence is
/// visible to whoever reads the receipt.
fn posix_prologue(interpreter: &str, policy: ExitPolicy) -> Option<&'static str> {
    match (policy, honours_pipefail(interpreter)) {
        (ExitPolicy::Last, _) | (ExitPolicy::Pipefail, false) => None,
        (ExitPolicy::Pipefail, true) => Some("set -o pipefail"),
        (ExitPolicy::All, true) => Some("set -eo pipefail"),
        (ExitPolicy::All, false) => Some("set -e"),
    }
}

/// The script a POSIX interpreter is handed for `command` under `policy`.
///
/// The prologue is prepended on its own line rather than wrapping the command in a
/// subshell or a function. The caller's command therefore still runs in the same
/// shell at the same level, so its own `set`, `trap`, `cd`, `exec`, and variable
/// assignments behave exactly as they did before this wrapper existed — and a
/// command that deliberately turns an option back off still wins, because its
/// `set` runs after ours.
///
/// Only the interpreter sees this text. [`analyze_command`] and the
/// destructive-command gate run on the caller's command, so permission resources
/// and risk verdicts are decided on what the caller wrote, never on the wrapper.
fn posix_script(interpreter: &str, policy: ExitPolicy, command: &str) -> String {
    posix_prologue(interpreter, policy).map_or_else(
        || command.to_owned(),
        |prologue| format!("{prologue}\n{command}"),
    )
}

/// The script PowerShell is handed for `command` under `policy`.
///
/// PowerShell has no `pipefail`, and the asymmetry with [`posix_script`] is
/// deliberate rather than an omission: a pipeline's status is the last command's,
/// and a failed *native* command routinely leaves the status successful while only
/// `$LASTEXITCODE` records the failure. No prologue repairs that, so `pipefail`
/// and `last` set nothing and their status is reported as
/// [`ExitAuthority::Derived`]. `all` is the one policy PowerShell can honour: it
/// promotes errors to terminating and re-raises a native command's
/// `$LASTEXITCODE` as the process exit code, which is what makes that status
/// authoritative.
fn powershell_script(policy: ExitPolicy, command: &str) -> String {
    match policy {
        ExitPolicy::Pipefail | ExitPolicy::Last => command.to_owned(),
        ExitPolicy::All => format!(
            "$ErrorActionPreference = 'Stop'\n{command}\nif ($LASTEXITCODE) {{ exit $LASTEXITCODE }}"
        ),
    }
}

/// The interpreter arguments that run `command` under `policy`.
fn shell_arguments(
    kind: CommandShellKind,
    interpreter: &str,
    policy: ExitPolicy,
    command: &str,
) -> Vec<OsString> {
    match kind {
        CommandShellKind::PowerShell => vec![
            OsString::from("-NoLogo"),
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-Command"),
            OsString::from(powershell_script(policy, command)),
        ],
        CommandShellKind::Posix => vec![
            OsString::from("-lc"),
            OsString::from(posix_script(interpreter, policy, command)),
        ],
    }
}

/// The exit contract of the configuration a command actually runs under.
///
/// Derived from the interpreter and the policy the wrapper could honour, never
/// from the policy that was requested: a `pipefail` request on `dash` sets
/// nothing, so calling its status authoritative would manufacture exactly the
/// false confidence this policy exists to remove.
///
/// The configuration is only half of it. A caller's `set +e`, `|| true`, or inner
/// `bash -c` outlives the prologue, so a configuration that would otherwise be
/// authoritative is re-read against the text in [`authority::text_limitation`]
/// before this returns. That changes nothing about how the command runs — only what
/// the receipt is allowed to claim about the status it produced.
fn exit_contract(
    kind: CommandShellKind,
    interpreter: &str,
    policy: ExitPolicy,
    command: &str,
) -> ExitContract {
    let derived = |limitation: String| ExitContract {
        authority: ExitAuthority::Derived,
        limitation: Some(limitation),
    };
    // Lazy: the branches that are derived on configuration alone have nothing to
    // learn from the text, and parsing a command to answer a question already
    // settled would be work spent on an answer that cannot change.
    let configured = || match authority::text_limitation(kind, policy, command) {
        Some(limitation) => derived(limitation),
        None => ExitContract {
            authority: ExitAuthority::Authoritative,
            limitation: None,
        },
    };
    match (kind, policy) {
        (_, ExitPolicy::Last) => derived(format!(
            "exitPolicy \"{}\" reports only the last stage of a pipeline, so a failure in an \
             earlier stage is not reflected in this exit status",
            ExitPolicy::Last.as_str()
        )),
        (CommandShellKind::PowerShell, ExitPolicy::Pipefail) => derived(format!(
            "PowerShell has no pipefail equivalent, so this exit status is the last command's and \
             a failed native command need not have changed it; exitPolicy \"{}\" is the only \
             policy that covers the whole command here",
            ExitPolicy::All.as_str()
        )),
        (CommandShellKind::PowerShell, ExitPolicy::All) => configured(),
        (CommandShellKind::Posix, ExitPolicy::Pipefail | ExitPolicy::All) => {
            if honours_pipefail(interpreter) {
                configured()
            } else {
                derived(format!(
                    "the {interpreter} interpreter does not implement `set -o pipefail`, so a \
                     failure in an earlier pipeline stage is not reflected in this exit status"
                ))
            }
        }
    }
}

/// The command as one line of receipt summary, cut on a character boundary.
///
/// Whitespace is folded first so a heredoc or a line-continued command collapses
/// to something a checklist can hold, and the cut never splits a multi-byte
/// character in half.
fn summarize_command(command: &str) -> String {
    let single_line = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.len() <= SUMMARY_MAX_BYTES {
        return single_line;
    }
    let ellipsis = '…';
    let mut end = SUMMARY_MAX_BYTES.saturating_sub(ellipsis.len_utf8());
    while end > 0 && !single_line.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut summary = single_line[..end].trim_end().to_owned();
    summary.push(ellipsis);
    summary
}

fn with_sandbox_metadata(output: ToolOutput, authority: &ExecutionAuthority) -> ToolOutput {
    output
        .with_metadata("sandboxBackend", authority.backend.clone())
        .with_metadata("sandboxMode", json!(authority.mode))
        .with_metadata("sandboxNetwork", json!(authority.network))
        .with_metadata("sandboxRequestedMode", json!(authority.requested_mode()))
        .with_metadata(
            "sandboxRequestedNetwork",
            json!(authority.requested_network()),
        )
        .with_metadata("sandboxResolutionKind", json!(authority.resolution_kind))
        .with_metadata(
            "sandboxFallback",
            authority.resolution_kind == SandboxResolutionKind::UnavailableFallback,
        )
        .with_metadata("sandboxFallbackReason", json!(authority.fallback_reason))
}

/// Refuse a local history rewrite whose `HEAD` is not the one the caller approved.
///
/// Returns the full object id at `HEAD` when this call resolved one, so the
/// receipt can name the revision the command ran against without a second `git`
/// invocation. A call that needs no guard returns `Ok(None)`: filling a receipt
/// field is not worth adding a process spawn to every shell command, and an
/// unresolved revision is honestly reported as absent.
///
/// # Errors
///
/// [`ToolError::InvalidArgs`] when a rewrite arrives with a repository-redirecting
/// environment variable set, without `expectedGitHead`, or with a malformed one —
/// each of which means the caller has not proved which history it is rewriting.
/// [`ToolError::Failed`] when `HEAD` cannot be read or no longer matches, which
/// means the history moved since the caller looked; in every failing case the
/// command has not run.
/// Commit options that consume the token after them.
///
/// Needed only so a value is not mistaken for a flag: `git commit -m -a` would
/// otherwise look like a commit that stages everything, and the mistake costs a
/// refusal the caller cannot explain. Options spelled `--name=value` carry their value
/// already and need no entry. `-S` is deliberately absent: its key is optional and
/// attached, so it never consumes the next token.
const COMMIT_OPTIONS_TAKING_A_VALUE: &[&str] = &[
    "-m",
    "--message",
    "-F",
    "--file",
    "-c",
    "--reedit-message",
    "-C",
    "--reuse-message",
    "--author",
    "--date",
    "--fixup",
    "--squash",
    "--trailer",
    "--cleanup",
    "--pathspec-from-file",
];

/// Whether a `git commit` stages tracked modifications as part of committing.
///
/// `-a` arrives alone, inside a short cluster such as `-am`, or spelled `--all`.
/// Everything after `--` is a pathspec, so the scan stops there.
fn commit_stages_tracked_changes(arguments: &[String]) -> bool {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        let argument = unquote(argument);
        if COMMIT_OPTIONS_TAKING_A_VALUE.contains(&argument.as_str()) {
            index = index.saturating_add(2);
            continue;
        }
        if argument == "--" {
            return false;
        }
        if argument == "--all" {
            return true;
        }
        if let Some(cluster) = argument.strip_prefix('-')
            && !argument.starts_with("--")
            && !cluster.is_empty()
            && cluster.chars().all(|flag| flag.is_ascii_alphabetic())
            && cluster.contains('a')
        {
            return true;
        }
        index = index.saturating_add(1);
    }
    false
}

/// Whether this command line creates a commit, and whether it stages while doing it.
fn commit_delivery(analysis: &ShellAnalysis) -> Option<CommitDelivery> {
    analysis.commands.iter().find_map(|resource| {
        let (subcommand, arguments) = git_subcommand(&resource.tokens)?;
        (subcommand == "commit").then(|| CommitDelivery {
            stages_tracked_changes: commit_stages_tracked_changes(arguments),
        })
    })
}

/// What a `git commit` in this command line is about to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommitDelivery {
    /// Whether the commit stages tracked modifications itself, as `-a` does.
    ///
    /// The index alone is then not the whole delivery, so the worktree's tracked
    /// changes have to be read as well.
    stages_tracked_changes: bool,
}

/// The paths one git list command reports, read from its `-z` output.
///
/// `-z` because a path is bytes: git quotes anything unusual in its default output,
/// and a quoted path is not a path. `None` when git could not answer at all, which is
/// how "there is no repository here" arrives.
fn git_reported_paths(
    cwd: &Path,
    env: &BTreeMap<String, String>,
    arguments: &[&str],
) -> Result<Option<Vec<PathBuf>>, ToolError> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(failed)?;
    if !output.status.success() {
        return Ok(None);
    }
    let listing = String::from_utf8(output.stdout).map_err(failed)?;
    Ok(Some(
        listing
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .collect(),
    ))
}

/// Refuse a commit that would deliver Zuno's own generated working state.
///
/// Goal documents, tool output spills, background terminal state: files the runtime
/// writes to keep working, which the repository-private exclude block normally hides.
/// Committing them makes a later session read runtime residue as project source and
/// reason from it, and the reasoning looks well founded because the files are in git.
///
/// What is delivered is read from git rather than from the command line, because the
/// command line does not know it: an alias, a `-a`, a `commit.template`, or a
/// pre-commit hook that stages all put paths in a commit that no argument named. The
/// index is always read, and a commit that stages tracked modifications itself has
/// those read too.
///
/// Pathspecs are not classified. `git commit -- <path>` commits that path from the
/// worktree, so a generated path spelled there would pass, and that is the accepted
/// gap: a pathspec has to be typed deliberately, while the refusal for a
/// mis-classified message or option would land on an ordinary commit.
///
/// No repository, nothing delivered: when git cannot name a worktree the check does
/// not run, and the commit fails on its own terms rather than through a refusal about
/// generated state.
///
/// # Errors
///
/// [`ToolError::Failed`] carrying every generated path with the reason it exists and
/// the remedy, when a commit would deliver one.
fn refuse_generated_delivery(
    analysis: &ShellAnalysis,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<(), ToolError> {
    let Some(delivery) = commit_delivery(analysis) else {
        return Ok(());
    };
    let Some(worktree) = git_reported_paths(cwd, env, &["rev-parse", "--show-toplevel"])?
        .and_then(|paths| paths.into_iter().next())
    else {
        return Ok(());
    };
    let mut delivered = git_reported_paths(cwd, env, &["diff", "--cached", "--name-only", "-z"])?
        .unwrap_or_default();
    if delivery.stages_tracked_changes {
        delivered.extend(
            git_reported_paths(cwd, env, &["diff", "--name-only", "-z"])?.unwrap_or_default(),
        );
    }
    zuno_paths::refuse_generated_state(&worktree, &delivered).map_err(|refusal| {
        failed(io::Error::new(
            io::ErrorKind::PermissionDenied,
            refusal.report(),
        ))
    })
}

fn validate_expected_git_head(
    assessment: &RiskAssessment,
    expected: Option<&str>,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Option<String>, ToolError> {
    if !assessment.requires_expected_git_head() {
        return Ok(None);
    }
    if let Some(variable) = env.keys().find(|key| {
        GIT_REPOSITORY_ENVIRONMENT_VARIABLES
            .iter()
            .any(|variable| key.eq_ignore_ascii_case(variable))
    }) {
        return Err(invalid(format!(
            "{variable} may not be set for a local Git history rewrite; select the repository \
             with the Shell workdir"
        )));
    }
    let expected = expected.ok_or_else(|| {
        invalid(
            "expectedGitHead is required for commit --amend, rebase, and forced tag movement; \
             inspect `git rev-parse HEAD` immediately before this call",
        )
    })?;
    if !matches!(expected.len(), 40 | 64) || !expected.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid(
            "expectedGitHead must be a full 40- or 64-character hexadecimal object id",
        ));
    }
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(failed)?;
    if !output.status.success() {
        return Err(failed(io::Error::other(format!(
            "could not verify Git HEAD before history rewrite: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))));
    }
    let actual = String::from_utf8(output.stdout)
        .map_err(failed)?
        .trim()
        .to_owned();
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(failed(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Git HEAD changed before the history rewrite: expected {expected}, found {actual}; \
                 inspect the new history and prepare a fresh command"
            ),
        )));
    }
    Ok(Some(actual))
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
    // A command is fed by a pipe when it is not the first stage of its pipeline. Bash
    // nests the stages directly under `pipeline`; PowerShell puts them under
    // `pipeline_chain`, one chain per `&&`/`||` operand, so a chained command is
    // still the first stage of its own chain.
    let stdin_from_pipeline = source_node.parent().is_some_and(|parent| {
        matches!(parent.kind(), "pipeline" | "pipeline_chain")
            && parent
                .named_child(0)
                .is_some_and(|first| first.id() != source_node.id())
    });
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
        stdin_from_pipeline,
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
        // `number` is what tree-sitter-bash makes of a purely numeric word such as the
        // `10` in `nice -n 10 rg foo`. Dropping it shifted every later argument one
        // place left, so a wrapper option that takes a value swallowed the program
        // instead, and both the risk gate and the navigation gate lost sight of it.
        if matches!(
            child.kind(),
            "command_name"
                | "command_name_expr"
                | "word"
                | "number"
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

/// Zuno's own secrets, withheld from every model-composed command.
///
/// These three are set for the Zuno process itself — the HTTP server's credentials and
/// the provider auth store's contents — and no shell command a model writes has any
/// use for them. Inheriting them meant one `env`, one `printenv`, or one curl of an
/// attacker-chosen URL exfiltrated the operator's server password and every provider
/// key in the auth store.
///
/// Everything else is still inherited on purpose. A wildcard `*_API_KEY` / `*_TOKEN`
/// filter was rejected: it silently breaks `gh`, `aws`, `az`, and `gcloud` (see
/// [`THREE_TOKEN_COMMANDS`]) along with every user who exports a token deliberately,
/// and a tool that quietly removes the credentials a command needs is a worse failure
/// than one that keeps them. A deployment that wants a credential in the tool
/// environment supplies it through [`ShellEnvHook`], which runs after this removal, so
/// the host — not this crate — stays the single place that decides.
const WITHHELD_ENVIRONMENT: &[&str] = &[
    "ZUNO_AUTH_CONTENT",
    "ZUNO_SERVER_PASSWORD",
    "ZUNO_SERVER_USERNAME",
];

/// Whether a variable name is one of Zuno's own secrets.
///
/// Compared case-insensitively because Windows environment variable names are
/// case-insensitive, so `%zuno_server_password%` names the same secret there.
fn is_withheld_variable(name: &str) -> bool {
    WITHHELD_ENVIRONMENT
        .iter()
        .any(|withheld| name.eq_ignore_ascii_case(withheld))
}

/// The inherited environment with Zuno's own secrets removed.
///
/// Takes the variables as an argument so the decision is testable: setting a process
/// environment variable is `unsafe`, which this workspace forbids.
fn withhold_zuno_secrets(
    variables: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    variables
        .into_iter()
        .filter(|(name, _)| !is_withheld_variable(name))
        .collect()
}

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
        git_subcommand(&resource.tokens).is_some_and(|(subcommand, remaining)| {
            !git_subcommand_is_read_only(&subcommand, remaining)
        })
    })
}

/// The subcommand of a `git` invocation, lowercased, and the arguments after it.
///
/// `None` when the command is not git or names no subcommand at all. Global options
/// are skipped the way git parses them: the five that take a separate value consume
/// the token after them, and any other dashed token stands alone.
fn git_subcommand(tokens: &[String]) -> Option<(String, &[String])> {
    if !unquote(tokens.first()?).eq_ignore_ascii_case("git") {
        return None;
    }
    let mut index = 1;
    while let Some(argument) = tokens.get(index) {
        let argument = unquote(argument);
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
        return Some((argument.to_ascii_lowercase(), &tokens[index + 1..]));
    }
    None
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

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: TOOL_ID.to_owned(),
        source: Box::new(io::Error::new(io::ErrorKind::InvalidInput, message.into())),
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

    #[test]
    fn zunos_own_secrets_never_reach_a_model_composed_command() {
        let inherited = [
            ("ZUNO_SERVER_PASSWORD", "hunter2"),
            ("ZUNO_SERVER_USERNAME", "operator"),
            ("ZUNO_AUTH_CONTENT", r#"{"anthropic":{"key":"sk-live"}}"#),
            // Windows environment names are case-insensitive, so the same secret can
            // arrive under any spelling.
            ("zuno_auth_content", "{}"),
            // Deliberately kept: a wildcard `*_TOKEN` / `*_API_KEY` filter would take
            // these away and silently break `gh`, `aws`, `az`, and `gcloud`.
            ("GITHUB_TOKEN", "gho_kept"),
            ("AWS_ACCESS_KEY_ID", "AKIA_kept"),
            ("OPENAI_API_KEY", "sk-kept"),
            ("PATH", "/usr/bin"),
            // Non-secret Zuno variables a command may legitimately need to see.
            ("ZUNO_WORKSPACE_ID", "wsp_1"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));

        let env = withhold_zuno_secrets(inherited);

        assert_eq!(
            env.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "AWS_ACCESS_KEY_ID",
                "GITHUB_TOKEN",
                "OPENAI_API_KEY",
                "PATH",
                "ZUNO_WORKSPACE_ID",
            ]
        );
    }

    fn mutates(command: &str) -> bool {
        mutates_git_metadata(&analyze_command(command, ShellSyntax::Bash).expect("analysis"))
    }

    fn delivery(command: &str) -> Option<CommitDelivery> {
        commit_delivery(&analyze_command(command, ShellSyntax::Bash).expect("analysis"))
    }

    #[test]
    fn only_a_commit_is_a_delivery() {
        for command in [
            "git status --short",
            "git add .zuno/goal/ses_1.md",
            "git stash",
            "git commit-tree abc123",
            "commit -m not-git",
        ] {
            assert!(delivery(command).is_none(), "{command}");
        }
        assert!(delivery("git commit -m done").is_some());
        assert!(delivery("git -C repo commit --amend --no-edit").is_some());
        assert!(delivery("cargo test && git commit -m done").is_some());
    }

    #[test]
    fn a_commit_that_stages_as_it_commits_is_recognised_in_every_spelling() {
        for command in [
            "git commit -a -m done",
            "git commit -am done",
            "git commit --all -m done",
            "git commit -qam done",
        ] {
            assert!(
                delivery(command).expect("a commit").stages_tracked_changes,
                "{command}"
            );
        }
    }

    /// A value is not a flag. Reading one as `-a` would cost a refusal on a commit
    /// that stages nothing, and a refusal nobody can explain is worse than the risk
    /// it was guarding against.
    #[test]
    fn a_commit_message_that_looks_like_a_flag_stages_nothing() {
        for command in [
            "git commit -m done",
            "git commit -m -a",
            "git commit --message -a",
            "git commit --file -a",
            "git commit -m done -- -a",
        ] {
            assert!(
                !delivery(command).expect("a commit").stages_tracked_changes,
                "{command}"
            );
        }
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

    #[test]
    fn numeric_arguments_stay_in_place_so_wrapper_values_do_not_swallow_the_program() {
        let analysis = analyze_command("nice -n 10 rg foo", ShellSyntax::Bash).expect("analysis");
        assert_eq!(
            analysis.commands[0].tokens,
            ["nice", "-n", "10", "rg", "foo"].map(str::to_owned)
        );
        let analysis = analyze_command("head -5 Cargo.toml", ShellSyntax::Bash).expect("analysis");
        assert_eq!(
            analysis.commands[0].tokens,
            ["head", "-5", "Cargo.toml"].map(str::to_owned)
        );
    }

    #[test]
    fn only_a_later_pipeline_stage_is_marked_as_reading_the_pipe() {
        let piped = |command: &str, syntax: ShellSyntax| -> Vec<bool> {
            analyze_command(command, syntax)
                .expect("analysis")
                .commands
                .iter()
                .map(|resource| resource.stdin_from_pipeline)
                .collect()
        };
        assert_eq!(
            piped("cargo test 2>&1 | grep -c FAILED", ShellSyntax::Bash),
            [false, true]
        );
        assert_eq!(
            piped("cd crates && rg foo", ShellSyntax::Bash),
            [false, false]
        );
        assert_eq!(
            piped("rg foo 2>/dev/null | head", ShellSyntax::Bash),
            [false, true]
        );
        assert_eq!(
            piped("(cd crates; rg foo)", ShellSyntax::Bash),
            [false, false]
        );
        assert_eq!(
            piped("cargo test 2>&1 | grep -c FAILED", ShellSyntax::PowerShell),
            [false, true]
        );
        assert_eq!(
            piped("Set-Location crates && rg foo", ShellSyntax::PowerShell),
            [false, false]
        );
    }

    const PIPELINE: &str = "cargo test | tail -5";

    fn arguments(kind: CommandShellKind, interpreter: &str, policy: ExitPolicy) -> Vec<String> {
        shell_arguments(kind, interpreter, policy, PIPELINE)
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    /// The contract [`PIPELINE`] earns under one configuration.
    ///
    /// Every configuration test uses the same command so the verdict is the
    /// configuration's alone; the text's own effect on it has its own tests.
    fn contract(kind: CommandShellKind, interpreter: &str, policy: ExitPolicy) -> ExitContract {
        exit_contract(kind, interpreter, policy, PIPELINE)
    }

    #[test]
    fn the_default_policy_puts_pipefail_in_effect_for_a_posix_command() {
        assert_eq!(ExitPolicy::default(), ExitPolicy::Pipefail);
        assert_eq!(
            arguments(CommandShellKind::Posix, "bash", ExitPolicy::default()),
            vec!["-lc".to_owned(), format!("set -o pipefail\n{PIPELINE}")]
        );
        assert_eq!(
            contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
            ExitContract {
                authority: ExitAuthority::Authoritative,
                limitation: None,
            }
        );
    }

    #[test]
    fn the_last_policy_leaves_the_command_unwrapped_and_only_claims_a_derived_status() {
        assert_eq!(
            arguments(CommandShellKind::Posix, "bash", ExitPolicy::Last),
            vec!["-lc".to_owned(), PIPELINE.to_owned()]
        );
        let contract = contract(CommandShellKind::Posix, "bash", ExitPolicy::Last);
        assert_eq!(contract.authority, ExitAuthority::Derived);
        assert!(
            contract
                .limitation
                .is_some_and(|reason| reason.contains("only the last stage")),
            "the receipt must say why the status is partial"
        );
    }

    #[test]
    fn the_all_policy_adds_errexit_on_top_of_pipefail_for_a_posix_command() {
        assert_eq!(
            arguments(CommandShellKind::Posix, "zsh", ExitPolicy::All),
            vec!["-lc".to_owned(), format!("set -eo pipefail\n{PIPELINE}")]
        );
        assert_eq!(
            contract(CommandShellKind::Posix, "zsh", ExitPolicy::All).authority,
            ExitAuthority::Authoritative
        );
    }

    #[test]
    fn a_command_that_masks_its_own_status_is_derived_under_any_configuration() {
        // The best configuration this tool can build still runs whatever it was
        // given, and `|| true` survives `set -eo pipefail` untouched. A contract
        // read off the prologue alone would call this status authoritative and let
        // it close a success criterion the command never demonstrated.
        let masked = exit_contract(
            CommandShellKind::Posix,
            "bash",
            ExitPolicy::All,
            "cargo test || true",
        );
        assert_eq!(masked.authority, ExitAuthority::Derived);
        assert!(
            masked
                .limitation
                .as_deref()
                .is_some_and(|limitation| limitation.contains("|| true")),
            "{:?}",
            masked.limitation
        );

        // A configuration that was already derived keeps the reason it was derived
        // for: the interpreter's own gap is the more useful thing to report.
        let dash = exit_contract(
            CommandShellKind::Posix,
            "dash",
            ExitPolicy::All,
            "cargo test || true",
        );
        assert_eq!(dash.authority, ExitAuthority::Derived);
        assert!(
            dash.limitation
                .as_deref()
                .is_some_and(|limitation| limitation.contains("pipefail")),
            "{:?}",
            dash.limitation
        );
    }

    #[test]
    fn an_interpreter_without_pipefail_is_never_reported_as_authoritative() {
        // `set` is a special builtin, so emitting `set -o pipefail` to dash would
        // abort the shell before the caller's command ran. Saying so in the
        // receipt is the honest alternative to pretending the option took effect.
        assert_eq!(
            arguments(CommandShellKind::Posix, "dash", ExitPolicy::Pipefail),
            vec!["-lc".to_owned(), PIPELINE.to_owned()]
        );
        assert_eq!(
            arguments(CommandShellKind::Posix, "dash", ExitPolicy::All),
            vec!["-lc".to_owned(), format!("set -e\n{PIPELINE}")]
        );
        for policy in [ExitPolicy::Pipefail, ExitPolicy::All] {
            let contract = contract(CommandShellKind::Posix, "dash", policy);
            assert_eq!(contract.authority, ExitAuthority::Derived, "{policy:?}");
            assert!(
                contract
                    .limitation
                    .is_some_and(|reason| reason.contains("does not implement `set -o pipefail`")),
                "{policy:?}"
            );
        }
    }

    #[test]
    fn powershell_sets_nothing_under_pipefail_and_admits_the_status_is_partial() {
        for policy in [ExitPolicy::Pipefail, ExitPolicy::Last] {
            assert_eq!(
                arguments(CommandShellKind::PowerShell, "pwsh", policy),
                vec![
                    "-NoLogo".to_owned(),
                    "-NoProfile".to_owned(),
                    "-NonInteractive".to_owned(),
                    "-Command".to_owned(),
                    PIPELINE.to_owned(),
                ],
                "{policy:?}"
            );
            assert_eq!(
                contract(CommandShellKind::PowerShell, "pwsh", policy).authority,
                ExitAuthority::Derived,
                "{policy:?}"
            );
        }
        assert!(
            contract(CommandShellKind::PowerShell, "pwsh", ExitPolicy::Pipefail)
                .limitation
                .is_some_and(|reason| reason.contains("no pipefail equivalent")),
            "the asymmetry with POSIX belongs in the receipt, not only in the code"
        );
    }

    #[test]
    fn powershell_under_all_stops_on_error_and_re_raises_a_native_exit_code() {
        assert_eq!(
            arguments(CommandShellKind::PowerShell, "pwsh", ExitPolicy::All),
            vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                format!(
                    "$ErrorActionPreference = 'Stop'\n{PIPELINE}\nif ($LASTEXITCODE) {{ exit \
                     $LASTEXITCODE }}"
                ),
            ]
        );
        // The configuration is authoritative, so a command with a single native
        // program in it earns the full claim.
        assert_eq!(
            exit_contract(
                CommandShellKind::PowerShell,
                "pwsh",
                ExitPolicy::All,
                "cargo test --workspace"
            ),
            ExitContract {
                authority: ExitAuthority::Authoritative,
                limitation: None,
            }
        );
        // `PIPELINE` is the exception the one re-raise cannot cover: `$LASTEXITCODE`
        // holds `tail`'s code, and `cargo`'s is gone by the time the script reads it.
        let piped = contract(CommandShellKind::PowerShell, "pwsh", ExitPolicy::All);
        assert_eq!(piped.authority, ExitAuthority::Derived);
        assert!(
            piped
                .limitation
                .is_some_and(|reason| reason.contains("holds only the last one's code")),
            "the first native stage's status has to be admitted as lost"
        );
    }

    #[test]
    fn a_receipt_summary_is_one_line_cut_on_a_character_boundary() {
        assert_eq!(
            summarize_command("cargo test \\\n  --workspace"),
            "cargo test \\ --workspace"
        );

        let long = format!("printf '{}'", "é".repeat(200));
        let summary = summarize_command(&long);
        assert!(summary.len() <= SUMMARY_MAX_BYTES, "{}", summary.len());
        assert!(summary.ends_with('…'), "{summary}");
        assert!(!summary.contains('\n'));
        assert!(
            summary.starts_with("printf 'é"),
            "the head of the command has to stay legible: {summary}"
        );
    }

    #[test]
    fn a_launched_or_signalled_command_produces_a_receipt_that_proves_nothing() {
        let verification = ShellVerification {
            command: "cargo test --workspace".to_owned(),
            workdir: "/workspace".to_owned(),
            git_head: Some("f".repeat(40)),
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
        };

        let launched = verification.unresolved("still running in the background");
        assert!(!launched.proves_success());
        assert_eq!(launched.exit_authority, ExitAuthority::Absent);
        assert_eq!(launched.outcome, ReceiptOutcome::Unknown);
        assert_eq!(launched.exit_code, None);
        assert_eq!(launched.output_digest, None);
        assert_eq!(launched.workdir.as_deref(), Some("/workspace"));
        assert_eq!(launched.git_head, verification.git_head);

        let signalled = verification.settled(None, b"partial output");
        assert!(!signalled.proves_success());
        assert_eq!(signalled.exit_authority, ExitAuthority::Absent);
        assert!(
            signalled
                .detail
                .is_some_and(|detail| detail.contains("killed by a signal"))
        );
        assert_eq!(
            signalled.output_digest,
            Some(crate::read::digest_bytes(b"partial output"))
        );

        let passed = verification.settled(Some(0), b"ok");
        assert!(passed.proves_success());
        assert_eq!(passed.exit_code, Some(0));
        assert_eq!(passed.detail, None);

        let failed = verification.settled(Some(101), b"ok");
        assert!(!failed.proves_success());
        assert_eq!(failed.outcome, ReceiptOutcome::Failed);
        assert_eq!(failed.exit_code, Some(101));
    }

    #[test]
    fn a_guard_failure_is_an_uncertain_outcome_rather_than_a_failed_exit_125() {
        // Every background execution runs behind the child-process guard, so `exit 125`
        // plus the guard's diagnostic means the guard's own machinery broke: the command
        // may have run and changed anything. Rendering that as an authoritative
        // `Failed exit 125` invites the model to replay a call that already had effects.
        let verification = ShellVerification {
            command: "cargo publish -p zuno".to_owned(),
            workdir: "/workspace".to_owned(),
            git_head: None,
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
        };
        let output = format!(
            "{}pidfd_open: Permission denied\n",
            zuno_process::GUARD_DIAGNOSTIC_PREFIX
        );

        let error = ShellTool::guard_aware_receipt(
            &verification,
            Some(125),
            output.as_bytes(),
            output.as_str(),
        )
        .expect_err("a guard failure decides nothing");
        assert!(matches!(error, ToolError::Uncertain { .. }), "{error:?}");
        assert_eq!(error.recovery(), zuno_error::Recovery::Fail);
        // `describe` is what the dispatcher renders for the model, so the reason has to
        // survive the walk down the source chain.
        let rendered = zuno_error::source::describe(&error);
        assert!(
            rendered.contains("authoritative state the command would have changed"),
            "an uncertain outcome must ask for state inspection: {rendered}"
        );

        // The same code without the guard's diagnostic is the command's own choice.
        let ordinary = ShellTool::guard_aware_receipt(
            &verification,
            Some(125),
            b"make: *** [check] Error 125\n",
            "make: *** [check] Error 125\n",
        )
        .expect("an ordinary failure is not uncertain");
        assert_eq!(ordinary.exit_code, Some(125));
        assert_eq!(ordinary.exit_authority, verification.contract.authority);
        assert_eq!(ordinary.outcome, ReceiptOutcome::Failed);
    }

    #[test]
    fn a_program_that_never_started_yields_a_code_with_no_authority() {
        let verification = ShellVerification {
            command: "cargo-nextest run".to_owned(),
            workdir: "/workspace".to_owned(),
            git_head: None,
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::All),
        };
        assert_eq!(
            verification.contract.authority,
            ExitAuthority::Authoritative,
            "the point of the case is that the contract would otherwise claim authority"
        );
        let output = format!(
            "{}guarded program could not be started: No such file or directory\n",
            zuno_process::GUARD_DIAGNOSTIC_PREFIX
        );

        for (code, expected) in [(127, "could not be found"), (126, "could not be executed")] {
            let receipt = ShellTool::guard_aware_receipt(
                &verification,
                Some(code),
                output.as_bytes(),
                output.as_str(),
            )
            .expect("a program that never ran is an ordinary failure");
            assert!(!receipt.proves_success());
            assert_eq!(receipt.exit_code, Some(i64::from(code)));
            assert_eq!(
                receipt.exit_authority,
                ExitAuthority::Absent,
                "a code the command never produced cannot be cited as its verdict"
            );
            assert_eq!(receipt.outcome, ReceiptOutcome::Failed);
            assert!(
                receipt
                    .detail
                    .as_ref()
                    .is_some_and(|d| d.contains(expected)),
                "{:?} does not say why nothing ran",
                receipt.detail
            );
            assert_eq!(
                receipt.output_digest,
                Some(crate::read::digest_bytes(output.as_bytes()))
            );
        }
    }
}
