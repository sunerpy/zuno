//! The chat-completions chunk shape, exactly as the recorded corpus shows it.
//!
//! Every field here was observed in
//! `packages/llm/test/fixtures/recordings/{openai-chat,openai-compatible-chat,cloudflare-workers-ai,cloudflare-ai-gateway}`,
//! which is real traffic from seven vendors. Nothing is speculative, and nothing
//! is a hand-written JSON schema: `serde` derives the reader from the shape.
//!
//! # Tolerances that are not optional
//!
//! - **`tool_calls: null`.** Some vendors send the key with a null value rather
//!   than omitting it. `#[serde(default)]` on an `Option` does not cover that for
//!   a `Vec`, so [`null_as_empty`] does.
//! - **Unknown fields are ignored.** Vendors add fields continuously
//!   (`obfuscation`, `system_fingerprint`, `service_tier`, `citations`). A
//!   `deny_unknown_fields` reader would turn every vendor release into an outage.
//! - **`reasoning_content` and `reasoning` are both read.** Cloudflare Workers AI
//!   and its AI Gateway emit `delta.reasoning_content`; other gateways emit
//!   `delta.reasoning`. Reading both costs nothing and neither is standard.

use serde::Deserialize;
use serde::de::Deserializer;

/// One `data:` payload from a chat-completions stream.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ChatChunk {
    /// The provider-assigned completion id, when present.
    #[serde(default)]
    pub id: Option<String>,
    /// Usually exactly one choice; `n > 1` is not used by this project.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub choices: Vec<ChunkChoice>,
    /// Token accounting, sent on the final chunk by most vendors.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// The upstream a router selected. OpenRouter and the Vercel gateway send it.
    #[serde(default)]
    pub provider: Option<String>,
    /// An error delivered inside the stream rather than as a status code.
    #[serde(default)]
    pub error: Option<WireError>,
}

/// One choice's delta and terminal reason.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ChunkChoice {
    /// Incremental content for this choice.
    #[serde(default)]
    pub delta: ChunkDelta,
    /// Why generation stopped, on the last chunk for this choice.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The incremental payload of one chunk.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ChunkDelta {
    /// Assistant-visible text.
    #[serde(default)]
    pub content: Option<String>,
    /// Reasoning under the non-standard name Cloudflare and DeepSeek use.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Reasoning under the other non-standard name some gateways use.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// A model refusal, which is a terminal state rather than text.
    #[serde(default)]
    pub refusal: Option<String>,
    /// Tool calls, arriving as name-then-arguments fragments.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub tool_calls: Vec<DeltaToolCall>,
}

impl ChunkDelta {
    /// The reasoning fragment under whichever name this vendor used.
    ///
    /// `reasoning_content` wins when a vendor sends both, because that is the
    /// name the recorded corpus carries.
    #[must_use]
    pub fn reasoning_fragment(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
    }
}

/// One streamed tool call fragment.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct DeltaToolCall {
    /// Position in the tool-call array; the only reliable identity across chunks.
    #[serde(default)]
    pub index: Option<u32>,
    /// The call id, sent once on the opening fragment.
    #[serde(default)]
    pub id: Option<String>,
    /// Name and argument fragments.
    #[serde(default)]
    pub function: Option<DeltaFunction>,
}

/// The function half of a tool-call fragment.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct DeltaFunction {
    /// Tool name, sent once on the opening fragment.
    #[serde(default)]
    pub name: Option<String>,
    /// A fragment of the JSON argument object, as text.
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Provider token accounting.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Usage {
    /// Input tokens.
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    /// Output tokens.
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    /// Cache accounting, when the vendor breaks it out.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// The cache half of usage.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct PromptTokensDetails {
    /// Input tokens served from the vendor's prompt cache.
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// A structured error, whether in-stream or in a non-2xx body.
///
/// Classification reads [`code`](WireError::code) and [`kind`](WireError::kind) —
/// structured fields — and never [`message`](WireError::message). The message is
/// payload for a human; the reference implementations both classified retryability
/// by lowercasing it, and `oc-error` exists so this crate never has to.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct WireError {
    /// The vendor's wording. Display only.
    #[serde(default)]
    pub message: Option<String>,
    /// A machine code. OpenAI sends a string, some vendors send an HTTP status.
    #[serde(default)]
    pub code: Option<serde_json::Value>,
    /// The error class, e.g. `invalid_request_error`.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

impl WireError {
    /// The numeric status the body carried, when `code` is a number or a numeric
    /// string.
    ///
    /// Several gateways report the upstream status only here — a 429 arriving as
    /// a 200 with `{"error":{"code":429}}` is common — so reading it is how a rate
    /// limit stays a rate limit.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self.code.as_ref()? {
            serde_json::Value::Number(number) => number.as_u64()?.try_into().ok(),
            serde_json::Value::String(text) => text.parse().ok(),
            _ => None,
        }
    }

    /// The string code, when `code` is a string.
    #[must_use]
    pub fn code_str(&self) -> Option<&str> {
        self.code.as_ref()?.as_str()
    }
}

/// A non-2xx body, which wraps the same error shape.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ErrorEnvelope {
    /// The error, when the vendor nests it.
    #[serde(default)]
    pub error: Option<WireError>,
    /// The error, when the vendor puts it at the top level.
    #[serde(default)]
    pub message: Option<String>,
    /// A top-level code, used by vendors that do not nest.
    #[serde(default)]
    pub code: Option<serde_json::Value>,
    /// A top-level class, used by vendors that do not nest.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

impl ErrorEnvelope {
    /// Flatten both spellings into one error.
    #[must_use]
    pub fn into_error(self) -> WireError {
        if let Some(error) = self.error {
            return error;
        }
        WireError {
            message: self.message,
            code: self.code,
            kind: self.kind,
        }
    }
}

/// Accepts `null` where a sequence is expected.
///
/// `#[serde(default)]` handles an absent key; it does not handle a present null.
/// Vendors send both.
fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// The sentinel that ends an OpenAI-compatible stream.
pub const DONE_SENTINEL: &str = "[DONE]";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_tool_calls_array_does_not_fail_the_chunk() {
        let chunk: ChatChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"content":"hi","tool_calls":null}}]}"#,
        )
        .expect("null tool_calls is tolerated");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
        assert!(chunk.choices[0].delta.tool_calls.is_empty());
    }

    #[test]
    fn unknown_vendor_fields_are_ignored_rather_than_fatal() {
        let chunk: ChatChunk = serde_json::from_str(
            r#"{"id":"x","object":"chat.completion.chunk","service_tier":"default",
                "obfuscation":"abc","choices":[{"index":0,"delta":{"content":"a"},
                "logprobs":null,"finish_reason":null}]}"#,
        )
        .expect("a new vendor field must not break the reader");
        assert_eq!(chunk.id.as_deref(), Some("x"));
    }

    #[test]
    fn both_non_standard_reasoning_names_are_read() {
        let cloudflare: ChatChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"reasoning_content":"We"}}]}"#)
                .expect("reasoning_content");
        assert_eq!(cloudflare.choices[0].delta.reasoning_fragment(), Some("We"));

        let gateway: ChatChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"reasoning":"think"}}]}"#)
                .expect("reasoning");
        assert_eq!(gateway.choices[0].delta.reasoning_fragment(), Some("think"));
    }

    #[test]
    fn a_numeric_code_is_read_as_a_status_and_a_string_code_is_not() {
        let numeric: WireError =
            serde_json::from_str(r#"{"code":429,"message":"slow down"}"#).expect("numeric code");
        assert_eq!(numeric.status(), Some(429));
        assert_eq!(numeric.code_str(), None);

        let textual: WireError =
            serde_json::from_str(r#"{"code":"context_length_exceeded"}"#).expect("string code");
        assert_eq!(textual.status(), None);
        assert_eq!(textual.code_str(), Some("context_length_exceeded"));
    }

    #[test]
    fn an_error_envelope_flattens_both_spellings() {
        let nested: ErrorEnvelope =
            serde_json::from_str(r#"{"error":{"code":"bad","message":"m"}}"#).expect("nested");
        assert_eq!(nested.into_error().code_str(), Some("bad"));

        let flat: ErrorEnvelope =
            serde_json::from_str(r#"{"code":"bad","message":"m"}"#).expect("flat");
        assert_eq!(flat.into_error().code_str(), Some("bad"));
    }
}
