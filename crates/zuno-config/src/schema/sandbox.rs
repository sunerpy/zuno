//! Native OS sandbox configuration.
//!
//! Restricted modes require a proved platform backend and fail closed when one is
//! unavailable. `danger-full-access` is a separate, explicit native-execution
//! policy; it is never selected as a fallback.

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
    /// Existing directories writable in addition to the workspace for write-capable Agents.
    ///
    /// Relative paths resolve against the active workspace.
    #[serde(rename = "writableRoots", skip_serializing_if = "Option::is_none")]
    pub writable_roots: Option<Vec<String>>,
    /// Paths reapplied read-only after writable roots are mounted.
    ///
    /// Relative paths resolve against the active workspace. Missing paths are
    /// ignored until they exist.
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
}
