use std::collections::BTreeMap;

use oc_db::message::{MessageRecord, PartRecord};
use oc_llm::catalog::resolved::{JsonMap, ResolvedModel, ResolvedProvider};
use oc_llm::event::Message;
use oc_permission::PermissionRequest;
use serde_json::Value;

/// The selected model identity in `chat.message`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSelection<'a> {
    pub provider_id: &'a str,
    pub model_id: &'a str,
}

/// Input of `chat.message` (`index.ts:234-243`).
#[derive(Debug, Clone, Copy)]
pub struct ChatMessageInput<'a> {
    pub session_id: &'a str,
    pub agent: Option<&'a str>,
    pub model: Option<ModelSelection<'a>>,
    pub message_id: Option<&'a str>,
    pub variant: Option<&'a str>,
}

/// Mutable output of `chat.message`, including the complete upstream user-message metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessageOutput {
    pub message: MessageRecord,
    pub parts: Vec<PartRecord>,
}

/// How a provider became available to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSource {
    Env,
    Config,
    Custom,
    Api,
}

/// Provider information passed to request hooks (`index.ts:20-24`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderContext {
    pub source: ProviderSource,
    pub info: ResolvedProvider,
    pub options: JsonMap,
}

/// Shared input of `chat.params` and `chat.headers`.
#[derive(Debug, Clone)]
pub struct ChatContext<'a> {
    pub session_id: &'a str,
    pub agent: &'a str,
    pub model: &'a ResolvedModel,
    pub provider: &'a ProviderContext,
    pub message: Message,
}

/// Mutable provider parameters from `chat.params` (`index.ts:247-256`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChatParamsOutput {
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: f64,
    pub max_output_tokens: Option<u64>,
    pub options: JsonMap,
}

/// Mutable request headers from `chat.headers`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatHeadersOutput {
    pub headers: BTreeMap<String, String>,
}

/// Input of `permission.ask`.
#[derive(Debug, Clone, Copy)]
pub struct PermissionAskInput<'a> {
    pub request: &'a PermissionRequest,
}

/// Plugin override for a permission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionStatus {
    #[default]
    Ask,
    Deny,
    Allow,
}

/// Mutable output of `permission.ask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PermissionAskOutput {
    pub status: PermissionStatus,
}

/// Input of `command.execute.before`.
#[derive(Debug, Clone, Copy)]
pub struct CommandExecuteBeforeInput<'a> {
    pub command: &'a str,
    pub session_id: &'a str,
    pub arguments: &'a str,
}

/// Mutable command parts from `command.execute.before`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandExecuteBeforeOutput {
    pub parts: Vec<PartRecord>,
}

/// Input of `tool.execute.before`.
#[derive(Debug, Clone, Copy)]
pub struct ToolExecuteBeforeInput<'a> {
    pub tool: &'a str,
    pub session_id: &'a str,
    pub call_id: &'a str,
}

/// Mutable arguments from `tool.execute.before`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecuteBeforeOutput {
    pub args: Value,
}

/// Input of `shell.env`.
#[derive(Debug, Clone, Copy)]
pub struct ShellEnvInput<'a> {
    pub cwd: &'a str,
    pub session_id: Option<&'a str>,
    pub call_id: Option<&'a str>,
}

/// Mutable environment from `shell.env`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellEnvOutput {
    pub env: BTreeMap<String, String>,
}

/// Input of `tool.execute.after`.
#[derive(Debug, Clone, Copy)]
pub struct ToolExecuteAfterInput<'a> {
    pub tool: &'a str,
    pub session_id: &'a str,
    pub call_id: &'a str,
    pub args: &'a Value,
}

/// One `{ info, parts }` entry in the messages-transform hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageWithParts {
    pub info: Message,
    pub parts: Vec<PartRecord>,
}

/// Mutable output of `experimental.chat.messages.transform`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatMessagesTransformOutput {
    pub messages: Vec<MessageWithParts>,
}

/// Input of `experimental.chat.system.transform`.
#[derive(Debug, Clone, Copy)]
pub struct ChatSystemTransformInput<'a> {
    pub session_id: Option<&'a str>,
    pub model: &'a ResolvedModel,
}

/// Mutable system fragments from the system-transform hook.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatSystemTransformOutput {
    pub system: Vec<String>,
}

/// Input of `experimental.provider.small_model`.
#[derive(Debug, Clone, Copy)]
pub struct ProviderSmallModelInput<'a> {
    pub provider: &'a ResolvedProvider,
}

/// Mutable small-model selection.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderSmallModelOutput {
    pub model: Option<ResolvedModel>,
}

/// Input of `experimental.session.compacting`.
#[derive(Debug, Clone, Copy)]
pub struct SessionCompactingInput<'a> {
    pub session_id: &'a str,
}

/// Mutable compaction additions and replacement prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionCompactingOutput {
    pub context: Vec<String>,
    pub prompt: Option<String>,
}

/// Input of `experimental.compaction.autocontinue`.
#[derive(Debug, Clone, Copy)]
pub struct CompactionAutocontinueInput<'a> {
    pub context: &'a ChatContext<'a>,
    pub overflow: bool,
}

/// Mutable auto-continue policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionAutocontinueOutput {
    pub enabled: bool,
}

/// Input of `experimental.text.complete`.
#[derive(Debug, Clone, Copy)]
pub struct TextCompleteInput<'a> {
    pub session_id: &'a str,
    pub message_id: &'a str,
    pub part_id: &'a str,
}

/// Mutable completed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCompleteOutput {
    pub text: String,
}

/// Input of `tool.definition`.
#[derive(Debug, Clone, Copy)]
pub struct ToolDefinitionInput<'a> {
    pub tool_id: &'a str,
}
