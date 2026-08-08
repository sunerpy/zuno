//! Post-response reflection over an owned turn transcript.
//!
//! The caller invokes [`ReflectionFork::spawn_after_turn`] after delivery. The fork
//! receives a cloned transcript and an injected `memory` tool, so it cannot mutate
//! foreground conversation state or reach the rest of the tool registry.

mod policy;

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use oc_tool::{Tool, ToolContext, ToolOutput};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

pub use policy::{CommandOutcome, NEGATIVE_LEARNING_LIST, TranscriptEvent, TurnTranscript};

/// The only tool id a reflection fork may dispatch.
pub const MEMORY_TOOL_ID: &str = "memory";

/// Periodic reflection cadence when the caller does not override it.
pub const DEFAULT_TURN_INTERVAL: u64 = 10;

/// A failure reported by the injected model runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionError {
    detail: String,
}

impl ReflectionError {
    /// Construct a runner failure without leaking provider-specific error types.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ReflectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl Error for ReflectionError {}

impl From<std::io::Error> for ReflectionError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// Compaction policy for an isolated review request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionMode {
    /// Preserve the complete replayed transcript.
    Disabled,
}

/// Reflection trigger configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectionConfig {
    /// Master switch for all reflection triggers and task creation.
    pub enabled: bool,
    /// Reflect every N delivered user turns; zero disables this trigger.
    pub turn_interval: u64,
}

impl Default for ReflectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            turn_interval: DEFAULT_TURN_INTERVAL,
        }
    }
}

/// Delivery facts from the foreground turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnDelivery {
    final_response: bool,
    interrupted: bool,
}

impl TurnDelivery {
    /// Construct the terminal delivery state observed by the interface.
    #[must_use]
    pub const fn new(final_response: bool, interrupted: bool) -> Self {
        Self {
            final_response,
            interrupted,
        }
    }

    const fn permits_reflection(self) -> bool {
        self.final_response && !self.interrupted
    }
}

/// Inputs captured after one foreground turn.
#[derive(Clone)]
pub struct ReflectionTurn {
    delivery: TurnDelivery,
    transcript: TurnTranscript,
    tool_context: ToolContext,
}

impl ReflectionTurn {
    /// Bundle the delivered result, transcript replay, and memory call context.
    #[must_use]
    pub fn new(
        delivery: TurnDelivery,
        transcript: TurnTranscript,
        tool_context: ToolContext,
    ) -> Self {
        Self {
            delivery,
            transcript,
            tool_context,
        }
    }
}

/// Immutable input handed to the isolated reflection model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionRequest {
    /// The foreground turn replayed without sharing its storage.
    pub transcript: TurnTranscript,
    /// Always disabled for this fork.
    pub compaction: CompactionMode,
    /// Review instructions, including the negative-learning safety list.
    pub prompt: Arc<str>,
}

/// One model-requested call inside reflection.
#[derive(Debug, Clone, PartialEq)]
pub struct ReflectionToolCall {
    id: String,
    name: String,
    args: Value,
}

impl ReflectionToolCall {
    /// Construct a raw tool request from the reflection model.
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, args: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            args,
        }
    }
}

/// A memory-only dispatcher owned by one reflection request.
#[derive(Clone)]
pub struct ReflectionTools {
    memory: Arc<dyn Tool>,
    context: ToolContext,
}

impl ReflectionTools {
    /// Dispatch a reflection tool call after enforcing the hard whitelist.
    pub async fn dispatch(
        &self,
        call: ReflectionToolCall,
    ) -> Result<ToolOutput, ReflectionDispatchError> {
        if call.name != MEMORY_TOOL_ID {
            return Err(ReflectionDispatchError::Denied { tool: call.name });
        }

        let context = self.context.for_subcall(call.id);
        self.memory
            .execute(call.args, context)
            .await
            .map_err(|error| ReflectionDispatchError::Execution {
                tool: MEMORY_TOOL_ID.to_owned(),
                detail: error.to_string(),
            })
    }
}

/// A reflection dispatch refusal or memory-tool failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectionDispatchError {
    /// The model attempted a tool outside the memory-only whitelist.
    Denied { tool: String },
    /// The injected memory tool failed.
    Execution { tool: String, detail: String },
}

impl fmt::Display for ReflectionDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { tool } => write!(
                formatter,
                "Background review denied non-whitelisted tool: {tool}. Only memory tools are allowed."
            ),
            Self::Execution { tool, detail } => {
                write!(formatter, "Background review tool {tool} failed: {detail}")
            }
        }
    }
}

impl Error for ReflectionDispatchError {}

impl From<ReflectionDispatchError> for ReflectionError {
    fn from(error: ReflectionDispatchError) -> Self {
        Self::new(error.to_string())
    }
}

/// Runs the isolated model loop over one reflection request.
#[async_trait]
pub trait ReflectionRunner: Send + Sync {
    /// Review the transcript and optionally call the supplied memory-only dispatcher.
    async fn review(
        &self,
        request: ReflectionRequest,
        tools: ReflectionTools,
    ) -> Result<(), ReflectionError>;
}

/// Construction failure for a reflection fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionSetupError {
    tool: String,
}

impl fmt::Display for ReflectionSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "reflection requires tool `{MEMORY_TOOL_ID}`, received `{}`",
            self.tool
        )
    }
}

impl Error for ReflectionSetupError {}

/// Schedules best-effort, post-delivery reflection tasks.
pub struct ReflectionFork {
    config: ReflectionConfig,
    runner: Arc<dyn ReflectionRunner>,
    memory: Arc<dyn Tool>,
    delivered_turns: AtomicU64,
}

impl ReflectionFork {
    /// Build a fork around an injected runner and concrete memory tool.
    pub fn new(
        config: ReflectionConfig,
        runner: Arc<dyn ReflectionRunner>,
        memory: Arc<dyn Tool>,
    ) -> Result<Self, ReflectionSetupError> {
        if memory.id() != MEMORY_TOOL_ID {
            return Err(ReflectionSetupError {
                tool: memory.id().to_owned(),
            });
        }
        Ok(Self {
            config,
            runner,
            memory,
            delivered_turns: AtomicU64::new(0),
        })
    }

    /// Spawn only when delivery, trigger, and negative-learning policy all permit it.
    #[must_use]
    pub fn spawn_after_turn(&self, turn: ReflectionTurn) -> Option<JoinHandle<()>> {
        if !self.config.enabled || !turn.delivery.permits_reflection() {
            return None;
        }

        let periodic = self.periodic_trigger();
        let recovered = turn.transcript.has_failure_recovery();
        if (!periodic && !recovered) || turn.transcript.is_negative_learning() {
            return None;
        }

        let Ok(runtime) = Handle::try_current() else {
            tracing::warn!("background reflection skipped without a Tokio runtime");
            return None;
        };
        let request = ReflectionRequest {
            transcript: turn.transcript,
            compaction: CompactionMode::Disabled,
            prompt: reflection_prompt(),
        };
        let tools = ReflectionTools {
            memory: Arc::clone(&self.memory),
            context: turn.tool_context,
        };
        let runner = Arc::clone(&self.runner);

        Some(runtime.spawn(async move {
            let review = tokio::spawn(async move { runner.review(request, tools).await });
            match review.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "background reflection failed");
                }
                Err(error) => {
                    tracing::warn!(error = %error, "background reflection panicked");
                }
            }
        }))
    }

    fn periodic_trigger(&self) -> bool {
        let interval = self.config.turn_interval;
        if interval == 0 {
            return false;
        }
        let turn = self.delivered_turns.fetch_add(1, Ordering::Relaxed) + 1;
        turn.is_multiple_of(interval)
    }
}

fn reflection_prompt() -> Arc<str> {
    let mut prompt = String::from(
        "Review the completed turn and save only durable user or project facts with memory.\n\nDo NOT capture:\n",
    );
    for item in NEGATIVE_LEARNING_LIST {
        prompt.push_str("  • ");
        prompt.push_str(item);
        prompt.push('\n');
    }
    prompt.push_str(
        "\nYou can only call the memory tool. Other tools will be denied at runtime — do not attempt them.",
    );
    Arc::from(prompt)
}
