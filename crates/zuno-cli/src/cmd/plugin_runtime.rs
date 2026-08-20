use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use serde_json::{Map, Value};
use zuno_engine::compaction::{
    AutoContinueHookInput, CompactionHookInput, CompactionHooks, CompactionPrompt,
};
use zuno_engine::hooks::{
    HookMessageWithParts, PermissionHookDecision, RequestHookInput, ToolHooks, TurnHooks,
};
use zuno_engine::terminal_lease::{
    LeaseReason, ReclaimCause, TerminalBroker, TerminalLease, TerminalLeaseError,
    TerminalLeaseGuard, TerminalOwner,
};
use zuno_llm::catalog::availability::AvailabilitySource;
use zuno_llm::catalog::resolved::{ResolvedModel as CatalogModel, ResolvedProvider};
use zuno_llm::registry::CompletionRequest;
use zuno_permission::PermissionRequest;
use zuno_plugin::{
    AuthCallbackResult, AuthCredentialResolver, AuthMethod, AuthOAuthCallback, AuthSuccess,
    ChatContext, ChatHeadersOutput, ChatMessageInput, ChatMessageOutput,
    ChatMessagesTransformOutput, ChatParamsOutput, ChatSystemTransformInput,
    ChatSystemTransformOutput, CommandExecuteBeforeInput, CommandExecuteBeforeOutput,
    CompactionAutocontinueInput, CompactionAutocontinueOutput, ConfigDirectory, HookBus,
    HookInvocation, HookName, JsHostConfig, JsPluginKind, JsPluginLoad, JsPluginSpec,
    MessageWithParts, PermissionAskInput, PermissionAskOutput, PermissionStatus, Plugin,
    PluginLoad, PluginOrigin, PluginProcessSpec, PluginScope, PluginTools, ProviderContext,
    ProviderHookContext, ProviderSmallModelInput, ProviderSmallModelOutput, ProviderSource,
    SessionCompactingInput, SessionCompactingOutput, ShellEnvInput as PluginShellEnvInput,
    ShellEnvOutput, TextCompleteInput, TextCompleteOutput, ToolDefinitionInput,
    ToolExecuteAfterInput, ToolExecuteBeforeInput, ToolExecuteBeforeOutput, discover_plugins,
    discover_process_plugins, load_js_plugins_ordered, load_plugins_ordered,
};
use zuno_server::{
    ProviderOAuthAuthorization, ProviderOAuthAuthorizeRequest, ProviderOAuthBackend,
    ProviderOAuthCallbackRequest, ProviderOAuthCompletion, ProviderOAuthFuture,
};
use zuno_tool::{ToolDefinition, ToolOutput};
use zuno_tools::shell::{ShellEnvHook, ShellEnvInput};

pub(crate) struct PluginRuntime {
    load: Option<JsPluginLoad>,
    processes: Option<PluginLoad>,
    bus: HookBus,
    providers: RwLock<BTreeMap<String, ResolvedProvider>>,
    oauth_callbacks: Arc<Mutex<BTreeMap<(String, usize), AuthOAuthCallback>>>,
    reported_diagnostics: Mutex<BTreeSet<String>>,
    shutdown: AtomicBool,
}

impl PluginRuntime {
    /// The `auth` and `provider` resources are JavaScript-only.
    ///
    /// Not an oversight in the process tier: the Rust SDK rejects both hook names
    /// outright (`zuno-plugin-sdk/src/lib.rs:112`), and `JsonRpcPlugin` implements
    /// only `manifest`, `tools`, and `call`. A process plugin therefore cannot
    /// contribute a credential loader or a model list, and iterating it here would
    /// promise a capability the wire has no shape for.
    fn javascript_plugins(&self) -> impl Iterator<Item = &Arc<zuno_plugin::JsPlugin>> {
        self.load.iter().flat_map(JsPluginLoad::plugins)
    }
}

fn diagnostic_message(plugin: &str, hook: Option<&str>, message: &str) -> String {
    let hook = hook.map_or_else(|| "startup".to_owned(), |hook| format!("hook `{hook}`"));
    format!("disabled plugin `{plugin}` after {hook} failed: {message}")
}

pub(crate) struct PluginRuntimeTarget {
    kind: JsPluginKind,
    surface: &'static str,
    terminal: PluginRuntimeTerminal,
    server_url: Option<reqwest::Url>,
}

#[derive(Clone, Copy)]
enum PluginRuntimeTerminal {
    Reject,
    Stdio,
}

impl PluginRuntimeTarget {
    pub(crate) const fn server(surface: &'static str) -> Self {
        Self {
            kind: JsPluginKind::Server,
            surface,
            terminal: PluginRuntimeTerminal::Reject,
            server_url: None,
        }
    }

    pub(crate) fn server_with_stdio(surface: &'static str, server_url: reqwest::Url) -> Self {
        Self {
            kind: JsPluginKind::Server,
            surface,
            terminal: PluginRuntimeTerminal::Stdio,
            server_url: Some(server_url),
        }
    }

    pub(crate) const fn tui(surface: &'static str) -> Self {
        Self {
            kind: JsPluginKind::Tui,
            surface,
            terminal: PluginRuntimeTerminal::Reject,
            server_url: None,
        }
    }
}

/// Whether the JavaScript plugin host may start, and what decided it.
///
/// Reported rather than returned as a bare `bool` so `debug config` can say *why* a
/// user's plugins are not running. "Disabled" and "you never asked" look identical
/// from the outside, and telling them apart is the difference between a one-line fix
/// and a bug report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JsPluginPolicy {
    pub(crate) enabled: bool,
    pub(crate) source: JsPluginSource,
}

/// What decided whether the JavaScript plugin host may start.
///
/// An enum and not the reported string because one case carries behaviour:
/// [`Self::Pure`] is a caller who asked for no external plugins, and telling that
/// caller how to switch them on is noise on a path that was previously silent. A
/// string compare would express the same rule with none of the checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsPluginSource {
    /// `--pure`: an explicit request for no external plugins.
    Pure,
    Environment,
    Config,
    /// Nobody said anything either way, which means off.
    Default,
    #[cfg(test)]
    Test,
}

impl JsPluginSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::Environment => "environment",
            Self::Config => "config",
            Self::Default => "default",
            #[cfg(test)]
            Self::Test => "test",
        }
    }
}

impl JsPluginPolicy {
    /// Resolve the opt-in from `--pure`, the environment, then the configuration.
    ///
    /// `--pure` wins because it is the existing kill switch and a caller that asked
    /// for no external plugins must not get them from a config file it did not write.
    /// The environment beats the configuration so a one-off run needs no edit, which
    /// is the same precedence the CLI's other flags already have.
    pub(crate) fn resolve(config: &zuno_config::Config, env: &zuno_paths::Env) -> Self {
        if env.flag(crate::ZUNO_PURE) {
            return Self {
                enabled: false,
                source: JsPluginSource::Pure,
            };
        }
        if env.flag(crate::ZUNO_ENABLE_JS_PLUGINS) {
            return Self {
                enabled: true,
                source: JsPluginSource::Environment,
            };
        }
        if config.plugin_runtime.is_none() {
            return Self {
                enabled: false,
                source: JsPluginSource::Default,
            };
        }
        Self {
            enabled: config.javascript_plugins_enabled(),
            source: JsPluginSource::Config,
        }
    }

    /// Whether a caller with configured plugins deserves to be told they are idle.
    ///
    /// False under `--pure`: that caller asked for no external plugins, so naming the
    /// switch that would undo their own request is noise, on a path that printed
    /// nothing before the host became opt-in.
    pub(crate) fn should_explain_absence(self) -> bool {
        !self.enabled && self.source != JsPluginSource::Pure
    }
}

/// Whether out-of-process plugins found on disk may start, and what decided it.
///
/// A separate type from [`JsPluginPolicy`] because the two tiers answer to different
/// costs and must not share a switch. `jsonrpc.rs` is compiled into every binary with
/// no feature gate, and a discovered executable needs no runtime installed; the only
/// thing the two tiers genuinely share is `--pure`, which means "no external
/// plugins" for any tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessPluginPolicy {
    pub(crate) enabled: bool,
    pub(crate) source: ProcessPluginSource,
}

/// What decided whether out-of-process plugins may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessPluginSource {
    /// `--pure`: an explicit request for no external plugins, whatever the tier.
    Pure,
    Config,
    /// Nobody said anything, which for this tier means on.
    Default,
}

impl ProcessPluginPolicy {
    /// Honour `--pure`, then the configuration, and default to on.
    ///
    /// Reads `--pure` off the JavaScript policy rather than the environment a second
    /// time: [`JsPluginSource::Pure`] already *is* the record that the caller asked
    /// for no external plugins, and re-deriving it would let the two tiers disagree
    /// about whether `--pure` was passed.
    pub(crate) fn resolve(config: &zuno_config::Config, javascript: JsPluginPolicy) -> Self {
        if javascript.source == JsPluginSource::Pure {
            return Self {
                enabled: false,
                source: ProcessPluginSource::Pure,
            };
        }
        match config
            .plugin_runtime
            .as_ref()
            .and_then(|runtime| runtime.process)
        {
            Some(enabled) => Self {
                enabled,
                source: ProcessPluginSource::Config,
            },
            None => Self {
                enabled: config.process_plugins_enabled(),
                source: ProcessPluginSource::Default,
            },
        }
    }
}

impl PluginRuntime {
    pub(crate) async fn load(
        config: &zuno_config::Config,
        project: &zuno_paths::ResolvedProject,
        directory: &Path,
        worktree: &Path,
        layout: &zuno_paths::Layout,
        policy: JsPluginPolicy,
        target: PluginRuntimeTarget,
    ) -> Option<Self> {
        let process_policy = ProcessPluginPolicy::resolve(config, policy);
        // Before discovery, not after: scanning four directories and reading package
        // manifests is work a disabled runtime must not do either.
        let js_specs = if policy.enabled {
            let mut specs = configured_plugins(config, target.kind);
            specs.extend(auto_discovered_plugins(
                directory,
                worktree,
                layout,
                target.kind,
                target.surface,
            ));
            specs
        } else {
            Vec::new()
        };
        let process_specs = if process_policy.enabled {
            auto_discovered_process_plugins(directory, worktree, layout, target.surface)
        } else {
            Vec::new()
        };
        if js_specs.is_empty() && process_specs.is_empty() {
            return None;
        }

        let load = if js_specs.is_empty() {
            None
        } else {
            let terminal: Arc<dyn TerminalLease> = match target.terminal {
                PluginRuntimeTerminal::Reject => Arc::new(HeadlessTerminalLease),
                PluginRuntimeTerminal::Stdio => {
                    Arc::new(TerminalBroker::new(Arc::new(StdioTerminal)))
                }
            };
            let server_url = target.server_url.unwrap_or_else(|| {
                reqwest::Url::parse("http://127.0.0.1:0").expect("static plugin server URL")
            });
            let host = JsHostConfig::new(project.clone(), server_url, terminal)
                .directory(directory)
                .worktree(worktree)
                .cache_dir(layout.cache());
            Some(load_js_plugins_ordered(js_specs, host).await)
        };
        let processes = if process_specs.is_empty() {
            None
        } else {
            Some(load_plugins_ordered(process_specs).await)
        };

        for diagnostic in load.iter().flat_map(JsPluginLoad::diagnostics) {
            tracing::warn!(
                plugin = %diagnostic.plugin,
                hook = ?diagnostic.hook,
                kind = ?diagnostic.kind,
                message = %diagnostic.message,
                surface = target.surface,
                "JavaScript plugin did not fully load"
            );
        }
        for diagnostic in processes.iter().flat_map(PluginLoad::diagnostics) {
            tracing::warn!(
                plugin = %diagnostic.plugin,
                hook = ?diagnostic.hook,
                kind = ?diagnostic.kind,
                message = %diagnostic.message,
                surface = target.surface,
                "out-of-process plugin did not fully load"
            );
        }

        let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
        plugins.extend(
            load.iter()
                .flat_map(JsPluginLoad::plugins)
                .cloned()
                .map(|plugin| plugin as Arc<dyn Plugin>),
        );
        plugins.extend(
            processes
                .iter()
                .flat_map(PluginLoad::plugins)
                .cloned()
                .map(|plugin| plugin as Arc<dyn Plugin>),
        );
        Some(Self {
            load,
            processes,
            bus: HookBus::new(plugins),
            providers: RwLock::new(BTreeMap::new()),
            oauth_callbacks: Arc::new(Mutex::new(BTreeMap::new())),
            // Empty, not "everything so far": seeding this with the startup count is
            // what made a plugin that never started invisible to everyone but the log.
            // `turn` renders these as transcript notices and `models` prints them to
            // stderr, and a boot-time failure is exactly the one a user needs to see.
            reported_diagnostics: Mutex::new(BTreeSet::new()),
            shutdown: AtomicBool::new(false),
        })
    }

    pub(crate) fn take_diagnostics(&self) -> Vec<String> {
        let messages = self
            .load
            .iter()
            .flat_map(JsPluginLoad::diagnostics)
            .map(|diagnostic| {
                diagnostic_message(
                    &diagnostic.plugin,
                    diagnostic.hook.as_deref(),
                    &diagnostic.message,
                )
            })
            .chain(
                self.processes
                    .iter()
                    .flat_map(PluginLoad::diagnostics)
                    .map(|diagnostic| {
                        diagnostic_message(
                            &diagnostic.plugin,
                            diagnostic.hook.as_deref(),
                            &diagnostic.message,
                        )
                    }),
            );
        let Ok(mut reported) = self.reported_diagnostics.lock() else {
            return vec!["plugin diagnostic cursor lock was poisoned".to_owned()];
        };
        // Identity, not a running index. A plugin latches disabled after one failure,
        // so a repeated message is the same event; an index would misreport as soon as
        // an earlier plugin failed after a later one and shifted the list under it.
        messages
            .filter(|message| reported.insert(message.clone()))
            .collect()
    }

    pub(crate) async fn apply_config(
        &self,
        config: &mut zuno_config::Config,
    ) -> Result<(), String> {
        self.bus
            .dispatch(HookInvocation::Config { config })
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn apply_catalog(
        &self,
        catalog: &mut zuno_llm::catalog::Catalog,
        credentials: &BTreeMap<String, zuno_auth::Credential>,
    ) -> Result<(), String> {
        for plugin in self.javascript_plugins() {
            let Some(hook) = plugin.auth() else {
                continue;
            };
            let Some(loader) = hook.loader else {
                continue;
            };
            // The SDK promises `getAuth(): Promise<Auth>`, not `Promise<Auth | null>`.
            // Match upstream by skipping the loader when this provider has no stored
            // credential rather than sending a value its compiled type cannot express.
            let Some(credential) = credentials.get(&hook.provider).cloned() else {
                continue;
            };
            let Some(provider) = catalog.provider_mut(&hook.provider) else {
                continue;
            };
            let auth = StoredCredential(credential);
            match loader.load(&auth, provider).await {
                Ok(options) => provider.options.extend(options),
                Err(error) => {
                    plugin
                        .disable_after_callback_failure("auth.loader", &error)
                        .await;
                }
            }
        }

        for plugin in self.javascript_plugins() {
            let Some(hook) = plugin.provider() else {
                continue;
            };
            let Some(loader) = hook.models else {
                continue;
            };
            let Some(provider) = catalog.provider(&hook.id).cloned() else {
                continue;
            };
            match loader
                .models(
                    &provider,
                    ProviderHookContext {
                        auth: credentials.get(&hook.id),
                    },
                )
                .await
            {
                Ok(models) => {
                    catalog.replace_provider_models(&hook.id, models);
                }
                Err(error) => {
                    plugin
                        .disable_after_callback_failure("provider.models", &error)
                        .await;
                }
            }
        }
        *self
            .providers
            .write()
            .map_err(|_| "plugin catalog context lock was poisoned".to_owned())? =
            catalog.providers().clone();
        Ok(())
    }

    pub(crate) async fn tools(&self) -> Result<PluginTools, String> {
        let mut tools = PluginTools::new();
        self.bus
            .dispatch(HookInvocation::Tool { output: &mut tools })
            .await
            .map_err(|error| error.to_string())?;
        Ok(tools)
    }

    pub(crate) async fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(error) = self.bus.dispatch(HookInvocation::Dispose).await {
            tracing::warn!(%error, "plugin dispose hook failed");
        }
        if let Some(load) = &self.load {
            load.shutdown().await;
        }
        if let Some(processes) = &self.processes {
            processes.shutdown().await;
        }
    }

    pub(crate) async fn apply_chat_message(
        &self,
        input: &ChatMessageInput<'_>,
        output: &mut ChatMessageOutput,
    ) -> Result<(), String> {
        self.bus
            .dispatch(HookInvocation::ChatMessage { input, output })
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn apply_command(
        &self,
        command: &str,
        session_id: &str,
        arguments: &str,
        parts: &mut Vec<zuno_db::message::PartRecord>,
    ) -> Result<(), String> {
        let input = CommandExecuteBeforeInput {
            command,
            session_id,
            arguments,
        };
        let mut output = CommandExecuteBeforeOutput {
            parts: std::mem::take(parts),
        };
        self.bus
            .dispatch(HookInvocation::CommandExecuteBefore {
                input: &input,
                output: &mut output,
            })
            .await
            .map_err(|error| error.to_string())?;
        *parts = output.parts;
        Ok(())
    }

    pub(crate) async fn small_model(
        &self,
        provider: &ResolvedProvider,
    ) -> Result<Option<CatalogModel>, String> {
        let input = ProviderSmallModelInput { provider };
        let mut output = ProviderSmallModelOutput::default();
        self.bus
            .dispatch(HookInvocation::ProviderSmallModel {
                input: &input,
                output: &mut output,
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(output.model)
    }

    fn supports(&self, hook: HookName) -> bool {
        self.bus
            .plugins()
            .iter()
            .any(|plugin| plugin.manifest().supports(hook))
    }

    /// Each loaded plugin's id and the hooks its manifest claims.
    ///
    /// The hooks, not a version: the loaded manifest has no version field, and the version
    /// in an npm install spec is the version that was *asked* for — the two diverge exactly
    /// when a status panel is being read. Which hooks a plugin claims is also the fact that
    /// explains why it is or is not running.
    pub(crate) fn census(&self) -> Vec<(String, Vec<&'static str>)> {
        self.bus
            .plugins()
            .iter()
            .map(|plugin| {
                let manifest = plugin.manifest();
                (
                    manifest.id().to_owned(),
                    manifest
                        .hooks()
                        .iter()
                        .copied()
                        .map(HookName::as_str)
                        .collect(),
                )
            })
            .collect()
    }

    fn catalog_context(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<(ResolvedProvider, CatalogModel), String> {
        let providers = self
            .providers
            .read()
            .map_err(|_| "plugin catalog context lock was poisoned".to_owned())?;
        let provider = providers.get(provider_id).cloned().ok_or_else(|| {
            format!("plugin hook provider context `{provider_id}` is unavailable")
        })?;
        let model = provider
            .models
            .get(model_id)
            .or_else(|| {
                provider
                    .models
                    .values()
                    .find(|model| model.api.id == model_id)
            })
            .cloned()
            .ok_or_else(|| {
                format!("plugin hook model context `{provider_id}/{model_id}` is unavailable")
            })?;
        Ok((provider, model))
    }
}

impl std::fmt::Debug for PluginRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginRuntime")
            .field("plugins", &self.bus.plugins().len())
            .finish_non_exhaustive()
    }
}

impl ProviderOAuthBackend for PluginRuntime {
    fn authorize(
        &self,
        request: ProviderOAuthAuthorizeRequest,
    ) -> ProviderOAuthFuture<ProviderOAuthAuthorization> {
        let authorization = self
            .bus
            .plugins()
            .iter()
            .filter_map(|plugin| plugin.auth())
            .find(|hook| hook.provider == request.provider_id)
            .and_then(|hook| hook.methods.get(request.method).cloned());
        let callbacks = Arc::clone(&self.oauth_callbacks);
        Box::pin(async move {
            let AuthMethod::OAuth { authorize, .. } = authorization.ok_or_else(|| {
                format!(
                    "plugin provider `{}` has no OAuth method {}",
                    request.provider_id, request.method
                )
            })?
            else {
                return Err(format!(
                    "plugin provider `{}` method {} is not OAuth",
                    request.provider_id, request.method
                ));
            };
            let result = authorize
                .authorize((!request.inputs.is_empty()).then_some(&request.inputs))
                .await
                .map_err(|error| error.to_string())?;
            let method = match &result.callback {
                AuthOAuthCallback::Auto(_) => "auto",
                AuthOAuthCallback::Code(_) => "code",
            };
            callbacks
                .lock()
                .map_err(|_| "plugin OAuth callback lock was poisoned".to_owned())?
                .insert((request.provider_id, request.method), result.callback);
            Ok(ProviderOAuthAuthorization {
                url: result.url,
                method: method.to_owned(),
                instructions: result.instructions,
            })
        })
    }

    fn callback(
        &self,
        request: ProviderOAuthCallbackRequest,
    ) -> ProviderOAuthFuture<Option<ProviderOAuthCompletion>> {
        let callback = self
            .oauth_callbacks
            .lock()
            .map_err(|_| "plugin OAuth callback lock was poisoned".to_owned())
            .and_then(|mut callbacks| {
                callbacks
                    .remove(&(request.provider_id.clone(), request.method))
                    .ok_or_else(|| {
                        format!(
                            "plugin provider `{}` method {} has no active OAuth callback",
                            request.provider_id, request.method
                        )
                    })
            });
        Box::pin(async move {
            let result = match callback? {
                AuthOAuthCallback::Auto(callback) => callback.callback().await,
                AuthOAuthCallback::Code(callback) => {
                    let code = request.code.as_deref().ok_or_else(|| {
                        format!(
                            "plugin provider `{}` method {} requires an OAuth code",
                            request.provider_id, request.method
                        )
                    })?;
                    callback.callback(code).await
                }
            }
            .map_err(|error| error.to_string())?;
            Ok(match result {
                AuthCallbackResult::Failed => None,
                AuthCallbackResult::Success(success) => Some(oauth_completion(success)),
            })
        })
    }
}

fn oauth_completion(success: AuthSuccess) -> ProviderOAuthCompletion {
    match success {
        AuthSuccess::OAuth {
            provider,
            refresh,
            access,
            expires,
            account_id,
            enterprise_url,
        } => ProviderOAuthCompletion {
            provider_id: provider,
            credential: zuno_auth::Credential::Oauth {
                refresh,
                access,
                expires,
                account_id,
                enterprise_url,
            },
        },
        AuthSuccess::ApiKey {
            provider,
            key,
            metadata,
        } => ProviderOAuthCompletion {
            provider_id: provider,
            credential: zuno_auth::Credential::Api { key, metadata },
        },
    }
}

#[async_trait]
impl TurnHooks for PluginRuntime {
    fn enabled(&self) -> bool {
        !self.bus.plugins().is_empty()
    }

    async fn event(&self, event: &zuno_engine::r#loop::TurnEvent) -> Result<(), String> {
        self.bus
            .dispatch(HookInvocation::Event { event })
            .await
            .map_err(|error| error.to_string())
    }

    async fn transform_messages(
        &self,
        _session_id: &str,
        messages: &mut Vec<HookMessageWithParts>,
    ) -> Result<(), String> {
        let mut output = ChatMessagesTransformOutput {
            messages: messages
                .iter()
                .map(|message| MessageWithParts {
                    info: message.info.clone(),
                    parts: message.parts.clone(),
                })
                .collect(),
        };
        self.bus
            .dispatch(HookInvocation::ChatMessagesTransform {
                output: &mut output,
            })
            .await
            .map_err(|error| error.to_string())?;
        *messages = output
            .messages
            .into_iter()
            .map(|message| HookMessageWithParts {
                info: message.info,
                parts: message.parts,
            })
            .collect();
        Ok(())
    }

    async fn transform_system(
        &self,
        session_id: &str,
        model: &zuno_engine::r#loop::ResolvedModel,
        system: &mut Vec<String>,
    ) -> Result<(), String> {
        let (_, catalog_model) =
            self.catalog_context(&model.catalog_provider_id, &model.catalog_model_id)?;
        let input = ChatSystemTransformInput {
            session_id: Some(session_id),
            model: &catalog_model,
        };
        let mut output = ChatSystemTransformOutput {
            system: std::mem::take(system),
        };
        self.bus
            .dispatch(HookInvocation::ChatSystemTransform {
                input: &input,
                output: &mut output,
            })
            .await
            .map_err(|error| error.to_string())?;
        *system = output.system;
        Ok(())
    }

    async fn tool_definition(&self, definition: &mut ToolDefinition) -> Result<(), String> {
        let tool_id = definition.id.clone();
        self.bus
            .dispatch(HookInvocation::ToolDefinition {
                input: &ToolDefinitionInput { tool_id: &tool_id },
                output: definition,
            })
            .await
            .map_err(|error| error.to_string())
    }

    async fn prepare_request(
        &self,
        input: RequestHookInput<'_>,
        request: &mut CompletionRequest,
    ) -> Result<(), String> {
        let (provider, model) = self.catalog_context(
            &input.model.catalog_provider_id,
            &input.model.catalog_model_id,
        )?;
        let provider_context = ProviderContext {
            source: provider_source(&provider),
            options: provider.options.clone(),
            info: provider,
        };
        let context = ChatContext {
            session_id: input.session_id,
            agent: &input.agent.name,
            model: &model,
            provider: &provider_context,
            message: input.message.clone(),
        };

        if self.supports(HookName::ChatParams) {
            let mut base_options = input
                .model
                .provider
                .options
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Map<String, Value>>();
            base_options.extend(model.options.clone());
            base_options.extend(request.parameters.clone());
            let mut output = ChatParamsOutput {
                temperature: number_option(&base_options, &["temperature"]).unwrap_or_default(),
                top_p: number_option(&base_options, &["topP", "top_p"]).unwrap_or_default(),
                top_k: number_option(&base_options, &["topK", "top_k"]).unwrap_or_default(),
                max_output_tokens: integer_option(
                    &base_options,
                    &["maxOutputTokens", "max_output_tokens"],
                ),
                options: base_options,
            };
            self.bus
                .dispatch(HookInvocation::ChatParams {
                    input: &context,
                    output: &mut output,
                })
                .await
                .map_err(|error| error.to_string())?;
            request.parameters = output.options;
            request
                .parameters
                .insert("temperature".to_owned(), Value::from(output.temperature));
            request
                .parameters
                .insert("top_p".to_owned(), Value::from(output.top_p));
            request
                .parameters
                .insert("top_k".to_owned(), Value::from(output.top_k));
            if let Some(tokens) = output.max_output_tokens {
                request
                    .parameters
                    .insert("max_output_tokens".to_owned(), Value::from(tokens));
            }
        }

        if self.supports(HookName::ChatHeaders) {
            let mut output = ChatHeadersOutput {
                headers: request.headers.clone(),
            };
            self.bus
                .dispatch(HookInvocation::ChatHeaders {
                    input: &context,
                    output: &mut output,
                })
                .await
                .map_err(|error| error.to_string())?;
            request.headers = output.headers;
        }
        Ok(())
    }

    async fn text_complete(
        &self,
        session_id: &str,
        message_id: &str,
        part_id: &str,
        text: &mut String,
    ) -> Result<(), String> {
        let input = TextCompleteInput {
            session_id,
            message_id,
            part_id,
        };
        let mut output = TextCompleteOutput {
            text: std::mem::take(text),
        };
        self.bus
            .dispatch(HookInvocation::TextComplete {
                input: &input,
                output: &mut output,
            })
            .await
            .map_err(|error| error.to_string())?;
        *text = output.text;
        Ok(())
    }
}

#[async_trait]
impl ToolHooks for PluginRuntime {
    async fn before(
        &self,
        tool: &str,
        session_id: &str,
        call_id: &str,
        args: &mut Value,
    ) -> Result<(), String> {
        let input = ToolExecuteBeforeInput {
            tool,
            session_id,
            call_id,
        };
        let mut output = ToolExecuteBeforeOutput {
            args: std::mem::take(args),
        };
        self.bus
            .dispatch(HookInvocation::ToolExecuteBefore {
                input: &input,
                output: &mut output,
            })
            .await
            .map_err(|error| error.to_string())?;
        *args = output.args;
        Ok(())
    }

    async fn permission(
        &self,
        request: &PermissionRequest,
    ) -> Result<PermissionHookDecision, String> {
        let input = PermissionAskInput { request };
        let mut output = PermissionAskOutput::default();
        self.bus
            .dispatch(HookInvocation::PermissionAsk {
                input: &input,
                output: &mut output,
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(match output.status() {
            PermissionStatus::Ask => PermissionHookDecision::Ask,
            PermissionStatus::Deny => PermissionHookDecision::Deny,
            PermissionStatus::Allow => PermissionHookDecision::Allow,
        })
    }

    async fn after(
        &self,
        tool: &str,
        session_id: &str,
        call_id: &str,
        args: &Value,
        output: &mut ToolOutput,
    ) -> Result<(), String> {
        let input = ToolExecuteAfterInput {
            tool,
            session_id,
            call_id,
            args,
        };
        self.bus
            .dispatch(HookInvocation::ToolExecuteAfter {
                input: &input,
                output,
            })
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl ShellEnvHook for PluginRuntime {
    async fn env(
        &self,
        input: ShellEnvInput,
    ) -> Result<BTreeMap<String, String>, zuno_error::ToolError> {
        let cwd = input.cwd.to_string_lossy().into_owned();
        let plugin_input = PluginShellEnvInput {
            cwd: &cwd,
            session_id: Some(&input.session_id),
            call_id: Some(&input.call_id),
        };
        let mut output = ShellEnvOutput::default();
        self.bus
            .dispatch(HookInvocation::ShellEnv {
                input: &plugin_input,
                output: &mut output,
            })
            .await
            .map_err(|error| zuno_error::ToolError::Failed {
                tool: "bash".to_owned(),
                source: Box::new(io::Error::other(error.to_string())),
            })?;
        Ok(output.env)
    }
}

#[async_trait]
impl CompactionHooks for PluginRuntime {
    async fn compacting(
        &self,
        input: &CompactionHookInput<'_>,
        output: &mut CompactionPrompt,
    ) -> Result<(), String> {
        let plugin_input = SessionCompactingInput {
            session_id: input.session_id,
        };
        let mut plugin_output = SessionCompactingOutput {
            context: std::mem::take(&mut output.context),
            prompt: output.prompt.take(),
        };
        self.bus
            .dispatch(HookInvocation::SessionCompacting {
                input: &plugin_input,
                output: &mut plugin_output,
            })
            .await
            .map_err(|error| error.to_string())?;
        output.context = plugin_output.context;
        output.prompt = plugin_output.prompt;
        Ok(())
    }

    async fn auto_continue(&self, input: &AutoContinueHookInput<'_>) -> Result<bool, String> {
        let (provider, model) = self.catalog_context(input.provider_id, input.model_id)?;
        let provider_context = ProviderContext {
            source: provider_source(&provider),
            options: provider.options.clone(),
            info: provider,
        };
        let context = ChatContext {
            session_id: input.session_id,
            agent: input.agent,
            model: &model,
            provider: &provider_context,
            message: input.message.clone(),
        };
        let plugin_input = CompactionAutocontinueInput {
            context: &context,
            overflow: input.overflow,
        };
        let mut output = CompactionAutocontinueOutput { enabled: true };
        self.bus
            .dispatch(HookInvocation::CompactionAutocontinue {
                input: &plugin_input,
                output: &mut output,
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(output.enabled)
    }
}

fn provider_source(provider: &ResolvedProvider) -> ProviderSource {
    match provider.availability.effective_source() {
        Some(AvailabilitySource::EnvVar { .. }) => ProviderSource::Env,
        Some(AvailabilitySource::ConfigBlock) => ProviderSource::Config,
        Some(AvailabilitySource::StoredApiKey) => ProviderSource::Api,
        Some(AvailabilitySource::StoredOauth | AvailabilitySource::StoredWellKnown) | None => {
            ProviderSource::Custom
        }
    }
}

fn number_option(options: &Map<String, Value>, names: &[&str]) -> Option<f64> {
    names
        .iter()
        .find_map(|name| options.get(*name).and_then(Value::as_f64))
}

fn integer_option(options: &Map<String, Value>, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| options.get(*name).and_then(Value::as_u64))
}

fn configured_plugins(config: &zuno_config::Config, kind: JsPluginKind) -> Vec<JsPluginSpec> {
    config
        .plugin
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|plugin| js_plugin_spec(plugin, kind))
        .collect()
}

/// Turn every executable in a scanned directory into a spawnable process spec.
///
/// The missing half of the out-of-process tier. `jsonrpc.rs` has been complete and
/// compiled into every binary since it landed, but nothing ever built a
/// [`PluginProcessSpec`], so no user could reach it. This is that constructor.
fn auto_discovered_process_plugins(
    directory: &Path,
    worktree: &Path,
    layout: &zuno_paths::Layout,
    surface: &str,
) -> Vec<PluginProcessSpec> {
    let directories = layout.config_directories(directory, Some(worktree));
    let mut specs = Vec::new();
    for config_directory in directories {
        let scope = if config_directory.starts_with(worktree) {
            PluginScope::Local
        } else {
            PluginScope::Global
        };
        let discovered = match discover_process_plugins(&[ConfigDirectory::new(
            config_directory.as_path(),
            scope,
        )]) {
            Ok(discovered) => discovered,
            Err(error) => {
                tracing::warn!(
                    %error,
                    directory = %config_directory.display(),
                    surface,
                    "failed to auto-discover out-of-process plugins"
                );
                continue;
            }
        };
        for plugin in discovered {
            tracing::debug!(
                plugin = %plugin.name,
                program = %plugin.program.display(),
                scope = ?plugin.scope,
                surface,
                "auto-discovered out-of-process plugin"
            );
            specs.push(PluginProcessSpec::new(plugin.name, plugin.program).current_dir(directory));
        }
    }
    specs
}

fn auto_discovered_plugins(
    directory: &Path,
    worktree: &Path,
    layout: &zuno_paths::Layout,
    kind: JsPluginKind,
    surface: &str,
) -> Vec<JsPluginSpec> {
    let directories = layout.config_directories(directory, Some(worktree));
    let mut specs = Vec::new();
    for directory in directories {
        let scope = if directory.starts_with(worktree) {
            PluginScope::Local
        } else {
            PluginScope::Global
        };
        let discovered =
            match discover_plugins(&[], &[ConfigDirectory::new(directory.as_path(), scope)]) {
                Ok(discovered) => discovered,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        directory = %directory.display(),
                        surface,
                        "failed to auto-discover JavaScript plugins"
                    );
                    continue;
                }
            };
        for plugin in discovered {
            let PluginOrigin::AutoDiscovered { source, scope } = &plugin.origin else {
                continue;
            };
            tracing::debug!(
                plugin = plugin.spec.name(),
                source = %source.display(),
                ?scope,
                surface,
                "auto-discovered JavaScript plugin"
            );
            specs.push(js_plugin_spec(&plugin.spec, kind));
        }
    }
    specs
}

fn js_plugin_spec(
    plugin: &zuno_config::schema::plugin::PluginSpec,
    kind: JsPluginKind,
) -> JsPluginSpec {
    let spec = JsPluginSpec::new(plugin.name()).with_kind(kind);
    match plugin.options() {
        Some(options) => spec.options(serde_json::Value::Object(options.clone())),
        None => spec,
    }
}

struct StoredCredential(zuno_auth::Credential);

#[async_trait]
impl AuthCredentialResolver for StoredCredential {
    async fn resolve(&self) -> Result<Option<zuno_auth::Credential>, zuno_error::BoxSource> {
        Ok(Some(self.0.clone()))
    }
}

struct HeadlessTerminalLease;

#[async_trait]
impl TerminalLease for HeadlessTerminalLease {
    async fn acquire(&self, reason: LeaseReason) -> Result<TerminalLeaseGuard, TerminalLeaseError> {
        Err(TerminalLeaseError::Unavailable {
            requested_by: reason.plugin,
            detail: "this headless command cannot host an interactive plugin prompt".to_owned(),
        })
    }

    async fn acquire_with_cleanup(
        &self,
        reason: LeaseReason,
        _cleanup: Arc<dyn zuno_engine::terminal_lease::TerminalLeaseCleanup>,
    ) -> Result<TerminalLeaseGuard, TerminalLeaseError> {
        self.acquire(reason).await
    }
}

struct StdioTerminal;

#[async_trait]
impl TerminalOwner for StdioTerminal {
    async fn yield_terminal(&self, _reason: &LeaseReason) -> Result<(), String> {
        Ok(())
    }

    fn reclaim_terminal(&self, _reason: &LeaseReason, _cause: ReclaimCause) {}
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use serde_json::json;
    use zuno_auth::{Credential, Secret};
    use zuno_config::schema::CompactionConfig;
    use zuno_engine::compaction::{
        CompactedTranscript, CompactionCache, CompactionOutcome, CompactionRequest,
        CompactionState, CompactionTrigger, TokenWindow, TranscriptEntry, run_compaction,
    };
    use zuno_llm::cache::{CacheTracker, LockedTools};
    use zuno_llm::catalog::availability::Availability;
    use zuno_llm::catalog::models_dev::CatalogStatus;
    use zuno_llm::catalog::resolved::{
        ModelApi, ModelCapabilities, ModelCost, ModelLimit, ResolvedModel, ResolvedProvider,
    };
    use zuno_llm::catalog::{Catalog, ResolveInput};
    use zuno_llm::event::{Message, Role};
    use zuno_llm::registry::Spec;
    use zuno_plugin::{HookBus, HookInvocation, HookName, Plugin, PluginManifest};
    use zuno_provider_compatible::{CompatibleProvider, ReqwestTransport, Transport};
    use zuno_server::{
        ProviderOAuthAuthorizeRequest, ProviderOAuthBackend, ProviderOAuthCallbackRequest,
    };
    use zuno_testkit::{MockProvider, Scenario, ScriptedEnv};
    use zuno_tools::shell::{ShellEnvHook as _, ShellEnvInput};

    use super::{
        ChatContext, ChatHeadersOutput, ChatMessageInput, ChatMessageOutput,
        ChatMessagesTransformOutput, ChatParamsOutput, ChatSystemTransformInput,
        ChatSystemTransformOutput, CommandExecuteBeforeInput, CommandExecuteBeforeOutput,
        CompactionAutocontinueInput, CompactionAutocontinueOutput, MessageWithParts,
        PermissionAskInput, PermissionAskOutput, PermissionRequest, PluginRuntime,
        PluginShellEnvInput, PluginTools, ProviderContext, ProviderSmallModelInput,
        ProviderSmallModelOutput, ProviderSource, SessionCompactingInput, SessionCompactingOutput,
        ShellEnvOutput, TextCompleteInput, TextCompleteOutput, ToolDefinition, ToolDefinitionInput,
        ToolExecuteAfterInput, ToolExecuteBeforeInput, ToolExecuteBeforeOutput, ToolOutput,
    };
    use super::{JsPluginPolicy, JsPluginSource, ProcessPluginPolicy, ProcessPluginSource};

    fn config_with_javascript(javascript: Option<bool>) -> zuno_config::Config {
        zuno_config::Config {
            plugin_runtime: javascript.map(|javascript| zuno_config::schema::PluginRuntimeConfig {
                javascript: Some(javascript),
                process: None,
            }),
            ..zuno_config::Config::default()
        }
    }

    /// An absent key means off, which is the whole of the opt-in.
    ///
    /// Pinned apart from the precedence cases because it is the one a user reaches
    /// without doing anything: each JavaScript plugin spawns a Node process, measured
    /// at 1.4s of a 1.47s `models` run, so silence has to mean "do not pay that".
    #[test]
    fn an_absent_plugin_runtime_key_leaves_javascript_off() {
        let policy =
            JsPluginPolicy::resolve(&config_with_javascript(None), &zuno_paths::Env::empty());

        assert!(!policy.enabled, "no request must mean no Node process");
        assert_eq!(policy.source, JsPluginSource::Default);
    }

    /// `--pure` outranks both opt-ins, and says nothing about undoing itself.
    ///
    /// The silence is the assertion that matters: `--pure` printed nothing before the
    /// host became opt-in, and offering that caller the switch they just declined
    /// would be a behaviour regression dressed up as helpfulness.
    #[test]
    fn pure_refuses_the_host_even_when_the_environment_and_config_ask_for_it() {
        let env = zuno_paths::Env::empty()
            .with(crate::ZUNO_PURE, "1")
            .with(crate::ZUNO_ENABLE_JS_PLUGINS, "1");

        let policy = JsPluginPolicy::resolve(&config_with_javascript(Some(true)), &env);

        assert!(!policy.enabled, "the existing kill switch must still kill");
        assert_eq!(policy.source, JsPluginSource::Pure);
        assert!(
            !policy.should_explain_absence(),
            "`--pure` must stay as quiet as it was before the host became opt-in"
        );
    }

    /// The environment beats the configuration, so a one-off run needs no file edit.
    #[test]
    fn the_environment_opt_in_overrides_a_config_that_disables_the_host() {
        let env = zuno_paths::Env::empty().with(crate::ZUNO_ENABLE_JS_PLUGINS, "1");

        let policy = JsPluginPolicy::resolve(&config_with_javascript(Some(false)), &env);

        assert!(policy.enabled);
        assert_eq!(policy.source, JsPluginSource::Environment);
    }

    /// A configured setting is honoured either way, and reported as a choice.
    ///
    /// Both directions in one test because the distinction that matters is `source`:
    /// `Config` and `Default` mean the same thing to the loader but different things
    /// to a user reading `debug config`, and only one of them names a key to change.
    #[test]
    fn a_configured_javascript_setting_is_honoured_in_both_directions() {
        let enabled = JsPluginPolicy::resolve(
            &config_with_javascript(Some(true)),
            &zuno_paths::Env::empty(),
        );
        assert!(enabled.enabled);
        assert_eq!(enabled.source, JsPluginSource::Config);

        let disabled = JsPluginPolicy::resolve(
            &config_with_javascript(Some(false)),
            &zuno_paths::Env::empty(),
        );
        assert!(!disabled.enabled);
        assert_eq!(disabled.source, JsPluginSource::Config);
        assert!(
            disabled.should_explain_absence(),
            "a config that turned the host off is still worth naming: the user may \
             have inherited the file rather than written it"
        );
    }

    const COMPACTION_PLUGIN: &str = r#"
export default {
  id: "production-compaction-fixture",
  server: async () => ({
    "experimental.session.compacting": async (_input, output) => {
      output.prompt = "plugin-compaction-prompt";
    },
    "experimental.compaction.autocontinue": async (_input, output) => {
      output.enabled = false;
    },
  }),
};
"#;

    const OAUTH_PLUGIN: &str = r#"
export default {
  id: "production-oauth-fixture",
  server: async () => ({
    auth: {
      provider: "kiro-auth",
      methods: [{
        type: "oauth",
        label: "Fixture OAuth",
        authorize: async () => ({
          url: "https://device.example.test/authorize",
          instructions: "complete fixture authorization",
          method: "auto",
          callback: async () => ({
            type: "success",
            provider: "kiro-auth",
            refresh: "fixture-refresh",
            access: "fixture-access",
            expires: 1234
          })
        })
      }]
    }
  })
};
"#;

    const AUTH_LOADER_PLUGIN: &str = r#"
import { appendFileSync } from "node:fs";

export default {
  id: "production-auth-loader-fixture",
  server: async (_input, options) => ({
    auth: {
      provider: "groq",
      loader: async (getAuth) => {
        const auth = await getAuth();
        appendFileSync(options.callLog, `${JSON.stringify(auth)}\n`);
        if (options.alwaysFail || auth === null) {
          throw new Error(options.alwaysFail
            ? "task173 authenticated loader failure"
            : "task173 configured provider has no stored auth");
        }
        return {};
      },
      methods: [],
    },
  }),
};
"#;

    const PROVIDER_MODELS_PLUGIN: &str = r#"
export default {
  id: "production-provider-models-fixture",
  server: async () => ({
    provider: {
      id: "groq",
      models: async () => {
        throw new Error("task173 provider model loader failure");
      },
    },
  }),
};
"#;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Surface {
        Run,
        Models,
        Tui,
        Http,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FailureBoundary {
        HookBus,
        ResourceCallback,
        ToolResult,
        Shutdown,
        NotApplicable,
        Fatal,
    }

    const SURFACES: [Surface; 4] = [Surface::Run, Surface::Models, Surface::Tui, Surface::Http];

    const fn failure_boundary(surface: Surface, hook: zuno_plugin::HookName) -> FailureBoundary {
        match (surface, hook) {
            (Surface::Models, zuno_plugin::HookName::Dispose) => FailureBoundary::Shutdown,
            (Surface::Models, zuno_plugin::HookName::Config) => FailureBoundary::HookBus,
            (Surface::Models, zuno_plugin::HookName::Auth | zuno_plugin::HookName::Provider) => {
                FailureBoundary::ResourceCallback
            }
            (Surface::Models, _) => FailureBoundary::NotApplicable,
            (Surface::Run | Surface::Tui | Surface::Http, zuno_plugin::HookName::Dispose) => {
                FailureBoundary::Shutdown
            }
            (Surface::Run | Surface::Tui | Surface::Http, zuno_plugin::HookName::Tool) => {
                FailureBoundary::ToolResult
            }
            (
                Surface::Run | Surface::Tui | Surface::Http,
                zuno_plugin::HookName::Auth | zuno_plugin::HookName::Provider,
            ) => FailureBoundary::ResourceCallback,
            (Surface::Run | Surface::Tui | Surface::Http, _) => FailureBoundary::HookBus,
        }
    }

    struct DisablingFailurePlugin {
        manifest: PluginManifest,
        disabled: AtomicBool,
    }

    impl DisablingFailurePlugin {
        fn new(hook: HookName) -> Self {
            Self {
                manifest: PluginManifest::new(format!("failing-{}", hook.as_str()), vec![hook])
                    .expect("single-hook failure manifest"),
                disabled: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl Plugin for DisablingFailurePlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        fn is_disabled(&self) -> bool {
            self.disabled.load(Ordering::Acquire)
        }

        async fn call(&self, hook: &mut HookInvocation<'_>) -> Result<(), zuno_error::BoxSource> {
            self.disabled.store(true, Ordering::Release);
            Err(Box::new(std::io::Error::other(format!(
                "{} fixture failure",
                hook.name()
            ))))
        }
    }

    async fn dispatch_failure_fixture(
        bus: &HookBus,
        hook: HookName,
    ) -> Result<(), zuno_error::PluginError> {
        let provider = catalog_provider();
        let model = provider
            .models
            .get("small-model")
            .expect("fixture model")
            .clone();
        let provider_context = ProviderContext {
            source: ProviderSource::Config,
            options: serde_json::Map::new(),
            info: provider.clone(),
        };
        let chat_context = ChatContext {
            session_id: "ses",
            agent: "build",
            model: &model,
            provider: &provider_context,
            message: Message::new(Role::User, "hello"),
        };

        match hook {
            HookName::Dispose => bus.dispatch(HookInvocation::Dispose).await,
            HookName::Event => {
                let event = zuno_engine::r#loop::TurnEvent::TurnStarted {
                    session_id: "ses".to_owned(),
                };
                bus.dispatch(HookInvocation::Event { event: &event }).await
            }
            HookName::Config => {
                let mut config = zuno_config::Config::default();
                bus.dispatch(HookInvocation::Config {
                    config: &mut config,
                })
                .await
            }
            HookName::Tool => {
                let mut output = PluginTools::new();
                bus.dispatch(HookInvocation::Tool {
                    output: &mut output,
                })
                .await
            }
            HookName::Auth => {
                let mut output = Vec::new();
                bus.dispatch(HookInvocation::Auth {
                    output: &mut output,
                })
                .await
            }
            HookName::Provider => {
                let mut output = Vec::new();
                bus.dispatch(HookInvocation::Provider {
                    output: &mut output,
                })
                .await
            }
            HookName::ChatMessage => {
                let input = ChatMessageInput {
                    session_id: "ses",
                    agent: Some("build"),
                    model: None,
                    message_id: Some("message"),
                    variant: None,
                };
                let mut output = ChatMessageOutput {
                    message: zuno_db::message::MessageRecord::from_json(json!({
                        "id": "message",
                        "sessionID": "ses",
                        "role": "user",
                        "time": {"created": 1},
                        "agent": "build",
                        "model": {"providerID": "groq", "modelID": "small-model"}
                    }))
                    .expect("fixture user message"),
                    parts: Vec::new(),
                };
                bus.dispatch(HookInvocation::ChatMessage {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::ChatParams => {
                let mut output = ChatParamsOutput::default();
                bus.dispatch(HookInvocation::ChatParams {
                    input: &chat_context,
                    output: &mut output,
                })
                .await
            }
            HookName::ChatHeaders => {
                let mut output = ChatHeadersOutput::default();
                bus.dispatch(HookInvocation::ChatHeaders {
                    input: &chat_context,
                    output: &mut output,
                })
                .await
            }
            HookName::PermissionAsk => {
                let request = PermissionRequest {
                    id: "permission".to_owned(),
                    session_id: "ses".to_owned(),
                    permission: "read".to_owned(),
                    patterns: vec!["*".to_owned()],
                    metadata: serde_json::Map::new(),
                    always: Vec::new(),
                    tool: None,
                };
                let input = PermissionAskInput { request: &request };
                let mut output = PermissionAskOutput::default();
                bus.dispatch(HookInvocation::PermissionAsk {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::CommandExecuteBefore => {
                let input = CommandExecuteBeforeInput {
                    command: "build",
                    session_id: "ses",
                    arguments: "--release",
                };
                let mut output = CommandExecuteBeforeOutput::default();
                bus.dispatch(HookInvocation::CommandExecuteBefore {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::ToolExecuteBefore => {
                let input = ToolExecuteBeforeInput {
                    tool: "echo",
                    session_id: "ses",
                    call_id: "call",
                };
                let mut output = ToolExecuteBeforeOutput {
                    args: json!({"value": 1}),
                };
                bus.dispatch(HookInvocation::ToolExecuteBefore {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::ShellEnv => {
                let input = PluginShellEnvInput {
                    cwd: "/work",
                    session_id: Some("ses"),
                    call_id: Some("call"),
                };
                let mut output = ShellEnvOutput::default();
                bus.dispatch(HookInvocation::ShellEnv {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::ToolExecuteAfter => {
                let args = json!({"value": 1});
                let input = ToolExecuteAfterInput {
                    tool: "echo",
                    session_id: "ses",
                    call_id: "call",
                    args: &args,
                };
                let mut output = ToolOutput::text("echo", "ok");
                bus.dispatch(HookInvocation::ToolExecuteAfter {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::ChatMessagesTransform => {
                let mut output = ChatMessagesTransformOutput {
                    messages: vec![MessageWithParts {
                        info: Message::new(Role::User, "hello"),
                        parts: Vec::new(),
                    }],
                };
                bus.dispatch(HookInvocation::ChatMessagesTransform {
                    output: &mut output,
                })
                .await
            }
            HookName::ChatSystemTransform => {
                let input = ChatSystemTransformInput {
                    session_id: Some("ses"),
                    model: &model,
                };
                let mut output = ChatSystemTransformOutput::default();
                bus.dispatch(HookInvocation::ChatSystemTransform {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::ProviderSmallModel => {
                let input = ProviderSmallModelInput {
                    provider: &provider,
                };
                let mut output = ProviderSmallModelOutput::default();
                bus.dispatch(HookInvocation::ProviderSmallModel {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::SessionCompacting => {
                let input = SessionCompactingInput { session_id: "ses" };
                let mut output = SessionCompactingOutput::default();
                bus.dispatch(HookInvocation::SessionCompacting {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::CompactionAutocontinue => {
                let input = CompactionAutocontinueInput {
                    context: &chat_context,
                    overflow: true,
                };
                let mut output = CompactionAutocontinueOutput { enabled: true };
                bus.dispatch(HookInvocation::CompactionAutocontinue {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::TextComplete => {
                let input = TextCompleteInput {
                    session_id: "ses",
                    message_id: "message",
                    part_id: "part",
                };
                let mut output = TextCompleteOutput {
                    text: "done".to_owned(),
                };
                bus.dispatch(HookInvocation::TextComplete {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
            HookName::ToolDefinition => {
                let input = ToolDefinitionInput { tool_id: "echo" };
                let mut output = ToolDefinition {
                    id: "echo".to_owned(),
                    description: "echo".to_owned(),
                    parameters: json!({"type": "object"}),
                };
                bus.dispatch(HookInvocation::ToolDefinition {
                    input: &input,
                    output: &mut output,
                })
                .await
            }
        }
    }

    /// A language-agnostic plugin: POSIX `sh`, no SDK, no compiler.
    ///
    /// Deliberately not Rust. The claim under test is that the process tier is
    /// reachable by *dropping an executable in a directory*, and a fixture that
    /// needed this workspace's own SDK to build could not distinguish that claim
    /// from "Rust plugins work".
    ///
    /// `tr ',{}'` splits the frame so `"id":N` lands on its own line: `serde_json`
    /// has no `preserve_order` here, so keys arrive alphabetically and a greedy
    /// `.*"id":` would bind to whichever `"id":` came last.
    #[cfg(unix)]
    const SH_PLUGIN: &str = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | tr ',{}' '\n\n\n' | sed -n 's/^"id":\([0-9]*\)$/\1/p' | head -n1)
  case "$line" in
    *plugin.initialize*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"1.0","plugin":{"id":"sh-example","hooks":["shell.env"],"tools":[]}}}\n' "$id"
      ;;
    *hook.call*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"output":{"env":{"SH_PLUGIN":"enabled"}}}}\n' "$id"
      ;;
  esac
done
"#;

    /// An executable that exits before answering `plugin.initialize`.
    #[cfg(unix)]
    const SH_PLUGIN_THAT_DIES: &str = "#!/bin/sh\nexit 3\n";

    #[cfg(unix)]
    fn write_executable_plugin(
        directory: &std::path::Path,
        name: &str,
        body: &str,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::create_dir_all(directory).expect("create the plugin discovery directory");
        let path = directory.join(name);
        std::fs::write(&path, body).expect("write the executable plugin");
        let mut permissions = std::fs::metadata(&path)
            .expect("stat the executable plugin")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("mark the plugin executable");
        path
    }

    /// Drive `PluginRuntime::load` exactly as production does, with JavaScript off.
    #[cfg(unix)]
    async fn load_process_plugins(env: &ScriptedEnv) -> Option<PluginRuntime> {
        // No `plugin_runtime` key at all: the JavaScript host stays off, which is the
        // point — a process plugin must not need the JavaScript switch.
        load_process_plugins_with(
            env,
            &zuno_config::Config::default(),
            JsPluginPolicy {
                enabled: false,
                source: JsPluginSource::Default,
            },
        )
        .await
    }

    #[cfg(unix)]
    async fn load_process_plugins_with(
        env: &ScriptedEnv,
        config: &zuno_config::Config,
        policy: JsPluginPolicy,
    ) -> Option<PluginRuntime> {
        let process_env = zuno_paths::Env::from_pairs(env.env_vars());
        let layout = zuno_paths::Layout::resolve(&process_env);
        let project = zuno_paths::project::resolve_project(env.project());
        PluginRuntime::load(
            config,
            &project,
            env.project(),
            env.project(),
            &layout,
            policy,
            super::PluginRuntimeTarget::server("process-discovery-test"),
        )
        .await
    }

    #[cfg(unix)]
    fn shell_env_input(cwd: &std::path::Path) -> ShellEnvInput {
        ShellEnvInput {
            cwd: cwd.to_path_buf(),
            session_id: "ses_process_discovery".to_owned(),
            call_id: "call_process_discovery".to_owned(),
        }
    }

    /// An executable dropped in `.zuno/plugin/` must have its hook invoked.
    ///
    /// The whole reachability claim in one assertion. `jsonrpc.rs` has carried a
    /// complete out-of-process tier — spec, handshake, timeout, permanent disable —
    /// since it landed, with no feature gate, so it is in every shipped binary. What
    /// it has never had is a caller: nothing in `zuno-cli` ever built a
    /// `PluginProcessSpec`. A complete API with no way in is the defect this
    /// repository has shipped before, so the test drives the real
    /// `PluginRuntime::load` and the real `ShellEnvHook`, not a hand-assembled bus.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_executable_dropped_in_the_plugin_directory_has_its_hook_invoked() {
        let env = ScriptedEnv::new().expect("isolated process-plugin environment");
        let directory = env.project().join(".zuno").join("plugin");
        let plugin = write_executable_plugin(&directory, "sh-example", SH_PLUGIN);

        let runtime = load_process_plugins(&env).await.unwrap_or_else(|| {
            panic!(
                "dropping {} in a scanned directory must produce a runtime; \
                 the process tier is reachable or it is not",
                plugin.display()
            )
        });

        let values = runtime
            .env(shell_env_input(env.project()))
            .await
            .expect("the shell.env hook must reach the discovered executable");

        assert_eq!(
            values.get("SH_PLUGIN").map(String::as_str),
            Some("enabled"),
            "the executable answered `shell.env`, so its output must arrive: {values:?}"
        );
        runtime.shutdown().await;
    }

    /// `--pure` still means no external plugins, for the new tier as much as the old.
    ///
    /// The paired half of the separation. Freeing process plugins from the JavaScript
    /// switch must not free them from the kill switch, and the two claims fail in
    /// opposite directions: a tier that inherits `javascript` is unreachable, and a
    /// tier that ignores `--pure` is unstoppable.
    #[cfg(unix)]
    #[tokio::test]
    async fn pure_refuses_a_discovered_executable() {
        let env = ScriptedEnv::new().expect("isolated pure-policy environment");
        let directory = env.project().join(".zuno").join("plugin");
        write_executable_plugin(&directory, "sh-example", SH_PLUGIN);

        let runtime = load_process_plugins_with(
            &env,
            &zuno_config::Config::default(),
            JsPluginPolicy {
                enabled: false,
                source: JsPluginSource::Pure,
            },
        )
        .await;

        assert!(
            runtime.is_none(),
            "`--pure` asked for no external plugins, and an executable is external"
        );
    }

    /// The tier has its own key, and turning that key off turns this tier off only.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_configured_process_false_refuses_a_discovered_executable() {
        let env = ScriptedEnv::new().expect("isolated process-policy environment");
        let directory = env.project().join(".zuno").join("plugin");
        write_executable_plugin(&directory, "sh-example", SH_PLUGIN);
        let config = zuno_config::Config {
            plugin_runtime: Some(zuno_config::schema::PluginRuntimeConfig {
                javascript: None,
                process: Some(false),
            }),
            ..zuno_config::Config::default()
        };

        let runtime = load_process_plugins_with(
            &env,
            &config,
            JsPluginPolicy {
                enabled: false,
                source: JsPluginSource::Config,
            },
        )
        .await;

        assert!(runtime.is_none(), "`process: false` must be honoured");
    }

    /// An absent key leaves the process tier on, which is the reachability claim.
    ///
    /// Pinned next to `an_absent_plugin_runtime_key_leaves_javascript_off` on purpose:
    /// the same absent key means opposite things for the two tiers, and a reader who
    /// finds only one of these tests will assume symmetry.
    #[test]
    fn an_absent_plugin_runtime_key_leaves_the_process_tier_on() {
        let policy = ProcessPluginPolicy::resolve(
            &zuno_config::Config::default(),
            JsPluginPolicy {
                enabled: false,
                source: JsPluginSource::Default,
            },
        );

        assert!(
            policy.enabled,
            "a discovered executable needs no runtime installed, so there is no cost \
             left for a user to consent to"
        );
        assert_eq!(policy.source, ProcessPluginSource::Default);
    }

    /// A plugin that never starts must be visible to the user, not only to `tracing`.
    ///
    /// dsh documents this exact failure shape in its own plugin guide: a module whose
    /// path cannot be resolved is reported through the logger, "and at boot that
    /// report can be lost before a console exporter is watching". Our tier must not
    /// have that shape, so the diagnostic has to survive as far as
    /// `take_diagnostics`, which is what `turn` turns into a transcript notice and
    /// what `models` prints to stderr.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_process_plugin_that_fails_to_start_is_reported_to_the_user() {
        let env = ScriptedEnv::new().expect("isolated failing-plugin environment");
        let directory = env.project().join(".zuno").join("plugin");
        write_executable_plugin(&directory, "sh-broken", SH_PLUGIN_THAT_DIES);

        let runtime = load_process_plugins(&env)
            .await
            .expect("a failing plugin still has to produce a runtime that can report it");

        let diagnostics = runtime.take_diagnostics();

        assert_eq!(diagnostics.len(), 1, "diagnostics={diagnostics:?}");
        assert!(
            diagnostics[0].contains("sh-broken"),
            "a user cannot act on a failure that does not name the plugin: {diagnostics:?}"
        );
        runtime.shutdown().await;
    }

    fn plugin_catalog(config: &zuno_config::Config) -> Catalog {
        let document = serde_json::from_value(json!({
            "groq": {
                "id": "groq",
                "name": "Groq",
                "env": [],
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "small-model": {
                        "id": "small-model",
                        "name": "Small Model",
                        "limit": { "context": 100_000, "output": 4_096 }
                    }
                }
            }
        }))
        .expect("catalog document");
        Catalog::resolve(&document, &ResolveInput::new().with_config(config))
    }

    async fn load_catalog_plugin(
        env: &ScriptedEnv,
        source: &str,
        options: serde_json::Value,
    ) -> (PluginRuntime, zuno_config::Config) {
        let plugin = env.project().join("catalog-plugin.mjs");
        std::fs::write(&plugin, source).expect("write catalog plugin");
        let config: zuno_config::Config = serde_json::from_value(json!({
            "plugin": [[format!("file:{}", plugin.display()), options]],
            "provider": { "groq": {} }
        }))
        .expect("parse catalog plugin config");
        let process_env = zuno_paths::Env::from_pairs(env.env_vars());
        let layout = zuno_paths::Layout::resolve(&process_env);
        let project = zuno_paths::project::resolve_project(env.project());
        let runtime = PluginRuntime::load(
            &config,
            &project,
            env.project(),
            env.project(),
            &layout,
            // Explicit, not `pure: false`: this test exercises plugin behaviour, so it
            // has to ask for the host the product no longer starts by default.
            super::JsPluginPolicy {
                enabled: true,
                source: super::JsPluginSource::Test,
            },
            super::PluginRuntimeTarget::server("catalog-test"),
        )
        .await
        .expect("load catalog plugin");
        (runtime, config)
    }

    fn catalog_provider() -> ResolvedProvider {
        let model = ResolvedModel {
            id: "small-model".to_owned(),
            provider_id: "groq".to_owned(),
            name: "Small Model".to_owned(),
            family: "test".to_owned(),
            release_date: String::new(),
            status: CatalogStatus::Active,
            api: ModelApi {
                id: "small-model".to_owned(),
                npm: "@ai-sdk/openai-compatible".to_owned(),
                url: String::new(),
                endpoint: None,
            },
            capabilities: ModelCapabilities::default(),
            cost: ModelCost::default(),
            limit: ModelLimit {
                context: 100_000.0,
                input: None,
                output: 4_096.0,
            },
            options: serde_json::Map::new(),
            headers: BTreeMap::new(),
            variants: BTreeMap::new(),
        };
        ResolvedProvider {
            id: "groq".to_owned(),
            name: "Groq".to_owned(),
            env: Vec::new(),
            options: serde_json::Map::new(),
            availability: Availability::none(),
            models: BTreeMap::from([("small-model".to_owned(), model)]),
        }
    }

    #[tokio::test]
    async fn every_hook_surface_failure_boundary_is_non_fatal_by_name() {
        let mut applicable = 0;
        for surface in SURFACES {
            for hook in HookName::ALL {
                let boundary = failure_boundary(surface, hook);
                assert_ne!(
                    boundary,
                    FailureBoundary::Fatal,
                    "{} × {surface:?} maps a plugin failure to a fatal surface error",
                    hook.as_str()
                );
                if boundary != FailureBoundary::NotApplicable {
                    applicable += 1;
                }
                if matches!(
                    boundary,
                    FailureBoundary::HookBus | FailureBoundary::Shutdown
                ) {
                    let plugin = Arc::new(DisablingFailurePlugin::new(hook));
                    let bus = HookBus::new(vec![plugin]);
                    let result = dispatch_failure_fixture(&bus, hook).await;
                    assert!(
                        result.is_ok(),
                        "{} × {surface:?} propagated a disabled plugin failure: {result:?}",
                        hook.as_str()
                    );
                }
            }
        }
        assert_eq!(applicable, 67, "the hook × surface matrix changed coverage");
    }

    #[tokio::test]
    async fn configured_provider_without_stored_auth_does_not_invoke_auth_loader() {
        let env = ScriptedEnv::new().expect("isolated auth-loader environment");
        let call_log = env.project().join("auth-loader.calls");
        let (runtime, config) = load_catalog_plugin(
            &env,
            AUTH_LOADER_PLUGIN,
            json!({ "callLog": call_log, "alwaysFail": false }),
        )
        .await;
        let mut catalog = plugin_catalog(&config);

        runtime
            .apply_catalog(&mut catalog, &BTreeMap::new())
            .await
            .expect("an absent credential must skip the SDK's Promise<Auth> loader");

        assert!(
            !call_log.exists(),
            "getAuth cannot legitimately return null through Promise<Auth>; the loader must not run"
        );
        assert!(runtime.take_diagnostics().is_empty());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn failing_auth_loader_is_disabled_and_catalog_resolution_continues_with_a_diagnostic() {
        let env = ScriptedEnv::new().expect("isolated auth-loader environment");
        let call_log = env.project().join("auth-loader.calls");
        let (runtime, config) = load_catalog_plugin(
            &env,
            AUTH_LOADER_PLUGIN,
            json!({ "callLog": call_log, "alwaysFail": true }),
        )
        .await;
        let mut catalog = plugin_catalog(&config);
        let credentials = BTreeMap::from([(
            "groq".to_owned(),
            Credential::Api {
                key: Secret::new("fixture-key"),
                metadata: None,
            },
        )]);

        runtime
            .apply_catalog(&mut catalog, &credentials)
            .await
            .expect("a failing auth loader must not abort catalog resolution");

        let diagnostics = runtime.take_diagnostics();
        assert_eq!(diagnostics.len(), 1, "diagnostics={diagnostics:?}");
        assert!(
            diagnostics[0].contains("production-auth-loader-fixture")
                && diagnostics[0].contains("auth.loader")
                && diagnostics[0].contains("authenticated loader failure"),
            "the diagnostic must name plugin, hook, and cause: {diagnostics:?}"
        );
        assert!(
            runtime
                .javascript_plugins()
                .next()
                .expect("the loaded plugin")
                .is_disabled(),
            "the failing auth plugin must remain disabled"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn failing_provider_model_loader_is_disabled_and_catalog_resolution_continues() {
        let env = ScriptedEnv::new().expect("isolated provider-loader environment");
        let (runtime, config) = load_catalog_plugin(&env, PROVIDER_MODELS_PLUGIN, json!({})).await;
        let mut catalog = plugin_catalog(&config);

        runtime
            .apply_catalog(&mut catalog, &BTreeMap::new())
            .await
            .expect("a failing provider model loader must not abort catalog resolution");

        let diagnostics = runtime.take_diagnostics();
        assert_eq!(diagnostics.len(), 1, "diagnostics={diagnostics:?}");
        assert!(
            diagnostics[0].contains("production-provider-models-fixture")
                && diagnostics[0].contains("provider.models")
                && diagnostics[0].contains("provider model loader failure"),
            "the diagnostic must name plugin, hook, and cause: {diagnostics:?}"
        );
        runtime.shutdown().await;
    }

    fn seeded_connection() -> zuno_db::Connection {
        let mut connection =
            zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open compaction database");
        zuno_db::migration::apply(&mut connection).expect("apply compaction schema");
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
                 VALUES ('project-plugin-compaction', '/workspace', 1, 1, '[]');
                 INSERT INTO session \
                   (id, project_id, slug, directory, title, version, time_created, time_updated) \
                 VALUES ('ses_plugin_compaction', 'project-plugin-compaction', 'plugin', \
                   '/workspace', 'plugin', '1', 1, 1);",
            )
            .expect("seed compaction project and session");
        connection
    }

    fn transcript() -> Vec<TranscriptEntry> {
        vec![
            TranscriptEntry::new("system", Message::new(Role::System, "system"), 1),
            TranscriptEntry::new("user-old", Message::new(Role::User, "old request"), 50),
            TranscriptEntry::new(
                "assistant-old",
                Message::new(Role::Assistant, "old answer"),
                50,
            ),
            TranscriptEntry::new("user-new", Message::new(Role::User, "new request"), 10),
            TranscriptEntry::new(
                "assistant-new",
                Message::new(Role::Assistant, "new answer"),
                10,
            ),
        ]
    }

    #[tokio::test]
    async fn compaction_plugin_hooks_mutate_the_real_summary_request_and_continuation() {
        if zuno_testkit::recordings_root_or_skip(
            "compaction_plugin_hooks_mutate_the_real_summary_request_and_continuation",
            "the real summary request and continuation were NOT replayed",
        )
        .is_none()
        {
            return;
        }
        let env = ScriptedEnv::new().expect("isolated compaction environment");
        let plugin = env.project().join("compaction-plugin.mjs");
        std::fs::write(&plugin, COMPACTION_PLUGIN).expect("write compaction plugin");
        let config: zuno_config::Config = serde_json::from_value(json!({
            "plugin": [[format!("file:{}", plugin.display()), {}]]
        }))
        .expect("parse compaction plugin config");
        let process_env = zuno_paths::Env::from_pairs(env.env_vars());
        let layout = zuno_paths::Layout::resolve(&process_env);
        let project = zuno_paths::project::resolve_project(env.project());
        let runtime = PluginRuntime::load(
            &config,
            &project,
            env.project(),
            env.project(),
            &layout,
            // Explicit, not `pure: false`: this test exercises plugin behaviour, so it
            // has to ask for the host the product no longer starts by default.
            super::JsPluginPolicy {
                enabled: true,
                source: super::JsPluginSource::Test,
            },
            super::PluginRuntimeTarget::server("compaction-test"),
        )
        .await
        .expect("load compaction plugin");
        *runtime.providers.write().expect("catalog context lock") =
            BTreeMap::from([("groq".to_owned(), catalog_provider())]);

        let scenario = Scenario::new("production-compaction-hooks")
            .on_path("/v1/chat/completions")
            .from_oracle_cassette("openai-chat/streams-text")
            .expect("load compaction response");
        let mock = MockProvider::start(vec![scenario])
            .await
            .expect("start compaction provider");
        let transport: Arc<dyn Transport> = Arc::new(ReqwestTransport::new("groq"));
        let provider = CompatibleProvider::new(
            Spec::new("groq").with_base_url(format!("{}/v1", mock.base_url())),
            transport,
            None,
        )
        .expect("construct compaction provider");
        let mut connection = seeded_connection();
        let compaction = CompactionConfig {
            auto: Some(true),
            tail_turns: Some(1),
            preserve_recent_tokens: Some(20),
            reserved: Some(10),
            ..CompactionConfig::default()
        };
        let request = CompactionRequest::new(
            "ses_plugin_compaction",
            "plugin-attempt",
            "build",
            "groq",
            "small-model",
            transcript(),
            &compaction,
            TokenWindow {
                context: 100,
                max_output: 10,
            },
            CompactionTrigger::ContextLimit {
                used_tokens: Some(101),
                limit_tokens: Some(100),
            },
        );
        let mut state = CompactionState::default();
        let mut tracker = CacheTracker::new();
        let mut locked_tools: LockedTools<String> = LockedTools::new();
        let outcome = {
            let mut cache = CompactionCache::new(&mut tracker, &mut locked_tools);
            run_compaction(
                &mut connection,
                &provider,
                &runtime,
                &mut state,
                &mut cache,
                request,
            )
            .await
            .expect("run compaction")
        };
        let CompactionOutcome::Compacted(CompactedTranscript { auto_continue, .. }) = outcome
        else {
            panic!("the compaction hook fixture must complete: {outcome:?}");
        };
        assert!(
            !auto_continue,
            "experimental.compaction.autocontinue was not consumed"
        );
        let captured = mock.captured().await;
        assert_eq!(captured.len(), 1);
        assert!(
            captured[0].body.contains("plugin-compaction-prompt"),
            "experimental.session.compacting did not replace the real summary prompt: {}",
            captured[0].body
        );

        runtime.shutdown().await;
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn provider_oauth_backend_retains_and_consumes_the_live_plugin_callback() {
        let env = ScriptedEnv::new().expect("isolated OAuth environment");
        let plugin = env.project().join("oauth-plugin.mjs");
        std::fs::write(&plugin, OAUTH_PLUGIN).expect("write OAuth plugin");
        let config: zuno_config::Config = serde_json::from_value(json!({
            "plugin": [[format!("file:{}", plugin.display()), {}]]
        }))
        .expect("parse OAuth plugin config");
        let process_env = zuno_paths::Env::from_pairs(env.env_vars());
        let layout = zuno_paths::Layout::resolve(&process_env);
        let project = zuno_paths::project::resolve_project(env.project());
        let runtime = PluginRuntime::load(
            &config,
            &project,
            env.project(),
            env.project(),
            &layout,
            // Explicit, not `pure: false`: this test exercises plugin behaviour, so it
            // has to ask for the host the product no longer starts by default.
            super::JsPluginPolicy {
                enabled: true,
                source: super::JsPluginSource::Test,
            },
            super::PluginRuntimeTarget::server_with_stdio(
                "oauth-test",
                reqwest::Url::parse("http://127.0.0.1:1").expect("fixture server URL"),
            ),
        )
        .await
        .expect("load OAuth plugin");

        let authorization = runtime
            .authorize(ProviderOAuthAuthorizeRequest {
                provider_id: "kiro-auth".to_owned(),
                method: 0,
                inputs: BTreeMap::new(),
            })
            .await
            .expect("start OAuth authorization");
        assert_eq!(authorization.url, "https://device.example.test/authorize");
        assert_eq!(authorization.method, "auto");
        assert_eq!(authorization.instructions, "complete fixture authorization");

        let request = ProviderOAuthCallbackRequest {
            provider_id: "kiro-auth".to_owned(),
            method: 0,
            code: None,
        };
        let completion = runtime
            .callback(request.clone())
            .await
            .expect("invoke retained OAuth callback")
            .expect("OAuth callback succeeds");
        assert_eq!(completion.provider_id.as_deref(), Some("kiro-auth"));
        assert_eq!(
            completion.credential,
            zuno_auth::Credential::Oauth {
                refresh: zuno_auth::Secret::new("fixture-refresh"),
                access: zuno_auth::Secret::new("fixture-access"),
                expires: 1234,
                account_id: None,
                enterprise_url: None,
            }
        );
        assert!(
            runtime.callback(request).await.is_err(),
            "a live plugin callback must be consumed exactly once"
        );

        runtime.shutdown().await;
    }
}
