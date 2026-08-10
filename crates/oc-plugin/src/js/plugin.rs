use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use oc_error::{BoxSource, ToolError};
use oc_tool::{Tool, ToolContext, ToolOutput};
use serde_json::{Map, Value, json};

use crate::{
    AuthHook, HookInvocation, HookName, Plugin, PluginManifest, PluginTools, ProviderHook,
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
        }))
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<JsDiagnostic> {
        self.host
            .diagnostics()
            .into_iter()
            .map(|diagnostic| JsDiagnostic {
                plugin: diagnostic.plugin,
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
}

#[async_trait]
impl Plugin for JsPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn auth(&self) -> Option<AuthHook> {
        self.auth.clone()
    }

    fn provider(&self) -> Option<ProviderHook> {
        self.provider.clone()
    }

    fn tools(&self) -> PluginTools {
        self.tools.clone()
    }

    async fn call(&self, hook: &mut HookInvocation<'_>) -> Result<(), BoxSource> {
        let name = hook.name();
        let count = self.callbacks.get(name.as_str()).copied().unwrap_or(0);
        for index in 0..count {
            let call = crate::jsonrpc::encode_hook(hook).map_err(boxed)?;
            let (args, output_index) = if matches!(hook, HookInvocation::Config { .. }) {
                (vec![call.output], 0)
            } else {
                (vec![call.input, call.output], 1)
            };
            let result = self.host.call_hook(name.as_str(), index, args).await;
            let Ok(result) = result else {
                continue;
            };
            let output = invocation_output(&result, output_index).map_err(boxed)?;
            crate::jsonrpc::apply_hook_output(hook, output).map_err(boxed)?;
        }
        Ok(())
    }
}

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

fn invocation_output(result: &Value, index: usize) -> Result<Value, JsPluginBuildError> {
    result
        .get("args")
        .and_then(Value::as_array)
        .and_then(|args| args.get(index))
        .cloned()
        .ok_or(JsPluginBuildError::MissingHookOutput)
}

fn boxed(error: impl std::error::Error + Send + Sync + 'static) -> BoxSource {
    Box::new(error)
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
    #[error("JavaScript plugin tool descriptor {index} is incomplete")]
    InvalidToolDescriptor { index: usize },
}
