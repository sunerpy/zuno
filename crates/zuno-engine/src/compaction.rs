//! LLM-backed context compaction with tool-pair-safe transcript boundaries.
//!
//! Compaction changes the stable provider prefix, so a successful attempt is
//! persisted before the cache tracker and locked tool snapshot are reset. A
//! failed attempt writes an errored summary message and latches the session's
//! [`CompactionState`], preventing an outer turn loop from spending tokens by
//! entering the same failing compaction again.

use std::collections::HashMap;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tracing::Instrument as _;
use zuno_config::schema::{CompactionConfig, DEFAULT_COMPACTION_THRESHOLD_PERCENT};
use zuno_db::Connection;
use zuno_db::message::{MessageRecord, MessageStore, PartRecord, now_millis};
use zuno_error::{DbError, Recovery};
use zuno_llm::cache::{CacheTracker, LockedTools};
use zuno_llm::event::{Message, RequestContentBlock, Role, StreamEvent};
use zuno_llm::registry::{CompletionRequest, Provider};
use zuno_observability::span;

use crate::retry::{RecoveryBudget, RecoveryBudgets};

/// Default context headroom used when the configuration does not override it.
pub const DEFAULT_RESERVED_TOKENS: u64 = 20_000;
/// Default number of recent real user turns retained verbatim.
pub const DEFAULT_TAIL_TURNS: u32 = 2;
/// Lower bound for the derived verbatim-tail budget.
pub const MIN_PRESERVE_RECENT_TOKENS: u64 = 2_000;
/// Upper bound for the derived verbatim-tail budget.
pub const MAX_PRESERVE_RECENT_TOKENS: u64 = 8_000;
/// Maximum tool-result characters included in the summarizer request.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;

/// Compatibility-stable shape required from the compaction model.
pub const SUMMARY_TEMPLATE: &str = r#"Output exactly the Markdown structure shown inside <template> and keep the section order unchanged. Do not include the <template> tags in your response.
<template>
## Objective
- [one or two brief sentences describing what the user is trying to accomplish]

## Important Details
- [constraints/preferences, decisions and why, important facts/assumptions, exact context needed to continue, or "(none)"]

## Work State
### Completed
- [finished work, verified facts, or changes made; otherwise "(none)"]

### Active
- [current work, partial changes, or investigation state; otherwise "(none)"]

### Blocked
- [blockers, failing commands, or unknowns; otherwise "(none)"]

## Next Move
1. [immediate concrete action, or "(none)"]
2. [next action if known, or "(none)"]

## Relevant Files
- [file or directory path: why it matters, or "(none)"]
</template>

Rules:
- Keep every section, even when empty.
- Use terse bullets, not prose paragraphs.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers when known.
- Do not mention the summary process or that context was compacted."#;

/// Model limits used to resolve the configured trigger thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenWindow {
    /// Total model context window.
    pub context: u64,
    /// Maximum tokens reserved for model output.
    pub max_output: u64,
}

/// Why compaction is being considered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// Proactive compaction after measured usage crosses the configured window.
    Threshold {
        used_tokens: u64,
    },
    /// Reactive compaction after a typed provider context-limit failure.
    ContextLimit {
        used_tokens: Option<u64>,
        limit_tokens: Option<u64>,
    },
    Manual,
}

/// Fully resolved compaction settings for one model window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPolicy {
    pub auto: bool,
    pub threshold_percent: u8,
    pub threshold_tokens: u64,
    pub prune: bool,
    pub tail_turns: u32,
    pub preserve_recent_tokens: u64,
    pub reserved: u64,
    pub usable_tokens: u64,
    context_enabled: bool,
}

impl CompactionPolicy {
    /// Apply defaults and derive the proactive-compaction threshold.
    #[must_use]
    pub fn resolve(config: &CompactionConfig, window: TokenWindow) -> Self {
        let reserved = config
            .reserved
            .map(u64::from)
            .unwrap_or_else(|| DEFAULT_RESERVED_TOKENS.min(window.max_output));
        let usable_tokens = window
            .context
            .saturating_sub(window.max_output.max(reserved));
        let threshold_percent = config
            .threshold_percent
            .map_or(DEFAULT_COMPACTION_THRESHOLD_PERCENT, |percent| {
                percent.get()
            });
        let threshold_tokens =
            u64::try_from(u128::from(usable_tokens) * u128::from(threshold_percent) / 100)
                .unwrap_or(u64::MAX);
        let preserve_recent_tokens =
            config
                .preserve_recent_tokens
                .map(u64::from)
                .unwrap_or_else(|| {
                    (usable_tokens / 4)
                        .clamp(MIN_PRESERVE_RECENT_TOKENS, MAX_PRESERVE_RECENT_TOKENS)
                });
        Self {
            auto: config.auto.unwrap_or(true),
            threshold_percent,
            threshold_tokens,
            prune: config.prune.unwrap_or(false),
            tail_turns: config.tail_turns.unwrap_or(DEFAULT_TAIL_TURNS),
            preserve_recent_tokens,
            reserved,
            usable_tokens,
            context_enabled: window.context > 0,
        }
    }

    /// Context-limit failures always compact; proactive checks also require
    /// `auto` and a usable model context.
    #[must_use]
    pub const fn should_compact(self, trigger: CompactionTrigger) -> bool {
        match trigger {
            CompactionTrigger::Threshold { used_tokens } => {
                self.context_enabled && self.auto && used_tokens >= self.threshold_tokens
            }
            CompactionTrigger::ContextLimit { .. } => true,
            CompactionTrigger::Manual => true,
        }
    }
}

/// One identified transcript message plus selection metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub id: String,
    pub message: Message,
    pub estimated_tokens: u32,
    pub synthetic: bool,
    pub preserve_initial: bool,
}

impl TranscriptEntry {
    /// Build a real transcript entry. Leading system messages are initial
    /// context by default and are never summarized away.
    #[must_use]
    pub fn new(id: impl Into<String>, message: Message, estimated_tokens: u32) -> Self {
        let preserve_initial = message.role == Role::System;
        Self {
            id: id.into(),
            message,
            estimated_tokens,
            synthetic: false,
            preserve_initial,
        }
    }

    /// Mark an internal user message so it does not count as a real user turn.
    #[must_use]
    pub const fn synthetic(mut self) -> Self {
        self.synthetic = true;
        self
    }

    /// Preserve non-system bootstrap context with the leading system prefix.
    #[must_use]
    pub const fn preserve_as_initial(mut self) -> Self {
        self.preserve_initial = true;
        self
    }

    fn is_real_user(&self) -> bool {
        self.message.role == Role::User && !self.synthetic
    }
}

/// The raw and tool-pair-adjusted transcript split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionBoundary {
    /// End of the initial context prefix retained separately from the summary.
    pub initial_context_end: usize,
    /// Token- and turn-selected split before tool-pair repair.
    pub raw_retained_from: usize,
    /// First recent entry retained after walking backward over tool pairs.
    pub retained_from: usize,
}

/// Select the recent verbatim tail and move its boundary backward whenever a
/// retained tool result would otherwise lose the matching assistant tool use.
///
/// Providers reject that orphaned shape: on OpenAI-compatible APIs a `tool`
/// message must immediately follow an assistant message carrying the matching
/// `tool_calls`, otherwise the request receives a 400 response.
#[must_use]
pub fn select_boundary(
    entries: &[TranscriptEntry],
    tail_turns: u32,
    preserve_recent_tokens: u32,
) -> Option<CompactionBoundary> {
    let initial_context_end = entries
        .iter()
        .take_while(|entry| entry.preserve_initial)
        .count();
    if initial_context_end >= entries.len() {
        return None;
    }

    let earliest_tail_turn = if tail_turns == 0 {
        entries.len()
    } else {
        entries
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, entry)| entry.is_real_user())
            .nth(tail_turns.saturating_sub(1) as usize)
            .map_or(initial_context_end, |(index, _)| index)
    };

    let mut raw_retained_from = entries.len();
    let mut retained_tokens = 0_u64;
    let budget = u64::from(preserve_recent_tokens);
    for index in (earliest_tail_turn..entries.len()).rev() {
        let next = retained_tokens.saturating_add(u64::from(entries[index].estimated_tokens));
        if next > budget {
            break;
        }
        retained_tokens = next;
        raw_retained_from = index;
    }
    raw_retained_from = raw_retained_from.max(earliest_tail_turn);

    let retained_from = walk_back_over_tool_pairs(entries, raw_retained_from, initial_context_end);
    (retained_from > initial_context_end).then_some(CompactionBoundary {
        initial_context_end,
        raw_retained_from,
        retained_from,
    })
}

fn walk_back_over_tool_pairs(
    entries: &[TranscriptEntry],
    raw_boundary: usize,
    floor: usize,
) -> usize {
    let mut tool_uses: HashMap<&str, usize> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        for block in &entry.message.content {
            if let RequestContentBlock::ToolUse { id, .. } = block {
                tool_uses.insert(id.as_str(), index);
            }
        }
    }

    let mut boundary = raw_boundary;
    loop {
        let mut adjusted = boundary;
        for entry in &entries[boundary..] {
            for block in &entry.message.content {
                if let RequestContentBlock::ToolResult { tool_use_id, .. } = block
                    && let Some(use_index) = tool_uses.get(tool_use_id.as_str())
                    && *use_index < adjusted
                    && *use_index >= floor
                {
                    adjusted = *use_index;
                }
            }
        }
        if adjusted == boundary {
            return boundary;
        }
        boundary = adjusted;
    }
}

/// Mutable hook output matching `experimental.session.compacting`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionPrompt {
    pub context: Vec<String>,
    pub prompt: Option<String>,
}

/// Input for the prompt customization hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionHookInput<'a> {
    pub session_id: &'a str,
}

/// Input for `experimental.compaction.autocontinue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoContinueHookInput<'a> {
    pub session_id: &'a str,
    pub agent: &'a str,
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub message: &'a Message,
    pub overflow: bool,
}

/// Named seam for Todos 57-62's plugin host.
#[async_trait]
pub trait CompactionHooks: Send + Sync {
    /// Add context or replace the default summary prompt before the model call.
    async fn compacting(
        &self,
        input: &CompactionHookInput<'_>,
        output: &mut CompactionPrompt,
    ) -> Result<(), String>;

    /// Decide whether a successful automatic compaction should synthesize a
    /// continuation turn. The default plugin value is `true`.
    async fn auto_continue(&self, input: &AutoContinueHookInput<'_>) -> Result<bool, String>;
}

/// Hook implementation for runtimes that have not loaded a plugin host.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopCompactionHooks;

#[async_trait]
impl CompactionHooks for NoopCompactionHooks {
    async fn compacting(
        &self,
        _input: &CompactionHookInput<'_>,
        _output: &mut CompactionPrompt,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn auto_continue(&self, _input: &AutoContinueHookInput<'_>) -> Result<bool, String> {
        Ok(true)
    }
}

/// Direct access to Todo 31's two cache mechanisms.
pub struct CompactionCache<'a, T> {
    tracker: &'a mut CacheTracker,
    locked_tools: &'a mut LockedTools<T>,
}

impl<'a, T> CompactionCache<'a, T>
where
    T: Clone + PartialEq,
{
    #[must_use]
    pub fn new(tracker: &'a mut CacheTracker, locked_tools: &'a mut LockedTools<T>) -> Self {
        Self {
            tracker,
            locked_tools,
        }
    }

    fn reset_after_compaction(&mut self) {
        self.tracker.reset();
        self.locked_tools.reset();
    }
}

/// Per-turn compaction recovery state.
#[derive(Debug, Default)]
pub struct CompactionState {
    budgets: RecoveryBudgets,
    failure: Option<CompactionFailure>,
}

impl CompactionState {
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        self.failure.is_some()
    }

    #[must_use]
    pub const fn context_limit_attempts(&self) -> u32 {
        self.budgets.attempts(RecoveryBudget::ContextLimit)
    }

    /// A completed ordinary turn starts a fresh context-recovery budget.
    pub fn reset_after_turn_success(&mut self) {
        self.budgets.reset_context_limit_retries();
        self.failure = None;
    }

    /// Permit another compaction only when the latched provider failure was retryable.
    pub fn reset_retryable_failure(&mut self) {
        if self
            .failure
            .as_ref()
            .is_some_and(|failure| failure.recovery.is_retry())
        {
            self.failure = None;
        }
    }

    fn mark_failed(&mut self, message: String, recovery: Recovery) {
        self.failure = Some(CompactionFailure { message, recovery });
    }
}

#[derive(Debug)]
struct CompactionFailure {
    message: String,
    recovery: Recovery,
}

/// Inputs for one compaction attempt.
#[derive(Debug, Clone)]
pub struct CompactionRequest<'a> {
    pub session_id: &'a str,
    pub attempt_id: &'a str,
    pub agent: &'a str,
    pub provider_id: &'a str,
    pub small_model_id: &'a str,
    pub entries: Vec<TranscriptEntry>,
    pub config: &'a CompactionConfig,
    pub window: TokenWindow,
    pub trigger: CompactionTrigger,
    pub previous_summary: Option<&'a str>,
    pub automatic: bool,
    pub overflow: bool,
}

impl<'a> CompactionRequest<'a> {
    #[allow(
        clippy::too_many_arguments,
        reason = "the constructor keeps every required compaction invariant explicit; optional state uses builders"
    )]
    #[must_use]
    pub const fn new(
        session_id: &'a str,
        attempt_id: &'a str,
        agent: &'a str,
        provider_id: &'a str,
        small_model_id: &'a str,
        entries: Vec<TranscriptEntry>,
        config: &'a CompactionConfig,
        window: TokenWindow,
        trigger: CompactionTrigger,
    ) -> Self {
        Self {
            session_id,
            attempt_id,
            agent,
            provider_id,
            small_model_id,
            entries,
            config,
            window,
            trigger,
            previous_summary: None,
            automatic: true,
            overflow: matches!(trigger, CompactionTrigger::ContextLimit { .. }),
        }
    }

    #[must_use]
    pub const fn with_previous_summary(mut self, previous_summary: &'a str) -> Self {
        self.previous_summary = Some(previous_summary);
        self
    }

    #[must_use]
    pub const fn manual(mut self) -> Self {
        self.automatic = false;
        self
    }
}

/// Successful compacted request history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactedTranscript {
    pub summary: String,
    pub messages: Vec<Message>,
    pub boundary: CompactionBoundary,
    pub marker_part_id: String,
    pub auto_continue: bool,
}

/// Terminal reason for a compaction that cannot continue the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStopReason {
    AlreadyFailed,
    BudgetExhausted,
    NoCompactableHistory,
    Hook,
    Provider,
    EmptySummary,
}

/// Decision returned to the turn owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionOutcome {
    NotNeeded,
    Compacted(CompactedTranscript),
    Stopped {
        reason: CompactionStopReason,
        message: String,
        recovery: Recovery,
    },
}

/// Persistence failure while recording a marker, summary, or failure.
#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error(transparent)]
    Database(#[from] DbError),
}

/// Run one bounded, LLM-backed compaction attempt.
///
/// `connection` is `&mut` although nothing here needs a transaction: an attempt
/// interleaves database writes with a provider stream, so a shared `&Connection` held
/// across those awaits would make the whole future non-`Send` and unspawnable — and
/// the interactive surface drives its turns from a spawned task. Exclusive is also the
/// honest signature for something that writes.
pub async fn run_compaction<T, H>(
    connection: &mut Connection,
    provider: &dyn Provider,
    hooks: &H,
    state: &mut CompactionState,
    cache: &mut CompactionCache<'_, T>,
    request: CompactionRequest<'_>,
) -> Result<CompactionOutcome, CompactionError>
where
    T: Clone + PartialEq,
    H: CompactionHooks + ?Sized,
{
    if let Some(failure) = &state.failure {
        return Ok(CompactionOutcome::Stopped {
            reason: CompactionStopReason::AlreadyFailed,
            message: failure.message.clone(),
            recovery: failure.recovery,
        });
    }

    let policy = CompactionPolicy::resolve(request.config, request.window);
    if !policy.should_compact(request.trigger) {
        return Ok(CompactionOutcome::NotNeeded);
    }
    let Some(boundary) = select_boundary(
        &request.entries,
        policy.tail_turns,
        u32::try_from(policy.preserve_recent_tokens).unwrap_or(u32::MAX),
    ) else {
        let message = "session has no compactable history before the preserved tail".to_owned();
        state.mark_failed(message.clone(), Recovery::Fail);
        return Ok(CompactionOutcome::Stopped {
            reason: CompactionStopReason::NoCompactableHistory,
            message,
            recovery: Recovery::Fail,
        });
    };

    if matches!(request.trigger, CompactionTrigger::ContextLimit { .. })
        && let Err(error) = state.budgets.record_context_limit_retry()
    {
        let message = error.to_string();
        let mut summary_message = persist_compaction_shell(connection, &request, boundary)?;
        persist_failure(connection, &mut summary_message, &message)?;
        state.mark_failed(message.clone(), Recovery::Fail);
        return Ok(CompactionOutcome::Stopped {
            reason: CompactionStopReason::BudgetExhausted,
            message,
            recovery: Recovery::Fail,
        });
    }

    let mut summary_message = persist_compaction_shell(connection, &request, boundary)?;
    let marker_part_id = compaction_part_id(request.attempt_id);
    let mut prompt = CompactionPrompt::default();
    if let Err(message) = hooks
        .compacting(
            &CompactionHookInput {
                session_id: request.session_id,
            },
            &mut prompt,
        )
        .await
    {
        persist_failure(connection, &mut summary_message, &message)?;
        state.mark_failed(message.clone(), Recovery::Fail);
        return Ok(CompactionOutcome::Stopped {
            reason: CompactionStopReason::Hook,
            message,
            recovery: Recovery::Fail,
        });
    }

    let summary_prompt = prompt.prompt.unwrap_or_else(|| {
        build_summary_prompt(request.previous_summary, prompt.context.as_slice())
    });
    let auto_continue_message = request
        .entries
        .iter()
        .rev()
        .find(|entry| entry.message.role == Role::User && !entry.synthetic)
        .map(|entry| entry.message.clone())
        .unwrap_or_else(|| Message::new(Role::User, ""));
    let mut entries = request.entries;
    let retained = entries.split_off(boundary.retained_from);
    let summarized = entries.split_off(boundary.initial_context_end);
    let initial = entries;
    let mut model_messages = summarized
        .into_iter()
        .map(|entry| summary_safe_message_owned(entry.message))
        .collect::<Vec<_>>();
    model_messages.push(Message::new(Role::User, summary_prompt));
    let request_span = span::provider_request_for_session(
        request.session_id,
        request.provider_id,
        request.small_model_id,
        1,
        true,
        "compaction",
    );
    let operation_span = request_span.clone();
    let (chunks, provider_failure) = async move {
        let mut stream = provider.stream(CompletionRequest::new(
            request.small_model_id,
            model_messages,
        ));
        let mut chunks = Vec::new();
        let mut provider_failure = None;
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(text)) => chunks.push(text),
                Ok(StreamEvent::Error {
                    message,
                    retry_after,
                }) => {
                    provider_failure = Some((message, Recovery::Retry { after: retry_after }));
                    break;
                }
                Err(error) => {
                    let recovery = match error.recovery() {
                        Recovery::Compact => Recovery::Fail,
                        recovery => recovery,
                    };
                    provider_failure = Some((error.to_string(), recovery));
                    break;
                }
                Ok(_) => {}
            }
        }
        (chunks, provider_failure)
    }
    .instrument(operation_span)
    .await;
    let (outcome, error_kind) = if provider_failure.is_some() {
        ("error", Some("provider"))
    } else {
        ("completed", None)
    };
    span::record_provider_outcome(&request_span, outcome, error_kind, None);
    request_span.in_scope(|| {
        tracing::debug!(
            target: "zuno_engine::provider",
            event = "provider.request.finished",
            operation = "compaction",
            outcome,
            "compaction provider request finished"
        );
    });

    if let Some((message, recovery)) = provider_failure {
        persist_failure(connection, &mut summary_message, &message)?;
        state.mark_failed(message.clone(), recovery);
        return Ok(CompactionOutcome::Stopped {
            reason: CompactionStopReason::Provider,
            message,
            recovery,
        });
    }

    let summary = chunks.concat();
    if summary.trim().is_empty() {
        let message = "compaction model returned an empty summary".to_owned();
        persist_failure(connection, &mut summary_message, &message)?;
        state.mark_failed(message.clone(), Recovery::Fail);
        return Ok(CompactionOutcome::Stopped {
            reason: CompactionStopReason::EmptySummary,
            message,
            recovery: Recovery::Fail,
        });
    }

    persist_summary(
        connection,
        &mut summary_message,
        &summary,
        request.attempt_id,
    )?;
    cache.reset_after_compaction();

    let auto_continue = if request.automatic {
        match hooks
            .auto_continue(&AutoContinueHookInput {
                session_id: request.session_id,
                agent: request.agent,
                provider_id: request.provider_id,
                model_id: request.small_model_id,
                message: &auto_continue_message,
                overflow: request.overflow,
            })
            .await
        {
            Ok(enabled) => enabled,
            Err(message) => {
                persist_failure(connection, &mut summary_message, &message)?;
                state.mark_failed(message.clone(), Recovery::Fail);
                return Ok(CompactionOutcome::Stopped {
                    reason: CompactionStopReason::Hook,
                    message,
                    recovery: Recovery::Fail,
                });
            }
        }
    } else {
        false
    };

    let mut messages = initial
        .into_iter()
        .map(|entry| entry.message)
        .collect::<Vec<_>>();
    messages.push(Message::new(Role::Assistant, summary.clone()));
    messages.extend(retained.into_iter().map(|entry| entry.message));

    Ok(CompactionOutcome::Compacted(CompactedTranscript {
        summary,
        messages,
        boundary,
        marker_part_id,
        auto_continue,
    }))
}

/// Build the user instruction sent after the selected history.
#[must_use]
pub fn build_summary_prompt(previous_summary: Option<&str>, context: &[String]) -> String {
    let anchor = previous_summary.map_or_else(
        || "Create a new anchored summary from the conversation history.".to_owned(),
        |summary| {
            format!(
                "Update the anchored summary below using the conversation history above.\n\
                 Preserve still-true details, remove stale details, and merge in the new facts.\n\
                 <previous-summary>\n{summary}\n</previous-summary>"
            )
        },
    );
    std::iter::once(anchor.as_str())
        .chain(std::iter::once(SUMMARY_TEMPLATE))
        .chain(context.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn summary_safe_message_owned(message: Message) -> Message {
    Message::from_content(
        message.role,
        message
            .content
            .into_iter()
            .map(|block| match block {
                RequestContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => RequestContentBlock::ToolResult {
                    tool_use_id,
                    content: truncate_tool_output_owned(content),
                    is_error,
                },
                RequestContentBlock::Image { media_type, .. } => RequestContentBlock::Text {
                    text: format!("[Attached {media_type}]"),
                },
                RequestContentBlock::Text { text } => RequestContentBlock::Text { text },
                RequestContentBlock::SignedThinking {
                    thinking,
                    signature,
                } => RequestContentBlock::SignedThinking {
                    thinking,
                    signature,
                },
                RequestContentBlock::ProviderEncryptedReasoning {
                    id,
                    summary,
                    encrypted_content,
                    status,
                } => RequestContentBlock::ProviderEncryptedReasoning {
                    id,
                    summary,
                    encrypted_content,
                    status,
                },
                RequestContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature,
                } => RequestContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature,
                },
            })
            .collect(),
    )
}

fn truncate_tool_output_owned(content: String) -> String {
    if content.chars().count() <= TOOL_OUTPUT_MAX_CHARS {
        return content;
    }
    let mut truncated = content
        .chars()
        .take(TOOL_OUTPUT_MAX_CHARS)
        .collect::<String>();
    truncated.push_str("\n[truncated]");
    truncated
}

fn persist_compaction_shell(
    connection: &Connection,
    request: &CompactionRequest<'_>,
    boundary: CompactionBoundary,
) -> Result<MessageRecord, DbError> {
    let created = now_millis();
    let marker_id = compaction_message_id(request.attempt_id);
    let marker = MessageRecord::from_json(json!({
        "id": marker_id,
        "sessionID": request.session_id,
        "role": "user",
        "time": { "created": created },
        "agent": request.agent,
        "model": {
            "providerID": request.provider_id,
            "modelID": request.small_model_id,
        },
    }))?;
    let mut marker_payload = json!({
        "id": compaction_part_id(request.attempt_id),
        "sessionID": request.session_id,
        "messageID": marker.id,
        "type": "compaction",
        "auto": request.automatic,
        "overflow": request.overflow,
    });
    marker_payload["tail_start_id"] =
        Value::String(request.entries[boundary.retained_from].id.clone());
    let marker_part = PartRecord::from_json(marker_payload, created)?;

    let summary = MessageRecord::from_json(json!({
        "id": summary_message_id(request.attempt_id),
        "sessionID": request.session_id,
        "role": "assistant",
        "parentID": marker.id,
        "time": { "created": created },
        "modelID": request.small_model_id,
        "providerID": request.provider_id,
        "mode": "compaction",
        "agent": "compaction",
        "summary": true,
        "cost": 0.0,
        "tokens": {
            "input": 0,
            "output": 0,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 },
        },
    }))?;
    let store = MessageStore::new(connection);
    store.put_message_at(&marker, created)?;
    store.put_part_at(&marker_part, created)?;
    store.put_message_at(&summary, created)?;
    Ok(summary)
}

fn persist_summary(
    connection: &Connection,
    summary_message: &mut MessageRecord,
    summary: &str,
    attempt_id: &str,
) -> Result<(), DbError> {
    let completed = now_millis();
    summary_message
        .data
        .insert("finish".to_owned(), Value::String("stop".to_owned()));
    if let Some(time) = summary_message
        .data
        .get_mut("time")
        .and_then(Value::as_object_mut)
    {
        time.insert("completed".to_owned(), Value::from(completed));
    }
    let text = PartRecord::from_json(
        json!({
            "id": summary_part_id(attempt_id),
            "sessionID": summary_message.session_id,
            "messageID": summary_message.id,
            "type": "text",
            "text": summary,
            "time": { "start": summary_message.time_created, "end": completed },
        }),
        summary_message.time_created,
    )?;
    let store = MessageStore::new(connection);
    store.put_message_at(summary_message, completed)?;
    store.put_part_at(&text, completed)
}

fn persist_failure(
    connection: &Connection,
    summary_message: &mut MessageRecord,
    message: &str,
) -> Result<(), DbError> {
    let completed = now_millis();
    summary_message
        .data
        .insert("finish".to_owned(), Value::String("error".to_owned()));
    summary_message.data.insert(
        "error".to_owned(),
        json!({
            "name": "CompactionError",
            "data": {
                "message": message,
                "isRetryable": false,
            },
        }),
    );
    if let Some(time) = summary_message
        .data
        .get_mut("time")
        .and_then(Value::as_object_mut)
    {
        time.insert("completed".to_owned(), Value::from(completed));
    }
    MessageStore::new(connection).put_message_at(summary_message, completed)
}

fn compaction_message_id(attempt_id: &str) -> String {
    format!("msg_{attempt_id}_compaction")
}

fn summary_message_id(attempt_id: &str) -> String {
    format!("msg_{attempt_id}_summary")
}

fn compaction_part_id(attempt_id: &str) -> String {
    format!("prt_{attempt_id}_compaction")
}

fn summary_part_id(attempt_id: &str) -> String {
    format!("prt_{attempt_id}_summary")
}
