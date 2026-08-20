//! The opencode configuration vocabulary, translated from the TypeScript oracle.
//!
//! Oracle: `packages/core/src/v1/config/config.ts:32-190` for the top level, and
//! the files under `packages/core/src/v1/config/` for each nested type. Every
//! module here names its own oracle lines.
//!
//! # Unknown keys
//!
//! The oracle's policy is not uniform, and neither is this module's:
//!
//! * **Top level — rejected.** `packages/opencode/src/config/parse.ts:40-53`
//!   collects the keys the top-level struct does not name and throws
//!   `unrecognized_keys` *before* validation runs. [`Config`] therefore carries
//!   `deny_unknown_fields`, and [`parse::from_json_value`] reports each offending
//!   key with its own [`zuno_error::ConfigIssue`].
//! * **Nested — ignored.** Effect Schema's default `onExcessProperty` is
//!   `"ignore"`, so an unknown key inside `server`, `command`, and friends is
//!   dropped rather than rejected. These structs are plain `serde` structs, which
//!   behave the same way.
//! * **Where the oracle writes `StructWithRest` — captured.** Agent definitions
//!   ([`agent::AgentConfig`]), provider options ([`provider::ProviderOptions`]),
//!   model variants ([`provider::ModelVariant`]), and permission objects
//!   ([`permission::PermissionObject`]) keep their extra keys. For agents the
//!   captured keys are additionally swept into `options`, which is how
//!   `reasoningEffort` and `thinking` reach the provider.
//!
//! # Deprecated keys are absent on purpose
//!
//! `mode`, `layout`, `autoshare`, and the agent-level `tools` and `maxSteps` exist
//! in the oracle but are **not** fields here. A config that uses them must fail
//! with an actionable message rather than be silently accepted, and producing that
//! message is the legacy-rejection pass's job — which is only possible if this
//! schema does not quietly absorb them.
//!
//! # The legacy TUI keys are the one exception
//!
//! `theme`, `keybinds`, and `tui` are *also* absent from the oracle's top-level
//! schema, and yet a config that carries them loads without complaint: the oracle
//! deletes all three from the loaded document **before** the unrecognized-key check
//! runs (`packages/opencode/src/config/config.ts:53-61`, applied at `:227`). They
//! belong to `tui.json` now, and the migration that relocates them
//! (`packages/opencode/src/config/tui-migrate.ts`) skips any directory that already
//! has a `tui.json` — so on a long-lived installation the keys stay in
//! `opencode.json` forever while the oracle keeps ignoring them.
//!
//! This port matches that exactly: [`LEGACY_TUI_KEYS`] is accepted, carried by a
//! [`LegacyTuiKey`] field that holds nothing, and never serialized. Nothing else is
//! relaxed — every other unrecognized top-level key is still rejected, and every
//! form on the legacy-rejection list still fails.

pub mod agent;
pub mod formatter;
pub mod lsp;
pub mod mcp;
pub mod ordered;
pub mod parse;
pub mod permission;
pub mod plugin;
pub mod provider;
pub mod reference;

#[cfg(test)]
mod tests;

use crate::schema::agent::AgentConfig;
use crate::schema::formatter::FormatterConfig;
use crate::schema::lsp::LspConfig;
use crate::schema::mcp::McpServerConfig;
use crate::schema::ordered::OrderedMap;
use crate::schema::permission::PermissionConfig;
use crate::schema::plugin::PluginSpec;
use crate::schema::provider::ProviderConfig;
use crate::schema::reference::ReferenceEntry;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

/// A free-form JSON object, for the oracle's `Record(String, Any | Unknown)`.
pub type JsonMap = serde_json::Map<String, serde_json::Value>;

/// Every key [`Config`] accepts, in declaration order.
///
/// Mirrors the `known` set that `topLevelExtraKeys` builds from the schema's
/// property signatures (`packages/opencode/src/config/parse.ts:74-78`).
pub const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "$schema",
    "shell",
    "logLevel",
    "server",
    "command",
    "skills",
    "references",
    "reference",
    "watcher",
    "snapshot",
    "plugin",
    "plugin_runtime",
    "share",
    "autoupdate",
    "disabled_providers",
    "enabled_providers",
    "model",
    "small_model",
    "default_agent",
    "subagent_depth",
    "username",
    "agent",
    "provider",
    "mcp",
    "formatter",
    "lsp",
    "instructions",
    "permission",
    "tools",
    "attachment",
    "enterprise",
    "tool_output",
    "compaction",
    "memory",
    "experimental",
];

/// The keys the oracle deletes from a loaded config before it checks for
/// unrecognized keys (`packages/opencode/src/config/config.ts:53-61`).
///
/// Deliberately not part of [`KNOWN_TOP_LEVEL_KEYS`], which is the set that
/// survives into the merged result. These are accepted and then dropped, which is
/// why the two lists are separate rather than one.
pub const LEGACY_TUI_KEYS: &[&str] = &["theme", "keybinds", "tui"];

/// A legacy TUI key's value, accepted and discarded.
///
/// Any JSON shape deserializes into this, and it carries nothing — so a config
/// that sets `theme` compares equal to one that does not, exactly as it would
/// after the oracle's `delete copy.theme`. The field is never serialized, so the
/// merged document matches the oracle's, which no longer has the key either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LegacyTuiKey;

impl<'de> Deserialize<'de> for LegacyTuiKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(Self)
    }
}

/// A parsed opencode config file — one layer, not a merged result.
///
/// Every field is optional because every field is optional in the oracle, and
/// because merging layers depends on being able to tell "absent" from "set to the
/// default".
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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
    /// Server configuration for `opencode serve` and the web command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerConfig>,
    /// Custom commands, keyed by command name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<OrderedMap<CommandConfig>>,
    /// Additional skill sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<SkillsConfig>,
    /// Named git or local directory references.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<OrderedMap<ReferenceEntry>>,
    /// Deprecated spelling of [`references`](Self::references).
    ///
    /// Still accepted by the oracle (`config/config.ts:46-48`), and not on the
    /// legacy-rejection list, so dropping it here would lose a real user's data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<OrderedMap<ReferenceEntry>>,
    /// File-watcher configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher: Option<WatcherConfig>,
    /// Record filesystem snapshots so edits can be undone. Defaults to true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<bool>,
    /// Plugins to load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<Vec<PluginSpec>>,
    /// Which plugin runtimes may start. Absent leaves JavaScript off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_runtime: Option<PluginRuntimeConfig>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<OrderedMap<AgentConfig>>,
    /// Custom providers and model overrides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<OrderedMap<ProviderConfig>>,
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
    /// Global permissions.
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
    /// Thresholds for truncating tool output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<ToolOutputConfig>,
    /// Context-compaction behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionConfig>,
    /// Persistent resident-memory configuration. Absent defaults to enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryConfig>,
    /// Options under active development.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<ExperimentalConfig>,
    /// Legacy TUI theme, moved to `tui.json`. Accepted, then discarded.
    #[serde(default, skip_serializing)]
    pub theme: LegacyTuiKey,
    /// Legacy TUI keybinds, moved to `tui.json`. Accepted, then discarded.
    #[serde(default, skip_serializing)]
    pub keybinds: LegacyTuiKey,
    /// Legacy TUI block, moved to `tui.json`. Accepted, then discarded.
    #[serde(default, skip_serializing)]
    pub tui: LegacyTuiKey,
}

impl Config {
    /// Resolve the memory master switch and all component defaults.
    #[must_use]
    pub fn resolved_memory(&self) -> ResolvedMemoryConfig {
        self.memory
            .as_ref()
            .map_or_else(ResolvedMemoryConfig::default, MemoryConfig::resolved)
    }

    /// Whether the configuration asks for the JavaScript plugin host.
    ///
    /// `false` for an absent key, which is what makes JavaScript plugins opt-in: each
    /// one starts a Node process, and paying for that on every invocation is a cost a
    /// user should choose rather than inherit.
    #[must_use]
    pub fn javascript_plugins_enabled(&self) -> bool {
        self.plugin_runtime
            .as_ref()
            .and_then(|runtime| runtime.javascript)
            .unwrap_or(false)
    }

    /// Whether out-of-process plugins found on disk may be started.
    ///
    /// `true` for an absent key, which is the opposite of
    /// [`Self::javascript_plugins_enabled`] and deliberately so. JavaScript is
    /// opt-in because it installs and starts a Node process the user never asked
    /// for. A process plugin needs no runtime and no install: the user marked a file
    /// executable and put it in a scanned directory, and there is no second step
    /// left for them to consent to. Gating it behind the JavaScript switch would
    /// make one tier's cost decide another tier's reachability.
    #[must_use]
    pub fn process_plugins_enabled(&self) -> bool {
        self.plugin_runtime
            .as_ref()
            .and_then(|runtime| runtime.process)
            .unwrap_or(true)
    }
}

/// Log level (`config/config.ts:27-30`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoupdateMode {
    /// Announce updates without installing them.
    Notify,
}

/// Update behaviour (`config/config.ts:67-71`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Autoupdate {
    /// Install updates automatically, or never.
    Enabled(bool),
    /// Announce updates only.
    Mode(AutoupdateMode),
}

/// Server configuration (`config/server.ts:6-18`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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
    /// mDNS domain; the runtime defaults to `opencode.local`.
    #[serde(rename = "mdnsDomain", skip_serializing_if = "Option::is_none")]
    pub mdns_domain: Option<String>,
    /// Additional origins to allow for CORS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<Vec<String>>,
}

/// One custom command (`config/command.ts:5-12`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// Additional skill sources (`config/skills.ts:5-12`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// Additional paths to skill folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// URLs to fetch skills from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
}

/// File-watcher configuration (`config/config.ts:49`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WatcherConfig {
    /// Glob patterns the watcher ignores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
}

/// Attachment processing (`config/attachment.ts:22-24`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AttachmentConfig {
    /// Image attachment limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageAttachmentConfig>,
}

/// Image attachment limits (`config/attachment.ts:6-19`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EnterpriseConfig {
    /// Enterprise URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Tool-output truncation thresholds (`config/config.ts:137-150`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolOutputConfig {
    /// Lines of output before truncation. Defaults to 2000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<NonZeroU32>,
    /// Bytes of output before truncation. Defaults to 51200.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<NonZeroU32>,
}

/// Context-compaction behaviour (`config/config.ts:151-172`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Compact automatically when the context fills. Defaults to true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
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

/// Default characters available to cross-project agent notes.
pub const DEFAULT_GLOBAL_MEMORY_CHAR_LIMIT: u32 = 2_200;

/// Default characters available to repository-local rules.
pub const DEFAULT_PROJECT_MEMORY_CHAR_LIMIT: u32 = 3_000;

/// Default delivered-turn interval for background reflection.
pub const DEFAULT_MEMORY_NUDGE_INTERVAL: u32 = 10;

/// Which plugin runtimes a session may start.
///
/// A table of its own rather than a field on any existing key because the decision
/// is per-runtime: JavaScript costs a Node process per plugin and is therefore
/// opt-in, while in-process runtimes are not gated here at all. Named
/// `plugin_runtime` and not `plugins` deliberately — a near-homograph of the
/// existing `plugin` list would let a typo silently mean the other key.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PluginRuntimeConfig {
    /// Start the JavaScript plugin host. Absent means no.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub javascript: Option<bool>,
    /// Start out-of-process plugins found on disk. Absent means yes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<bool>,
}

/// Persistent memory: a master boolean, or component settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MemoryConfig {
    /// `false` is the strict-parity kill switch; `true` selects all defaults.
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
            },
        }
    }
}

/// Fine-grained settings under the `memory` top-level key.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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
}

/// Fully defaulted memory settings consumed by runtime composition roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        }
    }
}

/// Options under active development (`config/config.ts:173-188`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyAction {
    /// Use of a provider.
    #[serde(rename = "provider.use")]
    ProviderUse,
}

/// Whether a policy statement permits or refuses (`packages/core/src/policy.ts:8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyEffect {
    /// Permit.
    Allow,
    /// Refuse.
    Deny,
}

/// One policy statement (`packages/core/src/policy.ts:11-15` plus the narrowed
/// `action` from `config/experimental.ts:11-14`). Every field is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyStatement {
    /// The action governed.
    pub action: PolicyAction,
    /// Permit or refuse.
    pub effect: PolicyEffect,
    /// The resource pattern the statement matches.
    pub resource: String,
}
