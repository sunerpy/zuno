//! Provider and normalized-request boundaries for canonical context usage.
//!
//! The state machine lives in `zuno_types::context_usage`; this module translates
//! provider events and already assembled request content without reading files or
//! invoking a model. Hosts persist the tracker and publish its shared snapshot.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zuno_db::message::{MessageRole, MessageWithParts};
use zuno_error::DbError;
use zuno_llm::event::{PromptAccounting, Role, StreamEvent};
use zuno_llm::registry::{CompletionRequest, ProviderRequestContext, RequestMessage};
use zuno_types::context_usage::{
    ContextHistoryPrefix, ContextRequestIdentity, ContextTokenAccounting, ContextUsageCounters,
    ContextUsageSnapshot, ContextUsageSource, ContextUsageTotals, ContextUsageTracker,
};

pub const CONTEXT_USAGE_HISTORY_LIMIT: usize = 512;
pub const CONTEXT_USAGE_HISTORY_BYTE_LIMIT: u64 = 16 * 1_024 * 1_024;

/// Runtime writer for one foreground context. Consumers never instantiate this.
pub(crate) struct ContextUsageRecorder {
    tracker: ContextUsageTracker,
    persisted_revision: Option<u64>,
}

impl ContextUsageRecorder {
    pub(crate) fn load(
        connection: &zuno_db::Connection,
        session: &zuno_db::session::Session,
    ) -> Result<Self, DbError> {
        let source = session_context_source(session);
        let stored = zuno_db::context_usage::read_source_in(connection, &session.id, source)?;
        let persisted_revision = stored.as_ref().map(|tracker| tracker.snapshot().revision);
        let tracker = match stored {
            Some(tracker) => tracker,
            None => ContextUsageTracker::from_snapshot(read_context_usage(connection, session)?)
                .map_err(context_read_error)?,
        };
        // Reading an active checkpoint is not evidence that its host died. Keep
        // it intact; only an authorized request/rollback boundary may supersede
        // it, with revision CAS fencing writers and observed usage preserved.
        Ok(Self {
            tracker,
            persisted_revision,
        })
    }

    pub(crate) fn snapshot(&self) -> &ContextUsageSnapshot {
        self.tracker.snapshot()
    }

    pub(crate) fn observe_history_epoch(&mut self, epoch: i64) -> Result<(), DbError> {
        if epoch < 0 {
            return Err(context_read_error("negative durable context epoch"));
        }
        self.tracker
            .observe_history_epoch(epoch, zuno_db::message::now_millis());
        Ok(())
    }

    pub(crate) fn begin_request(
        &mut self,
        mut identity: ContextRequestIdentity,
        request: &CompletionRequest,
        context_limit: Option<u64>,
    ) -> Result<(), DbError> {
        let estimate = estimate_request_context(request);
        let previous = self.tracker.snapshot();
        identity.context_epoch = previous.context_epoch;
        identity.request_context_tokens = Some(estimate.request_context_tokens);
        identity.history_prefix = history_prefix(request, None);
        let tail = self.prepared_tail(&identity.provider_id, &identity.model_id, request, estimate);
        let at_ms = identity.time_started;
        if !self.tracker.start_request(
            identity,
            Some(estimate.prompt_tokens),
            tail,
            context_limit,
            at_ms,
        ) {
            return Err(context_read_error(
                "context request identity is stale or belongs to another source",
            ));
        }
        Ok(())
    }

    pub(crate) fn projected_occupancy(
        &self,
        provider_id: &str,
        model_id: &str,
        request: &CompletionRequest,
    ) -> Option<u64> {
        let estimate = estimate_request_context(request);
        match self.snapshot().last_confirmed.as_ref() {
            Some(confirmed)
                if confirmed.request.provider_id == provider_id
                    && confirmed.request.model_id == model_id =>
            {
                confirmed
                    .usage
                    .context_tokens()
                    .zip(self.prepared_tail(provider_id, model_id, request, estimate))
                    .map(|(baseline, tail)| baseline.saturating_add(tail))
            }
            // The caller separately carries the raw post-hook estimate. Do not
            // label that estimate as a provider-confirmed context measurement.
            _ => None,
        }
    }

    pub(crate) fn confirmed_history_changed(&self, messages: &[RequestMessage]) -> bool {
        self.snapshot()
            .last_confirmed
            .as_ref()
            .and_then(|confirmed| confirmed.request.history_prefix.as_ref())
            .is_some_and(|prefix| {
                history_prefix_messages(messages, Some(prefix.message_count)).as_ref()
                    != Some(prefix)
            })
    }

    fn prepared_tail(
        &self,
        provider_id: &str,
        model_id: &str,
        request: &CompletionRequest,
        estimate: ContextRequestEstimate,
    ) -> Option<u64> {
        let previous = self.snapshot();
        previous
            .last_confirmed
            .as_ref()
            .map_or(Some(0), |confirmed| {
                if confirmed.request.provider_id != provider_id
                    || confirmed.request.model_id != model_id
                {
                    return Some(0);
                }
                if previous.request.as_ref() != Some(&confirmed.request)
                    || !confirmed.usage.is_complete()
                    || previous.freshness
                        == zuno_types::context_usage::ContextUsageFreshness::Unknown
                {
                    return None;
                }
                let prefix = confirmed.request.history_prefix.as_ref()?;
                if history_prefix(request, Some(prefix.message_count)).as_ref() != Some(prefix) {
                    // A foreground terminal result can rewrite an older tool output
                    // before the last assistant. Appended-tail arithmetic does not
                    // cover that mutation; retain the measurement but mark usage unknown.
                    return None;
                }
                let growth = confirmed
                    .request
                    .request_context_tokens
                    .map_or(0, |before| {
                        estimate.request_context_tokens.saturating_sub(before)
                    });
                estimate.tail_tokens.map(|tail| tail.saturating_add(growth))
            })
    }

    pub(crate) fn observe_frame(&mut self, event: &StreamEvent) -> Result<bool, DbError> {
        let request = self.tracker.snapshot().request.clone().ok_or_else(|| {
            context_read_error("provider usage arrived without a context request")
        })?;
        let at_ms = zuno_db::message::now_millis();
        Ok(match event {
            StreamEvent::RetryRollback { attempt, .. } => {
                self.tracker.rollback_request(&request, *attempt, at_ms)
            }
            frame @ StreamEvent::TokenUsage { .. } => self.tracker.observe_usage(
                &request,
                counters_from_stream_event(frame).expect("usage frame"),
                at_ms,
            ),
            _ => false,
        })
    }

    pub(crate) fn commit_request(
        &mut self,
        complete_response: bool,
        context_rewritten: bool,
        at_ms: i64,
    ) {
        if let Some(request) = self.tracker.snapshot().request.clone() {
            self.tracker.commit_request(&request, at_ms);
            if !complete_response || context_rewritten {
                self.tracker.set_estimated_tail(None, at_ms);
            }
            if !complete_response {
                self.tracker.mark_cumulative_unknown(at_ms);
            }
        }
    }

    /// Caller commits its assistant/request rows in this same transaction.
    pub(crate) fn persist_in(
        &self,
        transaction: &zuno_db::Transaction<'_>,
    ) -> Result<Option<ContextUsageSnapshot>, DbError> {
        if self.persisted_revision == Some(self.tracker.snapshot().revision) {
            return Ok(None);
        }
        zuno_db::context_usage::write_in(transaction, self.persisted_revision, &self.tracker)?;
        let snapshot = self.tracker.snapshot().clone();
        let properties = serde_json::json!({"snapshot": snapshot})
            .as_object()
            .expect("fixed context event envelope")
            .clone();
        zuno_db::event_log::append_in(
            transaction,
            &snapshot.session_id,
            zuno_db::event_log::NewSessionEvent::new("session.context.usage", properties)?,
        )?;
        Ok(Some(snapshot))
    }

    pub(crate) fn did_commit(&mut self) {
        self.persisted_revision = Some(self.tracker.snapshot().revision);
    }

    pub(crate) fn persist(
        &mut self,
        connection: &mut zuno_db::Connection,
    ) -> Result<Option<ContextUsageSnapshot>, DbError> {
        let transaction = connection.transaction().map_err(zuno_db::map_error)?;
        let snapshot = self.persist_in(&transaction)?;
        transaction.commit().map_err(zuno_db::map_error)?;
        self.did_commit();
        Ok(snapshot)
    }
}

fn history_prefix(request: &CompletionRequest, count: Option<u64>) -> Option<ContextHistoryPrefix> {
    history_prefix_messages(&request.messages, count)
}

fn history_prefix_messages(
    history: &[RequestMessage],
    count: Option<u64>,
) -> Option<ContextHistoryPrefix> {
    let limit = match count {
        Some(count) => usize::try_from(count).ok()?,
        None => usize::MAX,
    };
    let messages = history
        .iter()
        .filter(|message| message.role != Role::System)
        .take(limit)
        .collect::<Vec<_>>();
    if count.is_some() && messages.len() != limit {
        return None;
    }
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, &messages).ok()?;
    Some(ContextHistoryPrefix {
        message_count: u64::try_from(messages.len()).ok()?,
        sha256: hex::encode(writer.0.finalize()),
    })
}

/// Deterministic estimates of the exact normalized model-visible request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextRequestEstimate {
    pub prompt_tokens: u64,
    /// Messages after the supplied confirmed boundary; `None` means no anchor.
    pub tail_tokens: Option<u64>,
    /// Current system/developer context and tool schemas, already included in `prompt_tokens`.
    ///
    /// A caller changing these between requests must include their unaccounted
    /// growth in its tail estimate, or mark that tail unknown.
    pub request_context_tokens: u64,
}

/// Estimate the local items following the last assistant in normalized history.
///
/// Use this boundary only when that assistant supplied the active measurement.
/// With missing assistant usage, use [`estimate_request_context_after`] and the
/// actual confirmed boundary, or pass `None` rather than guessing an empty tail.
#[must_use]
pub fn estimate_request_context(request: &CompletionRequest) -> ContextRequestEstimate {
    let confirmed_history_end = request
        .messages
        .iter()
        .rposition(|message| message.role == Role::Assistant)
        .map(|index| index.saturating_add(1));
    estimate_request_context_after(request, confirmed_history_end)
}

/// Estimate only the final prepared request, including tool schemas.
///
/// `confirmed_history_end` is an exclusive message index, after the assistant
/// whose provider usage established the baseline. No source path, resource-link
/// byte length, or unread file contributes content absent from this request.
#[must_use]
pub fn estimate_request_context_after(
    request: &CompletionRequest,
    confirmed_history_end: Option<usize>,
) -> ContextRequestEstimate {
    let message_bytes = normalized_message_bytes(&request.messages);
    let developer_bytes = request
        .developer_context
        .iter()
        .fold(0_usize, |bytes, context| {
            bytes.saturating_add(context.len())
        });
    let tool_bytes = request.tools.iter().fold(0_usize, |bytes, tool| {
        bytes
            .saturating_add(tool.name.len())
            .saturating_add(tool.description.len())
            .saturating_add(tool.parameters.to_string().len())
    });
    let context_bytes = developer_bytes.saturating_add(tool_bytes);
    let system_bytes = request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .fold(0_usize, |bytes, message| {
            bytes.saturating_add(normalized_message_bytes(std::slice::from_ref(message)))
        });
    ContextRequestEstimate {
        prompt_tokens: tokens_for_bytes(message_bytes.saturating_add(context_bytes)),
        tail_tokens: confirmed_history_end
            .and_then(|end| request.messages.get(end..).map(estimate_message_tokens)),
        request_context_tokens: tokens_for_bytes(context_bytes.saturating_add(system_bytes)),
    }
}

#[must_use]
pub fn estimate_message_tokens(messages: &[RequestMessage]) -> u64 {
    if messages.is_empty() {
        return 0;
    }
    tokens_for_bytes(normalized_message_bytes(messages))
}

fn tokens_for_bytes(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX).div_ceil(4)
}

fn normalized_message_bytes(messages: &[RequestMessage]) -> usize {
    let Ok(mut serialized) = serde_json::to_value(messages) else {
        return 0;
    };
    if let Some(messages) = serialized.as_array_mut() {
        for message in messages {
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && let Some(block) = block.as_object_mut()
                    {
                        // This field mirrors `input` at the request-block boundary.
                        // A user's own nested `raw_arguments` field is real model
                        // content and must survive the estimate.
                        block.remove("raw_arguments");
                    }
                }
            }
        }
    }
    serde_json::to_vec(&serialized).map_or(0, |bytes| bytes.len())
}

/// The accepted compaction summary, using the engine's existing checkpoint rules.
///
/// Failed, incomplete, orphaned and dangling compactions cannot reset a usage
/// baseline during history adoption.
#[must_use]
pub fn latest_context_compaction(history: &[MessageWithParts]) -> Option<&MessageWithParts> {
    crate::compaction::checkpoint::latest_checkpoint(history).map(|checkpoint| checkpoint.summary)
}

/// Read canonical context or reconstruct a bounded legacy projection.
///
/// This read-only service is shared by HTTP/SSE and hosts restoring a TUI/ACP
/// session. It neither starts a controller nor persists an invented request.
/// Corrupt canonical state fails closed; only an absent row takes the fallback.
pub fn read_context_usage(
    connection: &zuno_db::Connection,
    session: &zuno_db::session::Session,
) -> Result<ContextUsageSnapshot, DbError> {
    let source = session_context_source(session);
    if let Some(tracker) = zuno_db::context_usage::read_source_in(connection, &session.id, source)?
    {
        return Ok(tracker.snapshot().clone());
    }
    let epoch: i64 = connection
        .query_row(
            "SELECT COALESCE((SELECT baseline_seq FROM session_context_epoch \
             WHERE session_id = ?1), 0)",
            [&session.id],
            |row| row.get(0),
        )
        .map_err(zuno_db::map_error)?;
    let epoch = u64::try_from(epoch).map_err(context_read_error)?;
    let limit = session
        .usage
        .context_limit
        .and_then(|limit| u64::try_from(limit).ok())
        .filter(|limit| *limit > 0);
    let history = crate::r#loop::hydrate_retained_history_tail(
        connection,
        &session.id,
        CONTEXT_USAGE_HISTORY_LIMIT,
        CONTEXT_USAGE_HISTORY_BYTE_LIMIT,
    )?;
    let mut snapshot = context_usage_from_history(&history.messages, source, limit, Some(epoch))
        .unwrap_or_else(|| {
            let mut unknown = ContextUsageSnapshot::unknown(&session.id);
            unknown.source = source;
            unknown.context_epoch = epoch;
            unknown.context_limit = limit;
            unknown
        });
    let usage = session.usage.snapshot();
    snapshot.cumulative_usage = ContextUsageTotals {
        input: usage.confirmed.input,
        output: usage.confirmed.output,
        reasoning: usage.confirmed.reasoning,
        cache_read: usage.confirmed.cache_read,
        cache_write: usage.confirmed.cache_write,
        unclassified: usage.confirmed.unclassified,
    };
    let empty_model_history = usage.confirmed.is_empty()
        && history.omitted == 0
        && history
            .messages
            .iter()
            .all(|message| message.info.role != MessageRole::Assistant);
    let no_provider_attempts = if empty_model_history {
        !connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event WHERE aggregate_id = ?1 \
             AND type LIKE 'session.provider.attempt.%')",
                [&session.id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(zuno_db::map_error)?
    } else {
        false
    };
    snapshot.cumulative_known =
        usage.confirmed_known || (empty_model_history && no_provider_attempts);
    snapshot.validate().map_err(context_read_error)?;
    Ok(snapshot)
}

#[must_use]
pub fn session_context_source(session: &zuno_db::session::Session) -> ContextUsageSource {
    if session.parent_id.is_some() {
        ContextUsageSource::Child
    } else {
        ContextUsageSource::Main
    }
}

fn context_read_error(error: impl std::fmt::Display) -> DbError {
    DbError::Query {
        source: Box::new(std::io::Error::other(error.to_string())),
    }
}

/// Adopt legacy durable history through the same counter and request projection
/// used by native execution. Auxiliary work never becomes foreground context.
///
/// `context_epoch` is supplied by storage when available. A bounded history alone
/// cannot establish complete session consumption, so its cumulative meter remains
/// explicitly incomplete until the caller supplies the session aggregate.
#[must_use]
pub fn context_usage_from_history(
    history: &[MessageWithParts],
    source: ContextUsageSource,
    context_limit: Option<u64>,
    context_epoch: Option<u64>,
) -> Option<ContextUsageSnapshot> {
    let history = crate::r#loop::retained_history(history);
    let session_id = &history.last()?.info.session_id;
    if !matches!(source, ContextUsageSource::Main | ContextUsageSource::Child)
        || history
            .iter()
            .any(|message| &message.info.session_id != session_id)
    {
        return None;
    }
    let mut tracker = ContextUsageTracker::for_source(session_id, source);
    tracker.seed_cumulative(ContextUsageTotals::default(), false, 0);
    let compacted_end = latest_context_compaction(history)
        .and_then(|summary| {
            history
                .iter()
                .position(|message| message.info.id == summary.info.id)
        })
        .map_or(0, |index| index.saturating_add(1));
    let epoch = context_epoch.unwrap_or(u64::try_from(compacted_end).ok()?);
    if epoch > 0 {
        tracker.reset_epoch(
            epoch,
            compacted_end
                .checked_sub(1)
                .map_or(0, |index| history[index].info.time_updated),
        );
    }
    let mut confirmed_index = None;
    for (index, message) in history.iter().enumerate().skip(compacted_end) {
        if message.info.role != MessageRole::Assistant || !foreground_usage(message, source) {
            continue;
        }
        let data = &message.info.data;
        let request = ContextRequestIdentity {
            request_id: data
                .get("requestID")
                .and_then(Value::as_str)
                .unwrap_or(&message.info.id)
                .to_owned(),
            request_sequence: u64::try_from(index).ok()?.saturating_add(1),
            attempt: data
                .get("attempt")
                .and_then(Value::as_u64)
                .and_then(|attempt| u32::try_from(attempt).ok())
                .filter(|attempt| *attempt > 0)
                .unwrap_or(1),
            context_epoch: tracker.snapshot().context_epoch,
            provider_id: data
                .get("providerID")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            model_id: data
                .get("modelID")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            source,
            turn_id: data
                .get("turnID")
                .and_then(Value::as_str)
                .map(str::to_owned),
            time_started: message.info.time_created,
            request_context_tokens: data.get("requestContextTokens").and_then(Value::as_u64),
            history_prefix: data
                .get("historyPrefix")
                .cloned()
                .and_then(|prefix| serde_json::from_value(prefix).ok()),
        };
        tracker.start_request(
            request.clone(),
            None,
            None,
            context_limit,
            message.info.time_created,
        );
        if tracker.snapshot().last_confirmed.is_none() {
            confirmed_index = None;
        }
        if let Some(counters) = counters_from_message_data(data) {
            tracker.observe_usage(&request, counters, message.info.time_updated);
            if counters.prompt_tokens().is_some() {
                confirmed_index = Some(index);
            }
        }
        tracker.commit_request(&request, message.info.time_updated);
    }
    if let Some(confirmed_index) = confirmed_index {
        let tail = crate::r#loop::project_history("", history)
            .into_iter()
            .filter(|projected| {
                projected.message_id.as_ref().is_some_and(|message_id| {
                    history.iter().enumerate().any(|(index, stored)| {
                        &stored.info.id == message_id
                            && foreground_usage(stored, source)
                            && (index > confirmed_index
                                || (index == confirmed_index
                                    && projected.message.role == Role::Tool))
                    })
                })
            })
            .map(|projected| {
                RequestMessage::new(projected.message)
                    .with_preceding_responses_input(projected.preceding_responses_input)
            })
            .collect::<Vec<_>>();
        tracker.set_estimated_tail(
            Some(estimate_message_tokens(&tail)),
            history.last()?.info.time_updated,
        );
    }
    let mut snapshot = tracker.snapshot().clone();
    snapshot.context_limit = context_limit.filter(|limit| *limit > 0);
    // A read-only legacy adoption must not outrank the first durable context row.
    snapshot.revision = 0;
    if let Some(request) = &mut snapshot.request {
        request.request_sequence = 0;
    }
    if let Some(confirmed) = &mut snapshot.last_confirmed {
        confirmed.request.request_sequence = 0;
    }
    snapshot.validate().ok()?;
    Some(snapshot)
}

fn foreground_usage(message: &MessageWithParts, source: ContextUsageSource) -> bool {
    if message.info.data.get("summary").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match message
        .info
        .data
        .get("requestPurpose")
        .and_then(Value::as_str)
    {
        None => true,
        Some("main-turn" | "main_turn") => source == ContextUsageSource::Main,
        Some("child-turn" | "child_turn") => source == ContextUsageSource::Child,
        Some(_) => false,
    }
}

/// Preserve the provider's accounting convention and optional snapshot fields.
#[must_use]
pub fn counters_from_stream_event(event: &StreamEvent) -> Option<ContextUsageCounters> {
    let StreamEvent::TokenUsage {
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        accounting,
    } = event
    else {
        return None;
    };
    Some(ContextUsageCounters {
        input_tokens: *input_tokens,
        output_tokens: *output_tokens,
        reasoning_tokens: *reasoning_tokens,
        cache_read_input_tokens: *cache_read_input_tokens,
        cache_write_input_tokens: *cache_write_input_tokens,
        accounting: match accounting {
            PromptAccounting::CacheInsideInput => ContextTokenAccounting::CacheInsideInput,
            PromptAccounting::CacheBesideInput => ContextTokenAccounting::CacheBesideInput,
        },
    })
}

/// Decode Zuno's durable assistant counters without fabricating missing fields.
///
/// Stored `output` is visible output, while the stream's output includes
/// reasoning. Reconstruct the inclusive output exactly once at this boundary.
#[must_use]
pub fn counters_from_message_data(data: &Map<String, Value>) -> Option<ContextUsageCounters> {
    let tokens = data.get("tokens")?.as_object()?;
    let cache = tokens.get("cache").and_then(Value::as_object);
    let reasoning = tokens.get("reasoning").and_then(Value::as_u64);
    Some(ContextUsageCounters {
        input_tokens: tokens.get("input").and_then(Value::as_u64),
        output_tokens: tokens
            .get("output")
            .and_then(Value::as_u64)
            .map(|output| output.saturating_add(reasoning.unwrap_or_default())),
        reasoning_tokens: reasoning,
        cache_read_input_tokens: cache
            .and_then(|cache| cache.get("read"))
            .and_then(Value::as_u64),
        cache_write_input_tokens: cache
            .and_then(|cache| cache.get("write"))
            .and_then(Value::as_u64),
        accounting: match tokens.get("accounting").and_then(Value::as_str) {
            Some("cache-inside-input") => ContextTokenAccounting::CacheInsideInput,
            Some("cache-beside-input") => ContextTokenAccounting::CacheBesideInput,
            _ => ContextTokenAccounting::Unknown,
        },
    })
}

/// Auxiliary work cannot acquire the foreground context by sharing a model.
#[must_use]
pub fn source_for_request(request: &ProviderRequestContext) -> ContextUsageSource {
    match request {
        ProviderRequestContext::MainTurn(_) => ContextUsageSource::Main,
        ProviderRequestContext::ChildTurn(_) => ContextUsageSource::Child,
        ProviderRequestContext::Learning | ProviderRequestContext::Reflection => {
            ContextUsageSource::Learning
        }
        ProviderRequestContext::Compaction => ContextUsageSource::Compaction,
        ProviderRequestContext::Title
        | ProviderRequestContext::Summary
        | ProviderRequestContext::Evaluation
        | ProviderRequestContext::Council => ContextUsageSource::Auxiliary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zuno_llm::event::{Message, RequestContentBlock};
    use zuno_llm::registry::ToolSchema;

    fn request(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest::new("synthetic-model", messages)
    }

    #[test]
    fn request_estimate_includes_actual_policy_and_tool_schema() {
        let original = request(vec![Message::new(Role::User, "Read the first ten lines.")]);
        let bare = estimate_request_context(&original);
        let prepared = original
            .with_developer_context(vec!["Apply the normalized request policy.".repeat(8)])
            .with_tools(vec![ToolSchema {
                name: "read".to_owned(),
                description: "Read a bounded section of an existing file.".repeat(4),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1}
                    },
                    "required": ["path"],
                }),
            }]);
        let estimate = estimate_request_context(&prepared);
        assert!(estimate.prompt_tokens > bare.prompt_tokens + 100);
        assert!(estimate.request_context_tokens > 100);
        assert_eq!(estimate.tail_tokens, None);
    }

    #[test]
    fn tail_is_only_actual_content_after_the_confirmed_assistant() {
        let prepared = request(vec![
            Message::new(Role::System, "Existing policy.".repeat(1_000)),
            Message::new(Role::User, "Previous input.".repeat(1_000)),
            Message::new(Role::Assistant, "Read a small section."),
            Message::from_content(
                Role::Tool,
                vec![RequestContentBlock::ToolResult {
                    tool_use_id: "synthetic-read".to_owned(),
                    content: "Only these two lines were returned.\nNo other file content."
                        .to_owned(),
                    is_error: Some(false),
                }],
            ),
            Message::new(Role::User, "Continue."),
        ]);
        let estimate = estimate_request_context(&prepared);
        let tail = estimate_request_context_after(&prepared, Some(3));
        assert_eq!(estimate.tail_tokens, tail.tail_tokens);
        assert!(estimate.prompt_tokens > 5_000);
        assert!(estimate.tail_tokens.unwrap() < 100);
        assert_eq!(
            estimate_request_context_after(&prepared, Some(5)).tail_tokens,
            Some(0)
        );
        assert_eq!(
            estimate_request_context_after(&prepared, Some(6)).tail_tokens,
            None
        );
    }

    #[test]
    fn resource_size_does_not_charge_unread_file_bytes() {
        let prepared = request(vec![Message::from_content(
            Role::User,
            vec![RequestContentBlock::ResourceLink {
                name: "source.rs".to_owned(),
                uri: "file:///workspace/source.rs".to_owned(),
                title: None,
                description: Some("Only a link has been supplied.".to_owned()),
                media_type: Some("text/plain".to_owned()),
                size: Some(1_000_000_000),
            }],
        )]);
        assert!(estimate_request_context(&prepared).prompt_tokens < 200);
    }

    #[test]
    fn tool_argument_mirror_is_not_counted_twice() {
        let input = json!({"content": "large but single model-visible argument".repeat(400)});
        let block = RequestContentBlock::ToolUse {
            id: "synthetic-call".to_owned(),
            name: "write".to_owned(),
            input: input.clone(),
            raw_arguments: Some(input.to_string()),
            thought_signature: None,
        };
        let with_mirror = request(vec![Message::from_content(Role::Assistant, vec![block])]);
        let without_mirror = request(vec![Message::from_content(
            Role::Assistant,
            vec![RequestContentBlock::ToolUse {
                id: "synthetic-call".to_owned(),
                name: "write".to_owned(),
                input,
                raw_arguments: None,
                thought_signature: None,
            }],
        )]);
        assert_eq!(
            estimate_request_context(&with_mirror).prompt_tokens,
            estimate_request_context(&without_mirror).prompt_tokens
        );
    }

    #[test]
    fn a_tool_inputs_own_raw_arguments_property_is_real_request_content() {
        let prepared = request(vec![Message::from_content(
            Role::Assistant,
            vec![RequestContentBlock::ToolUse {
                id: "synthetic-call".to_owned(),
                name: "analyze".to_owned(),
                input: json!({"raw_arguments": "x".repeat(8_000)}),
                raw_arguments: None,
                thought_signature: None,
            }],
        )]);
        assert!(estimate_request_context(&prepared).prompt_tokens > 2_000);
    }

    #[test]
    fn durable_reasoning_is_added_back_once_to_inclusive_output() {
        let data = json!({
            "tokens": {
                "input": 100,
                "output": 15,
                "reasoning": 10,
                "cache": {"read": 40, "write": 10},
                "accounting": "cache-beside-input",
            }
        });
        let counters = counters_from_message_data(data.as_object().unwrap()).unwrap();
        assert_eq!(counters.output_tokens, Some(25));
        assert_eq!(counters.context_tokens(), Some(175));
        assert_eq!(counters.disjoint().total(), 175);
    }

    #[test]
    fn missing_durable_fields_and_unknown_accounting_do_not_become_zero() {
        let data = json!({"tokens": {"output": 9, "accounting": "cache-inside-input"}});
        let counters = counters_from_message_data(data.as_object().unwrap()).unwrap();
        assert_eq!(counters.input_tokens, None);
        assert_eq!(counters.context_tokens(), None);
        let data = json!({"tokens": {"input": 100, "output": 9}});
        let counters = counters_from_message_data(data.as_object().unwrap()).unwrap();
        assert_eq!(counters.accounting, ContextTokenAccounting::Unknown);
        assert_eq!(counters.context_tokens(), None);
    }

    #[test]
    fn legacy_projection_does_not_outrank_the_first_durable_request() {
        let history = (1..=4)
            .map(|index| MessageWithParts {
                info: zuno_db::message::MessageRecord::from_json(json!({
                    "id": format!("legacy-message-{index}"),
                    "sessionID": "ses_legacy",
                    "role": "assistant",
                    "providerID": "synthetic-provider",
                    "modelID": "synthetic-model",
                    "time": {"created": index},
                    "tokens": {
                        "input": 100, "output": 5, "reasoning": 2,
                        "accounting": "cache-inside-input",
                    },
                }))
                .unwrap(),
                parts: Vec::new(),
            })
            .collect::<Vec<_>>();
        let snapshot =
            context_usage_from_history(&history, ContextUsageSource::Main, Some(1_000), Some(0))
                .unwrap();
        assert_eq!(snapshot.used_tokens, Some(107));
        assert_eq!(snapshot.revision, 0);
        assert_eq!(snapshot.request.as_ref().unwrap().request_sequence, 0);
        let mut tracker = ContextUsageTracker::from_snapshot(snapshot).unwrap();
        let next = ContextRequestIdentity {
            request_id: "first-durable-request".to_owned(),
            request_sequence: 1,
            attempt: 1,
            context_epoch: 0,
            provider_id: "synthetic-provider".to_owned(),
            model_id: "synthetic-model".to_owned(),
            source: ContextUsageSource::Main,
            turn_id: Some("new-turn".to_owned()),
            time_started: 5,
            request_context_tokens: None,
            history_prefix: None,
        };
        assert!(tracker.start_request(next, Some(80), Some(10), Some(1_000), 5));
        assert_eq!(tracker.snapshot().used_tokens, Some(117));
        tracker.validate().unwrap();
    }

    #[test]
    fn loading_active_context_does_not_claim_its_other_host_has_stopped() {
        let mut connection = zuno_db::open::open(&zuno_paths::DbLocation::Memory).unwrap();
        zuno_db::migration::apply(&mut connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('context-project', '/workspace', 1, 1, '[]')",
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        let session = zuno_db::session::create(
            &transaction,
            &zuno_db::session::SessionCreate::new(
                "ses_active_context",
                "active",
                "context-project",
                "/workspace",
                "/workspace",
                "Active context",
                "test",
            ),
        )
        .unwrap()
        .into_session();
        transaction.commit().unwrap();
        let identity: ContextRequestIdentity = serde_json::from_value(json!({
            "requestId": "active-request", "requestSequence": 1, "attempt": 1,
            "contextEpoch": 0, "providerId": "synthetic", "modelId": "synthetic",
            "source": "main", "turnId": "another-host-turn", "timeStarted": 1,
        }))
        .unwrap();
        let mut tracker = ContextUsageTracker::new(&session.id);
        tracker.start_request(identity.clone(), Some(100), Some(0), Some(1_000), 1);
        tracker.observe_usage(
            &identity,
            ContextUsageCounters {
                input_tokens: Some(120),
                output_tokens: Some(5),
                accounting: ContextTokenAccounting::CacheInsideInput,
                ..ContextUsageCounters::default()
            },
            2,
        );
        zuno_db::context_usage::write_in(&connection, None, &tracker).unwrap();
        let loaded = ContextUsageRecorder::load(&connection, &session).unwrap();
        assert_eq!(loaded.tracker, tracker);
        assert_eq!(
            zuno_db::context_usage::read_in(&connection, &session.id).unwrap(),
            Some(tracker)
        );
    }
}
