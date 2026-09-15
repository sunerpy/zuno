use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_plugins::PluginIdentity;

/// Versioned schema accepted by a plugin's `agentBackends` resource.
pub const PLUGIN_AGENT_BACKENDS_API_VERSION: &str = "zuno.agent-backends/v1";

/// Runtime implementation selected by one plugin-owned Agent backend declaration.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PluginAgentBackendKind {
    NativeCodex,
    ClaudeCode,
    Acp,
}

/// Executable selected by a plugin declaration without granting arbitrary host paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginAgentBackendCommand {
    /// A bare executable name resolved through the host's ordinary PATH policy.
    Name(String),
    /// A package-relative executable resolved beneath the installed plugin root.
    PluginPath(AbsolutePathBuf),
}

impl PluginAgentBackendCommand {
    pub fn as_os_str(&self) -> &std::ffi::OsStr {
        match self {
            Self::Name(name) => name.as_ref(),
            Self::PluginPath(path) => path.as_path().as_os_str(),
        }
    }
}

/// Validated, package-local backend declaration.
///
/// Model, provider, reasoning, permission, and sandbox choices intentionally do
/// not appear here. Those values come from the workflow's user-owned
/// `executionProfile` when a call is admitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginAgentBackendDeclaration {
    pub local_id: String,
    pub kind: PluginAgentBackendKind,
    /// Version declared by the owning plugin manifest, when present.
    pub plugin_version: Option<String>,
    /// Exact declaration document used to create this loaded generation.
    pub source_path: AbsolutePathBuf,
    pub source_digest: String,
    /// Current-platform package executable bytes, when the command is package-local.
    pub executable_digest: Option<String>,
    /// Non-secret loader generation covering the declaration document and the
    /// selected package-relative executable bytes on this platform.
    pub source_generation: String,
    pub command: Option<PluginAgentBackendCommand>,
    pub command_windows: Option<PluginAgentBackendCommand>,
    pub args: Vec<String>,
    /// Environment variable names explicitly forwarded from the launching host.
    /// Values are never stored in plugin metadata or the loaded-plugin cache.
    pub env_vars: Vec<String>,
    pub startup_timeout_ms: u64,
    pub run_timeout_ms: Option<u64>,
    pub dispose_grace_ms: u64,
    pub max_message_bytes: usize,
}

impl PluginAgentBackendDeclaration {
    pub fn command_for_current_platform(&self) -> Option<&PluginAgentBackendCommand> {
        #[cfg(windows)]
        if let Some(command) = self.command_windows.as_ref() {
            return Some(command);
        }
        self.command.as_ref()
    }
}

/// Active plugin backend with stable package attribution and a namespaced ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectivePluginAgentBackend {
    /// `<plugin manifest name>/<local id>`; workflows reference this exact value.
    pub id: String,
    pub plugin_identity: PluginIdentity,
    pub declaration: PluginAgentBackendDeclaration,
}
