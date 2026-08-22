use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use zuno_error::ToolError;
use zuno_tool::{Tool, ToolContext, ToolOutput, ToolReplayPolicy, TypedTool, erase};

use crate::{
    DynamicState, ExtensionRegistry, Package, PackageOrigin, Scope, StaticPackage, resolve_active,
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

    async fn run(&self, params: InspectParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let active = resolve_active(&self.scope, &self.static_packages, &self.registry)
            .map_err(|error| failed(self.id(), error))?;
        let mut statuses = active
            .packages()
            .iter()
            .map(|entry| {
                let source = match &entry.origin {
                    PackageOrigin::Static { manifest } => {
                        json!({"lifetime": "static", "manifest": manifest})
                    }
                    PackageOrigin::Process => json!({"lifetime": "process"}),
                };
                json!({
                    "id": entry.package.id,
                    "description": entry.package.description,
                    "state": "running",
                    "source": source,
                    "agents": entry.package.agents.keys().collect::<Vec<_>>(),
                    "workflows": entry.package.workflows.keys().collect::<Vec<_>>(),
                    "skills": entry.package.skills.iter().map(|skill| &skill.name).collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        for status in self.registry.dynamic_statuses(&self.scope) {
            if status.state == DynamicState::Running {
                continue;
            }
            statuses.push(json!({
                "id": status.id,
                "description": status.description,
                "state": status.state,
                "source": {"lifetime": "process"},
                "agents": status.agents,
                "workflows": status.workflows,
                "skills": status.skills
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
        self.registry
            .run_with_static(&self.scope, &params.id, &self.static_packages)
            .map_err(|error| failed(self.id(), error))?;
        Ok(ToolOutput::text(
            "Extension running",
            format!(
                "Activated process-local package `{}`. Its agents, workflows, and \
                 skills are available after the host refreshes for the next turn.",
                params.id
            ),
        )
        .with_metadata("package", Value::String(params.id)))
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
        self.registry
            .stop(&self.scope, &params.id)
            .map_err(|error| failed(self.id(), error))?;
        Ok(ToolOutput::text(
            "Extension stopped",
            format!(
                "Stopped process-local package `{}`. Its definition remains available \
                 to `extension_inspect` and can be run again.",
                params.id
            ),
        )
        .with_metadata("package", Value::String(params.id)))
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
        self.registry
            .undefine(&self.scope, &params.id)
            .map_err(|error| failed(self.id(), error))?;
        Ok(ToolOutput::text(
            "Extension undefined",
            format!(
                "Removed process-local package `{}` from this Zuno process.",
                params.id
            ),
        )
        .with_metadata("package", Value::String(params.id)))
    }
}

fn failed(tool: &str, source: impl std::error::Error + Send + Sync + 'static) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(source),
    }
}
