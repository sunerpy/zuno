//! The three internal agents, at the point in a turn where they actually run.
//!
//! `zuno_agent::builtin::INTERNAL_NAMES` has named `compaction`, `title` and `summary`
//! since todo 63, and todo 63's own doc comment predicted what dropping any of them
//! would cost: *"silently removes auto-compaction, session titles, or session
//! summaries, with nothing else in the roster providing them."* Declaring them did
//! not provide them either. Nothing invoked the roster entries, no title-generating
//! request existed anywhere in the workspace, and `compaction::select_boundary` was
//! reachable only from its own tests. This module is the invocation.
//!
//! # Why a prelude and not a step of the loop
//!
//! Upstream runs both from inside its turn loop — `title` forked at step 1
//! (`session/prompt.ts:1132-1138`) and the overflow check on the previous
//! assistant's measured usage each iteration (`:1161-1167`). Here they run *before*
//! [`crate::r#loop::run_turn`] is entered, for three reasons that are properties of
//! this port rather than preferences:
//!
//! 1. **The prompt cache.** `run_turn` builds one [`zuno_llm::cache::PromptCache`] per
//!    turn and its append-only tracker refuses a request whose stable prefix moved.
//!    Compaction moves it deliberately. Compacting before the loop means the tracker
//!    only ever sees the post-compaction prefix, so there is nothing to reset and no
//!    window in which a half-compacted prefix could be sent.
//! 2. **Determinism.** Upstream forks the title request, so whether it or the turn's
//!    first request reaches the provider first is a race. Sequencing it makes the
//!    request order a fact a test can assert, which is what the count in
//!    `zuno-testkit`'s frozen perf harness needs to mean something.
//! 3. **One entry point.** `run_turn`'s signature is unchanged, so every existing
//!    caller keeps working and the prelude is opt-in per surface.
//!
//! The cost is recorded rather than hidden: an *overflow discovered mid-turn* — a
//! step whose own usage crosses the window — is not compacted until the next turn.
//! Upstream re-checks after every step. Closing that would mean giving `run_turn` a
//! provider for the small model, the hooks and a [`crate::compaction::CompactionState`],
//! and the place it would go is the `if !accumulator.calls.is_empty()` continuation.

use serde_json::Value;
use zuno_db::message::{MessageRole, MessageStore, MessageWithParts, PartKind};
use zuno_db::{Connection, open, session};
use zuno_error::{DbError, Recovery};
use zuno_llm::cache::{CacheTracker, LockedTools};
use zuno_llm::event::{Message, Role, StreamEvent};
use zuno_llm::registry::{CompletionRequest, Provider};
use zuno_tool::ToolDefinition;

use crate::compaction::{
    CompactionCache, CompactionError, CompactionHooks, CompactionOutcome, CompactionPolicy,
    CompactionRequest, CompactionState, CompactionStopReason, CompactionTrigger, TokenWindow,
    TranscriptEntry, run_compaction, summary_safe_message_owned,
};
use crate::r#loop::{
    ResolvedModel, hydrate_retained_history, map_project_history_owned_with_ids, project_history,
    retained_history,
};
use futures::StreamExt;

/// Characters per token in upstream's estimator (`core/src/util/token.ts:3`).
///
/// Not a tokenizer, and deliberately the same wrong number upstream uses: the
/// estimate feeds a *threshold comparison* against a window measured with the same
/// rule, so agreeing with upstream matters more than being accurate.
const CHARS_PER_TOKEN: usize = 4;

/// Longest title accepted verbatim (`session/prompt.ts:249`).
const TITLE_MAX_CHARS: usize = 100;

/// Characters kept when a title is too long, before the ellipsis (`prompt.ts:249`).
const TITLE_TRUNCATED_CHARS: usize = 97;

/// The instruction that precedes the conversation in a title request.
///
/// Byte-for-byte upstream's, including the trailing newline
/// (`session/prompt.ts:236`), because the title agent's prompt was written against
/// exactly this framing.
pub const TITLE_INSTRUCTION: &str = "Generate a title for this conversation:\n";

/// One internal agent, resolved to a model this runtime can actually reach.
///
/// `prompt` is the upstream native's text, carried in from
/// `zuno_catalog::agent::builtin` by the composition root rather than read here — this
/// crate has no catalog dependency, and the prompt is data, not engine policy.
/// `model` is whatever the caller's model policy answered with; nothing in this
/// module names a model, and nothing in it may.
#[derive(Debug, Clone, PartialEq)]
pub struct InternalAgent {
    /// The roster name, one of `zuno_agent::builtin::INTERNAL_NAMES`.
    pub name: String,
    /// The system prompt the upstream native declares.
    pub prompt: String,
    /// The model and provider spec resolved for this agent.
    pub model: ResolvedModel,
}

/// All three internal agents, resolved together.
///
/// One struct rather than three arguments because they are resolved by one policy
/// pass and a surface that has any of them has all of them. A field left unread by
/// this crate is still resolved on the same path, which is the point: `summary`'s
/// model comes from the same precedence chain as `title`'s, so a preset that
/// redirects the internals cannot redirect two of them and miss the third.
#[derive(Debug, Clone, PartialEq)]
pub struct Internals {
    /// Names a session from its first exchange.
    pub title: InternalAgent,
    /// Rewrites a transcript that outgrew the context window.
    pub compaction: InternalAgent,
    /// Summarises what a session accomplished.
    pub summary: InternalAgent,
}

/// Everything the prelude needs that is not the session itself.
///
/// Borrowed rather than owned so a host can hand over its live registry and config
/// without cloning either, and so the prelude cannot outlive the turn it precedes.
pub struct PreludeContext<'a> {
    /// The database the session lives in.
    pub connection: &'a mut Connection,
    /// The provider for each internal agent's resolved model.
    pub providers: &'a dyn InternalProviders,
    /// The three resolved internal agents.
    pub internals: &'a Internals,
    /// The user's compaction settings, as configured.
    pub compaction: &'a zuno_config::schema::CompactionConfig,
    /// The window the session's own model declares.
    pub window: TokenWindow,
    /// Latched compaction failure state, so a failing attempt is tried once.
    pub state: &'a mut CompactionState,
    pub hooks: &'a dyn CompactionHooks,
}

/// How the prelude gets a provider for an internal agent's model.
///
/// A trait and not `&ProviderRegistry` because the two internal agents that make
/// requests may be resolved onto a model the *session's* provider does not serve,
/// and only the composition root knows which credential answers for which spec.
/// It also lets a test supply one recording provider without building a registry.
pub trait InternalProviders: Send + Sync {
    /// The provider for `agent`'s model, or why there is not one.
    ///
    /// # Errors
    ///
    /// A message naming the agent and what could not be resolved. Every caller in
    /// this module treats that as "skip this internal", never as a turn failure.
    fn provider_for(&self, agent: &InternalAgent) -> Result<std::sync::Arc<dyn Provider>, String>;
}

/// What the prelude did, for a caller that wants to report or assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreludeOutcome {
    /// The title written to the session, when one was generated.
    pub title: Option<String>,
    /// Whether the history was compacted before the turn.
    pub compacted: bool,
    /// Whether the caller should enter the ordinary turn after the prelude.
    ///
    /// A plugin may suppress the synthetic continuation produced by automatic
    /// compaction. This is `true` when no compaction ran and for the default hook.
    pub continue_turn: bool,
    /// Why an internal was skipped, in the order the internals ran.
    pub skipped: Vec<String>,
}

/// Run every internal that applies before a turn over `session_id`.
///
/// Never fails the turn. Both internals are best-effort by construction — upstream
/// forks the title with `Effect.ignore` (`session/prompt.ts:1133-1138`) and logs a
/// title write failure rather than aborting (`:251-253`) — and the reason is worth
/// stating: a session that cannot be named is a cosmetic loss, while refusing the
/// turn over it loses the user's work. Reasons land in
/// [`PreludeOutcome::skipped`] so the loss is visible rather than silent.
///
/// # Errors
///
/// Only a database failure while reading the session or its history, which is the
/// one condition under which the turn could not have run either.
pub async fn run_prelude(
    session_id: &str,
    context: &mut PreludeContext<'_>,
) -> Result<PreludeOutcome, DbError> {
    let mut outcome = PreludeOutcome {
        title: None,
        compacted: false,
        continue_turn: true,
        skipped: Vec::new(),
    };
    match generate_title(session_id, context).await {
        Ok(title) => outcome.title = title,
        Err(TitleSkipped::Database(error)) => return Err(error),
        Err(TitleSkipped::Reason(reason)) => outcome.skipped.push(format!("title: {reason}")),
    }
    match compact_if_overflowing(session_id, context).await {
        Ok(compaction) => {
            outcome.compacted = compaction.compacted;
            outcome.continue_turn = compaction.continue_turn;
        }
        Err(CompactionSkipped::Database(error)) => return Err(error),
        Err(CompactionSkipped::Reason(reason)) => {
            outcome.skipped.push(format!("compaction: {reason}"));
        }
        Err(CompactionSkipped::Stopped {
            reason, message, ..
        }) => {
            outcome
                .skipped
                .push(format!("compaction: {reason:?}: {message}"));
        }
    }
    Ok(outcome)
}

/// Why no title was generated.
#[derive(Debug)]
pub enum TitleSkipped {
    /// The session or its history could not be read.
    Database(DbError),
    /// A condition upstream also declines on, or the model declined.
    Reason(String),
}

/// Generate and persist a session title from its first exchange.
///
/// Returns [`None`] when the session already has one, mirroring upstream's guards in
/// order (`session/prompt.ts:199-206`): a child session inherits its parent's
/// naming, an already-named session is the user's to rename, and a session whose
/// history holds anything other than exactly one real user turn is past the point
/// where its opening exchange describes it.
///
/// The request is **tool-free and generated**, not derived. A title spliced together
/// from the prompt locally would read plausibly and would still leave the model
/// uncalled, which is precisely the shape this todo exists to remove.
///
/// # Errors
///
/// [`TitleSkipped::Database`] when the session or history cannot be read;
/// [`TitleSkipped::Reason`] for every declined or failed generation.
pub async fn generate_title(
    session_id: &str,
    context: &mut PreludeContext<'_>,
) -> Result<Option<String>, TitleSkipped> {
    let session = session::get(context.connection, session_id).map_err(TitleSkipped::Database)?;
    if session.parent_id.is_some() {
        return Ok(None);
    }
    if !session::is_default_title(&session.title) {
        return Ok(None);
    }
    let history = MessageStore::new(context.connection)
        .hydrate_session(session_id)
        .map_err(TitleSkipped::Database)?;
    let Some(opening) = opening_exchange(&history) else {
        return Ok(None);
    };

    let agent = &context.internals.title;
    let provider = context
        .providers
        .provider_for(agent)
        .map_err(TitleSkipped::Reason)?;
    let mut messages = vec![Message::new(Role::System, agent.prompt.clone())];
    messages.push(Message::new(Role::User, TITLE_INSTRUCTION));
    messages.extend(
        project_history("", opening)
            .into_iter()
            .skip(1)
            .map(|projected| projected.message),
    );

    let text = collect_text(provider.as_ref(), agent, messages)
        .await
        .map_err(TitleSkipped::Reason)?;
    let Some(title) = clean_title(&text) else {
        return Err(TitleSkipped::Reason(
            "the title model returned no usable line".to_owned(),
        ));
    };
    let transaction = context
        .connection
        .transaction()
        .map_err(|error| TitleSkipped::Database(open::map_error(error)))?;
    session::set_title(&transaction, session_id, &title).map_err(TitleSkipped::Database)?;
    transaction
        .commit()
        .map_err(|error| TitleSkipped::Database(open::map_error(error)))?;
    Ok(Some(title))
}

/// Why compaction could not happen although the history needed it.
///
/// A history that simply fits is **not** represented here — that is an unchanged
/// [`PreludeCompactionOutcome`]. The
/// distinction is what keeps the report honest: "your context did not need compacting"
/// is the ordinary case and belongs in no report, while "your context overflowed and I
/// could not compact it" is a loss the user has to be told about. Collapsing the two
/// would put a line on the screen for every ordinary turn, and a warning nobody can act
/// on is a warning everybody learns to ignore.
#[derive(Debug)]
pub enum CompactionSkipped {
    /// The history could not be read, or the attempt could not be recorded.
    Database(DbError),
    /// The history overflowed and the attempt could not be made or completed.
    Reason(String),
    /// A compaction attempt reached a typed terminal result.
    Stopped {
        reason: CompactionStopReason,
        message: String,
        recovery: Recovery,
    },
}

/// The prelude-facing result of checking and, when needed, compacting history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreludeCompactionOutcome {
    /// Whether compaction replaced the old history with a summary.
    pub compacted: bool,
    /// Whether the caller should immediately continue into the ordinary turn.
    pub continue_turn: bool,
}

impl PreludeCompactionOutcome {
    const NOT_NEEDED: Self = Self {
        compacted: false,
        continue_turn: true,
    };
}

/// Compact the session's history when it no longer fits the model's window.
///
/// The trigger is the *measured* usage of the newest finished, non-summary assistant
/// message against [`crate::compaction::CompactionPolicy`]'s resolved threshold —
/// upstream's `isOverflow` reads exactly that (`session/overflow.ts:22-34`,
/// `session/prompt.ts:1161-1167`), and measured beats estimated because the provider
/// counted the tokens it actually charged for. A session with no finished assistant
/// message yet cannot overflow: its whole history is one prompt.
///
/// The boundary comes from [`crate::compaction::select_boundary`] and is not
/// re-derived. That function's proptest is the reason a compacted request cannot
/// carry a `tool_result` whose `tool_use` was summarised away, and pairs produced by
/// one stored message are additionally inseparable because
/// [`retained_history`] filters by stored message.
///
/// # Errors
///
/// [`CompactionSkipped::Database`] for a read or write failure;
/// [`CompactionSkipped::Reason`] when the history does not overflow, no provider
/// answers for the compaction model, or the attempt stops.
pub async fn compact_if_overflowing(
    session_id: &str,
    context: &mut PreludeContext<'_>,
) -> Result<PreludeCompactionOutcome, CompactionSkipped> {
    let store_history = hydrate_retained_history(context.connection, session_id)
        .map_err(CompactionSkipped::Database)?;
    let retained = retained_history(&store_history);
    let Some(used_tokens) = measured_tokens(retained) else {
        return Ok(PreludeCompactionOutcome::NOT_NEEDED);
    };
    let trigger = CompactionTrigger::Threshold { used_tokens };
    if !CompactionPolicy::resolve(context.compaction, context.window).should_compact(trigger) {
        return Ok(PreludeCompactionOutcome::NOT_NEEDED);
    }

    compact_history(session_id, context, store_history, trigger, true).await
}

pub async fn compact_manually(
    session_id: &str,
    context: &mut PreludeContext<'_>,
) -> Result<bool, CompactionSkipped> {
    compact_requested(session_id, context, false).await
}

/// Runs an explicitly requested compaction while preserving whether its caller
/// classified the compaction as automatic.
pub async fn compact_requested(
    session_id: &str,
    context: &mut PreludeContext<'_>,
    automatic: bool,
) -> Result<bool, CompactionSkipped> {
    let store_history = hydrate_retained_history(context.connection, session_id)
        .map_err(CompactionSkipped::Database)?;
    compact_history(
        session_id,
        context,
        store_history,
        CompactionTrigger::Manual,
        automatic,
    )
    .await
    .map(|outcome| outcome.compacted)
}

async fn compact_history(
    session_id: &str,
    context: &mut PreludeContext<'_>,
    store_history: Vec<MessageWithParts>,
    trigger: CompactionTrigger,
    automatic: bool,
) -> Result<PreludeCompactionOutcome, CompactionSkipped> {
    let retained = retained_history(&store_history);

    let agent = &context.internals.compaction;
    let provider = context
        .providers
        .provider_for(agent)
        .map_err(CompactionSkipped::Reason)?;

    let mut tracker = CacheTracker::new();
    let mut locked: LockedTools<ToolDefinition> = LockedTools::new();
    let mut cache = CompactionCache::new(&mut tracker, &mut locked);
    let attempt_id = format!("compact_{}", zuno_db::message::now_millis());
    let requested_agent = requested_agent(retained).unwrap_or_else(|| agent.name.clone());
    let entries = transcript_owned(&agent.prompt, store_history);
    let request = CompactionRequest::new(
        session_id,
        &attempt_id,
        &requested_agent,
        &agent.model.provider.provider,
        &agent.model.model_id,
        entries,
        context.compaction,
        context.window,
        trigger,
    );
    let request = if automatic { request } else { request.manual() };
    let outcome = run_compaction(
        context.connection,
        provider.as_ref(),
        context.hooks,
        context.state,
        &mut cache,
        request,
    )
    .await
    .map_err(|CompactionError::Database(error)| CompactionSkipped::Database(error))?;

    match outcome {
        CompactionOutcome::Compacted(transcript) => Ok(PreludeCompactionOutcome {
            compacted: true,
            continue_turn: transcript.auto_continue,
        }),
        CompactionOutcome::NotNeeded => Ok(PreludeCompactionOutcome::NOT_NEEDED),
        CompactionOutcome::Stopped {
            reason,
            message,
            recovery,
        } => Err(CompactionSkipped::Stopped {
            reason,
            message,
            recovery,
        }),
    }
}

/// Summarise what a session accomplished, without persisting anything.
///
/// The third internal, on the same resolution path as the other two and with the
/// same tool-free request shape. It writes nothing because a summary's destination is
/// the caller's: the surface that renders one owns where it goes, and a function that
/// both generated and filed it would force one answer on every future caller. No
/// surface in this workspace requests a summary today; the seam is here so that when
/// one does it does not resolve a model of its own, which is exactly how the three
/// internals came to be declared and never invoked.
///
/// # Errors
///
/// A message when the history cannot be read, no provider answers for the summary
/// model, or the model returns nothing.
pub async fn summarize(
    session_id: &str,
    context: &mut PreludeContext<'_>,
) -> Result<String, String> {
    let history = hydrate_retained_history(context.connection, session_id)
        .map_err(|error| error.to_string())?;
    let agent = &context.internals.summary;
    let provider = context.providers.provider_for(agent)?;
    let mut messages = vec![Message::new(Role::System, agent.prompt.clone())];
    messages.extend(
        project_history("", retained_history(&history))
            .into_iter()
            .skip(1)
            .map(|projected| projected.message),
    );
    let text = collect_text(provider.as_ref(), agent, messages).await?;
    if text.trim().is_empty() {
        return Err("the summary model returned no text".to_owned());
    }
    Ok(text)
}

/// The measured token usage the overflow check compares against the window.
///
/// The newest assistant message that finished and is not itself a compaction summary
/// — `lastFinished && lastFinished.summary !== true` (`session/prompt.ts:1161-1163`).
/// Skipping summaries matters: a summary message's own usage is the cost of
/// *compacting*, not of the conversation, and counting it would make a session that
/// just compacted look like it needs compacting again.
#[must_use]
pub fn measured_tokens(history: &[MessageWithParts]) -> Option<u64> {
    history
        .iter()
        .rev()
        .filter(|message| {
            message.info.data.contains_key("finish")
                && message.info.data.get("summary").and_then(Value::as_bool) != Some(true)
        })
        .find_map(|message| message.info.data.get("tokens").and_then(Value::as_object))
        .map(|tokens| {
            let count = |key: &str| tokens.get(key).and_then(Value::as_u64).unwrap_or(0);
            let cache = |key: &str| {
                tokens
                    .get("cache")
                    .and_then(Value::as_object)
                    .and_then(|cache| cache.get(key))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            let total = count("total");
            if total > 0 {
                return total;
            }
            count("input")
                .saturating_add(count("output"))
                .saturating_add(cache("read"))
                .saturating_add(cache("write"))
        })
}

/// Project stored history into the transcript [`select_boundary`] selects over.
///
/// [`select_boundary`]: crate::compaction::select_boundary
///
/// Built from [`project_history`] so the entries the boundary is chosen over are the
/// same messages the request would have carried — a transcript assembled any other
/// way would have the boundary land somewhere the request does not have a seam. The
/// leading system entry is marked initial context by [`TranscriptEntry::new`], and a
/// stored message that projects to no request content contributes no entry at all,
/// so a compaction marker cannot be chosen as a tail start.
#[must_use]
pub fn transcript(system_prompt: &str, history: &[MessageWithParts]) -> Vec<TranscriptEntry> {
    project_history(system_prompt, history)
        .into_iter()
        .map(|projected| {
            let estimated = estimate_tokens(&projected.message);
            TranscriptEntry::new(
                projected.message_id.unwrap_or_default(),
                projected.message,
                estimated,
            )
        })
        .collect()
}

/// Consume stored history into the transcript selected and summarized by compaction.
///
/// Part strings and JSON values move into provider messages. Each complete message
/// is charged before tool output is reduced to the summary-safe representation, so
/// boundary selection remains based on the provider-visible history without ever
/// collecting every complete tool result into a second transcript.
#[must_use]
pub fn transcript_owned(
    system_prompt: &str,
    history: Vec<MessageWithParts>,
) -> Vec<TranscriptEntry> {
    map_project_history_owned_with_ids(system_prompt, history, |projected| {
        let estimated = estimate_tokens(&projected.message);
        TranscriptEntry::new(
            projected.message_id.unwrap_or_default(),
            summary_safe_message_owned(projected.message),
            estimated,
        )
    })
}

/// Estimate one message's token cost the way upstream does.
///
/// `Token.estimate(JSON.stringify(msgs))` (`session/compaction.ts:180-185`) over the
/// serialized model messages, which is `round(len / 4)`. Serializing per message
/// rather than over the whole slice differs by the array punctuation between
/// elements — a handful of characters against a budget of thousands — and buys a
/// per-entry number, which is what [`select_boundary`] needs to walk backwards.
///
/// [`select_boundary`]: crate::compaction::select_boundary
///
/// A message that cannot be serialized is charged nothing rather than refused: the
/// estimate governs a threshold, and a type that fails to serialize would fail the
/// request itself a moment later with a far better message.
#[must_use]
pub fn estimate_tokens(message: &Message) -> u32 {
    let length = serde_json::to_string(message).map_or(0, |json| json.len());
    let rounded = (length + CHARS_PER_TOKEN / 2) / CHARS_PER_TOKEN;
    u32::try_from(rounded).unwrap_or(u32::MAX)
}

/// The opening exchange a title is generated from, when there is exactly one.
///
/// `session/prompt.ts:202-206`: find the first real user message, require that it is
/// the *only* one, and take the history up to and including it. A message whose parts
/// project to no request content is not a real user turn — that is upstream's
/// `!m.parts.every(p => p.synthetic)` test expressed in terms this port already has.
fn opening_exchange(history: &[MessageWithParts]) -> Option<&[MessageWithParts]> {
    let mut real_users = history
        .iter()
        .enumerate()
        .filter(|(_, message)| is_real_user_turn(message));
    let (index, _) = real_users.next()?;
    if real_users.next().is_some() {
        return None;
    }
    Some(&history[..=index])
}

fn is_real_user_turn(message: &MessageWithParts) -> bool {
    message.info.role == MessageRole::User
        && message
            .parts
            .iter()
            .any(|part| matches!(part.kind, PartKind::Text | PartKind::File))
}

fn requested_agent(history: &[MessageWithParts]) -> Option<String> {
    history
        .iter()
        .rev()
        .filter(|message| is_real_user_turn(message))
        .find_map(|message| {
            message
                .info
                .data
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

/// Collect a tool-free completion's text, or say why there is none.
///
/// The `tools` vector is empty and that is the whole point of the function existing:
/// all three internals deny every tool (`agent.ts:221`, `:241`, `:256`, ported at
/// `zuno_agent::builtin::internal`), and a request that offered one could come back
/// with a call no dispatcher on this path would execute.
async fn collect_text(
    provider: &dyn Provider,
    agent: &InternalAgent,
    messages: Vec<Message>,
) -> Result<String, String> {
    let request = CompletionRequest {
        model_id: agent.model.model_id.clone(),
        surface: agent.model.surface,
        messages,
        tools: Vec::new(),
        parameters: serde_json::Map::new(),
        headers: std::collections::BTreeMap::new(),
    };
    let mut stream = provider.stream(request);
    let mut chunks = Vec::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(text)) => chunks.push(text),
            Ok(StreamEvent::Error { message, .. }) => return Err(message),
            Err(error) => return Err(error.to_string()),
            Ok(_) => {}
        }
    }
    Ok(chunks.concat())
}

/// Reduce a model's answer to the one line that may become a title.
///
/// `session/prompt.ts:243-250`: drop a `<think>` block, take the first non-empty
/// trimmed line, and cut anything over [`TITLE_MAX_CHARS`] to
/// [`TITLE_TRUNCATED_CHARS`] plus an ellipsis. Counting characters and not bytes,
/// because the title agent is instructed to answer in the user's own language and a
/// byte slice would split a multi-byte character and panic.
fn clean_title(text: &str) -> Option<String> {
    let without_thinking = strip_thinking(text);
    let line = without_thinking
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    if line.chars().count() <= TITLE_MAX_CHARS {
        return Some(line.to_owned());
    }
    let mut truncated: String = line.chars().take(TITLE_TRUNCATED_CHARS).collect();
    truncated.push_str("...");
    Some(truncated)
}

fn strip_thinking(text: &str) -> String {
    let mut remaining = text;
    let mut kept = String::new();
    while let Some(start) = remaining.find("<think>") {
        kept.push_str(&remaining[..start]);
        let after = &remaining[start..];
        match after.find("</think>") {
            Some(end) => remaining = &after[end + "</think>".len()..],
            None => return kept,
        }
    }
    kept.push_str(remaining);
    kept
}

#[cfg(test)]
#[path = "prelude_tests.rs"]
mod tests;
