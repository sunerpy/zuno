//! Native OS sandbox configuration.
//!
//! Restricted modes require a proved platform backend and fail closed when one is
//! unavailable unless a trusted layer explicitly permits unconfined execution.
//! `danger-full-access` remains a separate, explicit native-execution policy, and
//! `backend: native` is a separate, explicit backend selection that keeps the
//! configured permission mode while running every Agent's Shell natively.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Filesystem/process confinement selected for model-initiated Shell calls.
#[derive(JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Read the host filesystem, but do not permit host writes.
    ReadOnly,
    /// Write the active workspace and explicitly trusted extra roots.
    #[default]
    WorkspaceWrite,
    /// Run as the Zuno user with the host filesystem, processes, and network.
    DangerFullAccess,
}

impl SandboxMode {
    /// Stable configuration and CLI spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    /// Parse the exact public spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }
}

/// Network authority available to model-initiated Shell calls.
#[derive(JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNetworkMode {
    /// Create a private network namespace and deny network syscalls.
    #[default]
    Deny,
    /// Inherit the host network namespace.
    Allow,
}

/// Runtime action when a confined OS backend cannot be deployed.
#[derive(JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxUnavailableAction {
    /// Refuse to publish or execute Shell without the requested confinement.
    #[default]
    Deny,
    /// Run through the native process backend after an eligible deployment failure.
    RunUnconfined,
}

impl SandboxUnavailableAction {
    /// Stable configuration, CLI, and environment spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::RunUnconfined => "run-unconfined",
        }
    }

    /// Parse the exact public spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "deny" => Some(Self::Deny),
            "run-unconfined" => Some(Self::RunUnconfined),
            _ => None,
        }
    }
}

/// Which execution backend resolves model-initiated Shell calls.
///
/// Orthogonal to `mode`: the mode is the authority an Agent is granted, the
/// backend is what enforces it. `native` enforces nothing at the OS level and
/// says so in every record and notice.
#[derive(JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxBackendSelection {
    /// Discover the platform's confined backend and apply `onUnavailable` when it
    /// cannot be deployed.
    #[default]
    Auto,
    /// Run every Agent's Shell, read-only contracts included, on the native process
    /// backend with the configured permission mode kept. The requested authority is
    /// recorded but not OS-enforced.
    Native,
}

impl SandboxBackendSelection {
    /// Stable configuration, CLI, and environment spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Native => "native",
        }
    }

    /// Parse the exact public spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "native" => Some(Self::Native),
            _ => None,
        }
    }
}

/// Policy additions shared by every Shell call in one resolved profile.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Maximum Shell authority for this invocation. Absence defaults to
    /// `workspace-write`; read-only Agent contracts still narrow it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<SandboxMode>,
    /// Network authority. Absence defaults to `deny` in confined modes and to
    /// host networking in `danger-full-access`; that mode rejects explicit
    /// `deny` because it cannot enforce it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<SandboxNetworkMode>,
    /// Action when a confined backend is unavailable. Absence fails closed.
    ///
    /// `run-unconfined` is accepted only from a trusted configuration layer.
    #[serde(rename = "onUnavailable", skip_serializing_if = "Option::is_none")]
    pub on_unavailable: Option<SandboxUnavailableAction>,
    /// Backend selection. `auto` (default) discovers the platform's confined
    /// backend and applies `onUnavailable`. `native` runs every Agent's Shell,
    /// read-only contracts included, on the native process backend with the
    /// configured permission mode kept; the requested authority is recorded but
    /// not OS-enforced. Accepted only from a trusted configuration layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<SandboxBackendSelection>,
    /// Existing directories writable in addition to the workspace for write-capable Agents.
    ///
    /// Relative paths resolve against the active workspace.
    #[serde(rename = "writableRoots", skip_serializing_if = "Option::is_none")]
    pub writable_roots: Option<Vec<String>>,
    /// Paths reapplied read-only after writable roots are mounted.
    ///
    /// Relative paths resolve against the active workspace. Each entry must
    /// already exist and must not be a symbolic link; deployment fails closed
    /// rather than starting an Agent whose protection was silently dropped.
    /// Cannot be combined with `danger-full-access`, which has no confined
    /// backend to enforce it.
    #[serde(rename = "protectedPaths", skip_serializing_if = "Option::is_none")]
    pub protected_paths: Option<Vec<String>>,
}

impl SandboxConfig {
    /// Resolve the default public mode.
    #[must_use]
    pub fn resolved_mode(&self) -> SandboxMode {
        self.mode.unwrap_or_default()
    }

    /// Resolve the fail-closed network default.
    ///
    /// Explicit full access inherently inherits the host network. A read-only
    /// Agent may narrow that configured maximum and returns to the denied default
    /// at runtime.
    #[must_use]
    pub fn resolved_network(&self) -> SandboxNetworkMode {
        self.network.unwrap_or_else(|| {
            if self.resolved_mode() == SandboxMode::DangerFullAccess {
                SandboxNetworkMode::Allow
            } else {
                SandboxNetworkMode::Deny
            }
        })
    }

    /// Resolve the fail-closed unavailable-backend default.
    #[must_use]
    pub fn resolved_on_unavailable(&self) -> SandboxUnavailableAction {
        self.on_unavailable.unwrap_or_default()
    }

    /// Resolve the backend selection; absence discovers the confined backend.
    #[must_use]
    pub fn resolved_backend(&self) -> SandboxBackendSelection {
        self.backend.unwrap_or_default()
    }
}
