use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use zuno_error::ToolError;
use zuno_runtime::{Component, EffectError, PrepareContext, ProfileBundle, RuntimeError};
use zuno_tool::{
    Tool, ToolConcurrencyPolicy, ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy,
    ToolUiIntent,
};

use crate::manifest::{
    PluginCapability, PluginRuntime, PluginToolConcurrency, PluginToolDefinition, PluginToolEffect,
    PluginToolReplay, PluginToolUiIntent,
};
use crate::{PackageOrigin, ResolvedExtensions};

mod process;
mod wasi;

/// Exact protocol negotiated by every executable plugin host.
pub const PLUGIN_PROTOCOL_VERSION: &str = "zuno.plugin/1";
/// Canonical Component Model interface implemented by WASI plugins.
pub const PLUGIN_WIT: &str = include_str!("../../../wit/zuno-plugin/plugin.wit");

const RUNTIME_BUNDLE_ID: &str = "zuno.extension-runtime";
const RUNTIME_COMPONENT_ID: &str = "zuno.extension-runtime.hosts";
const RUNTIME_EFFECT_ID: &str = "plugin-hosts";

/// Profile additions required by the currently resolved extension composition.
pub struct RuntimeSurface {
    tools: Vec<Arc<dyn Tool>>,
    bundle: Option<ProfileBundle>,
}

impl RuntimeSurface {
    /// Executable tool proxies in manifest order.
    #[must_use]
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Consume the lifecycle bundle, when at least one runtime is present.
    #[must_use]
    pub fn take_bundle(&mut self) -> Option<ProfileBundle> {
        self.bundle.take()
    }
}

/// Build executable tool proxies and the lifecycle component that owns their hosts.
pub fn runtime_surface(
    extensions: &ResolvedExtensions,
    workspace: &Path,
) -> Result<RuntimeSurface, RuntimeSurfaceError> {
    let hosts = Arc::new(PluginHostSet::default());
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut specs = Vec::new();
    for resolved in extensions.packages() {
        let Some(runtime) = &resolved.package.runtime else {
            continue;
        };
        let manifest = match &resolved.origin {
            PackageOrigin::Static { manifest } => manifest,
            PackageOrigin::Process => {
                return Err(RuntimeSurfaceError::DynamicExecutable {
                    package: resolved.package.id.clone(),
                });
            }
        };
        let root = manifest
            .parent()
            .ok_or_else(|| RuntimeSurfaceError::MissingPackageRoot {
                package: resolved.package.id.clone(),
                manifest: manifest.clone(),
            })?
            .to_path_buf();
        let spec = RuntimeSpec {
            package: resolved.package.id.clone(),
            root,
            workspace: workspace.to_path_buf(),
            runtime: runtime.clone(),
        };
        for (name, definition) in resolved.package.tools.iter() {
            tools.push(Arc::new(PluginTool::new(
                resolved.package.id.clone(),
                name.to_owned(),
                definition.clone(),
                Arc::clone(&hosts),
            )));
        }
        specs.push(spec);
    }
    let bundle = (!specs.is_empty()).then(|| {
        ProfileBundle::new(RUNTIME_BUNDLE_ID)
            .with_component(PluginRuntimeComponent::new(specs, hosts))
    });
    Ok(RuntimeSurface { tools, bundle })
}

/// Failure while projecting a resolved package set into executable hosts.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeSurfaceError {
    #[error(
        "process-local extension package `{package}` cannot declare executable runtime code; install it as a static package"
    )]
    DynamicExecutable { package: String },
    #[error(
        "extension package `{package}` manifest {} has no package directory",
        manifest.display()
    )]
    MissingPackageRoot { package: String, manifest: PathBuf },
}

#[derive(Clone)]
pub(crate) struct RuntimeSpec {
    pub(crate) package: String,
    pub(crate) root: PathBuf,
    pub(crate) workspace: PathBuf,
    pub(crate) runtime: PluginRuntime,
}

impl RuntimeSpec {
    pub(crate) fn timeout(&self) -> Duration {
        Duration::from_millis(self.runtime.timeout_ms())
    }

    pub(crate) fn capability_names(&self) -> Vec<String> {
        self.runtime
            .capabilities()
            .iter()
            .map(|capability| capability.as_str().to_owned())
            .collect()
    }
}

/// One runtime request after native tool validation and authorization.
#[derive(Clone)]
pub(crate) struct PluginInvocation {
    pub(crate) tool: String,
    pub(crate) arguments: Value,
    pub(crate) session_id: String,
    pub(crate) message_id: String,
    pub(crate) call_id: String,
    pub(crate) agent: String,
    pub(crate) interrupt: Arc<dyn zuno_tool::InterruptHandle>,
}

/// One successful runtime response.
pub(crate) struct PluginResult {
    pub(crate) title: String,
    pub(crate) output: String,
    pub(crate) metadata: Map<String, Value>,
}

/// Runtime provider interface shared by the WASI and process hosts.
#[async_trait]
pub(crate) trait PluginHost: Send + Sync {
    async fn invoke(&self, request: PluginInvocation) -> Result<PluginResult, PluginHostError>;
    async fn shutdown(&self) -> Result<(), PluginHostError>;
}

/// Typed failure from an executable plugin boundary.
#[derive(Debug, thiserror::Error)]
pub enum PluginHostError {
    #[error("plugin `{package}` failed to start: {message}")]
    Start { package: String, message: String },
    #[error("plugin `{package}` is protocol-incompatible: {message}")]
    Incompatible { package: String, message: String },
    #[error("plugin `{package}` tool `{tool}` failed: {message}")]
    Failed {
        package: String,
        tool: String,
        message: String,
    },
    #[error("plugin `{package}` {operation} timed out after {elapsed:?}")]
    Timeout {
        package: String,
        operation: String,
        elapsed: Duration,
    },
    #[error("plugin `{package}` tool `{tool}` was cancelled")]
    Cancelled { package: String, tool: String },
    #[error("plugin `{package}` outcome is uncertain during {operation}: {message}")]
    Uncertain {
        package: String,
        operation: String,
        message: String,
    },
    #[error("plugin `{package}` failed to stop authoritatively: {message}")]
    Stop { package: String, message: String },
}

impl PluginHostError {
    /// Whether authoritative state must be inspected before any replay.
    #[must_use]
    pub const fn is_uncertain(&self) -> bool {
        matches!(self, Self::Uncertain { .. } | Self::Stop { .. })
    }
}

#[derive(Default)]
struct PluginHostSet {
    active: RwLock<BTreeMap<String, Arc<dyn PluginHost>>>,
}

impl PluginHostSet {
    fn publish(&self, hosts: &[ActiveHost]) -> Result<(), EffectError> {
        let mut active = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !active.is_empty() {
            return Err(EffectError::new(
                "plugin host set was already published before lifecycle start",
            ));
        }
        active.extend(
            hosts
                .iter()
                .map(|entry| (entry.package.clone(), Arc::clone(&entry.host))),
        );
        Ok(())
    }

    fn withdraw(&self) {
        self.active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    async fn invoke(
        &self,
        package: &str,
        request: PluginInvocation,
    ) -> Result<PluginResult, PluginHostError> {
        let host = self
            .active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(package)
            .cloned()
            .ok_or_else(|| PluginHostError::Uncertain {
                package: package.to_owned(),
                operation: "tool dispatch".to_owned(),
                message: "the profile no longer owns an active runtime host".to_owned(),
            })?;
        host.invoke(request).await
    }
}

struct PluginRuntimeComponent {
    specs: Vec<RuntimeSpec>,
    hosts: Arc<PluginHostSet>,
}

impl PluginRuntimeComponent {
    fn new(specs: Vec<RuntimeSpec>, hosts: Arc<PluginHostSet>) -> Self {
        Self { specs, hosts }
    }
}

#[async_trait]
impl Component for PluginRuntimeComponent {
    fn id(&self) -> &str {
        RUNTIME_COMPONENT_ID
    }

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        let specs = self.specs.clone();
        let hosts = Arc::clone(&self.hosts);
        context.effect(RUNTIME_EFFECT_ID, move || async move {
            let active = start_hosts(specs).await?;
            if let Err(error) = hosts.publish(&active) {
                let cleanup = stop_hosts(active).await;
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(EffectError::new(format!(
                        "{error}; unpublished plugin hosts also failed cleanup: {cleanup}"
                    ))),
                };
            }
            Ok(move || async move {
                hosts.withdraw();
                stop_hosts(active).await
            })
        })
    }
}

struct ActiveHost {
    package: String,
    host: Arc<dyn PluginHost>,
}

async fn start_hosts(specs: Vec<RuntimeSpec>) -> Result<Vec<ActiveHost>, EffectError> {
    let mut active: Vec<ActiveHost> = Vec::new();
    for spec in specs {
        let package = spec.package.clone();
        let started = match &spec.runtime {
            PluginRuntime::Wasi { .. } => wasi::start(spec).await,
            PluginRuntime::Process { .. } => process::start(spec).await,
        };
        match started {
            Ok(host) => active.push(ActiveHost { package, host }),
            Err(error) => {
                let cleanup = stop_hosts(active).await;
                return match cleanup {
                    Ok(()) => Err(EffectError::new(error.to_string())),
                    Err(cleanup) => Err(EffectError::new(format!(
                        "{error}; earlier plugin hosts also failed cleanup: {cleanup}"
                    ))),
                };
            }
        }
    }
    Ok(active)
}

async fn stop_hosts(mut active: Vec<ActiveHost>) -> Result<(), EffectError> {
    let mut failures = Vec::new();
    while let Some(entry) = active.pop() {
        if let Err(error) = entry.host.shutdown().await {
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(EffectError::new(failures.join("; ")))
    }
}

struct PluginTool {
    package: String,
    name: String,
    definition: PluginToolDefinition,
    hosts: Arc<PluginHostSet>,
}

impl PluginTool {
    fn new(
        package: String,
        name: String,
        definition: PluginToolDefinition,
        hosts: Arc<PluginHostSet>,
    ) -> Self {
        Self {
            package,
            name,
            definition,
            hosts,
        }
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn id(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        match self.definition.replay {
            PluginToolReplay::Never => ToolReplayPolicy::Never,
            PluginToolReplay::Safe => ToolReplayPolicy::Safe,
        }
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        match self.definition.concurrency {
            PluginToolConcurrency::Exclusive => ToolConcurrencyPolicy::Exclusive,
            PluginToolConcurrency::ParallelSafe => ToolConcurrencyPolicy::ParallelSafe,
            PluginToolConcurrency::IsolatedBackground => ToolConcurrencyPolicy::IsolatedBackground,
        }
    }

    fn ui_intent(&self) -> ToolUiIntent {
        match self.definition.ui_intent {
            PluginToolUiIntent::Generic => ToolUiIntent::Generic,
            PluginToolUiIntent::Subagent => ToolUiIntent::Subagent,
        }
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        match self.definition.effect {
            PluginToolEffect::ReadOnly => ToolEffect::ReadOnly,
            PluginToolEffect::UserMediated => ToolEffect::UserMediated,
            PluginToolEffect::Delegating => ToolEffect::Delegating,
            PluginToolEffect::SideEffecting => ToolEffect::SideEffecting,
        }
    }

    fn raw_parameters_schema(&self) -> Value {
        self.definition.parameters.clone()
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let request = PluginInvocation {
            tool: self.name.clone(),
            arguments: args,
            session_id: ctx.session_id,
            message_id: ctx.message_id,
            call_id: ctx.call_id,
            agent: ctx.agent,
            interrupt: ctx.interrupt,
        };
        let result = self
            .hosts
            .invoke(&self.package, request)
            .await
            .map_err(|source| plugin_tool_error(self.name.clone(), source))?;
        Ok(ToolOutput {
            title: result.title,
            output: result.output,
            metadata: result.metadata,
            attachments: Vec::new(),
        })
    }
}

fn plugin_tool_error(tool: String, source: PluginHostError) -> ToolError {
    if source.is_uncertain() || matches!(&source, PluginHostError::Timeout { .. }) {
        return ToolError::Transient {
            tool,
            retry_after: None,
            source: Box::new(source),
        };
    }
    ToolError::Failed {
        tool,
        source: Box::new(source),
    }
}

pub(crate) fn capabilities_contain(
    capabilities: &[PluginCapability],
    needle: PluginCapability,
) -> bool {
    capabilities.contains(&needle)
}

#[derive(Clone)]
pub(crate) struct SecretRedactor {
    secrets: Vec<String>,
}

impl SecretRedactor {
    pub(crate) fn from_process() -> Self {
        let secrets = std::env::vars_os()
            .filter_map(|(name, value)| {
                let name = name.to_string_lossy().to_ascii_uppercase();
                let sensitive = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
                    .iter()
                    .any(|marker| name.contains(marker));
                let value = value.to_string_lossy();
                (sensitive && value.len() >= 4).then(|| value.into_owned())
            })
            .collect();
        Self { secrets }
    }

    pub(crate) fn safe(&self, value: impl AsRef<str>) -> String {
        let mut safe = value.as_ref().to_owned();
        for secret in &self.secrets {
            safe = safe.replace(secret, "[REDACTED]");
        }
        safe.lines()
            .map(|line| {
                let lower = line.to_ascii_lowercase();
                if lower.contains("authorization: bearer ")
                    || lower.contains("api_key=")
                    || lower.contains("apikey=")
                {
                    "[REDACTED CREDENTIAL LINE]".to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_and_timeout_host_failures_reach_typed_tool_recovery() {
        for source in [
            PluginHostError::Uncertain {
                package: "review-kit".to_owned(),
                operation: "tools/call".to_owned(),
                message: "reply was lost".to_owned(),
            },
            PluginHostError::Timeout {
                package: "review-kit".to_owned(),
                operation: "tools/call".to_owned(),
                elapsed: Duration::from_secs(30),
            },
        ] {
            let error = plugin_tool_error("review".to_owned(), source);
            assert!(error.is_retryable(), "{error}");
            assert!(matches!(error, ToolError::Transient { .. }));
        }
    }

    #[test]
    fn authoritative_plugin_rejections_remain_terminal_tool_failures() {
        let error = plugin_tool_error(
            "review".to_owned(),
            PluginHostError::Failed {
                package: "review-kit".to_owned(),
                tool: "review".to_owned(),
                message: "invalid repository".to_owned(),
            },
        );
        assert!(!error.is_retryable());
        assert!(matches!(error, ToolError::Failed { .. }));
    }
}
