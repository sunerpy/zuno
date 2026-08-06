use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The first revision of the Rust plugin protocol.
pub const PROTOCOL_VERSION: &str = "1.0";

/// Every wire hook, in the authoritative TypeScript declaration order.
pub const HOOK_NAMES: [&str; 21] = [
    "dispose",
    "event",
    "config",
    "tool",
    "auth",
    "provider",
    "chat.message",
    "chat.params",
    "chat.headers",
    "permission.ask",
    "command.execute.before",
    "tool.execute.before",
    "shell.env",
    "tool.execute.after",
    "experimental.chat.messages.transform",
    "experimental.chat.system.transform",
    "experimental.provider.small_model",
    "experimental.session.compacting",
    "experimental.compaction.autocontinue",
    "experimental.text.complete",
    "tool.definition",
];

/// Host identity sent while selecting a mutually supported protocol revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub name: String,
    pub version: String,
}

/// The host offers versions rather than assuming the plugin speaks its newest one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_versions: Vec<String>,
    pub host: HostInfo,
}

/// Metadata frozen after initialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    pub plugin: PluginManifest,
}

/// Resource and callback inventory returned by a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub hooks: Vec<String>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
}

/// One model-visible tool implemented in the plugin process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub id: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDefinition {
    /// Build a tool definition without repeating field names in plugin code.
    #[must_use]
    pub fn new(id: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// A typed hook envelope; each hook owns the concrete shapes inside the two values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookCall {
    pub hook: String,
    pub input: Value,
    pub output: Value,
}

/// The host replaces its mutable output only after this complete response arrives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookResult {
    pub output: Value,
}

/// Safe, data-only subset of the host's tool context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolContext {
    pub session_id: String,
    pub message_id: String,
    pub call_id: String,
    pub agent: String,
    pub depth: u32,
}

/// A tool request sent to the process that registered it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool: String,
    pub arguments: Value,
    pub context: ToolContext,
}

/// A file emitted with a tool's text result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "file")]
pub struct Attachment {
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
}

/// Successful tool output, wire-compatible with `oc_tool::ToolOutput`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub title: String,
    pub output: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

impl ToolOutput {
    /// Text-only output is the common plugin-tool case.
    #[must_use]
    pub fn text(title: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            output: output.into(),
            metadata: Map::new(),
            attachments: Vec::new(),
        }
    }
}
