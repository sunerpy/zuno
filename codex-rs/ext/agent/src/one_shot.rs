mod acp;
mod claude_code;
mod native_codex;

pub use acp::AcpBackend;
pub use acp::AcpBackendConfig;
pub use claude_code::ClaudeCodeBackend;
pub use claude_code::ClaudeCodeBackendConfig;
pub use claude_code::ClaudeCodePermissionMode;
pub use native_codex::NativeCodexBackend;

use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxDirectSpawnTransformRequest;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxTransformRequest;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub(crate) const DEFAULT_DISPOSE_GRACE: Duration = Duration::from_secs(3);
const MAX_PROMPT_BYTES: usize = 1024 * 1024;

/// Product runtime selected for one delegated agent instance.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OneShotAgentBackendKind {
    NativeCodex,
    ClaudeCode,
    Acp,
}

impl fmt::Display for OneShotAgentBackendKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NativeCodex => "native-codex",
            Self::ClaudeCode => "claude-code",
            Self::Acp => "acp",
        })
    }
}

/// Lifecycle boundary at which a delegated agent stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OneShotAgentFailureStage {
    Validate,
    Start,
    Run,
    Decode,
    Teardown,
}

impl fmt::Display for OneShotAgentFailureStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Validate => "validate",
            Self::Start => "start",
            Self::Run => "run",
            Self::Decode => "decode",
            Self::Teardown => "teardown",
        })
    }
}

/// Coarse failure category safe to expose to a delegating model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OneShotAgentFailureCategory {
    InvalidRequest,
    Aborted,
    AccessPolicy,
    Limit,
    Process,
    ProductError,
    InvalidResult,
    Unknown,
}

impl fmt::Display for OneShotAgentFailureCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest => "invalid-request",
            Self::Aborted => "aborted",
            Self::AccessPolicy => "access-policy",
            Self::Limit => "limit",
            Self::Process => "process",
            Self::ProductError => "product-error",
            Self::InvalidResult => "invalid-result",
            Self::Unknown => "unknown",
        })
    }
}

/// Typed, redacted failure from a product subagent.
///
/// Product stderr, protocol payloads, prompts, and credential-bearing paths are
/// deliberately omitted. Callers can log their own private launch context.
#[derive(Debug, Eq, PartialEq)]
pub struct OneShotAgentError {
    pub backend: OneShotAgentBackendKind,
    pub stage: OneShotAgentFailureStage,
    pub category: OneShotAgentFailureCategory,
    pub exit_code: Option<i32>,
}

impl OneShotAgentError {
    pub(crate) fn new(
        backend: OneShotAgentBackendKind,
        stage: OneShotAgentFailureStage,
        category: OneShotAgentFailureCategory,
    ) -> Self {
        Self {
            backend,
            stage,
            category,
            exit_code: None,
        }
    }

    pub(crate) fn with_status(mut self, status: ExitStatus) -> Self {
        self.exit_code = status.code();
        self
    }
}

impl fmt::Display for OneShotAgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "product subagent failure (backend: {}; stage: {}; category: {}",
            self.backend, self.stage, self.category
        )?;
        if let Some(exit_code) = self.exit_code {
            write!(formatter, "; exit code: {exit_code}")?;
        }
        formatter.write_str(")")
    }
}

impl std::error::Error for OneShotAgentError {}

/// Self-contained text task executed in the supplied parent workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OneShotAgentRequest {
    pub prompt: String,
    pub cwd: AbsolutePathBuf,
}

/// Strict final result returned to the delegating workflow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OneShotAgentResult {
    pub backend: OneShotAgentBackendKind,
    pub final_answer: String,
    pub product_session_id: Option<String>,
}

/// Host-enforced process boundary for one external Agent backend generation.
///
/// Product-level permission flags remain useful defense in depth, but they are
/// not accepted as proof that the child process itself honors the selected
/// execution profile. When this snapshot is present, Zuno independently wraps
/// the external process in the same platform sandbox selected for that profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OneShotProcessSandboxConfig {
    pub permission_profile: PermissionProfile,
    pub workspace_roots: Vec<AbsolutePathBuf>,
    pub codex_home: AbsolutePathBuf,
    pub codex_self_exe: Option<PathBuf>,
    pub codex_linux_sandbox_exe: Option<PathBuf>,
    /// A configured managed proxy requires session-owned proxy state that this
    /// one-shot process adapter does not yet carry. Reject rather than bypass it.
    pub managed_network_configured: bool,
    pub use_legacy_landlock: bool,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
}

pub type OneShotAgentFuture<'a> =
    Pin<Box<dyn Future<Output = Result<OneShotAgentResult, OneShotAgentError>> + Send + 'a>>;

/// Pluggable one-shot product-agent provider.
///
/// Implementations must publish only one strict final answer, honor cancellation,
/// and own every process or thread created for the run through terminal cleanup.
pub trait OneShotAgentBackend: Send + Sync {
    fn kind(&self) -> OneShotAgentBackendKind;

    /// Immutable capabilities for this configured backend instance.
    fn capabilities(&self) -> crate::AgentBackendCapabilities {
        crate::AgentBackendCapabilities::one_shot_only()
    }

    fn run<'a>(
        &'a self,
        request: OneShotAgentRequest,
        cancellation: CancellationToken,
    ) -> OneShotAgentFuture<'a>;
}

pub(crate) fn validate_request(
    backend: OneShotAgentBackendKind,
    request: &OneShotAgentRequest,
) -> Result<(), OneShotAgentError> {
    if request.prompt.trim().is_empty() || request.prompt.len() > MAX_PROMPT_BYTES {
        return Err(OneShotAgentError::new(
            backend,
            OneShotAgentFailureStage::Validate,
            OneShotAgentFailureCategory::InvalidRequest,
        ));
    }
    Ok(())
}

pub(crate) fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) async fn verify_executable_sha256(
    backend: OneShotAgentBackendKind,
    path: &Path,
    expected: Option<&str>,
) -> Result<(), OneShotAgentError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let path = path.to_path_buf();
    let actual = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "backend executable is not an immutable regular file",
            ));
        }
        let mut file = File::open(path)?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Ok(digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    })
    .await
    .map_err(|_| {
        OneShotAgentError::new(
            backend,
            OneShotAgentFailureStage::Validate,
            OneShotAgentFailureCategory::Process,
        )
    })?
    .map_err(|_| {
        OneShotAgentError::new(
            backend,
            OneShotAgentFailureStage::Validate,
            OneShotAgentFailureCategory::Process,
        )
    })?;
    if actual != expected {
        return Err(OneShotAgentError::new(
            backend,
            OneShotAgentFailureStage::Validate,
            OneShotAgentFailureCategory::InvalidRequest,
        ));
    }
    Ok(())
}

pub(crate) fn prepare_external_process_command(
    backend: OneShotAgentBackendKind,
    executable: &Path,
    args: Vec<OsString>,
    explicit_env: &BTreeMap<String, String>,
    cwd: &AbsolutePathBuf,
    sandbox: Option<&OneShotProcessSandboxConfig>,
) -> Result<Command, OneShotAgentError> {
    let environment = scrubbed_child_environment(explicit_env);
    let Some(sandbox) = sandbox else {
        return Ok(native_process_command(executable, args, environment, cwd));
    };
    if sandbox.managed_network_configured {
        return Err(process_sandbox_error(backend));
    }

    let permission_profile = sandbox
        .permission_profile
        .clone()
        .materialize_project_roots_with_workspace_roots(&sandbox.workspace_roots);
    let manager = SandboxManager::new();
    let sandbox_type = manager.select_initial(
        &permission_profile,
        SandboxablePreference::Auto,
        sandbox.windows_sandbox_level,
        /*has_managed_network_requirements*/ false,
    );
    if manager.should_sandbox(
        &permission_profile,
        SandboxablePreference::Auto,
        /*has_managed_network_requirements*/ false,
    ) && sandbox_type == SandboxType::None
    {
        return Err(process_sandbox_error(backend));
    }
    if sandbox_type == SandboxType::None {
        return Ok(native_process_command(executable, args, environment, cwd));
    }

    let args = args
        .into_iter()
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| process_sandbox_error(backend))?;
    let environment = environment
        .into_iter()
        .map(|(name, value)| {
            let name = name
                .into_string()
                .map_err(|_| process_sandbox_error(backend))?;
            let value = value
                .into_string()
                .map_err(|_| process_sandbox_error(backend))?;
            Ok((name, value))
        })
        .collect::<Result<HashMap<_, _>, OneShotAgentError>>()?;
    #[cfg(windows)]
    if sandbox_type == SandboxType::WindowsRestrictedToken {
        return prepare_windows_sandbox_command(
            backend,
            executable,
            args,
            environment,
            cwd,
            sandbox,
            &permission_profile,
        );
    }
    let cwd_uri = PathUri::from_abs_path(cwd);
    let request = manager
        .transform_for_direct_spawn(SandboxDirectSpawnTransformRequest {
            workspace_roots: &sandbox.workspace_roots,
            windows_sandbox_proxy_settings_mode:
                codex_sandboxing::WindowsSandboxProxySettingsMode::Preserve,
            transform: SandboxTransformRequest {
                command: SandboxCommand {
                    program: executable.as_os_str().to_owned(),
                    args,
                    cwd: cwd_uri.clone(),
                    env: environment,
                    managed_network: None,
                    additional_permissions: None,
                },
                permissions: &permission_profile,
                sandbox: sandbox_type,
                enforce_managed_network: false,
                environment_id: None,
                network: None,
                sandbox_policy_cwd: &cwd_uri,
                codex_linux_sandbox_exe: sandbox.codex_linux_sandbox_exe.as_deref(),
                use_legacy_landlock: sandbox.use_legacy_landlock,
                windows_sandbox_level: sandbox.windows_sandbox_level,
                windows_sandbox_private_desktop: sandbox.windows_sandbox_private_desktop,
            },
        })
        .map_err(|_| process_sandbox_error(backend))?;
    let (program, args) = request
        .command
        .split_first()
        .ok_or_else(|| process_sandbox_error(backend))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd.as_path())
        .env_clear()
        .envs(request.env);
    #[cfg(unix)]
    if let Some(arg0) = request.arg0 {
        command.arg0(arg0);
    }
    Ok(command)
}

#[cfg(windows)]
fn prepare_windows_sandbox_command(
    backend: OneShotAgentBackendKind,
    executable: &Path,
    args: Vec<String>,
    environment: HashMap<String, String>,
    cwd: &AbsolutePathBuf,
    sandbox: &OneShotProcessSandboxConfig,
    permission_profile: &PermissionProfile,
) -> Result<Command, OneShotAgentError> {
    let wrapper = sandbox
        .codex_self_exe
        .as_deref()
        .ok_or_else(|| process_sandbox_error(backend))?;
    let executable = executable
        .to_str()
        .ok_or_else(|| process_sandbox_error(backend))?;
    let mut inner_command = Vec::with_capacity(1 + args.len());
    inner_command.push(executable.to_owned());
    inner_command.extend(args);
    let use_elevated =
        codex_sandboxing::windows_sandbox_uses_elevated_backend(sandbox.windows_sandbox_level);
    let overrides = if use_elevated {
        codex_sandboxing::resolve_windows_elevated_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            permission_profile,
            cwd,
            use_elevated,
        )
    } else {
        codex_sandboxing::resolve_windows_restricted_token_filesystem_overrides(
            SandboxType::WindowsRestrictedToken,
            permission_profile,
            cwd,
            sandbox.windows_sandbox_level,
        )
    }
    .map_err(|_| process_sandbox_error(backend))?;
    let empty_paths: &[AbsolutePathBuf] = &[];
    let args = codex_windows_sandbox::create_windows_sandbox_command_args_for_permission_profile(
        inner_command,
        cwd,
        &sandbox.workspace_roots,
        &environment,
        permission_profile,
        sandbox.windows_sandbox_level,
        sandbox.windows_sandbox_private_desktop,
        /*proxy_enforced*/ false,
        /*network_proxy_restricting_sid*/ None,
        codex_windows_sandbox::WindowsSandboxProxySettingsMode::Preserve,
        overrides
            .as_ref()
            .and_then(|value| value.read_roots_override.as_deref()),
        overrides
            .as_ref()
            .is_some_and(|value| value.read_roots_include_platform_defaults),
        overrides
            .as_ref()
            .and_then(|value| value.write_roots_override.as_deref()),
        overrides.as_ref().map_or(empty_paths, |value| {
            value.additional_deny_read_paths.as_slice()
        }),
        overrides.as_ref().map_or(empty_paths, |value| {
            value.additional_deny_write_paths.as_slice()
        }),
        sandbox.codex_home.as_path(),
    );
    let mut command = Command::new(wrapper);
    command
        .args(args)
        .current_dir(cwd.as_path())
        .env_clear()
        .envs(environment);
    Ok(command)
}

fn native_process_command(
    executable: &Path,
    args: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    cwd: &AbsolutePathBuf,
) -> Command {
    let mut command = Command::new(executable);
    command
        .args(args)
        .current_dir(cwd.as_path())
        .env_clear()
        .envs(environment);
    command
}

fn process_sandbox_error(backend: OneShotAgentBackendKind) -> OneShotAgentError {
    OneShotAgentError::new(
        backend,
        OneShotAgentFailureStage::Validate,
        OneShotAgentFailureCategory::AccessPolicy,
    )
}

pub(crate) fn scrubbed_child_environment(
    explicit: &BTreeMap<String, String>,
) -> BTreeMap<OsString, OsString> {
    let mut env = std::env::vars_os()
        .filter(|(name, _)| !is_credential_env_name(&name.to_string_lossy()))
        .collect::<BTreeMap<_, _>>();
    for (name, value) in explicit {
        env.insert(name.into(), value.into());
    }
    env
}

fn is_credential_env_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name == "AWS_ACCESS_KEY_ID"
        || name == "AWS_SECRET_ACCESS_KEY"
        || name == "AWS_SESSION_TOKEN"
        || name.ends_with("_API_KEY")
        || name.ends_with("_TOKEN")
        || name.ends_with("_SECRET")
        || name.ends_with("_PASSWORD")
        || name.ends_with("_CREDENTIAL")
        || name.ends_with("_CREDENTIALS")
}
