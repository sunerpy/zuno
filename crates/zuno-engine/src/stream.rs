//! Projection of provider stream events into durable session parts.
//!
//! Text and reasoning remain live in memory while they arrive. SQLite receives
//! an upsert only when the accumulated dirty payload reaches
//! [`DELTA_BATCH_BYTES`] or a part reaches a terminal event. A retry rollback
//! deletes any already-flushed parts from the abandoned attempt before replay.

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use zuno_db::message::{MessageStore, PartRecord, now_millis};
use zuno_db::{Connection, open, session};
use zuno_error::{DbError, ProviderError};
use zuno_llm::event::{FinishReason, PromptAccounting, StreamEvent, ThoughtSignature};
use zuno_llm::sse::{StreamLimits, append_tool_input};

/// Dirty delta bytes accumulated before live text/reasoning is upserted.
pub const DELTA_BATCH_BYTES: usize = 4 * 1024;

/// Stable identity and accounting inputs for one projected provider step.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionContext {
    /// Session that owns every projected part.
    pub session_id: String,
    /// Assistant message that owns every projected part.
    pub message_id: String,
    /// One-based provider step within the turn.
    pub step: u32,
    /// Assistant-message creation time and default part start time.
    pub created_at: i64,
    /// Resolved agent name, retained for projection consumers.
    pub agent: String,
    /// Cost added by this provider step.
    pub cost: f64,
    /// Active model context ceiling.
    pub context_limit: Option<u64>,
}

impl ProjectionContext {
    /// Build a step context with zero cost until provider accounting resolves it.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        message_id: impl Into<String>,
        step: u32,
        created_at: i64,
        agent: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            message_id: message_id.into(),
            step,
            created_at,
            agent: agent.into(),
            cost: 0.0,
            context_limit: None,
        }
    }

    /// Attach the model-derived cost for the step-finish record.
    #[must_use]
    pub fn with_cost(mut self, cost: f64) -> Self {
        self.cost = cost;
        self
    }

    /// Attach the model context ceiling used by this step.
    #[must_use]
    pub fn with_context_limit(mut self, context_limit: u64) -> Self {
        self.context_limit = (context_limit > 0).then_some(context_limit);
        self
    }
}

/// Token accounting persisted on both the assistant message and step-finish part.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepUsage {
    /// Input tokens billed for the request.
    pub input: u64,
    /// Output tokens emitted by the provider.
    pub output: u64,
    /// Reasoning tokens when a provider reports them separately.
    pub reasoning: u64,
    /// Input tokens served from a provider cache.
    pub cache_read: u64,
    /// Input tokens written into a provider cache.
    pub cache_write: u64,
}

impl StepUsage {
    fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.reasoning)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

/// Snapshot difference emitted after a provider step completes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotPatch {
    /// Snapshot hash the changed files are relative to.
    pub hash: String,
    /// Files changed since the step-start snapshot.
    pub files: Vec<String>,
}

/// Side effects that depend on snapshot, summary, and model-window services.
///
/// Keeping this seam synchronous and interface-neutral lets the stream projector
/// retain deterministic persistence while the turn owner schedules summary work.
pub trait ProjectionEffects {
    /// Capture the current worktree tree hash, or `None` when snapshots are off.
    fn track_snapshot(&mut self) -> Option<String>;

    /// Compute files changed since `snapshot`.
    fn patch(&mut self, snapshot: &str) -> Option<SnapshotPatch>;

    /// Schedule the post-step session summary check.
    fn trigger_summary(&mut self, session_id: &str, message_id: &str);

    /// Return whether this step's usage exceeds the active model window.
    fn is_overflow(&mut self, usage: &StepUsage) -> bool;
}

/// Snapshot/summary implementation for callers that have not wired those services.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopProjectionEffects;

impl ProjectionEffects for NoopProjectionEffects {
    fn track_snapshot(&mut self) -> Option<String> {
        None
    }

    fn patch(&mut self, _snapshot: &str) -> Option<SnapshotPatch> {
        None
    }

    fn trigger_summary(&mut self, _session_id: &str, _message_id: &str) {}

    fn is_overflow(&mut self, _usage: &StepUsage) -> bool {
        false
    }
}

/// Measured persistence work performed by one stream projection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectionStats {
    /// Text/reasoning upserts caused by batch or terminal flushes.
    pub delta_writes: u64,
    /// All part/message upserts and rollback deletes.
    pub total_writes: u64,
}

/// Terminal decisions produced while projecting a step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectionOutcome {
    /// Whether the completed usage crossed the model-window check.
    pub needs_compaction: bool,
}

/// A classified projection failure.
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    /// A tool start arrived while another tool input was still open.
    #[error("tool `{active_id}` is still receiving input when `{next_id}` starts")]
    NestedToolUse {
        /// Currently active provider tool id.
        active_id: String,
        /// Newly received provider tool id.
        next_id: String,
    },
    /// A tool-input end arrived without a matching start.
    #[error("ToolUseEnd arrived without ToolUseStart")]
    ToolUseEndWithoutStart,
    /// An event arrived after the projector had already terminated.
    #[error("stream projection already finished")]
    AlreadyFinished,
    /// A provider payload exceeded a stream limit.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// SQLite or record decoding failed.
    #[error(transparent)]
    Database(#[from] DbError),
}

#[derive(Debug)]
struct TextBuffer {
    id: String,
    text: String,
    dirty: bool,
}

#[derive(Debug)]
struct ReasoningBuffer {
    id: String,
    text: String,
    signature: String,
    metadata: Map<String, Value>,
    dirty: bool,
}

#[derive(Debug)]
struct ActiveTool {
    call_id: String,
    name: String,
    raw_input: String,
    signature: Option<ThoughtSignature>,
    started_at: i64,
}

#[derive(Debug, Clone)]
struct StoredTool {
    part_id: String,
    call_id: String,
    name: String,
    raw_input: String,
    input: Value,
    state: Value,
    signature: Option<ThoughtSignature>,
    started_at: i64,
}

/// Stateful projection of one provider stream onto a persisted assistant message.
pub struct StreamProjector<'connection, 'effects, Effects>
where
    Effects: ProjectionEffects,
{
    connection: &'connection Connection,
    context: ProjectionContext,
    effects: &'effects mut Effects,
    snapshot_before: Option<String>,
    usage: StepUsage,
    accounting: Option<PromptAccounting>,
    text: Option<TextBuffer>,
    reasoning: Option<ReasoningBuffer>,
    tools: HashMap<String, StoredTool>,
    active_tool: Option<ActiveTool>,
    tool_input_limit: usize,
    last_tool_id: Option<String>,
    attempt_part_ids: Vec<String>,
    dirty_delta_bytes: usize,
    next_part_sequence: u32,
    stats: ProjectionStats,
    outcome: ProjectionOutcome,
    finished: bool,
}

impl<'connection, 'effects, Effects> StreamProjector<'connection, 'effects, Effects>
where
    Effects: ProjectionEffects,
{
    /// Start a step and persist its step-start marker.
    pub fn start(
        connection: &'connection Connection,
        context: ProjectionContext,
        effects: &'effects mut Effects,
    ) -> Result<Self, ProjectionError> {
        let snapshot_before = effects.track_snapshot();
        let mut projector = Self {
            connection,
            context,
            effects,
            snapshot_before,
            usage: StepUsage::default(),
            accounting: None,
            text: None,
            reasoning: None,
            tools: HashMap::new(),
            active_tool: None,
            tool_input_limit: StreamLimits::from_environment().max_tool_input_bytes(),
            last_tool_id: None,
            attempt_part_ids: Vec::new(),
            dirty_delta_bytes: 0,
            next_part_sequence: 0,
            stats: ProjectionStats::default(),
            outcome: ProjectionOutcome::default(),
            finished: false,
        };
        projector.persist_step_start()?;
        Ok(projector)
    }

    /// Current measured write counts.
    #[must_use]
    pub const fn stats(&self) -> ProjectionStats {
        self.stats
    }

    /// Current terminal decisions.
    #[must_use]
    pub const fn outcome(&self) -> ProjectionOutcome {
        self.outcome
    }

    /// Apply one provider-neutral event.
    pub fn apply(&mut self, event: StreamEvent) -> Result<(), ProjectionError> {
        if self.finished {
            return Err(ProjectionError::AlreadyFinished);
        }
        match event {
            StreamEvent::TextDelta(delta) => self.push_text(&delta)?,
            StreamEvent::ToolUseStart { id, name } => self.start_tool(id, name)?,
            StreamEvent::ToolInputDelta(delta) => {
                if let Some(tool) = &mut self.active_tool {
                    append_tool_input(
                        &mut tool.raw_input,
                        &delta,
                        "stream-projector",
                        &self.context.message_id,
                        self.tool_input_limit,
                    )?;
                }
            }
            StreamEvent::ToolUseEnd => self.finish_active_tool()?,
            StreamEvent::ToolUseSignature(signature) => self.attach_tool_signature(signature)?,
            StreamEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => self.complete_provider_tool(tool_use_id, content, is_error)?,
            StreamEvent::GeneratedImage {
                id,
                path,
                metadata_path,
                output_format,
                revised_prompt,
            } => self.persist_generated_image(
                id,
                path,
                metadata_path,
                output_format,
                revised_prompt,
            )?,
            StreamEvent::ReasoningStart => self.start_reasoning()?,
            StreamEvent::ReasoningDelta(delta) => self.push_reasoning(&delta)?,
            StreamEvent::ReasoningSignatureDelta(delta) => {
                if let Some(reasoning) = &mut self.reasoning {
                    reasoning.signature.push_str(&delta);
                    reasoning.dirty = true;
                    self.dirty_delta_bytes = self.dirty_delta_bytes.saturating_add(delta.len());
                    self.flush_if_batch_full()?;
                }
            }
            StreamEvent::ProviderReasoningItem {
                id,
                summary,
                encrypted_content,
                status,
            } => self.persist_provider_reasoning(id, summary, encrypted_content, status)?,
            StreamEvent::ReasoningEnd => self.finish_reasoning(None)?,
            StreamEvent::ReasoningDone { duration_secs } => {
                self.finish_reasoning(Some(duration_secs))?;
            }
            StreamEvent::MessageEnd { stop_reason } => self.finish_step(stop_reason)?,
            StreamEvent::RetryRollback { attempt, max } => self.rollback(attempt, max)?,
            StreamEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_write_input_tokens,
                accounting,
            } => {
                if let Some(value) = input_tokens {
                    self.usage.input = value;
                }
                if let Some(value) = output_tokens {
                    self.usage.output = value;
                }
                if let Some(value) = cache_read_input_tokens {
                    self.usage.cache_read = value;
                }
                if let Some(value) = cache_write_input_tokens {
                    self.usage.cache_write = value;
                }
                self.accounting = Some(accounting);
            }
            StreamEvent::Compaction {
                trigger,
                pre_tokens,
                openai_encrypted_content,
            } => self.persist_compaction(trigger, pre_tokens, openai_encrypted_content)?,
            StreamEvent::NativeToolCall {
                request_id,
                tool_name,
                input,
            } => self.persist_native_tool(request_id, tool_name, input)?,
            StreamEvent::ConnectionType { .. }
            | StreamEvent::ConnectionPhase { .. }
            | StreamEvent::StatusDetail { .. }
            | StreamEvent::Error { .. }
            | StreamEvent::SessionId(_)
            | StreamEvent::UpstreamProvider { .. } => {}
        }
        Ok(())
    }

    /// Close an unexpectedly truncated stream without parsing partial tool JSON
    /// as if it were complete.
    pub fn finish_incomplete(&mut self, error: &str) -> Result<(), ProjectionError> {
        if self.finished {
            return Err(ProjectionError::AlreadyFinished);
        }
        if let Some(tool) = self.active_tool.take() {
            self.persist_tool_error(tool, error)?;
        }
        self.finish_reasoning(None)?;
        self.finish_text()?;
        self.finished = true;
        Ok(())
    }

    fn push_text(&mut self, delta: &str) -> Result<(), ProjectionError> {
        if self.text.is_none() {
            let id = self.next_part_id("text");
            self.text = Some(TextBuffer {
                id,
                text: String::new(),
                dirty: false,
            });
        }
        let text = self.text.as_mut().expect("text buffer was initialized");
        text.text.push_str(delta);
        text.dirty = true;
        self.dirty_delta_bytes = self.dirty_delta_bytes.saturating_add(delta.len());
        self.flush_if_batch_full()
    }

    fn start_reasoning(&mut self) -> Result<(), ProjectionError> {
        if self.reasoning.is_some() {
            self.finish_reasoning(None)?;
        }
        let id = self.next_part_id("reasoning");
        self.reasoning = Some(ReasoningBuffer {
            id,
            text: String::new(),
            signature: String::new(),
            metadata: Map::new(),
            dirty: true,
        });
        Ok(())
    }

    fn push_reasoning(&mut self, delta: &str) -> Result<(), ProjectionError> {
        let Some(reasoning) = &mut self.reasoning else {
            return Ok(());
        };
        reasoning.text.push_str(delta);
        reasoning.dirty = true;
        self.dirty_delta_bytes = self.dirty_delta_bytes.saturating_add(delta.len());
        self.flush_if_batch_full()
    }

    fn flush_if_batch_full(&mut self) -> Result<(), ProjectionError> {
        if self.dirty_delta_bytes < DELTA_BATCH_BYTES {
            return Ok(());
        }
        self.flush_dirty_text(false)?;
        self.flush_dirty_reasoning(false)?;
        self.dirty_delta_bytes = 0;
        Ok(())
    }

    fn finish_text(&mut self) -> Result<(), ProjectionError> {
        self.flush_dirty_text(true)
    }

    fn flush_dirty_text(&mut self, terminal: bool) -> Result<(), ProjectionError> {
        let Some(text) = &mut self.text else {
            return Ok(());
        };
        if !text.dirty && !terminal {
            return Ok(());
        }
        let mut time = json!({ "start": self.context.created_at });
        if terminal {
            time["end"] = Value::from(now_millis());
        }
        let id = text.id.clone();
        let payload = json!({
            "id": id,
            "sessionID": self.context.session_id,
            "messageID": self.context.message_id,
            "type": "text",
            "text": text.text,
            "time": time,
        });
        text.dirty = false;
        self.persist_attempt_part(payload, self.context.created_at, true)
    }

    fn finish_reasoning(&mut self, duration_secs: Option<f64>) -> Result<(), ProjectionError> {
        let Some(reasoning) = &mut self.reasoning else {
            return Ok(());
        };
        if let Some(duration) = duration_secs {
            reasoning
                .metadata
                .insert("durationSecs".to_owned(), Value::from(duration));
        }
        if !reasoning.signature.is_empty() {
            reasoning.metadata.insert(
                "signature".to_owned(),
                Value::String(reasoning.signature.clone()),
            );
        }
        reasoning.dirty = true;
        self.flush_dirty_reasoning(true)?;
        self.reasoning = None;
        Ok(())
    }

    fn flush_dirty_reasoning(&mut self, terminal: bool) -> Result<(), ProjectionError> {
        let Some(reasoning) = &mut self.reasoning else {
            return Ok(());
        };
        if !reasoning.dirty && !terminal {
            return Ok(());
        }
        let end = terminal.then(now_millis);
        let mut time = json!({ "start": self.context.created_at });
        if let Some(end) = end {
            time["end"] = Value::from(end);
        }
        let id = reasoning.id.clone();
        let payload = json!({
            "id": id,
            "sessionID": self.context.session_id,
            "messageID": self.context.message_id,
            "type": "reasoning",
            "text": reasoning.text,
            "metadata": reasoning.metadata,
            "time": time,
        });
        reasoning.dirty = false;
        self.persist_attempt_part(payload, self.context.created_at, true)
    }

    fn start_tool(&mut self, call_id: String, name: String) -> Result<(), ProjectionError> {
        if let Some(active) = &self.active_tool {
            return Err(ProjectionError::NestedToolUse {
                active_id: active.call_id.clone(),
                next_id: call_id,
            });
        }
        self.active_tool = Some(ActiveTool {
            call_id,
            name,
            raw_input: String::new(),
            signature: None,
            started_at: now_millis(),
        });
        Ok(())
    }

    fn finish_active_tool(&mut self) -> Result<(), ProjectionError> {
        let tool = self
            .active_tool
            .take()
            .ok_or(ProjectionError::ToolUseEndWithoutStart)?;
        match parse_tool_input(&tool.raw_input) {
            Ok(input) => self.persist_pending_tool(tool, input),
            Err(error) => {
                let message = format!("Invalid streamed tool input for `{}`: {error}", tool.name);
                self.persist_tool_error(tool, &message)
            }
        }
    }

    fn persist_pending_tool(
        &mut self,
        tool: ActiveTool,
        input: Value,
    ) -> Result<(), ProjectionError> {
        let part_id = self.next_part_id("tool");
        let state = json!({
            "status": "pending",
            "input": input,
            "raw": tool.raw_input,
        });
        let stored = StoredTool {
            part_id,
            call_id: tool.call_id.clone(),
            name: tool.name,
            raw_input: tool.raw_input,
            input,
            state,
            signature: tool.signature,
            started_at: tool.started_at,
        };
        self.persist_stored_tool(&stored)?;
        self.last_tool_id = Some(tool.call_id.clone());
        self.tools.insert(tool.call_id, stored);
        Ok(())
    }

    fn persist_tool_error(&mut self, tool: ActiveTool, error: &str) -> Result<(), ProjectionError> {
        let part_id = self.next_part_id("tool");
        let input = json!({});
        let state = json!({
            "status": "error",
            "input": input,
            "raw": tool.raw_input,
            "error": error,
            "metadata": { "synthetic": true },
            "time": { "start": tool.started_at, "end": now_millis() },
        });
        let stored = StoredTool {
            part_id,
            call_id: tool.call_id.clone(),
            name: tool.name,
            raw_input: tool.raw_input,
            input,
            state,
            signature: tool.signature,
            started_at: tool.started_at,
        };
        self.persist_stored_tool(&stored)?;
        self.last_tool_id = Some(tool.call_id.clone());
        self.tools.insert(tool.call_id, stored);
        Ok(())
    }

    fn attach_tool_signature(
        &mut self,
        signature: ThoughtSignature,
    ) -> Result<(), ProjectionError> {
        if let Some(tool) = &mut self.active_tool {
            tool.signature = Some(signature);
            return Ok(());
        }
        let Some(call_id) = &self.last_tool_id else {
            return Ok(());
        };
        let Some(tool) = self.tools.get_mut(call_id) else {
            return Ok(());
        };
        tool.signature = Some(signature);
        let tool = tool.clone();
        self.persist_stored_tool(&tool)
    }

    fn complete_provider_tool(
        &mut self,
        call_id: String,
        content: String,
        is_error: bool,
    ) -> Result<(), ProjectionError> {
        if !self.tools.contains_key(&call_id) {
            let part_id = self.next_part_id("tool");
            self.tools.insert(
                call_id.clone(),
                StoredTool {
                    part_id,
                    call_id: call_id.clone(),
                    name: "provider".to_owned(),
                    raw_input: String::new(),
                    input: json!({}),
                    state: Value::Null,
                    signature: None,
                    started_at: now_millis(),
                },
            );
        }
        let tool = self
            .tools
            .get_mut(&call_id)
            .expect("provider tool was initialized");
        let end = now_millis();
        tool.state = if is_error {
            json!({
                "status": "error",
                "input": tool.input,
                "error": content,
                "metadata": { "providerExecuted": true },
                "time": { "start": tool.started_at, "end": end },
            })
        } else {
            json!({
                "status": "completed",
                "input": tool.input,
                "output": content,
                "title": tool.name,
                "metadata": { "providerExecuted": true },
                "time": { "start": tool.started_at, "end": end },
            })
        };
        let tool = tool.clone();
        self.last_tool_id = Some(call_id);
        self.persist_stored_tool(&tool)
    }

    fn persist_native_tool(
        &mut self,
        call_id: String,
        name: String,
        input: Value,
    ) -> Result<(), ProjectionError> {
        let raw_input = input.to_string();
        let started_at = now_millis();
        if !input.is_object() {
            return self.persist_tool_error(
                ActiveTool {
                    call_id,
                    name,
                    raw_input,
                    signature: None,
                    started_at,
                },
                "Native tool input must be a JSON object",
            );
        }
        self.persist_pending_tool(
            ActiveTool {
                call_id,
                name,
                raw_input,
                signature: None,
                started_at,
            },
            input,
        )
    }

    fn persist_stored_tool(&mut self, tool: &StoredTool) -> Result<(), ProjectionError> {
        let mut payload = json!({
            "id": tool.part_id,
            "sessionID": self.context.session_id,
            "messageID": self.context.message_id,
            "type": "tool",
            "callID": tool.call_id,
            "tool": tool.name,
            "state": tool.state,
            "raw": tool.raw_input,
        });
        if let Some(signature) = &tool.signature {
            payload["metadata"] = json!({ "thoughtSignature": signature.as_str() });
        }
        self.persist_attempt_part(payload, tool.started_at, false)
    }

    fn persist_provider_reasoning(
        &mut self,
        provider_id: String,
        summary: Vec<String>,
        encrypted_content: Option<String>,
        status: Option<String>,
    ) -> Result<(), ProjectionError> {
        let part_id = self.next_part_id("reasoning-native");
        let now = now_millis();
        self.persist_attempt_part(
            json!({
                "id": part_id,
                "sessionID": self.context.session_id,
                "messageID": self.context.message_id,
                "type": "reasoning",
                "text": summary.join("\n"),
                "metadata": {
                    "providerReasoning": {
                        "id": provider_id,
                        "summary": summary,
                        "encryptedContent": encrypted_content,
                        "status": status,
                    }
                },
                "time": { "start": now, "end": now },
            }),
            now,
            false,
        )
    }

    fn persist_generated_image(
        &mut self,
        provider_id: String,
        path: String,
        metadata_path: Option<String>,
        output_format: String,
        revised_prompt: Option<String>,
    ) -> Result<(), ProjectionError> {
        let part_id = self.next_part_id("file");
        let filename = std::path::Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        let mime = if output_format.contains('/') {
            output_format.clone()
        } else {
            format!("image/{output_format}")
        };
        let mut payload = json!({
            "id": part_id,
            "sessionID": self.context.session_id,
            "messageID": self.context.message_id,
            "type": "file",
            "mime": mime,
            "url": path,
        });
        if let Some(filename) = filename {
            payload["filename"] = Value::String(filename);
        }
        payload["source"] = json!({
            "type": "resource",
            "clientName": provider_id,
            "uri": metadata_path.unwrap_or_else(|| "generated-image".to_owned()),
            "text": {
                "value": revised_prompt.unwrap_or_default(),
                "start": 0,
                "end": 0,
            },
        });
        self.persist_attempt_part(payload, now_millis(), false)
    }

    fn persist_compaction(
        &mut self,
        trigger: String,
        pre_tokens: Option<u64>,
        encrypted_content: Option<String>,
    ) -> Result<(), ProjectionError> {
        let part_id = self.next_part_id("compaction");
        self.persist_attempt_part(
            json!({
                "id": part_id,
                "sessionID": self.context.session_id,
                "messageID": self.context.message_id,
                "type": "compaction",
                "auto": true,
                "overflow": trigger == "overflow",
                "metadata": {
                    "trigger": trigger,
                    "preTokens": pre_tokens,
                    "openaiEncryptedContent": encrypted_content,
                },
            }),
            now_millis(),
            false,
        )
    }

    fn rollback(&mut self, attempt: u32, max: u32) -> Result<(), ProjectionError> {
        for part_id in std::mem::take(&mut self.attempt_part_ids) {
            self.connection
                .execute("DELETE FROM part WHERE id = ?1", [part_id.as_str()])
                .map_err(open::map_error)?;
            self.stats.total_writes = self.stats.total_writes.saturating_add(1);
        }
        self.text = None;
        self.reasoning = None;
        self.tools.clear();
        self.active_tool = None;
        self.last_tool_id = None;
        self.dirty_delta_bytes = 0;
        self.usage = StepUsage::default();
        self.accounting = None;

        let part_id = self.next_part_id("retry");
        let now = now_millis();
        self.persist_part(
            json!({
                "id": part_id,
                "sessionID": self.context.session_id,
                "messageID": self.context.message_id,
                "type": "retry",
                "attempt": attempt,
                "error": {
                    "name": "APIError",
                    "data": {
                        "message": format!("provider stream restarted attempt {attempt} of {max}"),
                        "isRetryable": true,
                        "metadata": { "attempt": attempt, "max": max },
                    }
                },
                "time": { "created": now },
            }),
            now,
            false,
        )
    }

    fn finish_step(&mut self, stop_reason: Option<FinishReason>) -> Result<(), ProjectionError> {
        if let Some(tool) = self.active_tool.take() {
            match parse_tool_input(&tool.raw_input) {
                Ok(input) => self.persist_pending_tool(tool, input)?,
                Err(error) => {
                    let message =
                        format!("Invalid streamed tool input for `{}`: {error}", tool.name);
                    self.persist_tool_error(tool, &message)?;
                }
            }
        }
        self.finish_reasoning(None)?;
        self.finish_text()?;

        let completed_snapshot = self.effects.track_snapshot();
        let reason = stop_reason.unwrap_or(FinishReason::Unknown);
        let now = now_millis();
        let part_id = self.next_part_id("step-finish");
        let mut finish_payload = json!({
            "id": part_id,
            "sessionID": self.context.session_id,
            "messageID": self.context.message_id,
            "type": "step-finish",
            "reason": reason.as_str(),
            "cost": self.context.cost,
            "tokens": {
                "total": self.usage.total(),
                "input": self.usage.input,
                "output": self.usage.output,
                "reasoning": self.usage.reasoning,
                "cache": {
                    "read": self.usage.cache_read,
                    "write": self.usage.cache_write,
                },
            },
        });
        if let Some(snapshot) = completed_snapshot {
            finish_payload["snapshot"] = Value::String(snapshot);
        }
        self.persist_part(finish_payload, now, false)?;
        self.update_assistant_message(reason, now)?;

        if let Some(snapshot) = self.snapshot_before.clone()
            && let Some(patch) = self.effects.patch(&snapshot)
            && !patch.files.is_empty()
        {
            let part_id = self.next_part_id("patch");
            self.persist_part(
                json!({
                    "id": part_id,
                    "sessionID": self.context.session_id,
                    "messageID": self.context.message_id,
                    "type": "patch",
                    "hash": patch.hash,
                    "files": patch.files,
                }),
                now,
                false,
            )?;
        }

        self.effects
            .trigger_summary(&self.context.session_id, &self.context.message_id);
        self.outcome.needs_compaction = self.effects.is_overflow(&self.usage);
        self.finished = true;
        Ok(())
    }

    fn update_assistant_message(
        &mut self,
        reason: FinishReason,
        now: i64,
    ) -> Result<(), ProjectionError> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(open::map_error)?;
        let store = MessageStore::new(&transaction);
        let mut message = store.message(&self.context.message_id)?;
        let previous = session::MessageUsage::from_data(&message.data);
        message
            .data
            .insert("finish".to_owned(), Value::String(reason.to_string()));
        let current_cost = message
            .data
            .get("cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        message.data.insert(
            "cost".to_owned(),
            Value::from(current_cost + self.context.cost),
        );
        let time = message
            .data
            .entry("time".to_owned())
            .or_insert_with(|| json!({ "created": self.context.created_at }));
        if let Some(time) = time.as_object_mut() {
            time.insert("completed".to_owned(), Value::from(now));
        }
        message.data.insert(
            "tokens".to_owned(),
            json!({
                "input": self.usage.input,
                "output": self.usage.output,
                "reasoning": self.usage.reasoning,
                "cache": {
                    "read": self.usage.cache_read,
                    "write": self.usage.cache_write,
                },
                "accounting": self.accounting.map(PromptAccounting::as_str),
            }),
        );
        store.put_message_at(&message, now)?;
        session::reconcile_usage(
            &transaction,
            &self.context.session_id,
            Some(previous),
            session::MessageUsage::from_data(&message.data),
            self.context
                .context_limit
                .and_then(|limit| i64::try_from(limit).ok()),
        )?;
        transaction.commit().map_err(open::map_error)?;
        self.stats.total_writes = self.stats.total_writes.saturating_add(1);
        Ok(())
    }

    fn persist_step_start(&mut self) -> Result<(), ProjectionError> {
        let part_id = self.next_part_id("step-start");
        let mut payload = json!({
            "id": part_id,
            "sessionID": self.context.session_id,
            "messageID": self.context.message_id,
            "type": "step-start",
        });
        if let Some(snapshot) = &self.snapshot_before {
            payload["snapshot"] = Value::String(snapshot.clone());
        }
        self.persist_part(payload, self.context.created_at, false)
    }

    fn persist_attempt_part(
        &mut self,
        payload: Value,
        created_at: i64,
        delta: bool,
    ) -> Result<(), ProjectionError> {
        let part_id = payload
            .get("id")
            .and_then(Value::as_str)
            .expect("projected parts always carry string ids")
            .to_owned();
        self.persist_part(payload, created_at, delta)?;
        if !self.attempt_part_ids.contains(&part_id) {
            self.attempt_part_ids.push(part_id);
        }
        Ok(())
    }

    fn persist_part(
        &mut self,
        payload: Value,
        created_at: i64,
        delta: bool,
    ) -> Result<(), ProjectionError> {
        let record = PartRecord::from_json(payload, created_at)?;
        MessageStore::new(self.connection).put_part_at(&record, now_millis())?;
        self.stats.total_writes = self.stats.total_writes.saturating_add(1);
        if delta {
            self.stats.delta_writes = self.stats.delta_writes.saturating_add(1);
        }
        Ok(())
    }

    fn next_part_id(&mut self, kind: &str) -> String {
        let sequence = self.next_part_sequence;
        self.next_part_sequence = self.next_part_sequence.saturating_add(1);
        format!(
            "prt_{}_stream_{:04}_{sequence:06}_{kind}",
            self.context.message_id, self.context.step
        )
    }
}

fn parse_tool_input(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(object)) => return Ok(Value::Object(object)),
        Ok(value) => {
            return Err(format!(
                "expected a JSON object, found {}",
                json_kind(&value)
            ));
        }
        Err(_) => {}
    }

    let repaired = without_trailing_commas(trimmed);
    match serde_json::from_str::<Value>(&repaired) {
        Ok(Value::Object(object)) => Ok(Value::Object(object)),
        Ok(value) => Err(format!(
            "expected a JSON object, found {}",
            json_kind(&value)
        )),
        Err(error) => Err(error.to_string()),
    }
}

fn without_trailing_commas(input: &str) -> String {
    let characters: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if character == '"' {
            in_string = true;
            output.push(character);
            index += 1;
            continue;
        }
        if character == ',' {
            let mut lookahead = index + 1;
            while lookahead < characters.len() && characters[lookahead].is_whitespace() {
                lookahead += 1;
            }
            if lookahead < characters.len() && matches!(characters[lookahead], '}' | ']') {
                index += 1;
                continue;
            }
        }
        output.push(character);
        index += 1;
    }
    output
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
