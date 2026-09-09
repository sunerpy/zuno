//! LLM-backed context compaction with tool-pair-safe transcript boundaries.
//!
//! Compaction changes the stable provider prefix, so a successful attempt is
//! persisted before the cache tracker and locked tool snapshot are reset. A
//! failed attempt writes an errored summary message and latches the session's
//! [`CompactionState`], preventing an outer turn loop from spending tokens by
//! entering the same failing compaction again.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::Instrument as _;
use zuno_config::schema::{CompactionConfig, DEFAULT_COMPACTION_THRESHOLD_PERCENT};
use zuno_db::Connection;
use zuno_db::message::{MessageRecord, MessageStore, PartRecord, now_millis};
use zuno_error::{DbError, Recovery};
use zuno_llm::cache::{CacheTracker, LockedTools};
use zuno_llm::catalog::resolved::ModelCost;
use zuno_llm::event::{Message, RequestContentBlock, Role};
use zuno_llm::registry::{ApiSurface, CompletionRequest, Provider, ProviderRequestContext};
use zuno_observability::span;

use crate::retry::{RecoveryBudget, RecoveryBudgets};

pub(crate) mod checkpoint;
mod response;
mod trace;

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

/// Task-neutral checkpoint instructions sent after the selected history.
pub const SUMMARY_TEMPLATE: &str = include_str!("compaction/summary.md");

/// Host-owned guidance for continuing the admitted task after context recovery.
pub const CONTINUATION_PROMPT: &str = include_str!("compaction/continuation.md");

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

    /// The threshold a running multi-step turn should enforce before its next request.
    ///
    /// The prelude applies the same threshold before a turn starts. Returning it here
    /// lets the turn loop yield after a durable tool step instead of growing all the way
    /// to the provider's hard context limit before the next prelude can run.
    #[must_use]
    pub const fn proactive_threshold(self) -> Option<u64> {
        if self.context_enabled && self.auto {
            Some(self.threshold_tokens)
        } else {
            None
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
    tool_uses: Vec<String>,
    tool_results: Vec<String>,
}

impl TranscriptEntry {
    /// Build a real transcript entry. Leading system messages are initial
    /// context by default and are never summarized away.
    #[must_use]
    pub fn new(id: impl Into<String>, message: Message, estimated_tokens: u32) -> Self {
        let preserve_initial = message.role == Role::System;
        let synthetic = message.role == Role::Tool;
        let mut tool_uses = Vec::new();
        let mut tool_results = Vec::new();
        for block in &message.content {
            match block {
                RequestContentBlock::ToolUse { id, .. } => tool_uses.push(id.clone()),
                RequestContentBlock::ToolResult { tool_use_id, .. } => {
                    tool_results.push(tool_use_id.clone());
                }
                _ => {}
            }
        }
        Self {
            id: id.into(),
            message,
            estimated_tokens,
            synthetic,
            preserve_initial,
            tool_uses,
            tool_results,
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

    /// Reduce model-specific content after capturing its selection provenance.
    pub(crate) fn summary_safe(mut self) -> Self {
        self.message = summary_safe_message_owned(self.message);
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
///
/// The returned boundary always addresses an existing entry: the retained tail is
/// what the durable compaction marker names by message id, so a tail that starts
/// past the end of the transcript is not a representable answer. A zero
/// `tail_turns`, a zero `preserve_recent_tokens`, or one newest entry larger than
/// the whole tail budget therefore still keeps that newest entry rather than
/// selecting an empty tail.
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
    let newest = entries.len() - 1;

    let earliest_tail_turn = if tail_turns == 0 {
        newest
    } else {
        entries
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, entry)| entry.is_real_user())
            .nth(tail_turns.saturating_sub(1) as usize)
            .map_or(initial_context_end, |(index, _)| index)
    };

    let mut raw_retained_from = newest;
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
    let mut pairs = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        for id in &entry.tool_uses {
            tool_uses.insert(id.as_str(), index);
        }
        for id in &entry.tool_results {
            if let Some(use_index) = tool_uses.get(id.as_str()) {
                pairs.push((*use_index, index));
            }
        }
    }

    let mut boundary = raw_boundary;
    loop {
        let mut adjusted = boundary;
        for &(use_index, result_index) in &pairs {
            if result_index >= boundary && use_index < adjusted && use_index >= floor {
                adjusted = use_index;
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
    pub interrupt: Option<&'a crate::interrupt::InterruptSignal>,
    pub surface: ApiSurface,
    pub model_cost: Option<&'a ModelCost>,
    /// Dedicated summarizer instructions, separate from the retained main context.
    pub system_prompt: Option<&'a str>,
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
            interrupt: None,
            surface: ApiSurface::Default,
            model_cost: None,
            system_prompt: None,
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

    #[must_use]
    pub const fn with_interrupt(
        mut self,
        interrupt: Option<&'a crate::interrupt::InterruptSignal>,
    ) -> Self {
        self.interrupt = interrupt;
        self
    }

    #[must_use]
    pub const fn with_surface(mut self, surface: ApiSurface) -> Self {
        self.surface = surface;
        self
    }

    #[must_use]
    pub const fn with_model_cost(mut self, cost: &'a ModelCost) -> Self {
        self.model_cost = Some(cost);
        self
    }

    #[must_use]
    pub const fn with_system_prompt(mut self, prompt: &'a str) -> Self {
        self.system_prompt = Some(prompt);
        self
    }
}

/// A successful checkpoint and its selected in-memory transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactedTranscript {
    pub summary: String,
    /// Logical transcript for the supplied entries. Hosts reconstruct live
    /// requests from durable history and the current agent's prompt instead.
    pub messages: Vec<Message>,
    pub boundary: CompactionBoundary,
    pub marker_part_id: String,
    /// Whether the turn owner should synthesize a continuation turn.
    pub auto_continue: bool,
    /// The auto-continue hook's failure, when it could not vote.
    ///
    /// The summary is durable by the time the hook runs, so its failure costs the
    /// plugin its vote — `auto_continue` is `false` — and nothing else.
    pub auto_continue_hook_failure: Option<String>,
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
    OutputLimit,
    Interrupted,
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
/// Short transactions commit the attempt, prompt receipt and completed summary.
/// None spans a provider await. Exclusive access also keeps the future `Send`
/// while it interleaves database writes with the provider stream.
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
        persist_failure(connection, &mut summary_message, &message, Recovery::Fail)?;
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
        persist_failure(connection, &mut summary_message, &message, Recovery::Fail)?;
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
    let mut model_messages = Vec::new();
    if let Some(prompt) = request
        .system_prompt
        .filter(|prompt| !prompt.trim().is_empty())
    {
        model_messages.push(Message::new(Role::System, prompt));
    }
    model_messages.extend(
        summarized
            .into_iter()
            .map(|entry| summary_safe_message_owned(entry.message)),
    );
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
    let completion = CompletionRequest::new(request.small_model_id, model_messages)
        .on_surface(request.surface)
        .with_request_context(ProviderRequestContext::Compaction);
    trace::record_request(
        connection,
        request.session_id,
        request.attempt_id,
        request.config,
        &completion,
        &mut summary_message,
    )?;
    let response = response::receive(provider, completion, request.config, request.interrupt)
        .instrument(operation_span)
        .await;
    if let Some(usage) = response.usage {
        summary_message
            .data
            .insert("tokens".to_owned(), usage.tokens());
        if let Some(model_cost) = request.model_cost {
            summary_message
                .data
                .insert("cost".to_owned(), Value::from(usage.cost(model_cost)));
        }
    } else {
        summary_message.data.remove("tokens");
    }
    let (outcome, error_kind) = if response.failure.is_some() {
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

    if let Some(response::SummaryFailure {
        reason,
        message,
        recovery,
    }) = response.failure
    {
        persist_failure(connection, &mut summary_message, &message, recovery)?;
        if reason != CompactionStopReason::Interrupted {
            state.mark_failed(message.clone(), recovery);
        }
        return Ok(CompactionOutcome::Stopped {
            reason,
            message,
            recovery,
        });
    }

    let summary = response.text;
    if summary.trim().is_empty() {
        let message = "compaction model returned an empty summary".to_owned();
        persist_failure(connection, &mut summary_message, &message, Recovery::Fail)?;
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

    let (auto_continue, auto_continue_hook_failure) = if request.automatic {
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
            Ok(enabled) => (enabled, None),
            Err(message) => {
                // The summary is already durable and the cache already reset. A hook
                // that cannot vote does not un-compact the session: rewriting the
                // persisted summary as a failure would discard work the model can
                // already resume from, and marking the state failed would refuse
                // every later compaction as `AlreadyFailed`. It loses only its vote,
                // and a vote nobody cast grants no synthesized continuation.
                tracing::warn!(
                    target: "zuno_engine::compaction",
                    session_id = request.session_id,
                    error = %message,
                    "auto-continue hook failed after a durable compaction; the summary \
                     stands and no continuation turn is synthesized"
                );
                (false, Some(message))
            }
        }
    } else {
        (false, None)
    };

    let mut messages = initial
        .into_iter()
        .map(|entry| entry.message)
        .collect::<Vec<_>>();
    messages.extend(retained.into_iter().map(|entry| entry.message));
    messages.push(Message::new(Role::Assistant, summary.clone()));

    Ok(CompactionOutcome::Compacted(CompactedTranscript {
        summary,
        messages,
        boundary,
        marker_part_id,
        auto_continue,
        auto_continue_hook_failure,
    }))
}

/// Build the user instruction sent after the selected history.
#[must_use]
pub fn build_summary_prompt(previous_summary: Option<&str>, context: &[String]) -> String {
    let anchor = previous_summary.map_or_else(
        || "Create a new checkpoint from the conversation history above.".to_owned(),
        |summary| {
            format!(
                "The previous checkpoint below is historical context. Update it using \
                 the conversation above. Carry forward still-relevant work and constraints; \
                 newer explicit corrections take precedence.\n\
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

/// Reshape one projected message into something the summarizer can be sent.
///
/// The summarizer runs on the session's small model, which is usually not the model
/// that produced the transcript. That is why a sealed reasoning envelope is dropped
/// here instead of passed through: the envelope is bound to the model that minted it,
/// so echoing it to another model fails the whole summary request and latches this
/// session's compaction failure. Nothing model-visible is lost — the envelope is
/// provider bookkeeping. Bounded plaintext reasoning remains historical context,
/// without the signature that could accidentally bind it to a different model.
pub(crate) fn summary_safe_message_owned(message: Message) -> Message {
    let role = if message.role == Role::Tool {
        // A tool-free compaction request cannot carry native tool-result blocks once
        // their declarations are intentionally absent. User is the provider-neutral
        // role for the environment observation the tool message represented.
        Role::User
    } else {
        message.role
    };
    Message::from_content(
        role,
        message
            .content
            .into_iter()
            .filter_map(|block| match block {
                RequestContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some(RequestContentBlock::Text {
                    text: format!(
                        "Historical tool result for compaction:\n{}",
                        json!({
                            "kind": "historical_tool_result",
                            "callID": tool_use_id,
                            "result": truncate_tool_output_owned(content),
                            "isError": is_error,
                        })
                    ),
                }),
                RequestContentBlock::Image {
                    filename,
                    media_type,
                    ..
                } => Some(RequestContentBlock::Text {
                    text: filename.map_or_else(
                        || format!("[Attached {media_type}]"),
                        |filename| format!("[Attached {filename} ({media_type})]"),
                    ),
                }),
                RequestContentBlock::ImageAttachment { reference } => {
                    Some(RequestContentBlock::Text {
                        text: reference.filename.map_or_else(
                            || format!("[Attached {}]", reference.media_type),
                            |filename| format!("[Attached {filename} ({})]", reference.media_type),
                        ),
                    })
                }
                RequestContentBlock::Text { text } => Some(RequestContentBlock::Text { text }),
                link @ RequestContentBlock::ResourceLink { .. } => Some(link),
                RequestContentBlock::SignedThinking { thinking, .. } => Some(RequestContentBlock::Text {
                    text: format!(
                        "Historical assistant reasoning (working notes, not verified results):\n{}",
                        truncate_tool_output_owned(thinking)
                    ),
                }),
                RequestContentBlock::ProviderEncryptedReasoning { .. } => None,
                RequestContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    raw_arguments,
                    ..
                } => {
                    let arguments = raw_arguments.map_or(input, Value::String);
                    let encoded = match &arguments {
                        Value::String(text) => text.clone(),
                        value => value.to_string(),
                    };
                    let truncated = encoded.chars().count() > TOOL_OUTPUT_MAX_CHARS;
                    let arguments = if truncated {
                        Value::String(truncate_tool_output_owned(encoded))
                    } else {
                        arguments
                    };
                    Some(RequestContentBlock::Text { text: format!(
                        "Historical tool call for compaction:\n{}",
                        json!({
                            "kind": "historical_tool_call",
                            "callID": id,
                            "tool": name,
                            "arguments": arguments,
                            "argumentsTruncated": truncated,
                        })
                    ) })
                },
            })
            .collect(),
    )
}

fn truncate_tool_output_owned(content: String) -> String {
    let length = content.chars().count();
    if length <= TOOL_OUTPUT_MAX_CHARS {
        return content;
    }
    let head_chars = TOOL_OUTPUT_MAX_CHARS / 2;
    let tail_chars = TOOL_OUTPUT_MAX_CHARS - head_chars;
    let head = content.chars().take(head_chars).collect::<String>();
    let tail_start = content
        .char_indices()
        .rev()
        .nth(tail_chars - 1)
        .map_or(content.len(), |(offset, _)| offset);
    format!(
        "{head}\n[{} characters omitted; full content remains in session history]\n{}",
        length - TOOL_OUTPUT_MAX_CHARS,
        &content[tail_start..]
    )
}

fn persist_compaction_shell(
    connection: &Connection,
    request: &CompactionRequest<'_>,
    boundary: CompactionBoundary,
) -> Result<MessageRecord, DbError> {
    let created = zuno_db::message::created_after(
        now_millis(),
        MessageStore::new(connection).latest_time_created(request.session_id)?,
    );
    let marker_id = compaction_message_id(request.attempt_id);
    let marker = MessageRecord::from_json(json!({
        "id": marker_id,
        "sessionID": request.session_id,
        "role": "user",
        "mode": "compaction",
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
    let transaction = connection
        .unchecked_transaction()
        .map_err(zuno_db::open::map_error)?;
    let store = MessageStore::new(&transaction);
    store.put_message_at(&marker, created)?;
    store.put_part_at(&marker_part, created)?;
    store.put_message_at(&summary, created)?;
    transaction.commit().map_err(zuno_db::open::map_error)?;
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
    let transaction = connection
        .unchecked_transaction()
        .map_err(zuno_db::open::map_error)?;
    let store = MessageStore::new(&transaction);
    store.put_message_at(summary_message, completed)?;
    store.put_part_at(&text, completed)?;
    transaction.commit().map_err(zuno_db::open::map_error)
}

fn persist_failure(
    connection: &Connection,
    summary_message: &mut MessageRecord,
    message: &str,
    recovery: Recovery,
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
                "isRetryable": recovery.is_retry(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_reasoning_loses_its_signature_and_keeps_bounded_working_notes() {
        let safe = summary_safe_message_owned(Message::from_content(
            Role::Assistant,
            vec![RequestContentBlock::SignedThinking {
                thinking: format!(
                    "initial hypothesis\n{}\nfinal uncertainty",
                    "x".repeat(10_000)
                ),
                signature: "signature-for-a-different-model".to_owned(),
            }],
        ));
        let RequestContentBlock::Text { text } = &safe.content[0] else {
            panic!("historical reasoning must be ordinary labelled text");
        };
        assert!(text.contains("working notes, not verified results"));
        assert!(text.contains("initial hypothesis"));
        assert!(text.ends_with("final uncertainty"));
        assert!(text.contains("characters omitted"));
        assert!(text.len() < TOOL_OUTPUT_MAX_CHARS + 200);
        assert!(
            !serde_json::to_string(&safe)
                .unwrap()
                .contains("signature-for-a-different-model")
        );
    }

    #[test]
    fn long_tool_evidence_keeps_both_ends_and_its_failure_metadata() {
        let safe = summary_safe_message_owned(Message::from_content(
            Role::Tool,
            vec![RequestContentBlock::ToolResult {
                tool_use_id: "call-uncertain".to_owned(),
                content: format!(
                    "The operation's outcome is uncertain\n{}\nInspect run R312 before retrying",
                    "中".repeat(10_000)
                ),
                is_error: Some(true),
            }],
        ));
        let RequestContentBlock::Text { text } = &safe.content[0] else {
            panic!("inert result")
        };
        let value: Value = serde_json::from_str(text.split_once('\n').unwrap().1).unwrap();
        assert_eq!(value["callID"], "call-uncertain");
        assert_eq!(value["isError"], true);
        let excerpt = value["result"].as_str().unwrap();
        assert!(excerpt.starts_with("The operation's outcome is uncertain"));
        assert!(excerpt.ends_with("Inspect run R312 before retrying"));
        assert!(excerpt.chars().count() < TOOL_OUTPUT_MAX_CHARS + 100);
    }

    #[test]
    fn summary_safe_images_keep_their_human_filename_without_their_bytes() {
        let message = Message::from_content(
            Role::User,
            vec![RequestContentBlock::Image {
                filename: Some("diagram.png".to_owned()),
                media_type: "image/png".to_owned(),
                data: "large-base64-payload".to_owned(),
            }],
        );

        let safe = summary_safe_message_owned(message);

        assert_eq!(
            safe.content,
            vec![RequestContentBlock::Text {
                text: "[Attached diagram.png (image/png)]".to_owned(),
            }]
        );
    }

    #[test]
    fn summary_safe_resource_links_keep_their_typed_metadata() {
        let link = RequestContentBlock::ResourceLink {
            name: "notes.md".to_owned(),
            uri: "file:///workspace/notes.md".to_owned(),
            title: Some("Design notes".to_owned()),
            description: Some("ACP design context".to_owned()),
            media_type: Some("text/markdown".to_owned()),
            size: Some(42),
        };
        let message = Message::from_content(Role::User, vec![link.clone()]);

        let safe = summary_safe_message_owned(message);

        assert_eq!(safe.content, vec![link]);
    }

    #[test]
    fn summary_safe_tool_history_is_inert_and_uses_provider_neutral_roles() {
        let call = summary_safe_message_owned(Message::from_content(
            Role::Assistant,
            vec![RequestContentBlock::ToolUse {
                id: "call-1".to_owned(),
                name: "penpot_execute_code".to_owned(),
                input: json!({"code": "return 1"}),
                raw_arguments: Some("{\"code\":\"return 1\"}".to_owned()),
                thought_signature: None,
            }],
        ));
        let result = summary_safe_message_owned(Message::from_content(
            Role::Tool,
            vec![RequestContentBlock::ToolResult {
                tool_use_id: "call-1".to_owned(),
                content: "created the board".to_owned(),
                is_error: Some(false),
            }],
        ));

        assert_eq!(call.role, Role::Assistant);
        assert_eq!(result.role, Role::User);
        for message in [&call, &result] {
            assert!(
                message
                    .content
                    .iter()
                    .all(|block| matches!(block, RequestContentBlock::Text { .. })),
                "tool-free compaction must receive no native tool protocol: {message:?}"
            );
        }
        assert!(matches!(
            &call.content[0],
            RequestContentBlock::Text { text }
                if text.contains("\"tool\":\"penpot_execute_code\"")
        ));
        assert!(matches!(
            &result.content[0],
            RequestContentBlock::Text { text }
                if text.contains("\"result\":\"created the board\"")
        ));
    }
}
