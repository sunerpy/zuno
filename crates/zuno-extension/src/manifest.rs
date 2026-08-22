use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};
use zuno_config::schema::agent::AgentConfig;
use zuno_config::schema::ordered::OrderedMap;

/// The only extension manifest version this build accepts.
pub const API_VERSION: &str = "zuno.extension/v1";

/// One declarative Zuno extension package.
///
/// The package intentionally contains no JavaScript or foreign plugin ABI.
/// Agents, workflows, and skills are native catalog inputs and therefore pass
/// through the same validation and consumers as their file/config equivalents.
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
        };
        package.validate().map_err(de::Error::custom)?;
        Ok(package)
    }
}

impl Package {
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
        if self.agents.is_empty() && self.workflows.is_empty() && self.skills.is_empty() {
            return Err(ManifestError::EmptyPackage(self.id.clone()));
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
        Ok(())
    }
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
    #[error("extension package `{0}` contributes no agents, workflows, or skills")]
    EmptyPackage(String),
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
