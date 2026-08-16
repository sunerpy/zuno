use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

/// One key of the authoritative TypeScript `Hooks` interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookName {
    Dispose,
    Event,
    Config,
    Tool,
    Auth,
    Provider,
    ChatMessage,
    ChatParams,
    ChatHeaders,
    PermissionAsk,
    CommandExecuteBefore,
    ToolExecuteBefore,
    ShellEnv,
    ToolExecuteAfter,
    ChatMessagesTransform,
    ChatSystemTransform,
    ProviderSmallModel,
    SessionCompacting,
    CompactionAutocontinue,
    TextComplete,
    ToolDefinition,
}

impl HookName {
    /// Every supported key, in upstream declaration order.
    pub const ALL: [Self; 21] = [
        Self::Dispose,
        Self::Event,
        Self::Config,
        Self::Tool,
        Self::Auth,
        Self::Provider,
        Self::ChatMessage,
        Self::ChatParams,
        Self::ChatHeaders,
        Self::PermissionAsk,
        Self::CommandExecuteBefore,
        Self::ToolExecuteBefore,
        Self::ShellEnv,
        Self::ToolExecuteAfter,
        Self::ChatMessagesTransform,
        Self::ChatSystemTransform,
        Self::ProviderSmallModel,
        Self::SessionCompacting,
        Self::CompactionAutocontinue,
        Self::TextComplete,
        Self::ToolDefinition,
    ];

    /// The exact property name used by JavaScript plugins.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dispose => "dispose",
            Self::Event => "event",
            Self::Config => "config",
            Self::Tool => "tool",
            Self::Auth => "auth",
            Self::Provider => "provider",
            Self::ChatMessage => "chat.message",
            Self::ChatParams => "chat.params",
            Self::ChatHeaders => "chat.headers",
            Self::PermissionAsk => "permission.ask",
            Self::CommandExecuteBefore => "command.execute.before",
            Self::ToolExecuteBefore => "tool.execute.before",
            Self::ShellEnv => "shell.env",
            Self::ToolExecuteAfter => "tool.execute.after",
            Self::ChatMessagesTransform => "experimental.chat.messages.transform",
            Self::ChatSystemTransform => "experimental.chat.system.transform",
            Self::ProviderSmallModel => "experimental.provider.small_model",
            Self::SessionCompacting => "experimental.session.compacting",
            Self::CompactionAutocontinue => "experimental.compaction.autocontinue",
            Self::TextComplete => "experimental.text.complete",
            Self::ToolDefinition => "tool.definition",
        }
    }
}

impl fmt::Display for HookName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for HookName {
    type Err = UnknownHookName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|hook| hook.as_str() == name)
            .ok_or_else(|| UnknownHookName {
                name: name.to_owned(),
                valid: Self::ALL.map(Self::as_str).join(", "),
            })
    }
}

/// A manifest named a hook that this host cannot dispatch.
#[derive(Debug, thiserror::Error)]
#[error("unknown hook `{name}`; valid hooks: {valid}")]
pub struct UnknownHookName {
    name: String,
    valid: String,
}

/// A validated loaded-plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    id: String,
    hooks: Box<[HookName]>,
}

impl PluginManifest {
    /// Validate an identity and its supported hook set.
    ///
    /// # Errors
    /// Returns [`ManifestError`] for an empty id or a duplicate hook.
    pub fn new(id: impl Into<String>, hooks: Vec<HookName>) -> Result<Self, ManifestError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(ManifestError::EmptyId);
        }
        let mut seen = HashSet::with_capacity(hooks.len());
        for hook in &hooks {
            if !seen.insert(*hook) {
                return Err(ManifestError::DuplicateHook { hook: *hook });
            }
        }
        Ok(Self {
            id,
            hooks: hooks.into_boxed_slice(),
        })
    }

    /// Stable plugin identity used in diagnostics.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Hooks implemented by this plugin, in manifest order.
    #[must_use]
    pub fn hooks(&self) -> &[HookName] {
        &self.hooks
    }

    /// Whether the plugin accepts this invocation.
    #[must_use]
    pub fn supports(&self, hook: HookName) -> bool {
        self.hooks.contains(&hook)
    }
}

/// Invalid loaded-plugin metadata.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("plugin id must not be empty")]
    EmptyId,
    #[error("plugin manifest contains duplicate hook `{hook}`")]
    DuplicateHook { hook: HookName },
}
