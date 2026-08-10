use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use oc_engine::terminal_lease::{
    LeaseReason, TerminalLease, TerminalLeaseError, TerminalLeaseGuard,
};
use oc_plugin::{
    AuthCredentialResolver, HookBus, HookInvocation, JsHostConfig, JsPluginLoad, JsPluginSpec,
    Plugin, PluginTools, ProviderHookContext, load_js_plugins_ordered,
};

pub(crate) struct PluginRuntime {
    load: JsPluginLoad,
    bus: HookBus,
}

impl PluginRuntime {
    pub(crate) async fn load(
        config: &oc_config::Config,
        project: &oc_paths::ResolvedProject,
        directory: &Path,
        worktree: &Path,
        layout: &oc_paths::Layout,
        pure: bool,
        surface: &str,
    ) -> Option<Self> {
        let specs = if pure {
            Vec::new()
        } else {
            configured_plugins(config)
        };
        if specs.is_empty() {
            return None;
        }
        let terminal: Arc<dyn TerminalLease> = Arc::new(HeadlessTerminalLease);
        let host = JsHostConfig::new(
            project.clone(),
            reqwest::Url::parse("http://127.0.0.1:0").expect("static plugin server URL"),
            terminal,
        )
        .directory(directory)
        .worktree(worktree)
        .cache_dir(layout.cache());
        let load = load_js_plugins_ordered(specs, host).await;
        for diagnostic in load.diagnostics() {
            tracing::warn!(
                plugin = %diagnostic.plugin,
                kind = ?diagnostic.kind,
                message = %diagnostic.message,
                %surface,
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
        })
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
        self.load.shutdown().await;
    }
}

fn configured_plugins(config: &oc_config::Config) -> Vec<JsPluginSpec> {
    config
        .plugin
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|plugin| {
            let spec = JsPluginSpec::new(plugin.name());
            match plugin.options() {
                Some(options) => spec.options(serde_json::Value::Object(options.clone())),
                None => spec,
            }
        })
        .collect()
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
