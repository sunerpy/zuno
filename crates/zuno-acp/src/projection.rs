use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{Value, json};
use zuno_engine::context_usage::counters_from_stream_event;
use zuno_engine::r#loop::{INTERRUPTED_TURN_NOTICE, ToolDiff, ToolInterruption, TurnEvent};
use zuno_llm::event::StreamEvent;
use zuno_tool::{QuestionResultStatus, ToolResultPresentation};
use zuno_types::context_usage::{
    ContextRequestIdentity, ContextUsageSnapshot, ContextUsageSource, ContextUsageTracker,
    InvalidContextUsage,
};

use crate::presentation::{
    decorate_completed_tool_update, decorate_file_tool_call, decorate_file_tool_result,
    decorate_tool_call,
};

#[derive(Debug, Default)]
pub struct TurnEventProjector {
    context_size: Option<u64>,
    session_id: Option<String>,
    turn_id: Option<String>,
    context_source: ContextUsageSource,
    context_tracker: Option<ContextUsageTracker>,
    canonical_context: Option<ContextUsageSnapshot>,
    provider_id: Option<String>,
    model_id: Option<String>,
    assistant_message_id: Option<String>,
    request_step: Option<u32>,
    raw_inputs: HashMap<String, String>,
    tool_names: HashMap<String, String>,
    visible_tools: HashSet<String>,
    result_presentations: HashMap<String, ToolResultPresentation>,
}

/// ACP projection that exposes provider output only after its attempt is durable.
///
/// ACP message chunks are append-only. Holding attempt-scoped updates until
/// [`TurnEvent::AssistantCheckpointed`] is therefore the only protocol-safe way
/// to discard a failed partial stream when the engine emits `RetryRollback`.
#[derive(Debug, Default)]
pub struct AttemptBufferedTurnEventProjector {
    projector: TurnEventProjector,
    pending: Vec<Value>,
    deferred_completions: Vec<Value>,
    answered_questions: HashSet<String>,
    buffering: bool,
}

impl AttemptBufferedTurnEventProjector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_context_size(context_size: u64) -> Self {
        Self {
            projector: TurnEventProjector::with_context_size(context_size),
            ..Self::default()
        }
    }

    pub fn with_context_usage(snapshot: ContextUsageSnapshot) -> Result<Self, InvalidContextUsage> {
        Ok(Self {
            projector: TurnEventProjector::with_context_usage(snapshot)?,
            ..Self::default()
        })
    }

    /// State updates are revisable and must not wait behind append-only content.
    #[must_use]
    pub fn project_context_usage(&mut self, snapshot: &ContextUsageSnapshot) -> Vec<Value> {
        self.projector
            .project_context_usage(snapshot)
            .into_iter()
            .collect()
    }

    /// Project one engine event into zero or more committed ACP updates.
    #[must_use]
    pub fn project(&mut self, event: &TurnEvent) -> Vec<Value> {
        match event {
            TurnEvent::ContextUsageUpdated { snapshot } => self.project_context_usage(snapshot),
            TurnEvent::ToolResultPresented {
                call_id,
                presentation,
                ..
            } => {
                if matches!(
                    presentation,
                    ToolResultPresentation::Question(question)
                        if question.status() == QuestionResultStatus::Answered
                ) {
                    self.answered_questions.insert(call_id.clone());
                }
                let _ = self.projector.project(event);
                Vec::new()
            }
            TurnEvent::ToolDispatchCompleted {
                call_id, is_error, ..
            } if !*is_error && self.answered_questions.remove(call_id) => {
                let Some(mut completed) = self.projector.project(event) else {
                    return Vec::new();
                };
                set_question_continuation_pending(&mut completed, false);
                let mut continuing = completed.clone();
                continuing["status"] = json!("in_progress");
                set_question_continuation_pending(&mut continuing, true);
                self.deferred_completions.push(completed);
                vec![continuing]
            }
            TurnEvent::ProviderRequestStarted { .. } => {
                self.pending.clear();
                self.projector.reset_attempt();
                self.buffering = true;
                self.projector.project(event).into_iter().collect()
            }
            TurnEvent::Provider {
                event: StreamEvent::RetryRollback { .. },
                ..
            } => {
                self.pending.clear();
                self.projector.reset_attempt();
                self.projector.project(event).into_iter().collect()
            }
            TurnEvent::Provider {
                event: StreamEvent::TokenUsage { .. },
                ..
            } => self.projector.project(event).into_iter().collect(),
            TurnEvent::AssistantCheckpointed { .. } => {
                if let Some(update) = self.projector.project(event) {
                    self.pending.push(update);
                }
                self.buffering = false;
                let mut committed = std::mem::take(&mut self.deferred_completions);
                committed.append(&mut self.pending);
                committed
            }
            TurnEvent::TurnCompleted { .. }
            | TurnEvent::TurnInterrupted { .. }
            | TurnEvent::TurnFailed { .. }
            | TurnEvent::TurnWaitingForHuman { .. }
            | TurnEvent::SessionCommandCompleted { .. }
            | TurnEvent::SessionCommandFailed { .. } => {
                self.pending.clear();
                self.answered_questions.clear();
                self.buffering = false;
                let mut committed = std::mem::take(&mut self.deferred_completions);
                if let Some(update) = self.projector.abandon_context_request() {
                    committed.push(update);
                }
                if let Some(update) = self.projector.project(event) {
                    committed.push(update);
                }
                committed
            }
            _ if self.buffering => {
                if let Some(update) = self.projector.project(event) {
                    self.pending.push(update);
                }
                Vec::new()
            }
            _ => self.projector.project(event).into_iter().collect(),
        }
    }

    /// Settle durable client-visible results when an event stream ends without a
    /// terminal event. Attempt-scoped provider output remains discarded.
    #[must_use]
    pub fn finish(&mut self) -> Vec<Value> {
        self.pending.clear();
        self.answered_questions.clear();
        self.buffering = false;
        self.projector.reset_attempt();
        let mut committed = std::mem::take(&mut self.deferred_completions);
        if let Some(update) = self.projector.abandon_context_request() {
            committed.push(update);
        }
        committed
    }
}

impl TurnEventProjector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_context_size(context_size: u64) -> Self {
        Self {
            context_size: Some(context_size).filter(|size| *size > 0),
            ..Self::default()
        }
    }

    /// Seed the exact persisted snapshot when restoring a session.
    pub fn with_context_usage(snapshot: ContextUsageSnapshot) -> Result<Self, InvalidContextUsage> {
        snapshot.validate()?;
        Ok(Self {
            context_size: snapshot.context_limit,
            session_id: Some(snapshot.session_id.clone()),
            context_source: snapshot.source,
            canonical_context: Some(snapshot),
            ..Self::default()
        })
    }

    /// Project a canonical state revision supplied by the host.
    ///
    /// Once the host supplies canonical state, raw request estimates and provider
    /// frames cannot overwrite it. Different sessions/sources and old revisions
    /// are rejected; higher revisions can legitimately reduce occupancy.
    #[must_use]
    pub fn project_context_usage(&mut self, snapshot: &ContextUsageSnapshot) -> Option<Value> {
        snapshot.validate().ok()?;
        if snapshot.source != self.context_source
            || self
                .session_id
                .as_ref()
                .is_some_and(|session_id| session_id != &snapshot.session_id)
            || self.canonical_context.as_ref().is_some_and(|current| {
                snapshot.revision <= current.revision
                    || snapshot.context_epoch < current.context_epoch
            })
        {
            return None;
        }
        self.session_id = Some(snapshot.session_id.clone());
        self.context_size = snapshot.context_limit;
        self.canonical_context = Some(snapshot.clone());
        self.context_tracker = None;
        let mut update = context_usage_update(snapshot)?;
        if let Some(turn_id) = self.turn_id.as_deref() {
            attach_turn_id(&mut update, turn_id);
        }
        Some(update)
    }

    #[must_use]
    pub fn project(&mut self, event: &TurnEvent) -> Option<Value> {
        if let TurnEvent::TurnStarted {
            session_id,
            turn_id,
        } = event
        {
            if self
                .session_id
                .as_ref()
                .is_some_and(|current| current != session_id)
            {
                self.context_tracker = None;
                self.canonical_context = None;
                self.provider_id = None;
                self.model_id = None;
            }
            self.session_id = Some(session_id.clone());
            self.turn_id = Some(turn_id.clone());
            self.assistant_message_id = None;
            self.request_step = None;
            return None;
        }
        let mut update = self.project_inner(event)?;
        if let Some(turn_id) = self.turn_id.as_deref() {
            attach_turn_id(&mut update, turn_id);
        }
        Some(update)
    }

    fn reset_attempt(&mut self) {
        self.raw_inputs.clear();
        self.tool_names.clear();
        self.visible_tools.clear();
        self.result_presentations.clear();
    }

    fn begin_context_request(&mut self, step: u32, estimate: Option<u64>) {
        if self.canonical_context.is_some() || step == 0 {
            return;
        }
        let session_id = self.session_id.as_deref().unwrap_or("acp-unbound");
        let tracker = self.context_tracker.get_or_insert_with(|| {
            ContextUsageTracker::for_source(session_id, self.context_source)
        });
        let request_id = format!(
            "acp:{}:{}:{}",
            self.turn_id.as_deref().unwrap_or("unbound"),
            self.assistant_message_id.as_deref().unwrap_or("message"),
            step,
        );
        let current = tracker.snapshot().request.as_ref();
        if current.is_some_and(|current| current.request_id == request_id) {
            return;
        }
        let request = ContextRequestIdentity {
            request_id,
            request_sequence: current
                .map_or(1, |current| current.request_sequence.saturating_add(1)),
            attempt: 1,
            context_epoch: tracker.snapshot().context_epoch,
            provider_id: self
                .provider_id
                .clone()
                .unwrap_or_else(|| "unknown".to_owned()),
            model_id: self
                .model_id
                .clone()
                .unwrap_or_else(|| "unknown".to_owned()),
            source: self.context_source,
            turn_id: self.turn_id.clone(),
            // Legacy TurnEvent carries no request timestamp. Do not substitute
            // replay time for event time; canonical host snapshots carry it.
            time_started: 0,
            request_context_tokens: None,
            history_prefix: None,
        };
        self.request_step = Some(step);
        tracker.start_request(request, estimate, Some(0), self.context_size, 0);
    }

    fn projected_context(&self) -> Option<Value> {
        // Preserve the old API's explicit-window requirement. Canonical snapshots
        // can also publish an unknown state through project_context_usage().
        self.context_size?;
        let mut update = context_usage_update(self.context_tracker.as_ref()?.snapshot())?;
        update["_meta"]["zuno"]["contextUsageOrigin"] = json!("turn_events");
        Some(update)
    }

    fn abandon_context_request(&mut self) -> Option<Value> {
        if self.canonical_context.is_some() {
            return None;
        }
        let tracker = self.context_tracker.as_mut()?;
        let request = tracker.snapshot().request.clone()?;
        tracker
            .abandon_request(&request, 0)
            .then(|| self.projected_context())
            .flatten()
    }

    #[must_use]
    fn project_inner(&mut self, event: &TurnEvent) -> Option<Value> {
        match event {
            TurnEvent::ContextUsageUpdated { snapshot } => self.project_context_usage(snapshot),
            TurnEvent::Provider {
                event: StreamEvent::TextDelta(text),
                ..
            } => Some(content_update("agent_message_chunk", text)),
            TurnEvent::Provider {
                event: StreamEvent::ReasoningDelta(text),
                ..
            } => Some(content_update("agent_thought_chunk", text)),
            TurnEvent::SessionTitleUpdated { title } => Some(json!({
                "sessionUpdate": "session_info_update",
                "title": title,
            })),
            TurnEvent::ProviderRequestStarted {
                step,
                estimated_prompt_tokens,
                ..
            } => {
                if self.canonical_context.is_some() || *step == 0 {
                    return None;
                }
                self.begin_context_request(*step, Some(*estimated_prompt_tokens));
                self.projected_context()
            }
            TurnEvent::ModelResolved {
                provider_id,
                model_id,
                ..
            } => {
                self.provider_id = Some(provider_id.clone());
                self.model_id = Some(model_id.clone());
                None
            }
            TurnEvent::AssistantMessageCreated { message_id, .. } => {
                self.assistant_message_id = Some(message_id.clone());
                None
            }
            TurnEvent::AssistantCheckpointed { step, .. } => {
                if self.request_step == Some(*step)
                    && let Some(tracker) = self.context_tracker.as_mut()
                    && let Some(request) = tracker.snapshot().request.clone()
                {
                    tracker.commit_request(&request, 0);
                }
                None
            }
            TurnEvent::Provider {
                step,
                event: StreamEvent::RetryRollback { attempt, .. },
            } => {
                if self.canonical_context.is_some() || self.request_step != Some(*step) {
                    return None;
                }
                let tracker = self.context_tracker.as_mut()?;
                let request = tracker.snapshot().request.clone()?;
                tracker
                    .rollback_request(&request, *attempt, 0)
                    .then(|| self.projected_context())
                    .flatten()
            }
            TurnEvent::SessionCommandOutput { command, content } => {
                let mut update = content_update("agent_message_chunk", content);
                if matches!(
                    command,
                    zuno_engine::session_command::SessionCommand::Compact
                ) {
                    update["_meta"] = json!({
                        "zuno": {
                            "kind": "compaction_summary",
                        },
                    });
                }
                Some(update)
            }
            TurnEvent::Provider {
                event: StreamEvent::ToolUseStart { id, name },
                ..
            } => {
                self.raw_inputs.entry(id.clone()).or_default();
                self.tool_names.insert(id.clone(), name.clone());
                None
            }
            TurnEvent::Provider {
                event: StreamEvent::ToolInputDelta { id, delta },
                ..
            } => {
                let visible = self.visible_tools.contains(id);
                let raw_input = {
                    let raw_input = self.raw_inputs.entry(id.clone()).or_default();
                    raw_input.push_str(delta);
                    json_or_string(raw_input)
                };
                visible.then(|| {
                    let name = self.tool_names.get(id).map(String::as_str);
                    let command = name
                        .and_then(|name| shell_command(name, Some(&raw_input)))
                        .map(str::to_owned);
                    let mut update = json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                    });
                    if let Some(command) = command {
                        update["title"] = Value::String(command);
                    }
                    if let Some(name) = name {
                        decorate_tool_call(&mut update, name, Some(&raw_input));
                    }
                    update["rawInput"] = raw_input;
                    update
                })
            }
            TurnEvent::ToolCallStarted {
                call_id,
                display_name,
                name,
                ..
            } => {
                self.visible_tools.insert(call_id.clone());
                self.tool_names.insert(call_id.clone(), name.clone());
                Some(tool_call(
                    call_id,
                    display_name,
                    name,
                    "pending",
                    self.raw_inputs
                        .get(call_id)
                        .map(|value| json_or_string(value)),
                ))
            }
            TurnEvent::ToolDispatchStarted {
                call_id,
                display_name,
                name,
                ..
            } => {
                self.visible_tools.insert(call_id.clone());
                self.tool_names.insert(call_id.clone(), name.clone());
                let mut update = tool_call(
                    call_id,
                    display_name,
                    name,
                    "in_progress",
                    self.raw_inputs
                        .get(call_id)
                        .map(|value| json_or_string(value)),
                );
                update["sessionUpdate"] = json!("tool_call_update");
                Some(update)
            }
            TurnEvent::ToolDispatchBlocked { call_id, kind, .. } => {
                // The engine follows this notice with ToolDispatchCompleted.
                // Keep file target identity until that final update consumes it;
                // other tools retain their existing blocked-result presentation.
                let raw_input = if self
                    .tool_names
                    .get(call_id)
                    .is_some_and(|name| is_file_edit_tool(name))
                {
                    self.raw_inputs
                        .get(call_id)
                        .map(|value| json_or_string(value))
                } else {
                    self.raw_inputs.remove(call_id);
                    self.tool_names.remove(call_id);
                    None
                };
                self.visible_tools.remove(call_id);
                self.result_presentations.remove(call_id);
                let kind = kind.as_str();
                let mut update = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "status": "failed",
                    "rawOutput": { "blocked": true, "kind": kind },
                    "content": [text_content(&format!("Tool dispatch blocked: {kind}"))],
                    "_meta": {
                        "zuno": {
                            "blockedKind": kind,
                        },
                    },
                });
                if let Some(name) = self.tool_names.get(call_id) {
                    decorate_file_tool_call(&mut update, name, raw_input.as_ref());
                }
                Some(update)
            }
            TurnEvent::ToolDispatchInterrupted {
                call_id,
                display_name,
                name,
                title,
                output,
                interruption,
                uncertain,
                ..
            } => {
                let raw_input = self
                    .raw_inputs
                    .remove(call_id)
                    .map(|value| json_or_string(&value));
                self.tool_names.remove(call_id);
                self.visible_tools.remove(call_id);
                self.result_presentations.remove(call_id);
                Some(interrupted_tool_update(
                    CompletedToolUpdate {
                        call_id,
                        display_name,
                        name,
                        title,
                        raw_input: raw_input.as_ref(),
                        output,
                        diff: None,
                        written_paths: &[],
                        is_error: true,
                        presentation: None,
                        metadata: None,
                    },
                    *interruption,
                    *uncertain,
                ))
            }
            TurnEvent::ToolResultPresented {
                call_id,
                presentation,
                ..
            } => {
                self.result_presentations
                    .insert(call_id.clone(), presentation.clone());
                None
            }
            TurnEvent::ToolDispatchCompleted {
                call_id,
                display_name,
                name,
                title,
                output,
                diff,
                written_paths,
                is_error,
                ..
            } => {
                let raw_input = self
                    .raw_inputs
                    .remove(call_id)
                    .map(|value| json_or_string(&value));
                self.tool_names.remove(call_id);
                self.visible_tools.remove(call_id);
                let presentation = self.result_presentations.remove(call_id);
                Some(completed_tool_update(CompletedToolUpdate {
                    call_id,
                    display_name,
                    name,
                    title,
                    raw_input: raw_input.as_ref(),
                    output,
                    diff: diff.as_ref(),
                    written_paths,
                    is_error: *is_error,
                    presentation: presentation.as_ref(),
                    metadata: None,
                }))
            }
            TurnEvent::TurnInterrupted { request, .. } => Some(interruption_update(
                request.map(|request| request.source),
                request.map(|request| request.reason),
            )),
            TurnEvent::Provider {
                event:
                    StreamEvent::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    },
                ..
            } => {
                let raw_input = self
                    .raw_inputs
                    .remove(tool_use_id)
                    .map(|value| json_or_string(&value));
                let name = self.tool_names.remove(tool_use_id);
                self.visible_tools.remove(tool_use_id);
                self.result_presentations.remove(tool_use_id);
                let mut update = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_use_id,
                    "status": if *is_error { "failed" } else { "completed" },
                    "rawOutput": json_or_string(content),
                    "content": [text_content(content)],
                });
                if let Some(name) = name {
                    decorate_file_tool_call(&mut update, &name, raw_input.as_ref());
                }
                Some(update)
            }
            TurnEvent::Provider {
                step,
                event: frame @ StreamEvent::TokenUsage { .. },
            } => {
                if self.canonical_context.is_some() || *step == 0 {
                    return None;
                }
                if self.context_tracker.is_none() {
                    self.begin_context_request(*step, None);
                }
                if self.request_step != Some(*step) {
                    return None;
                }
                let tracker = self.context_tracker.as_mut()?;
                let request = tracker.snapshot().request.clone()?;
                tracker
                    .observe_usage(&request, counters_from_stream_event(frame)?, 0)
                    .then(|| self.projected_context())
                    .flatten()
            }
            TurnEvent::Notice {
                audience,
                severity,
                code,
                detail,
            } => {
                if *audience == zuno_engine::r#loop::NoticeAudience::Diagnostic {
                    return None;
                }
                let mut update = content_update("agent_thought_chunk", detail);
                update["_meta"] = json!({
                    "zuno": {
                        "notice": { "severity": severity.as_str(), "code": code },
                    },
                });
                Some(update)
            }
            TurnEvent::Provider {
                event: StreamEvent::StatusDetail { .. },
                ..
            } => None,
            TurnEvent::Provider {
                event: StreamEvent::Error { .. },
                ..
            } => None,
            _ => None,
        }
    }
}

#[must_use]
pub fn turn_event_update(event: &TurnEvent) -> Option<Value> {
    TurnEventProjector::new().project(event)
}

/// One representation for live state and durable replay.
///
/// ACP's numeric usage update requires both `used` and `size`. Unknown state
/// travels as session metadata, so no client receives a fabricated zero or
/// context window.
pub(crate) fn context_usage_update(snapshot: &ContextUsageSnapshot) -> Option<Value> {
    snapshot.validate().ok()?;
    let mut update = match snapshot.used_tokens.zip(snapshot.context_limit) {
        Some((used, size)) => json!({
            "sessionUpdate": "usage_update",
            "used": used,
            "size": size,
        }),
        None => json!({
            "sessionUpdate": "session_info_update",
        }),
    };
    update["_meta"] = json!({
        "zuno": {
            "contextUsage": snapshot,
        },
    });
    if let Some(turn_id) = snapshot
        .request
        .as_ref()
        .and_then(|request| request.turn_id.as_deref())
    {
        attach_turn_id(&mut update, turn_id);
    }
    Some(update)
}

fn attach_turn_id(update: &mut Value, turn_id: &str) {
    if !update["_meta"].is_object() {
        update["_meta"] = json!({});
    }
    if !update["_meta"]["zuno"].is_object() {
        update["_meta"]["zuno"] = json!({});
    }
    update["_meta"]["zuno"]["turnId"] = json!(turn_id);
}

#[must_use]
pub fn tool_call(
    call_id: &str,
    display_name: &str,
    name: &str,
    status: &str,
    raw_input: Option<Value>,
) -> Value {
    let title = if is_file_edit_tool(name) {
        "Editing files"
    } else {
        shell_command(name, raw_input.as_ref()).unwrap_or(display_name)
    };
    let mut update = json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call_id,
        "title": title,
        "kind": tool_kind(name),
        "status": status,
    });
    add_shell_interpreter(&mut update, name, display_name);
    decorate_tool_call(&mut update, name, raw_input.as_ref());
    if let Some(raw_input) = raw_input {
        update["rawInput"] = raw_input;
    }
    update
}

pub(crate) struct CompletedToolUpdate<'a> {
    pub call_id: &'a str,
    pub display_name: &'a str,
    pub name: &'a str,
    pub title: &'a str,
    pub raw_input: Option<&'a Value>,
    pub output: &'a str,
    pub diff: Option<&'a ToolDiff>,
    pub written_paths: &'a [String],
    pub is_error: bool,
    pub presentation: Option<&'a ToolResultPresentation>,
    pub metadata: Option<&'a serde_json::Map<String, Value>>,
}

#[must_use]
pub(crate) fn completed_tool_update(input: CompletedToolUpdate<'_>) -> Value {
    let CompletedToolUpdate {
        call_id,
        display_name,
        name,
        title,
        raw_input,
        output,
        diff,
        written_paths,
        is_error,
        presentation,
        metadata,
    } = input;
    let title = if is_file_edit_tool(name) {
        "Editing files"
    } else {
        shell_command(name, raw_input).unwrap_or({
            if title.is_empty() {
                display_name
            } else {
                title
            }
        })
    };
    let native_file_diff = diff.is_some_and(|diff| !diff.files().is_empty());
    let mut content = Vec::new();
    if !is_file_edit_tool(name) || is_error || !native_file_diff {
        content.push(text_content(output));
    }
    if let Some(diff) = diff {
        if diff.files().is_empty() {
            if let Some(unified) = diff.unified() {
                content.push(unified_diff_content(unified));
            }
        } else {
            content.extend(diff.files().iter().map(file_diff_content));
        }
    }
    if content.is_empty() {
        content.push(text_content(output));
    }
    let locations = tool_locations(written_paths, diff);
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": call_id,
        "title": title,
        "kind": tool_kind(name),
        "status": if is_error { "failed" } else { "completed" },
        "rawOutput": json_or_string(output),
        "content": content,
    });
    if !locations.is_empty() {
        update["locations"] = Value::Array(locations);
    }
    add_shell_interpreter(&mut update, name, display_name);
    decorate_file_tool_result(
        &mut update,
        name,
        raw_input,
        written_paths,
        diff,
        presentation,
        metadata,
    );
    decorate_completed_tool_update(
        &mut update,
        name,
        raw_input,
        presentation,
        metadata,
        output,
        is_error,
    );
    update
}

fn set_question_continuation_pending(update: &mut Value, pending: bool) {
    if update["_meta"]["zuno"]["question"].is_object() {
        update["_meta"]["zuno"]["question"]["continuationPending"] = Value::Bool(pending);
    }
}

/// Projects a cancelled call, presenting the certainty its caller resolved.
///
/// `uncertain` is a separate argument from `interruption` because the mode does not
/// decide it: a cooperative return whose tool says its work never reached a decided
/// outcome is uncertain too. The live caller passes the field the engine publishes and
/// replay reconstructs it from durable metadata, so both paths present one verdict —
/// which is what `outcome`, a `task` subagent's state, and a `question`'s status say.
pub(crate) fn interrupted_tool_update(
    input: CompletedToolUpdate<'_>,
    interruption: ToolInterruption,
    uncertain: bool,
) -> Value {
    let mut metadata = input.metadata.cloned().unwrap_or_default();
    let presentation_state = if uncertain { "uncertain" } else { "cancelled" };
    match input.name {
        "task" => {
            let subagent = metadata
                .entry("subagent".to_owned())
                .or_insert_with(|| json!({}));
            if !subagent.is_object() {
                *subagent = json!({});
            }
            subagent["state"] = json!(presentation_state);
        }
        "question" => {
            metadata.insert("questionStatus".to_owned(), json!(presentation_state));
        }
        _ => {}
    }
    let mut update = completed_tool_update(CompletedToolUpdate {
        metadata: Some(&metadata),
        ..input
    });
    let zuno = update
        .as_object_mut()
        .expect("tool update is an object")
        .entry("_meta")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("tool update metadata is an object")
        .entry("zuno")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("zuno tool metadata is an object");
    zuno.insert("outcome".to_owned(), json!(presentation_state));
    zuno.insert("cancelled".to_owned(), json!(true));
    zuno.insert("interruptionMode".to_owned(), json!(interruption.as_str()));
    // The mode alone: the grace window expired. Only `uncertain` answers whether
    // authoritative state has to be inspected.
    zuno.insert("forced".to_owned(), json!(interruption.is_forced()));
    zuno.insert("uncertain".to_owned(), json!(uncertain));
    update
}

fn shell_command<'a>(name: &str, raw_input: Option<&'a Value>) -> Option<&'a str> {
    if name != "shell" {
        return None;
    }
    raw_input?
        .as_object()?
        .get("command")?
        .as_str()
        .filter(|command| !command.is_empty())
}

fn add_shell_interpreter(update: &mut Value, name: &str, display_name: &str) {
    if name == "shell" {
        update["_meta"] = json!({
            "zuno": {
                "interpreter": display_name,
            },
        });
    }
}

fn content_update(kind: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": { "type": "text", "text": text },
    })
}

fn interruption_update(
    source: Option<zuno_engine::interrupt::HardInterruptSource>,
    reason: Option<zuno_engine::interrupt::HardInterruptReason>,
) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": INTERRUPTED_TURN_NOTICE },
        "_meta": {
            "zuno": {
                "kind": "turn_interrupted",
                "source": source,
                "reason": reason,
            },
        },
    })
}

fn json_or_string(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_owned()))
}

fn text_content(text: &str) -> Value {
    json!({
        "type": "content",
        "content": { "type": "text", "text": text },
    })
}

fn file_diff_content(diff: &zuno_tool::FileDiff) -> Value {
    json!({
        "type": "diff",
        "path": zuno_paths::wire_path(Path::new(diff.path())),
        "oldText": diff.old_text(),
        "newText": diff.new_text(),
    })
}

fn unified_diff_content(diff: &str) -> Value {
    json!({
        "type": "content",
        "content": { "type": "text", "text": diff },
        "_meta": {
            "zuno": {
                "kind": "unified_diff",
            },
        },
    })
}

fn tool_locations(paths: &[String], diff: Option<&ToolDiff>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut locations = Vec::new();
    for path in paths {
        let path = zuno_paths::wire_path(Path::new(path));
        if seen.insert(path.clone()) {
            locations.push(json!({ "path": path }));
        }
    }
    if let Some(diff) = diff {
        for file in diff.files() {
            let path = zuno_paths::wire_path(Path::new(file.path()));
            if seen.insert(path.clone()) {
                locations.push(json!({ "path": path }));
            }
        }
    }
    locations
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "glob" => "read",
        "write" | "edit" | "apply_patch" => "edit",
        "delete" => "delete",
        "move" => "move",
        "grep" | "search" => "search",
        "shell" | "execute" => "execute",
        "fetch" | "webfetch" => "fetch",
        _ => "other",
    }
}

fn is_file_edit_tool(name: &str) -> bool {
    matches!(name, "write" | "edit" | "apply_patch")
}
