//! Zuno's native configuration vocabulary.
//!
//! # Unknown keys
//!
//! * **Top level — rejected.** [`Config`] carries `deny_unknown_fields`, and
//!   [`parse::from_json_value`] reports each offending key with its own
//!   [`zuno_error::ConfigIssue`].
//! * **Nested — ignored unless their type says otherwise.** Plain `serde` structs
//!   drop unknown nested fields.
//! * **Provider-extensible records — captured.** Agent definitions
//!   ([`agent::AgentConfig`]), provider options ([`provider::ProviderOptions`]),
//!   model variants ([`provider::ModelVariant`]), and permission objects
//!   ([`permission::PermissionObject`]) keep their extra keys. For agents the
//!   captured keys are additionally swept into `options`, which is how
//!   `reasoningEffort` and `thinking` reach the provider.

pub mod agent;
pub mod formatter;
pub mod lsp;
pub mod mcp;
pub mod ordered;
pub mod parse;
pub mod permission;
pub mod product_agent;
pub mod provider;
pub mod reference;
pub mod workflow;

#[cfg(test)]
mod tests;

use crate::schema::agent::AgentConfig;
use crate::schema::formatter::FormatterConfig;
use crate::schema::lsp::LspConfig;
use crate::schema::mcp::McpServerConfig;
use crate::schema::ordered::OrderedMap;
use crate::schema::permission::{PermissionConfig, PermissionMode};
use crate::schema::product_agent::ProductAgentConfig;
use crate::schema::provider::ProviderConfig;
use crate::schema::reference::ReferenceEntry;
use crate::schema::workflow::AgentWorkflowConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

/// A free-form JSON object.
pub type JsonMap = serde_json::Map<String, serde_json::Value>;

/// Every key [`Config`] accepts, in declaration order.
///
pub const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "$schema",
    "shell",
    "logLevel",
    "server",
    "command",
    "skills",
    "references",
    "watcher",
    "snapshot",
    "share",
    "autoupdate",
    "disabled_providers",
    "enabled_providers",
    "model",
    "small_model",
    "default_agent",
    "subagent_depth",
    "username",
    "agents",
    "workflows",
    "provider",
    "productAgent",
    "mcp",
    "formatter",
    "lsp",
    "instructions",
    "permission",
    "tools",
    "attachment",
    "enterprise",
    "web_search",
    "goal",
    "tool_output",
    "compaction",
    "concurrency",
    "memory",
    "experimental",
];

/// One parsed Zuno config layer, not a merged result.
///
/// Every field is optional because every field is optional in the oracle, and
/// because merging layers depends on being able to tell "absent" from "set to the
/// default".
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// JSON schema reference for editor validation.
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Default shell for the terminal and the bash tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Log level.
    #[serde(rename = "logLevel", skip_serializing_if = "Option::is_none")]
    pub log_level: Option<LogLevel>,
    /// Server configuration for `zuno serve` and the web command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerConfig>,
    /// Custom commands, keyed by command name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<OrderedMap<CommandConfig>>,
    /// Skill discovery and model-visible catalog settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<SkillsConfig>,
    /// Named git or local directory references.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<OrderedMap<ReferenceEntry>>,
    /// File-watcher configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher: Option<WatcherConfig>,
    /// Record filesystem snapshots so edits can be undone. Defaults to true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<bool>,
    /// Session sharing behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<ShareMode>,
    /// Update behaviour: `true`, `false`, or `"notify"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoupdate: Option<Autoupdate>,
    /// Providers to drop even when their credentials are present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_providers: Option<Vec<String>>,
    /// When set, the only providers to enable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_providers: Option<Vec<String>>,
    /// Default model, in `provider/model` form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model for cheap side tasks such as title generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
    /// Agent to use when none is named. Must be a primary agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    /// Maximum subagent nesting depth. Defaults to 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_depth: Option<u32>,
    /// Name to show for the user instead of the system username.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Agent definitions and overrides, keyed by agent name.
    ///
    /// The oracle names `plan`, `build`, `general`, `explore`, `title`, `summary`,
    /// and `compaction` explicitly and then adds `Record(String, AgentConfig)`
    /// (`config/config.ts:96-110`), so every value has the same type and the named
    /// keys carry no extra meaning at parse time.
    #[serde(rename = "agents", skip_serializing_if = "Option::is_none")]
    pub agent: Option<OrderedMap<AgentConfig>>,
    /// Named, immutable multi-agent workflow templates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflows: Option<OrderedMap<AgentWorkflowConfig>>,
    /// Custom providers and model overrides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<OrderedMap<ProviderConfig>>,
    /// Native Codex and Claude Code subagent instances.
    #[serde(rename = "productAgent", skip_serializing_if = "Option::is_none")]
    pub product_agent: Option<OrderedMap<ProductAgentConfig>>,
    /// MCP servers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<OrderedMap<McpServerConfig>>,
    /// Formatters: a switch, or per-formatter overrides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatter: Option<FormatterConfig>,
    /// LSP servers: a switch, or per-server overrides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp: Option<LspConfig>,
    /// Additional instruction files or glob patterns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Vec<String>>,
    /// Global permissions and cross-cutting HITL mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionConfig>,
    /// Tool switches, keyed by tool name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<OrderedMap<bool>>,
    /// Attachment processing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentConfig>,
    /// Enterprise deployment settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enterprise: Option<EnterpriseConfig>,
    /// Web-search provider and runtime-owned batch limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search: Option<WebSearchConfig>,
    /// Persistent goal continuation and retry settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalConfig>,
    /// Thresholds for truncating tool output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<ToolOutputConfig>,
    /// Context-compaction behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionConfig>,
    /// Bounded concurrency for independent runtime capabilities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<ConcurrencyConfig>,
    /// Persistent resident-memory configuration. Absent defaults to enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryConfig>,
    /// Options under active development.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<ExperimentalConfig>,
}

impl Config {
    /// Resolve the memory master switch and all component defaults.
    #[must_use]
    pub fn resolved_memory(&self) -> ResolvedMemoryConfig {
        self.memory
            .as_ref()
            .map_or_else(ResolvedMemoryConfig::default, MemoryConfig::resolved)
    }

    /// Resolve bounded runtime concurrency with native defaults.
    #[must_use]
    pub fn resolved_concurrency(&self) -> ResolvedConcurrencyConfig {
        self.concurrency.as_ref().map_or_else(
            ResolvedConcurrencyConfig::default,
            ConcurrencyConfig::resolved,
        )
    }

    /// Resolve the canonical permission mode.
    #[must_use]
    pub fn permission_mode(&self) -> PermissionMode {
        self.permission
            .as_ref()
            .map_or(PermissionMode::Standard, |permission| permission.mode)
    }

    /// Whether side-effecting tool calls require a fresh attached-user decision.
    #[must_use]
    pub fn strict_authorization(&self) -> bool {
        self.permission_mode() == PermissionMode::Strict
    }
}

/// Log level (`config/config.ts:27-30`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    /// Debug.
    Debug,
    /// Info.
    Info,
    /// Warn.
    Warn,
    /// Error.
    Error,
}

/// Session sharing behaviour (`config/config.ts:59-62`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareMode {
    /// Share only when asked.
    Manual,
    /// Share every new session.
    Auto,
    /// Never share.
    Disabled,
}

/// The `"notify"` arm of [`Autoupdate`].
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoupdateMode {
    /// Announce updates without installing them.
    Notify,
}

/// Update behaviour (`config/config.ts:67-71`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Autoupdate {
    /// Install updates automatically, or never.
    Enabled(bool),
    /// Announce updates only.
    Mode(AutoupdateMode),
}

/// Server configuration (`config/server.ts:6-18`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Port to listen on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<NonZeroU32>,
    /// Hostname to listen on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Advertise the server over mDNS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdns: Option<bool>,
    /// mDNS domain; the runtime defaults to `zuno.local`.
    #[serde(rename = "mdnsDomain", skip_serializing_if = "Option::is_none")]
    pub mdns_domain: Option<String>,
    /// Additional origins to allow for CORS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<Vec<String>>,
}

/// One custom command (`config/command.ts:5-12`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandConfig {
    /// The prompt template. The only required field in the whole schema's leaves.
    pub template: String,
    /// What the command does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Agent to run the command as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Model to run the command with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model variant to run the command with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Run the command in a subtask instead of the current session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtask: Option<bool>,
}

/// Skill discovery and model-visible catalog settings.
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// Additional paths to skill folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// URLs to fetch skills from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
    /// Whether turns receive the skill trigger policy and metadata catalog.
    #[serde(
        rename = "includeInstructions",
        skip_serializing_if = "Option::is_none"
    )]
    pub include_instructions: Option<bool>,
    /// Maximum approximate tokens used by the catalog. Values above 10,000 are
    /// clamped by the runtime.
    #[serde(rename = "maxContextTokens", skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<NonZeroU32>,
}

/// File-watcher configuration (`config/config.ts:49`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WatcherConfig {
    /// Glob patterns the watcher ignores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
}

/// Attachment processing (`config/attachment.ts:22-24`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AttachmentConfig {
    /// Image attachment limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageAttachmentConfig>,
}

/// Image attachment limits (`config/attachment.ts:6-19`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ImageAttachmentConfig {
    /// Resize oversized images instead of rejecting them. Defaults to true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_resize: Option<bool>,
    /// Maximum width before resizing or rejecting. Defaults to 2000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<NonZeroU32>,
    /// Maximum height before resizing or rejecting. Defaults to 2000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<NonZeroU32>,
    /// Maximum base64 payload in bytes. Defaults to 5242880.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_base64_bytes: Option<NonZeroU32>,
}

/// Enterprise deployment settings (`config/config.ts:134-136`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EnterpriseConfig {
    /// Enterprise URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Web-search settings owned by the active profile.
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WebSearchConfig {
    /// Hosted provider selected for this profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<WebSearchBackend>,
    /// Maximum queries accepted in one model call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_queries: Option<NonZeroU32>,
    /// Maximum sources returned after combining all queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_results: Option<NonZeroU32>,
    /// Per-query provider time budget in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<NonZeroU64>,
}

/// Hosted web-search providers supported by the built-in adapter.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchBackend {
    /// Exa MCP search.
    Exa,
    /// Parallel MCP search.
    Parallel,
}

/// Persistent goal runtime settings.
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GoalConfig {
    /// Automatic recovery after a retryable terminal turn failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<GoalRetryConfig>,
}

/// Exponential backoff settings for automatic goal recovery.
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GoalRetryConfig {
    /// Delay before the first retry, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_delay_ms: Option<NonZeroU64>,
    /// Maximum delay between retries, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_delay_ms: Option<NonZeroU64>,
    /// Symmetric jitter percentage for locally selected delays, in the inclusive range 0..=100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jitter_percent: Option<u8>,
    /// Maximum wait before checking for queued human input, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<NonZeroU64>,
}

/// Tool-output truncation thresholds (`config/config.ts:137-150`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolOutputConfig {
    /// Lines of output before truncation. Defaults to 2000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<NonZeroU32>,
    /// Bytes of output before truncation. Defaults to 51200.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<NonZeroU32>,
}

/// Context-compaction behaviour (`config/config.ts:151-172`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Compact automatically when the context fills. Defaults to true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    /// Percentage of the usable context window that triggers automatic compaction.
    /// Defaults to 80.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_percent: Option<CompactionThresholdPercent>,
    /// Prune old tool outputs. Defaults to false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prune: Option<bool>,
    /// Recent user turns to keep verbatim. Defaults to 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail_turns: Option<u32>,
    /// Token ceiling for the verbatim tail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserve_recent_tokens: Option<u32>,
    /// Token buffer left free so compaction itself cannot overflow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserved: Option<u32>,
}

/// Default percentage of the usable context window consumed before auto compaction.
pub const DEFAULT_COMPACTION_THRESHOLD_PERCENT: u8 = 80;

/// A validated automatic-compaction percentage.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CompactionThresholdPercent(#[schemars(range(min = 1, max = 100))] u8);

impl CompactionThresholdPercent {
    /// Construct a percentage accepted by the configuration boundary.
    pub const fn new(value: u8) -> Result<Self, &'static str> {
        if value >= 1 && value <= 100 {
            Ok(Self(value))
        } else {
            Err("compaction threshold percent must be between 1 and 100")
        }
    }

    /// The validated percentage.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl<'de> Deserialize<'de> for CompactionThresholdPercent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Small positive concurrency bound accepted at every runtime boundary.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ConcurrencyLimit(#[schemars(range(min = 1, max = 64))] u8);

impl ConcurrencyLimit {
    /// The validated integer value.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ConcurrencyLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        if (1..=64).contains(&value) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "concurrency limit must be between 1 and 64",
            ))
        }
    }
}

impl fmt::Display for ConcurrencyLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Optional per-layer concurrency overrides.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcurrencyConfig {
    /// Maximum independent model-issued tool calls executing at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<ConcurrencyLimit>,
    /// Maximum MCP servers connecting or disconnecting at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_connections: Option<ConcurrencyLimit>,
    /// Maximum independent LSP servers or file requests executing at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp_requests: Option<ConcurrencyLimit>,
}

impl ConcurrencyConfig {
    /// Fill absent values with the native runtime defaults.
    #[must_use]
    pub fn resolved(&self) -> ResolvedConcurrencyConfig {
        let defaults = ResolvedConcurrencyConfig::default();
        ResolvedConcurrencyConfig {
            tool_calls: self
                .tool_calls
                .map_or(defaults.tool_calls, ConcurrencyLimit::get),
            mcp_connections: self
                .mcp_connections
                .map_or(defaults.mcp_connections, ConcurrencyLimit::get),
            lsp_requests: self
                .lsp_requests
                .map_or(defaults.lsp_requests, ConcurrencyLimit::get),
        }
    }
}

/// Fully defaulted concurrency settings consumed by composition roots.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedConcurrencyConfig {
    pub tool_calls: u8,
    pub mcp_connections: u8,
    pub lsp_requests: u8,
}

impl Default for ResolvedConcurrencyConfig {
    fn default() -> Self {
        Self {
            tool_calls: 8,
            mcp_connections: 8,
            lsp_requests: 4,
        }
    }
}

/// Default characters available to cross-project agent notes.
pub const DEFAULT_GLOBAL_MEMORY_CHAR_LIMIT: u32 = 2_200;

/// Default characters available to repository-local rules.
pub const DEFAULT_PROJECT_MEMORY_CHAR_LIMIT: u32 = 3_000;

/// Default delivered-turn interval for background reflection.
pub const DEFAULT_MEMORY_NUDGE_INTERVAL: u32 = 10;

/// Confidence threshold used by high-confidence promotion.
pub const DEFAULT_MEMORY_AUTO_CONFIDENCE: f64 = 0.9;

/// How durable memory candidates become resident entries.
#[derive(Default, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPromotion {
    /// Keep every candidate pending until a user approves it.
    #[default]
    Review,
    /// Apply only candidates at or above `auto_confidence`.
    HighConfidence,
    /// Apply every validated candidate immediately.
    Automatic,
}

/// A validated probability used by memory promotion.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MemoryConfidence(#[schemars(range(min = 0.0, max = 1.0))] f64);

impl MemoryConfidence {
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for MemoryConfidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "memory confidence must be between 0 and 1",
            ))
        }
    }
}

/// Persistent memory: a master boolean, or component settings.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MemoryConfig {
    /// `false` disables the subsystem; `true` selects all defaults.
    Enabled(bool),
    /// Fine-grained settings for an enabled subsystem.
    Options(MemoryOptions),
}

impl MemoryConfig {
    /// Resolve absent fields and make a false master switch dominate every flag.
    #[must_use]
    pub fn resolved(&self) -> ResolvedMemoryConfig {
        match self {
            Self::Enabled(false) => ResolvedMemoryConfig {
                enabled: false,
                resident: false,
                tool: false,
                reflection: false,
                ..ResolvedMemoryConfig::default()
            },
            Self::Enabled(true) => ResolvedMemoryConfig::default(),
            Self::Options(options) => ResolvedMemoryConfig {
                enabled: true,
                resident: options.resident.unwrap_or(true),
                tool: options.tool.unwrap_or(true),
                reflection: options.reflection.unwrap_or(true),
                global_char_limit: options
                    .global_char_limit
                    .map_or(DEFAULT_GLOBAL_MEMORY_CHAR_LIMIT as usize, |limit| {
                        limit.get() as usize
                    }),
                project_char_limit: options
                    .project_char_limit
                    .map_or(DEFAULT_PROJECT_MEMORY_CHAR_LIMIT as usize, |limit| {
                        limit.get() as usize
                    }),
                nudge_interval: options
                    .nudge_interval
                    .unwrap_or(DEFAULT_MEMORY_NUDGE_INTERVAL)
                    as u64,
                promotion: options.promotion.unwrap_or_default(),
                auto_confidence: options
                    .auto_confidence
                    .map_or(DEFAULT_MEMORY_AUTO_CONFIDENCE, MemoryConfidence::get),
            },
        }
    }
}

/// Fine-grained settings under the `memory` top-level key.
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MemoryOptions {
    /// Inject frozen resident blocks into each session's system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resident: Option<bool>,
    /// Expose the model-facing `memory` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<bool>,
    /// Run post-response background reflection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reflection: Option<bool>,
    /// Character cap for global agent notes. Defaults to 2200.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_char_limit: Option<NonZeroU32>,
    /// Character cap for project rules. Defaults to 3000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_char_limit: Option<NonZeroU32>,
    /// Reflect every N delivered turns; zero disables only the periodic trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nudge_interval: Option<u32>,
    /// Candidate promotion policy. Defaults to `review`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotion: Option<MemoryPromotion>,
    /// Threshold used by `high_confidence`. Defaults to 0.9.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_confidence: Option<MemoryConfidence>,
}

/// Fully defaulted memory settings consumed by runtime composition roots.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq)]
pub struct ResolvedMemoryConfig {
    /// Whether the master switch is on.
    pub enabled: bool,
    /// Whether resident prompt injection is on.
    pub resident: bool,
    /// Whether the model-facing tool is on.
    pub tool: bool,
    /// Whether background reflection is on.
    pub reflection: bool,
    /// Global store cap in Unicode scalar values.
    pub global_char_limit: usize,
    /// Project store cap in Unicode scalar values.
    pub project_char_limit: usize,
    /// Delivered-turn reflection cadence; zero disables the periodic trigger.
    pub nudge_interval: u64,
    /// How candidates become resident entries.
    pub promotion: MemoryPromotion,
    /// High-confidence promotion threshold.
    pub auto_confidence: f64,
}

impl Default for ResolvedMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            resident: true,
            tool: true,
            reflection: true,
            global_char_limit: DEFAULT_GLOBAL_MEMORY_CHAR_LIMIT as usize,
            project_char_limit: DEFAULT_PROJECT_MEMORY_CHAR_LIMIT as usize,
            nudge_interval: u64::from(DEFAULT_MEMORY_NUDGE_INTERVAL),
            promotion: MemoryPromotion::Review,
            auto_confidence: DEFAULT_MEMORY_AUTO_CONFIDENCE,
        }
    }
}

/// Options under active development (`config/config.ts:173-188`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExperimentalConfig {
    /// Stop summarizing pasted text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_paste_summary: Option<bool>,
    /// Enable the batch tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_tool: Option<bool>,
    /// Emit OpenTelemetry spans for model calls.
    #[serde(rename = "openTelemetry", skip_serializing_if = "Option::is_none")]
    pub open_telemetry: Option<bool>,
    /// Tools only primary agents may use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_tools: Option<Vec<String>>,
    /// Keep the agent loop running after a denied tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continue_loop_on_deny: Option<bool>,
    /// Timeout in milliseconds for MCP requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_timeout: Option<NonZeroU32>,
    /// Policy statements applied to supported resources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policies: Option<Vec<PolicyStatement>>,
}

/// An action a policy statement can govern.
///
/// `packages/core/src/config/experimental.ts:9` builds this from
/// `Catalog.PolicyActions`, which is `Literals(["provider.use"])` — one action,
/// and anything else is rejected.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyAction {
    /// Use of a provider.
    #[serde(rename = "provider.use")]
    ProviderUse,
}

/// Whether a policy statement permits or refuses (`packages/core/src/policy.ts:8`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyEffect {
    /// Permit.
    Allow,
    /// Refuse.
    Deny,
}

/// One policy statement (`packages/core/src/policy.ts:11-15` plus the narrowed
/// `action` from `config/experimental.ts:11-14`). Every field is required.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyStatement {
    /// The action governed.
    pub action: PolicyAction,
    /// Permit or refuse.
    pub effect: PolicyEffect,
    /// The resource pattern the statement matches.
    pub resource: String,
}
