use super::DEFAULT_DISPOSE_GRACE;
use super::OneShotAgentBackend;
use super::OneShotAgentBackendKind;
use super::OneShotAgentError;
use super::OneShotAgentFailureCategory;
use super::OneShotAgentFailureStage;
use super::OneShotAgentFuture;
use super::OneShotAgentRequest;
use super::OneShotAgentResult;
use super::OneShotProcessSandboxConfig;
use super::prepare_external_process_command;
use super::valid_sha256;
use super::validate_request;
use super::verify_executable_sha256;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::future::pending;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const DEFAULT_OUTPUT_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// Non-interactive Claude Code permission policy fixed by a provider instance.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaudeCodePermissionMode {
    #[default]
    DontAsk,
    AcceptEdits,
    Auto,
    Plan,
    BypassPermissions,
}

impl ClaudeCodePermissionMode {
    fn as_cli_value(self) -> &'static str {
        match self {
            Self::DontAsk => "dontAsk",
            Self::AcceptEdits => "acceptEdits",
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

/// Deployment-owned configuration for one Claude Code provider instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeCodeBackendConfig {
    pub executable: PathBuf,
    /// Optional immutable executable digest checked again immediately before spawn.
    pub executable_sha256: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub permission_mode: ClaudeCodePermissionMode,
    pub env: BTreeMap<String, String>,
    pub sandbox: Option<OneShotProcessSandboxConfig>,
    pub output_limit_bytes: usize,
    /// Optional total process run deadline. `None` delegates the deadline to the
    /// owning workflow or Agent lifecycle.
    pub run_timeout: Option<Duration>,
    pub dispose_grace: Duration,
}

impl Default for ClaudeCodeBackendConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("claude"),
            executable_sha256: None,
            model: None,
            reasoning_effort: None,
            permission_mode: ClaudeCodePermissionMode::DontAsk,
            env: BTreeMap::new(),
            sandbox: None,
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT_BYTES,
            run_timeout: None,
            dispose_grace: DEFAULT_DISPOSE_GRACE,
        }
    }
}

/// One-shot Claude Code CLI provider.
///
/// Native Claude settings and on-disk authentication remain authoritative.
/// Optional model and effort values are provider-instance overrides. Ambient
/// credential-like environment variables are removed unless explicitly supplied
/// in `ClaudeCodeBackendConfig::env`.
#[derive(Clone, Debug)]
pub struct ClaudeCodeBackend {
    config: ClaudeCodeBackendConfig,
}

impl ClaudeCodeBackend {
    pub fn new(config: ClaudeCodeBackendConfig) -> Result<Self, OneShotAgentError> {
        let invalid = config.executable.as_os_str().is_empty()
            || config
                .executable_sha256
                .as_deref()
                .is_some_and(|digest| !valid_sha256(digest))
            || config.output_limit_bytes == 0
            || config.run_timeout.is_some_and(|timeout| timeout.is_zero())
            || config.dispose_grace.is_zero()
            || config
                .model
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            || config
                .reasoning_effort
                .as_deref()
                .is_some_and(|value| value.trim().is_empty());
        if invalid {
            return Err(OneShotAgentError::new(
                OneShotAgentBackendKind::ClaudeCode,
                OneShotAgentFailureStage::Validate,
                OneShotAgentFailureCategory::InvalidRequest,
            ));
        }
        Ok(Self { config })
    }

    fn command_args(&self) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("--print"),
            OsString::from("--output-format"),
            OsString::from("json"),
            OsString::from("--no-session-persistence"),
            OsString::from("--permission-mode"),
            OsString::from(self.config.permission_mode.as_cli_value()),
            OsString::from("--disallowedTools"),
            OsString::from(
                if self.config.permission_mode == ClaudeCodePermissionMode::Plan {
                    "AskUserQuestion,ExitPlanMode"
                } else {
                    "AskUserQuestion"
                },
            ),
        ];
        if self.config.permission_mode == ClaudeCodePermissionMode::BypassPermissions {
            args.push(OsString::from("--dangerously-skip-permissions"));
        }
        if let Some(model) = &self.config.model {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model));
        }
        if let Some(effort) = &self.config.reasoning_effort {
            args.push(OsString::from("--effort"));
            args.push(OsString::from(effort));
        }
        args
    }
}

impl OneShotAgentBackend for ClaudeCodeBackend {
    fn kind(&self) -> OneShotAgentBackendKind {
        OneShotAgentBackendKind::ClaudeCode
    }

    fn capabilities(&self) -> crate::AgentBackendCapabilities {
        crate::AgentBackendCapabilities::claude_code()
    }

    fn run<'a>(
        &'a self,
        request: OneShotAgentRequest,
        cancellation: CancellationToken,
    ) -> OneShotAgentFuture<'a> {
        Box::pin(async move {
            validate_request(self.kind(), &request)?;
            verify_executable_sha256(
                self.kind(),
                &self.config.executable,
                self.config.executable_sha256.as_deref(),
            )
            .await?;
            let mut command = prepare_external_process_command(
                self.kind(),
                &self.config.executable,
                self.command_args(),
                &self.config.env,
                &request.cwd,
                self.config.sandbox.as_ref(),
            )?;
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);

            let (mut child, process_tree) = spawn_owned_process(&mut command).map_err(|_| {
                OneShotAgentError::new(
                    self.kind(),
                    OneShotAgentFailureStage::Start,
                    OneShotAgentFailureCategory::Process,
                )
            })?;
            let run_deadline = self.config.run_timeout.map(|limit| Instant::now() + limit);
            let mut stdin = child.stdin.take().ok_or_else(|| {
                OneShotAgentError::new(
                    self.kind(),
                    OneShotAgentFailureStage::Start,
                    OneShotAgentFailureCategory::Process,
                )
            })?;
            let stdout = child.stdout.take().ok_or_else(|| {
                OneShotAgentError::new(
                    self.kind(),
                    OneShotAgentFailureStage::Start,
                    OneShotAgentFailureCategory::Process,
                )
            })?;
            let stderr = child.stderr.take().ok_or_else(|| {
                OneShotAgentError::new(
                    self.kind(),
                    OneShotAgentFailureStage::Start,
                    OneShotAgentFailureCategory::Process,
                )
            })?;
            let output_exceeded = CancellationToken::new();
            let stdout_task = spawn_bounded_reader(
                stdout,
                self.config.output_limit_bytes,
                output_exceeded.clone(),
            );
            let stderr_task = spawn_bounded_reader(
                stderr,
                self.config.output_limit_bytes,
                output_exceeded.clone(),
            );

            let write_result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(OneShotAgentFailureCategory::Aborted),
                _ = wait_for_deadline(run_deadline) => Err(OneShotAgentFailureCategory::Limit),
                result = stdin.write_all(request.prompt.as_bytes()) => {
                    result.map_err(|_| OneShotAgentFailureCategory::Process)
                },
            };
            drop(stdin);
            if let Err(category) = write_result {
                terminate_process(&mut child, &process_tree, self.config.dispose_grace).await;
                return Err(OneShotAgentError::new(
                    self.kind(),
                    OneShotAgentFailureStage::Run,
                    category,
                ));
            }

            let status = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    terminate_process(&mut child, &process_tree, self.config.dispose_grace).await;
                    return Err(OneShotAgentError::new(
                        self.kind(),
                        OneShotAgentFailureStage::Run,
                        OneShotAgentFailureCategory::Aborted,
                    ));
                }
                _ = output_exceeded.cancelled() => {
                    terminate_process(&mut child, &process_tree, self.config.dispose_grace).await;
                    return Err(OneShotAgentError::new(
                        self.kind(),
                        OneShotAgentFailureStage::Run,
                        OneShotAgentFailureCategory::Limit,
                    ));
                }
                _ = wait_for_deadline(run_deadline) => {
                    terminate_process(&mut child, &process_tree, self.config.dispose_grace).await;
                    return Err(OneShotAgentError::new(
                        self.kind(),
                        OneShotAgentFailureStage::Run,
                        OneShotAgentFailureCategory::Limit,
                    ));
                }
                status = child.wait() => status.map_err(|_| {
                    OneShotAgentError::new(
                        self.kind(),
                        OneShotAgentFailureStage::Run,
                        OneShotAgentFailureCategory::Process,
                    )
                })?,
            };
            process_tree.terminate();
            let stdout =
                collect_reader(stdout_task, self.config.dispose_grace, self.kind()).await?;
            let _ = collect_reader(stderr_task, self.config.dispose_grace, self.kind()).await;
            process_tree.kill();

            if !status.success() {
                return Err(OneShotAgentError::new(
                    self.kind(),
                    OneShotAgentFailureStage::Run,
                    OneShotAgentFailureCategory::Process,
                )
                .with_status(status));
            }
            decode_claude_result(&stdout)
        })
    }
}

pub(super) async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending::<()>().await,
    }
}

#[derive(Debug, Deserialize)]
struct ClaudeResult {
    #[serde(rename = "type")]
    message_type: Option<String>,
    subtype: Option<String>,
    #[serde(default)]
    is_error: bool,
    result: Option<String>,
    session_id: Option<String>,
}

fn decode_claude_result(bytes: &[u8]) -> Result<OneShotAgentResult, OneShotAgentError> {
    let message: ClaudeResult = serde_json::from_slice(bytes).map_err(|_| {
        OneShotAgentError::new(
            OneShotAgentBackendKind::ClaudeCode,
            OneShotAgentFailureStage::Decode,
            OneShotAgentFailureCategory::InvalidResult,
        )
    })?;
    let valid_envelope = message.message_type.as_deref() == Some("result")
        && message.subtype.as_deref() == Some("success")
        && !message.is_error;
    let Some(final_answer) = message.result.filter(|result| !result.trim().is_empty()) else {
        return Err(OneShotAgentError::new(
            OneShotAgentBackendKind::ClaudeCode,
            OneShotAgentFailureStage::Decode,
            OneShotAgentFailureCategory::InvalidResult,
        ));
    };
    if !valid_envelope {
        return Err(OneShotAgentError::new(
            OneShotAgentBackendKind::ClaudeCode,
            OneShotAgentFailureStage::Decode,
            OneShotAgentFailureCategory::InvalidResult,
        ));
    }
    Ok(OneShotAgentResult {
        backend: OneShotAgentBackendKind::ClaudeCode,
        final_answer,
        product_session_id: message.session_id,
    })
}

fn spawn_bounded_reader<R>(
    mut reader: R,
    limit: usize,
    output_exceeded: CancellationToken,
) -> JoinHandle<std::io::Result<Vec<u8>>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                return Ok(output);
            }
            if output.len().saturating_add(read) > limit {
                output_exceeded.cancel();
                return Err(std::io::Error::other(
                    "product subagent output limit exceeded",
                ));
            }
            output.extend_from_slice(&buffer[..read]);
        }
    })
}

async fn collect_reader(
    task: JoinHandle<std::io::Result<Vec<u8>>>,
    grace: Duration,
    backend: OneShotAgentBackendKind,
) -> Result<Vec<u8>, OneShotAgentError> {
    timeout(grace, task)
        .await
        .map_err(|_| {
            OneShotAgentError::new(
                backend,
                OneShotAgentFailureStage::Teardown,
                OneShotAgentFailureCategory::Process,
            )
        })?
        .map_err(|_| {
            OneShotAgentError::new(
                backend,
                OneShotAgentFailureStage::Teardown,
                OneShotAgentFailureCategory::Process,
            )
        })?
        .map_err(|_| {
            OneShotAgentError::new(
                backend,
                OneShotAgentFailureStage::Run,
                OneShotAgentFailureCategory::Limit,
            )
        })
}

#[cfg(unix)]
pub(super) struct ProcessTree {
    process_group_id: u32,
}

#[cfg(windows)]
pub(super) struct ProcessTree {
    job: codex_utils_pty::JobObject,
}

#[cfg(not(any(unix, windows)))]
pub(super) struct ProcessTree;

impl ProcessTree {
    fn terminate(&self) {
        // On macOS the pty helper retries denied group signals against the
        // group's members itself, so one call serves every Unix target.
        #[cfg(unix)]
        let _ = codex_utils_pty::process_group::terminate_process_group(self.process_group_id);
        #[cfg(windows)]
        let _ = self.job.terminate();
    }

    fn kill(&self) {
        #[cfg(unix)]
        let _ = codex_utils_pty::process_group::kill_process_group(self.process_group_id);
        #[cfg(windows)]
        let _ = self.job.terminate();
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.kill();
    }
}

pub(super) fn spawn_owned_process(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    #[cfg(windows)]
    {
        // A workflow owns the whole external Agent process tree. Starting the
        // child without a kill-on-close Job Object would make cancellation and
        // timeout semantics untrue, so allocation or assignment failure is a
        // hard launch failure rather than a native fallback.
        let job = codex_utils_pty::JobObject::create_without_breakaway()?;
        let child = job.spawn_contained(command)?;
        return Ok((child, ProcessTree { job }));
    }
    #[cfg(not(windows))]
    {
        let child = command.spawn()?;
        #[cfg(unix)]
        let process_tree = ProcessTree {
            process_group_id: child
                .id()
                .ok_or_else(|| std::io::Error::other("missing product subagent process id"))?,
        };
        #[cfg(not(any(unix, windows)))]
        let process_tree = ProcessTree;
        Ok((child, process_tree))
    }
}

pub(super) async fn terminate_process(
    child: &mut Child,
    process_tree: &ProcessTree,
    grace: Duration,
) {
    process_tree.terminate();
    let _ = child.start_kill();
    if timeout(grace, child.wait()).await.is_err() {
        process_tree.kill();
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(test)]
#[path = "../one_shot_tests.rs"]
mod tests;
