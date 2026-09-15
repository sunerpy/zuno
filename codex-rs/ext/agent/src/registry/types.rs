use crate::OneShotAgentBackendKind;
use crate::OneShotAgentError;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as SerdeError;
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

const MAX_BACKEND_ID_CHARS: usize = 128;

/// Stable identity for one configured Agent backend instance.
///
/// The instance is configuration-owned: multiple IDs may point at the same
/// product backend with different model, reasoning, permission, or provider
/// profiles. IDs accept plugin namespaces such as `team/claude-review` while
/// rejecting ambiguous surrounding whitespace and control characters.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AgentBackendId(String);

impl AgentBackendId {
    pub fn new(value: impl Into<String>) -> Result<Self, AgentBackendRegistryError> {
        let value = value.into();
        let invalid = value.is_empty()
            || value.trim() != value
            || value.chars().count() > MAX_BACKEND_ID_CHARS
            || value.chars().any(char::is_control);
        if invalid {
            return Err(AgentBackendRegistryError::InvalidId { value });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentBackendId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for AgentBackendId {
    type Err = AgentBackendRegistryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for AgentBackendId {
    type Error = AgentBackendRegistryError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for AgentBackendId {
    type Error = AgentBackendRegistryError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for AgentBackendId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

/// Work lifecycle an Agent backend can own.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentBackendOperation {
    OneShot,
    Resume,
    Steer,
}

impl fmt::Display for AgentBackendOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::OneShot => "one-shot",
            Self::Resume => "resume",
            Self::Steer => "steer",
        })
    }
}

/// Where an Agent backend accepts a configurable option.
///
/// `Profile` means the value is fixed when the configured backend instance is
/// mounted. `Invocation` additionally permits a validated per-run override.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentBackendOptionScope {
    #[default]
    Unsupported,
    Profile,
    Invocation,
}

impl fmt::Display for AgentBackendOptionScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "unsupported",
            Self::Profile => "profile",
            Self::Invocation => "invocation",
        })
    }
}

/// Capabilities published by a configured Agent backend before any work starts.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBackendCapabilities {
    pub operations: BTreeSet<AgentBackendOperation>,
    pub model: AgentBackendOptionScope,
    pub reasoning_effort: AgentBackendOptionScope,
    pub permission_mode: AgentBackendOptionScope,
    pub service_tier: AgentBackendOptionScope,
}

impl AgentBackendCapabilities {
    /// Minimal capabilities for an implementation of [`OneShotAgentBackend`].
    pub fn one_shot_only() -> Self {
        Self {
            operations: BTreeSet::from([AgentBackendOperation::OneShot]),
            ..Self::default()
        }
    }

    /// Current in-process Codex child-thread capabilities.
    pub fn native_codex() -> Self {
        Self {
            operations: BTreeSet::from([AgentBackendOperation::OneShot]),
            model: AgentBackendOptionScope::Profile,
            reasoning_effort: AgentBackendOptionScope::Profile,
            permission_mode: AgentBackendOptionScope::Profile,
            service_tier: AgentBackendOptionScope::Profile,
        }
    }

    /// Current bounded Claude Code process capabilities.
    pub fn claude_code() -> Self {
        Self {
            operations: BTreeSet::from([AgentBackendOperation::OneShot]),
            model: AgentBackendOptionScope::Profile,
            reasoning_effort: AgentBackendOptionScope::Profile,
            permission_mode: AgentBackendOptionScope::Profile,
            service_tier: AgentBackendOptionScope::Unsupported,
        }
    }

    /// Current supervised external ACP stdio capabilities.
    pub fn acp() -> Self {
        Self {
            operations: BTreeSet::from([AgentBackendOperation::OneShot]),
            model: AgentBackendOptionScope::Profile,
            reasoning_effort: AgentBackendOptionScope::Profile,
            permission_mode: AgentBackendOptionScope::Profile,
            service_tier: AgentBackendOptionScope::Unsupported,
        }
    }

    pub fn supports(&self, requirements: &AgentBackendRequirements) -> bool {
        self.missing(requirements).is_empty()
    }

    pub fn missing(&self, requirements: &AgentBackendRequirements) -> Vec<AgentBackendRequirement> {
        let mut missing = Vec::new();
        if !self.operations.contains(&requirements.operation) {
            missing.push(AgentBackendRequirement::Operation(requirements.operation));
        }
        push_missing_option(
            &mut missing,
            AgentBackendRequirement::Model(requirements.model),
            self.model,
            requirements.model,
        );
        push_missing_option(
            &mut missing,
            AgentBackendRequirement::ReasoningEffort(requirements.reasoning_effort),
            self.reasoning_effort,
            requirements.reasoning_effort,
        );
        push_missing_option(
            &mut missing,
            AgentBackendRequirement::PermissionMode(requirements.permission_mode),
            self.permission_mode,
            requirements.permission_mode,
        );
        push_missing_option(
            &mut missing,
            AgentBackendRequirement::ServiceTier(requirements.service_tier),
            self.service_tier,
            requirements.service_tier,
        );
        missing
    }
}

fn push_missing_option(
    missing: &mut Vec<AgentBackendRequirement>,
    requirement: AgentBackendRequirement,
    available: AgentBackendOptionScope,
    required: AgentBackendOptionScope,
) {
    if available < required {
        missing.push(requirement);
    }
}

/// Capabilities a caller needs for one prospective dispatch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBackendRequirements {
    pub operation: AgentBackendOperation,
    #[serde(default)]
    pub model: AgentBackendOptionScope,
    #[serde(default)]
    pub reasoning_effort: AgentBackendOptionScope,
    #[serde(default)]
    pub permission_mode: AgentBackendOptionScope,
    #[serde(default)]
    pub service_tier: AgentBackendOptionScope,
}

impl AgentBackendRequirements {
    pub fn one_shot() -> Self {
        Self {
            operation: AgentBackendOperation::OneShot,
            model: AgentBackendOptionScope::Unsupported,
            reasoning_effort: AgentBackendOptionScope::Unsupported,
            permission_mode: AgentBackendOptionScope::Unsupported,
            service_tier: AgentBackendOptionScope::Unsupported,
        }
    }
}

/// One exact unsupported requirement, retained as typed diagnostic data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentBackendRequirement {
    Operation(AgentBackendOperation),
    Model(AgentBackendOptionScope),
    ReasoningEffort(AgentBackendOptionScope),
    PermissionMode(AgentBackendOptionScope),
    ServiceTier(AgentBackendOptionScope),
}

impl fmt::Display for AgentBackendRequirement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operation(operation) => write!(formatter, "operation:{operation}"),
            Self::Model(scope) => write!(formatter, "model:{scope}"),
            Self::ReasoningEffort(scope) => write!(formatter, "reasoning-effort:{scope}"),
            Self::PermissionMode(scope) => write!(formatter, "permission-mode:{scope}"),
            Self::ServiceTier(scope) => write!(formatter, "service-tier:{scope}"),
        }
    }
}

/// Public inventory record for one mounted backend instance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBackendDescriptor {
    pub id: AgentBackendId,
    pub kind: OneShotAgentBackendKind,
    pub capabilities: AgentBackendCapabilities,
    /// Stable implementation/configuration revision for a factory generation.
    ///
    /// Directly mounted runtime backends may omit this. Durable workflow
    /// admission requires a factory descriptor with a non-empty revision so a
    /// later plugin replacement cannot silently reuse the same backend id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// Failure before a backend invocation is admitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentBackendRegistryError {
    InvalidId {
        value: String,
    },
    Duplicate {
        id: AgentBackendId,
    },
    NotFound {
        id: AgentBackendId,
    },
    InvalidCapabilities {
        id: AgentBackendId,
    },
    InvalidRevision {
        id: AgentBackendId,
    },
    Unsupported {
        id: AgentBackendId,
        missing: Vec<AgentBackendRequirement>,
    },
}

impl fmt::Display for AgentBackendRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId { value } => write!(formatter, "invalid Agent backend id {value:?}"),
            Self::Duplicate { id } => write!(formatter, "Agent backend `{id}` is already mounted"),
            Self::NotFound { id } => write!(formatter, "Agent backend `{id}` is not mounted"),
            Self::InvalidCapabilities { id } => write!(
                formatter,
                "Agent backend `{id}` does not advertise one-shot execution"
            ),
            Self::InvalidRevision { id } => write!(
                formatter,
                "Agent backend factory `{id}` does not advertise a valid stable revision"
            ),
            Self::Unsupported { id, missing } => {
                write!(
                    formatter,
                    "Agent backend `{id}` lacks required capabilities: "
                )?;
                for (index, requirement) in missing.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    requirement.fmt(formatter)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for AgentBackendRegistryError {}

/// Dispatch failure retaining the distinction between admission and execution.
#[derive(Debug)]
pub enum AgentBackendDispatchError {
    Registry(AgentBackendRegistryError),
    Backend(OneShotAgentError),
}

impl fmt::Display for AgentBackendDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => error.fmt(formatter),
            Self::Backend(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AgentBackendDispatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(error) => Some(error),
            Self::Backend(error) => Some(error),
        }
    }
}
