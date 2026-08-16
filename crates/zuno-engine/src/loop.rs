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

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use zuno_db::message::{
    MessageRecord, MessageRole, MessageStore, MessageWithParts, PartKind, PartRecord,
    created_after, now_millis,
};
use zuno_db::{Connection, open, session};
use zuno_error::{DbError, ProviderError};
use zuno_llm::cache::{CacheViolation, DynamicContext, McpToolStatus, PreparedTurn, PromptCache};
use zuno_llm::event::{
    FinishReason, Message, RequestContentBlock, Role, StreamEvent, ThoughtSignature,
};
use zuno_llm::registry::{ApiSurface, ProviderRegistry, Spec};
use zuno_tool::{ToolDefinition, ToolOutput};

use crate::hooks::{HookMessageWithParts, NoopHooks, RequestHookInput, TurnHooks};
use crate::interrupt::InterruptSignal;
use crate::retry::{
    PROVIDER_RETRY_MAX_ATTEMPTS, ProviderRetryError, ProviderRetryPolicy, retry_provider,
};

/// Maximum queued transitions before the turn applies lossless backpressure.
pub const TURN_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Text used to close an unanswered tool call before its transcript is replayed.
pub const INTERRUPTED_TOOL_RESULT: &str = "[Tool execution was interrupted]";

/// Producer half of the engine's bounded event channel.
///
/// Its field is private so callers cannot substitute an unbounded sender. Clone
/// this handle when more than one engine component publishes into the same turn.
#[derive(Clone)]
pub struct TurnEventSender {
    sender: mpsc::Sender<TurnEvent>,
    hooks: Arc<dyn TurnHooks>,
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
        self.hooks.event(&event).await.map_err(TurnError::Hook)?;
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
    (
        TurnEventSender {
            sender,
            hooks: Arc::new(NoopHooks),
        },
        receiver,
    )
}

impl TurnEventSender {
    /// Route every event through `hooks` before the interface observes it.
    #[must_use]
    pub fn with_hooks(mut self, hooks: Arc<dyn TurnHooks>) -> Self {
        self.hooks = hooks;
        self
    }
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
    #[error("provider `{provider_id}` returned an empty assistant response during step {step}")]
    EmptyAssistantMessage { provider_id: String, step: u32 },
    #[error("provider emitted ToolUseStart before ending the active tool in step {step}")]
    NestedToolUse { step: u32 },
    #[error("provider emitted ToolUseEnd without ToolUseStart in step {step}")]
    ToolUseEndWithoutStart { step: u32 },
    #[error("the turn event consumer closed")]
    EventConsumerClosed,
    #[error("plugin hook failed: {0}")]
    Hook(String),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("provider retry deadline exceeded on attempt {attempt} after {elapsed:?}")]
    ProviderRetryDeadlineExceeded { attempt: u32, elapsed: Duration },
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
    /// User-visible provider id from the resolved catalog.
    pub catalog_provider_id: String,
    /// User-visible model id from the resolved catalog.
    pub catalog_model_id: String,
    /// Model id sent on the provider wire.
    pub model_id: String,
    pub surface: ApiSurface,
}

impl ResolvedModel {
    #[must_use]
    pub fn new(provider: Spec, model_id: impl Into<String>, surface: ApiSurface) -> Self {
        let model_id = model_id.into();
        Self {
            catalog_provider_id: provider.provider.clone(),
            catalog_model_id: model_id.clone(),
            provider,
            model_id,
            surface,
        }
    }

    /// Attach the catalog identity when it differs from the transport factory or wire id.
    #[must_use]
    pub fn with_catalog_identity(
        mut self,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        self.catalog_provider_id = provider_id.into();
        self.catalog_model_id = model_id.into();
        self
    }
}

/// Configuration seam for `zuno-agent` and the model catalog.
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
    hooks: Arc<dyn TurnHooks>,
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
            hooks: Arc::new(NoopHooks),
        }
    }

    #[must_use]
    pub fn with_hooks(mut self, hooks: Arc<dyn TurnHooks>) -> Self {
        self.hooks = hooks;
        self
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

    fn has_generated_output(&self) -> bool {
        !self.text.is_empty()
            || !self.reasoning.is_empty()
            || !self.provider_reasoning.is_empty()
            || !self.calls.is_empty()
            || self.active_tool.is_some()
    }

    fn has_assistant_parts(&self) -> bool {
        !self.text.is_empty() || !self.reasoning.is_empty() || !self.calls.is_empty()
    }
}

/// Run one complete user turn. Every continuation after a tool result re-enters
/// this same loop and emits through the same bounded channel.
pub async fn run_turn(
    request: RunTurnRequest,
    context: TurnContext<'_>,
    events: TurnEventSender,
) -> Result<TurnOutcome, TurnError> {
    let events = events.with_hooks(Arc::clone(&context.hooks));
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

        let repaired = repair_missing_tool_outputs(context.connection, &request.session_id)?;
        if repaired > 0 {
            events
                .send(TurnEvent::HistoryRepaired {
                    repaired_tool_results: repaired,
                })
                .await?;
        }

        let history = hydrate_retained_history(context.connection, &request.session_id)?;
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
                provider_id: model.catalog_provider_id.clone(),
                model_id: model.catalog_model_id.clone(),
            })
            .await?;

        let provider = context
            .providers
            .resolve(model.provider.clone())
            .map_err(ProviderError::from)?;
        let capabilities = provider.capabilities();
        let mut assistant = assistant_message(
            &request, &session, &requested, &agent, &model, step, &history,
        )?;
        let mut system = vec![agent.system_prompt.clone()];
        context
            .hooks
            .transform_system(&request.session_id, &model, &mut system)
            .await
            .map_err(TurnError::Hook)?;
        let system_prompt = system.join("\n");
        let stable_history = if context.hooks.enabled() {
            let mut transformed = hook_messages(&history);
            context
                .hooks
                .transform_messages(&request.session_id, &mut transformed)
                .await
                .map_err(TurnError::Hook)?;
            let mut messages = vec![Message::new(Role::System, system_prompt.clone())];
            for message in transformed {
                append_transformed_message_owned(&mut messages, message);
            }
            messages
        } else {
            project_history_owned(&system_prompt, history)
        };
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
        let mut definitions = if capabilities.tool_calls {
            available.definitions
        } else {
            Vec::new()
        };
        for definition in &mut definitions {
            context
                .hooks
                .tool_definition(definition)
                .await
                .map_err(TurnError::Hook)?;
        }
        let cache = prompt_cache.get_or_insert_with(|| PromptCache::new(system_prompt));
        let prepared = cache.prepare_turn_owned(
            stable_history,
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

        let mut completion = completion_request(&model, prepared);
        let hook_message = completion
            .messages
            .last()
            .cloned()
            .unwrap_or_else(|| Message::new(Role::System, ""));
        context
            .hooks
            .prepare_request(
                RequestHookInput {
                    session_id: &request.session_id,
                    agent: &agent,
                    model: &model,
                    message: &hook_message,
                },
                &mut completion,
            )
            .await
            .map_err(TurnError::Hook)?;
        let accumulator = Arc::new(Mutex::new(StepAccumulator::default()));
        let policy = ProviderRetryPolicy::new(
            NonZeroU32::new(PROVIDER_RETRY_MAX_ATTEMPTS)
                .expect("provider retry maximum is non-zero"),
        );
        let attempt = retry_provider(
            policy,
            |_| {
                let provider = Arc::clone(&provider);
                let completion = completion.clone();
                let interrupt = context.interrupt.clone();
                let events = events.clone();
                let accumulator = Arc::clone(&accumulator);
                async move {
                    let mut stream = provider.stream(completion);
                    loop {
                        let next = tokio::select! {
                            biased;
                            _ = interrupt.notified() => return Ok(Ok(true)),
                            event = stream.next() => event,
                        };
                        let Some(next) = next else {
                            return Ok(Ok(false));
                        };
                        let event = match next {
                            Ok(event) => event,
                            Err(error @ ProviderError::Transient { status: None, .. })
                                if accumulator
                                    .lock()
                                    .expect("step accumulator lock")
                                    .has_generated_output() =>
                            {
                                return Ok(Err(TurnError::Provider(error)));
                            }
                            Err(error) => return Err(error),
                        };
                        let ended = matches!(event, StreamEvent::MessageEnd { .. });
                        let apply = accumulator
                            .lock()
                            .expect("step accumulator lock")
                            .apply(step, &event);
                        if let Err(error) = apply {
                            return Ok(Err(error));
                        }
                        if let Err(error) = events.send(TurnEvent::Provider { step, event }).await {
                            return Ok(Err(error));
                        }
                        if ended {
                            return Ok(Ok(false));
                        }
                    }
                }
            },
            |event| {
                let events = events.clone();
                let accumulator = Arc::clone(&accumulator);
                async move {
                    accumulator
                        .lock()
                        .expect("step accumulator lock")
                        .apply(step, &event)?;
                    events.send(TurnEvent::Provider { step, event }).await
                }
            },
        )
        .await;
        let interrupted = match attempt {
            Ok(result) => result?,
            Err(ProviderRetryError::Provider(error))
            | Err(ProviderRetryError::AttemptsExhausted { source: error, .. }) => {
                return Err(TurnError::Provider(error));
            }
            Err(ProviderRetryError::DeadlineExceeded { attempt, elapsed }) => {
                return Err(TurnError::ProviderRetryDeadlineExceeded { attempt, elapsed });
            }
            Err(ProviderRetryError::RollbackEmission { source }) => return Err(*source),
        };
        let mut accumulator = {
            let mut accumulator = accumulator.lock().expect("step accumulator lock");
            std::mem::take(&mut *accumulator)
        };

        if !accumulator.text.is_empty() {
            context
                .hooks
                .text_complete(
                    &request.session_id,
                    &assistant_id,
                    &text_part_id(&request.turn_id, step),
                    &mut accumulator.text,
                )
                .await
                .map_err(TurnError::Hook)?;
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
        if !accumulator.has_assistant_parts() {
            return Err(TurnError::EmptyAssistantMessage {
                provider_id: model.catalog_provider_id,
                step,
            });
        }
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
        .find(|message| message.info.role == MessageRole::User && !is_compaction_marker(message))
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
    session_id: &str,
) -> Result<usize, TurnError> {
    let store = MessageStore::new(connection);
    let mut repaired = 0;
    for mut part in store.unfinished_tool_parts_for_session(session_id)? {
        let Some(state) = part.data.get_mut("state").and_then(Value::as_object_mut) else {
            continue;
        };
        state.insert("status".to_owned(), Value::String("error".to_owned()));
        state.insert(
            "error".to_owned(),
            Value::String(INTERRUPTED_TOOL_RESULT.to_owned()),
        );
        let mut metadata = Map::new();
        metadata.insert("interrupted".to_owned(), Value::Bool(true));
        state.insert("metadata".to_owned(), Value::Object(metadata));
        store.put_part(&part)?;
        repaired += 1;
    }
    Ok(repaired)
}

/// Hydrate exactly the suffix that [`retained_history`] permits a request to carry.
///
/// The first phase decodes only message metadata, compaction markers, and candidate
/// summary text. Full part hydration starts only after a successful marker's
/// `tail_start_id`; a failed or dangling compaction still falls back to the complete
/// session. Message and part ordering remains the database's `(time_created, id)` /
/// `part.id` order, which protects the byte-stable prefix checked by
/// `loop_compacted_prefix_is_byte_identical_without_decoding_the_discarded_head`.
///
/// # Errors
///
/// Database query or decode failures from either phase.
pub fn hydrate_retained_history(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<MessageWithParts>, DbError> {
    let store = MessageStore::new(connection);
    let mut messages = store.messages_for_session(session_id)?;
    if messages.is_empty() {
        return Ok(Vec::new());
    }

    let compaction_parts = store.parts_for_session_by_kind(session_id, PartKind::Compaction)?;
    let marker = messages.iter().rev().find_map(|message| {
        compaction_parts
            .iter()
            .filter(|part| part.message_id == message.id)
            .find_map(|part| {
                part.data
                    .get("tail_start_id")
                    .and_then(Value::as_str)
                    .map(|tail_start_id| (message.id.clone(), tail_start_id.to_owned()))
            })
    });

    let Some((marker_id, tail_start_id)) = marker else {
        return store.hydrate(messages);
    };
    let summary_ids = messages
        .iter()
        .filter(|message| {
            message.data.get("parentID").and_then(Value::as_str) == Some(marker_id.as_str())
                && !message.data.contains_key("error")
        })
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    let summary_text = store.parts_by_message_kind(&summary_ids, PartKind::Text)?;
    let summary_succeeded = summary_ids.iter().any(|id| {
        summary_text.get(id).is_some_and(|parts| {
            parts.iter().any(|part| {
                part.data
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
            })
        })
    });
    if !summary_succeeded {
        return store.hydrate(messages);
    }
    let Some(tail_index) = messages
        .iter()
        .position(|message| message.id == tail_start_id)
    else {
        return store.hydrate(messages);
    };

    messages.drain(..tail_index);
    store.hydrate(messages)
}

/// One provider-bound message, and the stored message it was projected from.
///
/// The provenance is here because compaction needs it: its boundary is expressed as
/// a *stored* message id (`tail_start_id`), while its token budget is measured over
/// *projected* messages, and one stored assistant message projects to two — the
/// assistant turn and the `tool` turn carrying its results. Carrying the id through
/// is what lets [`retained_history`] and
/// [`crate::prelude::transcript`] agree about where the tail starts; deriving it
/// twice is how they would come to disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedMessage {
    /// The stored message this was projected from, absent for the system prompt.
    pub message_id: Option<String>,
    /// The message as the provider will receive it.
    pub message: Message,
}

/// Project stored history into the exact message list a request carries.
///
/// The provenance-preserving projection used by compaction and byte-level tests.
/// Runtime provider requests use [`project_history_owned`] so large payloads move
/// rather than clone; the two paths are kept byte-equivalent by the loop regression
/// test. Keeping this projection single-sourced prevents compaction boundaries from
/// drifting away from the messages they measure.
#[must_use]
pub fn project_history(system_prompt: &str, history: &[MessageWithParts]) -> Vec<ProjectedMessage> {
    let mut projected = vec![ProjectedMessage {
        message_id: None,
        message: Message::new(Role::System, system_prompt),
    }];
    for message in history {
        let mut messages = Vec::new();
        match message.info.role {
            MessageRole::User => append_user_message(&mut messages, message),
            MessageRole::Assistant => append_assistant_message(&mut messages, message),
        }
        projected.extend(messages.into_iter().map(|projection| ProjectedMessage {
            message_id: Some(message.info.id.clone()),
            message: projection,
        }));
    }
    projected
}

/// Consume retained history into the exact provider message list.
///
/// The output is byte-equivalent to [`project_history`], but large strings and
/// JSON values are moved out of stored parts rather than cloned. The retained
/// boundary is computed before ownership is consumed, preserving failed and
/// dangling compaction semantics.
#[must_use]
pub fn project_history_owned(system_prompt: &str, history: Vec<MessageWithParts>) -> Vec<Message> {
    project_history_owned_with_ids(system_prompt, history)
        .into_iter()
        .map(|projected| projected.message)
        .collect()
}

/// Consume retained history while preserving each provider message's stored id.
///
/// This is the ownership-moving counterpart to [`project_history`]. Compaction uses
/// the ids to persist its tail boundary, while moving large part payloads directly
/// into transcript entries instead of keeping both hydrated and projected copies.
#[must_use]
pub fn project_history_owned_with_ids(
    system_prompt: &str,
    history: Vec<MessageWithParts>,
) -> Vec<ProjectedMessage> {
    map_project_history_owned_with_ids(system_prompt, history, std::convert::identity)
}

/// Consume retained history and transform each projected message before the next
/// stored message is projected.
///
/// Unlike mapping the [`Vec`] returned by [`project_history_owned_with_ids`], this
/// keeps at most one stored message's projected payloads alive before `map` can
/// reduce them. Compaction uses that property to charge a complete tool result and
/// immediately truncate it instead of first aggregating every complete result in
/// the session.
pub(crate) fn map_project_history_owned_with_ids<T>(
    system_prompt: &str,
    mut history: Vec<MessageWithParts>,
    mut map: impl FnMut(ProjectedMessage) -> T,
) -> Vec<T> {
    let retained_start = retained_history(&history).as_ptr_range().start;
    let tail_index = history
        .iter()
        .position(|message| std::ptr::eq(message, retained_start))
        .unwrap_or(history.len());
    let mut projected = vec![map(ProjectedMessage {
        message_id: None,
        message: Message::new(Role::System, system_prompt),
    })];
    for message in history.drain(tail_index..) {
        let message_id = message.info.id;
        let mut messages = Vec::new();
        match message.info.role {
            MessageRole::User => append_user_message_owned(&mut messages, message.parts),
            MessageRole::Assistant => {
                append_assistant_message_owned(&mut messages, message.parts);
            }
        }
        projected.extend(messages.into_iter().map(|message| {
            map(ProjectedMessage {
                message_id: Some(message_id.clone()),
                message,
            })
        }));
    }
    projected
}

/// The suffix of `history` a request may carry, honouring the newest compaction.
///
/// A compaction attempt writes three things: a marker user message naming the stored
/// id its verbatim tail starts at, an assistant summary message, and — on success —
/// the summary text. All three sort *after* the tail, because they are stamped when
/// the attempt runs. So honouring a compaction is not a rewrite: it is dropping
/// every stored message before the tail. The marker itself projects to nothing
/// (`compaction` is not a request-bearing part kind) and the summary projects to the
/// assistant text message [`crate::compaction::run_compaction`] returns, so the
/// resulting request is exactly that function's `messages` without either being
/// reconstructed here.
///
/// # Why a failed attempt is ignored rather than honoured
///
/// A failed attempt persists the marker *and* an errored summary carrying no text.
/// Honouring that marker would drop the history and substitute nothing, silently
/// sending the model a conversation that starts mid-thought — the worst available
/// outcome, and indistinguishable from a working compaction from the outside. So a
/// marker only takes effect once its paired summary carries text and no error, and
/// an unrecognised or dangling `tail_start_id` leaves the full history in place.
/// Retaining too much costs tokens; retaining too little loses the conversation.
#[must_use]
pub fn retained_history(history: &[MessageWithParts]) -> &[MessageWithParts] {
    let Some((marker_index, tail_start_id)) = history
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| compaction_tail_start(message).map(|tail| (index, tail)))
    else {
        return history;
    };
    if !compaction_summary_succeeded(history, &history[marker_index].info.id) {
        return history;
    }
    history
        .iter()
        .position(|message| message.info.id == tail_start_id)
        .map_or(history, |tail_index| &history[tail_index..])
}

fn compaction_tail_start(message: &MessageWithParts) -> Option<String> {
    message
        .parts
        .iter()
        .filter(|part| part.kind == PartKind::Compaction)
        .find_map(|part| {
            part.data
                .get("tail_start_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

fn compaction_summary_succeeded(history: &[MessageWithParts], marker_id: &str) -> bool {
    history
        .iter()
        .filter(|message| {
            message.info.data.get("parentID").and_then(Value::as_str) == Some(marker_id)
        })
        .any(|summary| {
            !summary.info.data.contains_key("error")
                && summary.parts.iter().any(|part| {
                    part.kind == PartKind::Text
                        && part
                            .data
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.trim().is_empty())
                })
        })
}

/// Whether a stored user message records a compaction rather than a user's turn.
///
/// [`crate::compaction::run_compaction`] files its marker under the `user` role so
/// the transcript reads in order, but it is a machine's bookkeeping: its model is the
/// small summarising model, and answering it would run the turn on that model instead
/// of the one the user chose.
fn is_compaction_marker(message: &MessageWithParts) -> bool {
    !message.parts.is_empty()
        && message
            .parts
            .iter()
            .all(|part| part.kind == PartKind::Compaction)
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

fn take_string(data: &mut Map<String, Value>, key: &str) -> Option<String> {
    match data.remove(key) {
        Some(Value::String(value)) => Some(value),
        Some(_) | None => None,
    }
}

fn append_user_message_owned(messages: &mut Vec<Message>, parts: Vec<PartRecord>) {
    let mut content = Vec::new();
    for mut part in parts {
        match part.kind {
            PartKind::Text => {
                if let Some(text) = take_string(&mut part.data, "text") {
                    content.push(RequestContentBlock::Text { text });
                }
            }
            PartKind::File => {
                let media_type = take_string(&mut part.data, "mime");
                let data = take_string(&mut part.data, "data");
                if let (Some(media_type), Some(data)) = (media_type, data) {
                    content.push(RequestContentBlock::Image { media_type, data });
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

fn append_assistant_message_owned(messages: &mut Vec<Message>, parts: Vec<PartRecord>) {
    let mut assistant = Vec::new();
    let mut results = Vec::new();
    for mut part in parts {
        match part.kind {
            PartKind::Text => {
                if let Some(text) = take_string(&mut part.data, "text") {
                    assistant.push(RequestContentBlock::Text { text });
                }
            }
            PartKind::Reasoning => append_reasoning_owned(&mut assistant, part.data),
            PartKind::Tool => append_tool_pair_owned(&mut assistant, &mut results, part.data),
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

fn append_transformed_message_owned(messages: &mut Vec<Message>, message: HookMessageWithParts) {
    let role = message.info.role;
    match role {
        Role::System | Role::User => {
            let start = messages.len();
            append_user_message_owned(messages, message.parts);
            for projected in &mut messages[start..] {
                projected.role = role;
            }
        }
        Role::Assistant | Role::Tool => {
            let mut projected = Vec::new();
            append_assistant_message_owned(&mut projected, message.parts);
            messages.extend(
                projected
                    .into_iter()
                    .filter(|projected| projected.role == role),
            );
        }
    }
}

fn append_reasoning_owned(content: &mut Vec<RequestContentBlock>, mut data: Map<String, Value>) {
    let thinking = take_string(&mut data, "text");
    let signature = match data.remove("metadata") {
        Some(Value::Object(mut metadata)) => take_string(&mut metadata, "signature"),
        Some(_) | None => None,
    };
    if let (Some(thinking), Some(signature)) = (thinking, signature) {
        content.push(RequestContentBlock::SignedThinking {
            thinking,
            signature,
        });
    }
}

fn append_tool_pair_owned(
    assistant: &mut Vec<RequestContentBlock>,
    results: &mut Vec<RequestContentBlock>,
    mut data: Map<String, Value>,
) {
    let Some(call_id) = take_string(&mut data, "callID") else {
        return;
    };
    let Some(name) = take_string(&mut data, "tool") else {
        return;
    };
    let Some(Value::Object(mut state)) = data.remove("state") else {
        return;
    };
    let input = state.remove("input").unwrap_or_else(|| json!({}));
    let thought_signature = match data.remove("metadata") {
        Some(Value::Object(mut metadata)) => take_string(&mut metadata, "thoughtSignature"),
        Some(_) | None => None,
    }
    .map(ThoughtSignature::new);
    assistant.push(RequestContentBlock::ToolUse {
        id: call_id.clone(),
        name,
        input,
        thought_signature,
    });

    match take_string(&mut state, "status").as_deref() {
        Some("completed") => results.push(RequestContentBlock::ToolResult {
            tool_use_id: call_id,
            content: take_string(&mut state, "output").unwrap_or_default(),
            is_error: Some(false),
        }),
        Some("error") => results.push(RequestContentBlock::ToolResult {
            tool_use_id: call_id,
            content: take_string(&mut state, "error")
                .unwrap_or_else(|| INTERRUPTED_TOOL_RESULT.to_owned()),
            is_error: Some(true),
        }),
        Some(_) | None => {}
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
        "modelID": model.catalog_model_id,
        "providerID": model.catalog_provider_id,
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
    prepared: PreparedTurn<ToolDefinition>,
) -> zuno_llm::registry::CompletionRequest {
    let (messages, tools) = prepared.into_request_parts();
    zuno_llm::registry::CompletionRequest {
        model_id: model.model_id.clone(),
        surface: model.surface,
        messages,
        tools: tools
            .iter()
            .map(|tool| zuno_llm::registry::ToolSchema {
                name: tool.id.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect(),
        parameters: Map::new(),
        headers: std::collections::BTreeMap::new(),
    }
}

fn hook_messages(history: &[MessageWithParts]) -> Vec<HookMessageWithParts> {
    let retained = retained_history(history);
    project_history("", retained)
        .into_iter()
        .skip(1)
        .map(|projected| {
            let parts = projected
                .message_id
                .as_deref()
                .and_then(|id| retained.iter().find(|message| message.info.id == id))
                .map_or_else(Vec::new, |message| message.parts.clone());
            HookMessageWithParts {
                info: projected.message,
                parts,
            }
        })
        .collect()
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
