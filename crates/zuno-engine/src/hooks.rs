use async_trait::async_trait;
use serde_json::Value;
use zuno_llm::event::Message;
use zuno_llm::registry::CompletionRequest;
use zuno_permission::PermissionRequest;
use zuno_tool::{ToolDefinition, ToolOutput};

use crate::r#loop::{ResolvedAgent, ResolvedModel, TurnEvent};

#[derive(Debug, Clone, Copy)]
pub struct RequestHookInput<'a> {
    pub session_id: &'a str,
    pub agent: &'a ResolvedAgent,
    pub model: &'a ResolvedModel,
    pub message: &'a Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionHookDecision {
    Ask,
    Deny,
    Allow,
}

#[async_trait]
pub trait TurnHooks: Send + Sync {
    fn enabled(&self) -> bool {
        false
    }

    async fn event(&self, _event: &TurnEvent) -> Result<(), String> {
        Ok(())
    }

    async fn transform_messages(
        &self,
        _session_id: &str,
        _messages: &mut Vec<HookMessageWithParts>,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn transform_system(
        &self,
        _session_id: &str,
        _model: &ResolvedModel,
        _system: &mut Vec<String>,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn tool_definition(&self, _definition: &mut ToolDefinition) -> Result<(), String> {
        Ok(())
    }

    async fn prepare_request(
        &self,
        _input: RequestHookInput<'_>,
        _request: &mut CompletionRequest,
    ) -> Result<(), String> {
        Ok(())
    }

    /// One contiguous run of assistant text, before it is persisted.
    ///
    /// This fires once per text segment of a step, not once per step: a step whose
    /// text is interrupted by a tool call or a reasoning item is persisted as several
    /// text parts in stream order, and `part_id` names the one segment handed over
    /// (`prt_{turn}_{step}_{position}_text`). A hook rewrites exactly that segment.
    async fn text_complete(
        &self,
        _session_id: &str,
        _message_id: &str,
        _part_id: &str,
        _text: &mut String,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// One provider-projected message paired with the stored parts that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookMessageWithParts {
    pub info: Message,
    pub parts: Vec<zuno_db::message::PartRecord>,
}

#[async_trait]
pub trait ToolHooks: Send + Sync {
    async fn before(
        &self,
        _tool: &str,
        _session_id: &str,
        _call_id: &str,
        _args: &mut Value,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn permission(
        &self,
        _request: &PermissionRequest,
    ) -> Result<PermissionHookDecision, String> {
        Ok(PermissionHookDecision::Ask)
    }

    async fn after(
        &self,
        _tool: &str,
        _session_id: &str,
        _call_id: &str,
        _args: &Value,
        _output: &mut ToolOutput,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct NoopHooks;

impl TurnHooks for NoopHooks {}
impl ToolHooks for NoopHooks {}
