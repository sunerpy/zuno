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
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::LinuxBubblewrapSandbox;

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

/// Immutable policy selected before a shell command is prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    workspace: PathBuf,
    mode: SandboxMode,
    network: NetworkAccess,
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
    pub mode: SandboxMode,
    pub network: NetworkAccess,
    pub writable_roots: Vec<PathBuf>,
    pub protected_paths: Vec<PathBuf>,
    pub cwd: PathBuf,
    pub command_sha256: String,
    pub environment_keys: Vec<String>,
    pub approval_mode: String,
    pub reviewer_policy_sha256: String,
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
            schema_version: 2,
            backend: backend.backend.clone(),
            backend_executable: backend.executable.clone(),
            workspace: request.policy.workspace.clone(),
            mode: request.policy.mode,
            network: request.policy.network,
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
/// Restricted modes never fall back to [`DangerFullAccessSandbox`].
pub fn system_backend(
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
