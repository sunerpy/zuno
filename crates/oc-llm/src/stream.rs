//! Attempt-local accumulation for provider streams.
//!
//! This module does not project events into database parts or execute tools. It
//! holds only the partial values a turn-loop consumer must discard when a
//! [`StreamEvent::RetryRollback`] announces that the provider is replaying the
//! request from the beginning.

use crate::event::{StreamEvent, ThoughtSignature};

/// A tool call whose JSON input is still arriving as text fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallAccumulator {
    /// Provider-assigned tool call id.
    pub id: String,
    /// Tool name supplied by the model.
    pub name: String,
    /// Concatenated JSON fragments, intentionally not parsed until completion.
    pub raw_input: String,
    /// Gemini thought signature paired with this call, when present.
    pub thought_signature: Option<ThoughtSignature>,
}

impl ToolCallAccumulator {
    fn new(id: String, name: String) -> Self {
        Self {
            id,
            name,
            raw_input: String::new(),
            thought_signature: None,
        }
    }
}

/// Partial output belonging to one provider attempt.
///
/// Tools are only data here. The engine executes them after a stream completes,
/// which is the safety precondition that makes rollback side-effect free.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamAccumulator {
    text: String,
    tool_calls: Vec<ToolCallAccumulator>,
    reasoning: String,
    reasoning_signature: String,
}

impl StreamAccumulator {
    /// Create an empty attempt accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one event's attempt-local effect.
    ///
    /// Every event variant is listed explicitly so extending the vocabulary
    /// forces this rollback boundary to be reviewed.
    pub fn apply(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::TextDelta(delta) => self.text.push_str(delta),
            StreamEvent::ToolUseStart { id, name } => self
                .tool_calls
                .push(ToolCallAccumulator::new(id.clone(), name.clone())),
            StreamEvent::ToolInputDelta(delta) => {
                if let Some(tool_call) = self.tool_calls.last_mut() {
                    tool_call.raw_input.push_str(delta);
                }
            }
            StreamEvent::ToolUseEnd => {}
            StreamEvent::ToolUseSignature(signature) => {
                if let Some(tool_call) = self.tool_calls.last_mut() {
                    tool_call.thought_signature = Some(signature.clone());
                }
            }
            StreamEvent::ReasoningDelta(delta) => self.reasoning.push_str(delta),
            StreamEvent::ReasoningSignatureDelta(delta) => {
                self.reasoning_signature.push_str(delta);
            }
            StreamEvent::RetryRollback { .. } => self.clear_attempt(),
            StreamEvent::ToolResult { .. }
            | StreamEvent::GeneratedImage { .. }
            | StreamEvent::ReasoningStart
            | StreamEvent::ProviderReasoningItem { .. }
            | StreamEvent::ReasoningEnd
            | StreamEvent::ReasoningDone { .. }
            | StreamEvent::MessageEnd { .. }
            | StreamEvent::TokenUsage { .. }
            | StreamEvent::ConnectionType { .. }
            | StreamEvent::ConnectionPhase { .. }
            | StreamEvent::StatusDetail { .. }
            | StreamEvent::Error { .. }
            | StreamEvent::SessionId(_)
            | StreamEvent::Compaction { .. }
            | StreamEvent::UpstreamProvider { .. }
            | StreamEvent::NativeToolCall { .. } => {}
        }
    }

    /// Discard every partial value belonging to the interrupted attempt.
    pub fn clear_attempt(&mut self) {
        self.text.clear();
        self.tool_calls.clear();
        self.reasoning.clear();
        self.reasoning_signature.clear();
    }

    /// Concatenated assistant-visible text for the current attempt.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Tool calls begun during the current attempt.
    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCallAccumulator] {
        &self.tool_calls
    }

    /// Concatenated reasoning text for the current attempt.
    #[must_use]
    pub fn reasoning(&self) -> &str {
        &self.reasoning
    }

    /// Concatenated provider signature for the current reasoning block.
    #[must_use]
    pub fn reasoning_signature(&self) -> &str {
        &self.reasoning_signature
    }

    /// Whether the current attempt has accumulated no text, tools, or reasoning.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.tool_calls.is_empty()
            && self.reasoning.is_empty()
            && self.reasoning_signature.is_empty()
    }
}
