use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use oc_engine::compaction::{
    AutoContinueHookInput, CompactionHookInput, CompactionHooks, CompactionPrompt,
};
use oc_engine::hooks::{
    HookMessageWithParts, PermissionHookDecision, RequestHookInput, ToolHooks, TurnHooks,
};
use oc_engine::terminal_lease::{
    LeaseReason, ReclaimCause, TerminalBroker, TerminalLease, TerminalLeaseError,
    TerminalLeaseGuard, TerminalOwner,
};
use oc_llm::catalog::availability::AvailabilitySource;
use oc_llm::catalog::resolved::{ResolvedModel as CatalogModel, ResolvedProvider};
use oc_llm::registry::CompletionRequest;
use oc_permission::PermissionRequest;
use oc_plugin::{
    AuthCallbackResult, AuthCredentialResolver, AuthMethod, AuthOAuthCallback, AuthSuccess,
    ChatContext, ChatHeadersOutput, ChatMessageInput, ChatMessageOutput,
    ChatMessagesTransformOutput, ChatParamsOutput, ChatSystemTransformInput,
    ChatSystemTransformOutput, CommandExecuteBeforeInput, CommandExecuteBeforeOutput,
    CompactionAutocontinueInput, CompactionAutocontinueOutput, ConfigDirectory, HookBus,
    HookInvocation, HookName, JsHostConfig, JsPluginKind, JsPluginLoad, JsPluginSpec,
    MessageWithParts, PermissionAskInput, PermissionAskOutput, PermissionStatus, Plugin,
    PluginOrigin, PluginScope, PluginTools, ProviderContext, ProviderHookContext,
    ProviderSmallModelInput, ProviderSmallModelOutput, ProviderSource, SessionCompactingInput,
    SessionCompactingOutput, ShellEnvInput as PluginShellEnvInput, ShellEnvOutput,
    TextCompleteInput, TextCompleteOutput, ToolDefinitionInput, ToolExecuteAfterInput,
    ToolExecuteBeforeInput, ToolExecuteBeforeOutput, discover_plugins, load_js_plugins_ordered,
};
use oc_server::{
    ProviderOAuthAuthorization, ProviderOAuthAuthorizeRequest, ProviderOAuthBackend,
    ProviderOAuthCallbackRequest, ProviderOAuthCompletion, ProviderOAuthFuture,
};
use oc_tool::{ToolDefinition, ToolOutput};
use oc_tools::shell::{ShellEnvHook, ShellEnvInput};
use serde_json::{Map, Value};

pub(crate) struct PluginRuntime {
    load: JsPluginLoad,
    bus: HookBus,
    providers: RwLock<BTreeMap<String, ResolvedProvider>>,
    oauth_callbacks: Arc<Mutex<BTreeMap<(String, usize), AuthOAuthCallback>>>,
    reported_diagnostics: Mutex<usize>,
    shutdown: AtomicBool,
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

impl PluginRuntime {
    pub(crate) async fn load(
        config: &oc_config::Config,
        project: &oc_paths::ResolvedProject,
        directory: &Path,
        worktree: &Path,
        layout: &oc_paths::Layout,
        pure: bool,
        target: PluginRuntimeTarget,
    ) -> Option<Self> {
        let mut specs = if pure {
            Vec::new()
        } else {
            configured_plugins(config, target.kind)
        };
        if !pure {
            specs.extend(auto_discovered_plugins(
                directory,
                worktree,
                layout,
                target.kind,
                target.surface,
            ));
        }
        if specs.is_empty() {
            return None;
        }
        let terminal: Arc<dyn TerminalLease> = match target.terminal {
            PluginRuntimeTerminal::Reject => Arc::new(HeadlessTerminalLease),
            PluginRuntimeTerminal::Stdio => Arc::new(TerminalBroker::new(Arc::new(StdioTerminal))),
        };
        let server_url = target.server_url.unwrap_or_else(|| {
            reqwest::Url::parse("http://127.0.0.1:0").expect("static plugin server URL")
        });
        let host = JsHostConfig::new(project.clone(), server_url, terminal)
            .directory(directory)
            .worktree(worktree)
            .cache_dir(layout.cache());
        let load = load_js_plugins_ordered(specs, host).await;
        let diagnostics = load.diagnostics();
        for diagnostic in &diagnostics {
            tracing::warn!(
                plugin = %diagnostic.plugin,
                hook = ?diagnostic.hook,
                kind = ?diagnostic.kind,
                message = %diagnostic.message,
                surface = target.surface,
                "JavaScript plugin did not fully load"
            );
        }
        let plugins = load
            .plugins()
            .iter()
            .cloned()
            .map(|plugin| plugin as Arc<dyn Plugin>)
            .collect();
        Some(Self {
            load,
            bus: HookBus::new(plugins),
            providers: RwLock::new(BTreeMap::new()),
            oauth_callbacks: Arc::new(Mutex::new(BTreeMap::new())),
            reported_diagnostics: Mutex::new(diagnostics.len()),
            shutdown: AtomicBool::new(false),
        })
    }

    pub(crate) fn take_diagnostics(&self) -> Vec<String> {
        let diagnostics = self.load.diagnostics();
        let Ok(mut reported) = self.reported_diagnostics.lock() else {
            return vec!["plugin diagnostic cursor lock was poisoned".to_owned()];
        };
        let messages = diagnostics
            .iter()
            .skip(*reported)
            .map(|diagnostic| {
                let hook = diagnostic
                    .hook
                    .as_deref()
                    .map_or_else(|| "startup".to_owned(), |hook| format!("hook `{hook}`"));
                format!(
                    "disabled plugin `{}` after {hook} failed: {}",
                    diagnostic.plugin, diagnostic.message
                )
            })
            .collect();
        *reported = diagnostics.len();
        messages
    }

    pub(crate) async fn apply_config(&self, config: &mut oc_config::Config) -> Result<(), String> {
        self.bus
            .dispatch(HookInvocation::Config { config })
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn apply_catalog(
        &self,
        catalog: &mut oc_llm::catalog::Catalog,
        credentials: &BTreeMap<String, oc_auth::Credential>,
    ) -> Result<(), String> {
        let mut auth_hooks = Vec::new();
        self.bus
            .dispatch(HookInvocation::Auth {
                output: &mut auth_hooks,
            })
            .await
            .map_err(|error| error.to_string())?;
        for hook in auth_hooks {
            let Some(loader) = hook.loader else {
                continue;
            };
            let Some(provider) = catalog.provider_mut(&hook.provider) else {
                continue;
            };
            let auth = StoredCredential(credentials.get(&hook.provider).cloned());
            let options = loader.load(&auth, provider).await.map_err(|error| {
                format!("plugin auth loader `{}` failed: {error}", hook.provider)
            })?;
            provider.options.extend(options);
        }

        let mut provider_hooks = Vec::new();
        self.bus
            .dispatch(HookInvocation::Provider {
                output: &mut provider_hooks,
            })
            .await
            .map_err(|error| error.to_string())?;
        for hook in provider_hooks {
            let Some(loader) = hook.models else {
                continue;
            };
            let Some(provider) = catalog.provider(&hook.id).cloned() else {
                continue;
            };
            let models = loader
                .models(
                    &provider,
                    ProviderHookContext {
                        auth: credentials.get(&hook.id),
                    },
                )
                .await
                .map_err(|error| {
                    format!(
                        "plugin provider `{}` failed to contribute models: {error}",
                        hook.id
                    )
                })?;
            catalog.replace_provider_models(&hook.id, models);
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
        self.load.shutdown().await;
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
        parts: &mut Vec<oc_db::message::PartRecord>,
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
            credential: oc_auth::Credential::Oauth {
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
            credential: oc_auth::Credential::Api { key, metadata },
        },
    }
}

#[async_trait]
impl TurnHooks for PluginRuntime {
    fn enabled(&self) -> bool {
        !self.bus.plugins().is_empty()
    }

    async fn event(&self, event: &oc_engine::r#loop::TurnEvent) -> Result<(), String> {
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
        model: &oc_engine::r#loop::ResolvedModel,
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
        Ok(match output.status {
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
    ) -> Result<BTreeMap<String, String>, oc_error::ToolError> {
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
            .map_err(|error| oc_error::ToolError::Failed {
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

fn configured_plugins(config: &oc_config::Config, kind: JsPluginKind) -> Vec<JsPluginSpec> {
    config
        .plugin
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|plugin| js_plugin_spec(plugin, kind))
        .collect()
}

fn auto_discovered_plugins(
    directory: &Path,
    worktree: &Path,
    layout: &oc_paths::Layout,
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
    plugin: &oc_config::schema::plugin::PluginSpec,
    kind: JsPluginKind,
) -> JsPluginSpec {
    let spec = JsPluginSpec::new(plugin.name()).with_kind(kind);
    match plugin.options() {
        Some(options) => spec.options(serde_json::Value::Object(options.clone())),
        None => spec,
    }
}

struct StoredCredential(Option<oc_auth::Credential>);

#[async_trait]
impl AuthCredentialResolver for StoredCredential {
    async fn resolve(&self) -> Result<Option<oc_auth::Credential>, oc_error::BoxSource> {
        Ok(self.0.clone())
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

    use oc_config::schema::CompactionConfig;
    use oc_engine::compaction::{
        CompactedTranscript, CompactionCache, CompactionOutcome, CompactionRequest,
        CompactionState, CompactionTrigger, TokenWindow, TranscriptEntry, run_compaction,
    };
    use oc_llm::cache::{CacheTracker, LockedTools};
    use oc_llm::catalog::availability::Availability;
    use oc_llm::catalog::models_dev::CatalogStatus;
    use oc_llm::catalog::resolved::{
        ModelApi, ModelCapabilities, ModelCost, ModelLimit, ResolvedModel, ResolvedProvider,
    };
    use oc_llm::event::{Message, Role};
    use oc_llm::registry::Spec;
    use oc_provider_compatible::{CompatibleProvider, ReqwestTransport, Transport};
    use oc_server::{
        ProviderOAuthAuthorizeRequest, ProviderOAuthBackend, ProviderOAuthCallbackRequest,
    };
    use oc_testkit::{MockProvider, Scenario, ScriptedEnv};
    use serde_json::json;

    use super::PluginRuntime;

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

    fn seeded_connection() -> oc_db::Connection {
        let mut connection =
            oc_db::open::open(&oc_paths::DbLocation::Memory).expect("open compaction database");
        oc_db::migration::apply(&mut connection).expect("apply compaction schema");
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
        let env = ScriptedEnv::new().expect("isolated compaction environment");
        let plugin = env.project().join("compaction-plugin.mjs");
        std::fs::write(&plugin, COMPACTION_PLUGIN).expect("write compaction plugin");
        let config: oc_config::Config = serde_json::from_value(json!({
            "plugin": [[format!("file:{}", plugin.display()), {}]]
        }))
        .expect("parse compaction plugin config");
        let process_env = oc_paths::Env::from_pairs(env.env_vars());
        let layout = oc_paths::Layout::resolve(&process_env);
        let project = oc_paths::project::resolve_project(env.project());
        let runtime = PluginRuntime::load(
            &config,
            &project,
            env.project(),
            env.project(),
            &layout,
            false,
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
        let config: oc_config::Config = serde_json::from_value(json!({
            "plugin": [[format!("file:{}", plugin.display()), {}]]
        }))
        .expect("parse OAuth plugin config");
        let process_env = oc_paths::Env::from_pairs(env.env_vars());
        let layout = oc_paths::Layout::resolve(&process_env);
        let project = oc_paths::project::resolve_project(env.project());
        let runtime = PluginRuntime::load(
            &config,
            &project,
            env.project(),
            env.project(),
            &layout,
            false,
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
            oc_auth::Credential::Oauth {
                refresh: oc_auth::Secret::new("fixture-refresh"),
                access: oc_auth::Secret::new("fixture-access"),
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
