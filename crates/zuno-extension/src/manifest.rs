use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};
use zuno_config::schema::agent::AgentConfig;
use zuno_config::schema::ordered::OrderedMap;

/// The only extension manifest version this build accepts.
pub const API_VERSION: &str = "zuno.extension/v1";
/// Default execution budget for one WASI call.
pub const DEFAULT_WASI_FUEL: u64 = 10_000_000;
/// Default maximum linear-memory size for one WASI plugin instance.
pub const DEFAULT_WASI_MEMORY_MIB: u64 = 64;
/// Default wall-clock budget for plugin lifecycle and tool calls.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// One declarative Zuno extension package.
///
/// Agents, workflows, and skills are native catalog inputs and therefore pass
/// through the same validation and consumers as their file/config equivalents.
/// A static package may additionally expose executable tools through one isolated
/// runtime. Runtime code never crosses a Rust dynamic-library ABI.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    /// Must be [`API_VERSION`].
    pub api_version: String,
    /// Stable package identifier. A process registry never mutates a definition.
    pub id: String,
    /// Human-readable purpose shown by `extension_inspect`.
    pub description: String,
    /// Agents contributed while the package is active.
    #[serde(default, skip_serializing_if = "OrderedMap::is_empty")]
    pub agents: OrderedMap<AgentConfig>,
    /// Slash-command workflows contributed while the package is active.
    #[serde(default, skip_serializing_if = "OrderedMap::is_empty")]
    pub workflows: OrderedMap<WorkflowDefinition>,
    /// Skills contributed while the package is active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillDefinition>,
    /// Runtime used by executable tool contributions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<PluginRuntime>,
    /// Executable tools proxied into the native registry.
    #[serde(default, skip_serializing_if = "OrderedMap::is_empty")]
    pub tools: OrderedMap<PluginToolDefinition>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PackageWire {
    api_version: String,
    id: String,
    description: String,
    #[serde(default)]
    agents: OrderedMap<AgentConfig>,
    #[serde(default)]
    workflows: OrderedMap<WorkflowDefinition>,
    #[serde(default)]
    skills: Vec<SkillDefinition>,
    #[serde(default)]
    runtime: Option<PluginRuntime>,
    #[serde(default)]
    tools: OrderedMap<PluginToolDefinition>,
}

impl<'de> Deserialize<'de> for Package {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = PackageWire::deserialize(deserializer)?;
        let package = Self {
            api_version: wire.api_version,
            id: wire.id,
            description: wire.description,
            agents: wire.agents,
            workflows: wire.workflows,
            skills: wire.skills,
            runtime: wire.runtime,
            tools: wire.tools,
        };
        package.validate().map_err(de::Error::custom)?;
        Ok(package)
    }
}

impl Package {
    /// Validate one package identifier without constructing a complete manifest.
    pub fn validate_id(id: &str) -> Result<(), ManifestError> {
        validate_package_id(id)
    }

    /// Validate values that JSON Schema alone cannot express clearly.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.api_version != API_VERSION {
            return Err(ManifestError::ApiVersion {
                package: self.id.clone(),
                found: self.api_version.clone(),
            });
        }
        validate_package_id(&self.id)?;
        if self.description.trim().is_empty() {
            return Err(ManifestError::EmptyDescription(self.id.clone()));
        }
        if self.agents.is_empty()
            && self.workflows.is_empty()
            && self.skills.is_empty()
            && self.runtime.is_none()
            && self.tools.is_empty()
        {
            return Err(ManifestError::EmptyPackage(self.id.clone()));
        }
        if !self.tools.is_empty() && self.runtime.is_none() {
            return Err(ManifestError::ToolsWithoutRuntime(self.id.clone()));
        }
        if self.runtime.is_some() && self.tools.is_empty() {
            return Err(ManifestError::RuntimeWithoutTools(self.id.clone()));
        }
        if let Some(runtime) = &self.runtime {
            runtime.validate(&self.id)?;
        }
        for (name, agent) in self.agents.iter() {
            validate_contribution_name("agent", name)?;
            if agent.extra.contains_key("name") {
                return Err(ManifestError::AgentRename {
                    package: self.id.clone(),
                    agent: name.to_owned(),
                });
            }
            if agent.disable == Some(true) {
                return Err(ManifestError::DisabledAgent {
                    package: self.id.clone(),
                    agent: name.to_owned(),
                });
            }
        }
        for (name, workflow) in self.workflows.iter() {
            validate_contribution_name("workflow", name)?;
            if workflow.prompt.trim().is_empty() {
                return Err(ManifestError::EmptyWorkflow {
                    package: self.id.clone(),
                    workflow: name.to_owned(),
                });
            }
        }
        for skill in &self.skills {
            validate_skill_name(&skill.name)?;
            if skill.description.trim().is_empty() {
                return Err(ManifestError::EmptySkillField {
                    skill: skill.name.clone(),
                    field: "description",
                });
            }
            if skill.content.trim().is_empty() {
                return Err(ManifestError::EmptySkillField {
                    skill: skill.name.clone(),
                    field: "content",
                });
            }
        }
        for (name, tool) in self.tools.iter() {
            validate_contribution_name("tool", name)?;
            tool.validate(&self.id, name)?;
            if let Some(runtime) = &self.runtime {
                runtime.validate_tool_policy(&self.id, name, tool)?;
            }
        }
        Ok(())
    }
}

/// The executable boundary for one static extension package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PluginRuntime {
    /// A WebAssembly component hosted in-process under explicit WASI grants.
    Wasi {
        /// Component artifact relative to the package directory.
        artifact: String,
        /// Host capabilities granted to the component.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<PluginCapability>,
        /// Exact process environment names copied into the WASI context.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        environment: Vec<String>,
        /// Fuel replenished before every exported call.
        #[serde(default = "default_wasi_fuel")]
        fuel: u64,
        /// Maximum size of each guest linear memory.
        #[serde(default = "default_wasi_memory_mib")]
        #[serde(rename = "memoryMiB")]
        memory_mib: u64,
        /// Wall-clock budget for initialize, invoke, and shutdown.
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    /// A contained child process speaking Zuno's line-delimited JSON-RPC protocol.
    Process {
        /// Executable name, absolute path, or path relative to the package directory.
        command: String,
        /// Arguments placed before the protocol stream begins.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Must be exactly `host.full`; an OS process cannot enforce narrower grants.
        capabilities: Vec<PluginCapability>,
        /// Wall-clock budget for initialize, invoke, and shutdown.
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
}

impl PluginRuntime {
    /// The configured call timeout.
    #[must_use]
    pub const fn timeout_ms(&self) -> u64 {
        match self {
            Self::Wasi { timeout_ms, .. } | Self::Process { timeout_ms, .. } => *timeout_ms,
        }
    }

    /// Declared capabilities in manifest order.
    #[must_use]
    pub fn capabilities(&self) -> &[PluginCapability] {
        match self {
            Self::Wasi { capabilities, .. } | Self::Process { capabilities, .. } => capabilities,
        }
    }

    fn validate(&self, package: &str) -> Result<(), ManifestError> {
        let timeout_ms = self.timeout_ms();
        if !(1..=300_000).contains(&timeout_ms) {
            return Err(ManifestError::InvalidRuntimeBudget {
                package: package.to_owned(),
                field: "timeoutMs",
                value: timeout_ms,
            });
        }
        validate_unique_capabilities(package, self.capabilities())?;
        match self {
            Self::Wasi {
                artifact,
                capabilities,
                environment,
                fuel,
                memory_mib,
                ..
            } => {
                validate_relative_artifact(package, artifact)?;
                if !(10_000..=1_000_000_000).contains(fuel) {
                    return Err(ManifestError::InvalidRuntimeBudget {
                        package: package.to_owned(),
                        field: "fuel",
                        value: *fuel,
                    });
                }
                if !(8..=1_024).contains(memory_mib) {
                    return Err(ManifestError::InvalidRuntimeBudget {
                        package: package.to_owned(),
                        field: "memoryMiB",
                        value: *memory_mib,
                    });
                }
                if capabilities.contains(&PluginCapability::HostFull) {
                    return Err(ManifestError::InvalidRuntimeCapability {
                        package: package.to_owned(),
                        message: "`host.full` is reserved for process plugins".to_owned(),
                    });
                }
                validate_environment_names(package, environment)?;
            }
            Self::Process {
                command,
                capabilities,
                ..
            } => {
                if command.trim().is_empty() {
                    return Err(ManifestError::EmptyRuntimeField {
                        package: package.to_owned(),
                        field: "command",
                    });
                }
                if capabilities.as_slice() != [PluginCapability::HostFull] {
                    return Err(ManifestError::InvalidRuntimeCapability {
                        package: package.to_owned(),
                        message: "process plugins must declare exactly `[\"host.full\"]`"
                            .to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_tool_policy(
        &self,
        package: &str,
        tool: &str,
        definition: &PluginToolDefinition,
    ) -> Result<(), ManifestError> {
        if definition.effect != PluginToolEffect::ReadOnly {
            return Ok(());
        }
        let enforceably_read_only = match self {
            Self::Wasi { capabilities, .. } => !capabilities.iter().any(|capability| {
                matches!(
                    capability,
                    PluginCapability::WorkspaceWrite
                        | PluginCapability::Network
                        | PluginCapability::HostFull
                )
            }),
            Self::Process { .. } => false,
        };
        if enforceably_read_only {
            Ok(())
        } else {
            Err(ManifestError::UnsupportedToolPolicy {
                package: package.to_owned(),
                tool: tool.to_owned(),
                message:
                    "this runtime can mutate host or remote state, so the host cannot enforce \
                          `effect: readOnly`; use `sideEffecting` with `replay: never`"
                        .to_owned(),
            })
        }
    }
}

const fn default_wasi_fuel() -> u64 {
    DEFAULT_WASI_FUEL
}

const fn default_wasi_memory_mib() -> u64 {
    DEFAULT_WASI_MEMORY_MIB
}

const fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// Capabilities a runtime package may request.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum PluginCapability {
    /// Read the package's selected workspace through the `/workspace` preopen.
    #[serde(rename = "workspace.read")]
    WorkspaceRead,
    /// Read and write the selected workspace through the `/workspace` preopen.
    #[serde(rename = "workspace.write")]
    WorkspaceWrite,
    /// Open host TCP/UDP sockets and perform name lookup.
    #[serde(rename = "network")]
    Network,
    /// Run as a contained native process with the user's complete host authority.
    #[serde(rename = "host.full")]
    HostFull,
}

impl PluginCapability {
    /// Stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceRead => "workspace.read",
            Self::WorkspaceWrite => "workspace.write",
            Self::Network => "network",
            Self::HostFull => "host.full",
        }
    }
}

/// One executable tool exported by the package runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginToolDefinition {
    /// Provider-facing description.
    pub description: String,
    /// JSON Schema before Zuno injects cross-cutting tool properties.
    #[serde(default = "default_parameters_schema")]
    pub parameters: Value,
    /// Observable side-effect classification. Defaults to side-effecting.
    #[serde(default)]
    pub effect: PluginToolEffect,
    /// Replay classification. Defaults to never replay.
    #[serde(default)]
    pub replay: PluginToolReplay,
    /// Concurrency classification. Defaults to exclusive.
    #[serde(default)]
    pub concurrency: PluginToolConcurrency,
    /// Stable client presentation intent.
    #[serde(default)]
    pub ui_intent: PluginToolUiIntent,
}

impl PluginToolDefinition {
    fn validate(&self, package: &str, tool: &str) -> Result<(), ManifestError> {
        if self.description.trim().is_empty() {
            return Err(ManifestError::EmptyToolDescription {
                package: package.to_owned(),
                tool: tool.to_owned(),
            });
        }
        if !self.parameters.is_object() {
            return Err(ManifestError::InvalidToolSchema {
                package: package.to_owned(),
                tool: tool.to_owned(),
            });
        }
        if matches!(
            self.effect,
            PluginToolEffect::UserMediated | PluginToolEffect::Delegating
        ) {
            return Err(ManifestError::UnsupportedToolPolicy {
                package: package.to_owned(),
                tool: tool.to_owned(),
                message: "runtime plugins cannot bypass native HITL or delegate hidden child calls"
                    .to_owned(),
            });
        }
        if self.replay == PluginToolReplay::Safe && self.effect != PluginToolEffect::ReadOnly {
            return Err(ManifestError::UnsupportedToolPolicy {
                package: package.to_owned(),
                tool: tool.to_owned(),
                message: "`replay: safe` requires `effect: readOnly`".to_owned(),
            });
        }
        if self.concurrency != PluginToolConcurrency::Exclusive {
            return Err(ManifestError::UnsupportedToolPolicy {
                package: package.to_owned(),
                tool: tool.to_owned(),
                message:
                    "plugin protocol v1 serializes one runtime instance; concurrency must be `exclusive`"
                        .to_owned(),
            });
        }
        Ok(())
    }
}

fn default_parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

/// Plugin tool effect declaration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PluginToolEffect {
    ReadOnly,
    UserMediated,
    Delegating,
    #[default]
    SideEffecting,
}

/// Plugin tool replay declaration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PluginToolReplay {
    #[default]
    Never,
    Safe,
}

/// Plugin tool concurrency declaration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PluginToolConcurrency {
    #[default]
    Exclusive,
    ParallelSafe,
    IsolatedBackground,
}

/// Plugin tool client presentation declaration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PluginToolUiIntent {
    #[default]
    Generic,
    Subagent,
}

/// One slash-command workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinition {
    /// What the workflow does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Prompt template. Existing `$1`/`$ARGUMENTS` command expansion applies.
    pub prompt: String,
}

/// One model-loadable skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillDefinition {
    /// Lowercase hyphen-separated skill name.
    pub name: String,
    /// Trigger and purpose advertised in the available-skills catalog.
    pub description: String,
    /// Full Markdown instruction body returned by the `skill` tool.
    pub content: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error(
        "extension package `{package}` declares API version `{found}`; this build requires `{API_VERSION}`"
    )]
    ApiVersion { package: String, found: String },
    #[error("extension package id `{0}` is invalid")]
    InvalidPackageId(String),
    #[error("extension package `{0}` has an empty description")]
    EmptyDescription(String),
    #[error("extension package `{0}` contributes no agents, workflows, skills, tools, or runtime")]
    EmptyPackage(String),
    #[error("extension package `{0}` contributes executable tools without a runtime")]
    ToolsWithoutRuntime(String),
    #[error("extension package `{0}` declares an executable runtime without any tools")]
    RuntimeWithoutTools(String),
    #[error("extension {kind} name `{name}` is invalid")]
    InvalidContributionName { kind: &'static str, name: String },
    #[error(
        "extension package `{package}` agent `{agent}` cannot rename itself; the agents map key is its identity"
    )]
    AgentRename { package: String, agent: String },
    #[error(
        "extension package `{package}` agent `{agent}` is disabled and therefore contributes no agent"
    )]
    DisabledAgent { package: String, agent: String },
    #[error("extension package `{package}` workflow `{workflow}` has an empty prompt")]
    EmptyWorkflow { package: String, workflow: String },
    #[error("extension skill name `{0}` must be lowercase hyphen-separated")]
    InvalidSkillName(String),
    #[error("extension skill `{skill}` has an empty {field}")]
    EmptySkillField { skill: String, field: &'static str },
    #[error("extension package `{package}` has an empty runtime field `{field}`")]
    EmptyRuntimeField {
        package: String,
        field: &'static str,
    },
    #[error("extension package `{package}` runtime field `{field}` has unsupported budget {value}")]
    InvalidRuntimeBudget {
        package: String,
        field: &'static str,
        value: u64,
    },
    #[error("extension package `{package}` has invalid runtime capabilities: {message}")]
    InvalidRuntimeCapability { package: String, message: String },
    #[error("extension package `{package}` WASI artifact `{artifact}` must stay below its package")]
    InvalidArtifactPath { package: String, artifact: String },
    #[error("extension package `{package}` environment name `{name}` is invalid")]
    InvalidEnvironmentName { package: String, name: String },
    #[error("extension package `{package}` repeats environment name `{name}`")]
    DuplicateEnvironmentName { package: String, name: String },
    #[error("extension package `{package}` tool `{tool}` has an empty description")]
    EmptyToolDescription { package: String, tool: String },
    #[error("extension package `{package}` tool `{tool}` parameters must be a JSON Schema object")]
    InvalidToolSchema { package: String, tool: String },
    #[error("extension package `{package}` tool `{tool}` has unsupported policy: {message}")]
    UnsupportedToolPolicy {
        package: String,
        tool: String,
        message: String,
    },
}

fn validate_package_id(id: &str) -> Result<(), ManifestError> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(ManifestError::InvalidPackageId(id.to_owned()))
    }
}

fn validate_contribution_name(kind: &'static str, name: &str) -> Result<(), ManifestError> {
    let valid = !name.is_empty()
        && name.len() <= 96
        && !name.starts_with('/')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:/".contains(character));
    if valid {
        Ok(())
    } else {
        Err(ManifestError::InvalidContributionName {
            kind,
            name: name.to_owned(),
        })
    }
}

fn validate_skill_name(name: &str) -> Result<(), ManifestError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(ManifestError::InvalidSkillName(name.to_owned()))
    }
}

fn validate_relative_artifact(package: &str, artifact: &str) -> Result<(), ManifestError> {
    let path = std::path::Path::new(artifact);
    let valid = !artifact.trim().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
    if valid {
        Ok(())
    } else {
        Err(ManifestError::InvalidArtifactPath {
            package: package.to_owned(),
            artifact: artifact.to_owned(),
        })
    }
}

fn validate_unique_capabilities(
    package: &str,
    capabilities: &[PluginCapability],
) -> Result<(), ManifestError> {
    let mut seen = std::collections::BTreeSet::new();
    for capability in capabilities {
        if !seen.insert(*capability) {
            return Err(ManifestError::InvalidRuntimeCapability {
                package: package.to_owned(),
                message: format!("capability `{}` is repeated", capability.as_str()),
            });
        }
    }
    Ok(())
}

fn validate_environment_names(package: &str, environment: &[String]) -> Result<(), ManifestError> {
    let mut seen = std::collections::BTreeSet::new();
    for name in environment {
        let valid = !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && name
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_');
        if !valid {
            return Err(ManifestError::InvalidEnvironmentName {
                package: package.to_owned(),
                name: name.clone(),
            });
        }
        if !seen.insert(name) {
            return Err(ManifestError::DuplicateEnvironmentName {
                package: package.to_owned(),
                name: name.clone(),
            });
        }
    }
    Ok(())
}
