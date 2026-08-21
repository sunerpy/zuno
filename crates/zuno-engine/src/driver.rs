//! Replaceable drivers for one agent turn.

use crate::r#loop::{
    RunTurnRequest, TurnContext, TurnError, TurnEventSender, TurnOutcome, run_turn,
};
use async_trait::async_trait;
use futures::future::BoxFuture;
use std::sync::Arc;
use zuno_runtime::{Component, MountContext, RuntimeError};

/// Stable component id used by profiles that replace the active driver.
pub const AGENT_DRIVER_COMPONENT_ID: &str = "agent-driver";

/// Executes one turn using the services assembled for a session.
pub trait AgentDriver: Send + Sync {
    /// Human-readable implementation name used in diagnostics.
    fn name(&self) -> &str;

    /// Execute one turn.
    fn drive<'a>(
        &'a self,
        request: RunTurnRequest,
        context: TurnContext<'a>,
        events: TurnEventSender,
    ) -> BoxFuture<'a, Result<TurnOutcome, TurnError>>;
}

/// The built-in provider/tool loop.
#[derive(Debug, Default)]
pub struct DefaultAgentDriver;

impl AgentDriver for DefaultAgentDriver {
    fn name(&self) -> &str {
        "default"
    }

    fn drive<'a>(
        &'a self,
        request: RunTurnRequest,
        context: TurnContext<'a>,
        events: TurnEventSender,
    ) -> BoxFuture<'a, Result<TurnOutcome, TurnError>> {
        Box::pin(run_turn(request, context, events))
    }
}

/// Runtime component that publishes the selected agent driver.
pub struct AgentDriverComponent {
    driver: Arc<dyn AgentDriver>,
}

impl AgentDriverComponent {
    /// Create the stable driver contribution for a profile scope.
    #[must_use]
    pub fn new(driver: Arc<dyn AgentDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait]
impl Component for AgentDriverComponent {
    fn id(&self) -> &str {
        AGENT_DRIVER_COMPONENT_ID
    }

    async fn mount(&self, context: &mut MountContext) -> Result<(), RuntimeError> {
        context.provide::<dyn AgentDriver>(Arc::clone(&self.driver))
    }
}
