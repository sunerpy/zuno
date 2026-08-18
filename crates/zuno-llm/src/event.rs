//! Provider-neutral request content and streaming events.
//!
//! Providers translate their wire protocols into [`StreamEvent`]. The turn loop
//! consumes that one vocabulary without branching on provider names. Stored
//! transcript content and outbound request content are deliberately different
//! types: [`ContentBlock`] can retain reasoning that must never be replayed,
//! while [`RequestContentBlock`] has no variant capable of carrying it.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A Gemini per-tool-call thought signature.
///
/// Gemini 3 requires this value on the same function call when that call is
/// replayed in a later turn. Giving it a distinct type prevents serializers from
/// confusing it with an Anthropic thinking signature or arbitrary metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThoughtSignature(String);

impl ThoughtSignature {
    /// Wrap a provider-supplied thought signature.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The signature exactly as the provider supplied it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and return the provider value.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

/// A content block retained in the stored transcript.
///
/// The four reasoning block variants plus [`ThoughtSignature`] are intentionally
/// distinct. They have different replay rules, and collapsing them into a text
/// field would let one provider accidentally send another provider's private
/// state back on a later turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// User- or assistant-visible prose.
    Text { text: String },
    /// Unsigned reasoning captured from a provider.
    ///
    /// This is retained for display and debugging but excluded by the generic
    /// outbound conversion. In particular, Anthropic rejects replayed thinking
    /// that does not carry its matching signature.
    Reasoning { text: String },
    /// A history-only trace that is never replayed to any provider.
    ///
    /// Keeping traces in history supports recall and debugging. Omitting them
    /// from requests avoids paying their token cost on every later turn.
    ReasoningTrace { text: String },
    /// Signed thinking that may safely be replayed with its signature intact.
    SignedThinking { thinking: String, signature: String },
    /// A provider-native encrypted reasoning item for future-turn replay.
    ///
    /// OpenAI Responses can return this when storage is disabled. The native id,
    /// summary, encrypted payload, and status must survive together.
    ProviderEncryptedReasoning {
        id: String,
        summary: Vec<String>,
        encrypted_content: Option<String>,
        status: Option<String>,
    },
    /// A model-requested tool call.
    ToolUse {
        id: String,
        name: String,
        input: Value,
        /// Gemini 3's signature for this specific function call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<ThoughtSignature>,
    },
    /// The result paired with a prior [`ContentBlock::ToolUse`].
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Inline binary input represented in a provider-neutral form.
    Image { media_type: String, data: String },
}

impl ContentBlock {
    /// Convert a stored block into the provider-neutral outbound representation.
    ///
    /// Unsigned [`ContentBlock::Reasoning`] and history-only
    /// [`ContentBlock::ReasoningTrace`] return `None`. More importantly, the
    /// returned type has no equivalent variants, so later request shaping cannot
    /// accidentally put either form on the wire.
    #[must_use]
    pub fn to_request(&self) -> Option<RequestContentBlock> {
        match self {
            Self::Text { text } => Some(RequestContentBlock::Text { text: text.clone() }),
            Self::Reasoning { .. } | Self::ReasoningTrace { .. } => None,
            Self::SignedThinking {
                thinking,
                signature,
            } => Some(RequestContentBlock::SignedThinking {
                thinking: thinking.clone(),
                signature: signature.clone(),
            }),
            Self::ProviderEncryptedReasoning {
                id,
                summary,
                encrypted_content,
                status,
            } => Some(RequestContentBlock::ProviderEncryptedReasoning {
                id: id.clone(),
                summary: summary.clone(),
                encrypted_content: encrypted_content.clone(),
                status: status.clone(),
            }),
            Self::ToolUse {
                id,
                name,
                input,
                thought_signature,
            } => Some(RequestContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                thought_signature: thought_signature.clone(),
            }),
            Self::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(RequestContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            Self::Image { media_type, data } => Some(RequestContentBlock::Image {
                media_type: media_type.clone(),
                data: data.clone(),
            }),
        }
    }
}

/// A block permitted in a provider request.
///
/// There is deliberately no plain-reasoning or trace variant. Request builders
/// receive this type rather than [`ContentBlock`], making the generic unsafe
/// replay paths unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestContentBlock {
    /// User- or assistant-visible prose.
    Text { text: String },
    /// Thinking whose provider signature is preserved.
    SignedThinking { thinking: String, signature: String },
    /// A native encrypted reasoning item replayed as provider state.
    ProviderEncryptedReasoning {
        id: String,
        summary: Vec<String>,
        encrypted_content: Option<String>,
        status: Option<String>,
    },
    /// A model-requested tool call, optionally carrying Gemini's signature.
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<ThoughtSignature>,
    },
    /// A result paired with a prior tool call.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Inline binary input represented in a provider-neutral form.
    Image { media_type: String, data: String },
}

/// A stored message whose content has not yet been filtered for replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptMessage {
    /// Who produced the message.
    pub role: Role,
    /// Every stored block, including history-only reasoning.
    pub content: Vec<ContentBlock>,
}

impl TranscriptMessage {
    /// Build a stored message.
    #[must_use]
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    /// Build the safe provider request view of this transcript message.
    #[must_use]
    pub fn to_request(&self) -> Message {
        Message {
            role: self.role,
            content: self
                .content
                .iter()
                .filter_map(ContentBlock::to_request)
                .collect(),
        }
    }
}

/// One provider-bound message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Who produced the message.
    pub role: Role,
    /// Blocks that are structurally safe for generic replay.
    pub content: Vec<RequestContentBlock>,
}

impl Message {
    /// A plain-text message from `role`.
    #[must_use]
    pub fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![RequestContentBlock::Text { text: text.into() }],
        }
    }

    /// A message with already-filtered provider request content.
    #[must_use]
    pub fn from_content(role: Role, content: Vec<RequestContentBlock>) -> Self {
        Self { role, content }
    }
}

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Why a provider stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    /// The model completed the response normally.
    Stop,
    /// The configured or provider output limit was reached.
    Length,
    /// The model requested one or more tools.
    ToolCalls,
    /// A provider content filter stopped generation.
    ContentFilter,
    /// Generation ended because of an error.
    Error,
    /// The provider supplied no recognized reason.
    Unknown,
}

impl FinishReason {
    /// The stable wire/storage spelling used by the TypeScript schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool-calls",
            Self::ContentFilter => "content-filter",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for FinishReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How a provider's prompt figure relates to its cache figures.
///
/// Providers disagree, the numbers look identical either way, and no consumer can tell
/// them apart from the values alone — so the adapter that knows states it here rather
/// than leaving every reader to assume. A reader that assumes wrong either double-counts
/// the cache or loses it, and both were live: the status strip's session total added
/// `input + output + cache_read + cache_write` unconditionally, and the context
/// percentage added `input + cache_read`.
///
/// | Provider surface | Variant | Source |
/// | --- | --- | --- |
/// | OpenAI Chat Completions and Responses | [`Self::CacheInsideInput`] | `prompt_tokens_details.cached_tokens` is a breakdown of `prompt_tokens` |
/// | OpenAI-compatible endpoints | [`Self::CacheInsideInput`] | the OpenAI wire shape, so the OpenAI rule |
/// | Google Gemini `generateContent` | [`Self::CacheInsideInput`] | `cachedContentTokenCount` is part of `promptTokenCount` |
/// | Amazon Bedrock `ConverseStream` | [`Self::CacheInsideInput`] | `totalTokens` is `inputTokens + outputTokens`, cache excluded |
/// | Anthropic Messages | [`Self::CacheBesideInput`] | `input_tokens` excludes both cache figures; the three sum to the prompt |
/// | Google's Anthropic-compatible surface | [`Self::CacheBesideInput`] | the Anthropic wire shape |
/// | Bedrock `InvokeModelWithResponseStream` on Anthropic | [`Self::CacheBesideInput`] | the Anthropic wire shape |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAccounting {
    /// The prompt figure is the whole prompt; the cache figures itemise part of it.
    CacheInsideInput,
    /// The prompt figure excludes the cache figures; all three sum to the prompt.
    CacheBesideInput,
}

impl PromptAccounting {
    /// The wire spelling, for hosts that forward this to their own clients.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CacheInsideInput => "cache-inside-input",
            Self::CacheBesideInput => "cache-beside-input",
        }
    }

    /// The whole prompt this request sent, however the provider split it.
    #[must_use]
    pub const fn prompt_total(self, input: u64, cache_read: u64, cache_write: u64) -> u64 {
        match self {
            Self::CacheInsideInput => input,
            Self::CacheBesideInput => input.saturating_add(cache_read).saturating_add(cache_write),
        }
    }

    /// The prompt tokens that were neither read from nor written to the cache.
    ///
    /// The complement of [`Self::prompt_total`]: what remains once the cache figures are
    /// accounted for, which is the only part billed at the plain input rate.
    #[must_use]
    pub const fn uncached_input(self, input: u64, cache_read: u64, cache_write: u64) -> u64 {
        match self {
            Self::CacheInsideInput => input.saturating_sub(cache_read).saturating_sub(cache_write),
            Self::CacheBesideInput => input,
        }
    }
}

/// Connection progress reported while opening and consuming a provider stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPhase {
    /// Refreshing or obtaining provider credentials.
    Authenticating,
    /// Establishing the network transport.
    Connecting,
    /// Uploading request context and waiting for response headers.
    SendingRequest,
    /// The provider accepted the request but has not produced output yet.
    WaitingForResponse,
    /// At least one response byte has arrived.
    Streaming,
    /// A transient failure is backing off before another attempt.
    Retrying { attempt: u32, max: u32 },
}

impl fmt::Display for ConnectionPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authenticating => formatter.write_str("authenticating"),
            Self::Connecting => formatter.write_str("connecting"),
            Self::SendingRequest => formatter.write_str("sending request"),
            Self::WaitingForResponse => formatter.write_str("waiting for response"),
            Self::Streaming => formatter.write_str("streaming"),
            Self::Retrying { attempt, max } => write!(formatter, "retrying ({attempt}/{max})"),
        }
    }
}

/// One provider-neutral event emitted while completing a model turn.
///
/// The 24 variants cover every payload emitted by the five provider families.
/// Consumers should match variants explicitly: adding a provider capability must
/// force each projector to decide how that capability is stored and rendered.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A fragment of assistant-visible text.
    TextDelta(String),
    /// A tool call began.
    ToolUseStart { id: String, name: String },
    /// A raw JSON fragment for the current tool call.
    ToolInputDelta(String),
    /// The current tool call's input is complete.
    ToolUseEnd,
    /// Gemini's thought signature for the most recent tool call.
    ToolUseSignature(ThoughtSignature),
    /// A tool result produced inside the provider rather than by the turn loop.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// An image produced by a provider-native image-generation tool.
    GeneratedImage {
        id: String,
        path: String,
        metadata_path: Option<String>,
        output_format: String,
        revised_prompt: Option<String>,
    },
    /// A reasoning block began.
    ReasoningStart,
    /// A fragment of reasoning text.
    ReasoningDelta(String),
    /// A fragment of the current reasoning block's provider signature.
    ReasoningSignatureDelta(String),
    /// A provider-native encrypted reasoning item for future-turn replay.
    ProviderReasoningItem {
        id: String,
        summary: Vec<String>,
        encrypted_content: Option<String>,
        status: Option<String>,
    },
    /// The current reasoning block ended.
    ReasoningEnd,
    /// Reasoning completed, with its measured duration.
    ReasoningDone { duration_secs: f64 },
    /// The provider completed the message.
    MessageEnd { stop_reason: Option<FinishReason> },
    /// A provider will replay the same request from the beginning.
    ///
    /// Consumers must discard text, tools, and reasoning accumulated for the
    /// interrupted attempt. This is safe because the turn loop executes tools only
    /// after the provider stream completes.
    RetryRollback { attempt: u32, max: u32 },
    /// Provider token accounting, once known.
    ///
    /// The numbers are the provider's own, unaltered. `accounting` states how they fit
    /// together, because that differs by provider and no consumer can derive it: see
    /// [`PromptAccounting`].
    TokenUsage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_input_tokens: Option<u64>,
        cache_write_input_tokens: Option<u64>,
        accounting: PromptAccounting,
    },
    /// The active transport or connection implementation.
    ConnectionType { connection: String },
    /// A connection lifecycle update.
    ConnectionPhase { phase: ConnectionPhase },
    /// Provider-supplied human-readable transport detail.
    StatusDetail { detail: String },
    /// A stream error suitable for status projection.
    Error {
        message: String,
        /// The provider-requested delay, when one was supplied on the wire.
        retry_after: Option<Duration>,
    },
    /// A provider session id that can resume a conversation.
    SessionId(String),
    /// A provider or engine compaction boundary.
    Compaction {
        trigger: String,
        pre_tokens: Option<u64>,
        openai_encrypted_content: Option<String>,
    },
    /// The upstream selected by a routing provider such as OpenRouter.
    UpstreamProvider { provider: String },
    /// A provider-bridge tool call that the turn loop must execute.
    NativeToolCall {
        request_id: String,
        tool_name: String,
        input: Value,
    },
}
