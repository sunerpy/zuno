//! Replaceable drivers for one agent turn.

use crate::advance::{AdvanceError, AdvanceOutcome, AdvanceRequest};
use crate::r#loop::{
    RunTurnRequest, TurnContext, TurnError, TurnEventSender, TurnOutcome, advance_turn, run_turn,
};
use async_trait::async_trait;
use futures::future::BoxFuture;
use std::sync::Arc;
use zuno_runtime::{Component, PrepareContext, RuntimeError};

/// Stable component id used by profiles that replace the active driver.
pub const AGENT_DRIVER_COMPONENT_ID: &str = "agent-driver";

/// Executes one turn using the services assembled for a session.
pub trait AgentDriver: Send + Sync {
    /// Human-readable implementation name used in diagnostics.
    fn name(&self) -> &str;

    /// Whether this driver can issue and consume the bounded checkpoint contract.
    fn supports_advance(&self) -> bool {
        false
    }

    /// Execute a bounded segment. Hosts must reject incompatible drivers before
    /// dispatching a recoverable job; whole-turn drivers remain available locally.
    fn advance<'a>(
        &'a self,
        _request: AdvanceRequest,
        _context: TurnContext<'a>,
        _events: TurnEventSender,
    ) -> BoxFuture<'a, Result<AdvanceOutcome, AdvanceError>> {
        Box::pin(async { Err(AdvanceError::UnsupportedDriver(self.name().to_owned())) })
    }

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

    fn supports_advance(&self) -> bool {
        true
    }

    fn advance<'a>(
        &'a self,
        request: AdvanceRequest,
        context: TurnContext<'a>,
        events: TurnEventSender,
    ) -> BoxFuture<'a, Result<AdvanceOutcome, AdvanceError>> {
        Box::pin(advance_turn(request, context, events))
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

    async fn prepare(&self, context: &mut PrepareContext) -> Result<(), RuntimeError> {
        context.provide::<dyn AgentDriver>(Arc::clone(&self.driver))
    }
}
