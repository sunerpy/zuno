//! Native OS sandbox preparation.
//!
//! The process layer accepts only [`PreparedCommand`]. Platform backends own the
//! conversion from a user command and immutable authority into that opaque launch
//! value, so a confinement-required command cannot accidentally reach spawn as raw
//! argv.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{LinuxBubblewrapSandbox, probe_spawn_count};

/// Hidden argv marker used by the in-sandbox seccomp helper.
pub const HELPER_MARKER: &str = "--zuno-sandbox-helper";

/// Shell confinement selected for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// The host root and workspace are readable but no host path is writable.
    ReadOnly,
    /// The workspace and explicitly approved roots are writable.
    WorkspaceWrite,
    /// The command runs directly with the Zuno process user's host authority.
    DangerFullAccess,
}

impl SandboxMode {
    /// Stable configuration and durable-event spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

/// Network authority available to one shell call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAccess {
    /// A private network namespace plus seccomp deny network operations.
    Denied,
    /// Inherit the host network namespace.
    Allowed,
}

impl NetworkAccess {
    /// Stable configuration and metadata spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Allowed => "allowed",
        }
    }
}

/// Trusted policy for a restricted sandbox that cannot be deployed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxUnavailableAction {
    /// Refuse execution when the requested OS isolation cannot be established.
    #[default]
    Deny,
    /// Run with the Zuno process user's host authority when the failure is eligible.
    RunUnconfined,
}

impl SandboxUnavailableAction {
    /// Stable configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::RunUnconfined => "run-unconfined",
        }
    }
}

/// How the requested sandbox policy reached its execution backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxResolutionKind {
    /// A restricted backend proved and enforces the requested authority.
    Confined,
    /// The caller explicitly requested native host execution.
    ExplicitNative,
    /// A trusted policy allowed an unavailable restricted sandbox to run natively.
    UnavailableFallback,
    /// A pre-v3 durable authority record without explicit resolution metadata.
    #[default]
    Legacy,
}

impl SandboxResolutionKind {
    /// Stable metadata spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confined => "confined",
            Self::ExplicitNative => "explicit_native",
            Self::UnavailableFallback => "unavailable_fallback",
            Self::Legacy => "legacy",
        }
    }
}

/// Stable, typed reason why a restricted OS sandbox could not be deployed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum SandboxUnavailableCause {
    #[error("OS sandbox is not implemented for platform `{platform}`")]
    UnsupportedPlatform { platform: String },
    #[error("Linux sandbox does not support architecture `{architecture}`")]
    UnsupportedArchitecture { architecture: String },
    #[error("Linux sandbox is unavailable on WSL1")]
    Wsl1Unsupported,
    #[error("no trusted system bubblewrap executable was found")]
    BubblewrapNotFound,
    #[error("bubblewrap does not provide required option `{option}`")]
    MissingBubblewrapCapability { option: String },
    #[error("sandbox deployment capability `{capability}` is unavailable: {detail}")]
    DeploymentCapabilityUnavailable { capability: String, detail: String },
}

impl SandboxUnavailableCause {
    /// Stable machine-readable reason code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedPlatform { .. } => "unsupported_platform",
            Self::UnsupportedArchitecture { .. } => "unsupported_architecture",
            Self::Wsl1Unsupported => "wsl1_unsupported",
            Self::BubblewrapNotFound => "bubblewrap_not_found",
            Self::MissingBubblewrapCapability { .. } => "missing_bubblewrap_capability",
            Self::DeploymentCapabilityUnavailable { .. } => "deployment_capability_unavailable",
        }
    }

    fn from_error(error: &SandboxError) -> Option<Self> {
        match error {
            SandboxError::UnsupportedPlatform(platform) => Some(Self::UnsupportedPlatform {
                platform: platform.clone(),
            }),
            SandboxError::UnsupportedArchitecture(architecture) => {
                Some(Self::UnsupportedArchitecture {
                    architecture: architecture.clone(),
                })
            }
            SandboxError::Wsl1Unsupported => Some(Self::Wsl1Unsupported),
            SandboxError::BubblewrapNotFound => Some(Self::BubblewrapNotFound),
            SandboxError::MissingBubblewrapCapability(option) => {
                Some(Self::MissingBubblewrapCapability {
                    option: option.clone(),
                })
            }
            SandboxError::UnavailableCapability { capability, detail } => {
                Some(Self::DeploymentCapabilityUnavailable {
                    capability: (*capability).to_owned(),
                    detail: detail.clone(),
                })
            }
            SandboxError::UntrustedBubblewrap { .. }
            | SandboxError::ProbeFailed { .. }
            | SandboxError::UnsupportedPolicy { .. }
            | SandboxError::InvalidPolicy(_)
            | SandboxError::InvalidPath { .. }
            | SandboxError::Seccomp(_)
            | SandboxError::Helper(_)
            | SandboxError::Io(_) => None,
        }
    }
}

/// Immutable policy selected before a shell command is prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    workspace: PathBuf,
    mode: SandboxMode,
    network: NetworkAccess,
    requested_mode: SandboxMode,
    requested_network: NetworkAccess,
    resolution_kind: SandboxResolutionKind,
    fallback_reason: Option<SandboxUnavailableCause>,
    writable_roots: Vec<PathBuf>,
    protected_paths: Vec<PathBuf>,
    git_metadata_writable: bool,
    approval_mode: String,
    reviewer_policy_sha256: String,
}

impl SandboxPolicy {
    /// Creates a policy rooted at an existing workspace.
    pub fn new(
        workspace: impl AsRef<Path>,
        mode: SandboxMode,
        network: NetworkAccess,
    ) -> Result<Self, SandboxError> {
        if mode == SandboxMode::DangerFullAccess && network != NetworkAccess::Allowed {
            return Err(SandboxError::InvalidPolicy(
                "danger-full-access inherits the host network and requires NetworkAccess::Allowed"
                    .to_owned(),
            ));
        }
        let workspace = canonical_directory(workspace.as_ref(), "workspace")?;
        Ok(Self {
            workspace,
            mode,
            network,
            requested_mode: mode,
            requested_network: network,
            resolution_kind: if mode == SandboxMode::DangerFullAccess {
                SandboxResolutionKind::ExplicitNative
            } else {
                SandboxResolutionKind::Confined
            },
            fallback_reason: None,
            writable_roots: Vec::new(),
            protected_paths: Vec::new(),
            git_metadata_writable: false,
            approval_mode: "standard".to_owned(),
            reviewer_policy_sha256: String::new(),
        })
    }

    /// Adds roots explicitly granted for this invocation.
    pub fn with_writable_roots(
        mut self,
        roots: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, SandboxError> {
        let roots = roots.into_iter().collect::<Vec<_>>();
        if !roots.is_empty() && self.mode != SandboxMode::WorkspaceWrite {
            return Err(SandboxError::InvalidPolicy(format!(
                "{} mode cannot grant writable roots",
                self.mode.as_str()
            )));
        }
        self.writable_roots.extend(
            roots
                .into_iter()
                .map(|path| canonical_directory(&path, "writable root"))
                .collect::<Result<Vec<_>, _>>()?,
        );
        self.writable_roots.sort();
        self.writable_roots.dedup();
        Ok(self)
    }

    /// Adds existing descendants that remain read-only even beneath a writable root.
    pub fn with_protected_paths(
        mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, SandboxError> {
        let paths = paths.into_iter().collect::<Vec<_>>();
        if !paths.is_empty() && self.mode == SandboxMode::DangerFullAccess {
            return Err(SandboxError::InvalidPolicy(
                "danger-full-access cannot enforce protected paths".to_owned(),
            ));
        }
        self.protected_paths.extend(
            paths
                .into_iter()
                .map(|path| canonical_protected_path(&path))
                .collect::<Result<Vec<_>, _>>()?,
        );
        self.protected_paths.sort();
        self.protected_paths.dedup();
        Ok(self)
    }

    /// Allows this invocation to update the resolved Git metadata directories.
    #[must_use]
    pub fn with_git_metadata_writable(mut self, writable: bool) -> Self {
        self.git_metadata_writable = writable;
        self
    }

    /// Records the permission mode and exact reviewer-policy identity.
    #[must_use]
    pub fn with_approval_context(
        mut self,
        mode: impl Into<String>,
        reviewer_policy_sha256: impl Into<String>,
    ) -> Self {
        self.approval_mode = mode.into();
        self.reviewer_policy_sha256 = reviewer_policy_sha256.into();
        self
    }

    /// Canonical workspace root.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Effective sandbox mode.
    #[must_use]
    pub const fn mode(&self) -> SandboxMode {
        self.mode
    }

    /// Network mode.
    #[must_use]
    pub const fn network(&self) -> NetworkAccess {
        self.network
    }

    /// Authority originally requested before any unavailable-sandbox fallback.
    #[must_use]
    pub const fn requested_mode(&self) -> SandboxMode {
        self.requested_mode
    }

    /// Network authority originally requested before fallback.
    #[must_use]
    pub const fn requested_network(&self) -> NetworkAccess {
        self.requested_network
    }

    /// Resolution path that selected the effective backend.
    #[must_use]
    pub const fn resolution_kind(&self) -> SandboxResolutionKind {
        self.resolution_kind
    }

    /// Typed unavailable reason when native fallback was activated.
    #[must_use]
    pub const fn fallback_reason(&self) -> Option<&SandboxUnavailableCause> {
        self.fallback_reason.as_ref()
    }

    fn into_unavailable_fallback(self, cause: SandboxUnavailableCause) -> Self {
        Self {
            mode: SandboxMode::DangerFullAccess,
            network: NetworkAccess::Allowed,
            resolution_kind: SandboxResolutionKind::UnavailableFallback,
            fallback_reason: Some(cause),
            writable_roots: Vec::new(),
            protected_paths: Vec::new(),
            git_metadata_writable: false,
            ..self
        }
    }
}

/// Raw command plus the immutable policy a backend must compile.
#[derive(Debug)]
pub struct PrepareRequest {
    pub program: OsString,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub policy: SandboxPolicy,
}

/// Capabilities proved by one backend before it is advertised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxCapabilities {
    pub backend: String,
    pub executable: Option<PathBuf>,
    pub read_only: bool,
    pub workspace_write: bool,
    pub danger_full_access: bool,
    pub network_isolation: bool,
}

/// Host executable identity used to decide whether a sandbox launcher is trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxExecutableIdentity {
    pub path: PathBuf,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
    pub device: Option<u64>,
    pub inode: Option<u64>,
    pub root_owned: Option<bool>,
    pub group_or_world_writable: Option<bool>,
    pub special_permissions: Option<bool>,
}

/// Result of one staged sandbox deployment check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDeploymentCheck {
    pub name: String,
    pub status: SandboxDeploymentCheckStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl SandboxDeploymentCheck {
    fn passed(name: impl Into<String>, detail: impl Into<Option<String>>) -> Self {
        Self {
            name: name.into(),
            status: SandboxDeploymentCheckStatus::Passed,
            detail: detail.into(),
        }
    }

    fn failed(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: SandboxDeploymentCheckStatus::Failed,
            detail: Some(detail.into()),
        }
    }

    fn skipped(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: SandboxDeploymentCheckStatus::Skipped,
            detail: Some(detail.into()),
        }
    }
}

/// Stable status spelling for one deployment check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxDeploymentCheckStatus {
    Passed,
    Failed,
    Skipped,
}

/// Active deployment check for one requested sandbox policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDeploymentReport {
    pub platform: String,
    pub architecture: String,
    pub workspace: PathBuf,
    pub requested_mode: SandboxMode,
    pub requested_network: NetworkAccess,
    pub on_unavailable: SandboxUnavailableAction,
    pub effective_mode: Option<SandboxMode>,
    pub effective_network: Option<NetworkAccess>,
    pub resolution_kind: Option<SandboxResolutionKind>,
    pub fallback_eligible: bool,
    pub fallback_reason: Option<SandboxUnavailableCause>,
    pub ready: bool,
    pub native_execution_bypass: bool,
    pub capabilities: Option<SandboxCapabilities>,
    pub launcher: Option<SandboxExecutableIdentity>,
    pub checks: Vec<SandboxDeploymentCheck>,
    pub error: Option<String>,
}

impl SandboxCapabilities {
    /// Whether this backend can faithfully compile `policy`.
    pub fn supports(&self, policy: &SandboxPolicy) -> Result<(), SandboxError> {
        match policy.mode {
            SandboxMode::ReadOnly if !self.read_only => {
                return Err(SandboxError::UnsupportedPolicy {
                    backend: self.backend.clone(),
                    requirement: "read-only filesystem".to_owned(),
                });
            }
            SandboxMode::WorkspaceWrite if !self.workspace_write => {
                return Err(SandboxError::UnsupportedPolicy {
                    backend: self.backend.clone(),
                    requirement: "workspace-write filesystem".to_owned(),
                });
            }
            SandboxMode::DangerFullAccess if !self.danger_full_access => {
                return Err(SandboxError::UnsupportedPolicy {
                    backend: self.backend.clone(),
                    requirement: "danger-full-access native execution".to_owned(),
                });
            }
            SandboxMode::ReadOnly | SandboxMode::WorkspaceWrite | SandboxMode::DangerFullAccess => {
            }
        }
        if policy.network == NetworkAccess::Denied && !self.network_isolation {
            return Err(SandboxError::UnsupportedPolicy {
                backend: self.backend.clone(),
                requirement: "network isolation".to_owned(),
            });
        }
        Ok(())
    }
}

/// Durable authority attached to every prepared command and background record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionAuthority {
    pub schema_version: u32,
    pub backend: String,
    pub backend_executable: Option<PathBuf>,
    pub workspace: PathBuf,
    /// Effective execution mode at the process boundary.
    pub mode: SandboxMode,
    /// Effective network authority at the process boundary.
    pub network: NetworkAccess,
    /// Requested mode before unavailable-sandbox resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_mode: Option<SandboxMode>,
    /// Requested network authority before unavailable-sandbox resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_network: Option<NetworkAccess>,
    /// How the effective execution backend was selected.
    #[serde(default)]
    pub resolution_kind: SandboxResolutionKind,
    /// Typed reason for an unavailable-sandbox fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<SandboxUnavailableCause>,
    pub writable_roots: Vec<PathBuf>,
    pub protected_paths: Vec<PathBuf>,
    pub cwd: PathBuf,
    pub command_sha256: String,
    pub environment_keys: Vec<String>,
    pub approval_mode: String,
    pub reviewer_policy_sha256: String,
}

impl ExecutionAuthority {
    /// Requested mode, treating pre-v3 records as requested equals effective.
    #[must_use]
    pub const fn requested_mode(&self) -> SandboxMode {
        match self.requested_mode {
            Some(mode) => mode,
            None => self.mode,
        }
    }

    /// Requested network authority, treating pre-v3 records as requested equals effective.
    #[must_use]
    pub const fn requested_network(&self) -> NetworkAccess {
        match self.requested_network {
            Some(network) => network,
            None => self.network,
        }
    }
}

/// Command accepted by the process boundary.
#[derive(Debug)]
pub struct PreparedCommand {
    program: OsString,
    arguments: Vec<OsString>,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    authority: ExecutionAuthority,
}

impl PreparedCommand {
    /// Constructs the opaque process-boundary value.
    ///
    /// This is public for backend implementations and test doubles. Product code
    /// should obtain values through [`SandboxBackend::prepare`].
    #[must_use]
    pub fn from_backend(
        request: PrepareRequest,
        launch_program: OsString,
        launch_arguments: Vec<OsString>,
        backend: &SandboxCapabilities,
        writable_roots: Vec<PathBuf>,
        protected_paths: Vec<PathBuf>,
    ) -> Self {
        let command_sha256 = command_sha256(&request.program, &request.arguments);
        let mut environment_keys = request
            .environment
            .keys()
            .map(|key| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        environment_keys.sort();
        environment_keys.dedup();
        let authority = ExecutionAuthority {
            schema_version: 3,
            backend: backend.backend.clone(),
            backend_executable: backend.executable.clone(),
            workspace: request.policy.workspace.clone(),
            mode: request.policy.mode,
            network: request.policy.network,
            requested_mode: Some(request.policy.requested_mode),
            requested_network: Some(request.policy.requested_network),
            resolution_kind: request.policy.resolution_kind,
            fallback_reason: request.policy.fallback_reason.clone(),
            writable_roots,
            protected_paths,
            cwd: request.cwd.clone(),
            command_sha256,
            environment_keys,
            approval_mode: request.policy.approval_mode,
            reviewer_policy_sha256: request.policy.reviewer_policy_sha256,
        };
        Self {
            program: launch_program,
            arguments: launch_arguments,
            cwd: request.cwd,
            environment: request.environment,
            authority,
        }
    }

    /// Durable authority compiled into this launch.
    #[must_use]
    pub const fn authority(&self) -> &ExecutionAuthority {
        &self.authority
    }

    /// Consumes the prepared command at the process boundary.
    #[must_use]
    pub fn into_parts(self) -> PreparedCommandParts {
        PreparedCommandParts {
            program: self.program,
            arguments: self.arguments,
            cwd: self.cwd,
            environment: self.environment,
            authority: self.authority,
        }
    }
}

/// Owned launch parts revealed only when the process service is ready to spawn.
#[derive(Debug)]
pub struct PreparedCommandParts {
    pub program: OsString,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub authority: ExecutionAuthority,
}

/// Platform backend that turns policy plus raw argv into a prepared launch.
pub trait SandboxBackend: Send + Sync {
    /// Capabilities proved by the backend's startup probes.
    fn capabilities(&self) -> &SandboxCapabilities;

    /// Compiles and validates one command. There is no unrestricted fallback.
    fn prepare(&self, request: PrepareRequest) -> Result<PreparedCommand, SandboxError>;

    /// Proves that the requested policy reaches the backend's real execution boundary.
    ///
    /// Native full access has no confinement chain to exercise, so the default only
    /// verifies capability support. Confining backends override this and execute a
    /// bounded no-op through the same launcher and first-party helper used by commands.
    fn verify_deployment(&self, policy: &SandboxPolicy) -> Result<(), SandboxError> {
        self.capabilities().supports(policy)
    }
}

/// Complete, immutable selection of requested and effective execution authority.
#[derive(Clone)]
pub struct SandboxResolution {
    backend: Arc<dyn SandboxBackend>,
    requested_policy: SandboxPolicy,
    execution_policy: SandboxPolicy,
}

impl fmt::Debug for SandboxResolution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxResolution")
            .field("backend", &self.backend.capabilities().backend)
            .field("requested_policy", &self.requested_policy)
            .field("execution_policy", &self.execution_policy)
            .finish()
    }
}

impl SandboxResolution {
    fn new(
        backend: Arc<dyn SandboxBackend>,
        requested_policy: SandboxPolicy,
        execution_policy: SandboxPolicy,
    ) -> Self {
        Self {
            backend,
            requested_policy,
            execution_policy,
        }
    }

    /// Builds a verified resolution for an injected backend.
    ///
    /// Composition roots and tests use this when platform discovery is supplied by
    /// another component. The execution policy must already carry its resolution
    /// metadata and must describe the same workspace and original request.
    pub fn with_verified_backend(
        backend: Arc<dyn SandboxBackend>,
        requested_policy: SandboxPolicy,
        execution_policy: SandboxPolicy,
    ) -> Result<Self, SandboxError> {
        if requested_policy.workspace() != execution_policy.workspace()
            || requested_policy.mode() != execution_policy.requested_mode()
            || requested_policy.network() != execution_policy.requested_network()
        {
            return Err(SandboxError::InvalidPolicy(
                "sandbox resolution requested/effective authority is inconsistent".to_owned(),
            ));
        }
        backend.verify_deployment(&execution_policy)?;
        Ok(Self::new(backend, requested_policy, execution_policy))
    }

    /// Builds a verified native fallback for an injected unavailable cause.
    pub fn unavailable_fallback(
        requested_policy: SandboxPolicy,
        cause: SandboxUnavailableCause,
    ) -> Result<Self, SandboxError> {
        if requested_policy.mode() != SandboxMode::WorkspaceWrite {
            return Err(SandboxError::InvalidPolicy(
                "only workspace-write authority may fall back to native execution".to_owned(),
            ));
        }
        let execution_policy = requested_policy.clone().into_unavailable_fallback(cause);
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(DangerFullAccessSandbox::new(requested_policy.workspace())?);
        Self::with_verified_backend(backend, requested_policy, execution_policy)
    }

    /// Selected execution backend.
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn SandboxBackend> {
        &self.backend
    }

    /// Authority requested before unavailable-sandbox resolution.
    #[must_use]
    pub const fn requested_policy(&self) -> &SandboxPolicy {
        &self.requested_policy
    }

    /// Authority that will be compiled at the process boundary.
    #[must_use]
    pub const fn execution_policy(&self) -> &SandboxPolicy {
        &self.execution_policy
    }

    /// Resolution path that selected the backend.
    #[must_use]
    pub const fn kind(&self) -> SandboxResolutionKind {
        self.execution_policy.resolution_kind()
    }

    /// Typed unavailable reason when fallback was activated.
    #[must_use]
    pub const fn fallback_reason(&self) -> Option<&SandboxUnavailableCause> {
        self.execution_policy.fallback_reason()
    }

    /// Consumes the resolution for a tool that owns its backend and policy.
    #[must_use]
    pub fn into_execution(self) -> (Arc<dyn SandboxBackend>, SandboxPolicy) {
        (self.backend, self.execution_policy)
    }
}

/// Resolves one requested policy before a command can reach preparation.
pub trait SandboxResolver: Send + Sync {
    /// Selects and verifies a backend. Implementations may never switch during prepare/run.
    fn resolve(
        self: Arc<Self>,
        policy: SandboxPolicy,
        on_unavailable: SandboxUnavailableAction,
    ) -> Result<SandboxResolution, SandboxError>;
}

/// Production resolver for the current operating system.
#[derive(Debug, Default)]
pub struct SystemSandboxResolver;

impl SandboxResolver for SystemSandboxResolver {
    fn resolve(
        self: Arc<Self>,
        policy: SandboxPolicy,
        on_unavailable: SandboxUnavailableAction,
    ) -> Result<SandboxResolution, SandboxError> {
        let _ = self;
        resolve_policy_with(policy, on_unavailable, |policy| {
            system_backend(policy.workspace(), policy.mode()).map(Arc::<dyn SandboxBackend>::from)
        })
    }
}

fn resolve_policy_with(
    policy: SandboxPolicy,
    on_unavailable: SandboxUnavailableAction,
    discover: impl FnOnce(&SandboxPolicy) -> Result<Arc<dyn SandboxBackend>, SandboxError>,
) -> Result<SandboxResolution, SandboxError> {
    if policy.mode() == SandboxMode::DangerFullAccess {
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(DangerFullAccessSandbox::new(policy.workspace())?);
        return SandboxResolution::with_verified_backend(backend, policy.clone(), policy);
    }

    let resolution = discover(&policy).and_then(|backend| {
        SandboxResolution::with_verified_backend(backend, policy.clone(), policy.clone())
    });
    match resolution {
        Ok(resolution) => Ok(resolution),
        Err(error) => {
            let Some(cause) = SandboxUnavailableCause::from_error(&error) else {
                return Err(error);
            };
            if on_unavailable != SandboxUnavailableAction::RunUnconfined
                || policy.mode() != SandboxMode::WorkspaceWrite
            {
                return Err(error);
            }
            SandboxResolution::unavailable_fallback(policy, cause)
        }
    }
}

impl<T> SandboxResolver for T
where
    T: SandboxBackend + 'static,
{
    fn resolve(
        self: Arc<Self>,
        policy: SandboxPolicy,
        _on_unavailable: SandboxUnavailableAction,
    ) -> Result<SandboxResolution, SandboxError> {
        let backend: Arc<dyn SandboxBackend> = self;
        SandboxResolution::with_verified_backend(backend, policy.clone(), policy)
    }
}

/// Explicit unconfined backend used only for [`SandboxMode::DangerFullAccess`].
#[derive(Debug)]
pub struct DangerFullAccessSandbox {
    workspace: PathBuf,
    capabilities: SandboxCapabilities,
}

impl DangerFullAccessSandbox {
    /// Creates a native execution backend without probing a confinement service.
    pub fn new(workspace: &Path) -> Result<Self, SandboxError> {
        Ok(Self {
            workspace: canonical_directory(workspace, "workspace")?,
            capabilities: SandboxCapabilities {
                backend: "danger_full_access".to_owned(),
                executable: None,
                read_only: false,
                workspace_write: false,
                danger_full_access: true,
                network_isolation: false,
            },
        })
    }
}

impl SandboxBackend for DangerFullAccessSandbox {
    fn capabilities(&self) -> &SandboxCapabilities {
        &self.capabilities
    }

    fn prepare(&self, mut request: PrepareRequest) -> Result<PreparedCommand, SandboxError> {
        if request.policy.workspace() != self.workspace {
            return Err(SandboxError::InvalidPath {
                kind: "workspace",
                path: request.policy.workspace().to_owned(),
                reason: format!("backend was created for `{}`", self.workspace.display()),
            });
        }
        self.capabilities.supports(&request.policy)?;
        request.cwd = canonical_directory(&request.cwd, "working directory")?;
        let program = request.program.clone();
        let arguments = request.arguments.clone();
        Ok(PreparedCommand::from_backend(
            request,
            program,
            arguments,
            &self.capabilities,
            Vec::new(),
            Vec::new(),
        ))
    }
}

/// Selects the native backend for the effective policy.
///
/// Restricted modes never fall back to [`DangerFullAccessSandbox`]. Bubblewrap
/// discovery is served from a process-local cache keyed by the canonical workspace
/// and the helper executable (see [`LinuxBubblewrapSandbox::discover_cached`]): the
/// first call for a workspace probes the host, later calls reuse that backend after
/// re-checking the trusted files on disk, and failures are never cached. Use
/// [`probe_system_backend`] for a diagnostic that must describe the host as it is now.
pub fn system_backend(
    workspace: &Path,
    mode: SandboxMode,
) -> Result<Box<dyn SandboxBackend>, SandboxError> {
    if mode == SandboxMode::DangerFullAccess {
        return Ok(Box::new(DangerFullAccessSandbox::new(workspace)?));
    }

    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(SharedBackend(
            LinuxBubblewrapSandbox::discover_cached(workspace)?,
        )))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(SandboxError::UnsupportedPlatform(
            std::env::consts::OS.to_owned(),
        ))
    }
}

/// Discovers the native backend without consulting or updating the process cache.
///
/// Deployment reports use this so a diagnostic describes the host as it is now,
/// not as it was when the process first probed it.
pub fn probe_system_backend(
    workspace: &Path,
    mode: SandboxMode,
) -> Result<Box<dyn SandboxBackend>, SandboxError> {
    if mode == SandboxMode::DangerFullAccess {
        return Ok(Box::new(DangerFullAccessSandbox::new(workspace)?));
    }

    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(LinuxBubblewrapSandbox::discover(workspace)?))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(SandboxError::UnsupportedPlatform(
            std::env::consts::OS.to_owned(),
        ))
    }
}

/// A process-cached backend handed to one caller.
///
/// Every caller gets its own `Box`, as before, while the probes behind it are
/// shared; the delegation keeps [`system_backend`]'s signature stable for callers.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct SharedBackend(Arc<LinuxBubblewrapSandbox>);

#[cfg(target_os = "linux")]
impl SandboxBackend for SharedBackend {
    fn capabilities(&self) -> &SandboxCapabilities {
        self.0.capabilities()
    }

    fn prepare(&self, request: PrepareRequest) -> Result<PreparedCommand, SandboxError> {
        self.0.prepare(request)
    }

    fn verify_deployment(&self, policy: &SandboxPolicy) -> Result<(), SandboxError> {
        self.0.verify_deployment(policy)
    }
}

/// Probe the exact backend and policy a host deployment would use.
///
/// Restricted modes exercise launcher trust, required bubblewrap options,
/// namespaces, and seccomp compilation through [`system_backend`]. The
/// `danger-full-access` result is explicitly marked as a native-execution bypass;
/// it is not evidence that bubblewrap is deployable.
#[must_use]
pub fn deployment_report(
    workspace: &Path,
    mode: SandboxMode,
    network: NetworkAccess,
) -> SandboxDeploymentReport {
    deployment_report_with_action(workspace, mode, network, SandboxUnavailableAction::Deny)
}

/// Probe requested isolation while also reporting a trusted fallback decision.
///
/// `ready` remains strict: it is true only when the requested policy itself is
/// deployable. An eligible native fallback is represented by `effective_*` and
/// `resolution_kind` without turning `ready` into success.
#[must_use]
pub fn deployment_report_with_action(
    workspace: &Path,
    mode: SandboxMode,
    network: NetworkAccess,
    on_unavailable: SandboxUnavailableAction,
) -> SandboxDeploymentReport {
    let mut report = SandboxDeploymentReport {
        platform: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        workspace: workspace.to_owned(),
        requested_mode: mode,
        requested_network: network,
        on_unavailable,
        effective_mode: None,
        effective_network: None,
        resolution_kind: None,
        fallback_eligible: false,
        fallback_reason: None,
        ready: false,
        native_execution_bypass: false,
        capabilities: None,
        launcher: None,
        checks: Vec::new(),
        error: None,
    };
    let policy = match SandboxPolicy::new(workspace, mode, network) {
        Ok(policy) => {
            report.checks.push(SandboxDeploymentCheck::passed(
                "policy",
                Some("the requested mode and network authority form a valid policy".to_owned()),
            ));
            policy
        }
        Err(error) => {
            report
                .checks
                .push(SandboxDeploymentCheck::failed("policy", error.to_string()));
            record_deployment_failure(&mut report, &error);
            return report;
        }
    };
    report.workspace = policy.workspace().to_owned();

    #[cfg(target_os = "linux")]
    if mode != SandboxMode::DangerFullAccess {
        match linux::trusted_bubblewrap_path(policy.workspace()) {
            Ok(path) => {
                report.launcher = Some(executable_identity(&path));
                report.checks.push(SandboxDeploymentCheck::passed(
                    "launcher_trust",
                    Some(format!(
                        "`{}` is root-owned, immutable to non-root users, has no special bits or \
                         file capabilities, and every ancestor is trusted",
                        path.display()
                    )),
                ));
            }
            Err(error) => {
                if let SandboxError::UntrustedBubblewrap { path, .. } = &error {
                    report.launcher = Some(executable_identity(path));
                }
                report.checks.push(SandboxDeploymentCheck::failed(
                    "launcher_trust",
                    error.to_string(),
                ));
                record_deployment_failure(&mut report, &error);
                return report;
            }
        }
    }

    if mode == SandboxMode::DangerFullAccess {
        report.checks.push(SandboxDeploymentCheck::skipped(
            "launcher_trust",
            "danger-full-access intentionally bypasses OS confinement",
        ));
    }

    match probe_system_backend(policy.workspace(), mode) {
        Ok(backend) => {
            let capabilities = backend.capabilities();
            if report.launcher.is_none() {
                report.launcher = capabilities.executable.as_deref().map(executable_identity);
            }
            report.capabilities = Some(capabilities.clone());
            report.checks.push(SandboxDeploymentCheck::passed(
                "backend_discovery",
                Some(format!("selected `{}`", capabilities.backend)),
            ));
            if let Err(error) = backend.verify_deployment(&policy) {
                report.checks.push(match capabilities.supports(&policy) {
                    Ok(()) => SandboxDeploymentCheck::passed(
                        "policy_support",
                        Some("the backend advertises every requested capability".to_owned()),
                    ),
                    Err(support_error) => {
                        SandboxDeploymentCheck::failed("policy_support", support_error.to_string())
                    }
                });
                report.checks.push(SandboxDeploymentCheck::failed(
                    "execution_self_test",
                    error.to_string(),
                ));
                record_deployment_failure(&mut report, &error);
                return report;
            }
            report.checks.push(SandboxDeploymentCheck::passed(
                "policy_support",
                Some("the backend advertises every requested capability".to_owned()),
            ));
            report
                .checks
                .push(if mode == SandboxMode::DangerFullAccess {
                    SandboxDeploymentCheck::skipped(
                        "execution_self_test",
                        "native execution has no confinement helper to exercise",
                    )
                } else {
                    SandboxDeploymentCheck::passed(
                        "execution_self_test",
                        Some(
                            "a no-op completed through bubblewrap, capability drop, \
                         PR_SET_NO_NEW_PRIVS, and seccomp"
                                .to_owned(),
                        ),
                    )
                });
            report.ready = true;
            report.effective_mode = Some(mode);
            report.effective_network = Some(network);
            report.resolution_kind = Some(if mode == SandboxMode::DangerFullAccess {
                SandboxResolutionKind::ExplicitNative
            } else {
                SandboxResolutionKind::Confined
            });
            report.native_execution_bypass = mode == SandboxMode::DangerFullAccess;
        }
        Err(error) => {
            report.checks.push(SandboxDeploymentCheck::failed(
                "backend_discovery",
                error.to_string(),
            ));
            record_deployment_failure(&mut report, &error);
        }
    }
    report
}

fn record_deployment_failure(report: &mut SandboxDeploymentReport, error: &SandboxError) {
    report.error = Some(error.to_string());
    let Some(cause) = SandboxUnavailableCause::from_error(error) else {
        return;
    };
    report.fallback_reason = Some(cause);
    report.fallback_eligible = report.on_unavailable == SandboxUnavailableAction::RunUnconfined
        && report.requested_mode == SandboxMode::WorkspaceWrite;
    if report.fallback_eligible {
        report.effective_mode = Some(SandboxMode::DangerFullAccess);
        report.effective_network = Some(NetworkAccess::Allowed);
        report.resolution_kind = Some(SandboxResolutionKind::UnavailableFallback);
        report.native_execution_bypass = true;
    }
}

fn executable_identity(path: &Path) -> SandboxExecutableIdentity {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let metadata = std::fs::metadata(&canonical).ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let uid = metadata.as_ref().map(std::fs::Metadata::uid);
        let gid = metadata.as_ref().map(std::fs::Metadata::gid);
        let device = metadata.as_ref().map(std::fs::Metadata::dev);
        let inode = metadata.as_ref().map(std::fs::Metadata::ino);
        let mode = metadata
            .as_ref()
            .map(|metadata| metadata.permissions().mode());
        SandboxExecutableIdentity {
            path: canonical,
            uid,
            gid,
            mode,
            device,
            inode,
            root_owned: uid.map(|uid| uid == 0),
            group_or_world_writable: mode.map(|mode| mode & 0o022 != 0),
            special_permissions: mode.map(|mode| mode & 0o7000 != 0),
        }
    }
    #[cfg(not(unix))]
    {
        let _metadata = metadata;
        SandboxExecutableIdentity {
            path: canonical,
            uid: None,
            gid: None,
            mode: None,
            device: None,
            inode: None,
            root_owned: None,
            group_or_world_writable: None,
            special_permissions: None,
        }
    }
}

/// Runs the hidden in-sandbox helper when its argv marker is present.
#[must_use]
pub fn run_helper_from_args() -> Option<ExitCode> {
    #[cfg(target_os = "linux")]
    {
        linux::run_helper_from_args()
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut args = std::env::args_os();
        let _executable = args.next();
        (args.next().as_deref() == Some(OsStr::new(HELPER_MARKER))).then_some(ExitCode::FAILURE)
    }
}

/// Typed refusal before process spawn.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("OS sandbox is not implemented for platform `{0}`")]
    UnsupportedPlatform(String),
    #[error("Linux sandbox does not support architecture `{0}`")]
    UnsupportedArchitecture(String),
    #[error("Linux sandbox is unavailable on WSL1")]
    Wsl1Unsupported,
    #[error("no trusted system bubblewrap executable was found")]
    BubblewrapNotFound,
    #[error("bubblewrap candidate `{path}` is not trusted: {reason}")]
    UntrustedBubblewrap { path: PathBuf, reason: String },
    #[error("bubblewrap does not provide required option `{0}`")]
    MissingBubblewrapCapability(String),
    #[error("sandbox deployment capability `{capability}` is unavailable: {detail}")]
    UnavailableCapability {
        capability: &'static str,
        detail: String,
    },
    #[error("sandbox probe for `{capability}` failed: {detail}")]
    ProbeFailed {
        capability: &'static str,
        detail: String,
    },
    #[error("sandbox backend `{backend}` cannot enforce {requirement}")]
    UnsupportedPolicy {
        backend: String,
        requirement: String,
    },
    #[error("invalid sandbox policy: {0}")]
    InvalidPolicy(String),
    #[error("invalid sandbox {kind} `{path}`: {reason}")]
    InvalidPath {
        kind: &'static str,
        path: PathBuf,
        reason: String,
    },
    #[error("failed to compile seccomp policy: {0}")]
    Seccomp(String),
    #[error("sandbox helper failed: {0}")]
    Helper(String),
    #[error("sandbox process operation failed: {0}")]
    Io(#[from] std::io::Error),
}

fn canonical_directory(path: &Path, kind: &'static str) -> Result<PathBuf, SandboxError> {
    let canonical = path
        .canonicalize()
        .map_err(|error| SandboxError::InvalidPath {
            kind,
            path: path.to_owned(),
            reason: error.to_string(),
        })?;
    if !canonical.is_dir() {
        return Err(SandboxError::InvalidPath {
            kind,
            path: canonical,
            reason: "expected a directory".to_owned(),
        });
    }
    Ok(canonical)
}

fn canonical_protected_path(path: &Path) -> Result<PathBuf, SandboxError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| SandboxError::InvalidPath {
        kind: "protected path",
        path: path.to_owned(),
        reason: error.to_string(),
    })?;
    if metadata.file_type().is_symlink() {
        return Err(SandboxError::InvalidPath {
            kind: "protected path",
            path: path.to_owned(),
            reason: "symbolic links cannot be protected safely beneath a writable root".to_owned(),
        });
    }
    path.canonicalize()
        .map_err(|error| SandboxError::InvalidPath {
            kind: "protected path",
            path: path.to_owned(),
            reason: error.to_string(),
        })
}

fn command_sha256(program: &OsStr, arguments: &[OsString]) -> String {
    let mut digest = Sha256::new();
    digest.update(program.as_encoded_bytes());
    digest.update([0]);
    for argument in arguments {
        digest.update(argument.as_encoded_bytes());
        digest.update([0]);
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct LateUnavailableBackend {
        capabilities: SandboxCapabilities,
    }

    impl LateUnavailableBackend {
        fn new() -> Self {
            Self {
                capabilities: SandboxCapabilities {
                    backend: "late_unavailable".to_owned(),
                    executable: None,
                    read_only: true,
                    workspace_write: true,
                    danger_full_access: false,
                    network_isolation: true,
                },
            }
        }
    }

    impl SandboxBackend for LateUnavailableBackend {
        fn capabilities(&self) -> &SandboxCapabilities {
            &self.capabilities
        }

        fn prepare(&self, _request: PrepareRequest) -> Result<PreparedCommand, SandboxError> {
            Err(SandboxError::UnavailableCapability {
                capability: "command launch",
                detail: "the deployment changed after resolution".to_owned(),
            })
        }
    }

    #[test]
    fn capability_refuses_network_denial_without_a_network_namespace() {
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = SandboxPolicy::new(
            workspace.path(),
            SandboxMode::ReadOnly,
            NetworkAccess::Denied,
        )
        .expect("policy");
        let capabilities = SandboxCapabilities {
            backend: "test".to_owned(),
            executable: Some(PathBuf::from("/usr/bin/test")),
            read_only: true,
            workspace_write: true,
            danger_full_access: false,
            network_isolation: false,
        };

        let error = capabilities
            .supports(&policy)
            .expect_err("network denial must fail closed");

        assert!(matches!(
            error,
            SandboxError::UnsupportedPolicy { requirement, .. }
                if requirement == "network isolation"
        ));
    }

    #[test]
    fn prepared_authority_hashes_argv_and_never_persists_environment_values() {
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = SandboxPolicy::new(
            workspace.path(),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
        )
        .expect("policy")
        .with_approval_context("allow_all", "rules");
        let request = PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: vec![OsString::from("-lc"), OsString::from("printf ok")],
            cwd: workspace.path().to_owned(),
            environment: BTreeMap::from([(
                OsString::from("SECRET_TOKEN"),
                OsString::from("do-not-persist"),
            )]),
            policy,
        };
        let capabilities = SandboxCapabilities {
            backend: "test".to_owned(),
            executable: Some(PathBuf::from("/usr/bin/test")),
            read_only: true,
            workspace_write: true,
            danger_full_access: false,
            network_isolation: true,
        };

        let prepared = PreparedCommand::from_backend(
            request,
            OsString::from("/bin/sh"),
            vec![OsString::from("-lc"), OsString::from("printf ok")],
            &capabilities,
            vec![workspace.path().to_owned()],
            Vec::new(),
        );
        let encoded = serde_json::to_string(prepared.authority()).expect("authority JSON");

        assert_eq!(prepared.authority().environment_keys, ["SECRET_TOKEN"]);
        assert!(!encoded.contains("do-not-persist"));
        assert_eq!(prepared.authority().command_sha256.len(), 64);
        assert_eq!(prepared.authority().schema_version, 3);
        assert_eq!(
            prepared.authority().requested_mode(),
            SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            prepared.authority().resolution_kind,
            SandboxResolutionKind::Confined
        );
    }

    #[test]
    fn v2_authority_reads_requested_as_effective_and_legacy_resolution() {
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = SandboxPolicy::new(
            workspace.path(),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
        )
        .expect("policy");
        let capabilities = SandboxCapabilities {
            backend: "test".to_owned(),
            executable: None,
            read_only: true,
            workspace_write: true,
            danger_full_access: false,
            network_isolation: true,
        };
        let prepared = PreparedCommand::from_backend(
            PrepareRequest {
                program: OsString::from("/bin/sh"),
                arguments: Vec::new(),
                cwd: workspace.path().to_owned(),
                environment: BTreeMap::new(),
                policy,
            },
            OsString::from("/bin/sh"),
            Vec::new(),
            &capabilities,
            Vec::new(),
            Vec::new(),
        );
        let mut value = serde_json::to_value(prepared.authority()).expect("authority JSON");
        let object = value.as_object_mut().expect("authority object");
        object.insert("schemaVersion".to_owned(), serde_json::json!(2));
        object.remove("requestedMode");
        object.remove("requestedNetwork");
        object.remove("resolutionKind");
        object.remove("fallbackReason");

        let legacy: ExecutionAuthority =
            serde_json::from_value(value).expect("v2 authority remains readable");

        assert_eq!(legacy.requested_mode(), legacy.mode);
        assert_eq!(legacy.requested_network(), legacy.network);
        assert_eq!(legacy.resolution_kind, SandboxResolutionKind::Legacy);
        assert!(legacy.fallback_reason.is_none());
    }

    #[test]
    fn explicit_full_access_never_invokes_restricted_discovery() {
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = SandboxPolicy::new(
            workspace.path(),
            SandboxMode::DangerFullAccess,
            NetworkAccess::Allowed,
        )
        .expect("policy");

        let resolution =
            resolve_policy_with(policy, SandboxUnavailableAction::RunUnconfined, |_| {
                panic!("restricted discovery must not run for explicit full access")
            })
            .expect("explicit native resolution");

        assert_eq!(resolution.kind(), SandboxResolutionKind::ExplicitNative);
        assert!(resolution.fallback_reason().is_none());
        assert_eq!(
            resolution.execution_policy().mode(),
            SandboxMode::DangerFullAccess
        );
    }

    #[test]
    fn eligible_unavailable_error_resolves_to_native_without_widening_approval_mode() {
        let workspace = tempfile::tempdir().expect("workspace");
        let requested = SandboxPolicy::new(
            workspace.path(),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
        )
        .expect("policy")
        .with_writable_roots([workspace.path().to_owned()])
        .expect("writable root")
        .with_approval_context("strict", "rules");

        let resolution =
            resolve_policy_with(requested, SandboxUnavailableAction::RunUnconfined, |_| {
                Err(SandboxError::BubblewrapNotFound)
            })
            .expect("eligible fallback");

        assert_eq!(
            resolution.kind(),
            SandboxResolutionKind::UnavailableFallback
        );
        assert!(matches!(
            resolution.fallback_reason(),
            Some(SandboxUnavailableCause::BubblewrapNotFound)
        ));
        assert_eq!(
            resolution.requested_policy().mode(),
            SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            resolution.requested_policy().network(),
            NetworkAccess::Denied
        );
        assert_eq!(
            resolution.execution_policy().mode(),
            SandboxMode::DangerFullAccess
        );
        assert_eq!(
            resolution.execution_policy().network(),
            NetworkAccess::Allowed
        );

        let prepared = resolution
            .backend()
            .prepare(PrepareRequest {
                program: OsString::from("/bin/sh"),
                arguments: Vec::new(),
                cwd: workspace.path().to_owned(),
                environment: BTreeMap::new(),
                policy: resolution.execution_policy().clone(),
            })
            .expect("native command");
        let authority = prepared.authority();
        assert_eq!(authority.approval_mode, "strict");
        assert_eq!(authority.requested_mode(), SandboxMode::WorkspaceWrite);
        assert_eq!(authority.mode, SandboxMode::DangerFullAccess);
        assert_eq!(authority.requested_network(), NetworkAccess::Denied);
        assert_eq!(authority.network, NetworkAccess::Allowed);
        assert!(authority.writable_roots.is_empty());
        assert_eq!(
            authority.resolution_kind,
            SandboxResolutionKind::UnavailableFallback
        );
    }

    #[test]
    fn fallback_remains_fail_closed_for_deny_read_only_and_nonavailability_errors() {
        let workspace = tempfile::tempdir().expect("workspace");
        let workspace_write = || {
            SandboxPolicy::new(
                workspace.path(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
            )
            .expect("workspace-write policy")
        };
        let read_only = SandboxPolicy::new(
            workspace.path(),
            SandboxMode::ReadOnly,
            NetworkAccess::Denied,
        )
        .expect("read-only policy");

        let denied = resolve_policy_with(workspace_write(), SandboxUnavailableAction::Deny, |_| {
            Err(SandboxError::BubblewrapNotFound)
        })
        .expect_err("default policy remains fail-closed");
        assert!(matches!(denied, SandboxError::BubblewrapNotFound));

        let read_only =
            resolve_policy_with(read_only, SandboxUnavailableAction::RunUnconfined, |_| {
                Err(SandboxError::BubblewrapNotFound)
            })
            .expect_err("read-only authority cannot run unconfined");
        assert!(matches!(read_only, SandboxError::BubblewrapNotFound));

        let untrusted = resolve_policy_with(
            workspace_write(),
            SandboxUnavailableAction::RunUnconfined,
            |_| {
                Err(SandboxError::UntrustedBubblewrap {
                    path: PathBuf::from("/tmp/bwrap"),
                    reason: "writable by another user".to_owned(),
                })
            },
        )
        .expect_err("untrusted launchers are not availability failures");
        assert!(matches!(
            untrusted,
            SandboxError::UntrustedBubblewrap { .. }
        ));

        let helper = resolve_policy_with(
            workspace_write(),
            SandboxUnavailableAction::RunUnconfined,
            |_| Err(SandboxError::Helper("internal failure".to_owned())),
        )
        .expect_err("helper failures cannot widen authority");
        assert!(matches!(helper, SandboxError::Helper(_)));

        let probe = resolve_policy_with(
            workspace_write(),
            SandboxUnavailableAction::RunUnconfined,
            |_| {
                Err(SandboxError::ProbeFailed {
                    capability: "prepared sandbox helper execution",
                    detail: "helper exited unexpectedly".to_owned(),
                })
            },
        )
        .expect_err("helper execution probes cannot widen authority");
        assert!(matches!(probe, SandboxError::ProbeFailed { .. }));
    }

    #[test]
    fn namespace_unavailability_is_eligible_for_trusted_fallback() {
        let workspace = tempfile::tempdir().expect("workspace");
        let resolution = resolve_policy_with(
            SandboxPolicy::new(
                workspace.path(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
            )
            .expect("policy"),
            SandboxUnavailableAction::RunUnconfined,
            |_| {
                Err(SandboxError::UnavailableCapability {
                    capability: "network namespace",
                    detail: "operation not permitted".to_owned(),
                })
            },
        )
        .expect("namespace policy may activate trusted fallback");

        assert!(matches!(
            resolution.fallback_reason(),
            Some(SandboxUnavailableCause::DeploymentCapabilityUnavailable {
                capability,
                ..
            }) if capability == "network namespace"
        ));
    }

    #[test]
    fn command_preparation_failure_never_triggers_a_second_resolution() {
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = SandboxPolicy::new(
            workspace.path(),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
        )
        .expect("policy");
        let resolution =
            resolve_policy_with(policy, SandboxUnavailableAction::RunUnconfined, |_| {
                Ok(Arc::new(LateUnavailableBackend::new()))
            })
            .expect("deployment verified before the command");

        assert_eq!(resolution.kind(), SandboxResolutionKind::Confined);
        let error = resolution
            .backend()
            .prepare(PrepareRequest {
                program: OsString::from("/bin/sh"),
                arguments: Vec::new(),
                cwd: workspace.path().to_owned(),
                environment: BTreeMap::new(),
                policy: resolution.execution_policy().clone(),
            })
            .expect_err("command-stage availability errors are terminal");

        assert!(matches!(
            error,
            SandboxError::UnavailableCapability {
                capability: "command launch",
                ..
            }
        ));
    }

    #[test]
    fn explicit_full_access_keeps_the_raw_command_inside_the_managed_process_pipeline() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend =
            DangerFullAccessSandbox::new(workspace.path()).expect("native full-access backend");
        let request = PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: vec![OsString::from("-lc"), OsString::from("printf native")],
            cwd: workspace.path().to_owned(),
            environment: BTreeMap::new(),
            policy: SandboxPolicy::new(
                workspace.path(),
                SandboxMode::DangerFullAccess,
                NetworkAccess::Allowed,
            )
            .expect("full-access policy"),
        };

        let prepared = backend.prepare(request).expect("prepared raw launch");
        assert_eq!(prepared.authority().mode, SandboxMode::DangerFullAccess);
        assert_eq!(prepared.authority().backend, "danger_full_access");
        assert_eq!(prepared.authority().backend_executable, None);

        let parts = prepared.into_parts();
        assert_eq!(parts.program, OsString::from("/bin/sh"));
        assert_eq!(
            parts.arguments,
            [OsString::from("-lc"), OsString::from("printf native")]
        );
    }

    #[test]
    fn full_access_is_never_an_implicit_fallback_for_a_confined_policy() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend =
            DangerFullAccessSandbox::new(workspace.path()).expect("native full-access backend");
        let request = PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: Vec::new(),
            cwd: workspace.path().to_owned(),
            environment: BTreeMap::new(),
            policy: SandboxPolicy::new(
                workspace.path(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Allowed,
            )
            .expect("confined policy"),
        };

        let error = backend
            .prepare(request)
            .expect_err("the unconfined backend must reject confined policy");
        assert!(matches!(error, SandboxError::UnsupportedPolicy { .. }));
    }

    #[test]
    fn full_access_deployment_report_is_explicitly_marked_as_a_native_bypass() {
        let workspace = tempfile::tempdir().expect("workspace");

        let report = deployment_report(
            workspace.path(),
            SandboxMode::DangerFullAccess,
            NetworkAccess::Allowed,
        );

        assert!(report.ready);
        assert!(report.native_execution_bypass);
        assert!(report.launcher.is_none());
        assert!(report.error.is_none());
        assert!(report.checks.iter().any(|check| {
            check.name == "launcher_trust" && check.status == SandboxDeploymentCheckStatus::Skipped
        }));
        assert!(report.checks.iter().any(|check| {
            check.name == "execution_self_test"
                && check.status == SandboxDeploymentCheckStatus::Skipped
        }));
    }

    #[test]
    fn fallback_report_keeps_requested_deployment_unready() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut report = SandboxDeploymentReport {
            platform: "linux".to_owned(),
            architecture: "x86_64".to_owned(),
            workspace: workspace.path().to_owned(),
            requested_mode: SandboxMode::WorkspaceWrite,
            requested_network: NetworkAccess::Denied,
            on_unavailable: SandboxUnavailableAction::RunUnconfined,
            effective_mode: None,
            effective_network: None,
            resolution_kind: None,
            fallback_eligible: false,
            fallback_reason: None,
            ready: false,
            native_execution_bypass: false,
            capabilities: None,
            launcher: None,
            checks: Vec::new(),
            error: None,
        };

        record_deployment_failure(
            &mut report,
            &SandboxError::UnavailableCapability {
                capability: "network namespace",
                detail: "operation not permitted".to_owned(),
            },
        );

        assert!(!report.ready);
        assert!(report.fallback_eligible);
        assert_eq!(
            report.resolution_kind,
            Some(SandboxResolutionKind::UnavailableFallback)
        );
        assert_eq!(report.effective_mode, Some(SandboxMode::DangerFullAccess));
        assert_eq!(report.effective_network, Some(NetworkAccess::Allowed));
        assert!(matches!(
            report.fallback_reason,
            Some(SandboxUnavailableCause::DeploymentCapabilityUnavailable { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn configured_protected_paths_must_exist_and_cannot_be_symbolic_links() {
        let workspace = tempfile::tempdir().expect("workspace");
        let target = workspace.path().join("target");
        std::fs::create_dir(&target).expect("target");
        let link = workspace.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("link");

        let missing = SandboxPolicy::new(
            workspace.path(),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
        )
        .expect("policy")
        .with_protected_paths([workspace.path().join("missing")])
        .expect_err("missing protected path must fail");
        assert!(matches!(missing, SandboxError::InvalidPath { .. }));

        let symlink = SandboxPolicy::new(
            workspace.path(),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
        )
        .expect("policy")
        .with_protected_paths([link])
        .expect_err("symlink protected path must fail");
        assert!(
            matches!(symlink, SandboxError::InvalidPath { reason, .. } if reason.contains("symbolic"))
        );
    }
}
