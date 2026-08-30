//! Prompt assembly that preserves a provider's reusable request prefix.
//!
//! [`PromptCache`] owns the static system prompt for a session, so per-turn APIs
//! cannot replace it. Dynamic context and memory have a different type and enter
//! the request through dedicated, non-cacheable developer-context items. The
//! append-only tracker is a second line of defense for callers that assemble
//! stable history, while the tool lock permits one intentional cache miss when
//! asynchronous MCP discovery
//! finishes and then freezes again.

use crate::registry::Message;
use sha2::{Digest as _, Sha256};
use std::io;
use std::sync::Arc;

type MessageFingerprint = [u8; 32];

struct DigestWriter<'digest>(&'digest mut Sha256);

impl io::Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn message_fingerprint(message: &Message) -> MessageFingerprint {
    let mut digest = Sha256::new();
    serde_json::to_writer(DigestWriter(&mut digest), message)
        .expect("serializing a provider message into SHA-256 cannot fail");
    digest.finalize().into()
}

/// Immutable, cacheable system instructions for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticSystemPrompt(Arc<str>);

impl StaticSystemPrompt {
    /// Freeze the system prefix for a session.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(Arc::from(value.into()))
    }

    /// The exact string sent through the provider's cacheable system field.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The exact bytes used for cache-stability comparisons.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Volatile context for one turn, deliberately distinct from the static prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicContext {
    turn_context: String,
    memory: Option<String>,
    runtime_instructions: Vec<String>,
}

impl DynamicContext {
    /// Create the changing context for the current turn.
    #[must_use]
    pub fn new(turn_context: impl Into<String>) -> Self {
        Self {
            turn_context: turn_context.into(),
            memory: None,
            runtime_instructions: Vec::new(),
        }
    }

    /// Attach changing session memory to the same non-cacheable suffix.
    #[must_use]
    pub fn with_memory(mut self, memory: impl Into<String>) -> Self {
        self.memory = Some(memory.into());
        self
    }

    /// Append an engine-owned instruction that applies only to this request.
    #[must_use]
    pub fn with_runtime_instruction(mut self, instruction: impl Into<String>) -> Self {
        self.runtime_instructions.push(instruction.into());
        self
    }

    /// Whether none of the dynamic sources contains non-whitespace text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turn_context.trim().is_empty()
            && self
                .memory
                .as_deref()
                .is_none_or(|memory| memory.trim().is_empty())
            && self
                .runtime_instructions
                .iter()
                .all(|instruction| instruction.trim().is_empty())
    }

    fn into_developer_context(self) -> Vec<String> {
        let mut sections = Vec::with_capacity(2 + self.runtime_instructions.len());
        let turn_context = self.turn_context.trim();
        if !turn_context.is_empty() {
            sections.push(turn_context.to_owned());
        }
        if let Some(memory) = self.memory.as_deref().map(str::trim)
            && !memory.is_empty()
        {
            sections.push(memory.to_owned());
        }
        sections.extend(
            self.runtime_instructions
                .into_iter()
                .filter_map(|instruction| {
                    let instruction = instruction.trim();
                    (!instruction.is_empty()).then(|| instruction.to_owned())
                }),
        );
        sections
    }
}

/// A split prompt whose static half is frozen and whose dynamic half is a suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitSystemPrompt {
    static_prefix: StaticSystemPrompt,
}

impl SplitSystemPrompt {
    /// Create the split at the stable/volatile boundary.
    #[must_use]
    pub fn new(static_prefix: impl Into<String>) -> Self {
        Self {
            static_prefix: StaticSystemPrompt::new(static_prefix),
        }
    }

    /// The cacheable system prefix.
    #[must_use]
    pub fn static_prefix(&self) -> &StaticSystemPrompt {
        &self.static_prefix
    }
}

/// Why the stable request prefix was not append-only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheViolation {
    /// The system prefix changed after its baseline request.
    #[error(
        "append-only cache violation on turn {turn}: static system prefix changed byte-for-byte"
    )]
    StaticPrefixChanged {
        /// Request attempt that observed the change.
        turn: u64,
    },
    /// Earlier persisted messages were removed.
    #[error(
        "append-only cache violation on turn {turn}: stable history shrank from {previous} to {current} messages"
    )]
    HistoryShrank {
        /// Request attempt that observed the change.
        turn: u64,
        /// Stable messages in the preceding request.
        previous: usize,
        /// Stable messages in this request.
        current: usize,
    },
    /// A persisted message changed in place.
    #[error("append-only cache violation on turn {turn}: stable history message {index} changed")]
    HistoryPrefixChanged {
        /// Request attempt that observed the change.
        turn: u64,
        /// Zero-based index of the first changed message.
        index: usize,
    },
}

/// Detects mutations to the static prompt or persisted message prefix.
///
/// Dynamic context is intentionally absent from this API. Callers record the
/// stable history before [`PromptCache`] appends the volatile suffix.
#[derive(Debug, Clone, Default)]
pub struct CacheTracker {
    previous_static: Option<Vec<u8>>,
    previous_history: Vec<MessageFingerprint>,
    turn: u64,
}

impl CacheTracker {
    /// An empty tracker whose first request establishes the baseline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the stable prefix before sending a provider request.
    ///
    /// On failure the valid baseline is retained, so retrying the same invalid
    /// request cannot silently redefine what counts as stable.
    pub fn record(
        &mut self,
        static_prefix: &StaticSystemPrompt,
        stable_history: &[Message],
    ) -> Result<(), CacheViolation> {
        self.turn += 1;
        if let Some(previous_static) = &self.previous_static {
            if previous_static.as_slice() != static_prefix.as_bytes() {
                return Err(CacheViolation::StaticPrefixChanged { turn: self.turn });
            }
            if stable_history.len() < self.previous_history.len() {
                return Err(CacheViolation::HistoryShrank {
                    turn: self.turn,
                    previous: self.previous_history.len(),
                    current: stable_history.len(),
                });
            }
            if let Some(index) = self
                .previous_history
                .iter()
                .zip(stable_history)
                .position(|(previous, current)| *previous != message_fingerprint(current))
            {
                return Err(CacheViolation::HistoryPrefixChanged {
                    turn: self.turn,
                    index,
                });
            }
        }

        self.previous_static = Some(static_prefix.as_bytes().to_vec());
        self.previous_history = stable_history.iter().map(message_fingerprint).collect();
        Ok(())
    }

    /// Forget the baseline after an intentional cache break such as compaction.
    pub fn reset(&mut self) {
        self.previous_static = None;
        self.previous_history.clear();
        self.turn = 0;
    }

    /// Request attempts observed since construction or the last reset.
    #[must_use]
    pub const fn turn(&self) -> u64 {
        self.turn
    }
}

/// Whether asynchronous MCP tool discovery can still add tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpToolStatus {
    /// At least one MCP connection may still register tools.
    Pending,
    /// MCP discovery has settled for this request.
    Ready,
}

/// One immutable tool snapshot selected for a provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSnapshot<T> {
    tools: Vec<T>,
    rebuilt_for_late_mcp: bool,
}

impl<T> ToolSnapshot<T> {
    /// The frozen tools to send to the provider.
    #[must_use]
    pub fn tools(&self) -> &[T] {
        &self.tools
    }

    /// Whether this request consumed the one permitted late-MCP rebuild.
    #[must_use]
    pub const fn rebuilt_for_late_mcp(&self) -> bool {
        self.rebuilt_for_late_mcp
    }

    /// Consume the snapshot.
    #[must_use]
    pub fn into_tools(self) -> Vec<T> {
        self.tools
    }
}

/// A tool list frozen on first request with one late-MCP rebuild allowance.
#[derive(Debug, Clone)]
pub struct LockedTools<T> {
    locked: Option<Vec<T>>,
    late_mcp_resolved: bool,
    rebuild_count: u8,
}

impl<T> Default for LockedTools<T> {
    fn default() -> Self {
        Self {
            locked: None,
            late_mcp_resolved: false,
            rebuild_count: 0,
        }
    }
}

impl<T: Clone + PartialEq> LockedTools<T> {
    /// An unlocked tool list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Freeze `available` or return the already-frozen snapshot.
    ///
    /// Todo 47's MCP merge calls this with [`McpToolStatus::Ready`] when its
    /// asynchronous discovery settles. If the first request was made while MCP
    /// was pending and the list changed, this replaces the snapshot exactly once.
    /// Later registry changes are ignored until [`reset`](Self::reset).
    pub fn tools_for_request(
        &mut self,
        available: &[T],
        mcp_status: McpToolStatus,
    ) -> ToolSnapshot<T> {
        if self.locked.is_none() {
            self.locked = Some(available.to_vec());
            self.late_mcp_resolved = mcp_status == McpToolStatus::Ready;
            return ToolSnapshot {
                tools: available.to_vec(),
                rebuilt_for_late_mcp: false,
            };
        }

        let mut rebuilt = false;
        if !self.late_mcp_resolved && mcp_status == McpToolStatus::Ready {
            let locked = self.locked.as_ref().expect("locked snapshot checked above");
            if locked != available {
                self.locked = Some(available.to_vec());
                self.rebuild_count = 1;
                rebuilt = true;
            }
            self.late_mcp_resolved = true;
        }

        ToolSnapshot {
            tools: self
                .locked
                .as_ref()
                .expect("locked snapshot initialized above")
                .clone(),
            rebuilt_for_late_mcp: rebuilt,
        }
    }

    /// Number of accepted late-MCP rebuilds in the current lock generation.
    #[must_use]
    pub const fn rebuild_count(&self) -> u8 {
        self.rebuild_count
    }

    /// Explicitly unlock tools and re-arm one late-MCP rebuild.
    pub fn reset(&mut self) {
        self.locked = None;
        self.late_mcp_resolved = false;
        self.rebuild_count = 0;
    }
}

/// A complete request snapshot after cache-safe prompt and tool assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTurn<T> {
    system_static: StaticSystemPrompt,
    messages: Vec<Message>,
    developer_context: Vec<String>,
    tools: Vec<T>,
    rebuilt_tools: bool,
}

impl<T> PreparedTurn<T> {
    /// Cacheable system text, byte-identical for the life of the session.
    #[must_use]
    pub fn system_static(&self) -> &str {
        self.system_static.as_str()
    }

    /// Persisted provider history. Volatile policy is kept out of user messages.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Independent, volatile developer-context items for this request.
    #[must_use]
    pub fn developer_context(&self) -> &[String] {
        &self.developer_context
    }

    /// Frozen tools for this request.
    #[must_use]
    pub fn tools(&self) -> &[T] {
        &self.tools
    }

    /// Whether the tool snapshot intentionally broke cache once for late MCP.
    #[must_use]
    pub const fn rebuilt_tools(&self) -> bool {
        self.rebuilt_tools
    }

    /// Remove tool authority from this request without changing the session lock.
    ///
    /// This is used for an engine-owned text-only finalization request. A later
    /// turn may still reuse the stable locked tool snapshot.
    #[must_use]
    pub fn without_tools(mut self) -> Self {
        self.tools.clear();
        self
    }

    /// Consume the prepared snapshot without cloning its potentially large history.
    ///
    /// Provider requests own their messages and tools. Callers that have finished
    /// observing request metadata should move both vectors into that request rather
    /// than retaining this snapshot and cloning the complete prompt again.
    #[must_use]
    pub fn into_request_parts(self) -> (Vec<Message>, Vec<String>, Vec<T>) {
        (self.messages, self.developer_context, self.tools)
    }
}

/// Session-owned prompt-cache discipline combining all four stability mechanisms.
#[derive(Debug, Clone)]
pub struct PromptCache<T> {
    prompt: SplitSystemPrompt,
    tracker: CacheTracker,
    tools: LockedTools<T>,
}

impl<T: Clone + PartialEq> PromptCache<T> {
    /// Start a session and freeze its cacheable system prefix.
    #[must_use]
    pub fn new(static_prefix: impl Into<String>) -> Self {
        Self {
            prompt: SplitSystemPrompt::new(static_prefix),
            tracker: CacheTracker::new(),
            tools: LockedTools::new(),
        }
    }

    /// Assemble one provider request without allowing volatile data into the prefix.
    ///
    /// Stable history is tracked before dynamic context is appended. A late-MCP
    /// rebuild resets the message baseline because that request is already the one
    /// explicitly accepted cache miss.
    pub fn prepare_turn(
        &mut self,
        stable_history: &[Message],
        dynamic: DynamicContext,
        available_tools: &[T],
        mcp_status: McpToolStatus,
    ) -> Result<PreparedTurn<T>, CacheViolation> {
        let tool_snapshot = self.tools.tools_for_request(available_tools, mcp_status);
        if tool_snapshot.rebuilt_for_late_mcp() {
            self.tracker.reset();
        }
        self.tracker
            .record(self.prompt.static_prefix(), stable_history)?;
        Ok(PreparedTurn {
            system_static: self.prompt.static_prefix().clone(),
            messages: stable_history.to_vec(),
            developer_context: dynamic.into_developer_context(),
            tools: tool_snapshot.into_tools(),
            rebuilt_tools: self.tools.rebuild_count() == 1 && self.tracker.turn() == 1,
        })
    }

    /// Assemble a provider request by taking ownership of its stable history.
    ///
    /// This is equivalent to [`Self::prepare_turn`], but avoids cloning a large
    /// provider projection into the prepared request. The cache tracker records
    /// fixed-size fingerprints before the volatile suffix is appended, so moving
    /// the vector does not weaken append-only validation.
    pub fn prepare_turn_owned(
        &mut self,
        stable_history: Vec<Message>,
        dynamic: DynamicContext,
        available_tools: &[T],
        mcp_status: McpToolStatus,
    ) -> Result<PreparedTurn<T>, CacheViolation> {
        let tool_snapshot = self.tools.tools_for_request(available_tools, mcp_status);
        if tool_snapshot.rebuilt_for_late_mcp() {
            self.tracker.reset();
        }
        self.tracker
            .record(self.prompt.static_prefix(), &stable_history)?;
        Ok(PreparedTurn {
            system_static: self.prompt.static_prefix().clone(),
            messages: stable_history,
            developer_context: dynamic.into_developer_context(),
            tools: tool_snapshot.into_tools(),
            rebuilt_tools: self.tools.rebuild_count() == 1 && self.tracker.turn() == 1,
        })
    }

    /// Inspect append-only tracking state.
    #[must_use]
    pub const fn tracker(&self) -> &CacheTracker {
        &self.tracker
    }

    /// Inspect or explicitly reset the locked tool policy.
    #[must_use]
    pub const fn locked_tools(&self) -> &LockedTools<T> {
        &self.tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{RequestContentBlock, Role};

    fn message(role: Role, text: &str) -> Message {
        Message::new(role, text)
    }

    #[test]
    fn cache_static_prefix_is_byte_stable_across_three_dynamic_turns() {
        let static_text = "You are a coding agent.\nProject instructions are stable.";
        let mut cache = PromptCache::new(static_text);

        let history_one = vec![message(Role::User, "first question")];
        let turn_one = cache
            .prepare_turn(
                &history_one,
                DynamicContext::new("clock=10:00").with_memory("memory generation one"),
                &["shell"],
                McpToolStatus::Pending,
            )
            .unwrap();

        let history_two = vec![
            message(Role::User, "first question"),
            message(Role::Assistant, "first answer"),
            message(Role::User, "second question"),
        ];
        let turn_two = cache
            .prepare_turn(
                &history_two,
                DynamicContext::new("clock=10:01").with_memory("memory generation two"),
                &["shell", "mcp-memory"],
                McpToolStatus::Ready,
            )
            .unwrap();

        let history_three = vec![
            message(Role::User, "first question"),
            message(Role::Assistant, "first answer"),
            message(Role::User, "second question"),
            message(Role::Assistant, "second answer"),
            message(Role::User, "third question"),
        ];
        let turn_three = cache
            .prepare_turn(
                &history_three,
                DynamicContext::new("clock=10:02").with_memory("memory generation three"),
                &["shell", "mcp-memory", "mcp-late-second-wave"],
                McpToolStatus::Ready,
            )
            .unwrap();

        let static_bytes = [
            turn_one.system_static().as_bytes(),
            turn_two.system_static().as_bytes(),
            turn_three.system_static().as_bytes(),
        ];
        assert!(static_bytes.windows(2).all(|pair| pair[0] == pair[1]));
        assert!(
            static_bytes
                .iter()
                .all(|bytes| *bytes == static_text.as_bytes())
        );

        let dynamic_contexts = [
            turn_one.developer_context(),
            turn_two.developer_context(),
            turn_three.developer_context(),
        ];
        assert!(
            dynamic_contexts.iter().all(|context| context.len() == 2),
            "turn context and memory must remain independent developer items"
        );
        let dynamic_texts = dynamic_contexts.map(|context| context.join("\n"));
        assert_ne!(dynamic_texts[0], dynamic_texts[1]);
        assert_ne!(dynamic_texts[1], dynamic_texts[2]);
        assert!(dynamic_texts[0].contains("memory generation one"));
        assert!(dynamic_texts[1].contains("memory generation two"));
        assert!(dynamic_texts[2].contains("memory generation three"));
        assert!(!turn_one.system_static().contains("clock="));
        assert!(!turn_one.system_static().contains("memory generation"));

        assert_eq!(turn_one.tools(), ["shell"]);
        assert_eq!(turn_two.tools(), ["shell", "mcp-memory"]);
        assert_eq!(
            turn_three.tools(),
            ["shell", "mcp-memory"],
            "a second MCP wave must not trigger another cache-busting rebuild"
        );
        assert!(!turn_one.rebuilt_tools());
        assert!(turn_two.rebuilt_tools());
        assert!(!turn_three.rebuilt_tools());
        assert_eq!(cache.locked_tools().rebuild_count(), 1);
    }

    #[test]
    fn cache_tracker_catches_volatile_static_prefix_injection() {
        let history = [message(Role::User, "stable question")];
        let mut tracker = CacheTracker::new();
        tracker
            .record(&StaticSystemPrompt::new("clock=10:00"), &history)
            .unwrap();

        let violation = tracker
            .record(&StaticSystemPrompt::new("clock=10:01"), &history)
            .unwrap_err();

        assert_eq!(violation, CacheViolation::StaticPrefixChanged { turn: 2 });
        assert_eq!(
            violation.to_string(),
            "append-only cache violation on turn 2: static system prefix changed byte-for-byte"
        );
    }

    #[test]
    fn cache_tracker_retains_only_fixed_size_message_fingerprints() {
        let static_prompt = StaticSystemPrompt::new("stable");
        let large_history = [message(Role::User, &"x".repeat(2 * 1024 * 1024))];
        let mut tracker = CacheTracker::new();

        tracker.record(&static_prompt, &large_history).unwrap();

        fn assert_fingerprint_storage(_: &[MessageFingerprint]) {}
        assert_fingerprint_storage(&tracker.previous_history);
        assert_eq!(tracker.previous_history.len(), 1);
        assert!(
            tracker.previous_history.capacity() * std::mem::size_of::<[u8; 32]>() <= 128,
            "the append-only baseline must not retain the multi-megabyte message body"
        );
    }

    #[test]
    fn owned_prepare_moves_large_history_into_the_request() {
        let body = "x".repeat(2 * 1024 * 1024);
        let history = vec![message(Role::User, &body)];
        let original_ptr = match &history[0].content[0] {
            RequestContentBlock::Text { text } => text.as_ptr(),
            other => panic!("unexpected request block: {other:?}"),
        };
        let mut cache = PromptCache::new("stable");

        let prepared = cache
            .prepare_turn_owned(
                history,
                DynamicContext::default(),
                &["shell"],
                McpToolStatus::Ready,
            )
            .unwrap();

        let moved_ptr = match &prepared.messages()[0].content[0] {
            RequestContentBlock::Text { text } => text.as_ptr(),
            other => panic!("unexpected request block: {other:?}"),
        };
        assert_eq!(
            moved_ptr, original_ptr,
            "the owned path cloned the multi-megabyte message body"
        );
    }

    #[test]
    fn cache_tracker_rejects_changed_or_removed_history() {
        let static_prompt = StaticSystemPrompt::new("stable");
        let mut changed_tracker = CacheTracker::new();
        changed_tracker
            .record(
                &static_prompt,
                &[
                    message(Role::User, "question"),
                    message(Role::Assistant, "answer"),
                ],
            )
            .unwrap();
        let changed = changed_tracker
            .record(
                &static_prompt,
                &[
                    message(Role::User, "modified question"),
                    message(Role::Assistant, "answer"),
                    message(Role::User, "next"),
                ],
            )
            .unwrap_err();
        assert_eq!(
            changed,
            CacheViolation::HistoryPrefixChanged { turn: 2, index: 0 }
        );

        let mut shrunk_tracker = CacheTracker::new();
        shrunk_tracker
            .record(
                &static_prompt,
                &[
                    message(Role::User, "question"),
                    message(Role::Assistant, "answer"),
                ],
            )
            .unwrap();
        let shrunk = shrunk_tracker
            .record(&static_prompt, &[message(Role::User, "question")])
            .unwrap_err();
        assert_eq!(
            shrunk,
            CacheViolation::HistoryShrank {
                turn: 2,
                previous: 2,
                current: 1,
            }
        );
    }

    #[test]
    fn locked_tools_accept_exactly_one_late_mcp_rebuild() {
        let mut tools = LockedTools::new();
        let first = tools.tools_for_request(&["shell"], McpToolStatus::Pending);
        let rebuilt = tools.tools_for_request(&["shell", "mcp-first"], McpToolStatus::Ready);
        let frozen =
            tools.tools_for_request(&["shell", "mcp-first", "mcp-second"], McpToolStatus::Ready);

        assert_eq!(first.tools(), ["shell"]);
        assert!(rebuilt.rebuilt_for_late_mcp());
        assert_eq!(rebuilt.tools(), ["shell", "mcp-first"]);
        assert!(!frozen.rebuilt_for_late_mcp());
        assert_eq!(frozen.tools(), ["shell", "mcp-first"]);
        assert_eq!(tools.rebuild_count(), 1);
    }

    #[test]
    fn empty_dynamic_context_adds_no_message() {
        let mut cache = PromptCache::<&str>::new("stable");
        let history = [message(Role::User, "question")];
        let turn = cache
            .prepare_turn(
                &history,
                DynamicContext::default(),
                &[],
                McpToolStatus::Ready,
            )
            .unwrap();
        assert_eq!(turn.messages(), history);
        assert!(turn.developer_context().is_empty());
    }

    #[test]
    fn runtime_instructions_are_last_and_can_close_tool_authority_for_one_request() {
        let mut cache = PromptCache::new("stable");
        let history = [message(Role::User, "question")];
        let turn = cache
            .prepare_turn(
                &history,
                DynamicContext::new("clock=10:00")
                    .with_memory("remember this")
                    .with_runtime_instruction("Respond without tools."),
                &["shell"],
                McpToolStatus::Ready,
            )
            .unwrap()
            .without_tools();

        assert_eq!(
            turn.developer_context(),
            ["clock=10:00", "remember this", "Respond without tools."]
        );
        assert!(turn.tools().is_empty());
        let next = cache
            .prepare_turn(
                &history,
                DynamicContext::default(),
                &[],
                McpToolStatus::Ready,
            )
            .unwrap();
        assert_eq!(next.tools(), ["shell"]);
    }
}
