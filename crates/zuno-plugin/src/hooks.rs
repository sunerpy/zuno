use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use url::Url;
use zuno_config::Config;
use zuno_config::schema::ordered::OrderedMap;
use zuno_engine::r#loop::TurnEvent;
use zuno_error::{BoxSource, PluginError};
use zuno_paths::ResolvedProject;
use zuno_tool::{Tool, ToolDefinition, ToolOutput};

use crate::auth::AuthHook;
use crate::manifest::{HookName, PluginManifest};
use crate::payload::*;
use crate::provider::ProviderHook;

/// Runtime tools registered by one or more plugin resources.
pub type PluginTools = OrderedMap<Arc<dyn Tool>>;

/// Host values supplied while a plugin module is initialized (`index.ts:56-66`).
pub struct PluginInput<Client, WorkspaceRegistrar, Shell> {
    pub client: Client,
    pub project: ResolvedProject,
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub experimental_workspace: WorkspaceRegistrar,
    pub server_url: Url,
    pub shell: Shell,
}

/// Imported module shape; `Factory` remains host-specific until a runtime loads it.
pub struct PluginModule<Factory> {
    pub id: Option<String>,
    pub server: Factory,
}

/// A loaded plugin whose callback handles remain alive in its host.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;

    fn is_disabled(&self) -> bool {
        false
    }

    fn tools(&self) -> PluginTools {
        PluginTools::new()
    }

    fn auth(&self) -> Option<AuthHook> {
        None
    }

    fn provider(&self) -> Option<ProviderHook> {
        None
    }

    async fn call(&self, _hook: &mut HookInvocation<'_>) -> Result<(), BoxSource> {
        Ok(())
    }
}

/// Typed invocation union for every member of upstream `Hooks` (`index.ts:222-335`).
pub enum HookInvocation<'a> {
    Dispose,
    Event {
        event: &'a TurnEvent,
    },
    Config {
        config: &'a mut Config,
    },
    Tool {
        output: &'a mut PluginTools,
    },
    Auth {
        output: &'a mut Vec<AuthHook>,
    },
    Provider {
        output: &'a mut Vec<ProviderHook>,
    },
    ChatMessage {
        input: &'a ChatMessageInput<'a>,
        output: &'a mut ChatMessageOutput,
    },
    ChatParams {
        input: &'a ChatContext<'a>,
        output: &'a mut ChatParamsOutput,
    },
    ChatHeaders {
        input: &'a ChatContext<'a>,
        output: &'a mut ChatHeadersOutput,
    },
    PermissionAsk {
        input: &'a PermissionAskInput<'a>,
        output: &'a mut PermissionAskOutput,
    },
    CommandExecuteBefore {
        input: &'a CommandExecuteBeforeInput<'a>,
        output: &'a mut CommandExecuteBeforeOutput,
    },
    ToolExecuteBefore {
        input: &'a ToolExecuteBeforeInput<'a>,
        output: &'a mut ToolExecuteBeforeOutput,
    },
    ShellEnv {
        input: &'a ShellEnvInput<'a>,
        output: &'a mut ShellEnvOutput,
    },
    ToolExecuteAfter {
        input: &'a ToolExecuteAfterInput<'a>,
        output: &'a mut ToolOutput,
    },
    ChatMessagesTransform {
        output: &'a mut ChatMessagesTransformOutput,
    },
    ChatSystemTransform {
        input: &'a ChatSystemTransformInput<'a>,
        output: &'a mut ChatSystemTransformOutput,
    },
    ProviderSmallModel {
        input: &'a ProviderSmallModelInput<'a>,
        output: &'a mut ProviderSmallModelOutput,
    },
    SessionCompacting {
        input: &'a SessionCompactingInput<'a>,
        output: &'a mut SessionCompactingOutput,
    },
    CompactionAutocontinue {
        input: &'a CompactionAutocontinueInput<'a>,
        output: &'a mut CompactionAutocontinueOutput,
    },
    TextComplete {
        input: &'a TextCompleteInput<'a>,
        output: &'a mut TextCompleteOutput,
    },
    ToolDefinition {
        input: &'a ToolDefinitionInput<'a>,
        output: &'a mut ToolDefinition,
    },
}

impl HookInvocation<'_> {
    /// The stable dispatch key for this payload.
    #[must_use]
    pub const fn name(&self) -> HookName {
        match self {
            Self::Dispose => HookName::Dispose,
            Self::Event { .. } => HookName::Event,
            Self::Config { .. } => HookName::Config,
            Self::Tool { .. } => HookName::Tool,
            Self::Auth { .. } => HookName::Auth,
            Self::Provider { .. } => HookName::Provider,
            Self::ChatMessage { .. } => HookName::ChatMessage,
            Self::ChatParams { .. } => HookName::ChatParams,
            Self::ChatHeaders { .. } => HookName::ChatHeaders,
            Self::PermissionAsk { .. } => HookName::PermissionAsk,
            Self::CommandExecuteBefore { .. } => HookName::CommandExecuteBefore,
            Self::ToolExecuteBefore { .. } => HookName::ToolExecuteBefore,
            Self::ShellEnv { .. } => HookName::ShellEnv,
            Self::ToolExecuteAfter { .. } => HookName::ToolExecuteAfter,
            Self::ChatMessagesTransform { .. } => HookName::ChatMessagesTransform,
            Self::ChatSystemTransform { .. } => HookName::ChatSystemTransform,
            Self::ProviderSmallModel { .. } => HookName::ProviderSmallModel,
            Self::SessionCompacting { .. } => HookName::SessionCompacting,
            Self::CompactionAutocontinue { .. } => HookName::CompactionAutocontinue,
            Self::TextComplete { .. } => HookName::TextComplete,
            Self::ToolDefinition { .. } => HookName::ToolDefinition,
        }
    }
}

/// Configuration-ordered plugin registry and sequential hook dispatcher.
pub struct HookBus {
    plugins: Vec<Arc<dyn Plugin>>,
}

impl HookBus {
    /// Preserve the supplied configuration order without sorting or parallelism.
    #[must_use]
    pub const fn new(plugins: Vec<Arc<dyn Plugin>>) -> Self {
        Self { plugins }
    }

    /// Append one loaded plugin at the end of configuration order.
    pub fn register(&mut self, plugin: Arc<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    /// Loaded plugins in dispatch order.
    #[must_use]
    pub fn plugins(&self) -> &[Arc<dyn Plugin>] {
        &self.plugins
    }

    /// Dispatch one invocation sequentially, preserving prior output mutations.
    ///
    /// # Errors
    /// Returns [`PluginError::Hook`] naming the first plugin callback that fails.
    pub async fn dispatch(&self, mut hook: HookInvocation<'_>) -> Result<(), PluginError> {
        match &mut hook {
            HookInvocation::Tool { output } => {
                for plugin in &self.plugins {
                    if plugin.manifest().supports(HookName::Tool) {
                        for (id, tool) in plugin.tools() {
                            output.insert(id, tool);
                        }
                    }
                }
                Ok(())
            }
            HookInvocation::Auth { output } => {
                for plugin in &self.plugins {
                    if plugin.manifest().supports(HookName::Auth)
                        && let Some(auth) = plugin.auth()
                    {
                        output.push(auth);
                    }
                }
                Ok(())
            }
            HookInvocation::Provider { output } => {
                for plugin in &self.plugins {
                    if plugin.manifest().supports(HookName::Provider)
                        && let Some(provider) = plugin.provider()
                    {
                        output.push(provider);
                    }
                }
                Ok(())
            }
            HookInvocation::Dispose
            | HookInvocation::Event { .. }
            | HookInvocation::Config { .. }
            | HookInvocation::ChatMessage { .. }
            | HookInvocation::ChatParams { .. }
            | HookInvocation::ChatHeaders { .. }
            | HookInvocation::PermissionAsk { .. }
            | HookInvocation::CommandExecuteBefore { .. }
            | HookInvocation::ToolExecuteBefore { .. }
            | HookInvocation::ShellEnv { .. }
            | HookInvocation::ToolExecuteAfter { .. }
            | HookInvocation::ChatMessagesTransform { .. }
            | HookInvocation::ChatSystemTransform { .. }
            | HookInvocation::ProviderSmallModel { .. }
            | HookInvocation::SessionCompacting { .. }
            | HookInvocation::CompactionAutocontinue { .. }
            | HookInvocation::TextComplete { .. }
            | HookInvocation::ToolDefinition { .. } => {
                let name = hook.name();
                for plugin in &self.plugins {
                    if plugin.manifest().supports(name)
                        && let Err(source) = plugin.call(&mut hook).await
                        && !plugin.is_disabled()
                    {
                        return Err(PluginError::Hook {
                            plugin: plugin.manifest().id().to_owned(),
                            hook: name.to_string(),
                            source,
                        });
                    }
                }
                Ok(())
            }
        }
    }
}
