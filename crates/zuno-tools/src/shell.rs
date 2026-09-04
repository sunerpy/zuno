mod authority;

use crate::output_policy::OutputPolicy;
use crate::risk::{
    GIT_REPOSITORY_ENVIRONMENT_VARIABLES, GateOutcome, RiskAssessment, RiskContext,
    assess_and_gate, git_subcommand, git_uses_repository_override, nested_command_resources,
};
use crate::search_common::directory_grant_pattern;
use crate::timeout::{
    background_started_output, normalize_foreground_timeout, timeout_promoted_output,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
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
    ExitAuthority, InterruptHandle, OutputLimits, PermissionAsk, ReceiptOutcome, Tool, ToolContext,
    ToolOutput, ToolOutputStore, VerificationReceipt,
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

/// The metadata key carrying what a cancelled call still proved.
///
/// A cancelled `shell` result is a settled result, not an error, so a reader has to be
/// able to tell the two cancellations apart without parsing the notice: one where the
/// process had already exited and reported its own status, and one where it was killed
/// mid-flight and decided nothing. [`zuno_engine`]'s dispatcher reads `uncertain` from
/// here to decide whether the interruption it records needs authoritative inspection.
///
/// The engine does not depend on this crate, so the key is spelled once here and once
/// there; the contract is the JSON shape below, and both sides pin the spelling in a
/// test.
pub const METADATA_CANCELLATION_KEY: &str = "cancellation";

/// What a call the caller's interrupt ended is allowed to claim.
///
/// Serialized onto the result under [`METADATA_CANCELLATION_KEY`], beside — never
/// instead of — the notice a model reads, so a client surface and a later turn can act
/// on the facts without re-reading a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelledExecution {
    /// Always true: this key exists only on a result cancellation produced.
    pub cancelled: bool,
    /// Whether the reported facts decide what the command did.
    ///
    /// True only when the process exited on its own before the kill landed, so the
    /// exit status is the command's own verdict rather than the kill's.
    pub authoritative: bool,
    /// Whether the final side-effect state has to be inspected before anything else.
    ///
    /// The negation of [`Self::authoritative`], carried explicitly because it is the
    /// field a consumer acts on and a reader must not have to invert a claim to find
    /// it.
    pub uncertain: bool,
    /// The exit status the process reported before the kill landed, when it exited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The background service's terminal status for this execution.
    pub status: String,
    /// Why the outcome is, or is not, the command's own.
    pub detail: String,
}

impl CancelledExecution {
    /// The facts of a command that had already exited when the interrupt was serviced.
    fn exited(status: BackgroundExecutionStatus, exit_code: i32, detail: String) -> Self {
        Self {
            cancelled: true,
            authoritative: true,
            uncertain: false,
            exit_code: Some(exit_code),
            status: status.as_str().to_owned(),
            detail,
        }
    }

    /// The facts of a command that was still running and was killed.
    fn killed(status: BackgroundExecutionStatus, exit_code: Option<i32>, detail: String) -> Self {
        Self {
            cancelled: true,
            authoritative: false,
            uncertain: true,
            exit_code,
            status: status.as_str().to_owned(),
            detail,
        }
    }

    fn to_metadata_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
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

/// How long a call's pre-flight `git` reads may take *in total* before it is refused.
///
/// The reads decide whether a commit is allowed, so this ceiling is not a licence to
/// proceed without an answer: one that expires fails the call rather than reporting an
/// empty repository. Thirty seconds is far longer than an untracked-file walk needs on a
/// large worktree and far shorter than a session will spend on a `.git` that lives on a
/// stalled network mount, which is where the untimed reads hung forever.
///
/// Per phase rather than per read. `refuse_generated_delivery` makes up to four reads and
/// `validate_expected_git_head` one more, and five independent thirty-second ceilings is
/// two and a half minutes — long enough that a waiting user cannot tell a bounded
/// pre-flight from the hang it replaced. [`GitReadBudget`] takes the deadline once, when
/// the phase starts, and every read inside the phase races that one instant.
///
/// A total, not a bound on the reads alone. Stopping a read that did not answer takes time
/// of its own, and giving that the full ceiling a second time meant a configured thirty
/// seconds admitted a sixty-second phase — twice what this constant, the refusal message
/// and the documentation all say. The reads race `ceiling` minus
/// [`crate::child_process::teardown_ceiling`], the teardown gets that reserve, and the two
/// together are what this number bounds.
const GIT_READ_CEILING: Duration = Duration::from_secs(30);

pub struct ShellTool {
    workspace: PathBuf,
    shell: CommandShell,
    env_hook: Arc<dyn ShellEnvHook>,
    output_store: ToolOutputStore,
    output_limits: OutputLimits,
    hard_ceiling: Duration,
    git_ceiling: Duration,
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
        Self::with_sandbox_backend_and_generated_root(
            workspace,
            configured,
            sandbox,
            sandbox_policy,
            None,
        )
    }

    /// The same construction, with the generated root already resolved.
    ///
    /// `generated_root` is for a caller that is on a reactor. Resolving it costs up to
    /// three `git rev-parse` calls that are synchronous — bounded at ten seconds each in
    /// `zuno_paths`, so up to thirty seconds of a blocked *thread* — and this constructor
    /// is not async and cannot be, so an async caller resolves
    /// `zuno_paths::generated_root` off-reactor (one `tokio::task::spawn_blocking`) and
    /// hands the answer in here. It must be resolved from the same directory this
    /// constructor canonicalizes `workspace` to, because that is the worktree the exclude
    /// patterns are anchored in.
    ///
    /// `None` keeps the lazy resolution, which is right for every caller that was never on
    /// a reactor: a CLI one-shot, a test, or any synchronous host.
    pub fn with_sandbox_backend_and_generated_root(
        workspace: &Path,
        configured: Option<&str>,
        sandbox: Arc<dyn SandboxBackend>,
        sandbox_policy: SandboxPolicy,
        generated_root: Option<PathBuf>,
    ) -> io::Result<Self> {
        let workspace = workspace.canonicalize()?;
        let shell = zuno_pty::shells::command(configured)?;
        // Generated state is rooted at the worktree, because that is where the exclude
        // patterns are anchored and where `classify` looks; joining the project
        // directory onto a session's own directory put it somewhere nothing covered.
        // Resolved once because it spawns git — bounded now, but still synchronously, so
        // this line blocks its thread for up to thirty seconds on a `.git` that lives on a
        // stalled mount and must not run on a current-thread reactor; an async caller
        // supplies `generated_root` instead. Only these two roots move: the workspace
        // itself still decides the sandbox boundary, the default working directory, and
        // what a relative `workdir` resolves against.
        let generated_root =
            generated_root.unwrap_or_else(|| zuno_paths::generated_root(&workspace));
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
            git_ceiling: GIT_READ_CEILING,
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

    /// Shorten the pre-flight `git` phase ceiling, for a test that must watch one expire.
    #[must_use]
    pub const fn with_git_ceiling(mut self, ceiling: Duration) -> Self {
        self.git_ceiling = ceiling;
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
        // Every read in the pre-flight phase races the interrupt and shares one deadline.
        // Bounding the reads restored progress for every *other* session; it did nothing
        // for this call, which sat through up to five reads — five independent ceilings —
        // with no way for its own user to stop it, and `process_group(0)` took the
        // terminal's `SIGINT` away from them, so `ctx.interrupt` is the only cancellation
        // left that reaches them. The race lives inside `bounded_git_read` rather than
        // around this phase because that is where the child is: see [`GitReadBudget`].
        let budget = GitReadBudget::starting_now(self.git_ceiling, ctx.interrupt.as_ref());
        let git_head = validate_expected_git_head(
            &risk_assessment,
            params.expected_git_head.as_deref(),
            &cwd,
            &env,
            budget,
        )
        .await?;
        refuse_generated_delivery(&analysis, self.syntax(), &cwd, &env, budget).await?;
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
        // Taken before the command starts, because the report is the difference between
        // two observations rather than a guess from the command line: see [`WriteWatch`].
        let writes = WriteWatch::before(
            &analysis,
            &cwd,
            &self.workspace,
            &authorization.writable_roots,
        );
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
                // The settled info and the captured bytes are the whole point of this
                // arm: a cancelled command has usually already written something, and
                // the process may even have exited before the kill landed. Discarding
                // both used to turn a side effect that really happened into a bare
                // "interrupted" error.
                let settled = match self.background_executions.wait(&execution.id, None).await {
                    Ok(outcome) => outcome.info,
                    Err(error) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            error = %error,
                            "could not observe the terminal state of an interrupted \
                             foreground execution"
                        );
                        // The launch snapshot is still `Running`, which
                        // `cancelled_output` reads as an undecided outcome — exactly
                        // what a failed terminal wait leaves behind.
                        execution.clone()
                    }
                };
                let captured = match self.background_executions.finish_foreground(&execution.id) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            error = %error,
                            "could not remove interrupted foreground execution"
                        );
                        // The handoff is also how the captured bytes are read, so
                        // failing it would discard them a second way. The artifact is
                        // still there whenever the record is — the handoff removes it
                        // only after reading it — so it is read directly before this
                        // call gives up on the command's output.
                        self.background_executions
                            .complete_output(&execution.id)
                            .unwrap_or_default()
                    }
                };
                lease.disarm();
                // A cancelled command has usually already written something, which is the
                // whole reason its captured output is kept above.
                return self
                    .cancelled_output(
                        &verification,
                        foreground_timeout_ms,
                        &settled,
                        captured,
                        &ctx.session_id,
                        accept_large_output,
                    )
                    .map(|output| writes.report(output));
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
        // Reported for a failing exit status too: a command that wrote three files and then
        // failed on the fourth changed the workspace, and a consumer told nothing would go
        // on citing a check that no longer describes those files.
        self.completed_output(
            &verification,
            foreground_timeout_ms,
            waited.info,
            full,
            &ctx.session_id,
            accept_large_output,
        )
        .map(|output| writes.report(output))
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
        // The command line each resource runs through a wrapper or an inline script is
        // asked for as well, so a `deny` written as `rm -rf*` reaches the `rm -rf /`
        // inside `sh -c 'rm -rf /'`, `env rm -rf /` or `nice rm -rf /` — the permission
        // layer matches one flattened line and answered Ask for all three. The engine
        // refuses as soon as any pattern is denied, so this can only widen a deny; a
        // nested pattern left at ask can turn an allow into a prompt, never the reverse.
        // The inner shapes join `always` so one standing grant covers what was shown.
        let nested: Vec<CommandResource> = resources
            .iter()
            .flat_map(|resource| nested_command_resources(resource, self.syntax()))
            .filter(|resource| !resource.changes_directory)
            .collect();
        let mut patterns: Vec<String> = if resources.is_empty() {
            vec![command.to_owned()]
        } else {
            resources
                .iter()
                .map(|resource| resource.source.clone())
                .collect()
        };
        let mut always: Vec<String> = resources
            .iter()
            .map(|resource| resource.always.clone())
            .collect();
        for resource in &nested {
            if !patterns.contains(&resource.source) {
                patterns.push(resource.source.clone());
            }
            if !always.contains(&resource.always) {
                always.push(resource.always.clone());
            }
        }
        let ask = PermissionAsk {
            permission: TOOL_ID.to_owned(),
            patterns,
            metadata,
            always,
            ..PermissionAsk::default()
        };
        let ask = if risk_confirmation.is_some() {
            ask.require_manual()
        } else {
            ask
        };
        ctx.ask(TOOL_ID, ask).await?;

        let git_metadata_writable = mutates_git_metadata(analysis, self.syntax());
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
    ) -> Result<BTreeMap<OsString, OsString>, ToolError> {
        // Zuno's own environment is removed before the hook runs, so a host that wants a
        // credential in the tool environment can still put one back deliberately.
        //
        // `vars_os`, not `vars`: `std::env::vars` panics on any entry whose name or value
        // is not Unicode, and one such entry is ordinary on Linux and normal on Windows.
        // The panic was not confined to the call — it unwound the turn — so every `shell`
        // call in a process launched with, say, a Latin-1 filename in an environment
        // variable failed the same way, for a reason no message named.
        let mut env = withhold_zuno_environment(std::env::vars_os());
        let extra = self
            .env_hook
            .env(ShellEnvInput {
                cwd: cwd.to_owned(),
                session_id: ctx.session_id.clone(),
                call_id: ctx.call_id.clone(),
            })
            .await?;
        // A hook value is host-supplied configuration, which is `String` by construction;
        // it joins the inherited map as `OsString` so the whole environment has one type.
        env.extend(
            extra
                .into_iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        );
        Ok(env)
    }

    fn execution_input(
        &self,
        request: ShellRequest<'_>,
        env: BTreeMap<OsString, OsString>,
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
        let environment = env;
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
            // A terminal wait that returns a cancelled execution has the same facts the
            // interrupt arm has, and loses them the same way if it answers with an
            // error: the command ran, wrote output, and may have changed state.
            BackgroundExecutionStatus::Cancelled => {
                return self.cancelled_output(
                    verification,
                    foreground_timeout_ms,
                    &execution,
                    bytes,
                    session_id,
                    accept_large_output,
                );
            }
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
        // The bytes, not the lossy string: what a size policy persists is the copy that
        // outlives this call, and the ephemeral `<id>.output` file was already removed by
        // the foreground handoff above. Persisting the decoded text would have made the
        // artifact a copy of the damage — `U+FFFD` where the command's bytes were — with
        // no original left anywhere to page back.
        OutputPolicy::new(self.output_store.clone(), self.output_limits).apply_bytes(
            TOOL_ID,
            session_id,
            output,
            &bytes,
            accept_large_output,
        )
    }

    /// The settled result of a foreground call the caller's interrupt ended.
    ///
    /// Cancellation used to answer with a bare error, which threw away everything the
    /// command had written and told the model the tool had "completed its cleanup" —
    /// a clean, certain, effect-free reading of a command that may have been killed
    /// halfway through a write. This returns `Ok` instead, and separates the two
    /// cancellations that differ in what the model may do next:
    ///
    /// - the process had already exited when the interrupt was serviced, so its status
    ///   is its own verdict; the call is `cancelled` but not `uncertain`, and the
    ///   receipt is the one a completed run would have earned;
    /// - the process was killed, so it decided nothing; the receipt is
    ///   [`ShellVerification::unresolved`] and the notice says authoritative state has
    ///   to be inspected before anything is retried.
    ///
    /// A guard failure — `exit 125` with the guard's own diagnostic — is the second
    /// case even though a code exists, because that code says nothing about whether
    /// the command ran. [`Self::guard_aware_receipt`] already encodes that rule, so it
    /// is asked rather than re-derived: an outcome it declines to certify is exactly an
    /// outcome that cannot be cited.
    ///
    /// The preserved bytes go through [`OutputPolicy`], the same size policy a
    /// completed run uses, so oversized cancelled output is persisted byte for byte and
    /// withheld behind the windowed read rather than inlined or truncated. The notice
    /// is prepended after that decision, so it survives withholding and never counts
    /// against the threshold the command's own output is measured by.
    fn cancelled_output(
        &self,
        verification: &ShellVerification,
        foreground_timeout_ms: u64,
        execution: &BackgroundExecutionInfo,
        bytes: Vec<u8>,
        session_id: &str,
        accept_large_output: bool,
    ) -> Result<ToolOutput, ToolError> {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // Whatever the record holds, reported verbatim: a code that decides nothing is
        // still what a terminal would have shown, and dropping it would leave the
        // metadata disagreeing with the captured output.
        let reported = execution.exit_code;
        // Only `Completed` means the process reported a status of its own. Every other
        // terminal status says the number beside it, when there is one, is not the
        // command's verdict: `Cancelled` is the kill, a `Failed` is the hard ceiling or
        // an output capture that broke after the process had already exited, `Uncertain`
        // is a process that disappeared, and `Running` is the snapshot a failed terminal
        // wait leaves behind. Reading a code out of any of those would certify an
        // outcome the service explicitly declined to settle.
        let decided = matches!(execution.status, BackgroundExecutionStatus::Completed)
            .then_some(reported)
            .flatten();
        let certified = decided.and_then(|code| {
            Self::guard_aware_receipt(verification, Some(code), &bytes, &text)
                .ok()
                .map(|receipt| (code, receipt))
        });
        let (facts, receipt, notice) = match certified {
            Some((code, receipt)) => {
                let detail = format!(
                    "the command had already exited with status {code} when the cancellation was \
                     serviced, so this is its own outcome and the kill changed nothing about it"
                );
                let notice = format!(
                    "Cancelled by the user. The command had already exited with status {code} \
                     before the cancellation was serviced, so this result is the command's own \
                     outcome and the output below is complete."
                );
                (
                    CancelledExecution::exited(execution.status, code, detail),
                    receipt,
                    notice,
                )
            }
            None => {
                let reason = Self::undecided_cancellation_reason(execution, decided);
                let detail = format!(
                    "the command {reason}; whatever it had already changed is unknown from this \
                     result"
                );
                let notice = format!(
                    "Cancelled by the user. `{}` {reason}. Whatever output it had produced is \
                     below. Inspect the authoritative state this command would have changed \
                     before deciding what to do next; it must not be re-run on the assumption \
                     that it did nothing.",
                    summarize_command(&verification.command)
                );
                (
                    CancelledExecution::killed(execution.status, reported, detail.clone()),
                    verification.unresolved(detail),
                    notice,
                )
            }
        };
        let body = if text.is_empty() {
            "(no output)".to_owned()
        } else {
            text
        };
        let output = with_sandbox_metadata(
            ToolOutput::text(verification.command.as_str(), body)
                .with_metadata("exit", json!(facts.exit_code))
                .with_metadata("truncated", false)
                .with_metadata("background", false)
                .with_metadata("task_id", execution.id.as_str())
                .with_metadata("shell", self.shell.name())
                .with_metadata("timeout", json!(foreground_timeout_ms))
                .with_metadata(METADATA_CANCELLATION_KEY, facts.to_metadata_value())
                .with_verification(&receipt),
            &execution.authority,
        );
        let mut output = OutputPolicy::new(self.output_store.clone(), self.output_limits)
            .apply_bytes(TOOL_ID, session_id, output, &bytes, accept_large_output)?;
        output.output = format!("{notice}\n\n{}", output.output);
        Ok(output)
    }

    /// Why a cancelled call decided nothing, read off the record the service settled.
    ///
    /// The clause completes `` `<command>` … `` so the model-visible notice and the
    /// receipt's detail can both state the reason without either one inventing a fact
    /// the record does not carry. One fixed sentence cannot do that: an undecided
    /// cancellation reaches this point with a code as often as without one — the guard's
    /// own `125`, or the status a process had already reported before its output capture
    /// broke — and claiming the command "has no exit status" contradicts the `exit`
    /// metadata sitting beside it.
    fn undecided_cancellation_reason(
        execution: &BackgroundExecutionInfo,
        decided: Option<i32>,
    ) -> String {
        // A code the process really reported that `guard_aware_receipt` still declined:
        // the only outcome it declines is the guard's own failure.
        if let Some(code) = decided {
            return format!(
                "reported exit {code}, but that code is the child-process guard's own failure \
                 and says nothing about whether the command ran"
            );
        }
        if execution.timed_out {
            return "was still running at its hard ceiling and was killed there rather than \
                    reporting an outcome of its own"
                .to_owned();
        }
        if let Some(code) = execution.exit_code {
            let settled = execution.status.as_str();
            return match &execution.error {
                Some(error) => format!(
                    "reported exit {code}, but the execution settled as {settled} rather than as \
                     the command's own outcome: {error}"
                ),
                None => format!(
                    "reported exit {code}, but the execution settled as {settled} rather than as \
                     the command's own outcome"
                ),
            };
        }
        match execution.status {
            BackgroundExecutionStatus::Uncertain => {
                "left no authoritative terminal result behind, because its process disappeared"
                    .to_owned()
            }
            BackgroundExecutionStatus::Running => {
                "was cancelled without this call observing it reach a terminal state, so nothing \
                 about it was settled"
                    .to_owned()
            }
            BackgroundExecutionStatus::Cancelled
            | BackgroundExecutionStatus::Completed
            | BackgroundExecutionStatus::Failed => {
                "was still running and was killed, so it reported no outcome of its own".to_owned()
            }
        }
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

/// How much of the worktree a staging command reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StagedScope {
    /// The whole worktree, because no pathspec narrows it or none can be read.
    Worktree,
    /// Only these pathspecs, spelled the way the command spelled them.
    ///
    /// Passed back to git as pathspecs rather than compared as text: they are relative
    /// to where the command runs, and git's pathspec language is not the lexical
    /// comparison [`zuno_paths::refuse_generated_state`] performs.
    Paths(Vec<String>),
}

impl StagedScope {
    /// The union of two reaches.
    fn widen(&mut self, other: Self) {
        match (&mut *self, other) {
            (Self::Worktree, _) => {}
            (_, Self::Worktree) => *self = Self::Worktree,
            (Self::Paths(mine), Self::Paths(theirs)) => mine.extend(theirs),
        }
    }

    /// The pathspec arguments that limit a git read to this reach.
    ///
    /// `:/` is git's spelling for the whole worktree, which is what a read has to say
    /// when the command runs in a subdirectory: a bare `git diff` reports only the
    /// paths below it.
    fn pathspecs(&self) -> Vec<&str> {
        match self {
            Self::Worktree => vec![":/"],
            Self::Paths(paths) => paths.iter().map(String::as_str).collect(),
        }
    }
}

/// What a command line puts in the index before a commit in it runs.
///
/// The check reads git before the command runs, so the index it can read is the index as
/// it is *now*. A chain stages first — `git add -A && git commit -m wip` is the shape a
/// model writes most — and everything that `add` collects is invisible to a read of the
/// index taken before it. What the arguments do say is which part of the worktree the
/// staging reaches, and that is enough to read the same paths from the worktree instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StagedWork {
    /// Whether tracked modifications inside the reach are staged.
    ///
    /// This is the case no exclusion can answer. `.git/info/exclude` and a generated
    /// directory's own `.gitignore` suppress only an *untracked* path, so a goal
    /// document an earlier release committed is staged again by every `git add` that
    /// reaches it.
    tracked: bool,
    /// Whether files git does not track yet, and does not ignore, are staged too.
    ///
    /// False for `git add -u` and `git commit -a`, which update only what git already
    /// tracks.
    untracked: bool,
    /// How much of the worktree the staging reaches.
    scope: Option<StagedScope>,
}

impl StagedWork {
    /// Add everything `other` stages to what this already stages.
    fn absorb(&mut self, other: Self) {
        self.tracked |= other.tracked;
        self.untracked |= other.untracked;
        match (&mut self.scope, other.scope) {
            (_, None) => {}
            (Some(mine), Some(theirs)) => mine.widen(theirs),
            (slot @ None, theirs) => *slot = theirs,
        }
    }
}

/// What this command line's commits would deliver, read in the order the commands run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ChainDelivery {
    /// How the first commit that chooses its own repository chooses it.
    retarget: Option<Retarget>,
    /// What the chain stages before, or as part of, a commit.
    staged: StagedWork,
}

/// What every `git commit` in this command line would deliver, or `None` when it has
/// none.
///
/// Every commit, not the first: `git commit -m a && git commit -am b` delivers the
/// worktree's tracked changes through its second commit, and reading only the first said
/// the index was the whole delivery.
///
/// Order is why this is a walk and not a filter. A `git add` before a commit is part of
/// that commit's delivery; the same `git add` after the last commit is not, and reading
/// it as one would refuse a commit of source over a file staged for the next one.
fn chain_delivery(analysis: &ShellAnalysis, syntax: ShellSyntax) -> Option<ChainDelivery> {
    let mut pending = StagedWork::default();
    let mut delivery: Option<ChainDelivery> = None;
    for resource in &analysis.commands {
        let Some((subcommand, arguments)) = git_subcommand(&resource.tokens, syntax) else {
            continue;
        };
        match subcommand.as_str() {
            "add" | "stage" => pending.absorb(staged_by_add(arguments)),
            "commit" => {
                let chain = delivery.get_or_insert_default();
                chain.staged.absorb(std::mem::take(&mut pending));
                if commit_stages_tracked_changes(arguments) {
                    chain.staged.absorb(StagedWork {
                        tracked: true,
                        untracked: false,
                        scope: Some(StagedScope::Worktree),
                    });
                }
                if chain.retarget.is_none() {
                    chain.retarget = commit_retarget(resource);
                }
            }
            _ => {}
        }
    }
    delivery
}

/// How a `git commit` names a repository other than the one this call inspects, if it
/// does at all.
fn commit_retarget(resource: &CommandResource) -> Option<Retarget> {
    if git_uses_repository_override(&resource.tokens) {
        return Some(Retarget::Option);
    }
    leading_assignments(&resource.source)
        .find(|name| {
            GIT_REPOSITORY_ENVIRONMENT_VARIABLES
                .iter()
                .any(|variable| name.eq_ignore_ascii_case(variable))
        })
        .map(|name| Retarget::Assignment(name.to_owned()))
}

/// What one `git add` — or `git stage`, its alias — stages, read from its arguments.
///
/// A pathspec limits the reach; `-A` and `-u` without one reach the whole worktree; `-u`
/// leaves untracked files alone. A pathspec carrying a glob or one of git's magic
/// `:`-prefixed forms widens the reach to the worktree instead of being evaluated here,
/// and so does `--pathspec-from-file`, whose list lives in a file or on standard input.
/// Over-reporting the reach costs a refusal that names paths the caller can unstage;
/// under-reporting costs the commit this check exists to prevent.
///
/// A `git add` with neither a pathspec nor `-A` or `-u` stages nothing — git refuses it
/// — so it contributes nothing rather than everything.
fn staged_by_add(arguments: &[String]) -> StagedWork {
    let mut pathspecs: Vec<String> = Vec::new();
    let mut all = false;
    let mut update = false;
    let mut unreadable = false;
    let mut only_pathspecs = false;
    let mut index = 0;
    while let Some(raw) = arguments.get(index) {
        let argument = unquote(raw);
        index = index.saturating_add(1);
        if only_pathspecs || !argument.starts_with('-') || argument == "-" {
            pathspecs.push(argument);
            continue;
        }
        if argument == "--" {
            only_pathspecs = true;
            continue;
        }
        if let Some(long) = argument.strip_prefix("--") {
            match long.split('=').next().unwrap_or(long) {
                "all" | "no-ignore-removal" => all = true,
                "update" => update = true,
                "pathspec-from-file" => unreadable = true,
                "chmod" if !argument.contains('=') => index = index.saturating_add(1),
                _ => {}
            }
            continue;
        }
        for flag in argument.chars().skip(1) {
            match flag {
                'A' => all = true,
                'u' => update = true,
                _ => {}
            }
        }
    }
    let scope = if unreadable || pathspecs.iter().any(|spec| widens_to_the_worktree(spec)) {
        StagedScope::Worktree
    } else if pathspecs.is_empty() {
        if !(all || update) {
            return StagedWork::default();
        }
        StagedScope::Worktree
    } else {
        StagedScope::Paths(pathspecs)
    };
    StagedWork {
        tracked: true,
        untracked: all || !update,
        scope: Some(scope),
    }
}

/// Whether a pathspec is one this check reads as the whole worktree rather than as a
/// path.
///
/// `.` reaches everything below where the command runs, a `:`-prefixed pathspec is one
/// of git's magic forms — `:/` for the whole tree, `:(glob)` and the rest — and a glob
/// is a pattern git evaluates and this module does not.
fn widens_to_the_worktree(spec: &str) -> bool {
    spec.starts_with(':')
        || spec.contains(['*', '?', '['])
        || matches!(spec.trim_end_matches(['/', '\\']), "" | "." | "..")
}

/// How a commit points itself at a repository the check would not read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Retarget {
    /// A git global option: `-C`, `--git-dir`, `--work-tree`, `--namespace`.
    Option,
    /// An inline assignment of a Git repository variable, carrying its name.
    Assignment(String),
}

/// The variable names a command line assigns before it names its program.
///
/// The analyzer drops a `variable_assignment` from the token list, correctly — it is
/// not an argument — so `GIT_DIR=/elsewhere git commit` is indistinguishable from
/// `git commit` there and has to be read from the source. Only the leading run counts,
/// because that is the only position where a shell treats `NAME=value` as an assignment
/// to the command's environment: the same text after the program name is an argument,
/// and inside a commit message it is prose, and refusing that would be a refusal nobody
/// could explain.
fn leading_assignments(source: &str) -> impl Iterator<Item = &str> {
    source
        .split_whitespace()
        .map_while(|word| word.split_once('='))
        .take_while(|(name, _)| {
            !name.is_empty()
                && !name.starts_with(|first: char| first.is_ascii_digit())
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
        .map(|(name, _)| name)
}

/// What every pre-flight `git` read of a single call is allowed to spend, and what may
/// stop it early.
///
/// Carries the deadline rather than a per-read duration so the phase, not each read, is
/// what is bounded; `ceiling` travels with it only so the refusal can name the number a
/// reader would have to look up otherwise.
///
/// The interrupt travels with it because the cancellation and the teardown have to be
/// decided in the same place. Racing `ctx.interrupt` a level up, around the phase, reads
/// better but drops the read's future the moment the interrupt wins — and dropping it
/// reaps the leader, so whether the group is torn down or orphaned would come down to
/// which arm `tokio::select!` happened to poll first. The reads are the only awaits in the
/// phase, so racing here loses nothing and keeps the teardown deterministic.
///
/// `ceiling` is the whole phase, teardown included, which is why the deadline the reads
/// race is earlier than it. Stopping a read that did not answer costs time too — one
/// `kill(2)` on Unix, a `taskkill /t` on Windows — and giving that the full ceiling again
/// made a configured 30s bound admit a 60s phase. The reserve is carved out of the phase
/// instead: see [`crate::child_process::teardown_ceiling`].
#[derive(Clone, Copy)]
struct GitReadBudget<'a> {
    /// The whole phase, reads and teardown together. Named in the refusal, because it is
    /// the number an operator configured.
    ceiling: Duration,
    /// The tail of `ceiling` reserved for stopping a read that did not answer.
    teardown: Duration,
    /// When the reads themselves must be done: `ceiling` minus `teardown`.
    deadline: tokio::time::Instant,
    interrupt: &'a dyn InterruptHandle,
}

impl<'a> GitReadBudget<'a> {
    /// Starts the phase now.
    fn starting_now(ceiling: Duration, interrupt: &'a dyn InterruptHandle) -> Self {
        Self {
            ceiling,
            teardown: crate::child_process::teardown_ceiling(ceiling),
            deadline: tokio::time::Instant::now() + crate::child_process::work_window(ceiling),
            interrupt,
        }
    }
}

/// Why one bounded read stopped.
enum GitReadOutcome {
    /// git answered, well or badly.
    Answered(io::Result<std::process::Output>),
    /// The phase deadline passed first.
    Expired,
    /// The user cancelled the call first.
    Interrupted,
}

/// One pre-flight `git` read, bounded so a repository that never answers cannot park
/// the reactor.
///
/// Awaited rather than waited on: `zuno serve`, `zuno acp`, and `zuno run` all drive a
/// current-thread runtime, so a synchronous spawn-and-wait here stopped every other
/// session's provider stream, SSE frame, and interrupt for as long as git took — and
/// with `.git` on a stalled mount, permanently.
///
/// `Err` when git could not be started or did not answer before `budget`'s deadline.
/// Neither is an answer about the repository, and a caller that reads either as "nothing
/// to report" turns a hung read into permission to commit.
async fn bounded_git_read(
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    arguments: &[&str],
    budget: GitReadBudget<'_>,
) -> Result<std::process::Output, ToolError> {
    let mut process = Command::new("git");
    process
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A dedicated group so the read leads one, which is what lets the teardown below stop
    // a git that spawned helpers of its own together with them. It also takes the read out
    // of Zuno's process group, so a terminal `SIGINT` no longer reaches it; the phase
    // races `ctx.interrupt` instead, which is a cancellation every client surface has and
    // a foreground process group was not.
    #[cfg(unix)]
    process.process_group(0);
    let child = process.spawn().map_err(failed)?;
    // Registered while the child is certainly alive: on Unix this validates that the pid
    // leads its own group, and after the read is reaped the pid names nothing.
    let group = crate::child_process::group_of(&child);
    // Pinned rather than moved into `tokio::time::timeout`, because the order of the
    // teardown is the whole point. `timeout` drops the future it wraps before it returns,
    // and dropping this one runs `kill_on_drop`, which `SIGKILL`s *and reaps* the leader —
    // and a group whose leader has been reaped can no longer be named, so the helpers
    // would be signalled at nothing. Pinning keeps the child alive across the group kill;
    // the drop at the end of this function is what then reaps it.
    let read = child.wait_with_output();
    tokio::pin!(read);
    let outcome = tokio::select! {
        output = &mut read => GitReadOutcome::Answered(output),
        () = tokio::time::sleep_until(budget.deadline) => GitReadOutcome::Expired,
        () = budget.interrupt.notified() => GitReadOutcome::Interrupted,
    };
    match outcome {
        GitReadOutcome::Answered(output) => output.map_err(failed),
        // Both non-answers tear the group down first, for the same reason: the read is
        // about to be abandoned, and whatever git started beside it has nothing else that
        // will ever stop it.
        GitReadOutcome::Expired => {
            crate::child_process::stop_process_group(
                group,
                budget.teardown,
                "an expired pre-flight git read",
            )
            .await;
            Err(failed(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "`git {}` did not answer within the {:?} this command's pre-flight \
                     repository reads share, so the repository state it depends on is \
                     unknown; it was not run",
                    arguments.join(" "),
                    budget.ceiling
                ),
            )))
        }
        GitReadOutcome::Interrupted => {
            crate::child_process::stop_process_group(
                group,
                budget.teardown,
                "a cancelled pre-flight git read",
            )
            .await;
            Err(interrupted())
        }
    }
}

/// The paths one git list command reports, read from its `-z` output.
///
/// `-z` because a path is bytes: git quotes anything unusual in its default output,
/// and a quoted path is not a path. `None` when git answered that it cannot do this
/// here, which is how "there is no repository" arrives; a read that never answered is
/// an error instead, because an unknown repository is not an empty one.
async fn git_reported_paths(
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    arguments: &[&str],
    budget: GitReadBudget<'_>,
) -> Result<Option<Vec<PathBuf>>, ToolError> {
    let output = bounded_git_read(cwd, env, arguments, budget).await?;
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
/// command line does not know it: an alias, a `-a`, a `commit.template`, or a pre-commit
/// hook that stages all put paths in a commit that no argument named. The index is
/// always read.
///
/// A chain that stages before it commits is read from the worktree instead. The check
/// runs before the command, so `git add -A && git commit -m wip` has an index that says
/// nothing yet; what its arguments do say is how much of the worktree the staging
/// reaches, so the same reach is read as tracked modifications and as untracked
/// unignored files. That is the shape that matters most: an exclude rule cannot hide a
/// path git already tracks, so a `.zuno/goal` document an earlier release committed is
/// picked up by every `git add -A` until someone runs `git rm --cached` on it, and this
/// is the check that says so.
///
/// A pathspec is handed back to git as a pathspec, never compared as text. `git add
/// .zuno` and `git add -- ':(glob)**/*.md'` mean whatever git means by them, and a
/// lexical guess at that is how a check comes to report on paths a command never
/// touched. A pathspec whose reach cannot be read narrowly widens to the whole
/// worktree.
///
/// `git commit -- <path>` is still not classified: it commits that path from the
/// worktree without staging it, and that is the accepted gap — a pathspec on the commit
/// itself has to be typed deliberately, while the refusal for a mis-read message or
/// option would land on an ordinary commit.
///
/// A commit that chooses its own repository is refused instead of inspected. `-C`,
/// `--git-dir`, `--work-tree`, `--namespace` and an inline `GIT_DIR=…` all point the
/// commit somewhere these git reads do not follow, so inspecting anyway reports on a
/// repository that is not the one being written. The repository belongs in the tool's
/// `workdir`, where it is one fact both the check and the commit use.
///
/// No repository, nothing delivered: when git cannot name a worktree the check does not
/// run, and the commit fails on its own terms rather than through a refusal about
/// generated state.
///
/// # Errors
///
/// [`ToolError::InvalidArgs`] when a commit retargets its repository, and
/// [`ToolError::Failed`] carrying every generated path with the reason it exists and the
/// remedy, when a commit would deliver one.
async fn refuse_generated_delivery(
    analysis: &ShellAnalysis,
    syntax: ShellSyntax,
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    budget: GitReadBudget<'_>,
) -> Result<(), ToolError> {
    let Some(delivery) = chain_delivery(analysis, syntax) else {
        return Ok(());
    };
    if let Some(retarget) = &delivery.retarget {
        let chosen = match retarget {
            Retarget::Option => {
                "a Git global option (`-C`, `--git-dir`, `--work-tree`, `--namespace`)".to_owned()
            }
            Retarget::Assignment(name) => format!("`{name}`"),
        };
        return Err(invalid(format!(
            "this `git commit` selects its repository with {chosen}, which points it at a \
             repository this call does not inspect, so the check for Zuno's own generated \
             state would report on a different one; select the repository with the Shell \
             workdir instead"
        )));
    }
    let Some(worktree) = git_reported_paths(cwd, env, &["rev-parse", "--show-toplevel"], budget)
        .await?
        .and_then(|paths| paths.into_iter().next())
    else {
        return Ok(());
    };
    // `diff.relative` is a configuration a repository may set, and with it on, `git diff`
    // reports paths relative to the current directory and omits the ones above it: a
    // session in a subdirectory would then be told that generated state at the worktree
    // root is not part of the delivery.
    let mut delivered = git_reported_paths(
        cwd,
        env,
        &[
            "-c",
            "diff.relative=false",
            "diff",
            "--cached",
            "--name-only",
            "-z",
        ],
        budget,
    )
    .await?
    .unwrap_or_default();
    if let Some(scope) = &delivery.staged.scope {
        let pathspecs = scope.pathspecs();
        if delivery.staged.tracked {
            let mut arguments = vec![
                "-c",
                "diff.relative=false",
                "diff",
                "--name-only",
                "-z",
                "--",
            ];
            arguments.extend(pathspecs.iter().copied());
            delivered.extend(
                git_reported_paths(cwd, env, &arguments, budget)
                    .await?
                    .unwrap_or_default(),
            );
        }
        if delivery.staged.untracked {
            // `--full-name` for the same reason `diff.relative=false` is set above: without
            // it `ls-files` answers relative to the current directory, and a path above it
            // comes back with `../` segments that name no worktree entry.
            let mut arguments = vec![
                "ls-files",
                "--others",
                "--exclude-standard",
                "--full-name",
                "-z",
                "--",
            ];
            arguments.extend(pathspecs.iter().copied());
            delivered.extend(
                git_reported_paths(cwd, env, &arguments, budget)
                    .await?
                    .unwrap_or_default(),
            );
        }
    }
    zuno_paths::refuse_generated_state(&worktree, &delivered).map_err(|refusal| {
        failed(io::Error::new(
            io::ErrorKind::PermissionDenied,
            refusal.report(),
        ))
    })
}

async fn validate_expected_git_head(
    assessment: &RiskAssessment,
    expected: Option<&str>,
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    budget: GitReadBudget<'_>,
) -> Result<Option<String>, ToolError> {
    if !assessment.requires_expected_git_head() {
        return Ok(None);
    }
    if let Some(variable) = env.keys().find(|key| {
        GIT_REPOSITORY_ENVIRONMENT_VARIABLES
            .iter()
            .any(|variable| key.eq_ignore_ascii_case(variable))
    }) {
        // Lossy only to *name* the variable in the refusal. The decision above compares the
        // real `OsStr`, so a name Zuno cannot spell is still refused, and a lossy spelling
        // is never what the comparison sees.
        let variable = variable.to_string_lossy();
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
    let output = bounded_git_read(cwd, env, &["rev-parse", "--verify", "HEAD"], budget).await?;
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
    let mut commands: Vec<CommandResource> = nodes
        .into_iter()
        .filter_map(|node| command_resource(node, command.as_bytes()))
        .collect();
    // A tree with an error node is a tree whose command list cannot be trusted: the
    // parser recovers by skipping text, so `r[m] -rf /` — a subscript to the grammar,
    // `rm -rf /` to bash once a file named `rm` sits in the cwd — yielded the single
    // command `rf /`, and every gate saw a harmless `rf`. The line as written, tokenised
    // lexically, joins the list so the gates also see the program the shell would
    // resolve (here a glob, so dynamic). Deny-side only: the fragments stay, and a
    // well-formed command adds nothing. Bash only, because `lexical_tokens` is a POSIX
    // tokenizer and PowerShell quoting is not.
    if syntax == ShellSyntax::Bash && tree.root_node().has_error() {
        let source = command.trim();
        if !source.is_empty()
            && !commands.iter().any(|resource| resource.source == source)
            && let Some(resource) =
                resource_from_tokens(source.to_owned(), lexical_tokens(source), false)
        {
            commands.push(resource);
        }
    }
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
    resource_from_tokens(source, tokens, stdin_from_pipeline)
}

/// The resource for `source` once its words are known, however they were found.
fn resource_from_tokens(
    source: String,
    tokens: Vec<String>,
    stdin_from_pipeline: bool,
) -> Option<CommandResource> {
    if tokens.is_empty() {
        return None;
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
        //
        // `ansi_c_string` (`$'…'`) and `translated_string` (`$"…"`) are words too.
        // Dropping them made `sh -c $'rm -rf /'` a `sh -c` with no script and
        // `rm -rf $'/'` an `rm -rf` with no target, so neither gate saw anything. A
        // bare expansion or substitution (`$SUB`, `${DIR}`, `$(cmd)`, `$((n))`,
        // `<(cmd)`) is a word the shell computes; dropping it made `git $SUB --force`
        // a `git --force` with no subcommand. Kept as written, its `$` is what tells
        // the gates the word is dynamic.
        if matches!(
            child.kind(),
            "command_name"
                | "command_name_expr"
                | "word"
                | "number"
                | "string"
                | "raw_string"
                | "ansi_c_string"
                | "translated_string"
                | "simple_expansion"
                | "expansion"
                | "command_substitution"
                | "arithmetic_expansion"
                | "process_substitution"
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

/// Zuno's own environment, withheld from every model-composed command.
///
/// `ZUNO_*` is Zuno's private configuration namespace: the HTTP server's credentials,
/// the provider auth store's contents, and the inline configuration layer that carries a
/// provider `apiKey`. No shell command a model writes has any use for them, and
/// inheriting them meant one `env`, one `printenv`, or one curl of an attacker-chosen
/// URL exfiltrated the lot.
///
/// The whole namespace goes, rather than the names known to hold a secret today. That
/// list was wrong once already: `ZUNO_AUTH_CONTENT` was withheld while
/// `ZUNO_CONFIG_CONTENT` — the same injected-document shape, carrying the same provider
/// keys — was not, because it arrived as configuration rather than as credentials. A
/// list of secrets has to stay right for every variable Zuno ever reads; withholding the
/// namespace has to be right once, and the next variable Zuno invents is withheld before
/// anybody has had to notice it holds anything.
///
/// Everything outside the namespace is still inherited on purpose. A wildcard
/// `*_API_KEY` / `*_TOKEN` filter was rejected: it silently breaks `gh`, `aws`, `az`,
/// and `gcloud` (see [`THREE_TOKEN_COMMANDS`]) along with every user who exports a token
/// deliberately, and a tool that quietly removes the credentials a command needs is a
/// worse failure than one that keeps them. A deployment that wants a credential in the
/// tool environment supplies it through [`ShellEnvHook`], which runs after this removal,
/// so the host — not this crate — stays the single place that decides.
///
/// Not one name in the namespace is inherited, including the ones that look like bare
/// identity. `ZUNO_PID` was allowlisted once on the reasoning that a pid is neither a
/// document nor a credential: true of the value and beside the point, because the pid is
/// the *address* of every document withheld above. On an unconfined path `tr '\0' '\n' <
/// /proc/$ZUNO_PID/environ` on Linux and `ps eww $ZUNO_PID` on macOS read the Zuno
/// process environment straight back — verified from a descendant on Linux with
/// `kernel.yama.ptrace_scope=1` — so handing the pid over turns a discovery step into a
/// one-liner. `ZUNO_CLIENT` and `ZUNO_WORKSPACE_ID` went with it because no consumer
/// needs them in a model-composed command: `ZUNO_CLIENT` is read by Zuno itself in
/// process (see [`crate::exposure::ENV_CLIENT`]) and `ZUNO_WORKSPACE_ID` is read by
/// nothing outside the CLI's own flag snapshot. `ZUNO` and `AGENT`, the bare markers that
/// say a process was launched by Zuno, are outside the namespace and untouched, so a
/// script that only asks "am I under Zuno?" is unaffected.
///
/// Withholding is defence in depth, not a containment boundary. It removes the easiest
/// read — one `env` — and it does not make Zuno's environment unreadable: under
/// `danger-full-access`, under the trusted `run-unconfined` fallback, and on macOS and
/// Windows where there is no confined backend at all, a command that knows or finds the
/// Zuno pid can still read that process's environment. The boundary is the sandbox; the
/// Linux `workspace-write` and `read-only` backends are what actually deny it, with
/// `--unshare-pid` and a private `/proc`.
const ZUNO_ENVIRONMENT_PREFIX: &str = "ZUNO_";

/// Whether a variable belongs to Zuno's own environment rather than the command's.
///
/// Case-folded because Windows environment variable names are case-insensitive, so
/// `%zuno_server_password%` names the same secret there. The fold is only ever allowed to
/// grow this set: it decides what is *withheld*, so a spelling it collapses is withheld
/// on every platform, and there is no allowlist for it to collapse a name onto. An
/// exception list compared this way would be the other direction — a Linux `ZUNO_Pid`,
/// which is a different variable from `ZUNO_PID` and could hold anything, would have been
/// inherited for looking like an allowlisted name.
///
/// The name arrives as an [`OsStr`] because that is what the process really holds — bytes
/// on Unix, UTF-16 code units on Windows — and `to_string_lossy` is the only portable way
/// to compare it. That is a reduction, so it is audited in both directions rather than
/// assumed safe: it feeds the *withheld* side, and it can neither lose a match nor invent
/// one. It cannot lose one because a name whose first five bytes are `ZUNO_` still spells
/// `ZUNO_` after the conversion — ASCII is never part of an ill-formed sequence in either
/// encoding, so the replacement character can only appear elsewhere in the name. It cannot
/// invent one because `U+FFFD` is not ASCII, so no unrepresentable byte can become one of
/// the five. There is no allowlist left for it to collapse a name onto.
fn is_withheld_variable(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    let Some(prefix) = name.as_bytes().get(..ZUNO_ENVIRONMENT_PREFIX.len()) else {
        return false;
    };
    prefix.eq_ignore_ascii_case(ZUNO_ENVIRONMENT_PREFIX.as_bytes())
}

/// The inherited environment with Zuno's own environment removed.
///
/// Takes the variables as an argument so the decision is testable: setting a process
/// environment variable is `unsafe`, which this workspace forbids.
///
/// `OsString` rather than `String` all the way through, because the alternative is to
/// decide what happens to a variable Zuno cannot spell, and every answer to that is worse
/// than not having to answer. `std::env::vars` *panics* on an entry that is not Unicode,
/// which took the whole turn down and not just the call; collecting into `String` instead
/// would have had to drop such an entry, silently changing the environment a command runs
/// in. A child receives its `OsString` name and value unchanged, whatever it holds, and
/// the withholding decision above still applies to it.
pub(crate) fn withhold_zuno_environment(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    variables
        .into_iter()
        .filter(|(name, _)| !is_withheld_variable(name))
        .collect()
}

/// The most path tokens one call watches for a write.
///
/// A `stat` per candidate before the command and one after is cheap, but a generated
/// command line can hold thousands of arguments, and a write report is not worth an
/// unbounded amount of work on the turn's critical path. Sixty-four is far more than any
/// hand-written or model-written command names and small enough that the pair of passes
/// stays inside a millisecond. Past the cap the report is a shorter lower bound, which is
/// what it already is for every command whose targets are not statically resolvable.
const MAX_WRITE_CANDIDATES: usize = 64;

/// What a `stat` says about a regular file, to the precision two `stat`s can compare.
///
/// Modification time and length, and nothing platform-specific: an inode number would be
/// sharper on Unix and does not exist on Windows, and `created()` is unsupported on some
/// Linux filesystems. A rewrite that preserves *both* fields is not observed — which is a
/// residual of the mechanism, not of the command: see [`WriteWatch`].
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileFacts {
    modified: Option<std::time::SystemTime>,
    len: u64,
}

impl FileFacts {
    /// `None` when `path` is not a regular file right now — absent, a directory, or a
    /// device.
    ///
    /// Following symlinks deliberately: a command that writes through a link changes the
    /// target, and the target is the file whose content a later reader will see. A
    /// directory is never reported, because "the directory changed" says a file inside it
    /// was created or removed without saying which, and a path that names no file is not
    /// something a consumer can re-read.
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

/// The paths a shell call is watching, and what each held before the command ran.
///
/// `shell` is the one mutating tool that reported nothing. `write`, `edit` and
/// `apply_patch` each record what they wrote through
/// [`zuno_tool::ToolOutput::with_written_path`], and every consumer of that key — the goal
/// store's escalation from a question to a change and the freshness mark that retires a
/// stale verification receipt (`crates/zuno-cli/src/cmd/verification_ledger.rs`), the
/// changed-file list an interrupted call settles with
/// (`crates/zuno-engine/src/dispatch.rs`), the `ToolDispatchCompleted` event every client
/// renders from, and the ACP tool-call locations — was therefore blind to a command that
/// edited the workspace. `sed -i 's/foo/bar/' src/lib.rs` completed a user-created goal
/// with zero evidence, because nothing had told the store that anything changed.
///
/// Observed, never inferred. The report is what two `stat`s of the same path disagree
/// about, so a path is reported because the filesystem says it changed and not because a
/// command name looked like a writer. That makes the report a **lower bound**, and
/// deliberately so — a fabricated path is worse than a missing one, because a consumer
/// re-reads it and retires evidence over it. What is outside the bound:
///
/// * A target that is not statically resolvable — `$OUT`, `*.rs`, `$(ls)`, a here-doc, a
///   redirection — is skipped rather than guessed. Resolving it would mean re-implementing
///   the shell's own expansion, and getting that wrong invents paths.
/// * A path outside the workspace and outside the directories this call was granted is not
///   watched, so the report stays about state a later session can read.
/// * A deletion is not reported, matching
///   [`zuno_tool::METADATA_WRITTEN_PATHS_KEY`]: the key means "this file is now here to be
///   re-read".
/// * A rewrite that leaves both modification time and length unchanged is invisible to
///   `stat`, and a command promoted to the background is still writing when this call ends.
struct WriteWatch {
    before: Vec<(PathBuf, Option<FileFacts>)>,
}

impl WriteWatch {
    /// Snapshot every statically resolvable path token this call could write.
    fn before(analysis: &ShellAnalysis, cwd: &Path, workspace: &Path, granted: &[PathBuf]) -> Self {
        let mut candidates = BTreeSet::new();
        for resource in &analysis.commands {
            // `cd` names a directory and writes nothing, and its argument would otherwise
            // be stat'ed on every compound command.
            if resource.changes_directory {
                continue;
            }
            let Some(program) = resource.tokens.first() else {
                continue;
            };
            let program = unquote(program).to_ascii_lowercase();
            for argument in path_arguments(&resource.tokens, &program) {
                if candidates.len() >= MAX_WRITE_CANDIDATES {
                    break;
                }
                let argument = unquote(argument);
                // A token the shell would expand is not a path this process can resolve;
                // see the lower-bound note on [`WriteWatch`].
                if argument.is_empty() || is_dynamic_path(&argument) {
                    continue;
                }
                let candidate = if Path::new(&argument).is_absolute() {
                    PathBuf::from(&argument)
                } else {
                    cwd.join(&argument)
                };
                if !watchable(&candidate, cwd, workspace, granted) {
                    continue;
                }
                candidates.insert(candidate);
            }
        }
        Self {
            before: candidates
                .into_iter()
                .map(|path| {
                    let facts = FileFacts::of(&path);
                    (path, facts)
                })
                .collect(),
        }
    }

    /// The watched paths whose file changed or came into existence since [`Self::before`].
    ///
    /// Reported canonical, the way `write` and `edit` report theirs, so two tools naming
    /// the same file produce the same string. A canonicalization that fails — the file was
    /// removed between the comparison and this call — falls back to the resolved path
    /// rather than dropping the report.
    fn written(&self) -> Vec<PathBuf> {
        self.before
            .iter()
            .filter_map(|(path, before)| {
                let after = FileFacts::of(path)?;
                // Absent before and a file now: created. A file both times with different
                // facts: rewritten. Same facts, or gone now: nothing to report.
                if before.is_some_and(|before| before == after) {
                    return None;
                }
                Some(path.canonicalize().unwrap_or_else(|_| path.clone()))
            })
            .collect()
    }

    /// Record what this call was observed to write on `output`.
    fn report(&self, mut output: ToolOutput) -> ToolOutput {
        for path in self.written() {
            output = output.with_written_path(&path);
        }
        output
    }
}

/// Whether a write to `path` is inside what this call was allowed to change.
///
/// The workspace, the working directory, and any external directory the user granted this
/// call. Prefix comparison on already-resolved paths, with no case folding and no
/// separator rewriting: a reduction here would let a path match a root that does not
/// contain it, and this predicate decides what gets *reported*, so widening it invents
/// reports. A path outside every root is simply not watched.
fn watchable(path: &Path, cwd: &Path, workspace: &Path, granted: &[PathBuf]) -> bool {
    path.starts_with(workspace)
        || path.starts_with(cwd)
        || granted.iter().any(|root| path.starts_with(root))
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

fn mutates_git_metadata(analysis: &ShellAnalysis, syntax: ShellSyntax) -> bool {
    analysis.commands.iter().any(|resource| {
        git_subcommand(&resource.tokens, syntax).is_some_and(|(subcommand, remaining)| {
            !git_subcommand_is_read_only(&subcommand, remaining)
        })
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

    /// tree-sitter-bash reads `r[m]` as a subscript, recovers from the error by skipping
    /// text, and leaves `rf /` as the only command — so the risk gate assessed `rf` and
    /// the permission layer was asked for `rf /`, while bash, given a file named `rm` in
    /// the cwd, runs `rm -rf /`. An error tree adds the line as written.
    #[test]
    fn a_parse_error_adds_the_whole_line_so_no_gate_sees_only_a_fragment() {
        let analysis = analyze_command("r[m] -rf /", ShellSyntax::Bash).expect("analysis");
        let sources: Vec<&str> = analysis
            .commands
            .iter()
            .map(|resource| resource.source.as_str())
            .collect();
        assert_eq!(sources, vec!["rf /", "r[m] -rf /"]);
        assert_eq!(
            analysis.commands[1].tokens,
            vec!["r[m]".to_owned(), "-rf".to_owned(), "/".to_owned()]
        );

        let malformed = analyze_command("echo 'unterminated", ShellSyntax::Bash).expect("analysis");
        assert!(
            malformed
                .commands
                .iter()
                .any(|resource| resource.source == "echo 'unterminated"),
            "{malformed:?}"
        );

        // A well-formed simple command is exactly its one parsed resource, and a
        // well-formed compound never gains its whole line as a resource.
        for simple in ["rm -rf /", "sh -c 'rm -rf /'"] {
            let analysis = analyze_command(simple, ShellSyntax::Bash).expect("analysis");
            assert_eq!(analysis.commands.len(), 1, "{simple:?} -> {analysis:?}");
            assert_eq!(analysis.commands[0].source, simple);
        }
        for compound in [
            "for f in *.rs; do echo $f; done",
            "[[ -f x ]] && echo y",
            "(cd x && make)",
            "echo a && echo b || echo c",
            "cargo test 2>&1 | tail -20",
        ] {
            let analysis = analyze_command(compound, ShellSyntax::Bash).expect("analysis");
            assert!(
                !analysis
                    .commands
                    .iter()
                    .any(|resource| resource.source == compound),
                "a well-formed line gains no whole-line resource: {compound:?} -> {analysis:?}"
            );
        }
    }

    /// The pre-flight phase spends its ceiling once, not twice.
    ///
    /// The teardown of a read that never answered used to be given the full `ceiling`
    /// *after* the reads had already spent it, so the phase a `shell` call could sit in was
    /// twice the number [`GIT_READ_CEILING`], the refusal message, and the documentation all
    /// state: a configured thirty seconds admitted sixty. The reserve is carved out of the
    /// phase instead, so the advertised number is the total.
    #[tokio::test]
    async fn a_pre_flight_read_phase_spends_its_ceiling_once() {
        let interrupt = zuno_tool::NeverInterrupted;
        let budget = GitReadBudget::starting_now(GIT_READ_CEILING, &interrupt);
        // Read after construction, so `reads` is the window still ahead of a caller that is
        // about to spawn: measuring from before would add the clock's own delta to it and
        // make the sum look like an overrun that is not there.
        let now = tokio::time::Instant::now();
        let reads = budget.deadline.saturating_duration_since(now);

        assert_eq!(
            budget.ceiling, GIT_READ_CEILING,
            "the refusal names the ceiling, so it has to be the configured one"
        );
        assert_eq!(
            budget.teardown,
            Duration::from_secs(3),
            "the shipped thirty-second ceiling reserves three seconds for the teardown"
        );
        assert!(
            reads + budget.teardown <= GIT_READ_CEILING,
            "the phase can spend {reads:?} of reads plus a {:?} teardown, which is more than \
             the {GIT_READ_CEILING:?} it advertises",
            budget.teardown
        );
        assert!(
            reads >= Duration::from_secs(26),
            "the reads were left only {reads:?} of the {GIT_READ_CEILING:?} phase"
        );
    }

    #[test]
    fn zunos_own_environment_never_reaches_a_model_composed_command() {
        let inherited = [
            ("ZUNO_SERVER_PASSWORD", "hunter2"),
            ("ZUNO_SERVER_USERNAME", "operator"),
            ("ZUNO_AUTH_CONTENT", r#"{"anthropic":{"key":"sk-live"}}"#),
            // The inline configuration layer carries a provider `apiKey` exactly the way
            // the auth store does, and a list of named secrets missed it.
            (
                "ZUNO_CONFIG_CONTENT",
                r#"{"provider":{"anthropic":{"options":{"apiKey":"sk-live"}}}}"#,
            ),
            // Windows environment names are case-insensitive, so the same secret can
            // arrive under any spelling.
            ("zuno_auth_content", "{}"),
            // Withheld without being named anywhere: whatever Zuno reads next is not a
            // secret this list has to have anticipated.
            ("ZUNO_PROVIDER_TOKEN", "sk-live"),
            ("ZUNO_DB", "/srv/zuno/zuno.db"),
            // Deliberately kept: a wildcard `*_TOKEN` / `*_API_KEY` filter would take
            // these away and silently break `gh`, `aws`, `az`, and `gcloud`.
            ("GITHUB_TOKEN", "gho_kept"),
            ("AWS_ACCESS_KEY_ID", "AKIA_kept"),
            ("OPENAI_API_KEY", "sk-kept"),
            ("PATH", "/usr/bin"),
            // The bare markers are outside the namespace, so a command still learns it
            // runs under Zuno.
            ("AGENT", "1"),
            ("ZUNO", "1"),
            // Withheld even though the values are identity rather than content:
            // `ZUNO_PID` is the address of every document above — `/proc/$ZUNO_PID/environ`
            // reads them back on an unconfined path — and neither of the other two has a
            // consumer in a model-composed command.
            ("ZUNO_CLIENT", "cli"),
            ("ZUNO_PID", "4242"),
            ("ZUNO_WORKSPACE_ID", "wsp_1"),
            // A Linux-only spelling of an identity name. It is a different variable from
            // `ZUNO_PID` and could hold anything, which is why the case fold decides only
            // what is withheld and never what is kept.
            ("ZUNO_Pid", "4242"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));

        let env = withhold_zuno_environment(inherited);

        assert_eq!(
            env.keys()
                .map(|name| name.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                "AGENT",
                "AWS_ACCESS_KEY_ID",
                "GITHUB_TOKEN",
                "OPENAI_API_KEY",
                "PATH",
                "ZUNO",
            ]
        );
    }

    /// An entry Zuno cannot spell is decided by the same rule as one it can.
    ///
    /// The name arrives as an [`OsStr`], so the comparison has to reduce it to compare it,
    /// and a reduction on the withholding path is only safe in one direction. Both
    /// polarities are pinned here: a `ZUNO_`-prefixed name whose tail is not Unicode is
    /// still withheld — an unspellable tail is not a way around the namespace — and a name
    /// outside the namespace is kept with its bytes unchanged, rather than being dropped for
    /// being unrepresentable or handed on in a lossy spelling.
    ///
    /// Unix-gated because building the input needs a platform encoding: the Windows
    /// equivalent is an unpaired surrogate through `OsStringExt::from_wide`, and the rule
    /// under test is the same one.
    #[cfg(unix)]
    #[test]
    fn an_environment_entry_zuno_cannot_spell_is_withheld_by_the_same_rule() {
        use std::os::unix::ffi::OsStringExt as _;

        fn unspellable(prefix: &[u8]) -> OsString {
            let mut bytes = prefix.to_vec();
            // `0xE9` is `é` in Latin-1 and can start no UTF-8 sequence.
            bytes.push(0xE9);
            OsString::from_vec(bytes)
        }

        let kept_name = unspellable(b"LOCALE_PROBE_");
        let value = unspellable(b"caf");
        let env = withhold_zuno_environment([
            (kept_name.clone(), value.clone()),
            (unspellable(b"ZUNO_PROBE_"), value.clone()),
            (unspellable(b"zuno_probe_"), value.clone()),
            (OsString::from("ZUNO_PROBE"), value.clone()),
        ]);

        assert_eq!(
            env.keys().cloned().collect::<Vec<_>>(),
            vec![kept_name.clone()],
            "the only entry outside Zuno's namespace is the one that should have been kept"
        );
        assert_eq!(
            env.get(&kept_name),
            Some(&value),
            "the value reached the map in a spelling other than its own bytes"
        );
    }

    fn mutates(command: &str) -> bool {
        mutates_git_metadata(
            &analyze_command(command, ShellSyntax::Bash).expect("analysis"),
            ShellSyntax::Bash,
        )
    }

    fn delivery(command: &str) -> Option<ChainDelivery> {
        delivery_in(command, ShellSyntax::Bash)
    }

    fn delivery_in(command: &str, syntax: ShellSyntax) -> Option<ChainDelivery> {
        chain_delivery(&analyze_command(command, syntax).expect("analysis"), syntax)
    }

    fn staged(command: &str) -> StagedWork {
        delivery(command).expect("a commit").staged
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
    fn a_commit_that_stages_as_it_commits_reaches_every_tracked_change() {
        for command in [
            "git commit -a -m done",
            "git commit -am done",
            "git commit --all -m done",
            "git commit -qam done",
        ] {
            assert_eq!(
                staged(command),
                StagedWork {
                    tracked: true,
                    untracked: false,
                    scope: Some(StagedScope::Worktree),
                },
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
            assert_eq!(staged(command), StagedWork::default(), "{command}");
        }
    }

    /// The index a commit delivers is the index after the `git add` in front of it, and
    /// the check runs before either. Reading only the index said `git add -A && git
    /// commit -m wip` delivered nothing, which is the shape a model writes most.
    #[test]
    fn what_a_chain_stages_before_it_commits_is_part_of_the_delivery() {
        assert_eq!(
            staged("git add -A && git commit -m wip"),
            StagedWork {
                tracked: true,
                untracked: true,
                scope: Some(StagedScope::Worktree),
            }
        );
        assert_eq!(
            staged("git add -u && git commit -m wip"),
            StagedWork {
                tracked: true,
                untracked: false,
                scope: Some(StagedScope::Worktree),
            }
        );
        assert_eq!(
            staged("git stage src/lib.rs; git commit -m wip"),
            StagedWork {
                tracked: true,
                untracked: true,
                scope: Some(StagedScope::Paths(vec!["src/lib.rs".to_owned()])),
            }
        );
    }

    /// A pathspec that reaches everything is not a path. `.` and git's magic forms
    /// widen the read instead of being compared as text, because a lexical guess at
    /// git's pathspec language reports on paths the command never touched.
    #[test]
    fn a_pathspec_that_reaches_everything_widens_the_read_to_the_worktree() {
        for command in [
            "git add . && git commit -m wip",
            "git add ./ && git commit -m wip",
            "git add :/ && git commit -m wip",
            "git add ':(glob)**/*.md' && git commit -m wip",
            "git add '*.md' && git commit -m wip",
            "git add --pathspec-from-file=list && git commit -m wip",
        ] {
            assert_eq!(
                staged(command).scope,
                Some(StagedScope::Worktree),
                "{command}"
            );
        }
    }

    /// A `git add` after the last commit stages for the next call, not for this one.
    /// Absorbing it would refuse a commit of source over a file nobody delivered yet.
    #[test]
    fn staging_after_the_last_commit_is_not_part_of_the_delivery() {
        assert_eq!(
            staged("git commit -m done && git add -A"),
            StagedWork::default()
        );
        assert_eq!(
            staged("git add src/lib.rs && git commit -m first && git add -A"),
            StagedWork {
                tracked: true,
                untracked: true,
                scope: Some(StagedScope::Paths(vec!["src/lib.rs".to_owned()])),
            }
        );
    }

    /// `git add` needs a pathspec or `-A`/`-u`; without one git stages nothing, so
    /// widening the read to the worktree would be a refusal about a command that did
    /// not run.
    #[test]
    fn a_git_add_that_names_nothing_stages_nothing() {
        assert_eq!(
            staged("git add --dry-run && git commit -m done"),
            StagedWork::default()
        );
    }

    /// A check keyed on the program spelled `git` is a check a model steps around by
    /// accident. An absolute path is what a script writes, `.exe` is what Windows
    /// resolves to, and a backslash is how that path is spelled there.
    #[test]
    fn a_commit_is_a_delivery_however_the_program_is_spelled() {
        for command in [
            "/usr/bin/git commit -m done",
            "GIT commit -m done",
            "git.exe commit -m done",
            "/usr/local/bin/git.exe commit -m done",
        ] {
            assert!(delivery(command).is_some(), "{command}");
        }
        for command in [
            r"C:\Program\git.exe commit -m done",
            r"C:\Program\GIT.EXE commit -m done",
        ] {
            assert!(
                delivery_in(command, ShellSyntax::PowerShell).is_some(),
                "{command}"
            );
        }
    }

    /// The first commit is not the delivery. Reading only it said the index was
    /// everything, while the `-a` that follows delivers the whole worktree.
    #[test]
    fn every_commit_in_a_chain_is_read_not_only_the_first() {
        assert_eq!(
            staged("git commit -m first && git commit -am second"),
            StagedWork {
                tracked: true,
                untracked: false,
                scope: Some(StagedScope::Worktree),
            }
        );
    }

    #[test]
    fn a_commit_that_selects_its_own_repository_is_recognised_as_one() {
        for command in [
            "git -C other commit -m done",
            "git --git-dir=/elsewhere/.git commit -m done",
            "git --work-tree /elsewhere commit -m done",
            "git --namespace=zuno commit -m done",
        ] {
            assert_eq!(
                delivery(command).expect("a commit").retarget,
                Some(Retarget::Option),
                "{command}"
            );
        }
        assert_eq!(
            delivery("GIT_DIR=/elsewhere/.git git commit -m done")
                .expect("a commit")
                .retarget,
            Some(Retarget::Assignment("GIT_DIR".to_owned()))
        );
    }

    /// `NAME=value` is an assignment only in front of the program. Reading it anywhere
    /// would refuse an ordinary commit for what its message says.
    #[test]
    fn a_repository_variable_in_a_commit_message_is_prose() {
        for command in [
            "git commit -m GIT_DIR=/elsewhere",
            "git commit -m 'set GIT_WORK_TREE=. first'",
            "git commit -m done",
        ] {
            assert_eq!(
                delivery(command).expect("a commit").retarget,
                None,
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

    /// A sandbox that exists only to satisfy the constructor.
    ///
    /// Assembling a cancelled result never spawns anything: it reads a settled
    /// execution record and the bytes that were captured. A backend that refuses
    /// `prepare` therefore keeps the test honest — reaching it would mean the
    /// assembly path had started a command.
    #[derive(Debug)]
    struct NeverSpawningSandbox(zuno_sandbox::SandboxCapabilities);

    impl NeverSpawningSandbox {
        fn new() -> Self {
            Self(zuno_sandbox::SandboxCapabilities {
                backend: "test_never_spawning".to_owned(),
                executable: None,
                read_only: true,
                workspace_write: true,
                danger_full_access: true,
                network_isolation: true,
            })
        }
    }

    impl SandboxBackend for NeverSpawningSandbox {
        fn capabilities(&self) -> &zuno_sandbox::SandboxCapabilities {
            &self.0
        }

        fn prepare(
            &self,
            _request: PrepareRequest,
        ) -> Result<zuno_sandbox::PreparedCommand, zuno_sandbox::SandboxError> {
            unreachable!("assembling a cancelled result must not launch a command")
        }
    }

    fn assembly_tool(workspace: &Path) -> ShellTool {
        let policy = SandboxPolicy::new(
            workspace,
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
        )
        .expect("test sandbox policy");
        ShellTool::with_sandbox_backend(
            workspace,
            None,
            Arc::new(NeverSpawningSandbox::new()),
            policy,
        )
        .expect("shell tool")
    }

    fn test_authority(workspace: &Path) -> ExecutionAuthority {
        ExecutionAuthority {
            schema_version: 3,
            backend: "test_never_spawning".to_owned(),
            backend_executable: None,
            workspace: workspace.to_owned(),
            mode: SandboxMode::WorkspaceWrite,
            network: NetworkAccess::Allowed,
            requested_mode: None,
            requested_network: None,
            resolution_kind: SandboxResolutionKind::ExplicitNative,
            fallback_reason: None,
            writable_roots: vec![workspace.to_owned()],
            protected_paths: Vec::new(),
            cwd: workspace.to_owned(),
            command_sha256: String::new(),
            environment_keys: Vec::new(),
            approval_mode: "never".to_owned(),
            reviewer_policy_sha256: String::new(),
        }
    }

    /// A settled execution record, as the terminal wait in the interrupt arm returns one.
    fn settled_execution(
        workspace: &Path,
        status: BackgroundExecutionStatus,
        exit_code: Option<i32>,
    ) -> BackgroundExecutionInfo {
        BackgroundExecutionInfo {
            id: zuno_pty::BackgroundExecutionId::parse(format!("bg_{}", "a".repeat(32)))
                .expect("a well-formed execution id"),
            session_id: "ses_cancel".to_owned(),
            title: "cargo test --workspace".to_owned(),
            command: "cargo test --workspace".to_owned(),
            purpose: BackgroundExecutionPurpose::Command,
            cwd: workspace.to_owned(),
            status,
            pid: None,
            exit_code,
            timed_out: false,
            time_created: 0,
            time_updated: 0,
            time_completed: Some(0),
            error: None,
            output_file: workspace.join("execution.output"),
            status_file: workspace.join("execution.status"),
            authority: test_authority(workspace),
        }
    }

    fn cancellation_facts(output: &ToolOutput) -> &Value {
        output
            .metadata
            .get(METADATA_CANCELLATION_KEY)
            .expect("a cancelled result states what it proved")
    }

    /// Cancelling a command keeps what it wrote, and says the outcome is undecided.
    ///
    /// The bytes and the exit information used to be discarded on the way out, leaving
    /// the model an "interrupted" error for a command that had already written output
    /// and may have been killed mid-write.
    #[test]
    fn a_killed_command_keeps_its_output_and_reports_an_undecided_outcome() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let tool = assembly_tool(workspace.path());
        let verification = ShellVerification {
            command: "cargo test --workspace".to_owned(),
            workdir: workspace.path().to_string_lossy().into_owned(),
            git_head: None,
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
        };
        let execution =
            settled_execution(workspace.path(), BackgroundExecutionStatus::Cancelled, None);

        let output = tool
            .cancelled_output(
                &verification,
                120_000,
                &execution,
                b"running 3 tests\ntest one ... ok\n".to_vec(),
                "ses_cancel",
                false,
            )
            .expect("a cancelled command is a settled result, not a failure");

        assert!(
            output.output.contains("running 3 tests\ntest one ... ok\n"),
            "every captured byte has to survive cancellation: {}",
            output.output
        );
        assert!(
            output.output.contains("Inspect the authoritative state"),
            "an undecided outcome must ask for state inspection: {}",
            output.output
        );
        let facts = cancellation_facts(&output);
        assert_eq!(facts["cancelled"], true);
        assert_eq!(facts["uncertain"], true);
        assert_eq!(facts["authoritative"], false);
        assert_eq!(facts["status"], "cancelled");
        assert!(facts.get("exitCode").is_none());
        assert_eq!(output.metadata["exit"], Value::Null);

        // The receipt is the part a later success claim is checked against, so a killed
        // command must not leave one that could be cited.
        let receipt = VerificationReceipt::from_metadata(&output.metadata)
            .expect("decodable receipt")
            .expect("a cancelled result carries a receipt");
        assert_eq!(receipt.outcome, ReceiptOutcome::Unknown);
        assert_eq!(receipt.exit_authority, ExitAuthority::Absent);
        assert_eq!(receipt.exit_code, None);
        assert!(!receipt.proves_success());
        assert!(
            receipt
                .detail
                .is_some_and(|detail| detail.contains("still running")),
            "the receipt has to say why nothing was decided"
        );
    }

    /// A command that had already exited keeps its own verdict.
    ///
    /// The kill lands on a process that is already gone, so nothing about the command
    /// changed: the exit status is authoritative and the call is `cancelled` without
    /// being `uncertain`. Reporting it as undecided would send the model to inspect
    /// state that the command itself already reported on.
    #[test]
    fn a_command_that_exited_before_the_kill_landed_reports_its_own_exit_status() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let tool = assembly_tool(workspace.path());
        let verification = ShellVerification {
            command: "cargo test --workspace".to_owned(),
            workdir: workspace.path().to_string_lossy().into_owned(),
            git_head: Some("c".repeat(40)),
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
        };
        let execution = settled_execution(
            workspace.path(),
            BackgroundExecutionStatus::Completed,
            Some(101),
        );

        let output = tool
            .cancelled_output(
                &verification,
                120_000,
                &execution,
                b"test result: FAILED. 2 passed; 1 failed\n".to_vec(),
                "ses_cancel",
                false,
            )
            .expect("an exited command is a settled result");

        assert!(
            output
                .output
                .contains("test result: FAILED. 2 passed; 1 failed")
        );
        assert!(
            output.output.contains("already exited with status 101"),
            "{}",
            output.output
        );
        assert!(
            !output.output.contains("Inspect the authoritative state"),
            "a decided outcome must not send the model looking for state: {}",
            output.output
        );
        let facts = cancellation_facts(&output);
        assert_eq!(facts["cancelled"], true);
        assert_eq!(facts["uncertain"], false);
        assert_eq!(facts["authoritative"], true);
        assert_eq!(facts["exitCode"], 101);
        assert_eq!(output.metadata["exit"], 101);

        let receipt = VerificationReceipt::from_metadata(&output.metadata)
            .expect("decodable receipt")
            .expect("a cancelled result carries a receipt");
        assert_eq!(receipt.outcome, ReceiptOutcome::Failed);
        assert_eq!(receipt.exit_code, Some(101));
        assert_eq!(receipt.exit_authority, ExitAuthority::Authoritative);
        assert_eq!(receipt.git_head, verification.git_head);
    }

    /// A guard failure is undecided even though an exit code exists.
    ///
    /// `exit 125` with the guard's own diagnostic says the supervisor broke, so it
    /// decides nothing about whether the command ran. Cancellation must read it the way
    /// a completed run does rather than promote it to an authoritative verdict just
    /// because a number arrived.
    #[test]
    fn a_guard_failure_during_cancellation_is_still_an_undecided_outcome() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let tool = assembly_tool(workspace.path());
        let verification = ShellVerification {
            command: "cargo publish -p zuno".to_owned(),
            workdir: workspace.path().to_string_lossy().into_owned(),
            git_head: None,
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
        };
        let execution = settled_execution(
            workspace.path(),
            BackgroundExecutionStatus::Completed,
            Some(125),
        );
        let captured = format!(
            "{}pidfd_open: Permission denied\n",
            zuno_process::GUARD_DIAGNOSTIC_PREFIX
        );

        let output = tool
            .cancelled_output(
                &verification,
                120_000,
                &execution,
                captured.into_bytes(),
                "ses_cancel",
                false,
            )
            .expect("a guard failure is a settled result too");

        let facts = cancellation_facts(&output);
        assert_eq!(facts["uncertain"], true);
        assert_eq!(facts["authoritative"], false);
        assert_eq!(
            facts["exitCode"], 125,
            "the code is still recorded, it just proves nothing"
        );
        assert!(output.output.contains("Inspect the authoritative state"));
        // The notice and the metadata are read together, so the notice must not deny a
        // code the metadata reports. It says which claim the code cannot support.
        assert!(
            output
                .output
                .contains("that code is the child-process guard's own failure"),
            "an undecided outcome that has a code has to say what the code fails to \
             prove: {}",
            output.output
        );
        assert!(
            !output.output.contains("was still running and was killed"),
            "the guard failed; the command was not observed being killed: {}",
            output.output
        );
    }

    /// A capture failure around a cancelled command is not the command's verdict.
    ///
    /// The background service reports the code a process had already given and then
    /// settles the execution as `failed` when the output it captured was incomplete. An
    /// ordinary completed run refuses to return that as a result at all, so cancellation
    /// must not promote it to an authoritative `exit 0` with a receipt a later success
    /// claim could cite.
    #[test]
    fn a_cancelled_command_whose_output_capture_broke_is_not_certified() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let tool = assembly_tool(workspace.path());
        let verification = ShellVerification {
            command: "cargo build --release".to_owned(),
            workdir: workspace.path().to_string_lossy().into_owned(),
            git_head: None,
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
        };
        let mut execution =
            settled_execution(workspace.path(), BackgroundExecutionStatus::Failed, Some(0));
        execution.error = Some("capturing command output failed: broken pipe".to_owned());

        let output = tool
            .cancelled_output(
                &verification,
                120_000,
                &execution,
                b"Compiling zuno v0.6.6\n".to_vec(),
                "ses_cancel",
                false,
            )
            .expect("a settled result, damaged capture and all");

        let facts = cancellation_facts(&output);
        assert_eq!(facts["uncertain"], true);
        assert_eq!(facts["authoritative"], false);
        assert_eq!(facts["exitCode"], 0);
        assert!(
            output.output.contains("capturing command output failed"),
            "the reason the code cannot be cited belongs in the result: {}",
            output.output
        );
        let receipt = VerificationReceipt::from_metadata(&output.metadata)
            .expect("decodable receipt")
            .expect("a cancelled result carries a receipt");
        assert!(
            !receipt.proves_success(),
            "an exit 0 the service refused to settle must not prove success"
        );
        assert_eq!(receipt.exit_code, None);
        assert_eq!(receipt.exit_authority, ExitAuthority::Absent);
    }

    /// A hard-ceiling kill during cancellation says which deadline ended the command.
    #[test]
    fn a_cancelled_command_killed_at_its_hard_ceiling_says_so() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let tool = assembly_tool(workspace.path());
        let verification = ShellVerification {
            command: "sleep 900".to_owned(),
            workdir: workspace.path().to_string_lossy().into_owned(),
            git_head: None,
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
        };
        let mut execution =
            settled_execution(workspace.path(), BackgroundExecutionStatus::Failed, None);
        execution.timed_out = true;

        let output = tool
            .cancelled_output(
                &verification,
                120_000,
                &execution,
                Vec::new(),
                "ses_cancel",
                false,
            )
            .expect("settled");

        assert_eq!(cancellation_facts(&output)["uncertain"], true);
        assert!(
            output.output.contains("hard ceiling"),
            "the deadline that ended the command is part of what it decided: {}",
            output.output
        );
    }

    /// Oversized cancelled output is withheld, exactly as a completed run's is.
    ///
    /// Cancellation is not a second truncation path: the bytes are persisted whole and
    /// the model is handed the notice that names the windowed read, with the
    /// cancellation statement still in front of it.
    #[test]
    fn oversized_cancelled_output_is_withheld_rather_than_truncated() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let store_dir = tempfile::tempdir().expect("store dir");
        let store = ToolOutputStore::new(store_dir.path());
        let tool = assembly_tool(workspace.path())
            .with_output_store(store.clone())
            .with_output_limits(OutputLimits {
                max_lines: 1,
                max_bytes: 4,
            });
        let verification = ShellVerification {
            command: "cargo test --workspace".to_owned(),
            workdir: workspace.path().to_string_lossy().into_owned(),
            git_head: None,
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
        };
        let execution =
            settled_execution(workspace.path(), BackgroundExecutionStatus::Cancelled, None);
        let captured = b"one\ntwo\n";

        let output = tool
            .cancelled_output(
                &verification,
                120_000,
                &execution,
                captured.to_vec(),
                "ses_cancel",
                false,
            )
            .expect("oversized output is an outcome, not a failure");

        assert_eq!(output.metadata["oversized"], true);
        assert!(
            output.output.contains("Tool output withheld"),
            "{}",
            output.output
        );
        assert!(
            output.output.starts_with("Cancelled by the user."),
            "withholding must not swallow the cancellation statement: {}",
            output.output
        );
        assert!(
            !output.output.contains("one\ntwo\n"),
            "withheld output is not inlined: {}",
            output.output
        );
        assert_eq!(cancellation_facts(&output)["uncertain"], true);

        let paths = output.output_paths();
        let path = paths.first().expect("the artifact holding every byte");
        let window = store
            .read_window("shell", "ses_cancel", Path::new(path), 0, 4_096)
            .expect("stored cancelled output");
        assert_eq!(
            window.bytes, captured,
            "cancellation persists the bytes byte for byte"
        );
    }

    /// A cancelled command that wrote nothing says so instead of returning empty text.
    #[test]
    fn a_cancelled_command_with_no_output_still_returns_a_readable_result() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let tool = assembly_tool(workspace.path());
        let verification = ShellVerification {
            command: "sleep 30".to_owned(),
            workdir: workspace.path().to_string_lossy().into_owned(),
            git_head: None,
            contract: contract(CommandShellKind::Posix, "bash", ExitPolicy::Pipefail),
        };
        let execution =
            settled_execution(workspace.path(), BackgroundExecutionStatus::Cancelled, None);

        let output = tool
            .cancelled_output(
                &verification,
                120_000,
                &execution,
                Vec::new(),
                "ses_cancel",
                false,
            )
            .expect("settled");

        assert!(output.output.ends_with("(no output)"), "{}", output.output);
        assert_eq!(cancellation_facts(&output)["uncertain"], true);
    }

    /// The metadata key is a cross-crate contract, so its spelling is pinned here.
    ///
    /// `zuno-engine` reads `uncertain` out of this object to decide whether a
    /// cooperative interruption needs authoritative inspection, and it cannot import
    /// the constant: dispatch is the layer this crate's tools are handed to. Renaming
    /// the key silently would leave the dispatcher reading a key nothing writes.
    #[test]
    fn the_cancellation_metadata_key_is_the_one_the_dispatcher_reads() {
        assert_eq!(METADATA_CANCELLATION_KEY, "cancellation");
        let facts = CancelledExecution::killed(
            BackgroundExecutionStatus::Cancelled,
            None,
            "killed".to_owned(),
        );
        let value = facts.to_metadata_value();
        assert_eq!(value["uncertain"], true);
        assert_eq!(value["cancelled"], true);
        assert_eq!(value["authoritative"], false);
        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["detail"], "killed");
    }
}
