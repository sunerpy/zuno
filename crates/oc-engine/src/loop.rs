//! The single interface-neutral turn loop.
//!
//! CLI, TUI, HTTP, and ACP consume [`TurnEvent`] values from the same bounded
//! channel. This module neither renders nor writes output. The event channel uses
//! lossless backpressure: once its fixed capacity is full, the producer waits for
//! a consumer instead of dropping a transition or allocating without bound.
//!
//! The loop deliberately leaves narrow extension seams for the following engine
//! tasks. [`ToolDispatcher`] owns dispatch policy, while the checkpoint helpers in
//! this module perform only one terminal write per streamed part; batched live
//! projection can replace that terminal-only policy without creating another loop.
//! Retry, compaction, and the one-live-loop-per-session registry wrap the same
//! [`run_turn`] entry point rather than copying its state machine.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use oc_db::message::{
    MessageRecord, MessageRole, MessageStore, MessageWithParts, PartKind, PartRecord,
    created_after, now_millis,
};
use oc_db::{Connection, open, session};
use oc_error::{DbError, ProviderError};
use oc_llm::cache::{CacheViolation, DynamicContext, McpToolStatus, PreparedTurn, PromptCache};
use oc_llm::event::{
    FinishReason, Message, RequestContentBlock, Role, StreamEvent, ThoughtSignature,
};
use oc_llm::registry::{ApiSurface, ProviderRegistry, Spec};
use oc_tool::{ToolDefinition, ToolOutput};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::interrupt::InterruptSignal;

/// Maximum queued transitions before the turn applies lossless backpressure.
pub const TURN_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Text used to close an unanswered tool call before its transcript is replayed.
pub const INTERRUPTED_TOOL_RESULT: &str = "[Tool execution was interrupted]";

/// Producer half of the engine's bounded event channel.
///
/// Its field is private so callers cannot substitute an unbounded sender. Clone
/// this handle when more than one engine component publishes into the same turn.
#[derive(Debug, Clone)]
pub struct TurnEventSender {
    sender: mpsc::Sender<TurnEvent>,
}

impl TurnEventSender {
    /// Publish one event from a producer outside [`run_turn`].
    ///
    /// A host that drives turns has to be able to report a failure that happened
    /// before the loop could emit anything — resolving a session, persisting the
    /// prompt — and for an interface whose only window is the terminal this channel
    /// is the only place such a report can go. Fallible for the same reason the
    /// loop's own sends are: a consumer that has gone is not the producer's to fix.
    pub async fn publish(&self, event: TurnEvent) -> Result<(), TurnError> {
        self.send(event).await
    }

    async fn send(&self, event: TurnEvent) -> Result<(), TurnError> {
        self.sender
            .send(event)
            .await
            .map_err(|_| TurnError::EventConsumerClosed)
    }
}

/// Create the only event transport accepted by [`run_turn`].
#[must_use]
pub fn event_channel() -> (TurnEventSender, mpsc::Receiver<TurnEvent>) {
    let (sender, receiver) = mpsc::channel(TURN_EVENT_CHANNEL_CAPACITY);
    (TurnEventSender { sender }, receiver)
}

/// Every interface-observable transition of one turn.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    TurnStarted {
        session_id: String,
    },
    HistoryRepaired {
        repaired_tool_results: usize,
    },
    AgentResolved {
        step: u32,
        agent: String,
    },
    ModelResolved {
        step: u32,
        provider_id: String,
        model_id: String,
    },
    AssistantMessageCreated {
        step: u32,
        message_id: String,
    },
    ToolSnapshotLocked {
        step: u32,
        tool_ids: Vec<String>,
        rebuilt_for_late_mcp: bool,
    },
    ProviderRequestStarted {
        step: u32,
        message_count: usize,
    },
    Provider {
        step: u32,
        event: StreamEvent,
    },
    AssistantCheckpointed {
        step: u32,
        message_id: String,
        interrupted: bool,
    },
    ToolDispatchStarted {
        step: u32,
        call_id: String,
        name: String,
    },
    ToolDispatchCompleted {
        step: u32,
        call_id: String,
        name: String,
        title: String,
        output: String,
        is_error: bool,
    },
    ToolResultAppended {
        step: u32,
        call_id: String,
        is_error: bool,
    },
    StepCompleted {
        step: u32,
        finish_reason: Option<FinishReason>,
    },
    TurnCompleted {
        assistant_message_id: String,
        steps: u32,
    },
    TurnInterrupted {
        assistant_message_id: Option<String>,
        steps: u32,
    },
}

/// A normal terminal state of [`run_turn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed {
        assistant_message_id: String,
        steps: u32,
    },
    Interrupted {
        assistant_message_id: Option<String>,
        steps: u32,
    },
}

/// A classified failure of the turn spine.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("session `{session_id}` has no user message to answer")]
    NoUserMessage { session_id: String },
    #[error("user message `{message_id}` is missing required field `{field}`")]
    MissingUserField {
        message_id: String,
        field: &'static str,
    },
    #[error("agent `{agent}` is not available")]
    AgentNotFound { agent: String },
    #[error("model `{provider_id}/{model_id}` is not available")]
    ModelNotFound {
        provider_id: String,
        model_id: String,
    },
    #[error("agent `{agent}` exhausted its {max_steps}-step turn budget")]
    StepLimit { agent: String, max_steps: u32 },
    #[error("provider stream ended during step {step} without MessageEnd")]
    StreamEndedWithoutMessageEnd { step: u32 },
    #[error("provider emitted ToolUseStart before ending the active tool in step {step}")]
    NestedToolUse { step: u32 },
    #[error("provider emitted ToolUseEnd without ToolUseStart in step {step}")]
    ToolUseEndWithoutStart { step: u32 },
    #[error("the turn event consumer closed")]
    EventConsumerClosed,
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Cache(#[from] CacheViolation),
}

/// Agent data the loop needs after configuration resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub name: String,
    pub system_prompt: String,
    pub max_steps: u32,
}

impl ResolvedAgent {
    #[must_use]
    pub fn new(name: impl Into<String>, system_prompt: impl Into<String>, max_steps: u32) -> Self {
        Self {
            name: name.into(),
            system_prompt: system_prompt.into(),
            max_steps,
        }
    }
}

/// Model and provider-factory data selected for one step.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    pub provider: Spec,
    pub model_id: String,
    pub surface: ApiSurface,
}

impl ResolvedModel {
    #[must_use]
    pub fn new(provider: Spec, model_id: impl Into<String>, surface: ApiSurface) -> Self {
        Self {
            provider,
            model_id: model_id.into(),
            surface,
        }
    }
}

/// Configuration seam for `oc-agent` and the model catalog.
pub trait AgentModelResolver: Send + Sync {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent>;
    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel>;
}

/// The currently discoverable tool definitions and MCP discovery state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableTools {
    pub definitions: Vec<ToolDefinition>,
    pub mcp_status: McpToolStatus,
}

impl AvailableTools {
    #[must_use]
    pub fn new(definitions: Vec<ToolDefinition>, mcp_status: McpToolStatus) -> Self {
        Self {
            definitions,
            mcp_status,
        }
    }
}

/// One completed provider tool call ready for the dispatch seam.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub raw_input: String,
    pub input_error: Option<String>,
    pub thought_signature: Option<ThoughtSignature>,
}

/// Owned dispatch context so a future dispatcher may outlive a single poll.
#[derive(Debug, Clone)]
pub struct DispatchRequest {
    pub call: ToolCall,
    pub session_id: String,
    pub message_id: String,
    pub agent: String,
    pub available_tools: Arc<[ToolDefinition]>,
    pub interrupt: InterruptSignal,
}

/// A model-visible dispatch result. Dispatch failures are represented as error
/// outputs so the loop can append them and let the model recover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDispatchResult {
    pub output: ToolOutput,
    pub is_error: bool,
}

impl ToolDispatchResult {
    #[must_use]
    pub fn success(output: ToolOutput) -> Self {
        Self {
            output,
            is_error: false,
        }
    }

    #[must_use]
    pub fn error(output: ToolOutput) -> Self {
        Self {
            output,
            is_error: true,
        }
    }
}

/// Todo 33's single dispatch choke point.
#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    fn available_tools(&self) -> AvailableTools;
    async fn dispatch(&self, request: DispatchRequest) -> ToolDispatchResult;
}

/// Stable caller-owned identity and volatile suffix for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTurnRequest {
    pub session_id: String,
    pub turn_id: String,
    pub dynamic_context: DynamicContext,
}

impl RunTurnRequest {
    /// `turn_id` must be unique in the database because it forms persisted ids.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        dynamic_context: DynamicContext,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            dynamic_context,
        }
    }
}

/// Dependencies shared by every interface that invokes the same loop.
pub struct TurnContext<'a> {
    connection: &'a mut Connection,
    providers: &'a ProviderRegistry,
    resolver: &'a dyn AgentModelResolver,
    dispatcher: &'a dyn ToolDispatcher,
    interrupt: &'a InterruptSignal,
}

impl<'a> TurnContext<'a> {
    #[must_use]
    pub fn new(
        connection: &'a mut Connection,
        providers: &'a ProviderRegistry,
        resolver: &'a dyn AgentModelResolver,
        dispatcher: &'a dyn ToolDispatcher,
        interrupt: &'a InterruptSignal,
    ) -> Self {
        Self {
            connection,
            providers,
            resolver,
            dispatcher,
            interrupt,
        }
    }
}

#[derive(Debug)]
struct RequestedTurn {
    user_message_id: String,
    agent: String,
    provider_id: String,
    model_id: String,
}

#[derive(Debug, Default)]
struct ToolBuilder {
    id: String,
    name: String,
    raw_input: String,
    thought_signature: Option<ThoughtSignature>,
}

#[derive(Debug, Default)]
struct StepAccumulator {
    text: String,
    reasoning: String,
    reasoning_signature: String,
    provider_reasoning: Vec<RequestContentBlock>,
    calls: Vec<ToolCall>,
    active_tool: Option<ToolBuilder>,
    finish_reason: Option<FinishReason>,
    saw_message_end: bool,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
}

impl StepAccumulator {
    fn apply(&mut self, step: u32, event: &StreamEvent) -> Result<(), TurnError> {
        match event {
            StreamEvent::TextDelta(text) => self.text.push_str(text),
            StreamEvent::ToolUseStart { id, name } => {
                if self.active_tool.is_some() {
                    return Err(TurnError::NestedToolUse { step });
                }
                self.active_tool = Some(ToolBuilder {
                    id: id.clone(),
                    name: name.clone(),
                    ..ToolBuilder::default()
                });
            }
            StreamEvent::ToolInputDelta(delta) => {
                if let Some(tool) = &mut self.active_tool {
                    tool.raw_input.push_str(delta);
                }
            }
            StreamEvent::ToolUseEnd => self.finish_active_tool(step)?,
            StreamEvent::ToolUseSignature(signature) => {
                if let Some(tool) = &mut self.active_tool {
                    tool.thought_signature = Some(signature.clone());
                } else if let Some(tool) = self.calls.last_mut() {
                    tool.thought_signature = Some(signature.clone());
                }
            }
            StreamEvent::ToolResult { .. }
            | StreamEvent::GeneratedImage { .. }
            | StreamEvent::ReasoningDone { .. }
            | StreamEvent::ConnectionType { .. }
            | StreamEvent::ConnectionPhase { .. }
            | StreamEvent::StatusDetail { .. }
            | StreamEvent::Error { .. }
            | StreamEvent::SessionId(_)
            | StreamEvent::Compaction { .. }
            | StreamEvent::UpstreamProvider { .. } => {}
            StreamEvent::ReasoningStart => {
                self.reasoning.clear();
                self.reasoning_signature.clear();
            }
            StreamEvent::ReasoningDelta(delta) => self.reasoning.push_str(delta),
            StreamEvent::ReasoningSignatureDelta(delta) => {
                self.reasoning_signature.push_str(delta);
            }
            StreamEvent::ProviderReasoningItem {
                id,
                summary,
                encrypted_content,
                status,
            } => self
                .provider_reasoning
                .push(RequestContentBlock::ProviderEncryptedReasoning {
                    id: id.clone(),
                    summary: summary.clone(),
                    encrypted_content: encrypted_content.clone(),
                    status: status.clone(),
                }),
            StreamEvent::ReasoningEnd => {
                if !self.reasoning_signature.is_empty() {
                    self.provider_reasoning
                        .push(RequestContentBlock::SignedThinking {
                            thinking: self.reasoning.clone(),
                            signature: self.reasoning_signature.clone(),
                        });
                }
            }
            StreamEvent::MessageEnd { stop_reason } => {
                if self.active_tool.is_some() {
                    self.finish_active_tool(step)?;
                }
                self.finish_reason = *stop_reason;
                self.saw_message_end = true;
            }
            StreamEvent::RetryRollback { .. } => self.reset_generated(),
            StreamEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_write_input_tokens,
            } => {
                self.input_tokens = *input_tokens;
                self.output_tokens = *output_tokens;
                self.cache_read_input_tokens = *cache_read_input_tokens;
                self.cache_write_input_tokens = *cache_write_input_tokens;
            }
            StreamEvent::NativeToolCall {
                request_id,
                tool_name,
                input,
            } => self.calls.push(ToolCall {
                id: request_id.clone(),
                name: tool_name.clone(),
                input: input.clone(),
                raw_input: input.to_string(),
                input_error: None,
                thought_signature: None,
            }),
        }
        Ok(())
    }

    fn finish_active_tool(&mut self, step: u32) -> Result<(), TurnError> {
        let tool = self
            .active_tool
            .take()
            .ok_or(TurnError::ToolUseEndWithoutStart { step })?;
        let raw = tool.raw_input.trim();
        let (input, input_error) = if raw.is_empty() {
            (json!({}), None)
        } else {
            match serde_json::from_str(raw) {
                Ok(value) => (value, None),
                Err(error) => (
                    Value::String(tool.raw_input.clone()),
                    Some(error.to_string()),
                ),
            }
        };
        self.calls.push(ToolCall {
            id: tool.id,
            name: tool.name,
            input,
            raw_input: tool.raw_input,
            input_error,
            thought_signature: tool.thought_signature,
        });
        Ok(())
    }

    fn reset_generated(&mut self) {
        self.text.clear();
        self.reasoning.clear();
        self.reasoning_signature.clear();
        self.provider_reasoning.clear();
        self.calls.clear();
        self.active_tool = None;
        self.finish_reason = None;
        self.saw_message_end = false;
        self.input_tokens = None;
        self.output_tokens = None;
        self.cache_read_input_tokens = None;
        self.cache_write_input_tokens = None;
    }
}

/// Run one complete user turn. Every continuation after a tool result re-enters
/// this same loop and emits through the same bounded channel.
pub async fn run_turn(
    request: RunTurnRequest,
    context: TurnContext<'_>,
    events: TurnEventSender,
) -> Result<TurnOutcome, TurnError> {
    let session = session::get(context.connection, &request.session_id)?;
    touch_session(context.connection, &request.session_id)?;
    events
        .send(TurnEvent::TurnStarted {
            session_id: request.session_id.clone(),
        })
        .await?;

    let mut steps = 0_u32;
    let mut last_assistant_id = None;
    let mut prompt_cache: Option<PromptCache<ToolDefinition>> = None;

    loop {
        if context.interrupt.is_set() {
            let outcome = TurnOutcome::Interrupted {
                assistant_message_id: last_assistant_id.clone(),
                steps,
            };
            events
                .send(TurnEvent::TurnInterrupted {
                    assistant_message_id: last_assistant_id,
                    steps,
                })
                .await?;
            return Ok(outcome);
        }

        let mut history =
            MessageStore::new(context.connection).hydrate_session(&request.session_id)?;
        let repaired = repair_missing_tool_outputs(context.connection, &mut history)?;
        if repaired > 0 {
            events
                .send(TurnEvent::HistoryRepaired {
                    repaired_tool_results: repaired,
                })
                .await?;
        }

        let requested = requested_turn(&request.session_id, &history)?;
        let agent = context
            .resolver
            .resolve_agent(&requested.agent)
            .ok_or_else(|| TurnError::AgentNotFound {
                agent: requested.agent.clone(),
            })?;
        let model = context
            .resolver
            .resolve_model(&requested.provider_id, &requested.model_id)
            .ok_or_else(|| TurnError::ModelNotFound {
                provider_id: requested.provider_id.clone(),
                model_id: requested.model_id.clone(),
            })?;

        let step = steps.saturating_add(1);
        if step > agent.max_steps {
            return Err(TurnError::StepLimit {
                agent: agent.name,
                max_steps: agent.max_steps,
            });
        }
        steps = step;
        events
            .send(TurnEvent::AgentResolved {
                step,
                agent: agent.name.clone(),
            })
            .await?;
        events
            .send(TurnEvent::ModelResolved {
                step,
                provider_id: model.provider.provider.clone(),
                model_id: model.model_id.clone(),
            })
            .await?;

        let provider = context
            .providers
            .resolve(model.provider.clone())
            .map_err(ProviderError::from)?;
        let capabilities = provider.capabilities();
        let stable_history = provider_messages(&agent.system_prompt, &history);
        let mut assistant = assistant_message(
            &request, &session, &requested, &agent, &model, step, &history,
        )?;
        let assistant_id = assistant.id.clone();
        MessageStore::new(context.connection).put_message(&assistant)?;
        last_assistant_id = Some(assistant_id.clone());
        events
            .send(TurnEvent::AssistantMessageCreated {
                step,
                message_id: assistant_id.clone(),
            })
            .await?;

        let available = context.dispatcher.available_tools();
        let definitions = if capabilities.tool_calls {
            available.definitions
        } else {
            Vec::new()
        };
        let cache =
            prompt_cache.get_or_insert_with(|| PromptCache::new(agent.system_prompt.clone()));
        let prepared = cache.prepare_turn(
            &stable_history,
            request.dynamic_context.clone(),
            &definitions,
            available.mcp_status,
        )?;
        let locked_tools: Arc<[ToolDefinition]> = Arc::from(prepared.tools().to_vec());
        events
            .send(TurnEvent::ToolSnapshotLocked {
                step,
                tool_ids: locked_tools.iter().map(|tool| tool.id.clone()).collect(),
                rebuilt_for_late_mcp: prepared.rebuilt_tools(),
            })
            .await?;
        events
            .send(TurnEvent::ProviderRequestStarted {
                step,
                message_count: prepared.messages().len(),
            })
            .await?;

        let completion = completion_request(&model, &prepared);
        let mut stream = provider.stream(completion);
        let mut accumulator = StepAccumulator::default();
        let mut interrupted = false;

        loop {
            let next = tokio::select! {
                biased;
                _ = context.interrupt.notified() => {
                    interrupted = true;
                    None
                }
                event = stream.next() => event,
            };
            let Some(next) = next else {
                break;
            };
            let event = next?;
            accumulator.apply(step, &event)?;
            let ended = matches!(event, StreamEvent::MessageEnd { .. });
            events.send(TurnEvent::Provider { step, event }).await?;
            if ended {
                break;
            }
        }

        if interrupted {
            checkpoint_assistant(
                context.connection,
                &request,
                step,
                &mut assistant,
                &accumulator,
                true,
            )?;
            events
                .send(TurnEvent::AssistantCheckpointed {
                    step,
                    message_id: assistant_id.clone(),
                    interrupted: true,
                })
                .await?;
            events
                .send(TurnEvent::TurnInterrupted {
                    assistant_message_id: Some(assistant_id.clone()),
                    steps,
                })
                .await?;
            return Ok(TurnOutcome::Interrupted {
                assistant_message_id: Some(assistant_id),
                steps,
            });
        }

        if !accumulator.saw_message_end {
            checkpoint_assistant(
                context.connection,
                &request,
                step,
                &mut assistant,
                &accumulator,
                false,
            )?;
            return Err(TurnError::StreamEndedWithoutMessageEnd { step });
        }

        checkpoint_assistant(
            context.connection,
            &request,
            step,
            &mut assistant,
            &accumulator,
            false,
        )?;
        events
            .send(TurnEvent::AssistantCheckpointed {
                step,
                message_id: assistant_id.clone(),
                interrupted: false,
            })
            .await?;

        for (call_index, call) in accumulator.calls.iter().cloned().enumerate() {
            events
                .send(TurnEvent::ToolDispatchStarted {
                    step,
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                })
                .await?;
            let dispatch = context
                .dispatcher
                .dispatch(DispatchRequest {
                    call: call.clone(),
                    session_id: request.session_id.clone(),
                    message_id: assistant_id.clone(),
                    agent: agent.name.clone(),
                    available_tools: Arc::clone(&locked_tools),
                    interrupt: context.interrupt.clone(),
                })
                .await;
            persist_tool_result(
                context.connection,
                &request,
                step,
                call_index,
                &assistant_id,
                &call,
                &dispatch,
            )?;
            events
                .send(TurnEvent::ToolDispatchCompleted {
                    step,
                    call_id: call.id.clone(),
                    name: call.name,
                    title: dispatch.output.title.clone(),
                    output: dispatch.output.output.clone(),
                    is_error: dispatch.is_error,
                })
                .await?;
            events
                .send(TurnEvent::ToolResultAppended {
                    step,
                    call_id: call.id,
                    is_error: dispatch.is_error,
                })
                .await?;
        }

        events
            .send(TurnEvent::StepCompleted {
                step,
                finish_reason: accumulator.finish_reason,
            })
            .await?;

        if !accumulator.calls.is_empty() {
            continue;
        }

        events
            .send(TurnEvent::TurnCompleted {
                assistant_message_id: assistant_id.clone(),
                steps,
            })
            .await?;
        return Ok(TurnOutcome::Completed {
            assistant_message_id: assistant_id,
            steps,
        });
    }
}

fn touch_session(connection: &mut Connection, session_id: &str) -> Result<(), TurnError> {
    let transaction = connection.transaction().map_err(open::map_error)?;
    session::touch(&transaction, session_id)?;
    transaction.commit().map_err(open::map_error)?;
    Ok(())
}

fn requested_turn(
    session_id: &str,
    history: &[MessageWithParts],
) -> Result<RequestedTurn, TurnError> {
    let user = history
        .iter()
        .rev()
        .find(|message| message.info.role == MessageRole::User)
        .ok_or_else(|| TurnError::NoUserMessage {
            session_id: session_id.to_owned(),
        })?;
    let agent = required_string(&user.info, "agent")?;
    let model = user
        .info
        .data
        .get("model")
        .and_then(Value::as_object)
        .ok_or_else(|| TurnError::MissingUserField {
            message_id: user.info.id.clone(),
            field: "model",
        })?;
    let provider_id = model
        .get("providerID")
        .and_then(Value::as_str)
        .ok_or_else(|| TurnError::MissingUserField {
            message_id: user.info.id.clone(),
            field: "model.providerID",
        })?;
    let model_id = model
        .get("modelID")
        .and_then(Value::as_str)
        .ok_or_else(|| TurnError::MissingUserField {
            message_id: user.info.id.clone(),
            field: "model.modelID",
        })?;
    Ok(RequestedTurn {
        user_message_id: user.info.id.clone(),
        agent,
        provider_id: provider_id.to_owned(),
        model_id: model_id.to_owned(),
    })
}

fn required_string(record: &MessageRecord, field: &'static str) -> Result<String, TurnError> {
    record
        .data
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TurnError::MissingUserField {
            message_id: record.id.clone(),
            field,
        })
}

fn repair_missing_tool_outputs(
    connection: &Connection,
    history: &mut [MessageWithParts],
) -> Result<usize, TurnError> {
    let store = MessageStore::new(connection);
    let mut repaired = 0;
    for message in history {
        for part in &mut message.parts {
            if part.kind != PartKind::Tool {
                continue;
            }
            let Some(state) = part.data.get_mut("state").and_then(Value::as_object_mut) else {
                continue;
            };
            let status = state.get("status").and_then(Value::as_str);
            if matches!(status, Some("completed" | "error")) {
                continue;
            }
            state.insert("status".to_owned(), Value::String("error".to_owned()));
            state.insert(
                "error".to_owned(),
                Value::String(INTERRUPTED_TOOL_RESULT.to_owned()),
            );
            let mut metadata = Map::new();
            metadata.insert("interrupted".to_owned(), Value::Bool(true));
            state.insert("metadata".to_owned(), Value::Object(metadata));
            store.put_part(part)?;
            repaired += 1;
        }
    }
    Ok(repaired)
}

fn provider_messages(system_prompt: &str, history: &[MessageWithParts]) -> Vec<Message> {
    let mut messages = vec![Message::new(Role::System, system_prompt)];
    for message in history {
        match message.info.role {
            MessageRole::User => append_user_message(&mut messages, message),
            MessageRole::Assistant => append_assistant_message(&mut messages, message),
        }
    }
    messages
}

fn append_user_message(messages: &mut Vec<Message>, message: &MessageWithParts) {
    let mut content = Vec::new();
    for part in &message.parts {
        match part.kind {
            PartKind::Text => {
                if let Some(text) = part.data.get("text").and_then(Value::as_str) {
                    content.push(RequestContentBlock::Text {
                        text: text.to_owned(),
                    });
                }
            }
            PartKind::File => {
                let media_type = part.data.get("mime").and_then(Value::as_str);
                let data = part.data.get("data").and_then(Value::as_str);
                if let (Some(media_type), Some(data)) = (media_type, data) {
                    content.push(RequestContentBlock::Image {
                        media_type: media_type.to_owned(),
                        data: data.to_owned(),
                    });
                }
            }
            PartKind::Subtask
            | PartKind::Reasoning
            | PartKind::Tool
            | PartKind::StepStart
            | PartKind::StepFinish
            | PartKind::Snapshot
            | PartKind::Patch
            | PartKind::Agent
            | PartKind::Retry
            | PartKind::Compaction => {}
        }
    }
    if !content.is_empty() {
        messages.push(Message::from_content(Role::User, content));
    }
}

fn append_assistant_message(messages: &mut Vec<Message>, message: &MessageWithParts) {
    let mut assistant = Vec::new();
    let mut results = Vec::new();
    for part in &message.parts {
        match part.kind {
            PartKind::Text => {
                if let Some(text) = part.data.get("text").and_then(Value::as_str) {
                    assistant.push(RequestContentBlock::Text {
                        text: text.to_owned(),
                    });
                }
            }
            PartKind::Reasoning => append_reasoning(&mut assistant, part),
            PartKind::Tool => append_tool_pair(&mut assistant, &mut results, part),
            PartKind::Subtask
            | PartKind::File
            | PartKind::StepStart
            | PartKind::StepFinish
            | PartKind::Snapshot
            | PartKind::Patch
            | PartKind::Agent
            | PartKind::Retry
            | PartKind::Compaction => {}
        }
    }
    if !assistant.is_empty() {
        messages.push(Message::from_content(Role::Assistant, assistant));
    }
    if !results.is_empty() {
        messages.push(Message::from_content(Role::Tool, results));
    }
}

fn append_reasoning(content: &mut Vec<RequestContentBlock>, part: &PartRecord) {
    let thinking = part.data.get("text").and_then(Value::as_str);
    let signature = part
        .data
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("signature"))
        .and_then(Value::as_str);
    if let (Some(thinking), Some(signature)) = (thinking, signature) {
        content.push(RequestContentBlock::SignedThinking {
            thinking: thinking.to_owned(),
            signature: signature.to_owned(),
        });
    }
}

fn append_tool_pair(
    assistant: &mut Vec<RequestContentBlock>,
    results: &mut Vec<RequestContentBlock>,
    part: &PartRecord,
) {
    let Some(call_id) = part.data.get("callID").and_then(Value::as_str) else {
        return;
    };
    let Some(name) = part.data.get("tool").and_then(Value::as_str) else {
        return;
    };
    let Some(state) = part.data.get("state").and_then(Value::as_object) else {
        return;
    };
    let input = state.get("input").cloned().unwrap_or_else(|| json!({}));
    let thought_signature = part
        .data
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("thoughtSignature"))
        .and_then(Value::as_str)
        .map(ThoughtSignature::new);
    assistant.push(RequestContentBlock::ToolUse {
        id: call_id.to_owned(),
        name: name.to_owned(),
        input,
        thought_signature,
    });

    match state.get("status").and_then(Value::as_str) {
        Some("completed") => results.push(RequestContentBlock::ToolResult {
            tool_use_id: call_id.to_owned(),
            content: state
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            is_error: Some(false),
        }),
        Some("error") => results.push(RequestContentBlock::ToolResult {
            tool_use_id: call_id.to_owned(),
            content: state
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or(INTERRUPTED_TOOL_RESULT)
                .to_owned(),
            is_error: Some(true),
        }),
        Some(_) | None => {}
    }
}

/// The assistant record for one step, stamped so it sorts after `history`.
///
/// Deriving the stamp from the history rather than from the clock alone is what
/// makes the reply's position in the request a fact. `history` was just hydrated in
/// `(time_created, id)` order and the record built here is about to join it; when
/// the clock has not ticked since the prompt was persisted, an unclamped stamp ties
/// it, and the tie is broken by whichever random id sorts first. Losing that flip
/// puts the reply ahead of the prompt, changes the stable prefix between one step
/// and the next, and the append-only tracker rightly refuses the request.
fn assistant_message(
    request: &RunTurnRequest,
    session: &session::Session,
    requested: &RequestedTurn,
    agent: &ResolvedAgent,
    model: &ResolvedModel,
    step: u32,
    history: &[MessageWithParts],
) -> Result<MessageRecord, TurnError> {
    let created = created_after(
        now_millis(),
        history.iter().map(|entry| entry.info.time_created).max(),
    );
    MessageRecord::from_json(json!({
        "id": assistant_message_id(&request.turn_id, step),
        "sessionID": request.session_id,
        "role": "assistant",
        "time": { "created": created },
        "parentID": requested.user_message_id,
        "modelID": model.model_id,
        "providerID": model.provider.provider,
        "mode": agent.name,
        "agent": agent.name,
        "path": { "cwd": session.directory, "root": session.directory },
        "cost": 0.0,
        "tokens": {
            "input": 0,
            "output": 0,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        }
    }))
    .map_err(TurnError::from)
}

fn completion_request(
    model: &ResolvedModel,
    prepared: &PreparedTurn<ToolDefinition>,
) -> oc_llm::registry::CompletionRequest {
    oc_llm::registry::CompletionRequest {
        model_id: model.model_id.clone(),
        surface: model.surface,
        messages: prepared.messages().to_vec(),
        tools: prepared
            .tools()
            .iter()
            .map(|tool| oc_llm::registry::ToolSchema {
                name: tool.id.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect(),
    }
}

fn checkpoint_assistant(
    connection: &Connection,
    request: &RunTurnRequest,
    step: u32,
    assistant: &mut MessageRecord,
    accumulator: &StepAccumulator,
    interrupted: bool,
) -> Result<(), TurnError> {
    let completed = now_millis();
    let time = assistant
        .data
        .entry("time".to_owned())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("assistant time is created as an object");
    time.insert("completed".to_owned(), Value::from(completed));
    if let Some(reason) = accumulator.finish_reason {
        assistant
            .data
            .insert("finish".to_owned(), Value::String(reason.to_string()));
    }
    if interrupted {
        assistant.data.insert(
            "error".to_owned(),
            json!({
                "name": "AbortError",
                "data": { "message": "The operation was aborted." }
            }),
        );
    }
    update_usage(&mut assistant.data, accumulator);

    let store = MessageStore::new(connection);
    store.put_message_at(assistant, completed)?;
    if !accumulator.text.is_empty() {
        let text = PartRecord::from_json(
            json!({
                "id": text_part_id(&request.turn_id, step),
                "sessionID": request.session_id,
                "messageID": assistant.id,
                "type": "text",
                "text": accumulator.text,
                "time": { "start": assistant.time_created, "end": completed }
            }),
            assistant.time_created,
        )?;
        store.put_part_at(&text, completed)?;
    }
    if !accumulator.reasoning.is_empty() {
        let mut metadata = Map::new();
        if !accumulator.reasoning_signature.is_empty() {
            metadata.insert(
                "signature".to_owned(),
                Value::String(accumulator.reasoning_signature.clone()),
            );
        }
        let reasoning = PartRecord::from_json(
            json!({
                "id": reasoning_part_id(&request.turn_id, step),
                "sessionID": request.session_id,
                "messageID": assistant.id,
                "type": "reasoning",
                "text": accumulator.reasoning,
                "metadata": metadata,
                "time": { "start": assistant.time_created, "end": completed }
            }),
            assistant.time_created,
        )?;
        store.put_part_at(&reasoning, completed)?;
    }
    for (call_index, call) in accumulator.calls.iter().enumerate() {
        let tool = pending_tool_part(request, step, call_index, &assistant.id, call)?;
        store.put_part_at(&tool, completed)?;
    }
    Ok(())
}

fn update_usage(data: &mut Map<String, Value>, accumulator: &StepAccumulator) {
    let Some(tokens) = data.get_mut("tokens").and_then(Value::as_object_mut) else {
        return;
    };
    tokens.insert(
        "input".to_owned(),
        Value::from(accumulator.input_tokens.unwrap_or(0)),
    );
    tokens.insert(
        "output".to_owned(),
        Value::from(accumulator.output_tokens.unwrap_or(0)),
    );
    if let Some(cache) = tokens.get_mut("cache").and_then(Value::as_object_mut) {
        cache.insert(
            "read".to_owned(),
            Value::from(accumulator.cache_read_input_tokens.unwrap_or(0)),
        );
        cache.insert(
            "write".to_owned(),
            Value::from(accumulator.cache_write_input_tokens.unwrap_or(0)),
        );
    }
}

fn pending_tool_part(
    request: &RunTurnRequest,
    step: u32,
    call_index: usize,
    message_id: &str,
    call: &ToolCall,
) -> Result<PartRecord, TurnError> {
    let mut payload = json!({
        "id": tool_part_id(&request.turn_id, step, call_index),
        "sessionID": request.session_id,
        "messageID": message_id,
        "type": "tool",
        "callID": call.id,
        "tool": call.name,
        "state": {
            "status": "pending",
            "input": call.input,
            "raw": call.raw_input
        }
    });
    if let Some(signature) = &call.thought_signature {
        payload["metadata"] = json!({ "thoughtSignature": signature.as_str() });
    }
    PartRecord::from_json(payload, now_millis()).map_err(TurnError::from)
}

fn persist_tool_result(
    connection: &Connection,
    request: &RunTurnRequest,
    step: u32,
    call_index: usize,
    message_id: &str,
    call: &ToolCall,
    dispatch: &ToolDispatchResult,
) -> Result<(), TurnError> {
    let status = if dispatch.is_error {
        "error"
    } else {
        "completed"
    };
    let mut state = json!({
        "status": status,
        "input": call.input,
        "raw": call.raw_input,
        "title": dispatch.output.title,
        "metadata": dispatch.output.metadata,
        "attachments": dispatch.output.attachments,
        "time": { "start": now_millis(), "end": now_millis() }
    });
    if dispatch.is_error {
        state["error"] = Value::String(dispatch.output.output.clone());
    } else {
        state["output"] = Value::String(dispatch.output.output.clone());
    }
    let mut payload = json!({
        "id": tool_part_id(&request.turn_id, step, call_index),
        "sessionID": request.session_id,
        "messageID": message_id,
        "type": "tool",
        "callID": call.id,
        "tool": call.name,
        "state": state
    });
    if let Some(signature) = &call.thought_signature {
        payload["metadata"] = json!({ "thoughtSignature": signature.as_str() });
    }
    let now = now_millis();
    let part = PartRecord::from_json(payload, now)?;
    MessageStore::new(connection).put_part_at(&part, now)?;
    Ok(())
}

fn assistant_message_id(turn_id: &str, step: u32) -> String {
    format!("msg_{turn_id}_{step:04}")
}

fn text_part_id(turn_id: &str, step: u32) -> String {
    format!("prt_{turn_id}_{step:04}_text")
}

fn reasoning_part_id(turn_id: &str, step: u32) -> String {
    format!("prt_{turn_id}_{step:04}_reasoning")
}

fn tool_part_id(turn_id: &str, step: u32, call_index: usize) -> String {
    format!("prt_{turn_id}_{step:04}_tool_{call_index:04}")
}
