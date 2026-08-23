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

use std::collections::BTreeMap;
use std::num::{NonZeroU8, NonZeroU32};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::{StreamExt, stream};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;
use zuno_db::event_log::{NewSessionEvent, append_with_connection};
use zuno_db::inbox::SessionInbox;
use zuno_db::message::{
    MessageRecord, MessageRole, MessageStore, MessageWithParts, PartKind, PartRecord,
    created_after, now_millis,
};
use zuno_db::{Connection, open, session};
use zuno_error::{DbError, ProviderError};
use zuno_llm::cache::{CacheViolation, DynamicContext, McpToolStatus, PreparedTurn, PromptCache};
use zuno_llm::event::{
    FinishReason, Message, PromptAccounting, RequestContentBlock, Role, StreamEvent,
    ThoughtSignature,
};
use zuno_llm::registry::{ApiSurface, ProviderRegistry, Spec};
use zuno_llm::sse::{StreamLimits, append_tool_input};
use zuno_tool::{
    ToolConcurrencyPolicy, ToolDefinition, ToolOutput, ToolReplayPolicy, ToolUiIntent,
};

use crate::hooks::{HookMessageWithParts, NoopHooks, RequestHookInput, TurnHooks};
use crate::interrupt::{InterruptSignal, SoftInterruptMessage};
use crate::prompt::{PromptAssembly, PromptTraceSet};
use crate::retry::{
    PROVIDER_RETRY_MAX_ATTEMPTS, ProviderRetryError, ProviderRetryPolicy, retry_provider,
};
use crate::status::SessionRunGuard;

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
    /// A prepared interactive session acquired its durable row with the first input.
    SessionMaterialized {
        session_id: String,
        title: String,
    },
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
        ui_intent: ToolUiIntent,
    },
    /// A call that was refused before its requested effect could run.
    ///
    /// Kept separate from [`Self::ToolDispatchCompleted`] because the latter is the
    /// model-visible result append and still carries `is_error` for provider protocol
    /// semantics. A blocked call is not an execution failure: clients render it as a
    /// warning and can say that nothing ran, while a real failure remains an error.
    ToolDispatchBlocked {
        step: u32,
        call_id: String,
        kind: ToolBlockKind,
    },
    ToolDispatchCompleted {
        step: u32,
        call_id: String,
        name: String,
        title: String,
        output: String,
        /// The unified patch this call produced, when it changed a file.
        ///
        /// Lifted out of the result's `metadata["diff"]` rather than carrying the whole
        /// metadata map, because this enum is a typed event stream for hosts: a
        /// `Map<String, Value>` here would make every projection decide which
        /// tool-private keys to leak, and the only thing a host needs is the patch.
        /// `None` for every tool that changed nothing — see
        /// `zuno_tools::diff` for why an absent patch is not an empty one.
        diff: Option<String>,
        /// The files this call wrote, lifted from
        /// [`zuno_tool::output::METADATA_WRITTEN_PATHS_KEY`].
        ///
        /// Typed and plural rather than left in `metadata` for the reason `diff` is, and
        /// plural rather than derived from `title` because a title is prose: `apply_patch`
        /// summarises a whole patch set as `Success. Updated the following files:` and one
        /// call can write several files. A host checking the paths a turn wrote — the
        /// TUI's language-server hook — had neither, so it matched tool *names* against a
        /// hand-kept list and silently checked nothing on the models whose only writing
        /// tool is `apply_patch`.
        written_paths: Vec<String>,
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

/// Why a tool call was stopped before its requested effect ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolBlockKind {
    /// A permission rule, plugin, or human refused the call.
    Denied,
    /// The model supplied malformed or unsafe arguments.
    InvalidArguments,
    /// No callable implementation was available under the requested name.
    Unavailable,
}

impl ToolBlockKind {
    /// Stable wire/storage spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::InvalidArguments => "invalid_arguments",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Frames read after a message finishes, for the bookkeeping that follows it.
///
/// An OpenAI-compatible endpoint sends `usage` in a chunk after the finish reason and
/// `[DONE]` after that, so two is the real requirement; the budget is larger to absorb
/// a keep-alive or an upstream-provider frame without losing the usage behind it. It is
/// bounded at all because a provider that keeps streaming after saying it finished must
/// not be able to hold a turn open.
pub const TRAILING_FRAME_BUDGET: u8 = 8;

/// A normal terminal state of [`run_turn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed {
        assistant_message_id: String,
        steps: u32,
        /// Retryable tool failures not followed by a successful call to that tool.
        unresolved_tool_failures: Vec<ToolFailureRecovery>,
    },
    Interrupted {
        assistant_message_id: Option<String>,
        steps: u32,
    },
}

/// Recovery information retained after a model-visible tool failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFailureRecovery {
    /// Tool whose most recent retryable failure remains unresolved.
    pub tool: String,
    /// Whether an identical call is safe to issue again.
    pub replay_policy: ToolReplayPolicy,
    /// Delay requested by the failed peer, when one was supplied.
    pub retry_after: Option<Duration>,
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

/// Why a retryable terminal turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRetryReason {
    /// The provider explicitly rate limited the request.
    RateLimited,
    /// A transport or upstream server failure is expected to clear.
    ProviderTransient,
    /// The stream ended without a complete assistant message.
    ProviderStream,
    /// The bounded same-request recovery sequence exhausted its deadline.
    ProviderRetryDeadline,
    /// SQLite reported another active writer.
    DatabaseBusy,
    /// One turn reached its step ceiling while the larger goal may continue.
    StepLimit,
    /// The provider returned no assistant content.
    EmptyAssistantMessage,
}

/// Action a goal controller may take after a terminal turn failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRecovery {
    /// Start a fresh goal turn after backoff.
    Retry {
        /// Typed retry class.
        reason: TurnRetryReason,
        /// Peer-requested delay, when one was supplied.
        after: Option<Duration>,
    },
    /// Compact retained history before another attempt.
    Compact,
    /// Stop automatic work until a user explicitly resumes it.
    Pause,
    /// Repeating cannot repair this failure.
    Fail,
}

impl TurnError {
    /// Classify the failure without inspecting its rendered message.
    #[must_use]
    pub fn recovery(&self) -> TurnRecovery {
        match self {
            Self::StepLimit { .. } => TurnRecovery::Retry {
                reason: TurnRetryReason::StepLimit,
                after: None,
            },
            Self::StreamEndedWithoutMessageEnd { .. } => TurnRecovery::Retry {
                reason: TurnRetryReason::ProviderStream,
                after: None,
            },
            Self::EmptyAssistantMessage { .. } => TurnRecovery::Retry {
                reason: TurnRetryReason::EmptyAssistantMessage,
                after: None,
            },
            Self::Database(DbError::Busy { retry_after }) => TurnRecovery::Retry {
                reason: TurnRetryReason::DatabaseBusy,
                after: *retry_after,
            },
            Self::Database(
                DbError::Open { .. }
                | DbError::Schema { .. }
                | DbError::SchemaMismatch { .. }
                | DbError::Query { .. }
                | DbError::NotFound { .. }
                | DbError::Decode { .. },
            ) => TurnRecovery::Fail,
            Self::Provider(ProviderError::ContextLimit { .. }) => TurnRecovery::Compact,
            Self::Provider(ProviderError::RateLimited { retry_after }) => TurnRecovery::Retry {
                reason: TurnRetryReason::RateLimited,
                after: *retry_after,
            },
            Self::Provider(ProviderError::Transient { .. }) => TurnRecovery::Retry {
                reason: TurnRetryReason::ProviderTransient,
                after: None,
            },
            Self::Provider(ProviderError::Auth { .. }) | Self::EventConsumerClosed => {
                TurnRecovery::Pause
            }
            Self::Provider(ProviderError::Refused { .. } | ProviderError::Fatal { .. })
            | Self::NoUserMessage { .. }
            | Self::MissingUserField { .. }
            | Self::AgentNotFound { .. }
            | Self::ModelNotFound { .. }
            | Self::NestedToolUse { .. }
            | Self::ToolUseEndWithoutStart { .. }
            | Self::Hook(_)
            | Self::Cache(_) => TurnRecovery::Fail,
            Self::ProviderRetryDeadlineExceeded { .. } => TurnRecovery::Retry {
                reason: TurnRetryReason::ProviderRetryDeadline,
                after: None,
            },
        }
    }
}

/// Agent data the loop needs after configuration resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub name: String,
    pub system_prompt: String,
    /// Ordered provenance for [`Self::system_prompt`].
    pub prompt_assembly: PromptAssembly,
    pub max_steps: u32,
}

impl ResolvedAgent {
    #[must_use]
    pub fn new(name: impl Into<String>, system_prompt: impl Into<String>, max_steps: u32) -> Self {
        let system_prompt = system_prompt.into();
        let mut prompt_assembly = PromptAssembly::new();
        prompt_assembly
            .push(
                "agent.resolved",
                "AgentModelResolver",
                system_prompt.clone(),
            )
            .expect("the built-in resolved prompt section id is valid");
        Self {
            name: name.into(),
            system_prompt,
            prompt_assembly,
            max_steps,
        }
    }

    /// Replace the opaque prompt with its ordered assembly.
    #[must_use]
    pub fn with_prompt_assembly(mut self, prompt_assembly: PromptAssembly) -> Self {
        self.system_prompt = prompt_assembly.render();
        self.prompt_assembly = prompt_assembly;
        self
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
    /// Provider-native reasoning controls for the session's chosen effort level.
    ///
    /// The resolved options rather than a `ReasoningEffort`, because the canonical
    /// level means nothing on a wire: Anthropic wants `thinking.budgetTokens`,
    /// Google `thinkingConfig.thinkingLevel`, OpenAI `reasoningEffort`.
    /// `zuno_llm::effort::resolve_effort` owns that translation and needs the
    /// provider family — a catalog fact this module deliberately does not hold. The
    /// caller resolves, the engine transports.
    ///
    /// Empty for a model without reasoning support or a session that chose no level,
    /// which is what keeps such a request byte-identical to the pre-effort build.
    pub reasoning_options: Map<String, Value>,
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
            reasoning_options: Map::new(),
        }
    }

    /// Attach the provider-native reasoning controls for the chosen effort level.
    ///
    /// Built by the caller with `zuno_llm::effort::resolve_effort`, whose
    /// `EffortResolution::options` is exactly this shape.
    #[must_use]
    pub fn with_reasoning_options(mut self, options: Map<String, Value>) -> Self {
        self.reasoning_options = options;
        self
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
    pub recovery: Option<ToolFailureRecovery>,
    /// Present only when execution was refused before the requested effect ran.
    pub blocked: Option<ToolBlockKind>,
}

impl ToolDispatchResult {
    #[must_use]
    pub fn success(output: ToolOutput) -> Self {
        Self {
            output,
            is_error: false,
            recovery: None,
            blocked: None,
        }
    }

    #[must_use]
    pub fn error(output: ToolOutput) -> Self {
        Self {
            output,
            is_error: true,
            recovery: None,
            blocked: None,
        }
    }

    #[must_use]
    pub fn blocked(output: ToolOutput, kind: ToolBlockKind) -> Self {
        Self {
            output,
            is_error: true,
            recovery: None,
            blocked: Some(kind),
        }
    }

    #[must_use]
    pub fn retryable_error(output: ToolOutput, recovery: ToolFailureRecovery) -> Self {
        Self {
            output,
            is_error: true,
            recovery: Some(recovery),
            blocked: None,
        }
    }
}

/// Todo 33's single dispatch choke point.
pub struct PreparedToolDispatch {
    execution: BoxFuture<'static, ToolDispatchResult>,
}

impl PreparedToolDispatch {
    /// Stage an owned execution future after validation and permission checks.
    #[must_use]
    pub fn new(execution: BoxFuture<'static, ToolDispatchResult>) -> Self {
        Self { execution }
    }

    /// Stage a result that was fully decided during preparation.
    #[must_use]
    pub fn ready(result: ToolDispatchResult) -> Self {
        Self::new(Box::pin(async move { result }))
    }

    /// Execute the already-authorized call.
    pub async fn execute(self) -> ToolDispatchResult {
        self.execution.await
    }
}

/// Validated bound for independent calls in one assistant step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolConcurrencyLimit(NonZeroU8);

impl ToolConcurrencyLimit {
    pub const SERIAL: Self = Self(NonZeroU8::MIN);

    /// Accept only the configuration contract shared by tools, MCP, and LSP.
    #[must_use]
    pub const fn new(value: u8) -> Option<Self> {
        match NonZeroU8::new(value) {
            Some(value) if value.get() <= 64 => Some(Self(value)),
            _ => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get() as usize
    }
}

/// The engine-facing tool boundary.
///
/// Preparation is deliberately separate from execution: hooks, argument
/// validation, and permission prompts run in model order, while only calls whose
/// tools explicitly opt into a non-exclusive policy may execute concurrently.
#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    fn available_tools(&self) -> AvailableTools;

    fn concurrency_policy(&self, _request: &DispatchRequest) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::Exclusive
    }

    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch;

    async fn dispatch(&self, request: DispatchRequest) -> ToolDispatchResult {
        self.prepare(request).await.execute().await
    }
}

/// Stable caller-owned identity and volatile suffix for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTurnRequest {
    pub session_id: String,
    pub turn_id: String,
    pub dynamic_context: DynamicContext,
    /// Model context ceiling used to interpret the latest prompt occupancy.
    pub context_limit: Option<u64>,
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
            context_limit: None,
        }
    }

    /// Attach the active model's context ceiling.
    #[must_use]
    pub fn with_context_limit(mut self, context_limit: u64) -> Self {
        self.context_limit = (context_limit > 0).then_some(context_limit);
        self
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
    live_inputs: Option<LiveInputs<'a>>,
    tool_concurrency: ToolConcurrencyLimit,
}

struct LiveInputs<'a> {
    guard: &'a SessionRunGuard,
    inbox: &'a SessionInbox,
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
            live_inputs: None,
            tool_concurrency: ToolConcurrencyLimit::SERIAL,
        }
    }

    #[must_use]
    pub fn with_hooks(mut self, hooks: Arc<dyn TurnHooks>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Enable durable soft-input injection for a live session lease.
    #[must_use]
    pub fn with_live_inputs(mut self, guard: &'a SessionRunGuard, inbox: &'a SessionInbox) -> Self {
        self.live_inputs = Some(LiveInputs { guard, inbox });
        self
    }

    /// Bound explicitly parallel-safe calls in one assistant step.
    #[must_use]
    pub fn with_tool_concurrency(mut self, limit: ToolConcurrencyLimit) -> Self {
        self.tool_concurrency = limit;
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

#[derive(Debug)]
struct StepAccumulator {
    provider: String,
    stream: String,
    tool_input_limit: usize,
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
    prompt_accounting: Option<PromptAccounting>,
}

impl StepAccumulator {
    fn new(provider: String, stream: String, tool_input_limit: usize) -> Self {
        Self {
            provider,
            stream,
            tool_input_limit,
            text: String::new(),
            reasoning: String::new(),
            reasoning_signature: String::new(),
            provider_reasoning: Vec::new(),
            calls: Vec::new(),
            active_tool: None,
            finish_reason: None,
            saw_message_end: false,
            input_tokens: None,
            output_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            prompt_accounting: None,
        }
    }

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
                    append_tool_input(
                        &mut tool.raw_input,
                        delta,
                        &self.provider,
                        &self.stream,
                        self.tool_input_limit,
                    )?;
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
                accounting,
            } => {
                self.input_tokens = *input_tokens;
                self.output_tokens = *output_tokens;
                self.cache_read_input_tokens = *cache_read_input_tokens;
                self.cache_write_input_tokens = *cache_write_input_tokens;
                self.prompt_accounting = Some(*accounting);
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
        self.prompt_accounting = None;
    }

    fn provider_reasoning_capsules(&self) -> impl Iterator<Item = &RequestContentBlock> {
        self.provider_reasoning.iter().filter(|block| {
            matches!(
                block,
                RequestContentBlock::ProviderEncryptedReasoning { .. }
            )
        })
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
    mut context: TurnContext<'_>,
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
    let mut prompt_traces = PromptTraceSet::default();
    let mut unresolved_tool_failures = BTreeMap::<String, ToolFailureRecovery>::new();

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
        if prompt_traces.insert(&system_prompt) {
            let event = NewSessionEvent::new(
                "session.prompt.assembled",
                agent
                    .prompt_assembly
                    .event_properties(&agent.name, step, &system_prompt),
            )?;
            append_with_connection(context.connection, &request.session_id, event)?;
        }
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
        let accumulator = Arc::new(Mutex::new(StepAccumulator::new(
            model.catalog_provider_id.clone(),
            model.catalog_model_id.clone(),
            StreamLimits::from_environment().max_tool_input_bytes(),
        )));
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
                    // `Some(n)` once the message has finished: how many more frames may be
                    // read for their bookkeeping before the step ends regardless.
                    let mut trailing: Option<u8> = None;
                    loop {
                        let next = tokio::select! {
                            biased;
                            _ = interrupt.notified() => return Ok(Ok(true)),
                            event = stream.next() => event,
                        };
                        let Some(next) = next else {
                            return Ok(Ok(false));
                        };
                        // An error *after* the message has finished is not the turn's
                        // failure: the answer is already complete and persisted, and the
                        // only thing still outstanding is bookkeeping. Failing the turn
                        // over a truncated trailing frame would throw away a reply the
                        // user has already read.
                        if trailing.is_some() && next.is_err() {
                            return Ok(Ok(false));
                        }
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
                        // `MessageEnd` is not the last frame an OpenAI-compatible endpoint
                        // sends: `usage` arrives in a chunk *after* the one carrying the
                        // finish reason, and `[DONE]` after that. Returning here read the
                        // finish reason and discarded everything behind it, so
                        // `StreamEvent::TokenUsage` — and therefore
                        // `StepAccumulator`'s token fields, and therefore the `tokens`
                        // column `update_usage` writes — could never be reached on those
                        // providers. Measured: every assistant row in a nine-session
                        // database had `input: 0, output: 0`.
                        //
                        // So a finished message starts a bounded trailing drain rather
                        // than ending the step. Bounded because a provider that keeps
                        // streaming after saying it finished must not hold the turn open:
                        // the count is generous next to the one-or-two frames a real
                        // endpoint sends, and the provider's own idle timeout still
                        // governs how long any single frame may take to arrive.
                        if ended {
                            trailing = Some(TRAILING_FRAME_BUDGET);
                        }
                        if let Some(remaining) = trailing.as_mut() {
                            if *remaining == 0 {
                                return Ok(Ok(false));
                            }
                            *remaining -= 1;
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
            let replacement = StepAccumulator::new(
                accumulator.provider.clone(),
                accumulator.stream.clone(),
                accumulator.tool_input_limit,
            );
            std::mem::replace(&mut *accumulator, replacement)
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
                &locked_tools,
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
                &locked_tools,
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
            &locked_tools,
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

        let mut injected = inject_live_inputs(&mut context, &request, &requested)?;
        if !injected.skip_remaining_tools {
            let mut next_call = 0;
            while next_call < accumulator.calls.len() && !injected.skip_remaining_tools {
                let first_request = dispatch_request(
                    accumulator.calls[next_call].clone(),
                    &request,
                    &assistant_id,
                    &agent.name,
                    &locked_tools,
                    context.interrupt,
                );
                let first_policy = context.dispatcher.concurrency_policy(&first_request);
                let mut group_end = next_call.saturating_add(1);
                if first_policy != ToolConcurrencyPolicy::Exclusive {
                    while group_end < accumulator.calls.len() {
                        let candidate = dispatch_request(
                            accumulator.calls[group_end].clone(),
                            &request,
                            &assistant_id,
                            &agent.name,
                            &locked_tools,
                            context.interrupt,
                        );
                        if context.dispatcher.concurrency_policy(&candidate)
                            == ToolConcurrencyPolicy::Exclusive
                        {
                            break;
                        }
                        group_end = group_end.saturating_add(1);
                    }
                }

                let mut prepared = Vec::with_capacity(group_end.saturating_sub(next_call));
                for call_index in next_call..group_end {
                    let call = accumulator.calls[call_index].clone();
                    let ui_intent = tool_ui_intent(&locked_tools, &call.name);
                    events
                        .send(TurnEvent::ToolDispatchStarted {
                            step,
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            ui_intent,
                        })
                        .await?;
                    let dispatch = context
                        .dispatcher
                        .prepare(dispatch_request(
                            call.clone(),
                            &request,
                            &assistant_id,
                            &agent.name,
                            &locked_tools,
                            context.interrupt,
                        ))
                        .await;
                    prepared.push((call_index, call, ui_intent, dispatch));
                }

                let completed = if first_policy == ToolConcurrencyPolicy::Exclusive {
                    let (call_index, call, ui_intent, dispatch) =
                        prepared.pop().expect("exclusive group contains one call");
                    vec![(call_index, call, ui_intent, dispatch.execute().await)]
                } else {
                    stream::iter(prepared.into_iter().map(
                        |(call_index, call, ui_intent, dispatch)| async move {
                            (call_index, call, ui_intent, dispatch.execute().await)
                        },
                    ))
                    .buffered(context.tool_concurrency.get())
                    .collect::<Vec<_>>()
                    .await
                };

                // A completed parallel group is an indivisible durable unit: every
                // execution result is appended in model order before an urgent inbox
                // item may prevent the next group from starting.
                for (call_index, call, ui_intent, dispatch) in completed {
                    if let Some(recovery) = dispatch.recovery.clone() {
                        unresolved_tool_failures.insert(recovery.tool.clone(), recovery);
                    } else {
                        unresolved_tool_failures.remove(&call.name);
                    }
                    persist_tool_result(
                        context.connection,
                        &request,
                        ToolResultIdentity {
                            step,
                            call_index,
                            message_id: &assistant_id,
                            call: &call,
                            ui_intent,
                        },
                        &dispatch,
                    )?;
                    if let Some(kind) = dispatch.blocked {
                        events
                            .send(TurnEvent::ToolDispatchBlocked {
                                step,
                                call_id: call.id.clone(),
                                kind,
                            })
                            .await?;
                    }
                    events
                        .send(TurnEvent::ToolDispatchCompleted {
                            step,
                            call_id: call.id.clone(),
                            name: call.name,
                            title: dispatch.output.title.clone(),
                            output: dispatch.output.output.clone(),
                            diff: dispatch
                                .output
                                .metadata
                                .get("diff")
                                .and_then(serde_json::Value::as_str)
                                .filter(|patch| !patch.is_empty())
                                .map(str::to_owned),
                            written_paths: dispatch
                                .output
                                .written_paths()
                                .into_iter()
                                .map(str::to_owned)
                                .collect(),
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
                    injected.merge(inject_live_inputs(&mut context, &request, &requested)?);
                }
                next_call = group_end;
            }
        }
        injected.merge(inject_live_inputs(&mut context, &request, &requested)?);

        events
            .send(TurnEvent::StepCompleted {
                step,
                finish_reason: accumulator.finish_reason,
            })
            .await?;

        if !accumulator.calls.is_empty() || injected.count > 0 {
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
            unresolved_tool_failures: unresolved_tool_failures.into_values().collect(),
        });
    }
}

fn dispatch_request(
    call: ToolCall,
    request: &RunTurnRequest,
    message_id: &str,
    agent: &str,
    available_tools: &Arc<[ToolDefinition]>,
    interrupt: &InterruptSignal,
) -> DispatchRequest {
    DispatchRequest {
        call,
        session_id: request.session_id.clone(),
        message_id: message_id.to_owned(),
        agent: agent.to_owned(),
        available_tools: Arc::clone(available_tools),
        interrupt: interrupt.clone(),
    }
}

#[derive(Debug, Default)]
struct InjectedLiveInputs {
    count: usize,
    skip_remaining_tools: bool,
}

impl InjectedLiveInputs {
    fn merge(&mut self, other: Self) {
        self.count = self.count.saturating_add(other.count);
        self.skip_remaining_tools |= other.skip_remaining_tools;
    }
}

fn inject_live_inputs(
    context: &mut TurnContext<'_>,
    request: &RunTurnRequest,
    requested: &RequestedTurn,
) -> Result<InjectedLiveInputs, TurnError> {
    let Some(live) = context.live_inputs.as_ref() else {
        return Ok(InjectedLiveInputs::default());
    };
    let delivery = live.guard.take_soft_interrupts_at_safe_point();
    let mut injected = InjectedLiveInputs::default();
    for message in delivery.messages {
        if let Some(input_id) = message.input_id.as_deref()
            && live
                .inbox
                .promote_id(&request.session_id, input_id)?
                .is_none()
        {
            continue;
        }
        persist_live_input(context.connection, request, requested, &message)?;
        injected.count = injected.count.saturating_add(1);
        injected.skip_remaining_tools |= message.urgent;
    }
    Ok(injected)
}

fn persist_live_input(
    connection: &Connection,
    request: &RunTurnRequest,
    requested: &RequestedTurn,
    input: &SoftInterruptMessage,
) -> Result<(), TurnError> {
    let store = MessageStore::new(connection);
    let latest = store.latest_time_created(&request.session_id)?;
    let created = created_after(now_millis(), latest);
    let message_id = input
        .input_id
        .clone()
        .unwrap_or_else(|| format!("msg_{}", Uuid::new_v4().simple()));
    let message = MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": request.session_id,
        "role": "user",
        "time": { "created": created },
        "agent": requested.agent,
        "model": {
            "providerID": requested.provider_id,
            "modelID": requested.model_id
        }
    }))?;
    let mut parts = Vec::with_capacity(input.images.len().saturating_add(1));
    parts.push(PartRecord::from_json(
        json!({
            "id": format!("prt_{}", Uuid::new_v4().simple()),
            "sessionID": request.session_id,
            "messageID": message.id,
            "type": "text",
            "text": input.content
        }),
        created,
    )?);
    for (offset, (media_type, data)) in input.images.iter().enumerate() {
        parts.push(PartRecord::from_json(
            json!({
                "id": format!("prt_{}", Uuid::new_v4().simple()),
                "sessionID": request.session_id,
                "messageID": message.id,
                "type": "file",
                "mime": media_type,
                "data": data,
                "url": format!("data:{media_type};base64,{data}")
            }),
            created.saturating_add(i64::try_from(offset).unwrap_or(i64::MAX).saturating_add(1)),
        )?);
    }
    store.put_message_at(&message, created)?;
    for part in parts {
        store.put_part_at(&part, part.time_created)?;
    }
    Ok(())
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
    let mut metadata = match data.remove("metadata") {
        Some(Value::Object(metadata)) => metadata,
        Some(_) | None => Map::new(),
    };
    if let Some(Value::Object(capsule)) = metadata.remove(PROVIDER_REASONING_KEY) {
        push_provider_reasoning_owned(content, capsule);
        return;
    }
    if let (Some(thinking), Some(signature)) = (thinking, take_string(&mut metadata, "signature")) {
        content.push(RequestContentBlock::SignedThinking {
            thinking,
            signature,
        });
    }
}

/// The `metadata` key the stream projection stores a native reasoning capsule under.
///
/// Named once because the writer and this reader disagreeing about it silently drops
/// the capsule from every replayed turn, which an Anthropic-family model answers with
/// HTTP 400: the assistant message no longer opens with the reasoning it sealed.
const PROVIDER_REASONING_KEY: &str = "providerReasoning";

/// Rebuild a native reasoning capsule for replay, consuming the stored metadata.
///
/// `encryptedContent` is required. A capsule the provider never sealed cannot prove
/// anything, and the OpenAI Responses request builder rejects a reasoning item
/// without it — so an unsealed one is history only, exactly like unsigned thinking.
fn push_provider_reasoning_owned(
    content: &mut Vec<RequestContentBlock>,
    mut capsule: Map<String, Value>,
) {
    let Some(encrypted_content) = take_string(&mut capsule, "encryptedContent") else {
        return;
    };
    let Some(id) = take_string(&mut capsule, "id") else {
        return;
    };
    // The summary is replayed as the provider emitted it, line for line. Re-joining
    // it and splitting it again is how a capsule's visible prefix stops matching
    // what the provider sealed.
    let summary = match capsule.remove("summary") {
        Some(Value::Array(lines)) => lines
            .into_iter()
            .filter_map(|line| match line {
                Value::String(text) => Some(text),
                _ => None,
            })
            .collect(),
        Some(_) | None => Vec::new(),
    };
    content.push(RequestContentBlock::ProviderEncryptedReasoning {
        id,
        summary,
        encrypted_content: Some(encrypted_content),
        status: take_string(&mut capsule, "status"),
    });
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
    let metadata = part.data.get("metadata").and_then(Value::as_object);
    if let Some(capsule) = metadata
        .and_then(|metadata| metadata.get(PROVIDER_REASONING_KEY))
        .and_then(Value::as_object)
    {
        push_provider_reasoning(content, capsule);
        return;
    }
    let thinking = part.data.get("text").and_then(Value::as_str);
    let signature = metadata
        .and_then(|metadata| metadata.get("signature"))
        .and_then(Value::as_str);
    if let (Some(thinking), Some(signature)) = (thinking, signature) {
        content.push(RequestContentBlock::SignedThinking {
            thinking: thinking.to_owned(),
            signature: signature.to_owned(),
        });
    }
}

fn push_provider_reasoning(content: &mut Vec<RequestContentBlock>, capsule: &Map<String, Value>) {
    let Some(encrypted_content) = capsule.get("encryptedContent").and_then(Value::as_str) else {
        return;
    };
    let Some(id) = capsule.get("id").and_then(Value::as_str) else {
        return;
    };
    let summary = capsule
        .get("summary")
        .and_then(Value::as_array)
        .map(|lines| {
            lines
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    content.push(RequestContentBlock::ProviderEncryptedReasoning {
        id: id.to_owned(),
        summary,
        encrypted_content: Some(encrypted_content.to_owned()),
        status: capsule
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
    });
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

/// # Why the reasoning controls travel as `parameters`
///
/// `parameters` is the one channel every provider family already overlays onto its
/// outbound body: `CompletionRequest::apply_parameters` is called by all six
/// adapters (anthropic, openai, bedrock, compatible, and google's three surfaces).
/// Reaching the wire any other way would mean editing each adapter to read a new
/// field, and the two that already accept native reasoning read it from
/// *provider-scoped* options — a per-session choice cannot live there without
/// rewriting the model spec on every keypress.
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
        parameters: model.reasoning_options.clone(),
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
    locked_tools: &[ToolDefinition],
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

    let transaction = connection
        .unchecked_transaction()
        .map_err(open::map_error)?;
    let store = MessageStore::new(&transaction);
    let previous = store
        .find_message(&assistant.id)?
        .map(|message| session::MessageUsage::from_data(&message.data));
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
    for (capsule_index, capsule) in accumulator.provider_reasoning_capsules().enumerate() {
        let part = provider_reasoning_part(
            request,
            step,
            capsule_index,
            assistant,
            completed,
            capsule.clone(),
        )?;
        store.put_part_at(&part, completed)?;
    }
    for (call_index, call) in accumulator.calls.iter().enumerate() {
        let tool = pending_tool_part(
            request,
            step,
            call_index,
            &assistant.id,
            call,
            tool_ui_intent(locked_tools, &call.name),
        )?;
        store.put_part_at(&tool, completed)?;
    }
    session::reconcile_usage(
        &transaction,
        &request.session_id,
        previous,
        session::MessageUsage::from_data(&assistant.data),
        request
            .context_limit
            .and_then(|limit| i64::try_from(limit).ok()),
    )?;
    transaction.commit().map_err(open::map_error)?;
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
    if let Some(accounting) = accumulator.prompt_accounting {
        tokens.insert(
            "accounting".to_owned(),
            Value::String(accounting.as_str().to_owned()),
        );
    } else {
        tokens.remove("accounting");
    }
}

/// Persist one provider reasoning capsule under the key [`append_reasoning_owned`]
/// reads, so a replayed turn can re-send it.
///
/// The stored key spelling is [`PROVIDER_REASONING_KEY`] and the inner field names
/// are the ones [`push_provider_reasoning_owned`] takes; writing any other spelling
/// drops the capsule silently on replay, which is an HTTP 400 from the models that
/// require an assistant turn to open with the reasoning they signed.
fn provider_reasoning_part(
    request: &RunTurnRequest,
    step: u32,
    capsule_index: usize,
    assistant: &MessageRecord,
    completed: i64,
    capsule: RequestContentBlock,
) -> Result<PartRecord, TurnError> {
    let RequestContentBlock::ProviderEncryptedReasoning {
        id,
        summary,
        encrypted_content,
        status,
    } = capsule
    else {
        unreachable!("provider_reasoning_capsules yields only encrypted reasoning blocks");
    };
    PartRecord::from_json(
        json!({
            "id": provider_reasoning_part_id(&request.turn_id, step, capsule_index),
            "sessionID": request.session_id,
            "messageID": assistant.id,
            "type": "reasoning",
            "text": summary.join("\n"),
            "metadata": {
                PROVIDER_REASONING_KEY: {
                    "id": id,
                    "summary": summary,
                    "encryptedContent": encrypted_content,
                    "status": status,
                }
            },
            "time": { "start": assistant.time_created, "end": completed }
        }),
        assistant.time_created,
    )
    .map_err(TurnError::from)
}

fn pending_tool_part(
    request: &RunTurnRequest,
    step: u32,
    call_index: usize,
    message_id: &str,
    call: &ToolCall,
    ui_intent: ToolUiIntent,
) -> Result<PartRecord, TurnError> {
    let mut payload = json!({
        "id": tool_part_id(&request.turn_id, step, call_index),
        "sessionID": request.session_id,
        "messageID": message_id,
        "type": "tool",
        "callID": call.id,
        "tool": call.name,
        "uiIntent": ui_intent,
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

struct ToolResultIdentity<'a> {
    step: u32,
    call_index: usize,
    message_id: &'a str,
    call: &'a ToolCall,
    ui_intent: ToolUiIntent,
}

fn persist_tool_result(
    connection: &Connection,
    request: &RunTurnRequest,
    identity: ToolResultIdentity<'_>,
    dispatch: &ToolDispatchResult,
) -> Result<(), TurnError> {
    let status = if dispatch.is_error {
        "error"
    } else {
        "completed"
    };
    let mut state = json!({
        "status": status,
        "input": identity.call.input,
        "raw": identity.call.raw_input,
        "title": dispatch.output.title,
        "metadata": dispatch.output.metadata,
        "attachments": dispatch.output.attachments,
        "time": { "start": now_millis(), "end": now_millis() }
    });
    if let Some(kind) = dispatch.blocked {
        state["outcome"] = Value::String("blocked".to_owned());
        state["blockKind"] = Value::String(kind.as_str().to_owned());
    }
    if dispatch.is_error {
        state["error"] = Value::String(dispatch.output.output.clone());
    } else {
        state["output"] = Value::String(dispatch.output.output.clone());
    }
    let mut payload = json!({
        "id": tool_part_id(&request.turn_id, identity.step, identity.call_index),
        "sessionID": request.session_id,
        "messageID": identity.message_id,
        "type": "tool",
        "callID": identity.call.id,
        "tool": identity.call.name,
        "uiIntent": identity.ui_intent,
        "state": state
    });
    if let Some(signature) = &identity.call.thought_signature {
        payload["metadata"] = json!({ "thoughtSignature": signature.as_str() });
    }
    let now = now_millis();
    let part = PartRecord::from_json(payload, now)?;
    MessageStore::new(connection).put_part_at(&part, now)?;
    Ok(())
}

fn tool_ui_intent(definitions: &[ToolDefinition], name: &str) -> ToolUiIntent {
    definitions
        .iter()
        .find(|definition| definition.id == name)
        .map_or(ToolUiIntent::Generic, |definition| definition.ui_intent)
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

/// Parts with the same creation time use their id as the stable tie-break
/// (`zuno-db/src/message.rs`). This id has to sort *before* [`text_part_id`] and
/// [`tool_part_id`] so the capsule is replayed as the assistant message's opening
/// content — the only position the provider accepts it in. `"reasoning_"` &lt;
/// `"text"` &lt; `"tool_"` holds, and the zero-padded index keeps several capsules
/// in stream order.
fn provider_reasoning_part_id(turn_id: &str, step: u32, capsule_index: usize) -> String {
    format!("prt_{turn_id}_{step:04}_reasoning_{capsule_index:04}")
}

fn tool_part_id(turn_id: &str, step: u32, call_index: usize) -> String {
    format!("prt_{turn_id}_{step:04}_tool_{call_index:04}")
}
