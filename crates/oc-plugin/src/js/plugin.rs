use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use oc_error::{BoxSource, ToolError};
use oc_tool::{Tool, ToolContext, ToolOutput};
use serde_json::{Map, Value, json};

use crate::{
    AuthHook, HookInvocation, HookName, Plugin, PluginDiagnosticKind, PluginManifest, PluginTools,
    ProviderHook,
};

use super::host::JsHost;
use super::loader::{JsDiagnostic, map_diagnostic_kind};

pub struct JsPlugin {
    manifest: PluginManifest,
    auth: Option<AuthHook>,
    provider: Option<ProviderHook>,
    tools: PluginTools,
    host: JsHost,
    callbacks: HashMap<String, usize>,
    disabled: AtomicBool,
}

impl JsPlugin {
    pub(crate) fn build(
        host: JsHost,
        fallback_id: &str,
        directory: PathBuf,
        worktree: PathBuf,
    ) -> Result<Arc<Self>, JsPluginBuildError> {
        let report = host.report().ok_or(JsPluginBuildError::MissingReport)?;
        let hooks = report
            .hooks
            .iter()
            .map(|name| HookName::from_str(name))
            .collect::<Result<Vec<_>, _>>()?;
        let id = report.id.as_deref().unwrap_or(fallback_id);
        let manifest = PluginManifest::new(id, hooks)?;
        let auth = super::bridge::auth_hooks(&host, &report.auth)?
            .into_iter()
            .next();
        let provider = super::bridge::provider_hooks(&host, &report.provider)?
            .into_iter()
            .next();
        let tools = plugin_tools(&host, &report.tools, directory, worktree)?;
        let callbacks = report
            .callbacks
            .iter()
            .map(|(name, handles)| (name.clone(), handles.len()))
            .collect();
        Ok(Arc::new(Self {
            manifest,
            auth,
            provider,
            tools,
            host,
            callbacks,
            disabled: AtomicBool::new(false),
        }))
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<JsDiagnostic> {
        self.host
            .diagnostics()
            .into_iter()
            .map(|diagnostic| JsDiagnostic {
                plugin: diagnostic.plugin,
                hook: diagnostic.hook,
                kind: map_diagnostic_kind(diagnostic.kind),
                message: diagnostic.message,
            })
            .collect()
    }

    #[must_use]
    pub fn restart_count(&self) -> usize {
        self.host.restart_count()
    }

    #[must_use]
    pub fn init_report(&self) -> Option<Arc<super::host::JsInitReport>> {
        self.host.report()
    }

    pub async fn shutdown(&self) {
        self.host.shutdown().await;
    }

    async fn disable(
        &self,
        hook: HookName,
        kind: PluginDiagnosticKind,
        error: impl std::fmt::Display,
    ) {
        self.disable_named(hook.as_str(), kind, error).await;
    }

    async fn disable_named(
        &self,
        hook: &str,
        kind: PluginDiagnosticKind,
        error: impl std::fmt::Display,
    ) {
        if self.disabled.swap(true, Ordering::AcqRel) {
            return;
        }
        self.host
            .disable(self.manifest.id(), kind, hook, error.to_string())
            .await;
    }

    /// Permanently disable a resource callback that failed outside [`HookBus`].
    ///
    /// Auth and provider loaders are retained resource callbacks rather than
    /// [`HookInvocation`] variants, so their caller must enter the same diagnostic
    /// and disable path used by ordinary hooks instead of turning the error into a
    /// surface failure.
    pub async fn disable_after_callback_failure(&self, hook: &str, error: &BoxSource) {
        let kind = error
            .downcast_ref::<super::host::JsHostError>()
            .map_or(PluginDiagnosticKind::Protocol, |error| error.kind());
        self.disable_named(hook, kind, error).await;
    }
}

#[async_trait]
impl Plugin for JsPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }

    fn auth(&self) -> Option<AuthHook> {
        (!self.disabled.load(Ordering::Acquire))
            .then(|| self.auth.clone())
            .flatten()
    }

    fn provider(&self) -> Option<ProviderHook> {
        (!self.disabled.load(Ordering::Acquire))
            .then(|| self.provider.clone())
            .flatten()
    }

    fn tools(&self) -> PluginTools {
        if self.disabled.load(Ordering::Acquire) {
            PluginTools::new()
        } else {
            self.tools.clone()
        }
    }

    async fn call(&self, hook: &mut HookInvocation<'_>) -> Result<(), BoxSource> {
        if self.disabled.load(Ordering::Acquire) {
            return Ok(());
        }
        let name = hook.name();
        let count = self.callbacks.get(name.as_str()).copied().unwrap_or(0);
        for index in 0..count {
            let call = match crate::jsonrpc::encode_hook(hook) {
                Ok(call) => call,
                Err(error) => {
                    let message = error.to_string();
                    self.disable(name, PluginDiagnosticKind::Protocol, error)
                        .await;
                    return Err(boxed(JsPluginHookFailure(message)));
                }
            };
            let (args, output_index) = if matches!(hook, HookInvocation::Config { .. }) {
                (vec![call.output], 0)
            } else {
                (vec![call.input, call.output], 1)
            };
            let original_args = args.clone();
            let result = match self.host.call_hook(name.as_str(), index, args).await {
                Ok(result) => result,
                Err(error) => {
                    let kind = error.kind();
                    let message = error.to_string();
                    self.disable(name, kind, error).await;
                    return Err(boxed(JsPluginHookFailure(message)));
                }
            };
            let output = match invocation_output(
                &result,
                output_index,
                self.host.plugin(),
                name.as_str(),
                &original_args,
            ) {
                Ok(output) => output,
                Err(error @ JsPluginBuildError::HostTruncatedHookArgument { .. }) => {
                    tracing::warn!(hook = %name, %error, "refused a hook mutation after host-side encoder loss");
                    continue;
                }
                Err(error) => {
                    let message = error.to_string();
                    self.disable(name, PluginDiagnosticKind::Protocol, error)
                        .await;
                    return Err(boxed(JsPluginHookFailure(message)));
                }
            };
            if let Err(error) = crate::jsonrpc::apply_hook_output(hook, output) {
                let message = error.to_string();
                self.disable(name, PluginDiagnosticKind::Protocol, error)
                    .await;
                return Err(boxed(JsPluginHookFailure(message)));
            }
        }
        Ok(())
    }
}

fn boxed(error: impl std::error::Error + Send + Sync + 'static) -> BoxSource {
    Box::new(error)
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct JsPluginHookFailure(String);

fn plugin_tools(
    host: &JsHost,
    descriptors: &[Value],
    directory: PathBuf,
    worktree: PathBuf,
) -> Result<PluginTools, JsPluginBuildError> {
    let mut tools = PluginTools::new();
    for (index, descriptor) in descriptors.iter().enumerate() {
        let id = descriptor
            .get("id")
            .and_then(Value::as_str)
            .ok_or(JsPluginBuildError::InvalidToolDescriptor { index })?
            .to_owned();
        let description = descriptor
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if host
            .handle(descriptor.get("execute").unwrap_or(&Value::Null))
            .is_none()
        {
            return Err(JsPluginBuildError::InvalidToolDescriptor { index });
        }
        let properties = descriptor
            .get("args")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|name| (name.to_owned(), json!({})))
            .collect::<Map<_, _>>();
        let parameters = json!({
            "type": "object",
            "properties": properties,
            "additionalProperties": false
        });
        tools.insert(
            id.clone(),
            Arc::new(JsPluginTool {
                id,
                description,
                parameters,
                host: host.clone(),
                index,
                directory: directory.clone(),
                worktree: worktree.clone(),
            }),
        );
    }
    Ok(tools)
}

struct JsPluginTool {
    id: String,
    description: String,
    parameters: Value,
    host: JsHost,
    index: usize,
    directory: PathBuf,
    worktree: PathBuf,
}

#[async_trait]
impl Tool for JsPluginTool {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn raw_parameters_schema(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, args: Value, context: ToolContext) -> Result<ToolOutput, ToolError> {
        let response = self
            .host
            .call_tool(
                &self.id,
                self.index,
                args,
                context,
                &self.directory,
                &self.worktree,
            )
            .await
            .map_err(|error| self.failed(error.to_string()))?;
        if let Some(error) = response.get("error") {
            return Err(self.failed(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin tool failed without detail"),
            ));
        }
        serde_json::from_value(response.get("result").cloned().unwrap_or(Value::Null))
            .map_err(|error| self.failed(format!("invalid plugin tool result: {error}")))
    }
}

impl JsPluginTool {
    fn failed(&self, detail: impl Into<String>) -> ToolError {
        ToolError::Failed {
            tool: self.id.clone(),
            source: Box::new(JsPluginToolFailure(detail.into())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("JavaScript plugin tool failed: {0}")]
struct JsPluginToolFailure(String);

fn invocation_output(
    result: &Value,
    index: usize,
    plugin: &str,
    hook: &str,
    original_arguments: &[Value],
) -> Result<Value, JsPluginBuildError> {
    let mut arguments = result
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .ok_or(JsPluginBuildError::MissingHookOutput)?;
    let truncation_metadata = result.get("truncations").unwrap_or(&Value::Null);
    for argument in 0..arguments.len() {
        if let Some(truncation) = super::bridge::encoded_truncations(truncation_metadata, argument)
            .into_iter()
            .find(|truncation| truncation.source == super::bridge::TruncationSource::Plugin)
        {
            return Err(JsPluginBuildError::TruncatedHookArgument {
                plugin: plugin.to_owned(),
                hook: hook.to_owned(),
                argument,
                path: truncation.path,
            });
        }
    }
    for (argument, value) in arguments.iter_mut().enumerate() {
        let encoded = super::bridge::encoded_truncations(truncation_metadata, argument);
        if let Some(path) = original_arguments
            .get(argument)
            .and_then(|original| super::bridge::restore_host_truncations(value, original, &encoded))
        {
            return Err(JsPluginBuildError::HostTruncatedHookArgument {
                hook: hook.to_owned(),
                argument,
                path,
            });
        }
        if let Some(path) = super::bridge::truncated_path(value) {
            return Err(JsPluginBuildError::TruncatedHookArgument {
                plugin: plugin.to_owned(),
                hook: hook.to_owned(),
                argument,
                path,
            });
        }
    }
    arguments
        .get(index)
        .cloned()
        .ok_or(JsPluginBuildError::MissingHookOutput)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum JsPluginBuildError {
    #[error(transparent)]
    UnknownHook(#[from] crate::UnknownHookName),
    #[error(transparent)]
    Manifest(#[from] crate::ManifestError),
    #[error(transparent)]
    Bridge(#[from] super::bridge::BridgeError),
    #[error("JavaScript plugin initialization report is unavailable")]
    MissingReport,
    #[error("JavaScript hook invocation omitted its mutated output")]
    MissingHookOutput,
    #[error(
        "plugin `{plugin}` truncated `{hook}` hook argument {argument} at `{path}`; \
         refusing to apply any hook mutation"
    )]
    TruncatedHookArgument {
        plugin: String,
        hook: String,
        argument: usize,
        path: String,
    },
    #[error(
        "JavaScript host truncated `{hook}` hook argument {argument} at `{path}` and could not \
         restore it; refusing to apply any hook mutation"
    )]
    HostTruncatedHookArgument {
        hook: String,
        argument: usize,
        path: String,
    },
    #[error("JavaScript plugin tool descriptor {index} is incomplete")]
    InvalidToolDescriptor { index: usize },
}
