//! Config-directory JavaScript tools and their resident-host bridge.
//!
//! schemars is the single source of truth for first-party Rust tools; JS-provided
//! tools carry their own schema across the bridge — two populations, not a
//! contradiction, and the registry treats them uniformly downstream.
//!
//! Zod validation stays in JavaScript, beside the schema that defines it. Rust
//! receives finished JSON Schema for provider exposure and a stable tool index;
//! every execution re-resolves that index after a host restart before invoking the
//! retained callable.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use oc_error::ToolError;
use oc_tool::{Tool, ToolContext, ToolOutput};
use oc_tools::registry::{CustomTool, CustomToolLoader, config_tool_id};
use serde_json::Value;

use crate::js::{
    JsHost, JsHostBuilder, JsHostConfig, JsHostLimits, JsPluginInput, JsRuntime, discover_runtime,
    discover_runtime_in,
};
use crate::{PluginToolConflict, validate_tool_names};

/// Why one config tool module was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigToolDiagnosticKind {
    /// Neither Bun nor Node was available.
    MissingRuntime,
    /// The module could not be imported or initialized.
    FailedToLoad,
    /// The module returned a descriptor the bridge could not use.
    Protocol,
}

/// A contained discovery or initialization failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigToolDiagnostic {
    /// The module that failed.
    pub path: PathBuf,
    /// The failure class.
    pub kind: ConfigToolDiagnosticKind,
    /// Human-readable detail from the runtime or bridge.
    pub message: String,
}

/// Preloaded config tools, their resident hosts, and isolated diagnostics.
pub struct ConfigToolLoad {
    directories: Vec<PathBuf>,
    tools: Vec<CustomTool>,
    origins: Vec<PathBuf>,
    hosts: Vec<JsHost>,
    diagnostics: Vec<ConfigToolDiagnostic>,
}

impl ConfigToolLoad {
    /// Successfully loaded tools in config-directory, `tool`/`tools`, file, then
    /// export order.
    #[must_use]
    pub fn tools(&self) -> &[CustomTool] {
        &self.tools
    }

    /// Failures isolated during discovery and initialization.
    #[must_use]
    pub fn diagnostics(&self) -> &[ConfigToolDiagnostic] {
        &self.diagnostics
    }

    /// Reject built-in or sibling collisions before registry assembly can make
    /// lookup order observable.
    pub fn validate_tool_names<'a>(
        &self,
        reserved: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), PluginToolConflict> {
        validate_tool_names(
            self.tools
                .iter()
                .zip(&self.origins)
                .map(|(tool, origin)| (tool.id(), Some(origin.as_path()))),
            reserved,
        )
    }

    /// Stop every resident module host.
    pub async fn shutdown(&self) {
        for host in &self.hosts {
            host.shutdown().await;
        }
    }
}

impl CustomToolLoader for ConfigToolLoad {
    fn config_directory_tools(&self, directories: &[PathBuf]) -> Vec<CustomTool> {
        if directories != self.directories {
            tracing::warn!(
                loaded = ?self.directories,
                requested = ?directories,
                "config tool loader used with a different directory chain"
            );
        }
        self.tools.clone()
    }

    fn plugin_tools(&self) -> Vec<CustomTool> {
        Vec::new()
    }
}

/// Discover and initialize `{tool,tools}/*.{js,ts}` independently of configured
/// plugins.
pub async fn load_config_directory_tools(
    directories: &[PathBuf],
    config: JsHostConfig,
) -> ConfigToolLoad {
    let entries = discover_tool_modules(directories);
    if entries.is_empty() {
        return ConfigToolLoad {
            directories: directories.to_vec(),
            tools: Vec::new(),
            origins: Vec::new(),
            hosts: Vec::new(),
            diagnostics: Vec::new(),
        };
    }

    let labels = entries
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    let runtime = match config.runtime_search_path.as_deref() {
        Some(path) => discover_runtime_in(Some(path), &labels),
        None => discover_runtime(&labels),
    };
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            return ConfigToolLoad {
                directories: directories.to_vec(),
                tools: Vec::new(),
                origins: Vec::new(),
                hosts: Vec::new(),
                diagnostics: entries
                    .into_iter()
                    .map(|path| ConfigToolDiagnostic {
                        path,
                        kind: ConfigToolDiagnosticKind::MissingRuntime,
                        message: error.to_string(),
                    })
                    .collect(),
            };
        }
    };

    let attempts = join_all(
        entries
            .into_iter()
            .map(|path| load_module(path, config.clone(), runtime.clone())),
    )
    .await;
    let mut tools = Vec::new();
    let mut origins = Vec::new();
    let mut hosts = Vec::new();
    let mut diagnostics = Vec::new();
    for attempt in attempts {
        match attempt {
            Ok((mut loaded, host)) => {
                origins.extend(std::iter::repeat_n(
                    PathBuf::from(host.plugin()),
                    loaded.len(),
                ));
                tools.append(&mut loaded);
                hosts.push(host);
            }
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }
    ConfigToolLoad {
        directories: directories.to_vec(),
        tools,
        origins,
        hosts,
        diagnostics,
    }
}

fn discover_tool_modules(directories: &[PathBuf]) -> Vec<PathBuf> {
    let mut modules = Vec::new();
    for root in directories {
        for name in ["tool", "tools"] {
            let mut entries = std::fs::read_dir(root.join(name))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .and_then(OsStr::to_str)
                        .is_some_and(|extension| matches!(extension, "js" | "ts"))
                        && path.metadata().is_ok_and(|metadata| metadata.is_file())
                })
                .collect::<Vec<_>>();
            entries.sort();
            modules.extend(entries);
        }
    }
    modules
}

async fn load_module(
    path: PathBuf,
    config: JsHostConfig,
    runtime: JsRuntime,
) -> Result<(Vec<CustomTool>, JsHost), ConfigToolDiagnostic> {
    let input = JsPluginInput {
        project: serde_json::json!({
            "id": config.project.id,
            "worktree": config.worktree,
            "vcs": config.project.vcs.as_ref().map(|_| "git"),
        }),
        directory: config.directory.clone(),
        worktree: config.worktree.clone(),
        server_url: config.server_url.to_string(),
        options: None,
        sdk_module: None,
        loopback_port: None,
    };
    let limits = JsHostLimits {
        memory_ceiling: (config.policy.memory_limit_mib as u64) * 1024 * 1024,
        hook_timeout: config.policy.hook_timeout,
        max_restarts: config.policy.max_restarts.try_into().unwrap_or(u32::MAX),
        ..JsHostLimits::default()
    };
    let host = JsHostBuilder::config_tool(runtime, &path, input)
        .with_limits(limits)
        .with_terminal_lease(Arc::clone(&config.terminal))
        .start()
        .await
        .map_err(|error| ConfigToolDiagnostic {
            path: path.clone(),
            kind: ConfigToolDiagnosticKind::FailedToLoad,
            message: error.to_string(),
        })?;
    let report = host.report().ok_or_else(|| ConfigToolDiagnostic {
        path: path.clone(),
        kind: ConfigToolDiagnosticKind::Protocol,
        message: "config tool host completed initialization without a report".to_owned(),
    })?;
    let mut tools = Vec::new();
    for (index, descriptor) in report.tools.iter().enumerate() {
        let export_id =
            required_string(descriptor, "export").map_err(|message| ConfigToolDiagnostic {
                path: path.clone(),
                kind: ConfigToolDiagnosticKind::Protocol,
                message,
            })?;
        let id = config_tool_id(&path, export_id).ok_or_else(|| ConfigToolDiagnostic {
            path: path.clone(),
            kind: ConfigToolDiagnosticKind::Protocol,
            message: "config tool module has no filename stem".to_owned(),
        })?;
        let description = required_string(descriptor, "description")
            .map(str::to_owned)
            .map_err(|message| ConfigToolDiagnostic {
                path: path.clone(),
                kind: ConfigToolDiagnosticKind::Protocol,
                message,
            })?;
        let parameters = descriptor
            .get("parameters")
            .filter(|value| value.is_object())
            .cloned()
            .ok_or_else(|| ConfigToolDiagnostic {
                path: path.clone(),
                kind: ConfigToolDiagnosticKind::Protocol,
                message: format!("config tool export `{export_id}` has no object JSON Schema"),
            })?;
        if host
            .handle(descriptor.get("execute").unwrap_or(&Value::Null))
            .is_none()
        {
            return Err(ConfigToolDiagnostic {
                path: path.clone(),
                kind: ConfigToolDiagnosticKind::Protocol,
                message: format!("config tool export `{export_id}` has no executable handle"),
            });
        }
        tools.push(Arc::new(ConfigDirectoryTool {
            id,
            description,
            parameters,
            host: host.clone(),
            index,
            directory: config.directory.clone(),
            worktree: config.worktree.clone(),
        }) as CustomTool);
    }
    Ok((tools, host))
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("config tool descriptor has no string `{field}` field"))
}

struct ConfigDirectoryTool {
    id: String,
    description: String,
    parameters: Value,
    host: JsHost,
    index: usize,
    directory: PathBuf,
    worktree: PathBuf,
}

#[async_trait]
impl Tool for ConfigDirectoryTool {
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
            let detail = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("config tool failed without detail")
                .to_owned();
            let source = Box::new(ConfigToolFailure(detail));
            return if error.get("kind").and_then(Value::as_str) == Some("invalid_args") {
                Err(ToolError::InvalidArgs {
                    tool: self.id.clone(),
                    source,
                })
            } else {
                Err(ToolError::Failed {
                    tool: self.id.clone(),
                    source,
                })
            };
        }
        serde_json::from_value(response.get("result").cloned().unwrap_or(Value::Null))
            .map_err(|error| self.failed(format!("invalid config tool result: {error}")))
    }
}

impl ConfigDirectoryTool {
    fn failed(&self, detail: impl Into<String>) -> ToolError {
        ToolError::Failed {
            tool: self.id.clone(),
            source: Box::new(ConfigToolFailure(detail.into())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("config-directory JavaScript tool failed: {0}")]
struct ConfigToolFailure(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_preserves_directory_and_singular_before_plural_order() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        for path in [
            first.path().join("tools/z.ts"),
            first.path().join("tool/b.js"),
            first.path().join("tool/a.ts"),
            second.path().join("tools/c.js"),
        ] {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
            std::fs::write(path, "export {};").expect("fixture");
        }

        let modules =
            discover_tool_modules(&[first.path().to_path_buf(), second.path().to_path_buf()]);
        let names = modules
            .iter()
            .map(|path| {
                path.strip_prefix(first.path())
                    .or_else(|_| path.strip_prefix(second.path()))
                    .expect("fixture root")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            ["tool/a.ts", "tool/b.js", "tools/z.ts", "tools/c.js"]
        );
    }
}
