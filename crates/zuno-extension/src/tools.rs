use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use zuno_error::ToolError;
use zuno_tool::{Tool, ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy, TypedTool, erase};

use crate::{
    ExtensionRegistry, Package, PackageOrigin, Scope, StageOutcome, StaticPackage, resolve_active,
};

/// Lifecycle tools for one workspace and one process registry.
#[must_use]
pub fn lifecycle_tools(
    scope: Scope,
    static_packages: Vec<StaticPackage>,
    registry: Arc<ExtensionRegistry>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        erase(InspectTool::new(
            scope.clone(),
            static_packages.clone(),
            Arc::clone(&registry),
        )),
        erase(DefineTool::new(
            scope.clone(),
            static_packages.clone(),
            Arc::clone(&registry),
        )),
        erase(RunTool::new(
            scope.clone(),
            static_packages,
            Arc::clone(&registry),
        )),
        erase(StopTool::new(scope.clone(), Arc::clone(&registry))),
        erase(UndefineTool::new(scope, registry)),
    ]
}

#[derive(Clone)]
struct InspectTool {
    scope: Scope,
    static_packages: Vec<StaticPackage>,
    registry: Arc<ExtensionRegistry>,
}

impl InspectTool {
    fn new(
        scope: Scope,
        static_packages: Vec<StaticPackage>,
        registry: Arc<ExtensionRegistry>,
    ) -> Self {
        Self {
            scope,
            static_packages,
            registry,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InspectParams {
    /// Optional package id. Omit it to inspect every package in this workspace.
    #[serde(default)]
    id: Option<String>,
}

#[async_trait]
impl TypedTool for InspectTool {
    type Params = InspectParams;

    fn id(&self) -> &str {
        "extension_inspect"
    }

    fn description(&self) -> &str {
        "Inspect static and process-local Zuno extension packages and lifecycle state."
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn run(&self, params: InspectParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let active = resolve_active(&self.scope, &self.static_packages, &self.registry)
            .map_err(|error| failed(self.id(), error))?;
        let mut statuses = active
            .packages()
            .iter()
            .filter_map(|entry| match &entry.origin {
                PackageOrigin::Static { manifest } => Some((entry, manifest)),
                PackageOrigin::Process => None,
            })
            .map(|(entry, manifest)| {
                json!({
                    "id": entry.package.id,
                    "description": entry.package.description,
                    "state": "running",
                    "source": manifest_source(&entry.package.id, manifest),
                    "agents": entry.package.agents.keys().collect::<Vec<_>>(),
                    "workflows": entry.package.workflows.keys().collect::<Vec<_>>(),
                    "skills": entry.package.skills.iter().map(|skill| &skill.name).collect::<Vec<_>>(),
                    "tools": entry.package.tools.keys().collect::<Vec<_>>(),
                    "runtime": entry.package.runtime.as_ref().map(|runtime| match runtime {
                        crate::PluginRuntime::Wasi { .. } => "wasi",
                        crate::PluginRuntime::Process { .. } => "process",
                    })
                })
            })
            .collect::<Vec<_>>();
        for status in self.registry.dynamic_statuses(&self.scope) {
            statuses.push(json!({
                "id": status.id,
                "description": status.description,
                "state": status.state,
                "source": {"lifetime": "process"},
                "agents": status.agents,
                "workflows": status.workflows,
                "skills": status.skills,
                "tools": status.tools,
                "runtime": status.runtime
            }));
        }
        if let Some(id) = params.id {
            statuses.retain(|status| status["id"] == id);
        }
        let output =
            serde_json::to_string_pretty(&statuses).map_err(|error| failed(self.id(), error))?;
        Ok(ToolOutput::text("Zuno extensions", output)
            .with_metadata("count", json!(statuses.len())))
    }
}

/// The `source` object `extension_inspect` reports for a statically loaded package.
///
/// The manifest path crosses this boundary byte for byte, never through a display
/// rendering. `zuno_paths::wire_path` is `display_path(path).replace('\\', "/")` on every
/// platform, and on Linux and macOS `\` is an ordinary filename byte: folding it reports a
/// package under a directory the user named `zuno\ws` at `.../zuno/ws/...`, a different and
/// possibly existing path. This field is model-visible prose the model acts on — the step
/// after inspecting a package is to `read` or `grep` the manifest it names — so it carries
/// the same single rule as `crate::host::plugin_path_literal`: the exact bytes or nothing.
///
/// A path that is not valid UTF-8 has no JSON spelling at all, so it is reported as `null`
/// beside `manifestUnrepresentable`, never substituted with U+FFFD. Inspection is read-only
/// and still lists every other package and every other field of this one, so an
/// unrepresentable path is an unresolvable field rather than a failed call; the operator
/// gets the lossy rendering in a log, where nothing acts on it.
fn manifest_source(package: &str, manifest: &Path) -> Value {
    match manifest.to_str() {
        Some(literal) => json!({"lifetime": "static", "manifest": literal}),
        None => {
            tracing::warn!(
                package,
                manifest = %zuno_paths::display_path(manifest),
                "extension manifest path is not valid UTF-8; inspection reports it as unresolvable"
            );
            json!({
                "lifetime": "static",
                "manifest": null,
                "manifestUnrepresentable": true,
            })
        }
    }
}

#[derive(Clone)]
struct DefineTool {
    scope: Scope,
    static_ids: Vec<String>,
    registry: Arc<ExtensionRegistry>,
}

impl DefineTool {
    fn new(
        scope: Scope,
        static_packages: Vec<StaticPackage>,
        registry: Arc<ExtensionRegistry>,
    ) -> Self {
        Self {
            scope,
            static_ids: static_packages
                .iter()
                .map(|entry| entry.package().id.clone())
                .collect(),
            registry,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DefineParams {
    /// Complete `zuno.extension/v1` package. It is recorded only in process memory.
    package: Package,
}

#[async_trait]
impl TypedTool for DefineTool {
    type Params = DefineParams;

    fn id(&self) -> &str {
        "extension_define"
    }

    fn description(&self) -> &str {
        "Define an immutable process-local Zuno extension package without activating it or writing disk."
    }

    async fn run(&self, params: DefineParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if self.static_ids.contains(&params.package.id) {
            return Err(failed(
                self.id(),
                std::io::Error::other(format!(
                    "package `{}` is already loaded statically",
                    params.package.id
                )),
            ));
        }
        let id = params.package.id.clone();
        self.registry
            .define(&self.scope, params.package)
            .map_err(|error| failed(self.id(), error))?;
        Ok(ToolOutput::text(
            "Extension defined",
            format!(
                "Defined process-local package `{id}` in inactive state. Call \
                 `extension_run` to activate it. The definition disappears when this \
                 Zuno process exits."
            ),
        )
        .with_metadata("package", Value::String(id)))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdParams {
    /// Process-local package id.
    id: String,
}

#[derive(Clone)]
struct RunTool {
    scope: Scope,
    static_packages: Vec<StaticPackage>,
    registry: Arc<ExtensionRegistry>,
}

impl RunTool {
    fn new(
        scope: Scope,
        static_packages: Vec<StaticPackage>,
        registry: Arc<ExtensionRegistry>,
    ) -> Self {
        Self {
            scope,
            static_packages,
            registry,
        }
    }
}

#[async_trait]
impl TypedTool for RunTool {
    type Params = IdParams;

    fn id(&self) -> &str {
        "extension_run"
    }

    fn description(&self) -> &str {
        "Activate a defined process-local extension transactionally for following turns."
    }

    async fn run(&self, params: IdParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let outcome = self
            .registry
            .stage_run(&self.scope, &params.id, &self.static_packages)
            .map_err(|error| failed(self.id(), error))?;
        let pending = outcome.is_pending();
        let body = if pending {
            format!(
                "Activation of process-local package `{}` is scheduled as composition revision \
                 {}. It becomes active only after the current host stops and the replacement \
                 starts successfully.",
                params.id,
                outcome.revision()
            )
        } else {
            format!("Process-local package `{}` is already running.", params.id)
        };
        Ok(ToolOutput::text("Extension activation scheduled", body)
            .with_metadata("package", Value::String(params.id))
            .with_metadata("revision", json!(outcome.revision()))
            .with_metadata("pending", json!(pending)))
    }
}

#[derive(Clone)]
struct StopTool {
    scope: Scope,
    registry: Arc<ExtensionRegistry>,
}

impl StopTool {
    fn new(scope: Scope, registry: Arc<ExtensionRegistry>) -> Self {
        Self { scope, registry }
    }
}

#[async_trait]
impl TypedTool for StopTool {
    type Params = IdParams;

    fn id(&self) -> &str {
        "extension_stop"
    }

    fn description(&self) -> &str {
        "Deactivate a process-local extension while retaining its immutable definition."
    }

    async fn run(&self, params: IdParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let outcome = self
            .registry
            .stage_stop(&self.scope, &params.id)
            .map_err(|error| failed(self.id(), error))?;
        let pending = matches!(outcome, StageOutcome::Pending(_));
        let body = if pending {
            format!(
                "Deactivation of process-local package `{}` is scheduled as composition revision \
                 {}. Its contributions remain active until the old host stops cleanly.",
                params.id,
                outcome.revision()
            )
        } else {
            format!(
                "Process-local package `{}` is inactive; its definition remains available.",
                params.id
            )
        };
        Ok(ToolOutput::text("Extension deactivation scheduled", body)
            .with_metadata("package", Value::String(params.id))
            .with_metadata("revision", json!(outcome.revision()))
            .with_metadata("pending", json!(pending)))
    }
}

#[derive(Clone)]
struct UndefineTool {
    scope: Scope,
    registry: Arc<ExtensionRegistry>,
}

impl UndefineTool {
    fn new(scope: Scope, registry: Arc<ExtensionRegistry>) -> Self {
        Self { scope, registry }
    }
}

#[async_trait]
impl TypedTool for UndefineTool {
    type Params = IdParams;

    fn id(&self) -> &str {
        "extension_undefine"
    }

    fn description(&self) -> &str {
        "Remove a process-local extension definition from the current Zuno process."
    }

    async fn run(&self, params: IdParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let outcome = self
            .registry
            .stage_undefine(&self.scope, &params.id)
            .map_err(|error| failed(self.id(), error))?;
        let pending = outcome.is_pending();
        let body = if pending {
            format!(
                "Removal of process-local package `{}` is scheduled as composition revision {}. \
                 The definition is removed only after its active owner stops cleanly.",
                params.id,
                outcome.revision()
            )
        } else {
            format!(
                "Removed inactive process-local package `{}` from this Zuno process.",
                params.id
            )
        };
        Ok(ToolOutput::text("Extension removal scheduled", body)
            .with_metadata("package", Value::String(params.id))
            .with_metadata("revision", json!(outcome.revision()))
            .with_metadata("pending", json!(pending)))
    }
}

fn failed(tool: &str, source: impl std::error::Error + Send + Sync + 'static) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(source),
    }
}
