//! The small Rust surface for out-of-process OpenCode plugins.
//!
//! A plugin builds one [`Plugin`] and hands it to [`serve`]. Standard output is
//! reserved for newline-delimited JSON-RPC; application diagnostics belong on
//! standard error so one stray log line cannot corrupt the host connection.

mod conformance;
mod generated_client;
mod protocol;
mod server;

pub use crate::conformance::*;
pub use crate::generated_client::*;
pub use crate::protocol::*;
pub use crate::server::{serve, serve_io};

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;

/// Callback failure returned to the host as a JSON-RPC error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct HandlerError {
    message: String,
}

impl HandlerError {
    /// Keep callback errors displayable without exposing a host-specific type.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Result returned by tool and hook handlers.
pub type HandlerResult<T> = Result<T, HandlerError>;

trait HookHandler: Send + Sync {
    fn call(&self, call: HookCall) -> BoxFuture<'static, HandlerResult<HookCall>>;
}

struct HookFn<F>(F);

impl<F, Fut> HookHandler for HookFn<F>
where
    F: Fn(HookCall) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HandlerResult<HookCall>> + Send + 'static,
{
    fn call(&self, call: HookCall) -> BoxFuture<'static, HandlerResult<HookCall>> {
        Box::pin((self.0)(call))
    }
}

trait ToolHandler: Send + Sync {
    fn call(&self, call: ToolCall) -> BoxFuture<'static, HandlerResult<ToolOutput>>;
}

struct ToolFn<F>(F);

impl<F, Fut> ToolHandler for ToolFn<F>
where
    F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HandlerResult<ToolOutput>> + Send + 'static,
{
    fn call(&self, call: ToolCall) -> BoxFuture<'static, HandlerResult<ToolOutput>> {
        Box::pin((self.0)(call))
    }
}

struct RegisteredTool {
    definition: ToolDefinition,
    handler: Arc<dyn ToolHandler>,
}

/// A resident plugin definition served over standard input and output.
pub struct Plugin {
    id: String,
    hook_order: Vec<String>,
    hooks: Vec<(String, Arc<dyn HookHandler>)>,
    tools: Vec<RegisteredTool>,
}

impl Plugin {
    /// Start a plugin definition with a stable diagnostic identity.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            hook_order: Vec::new(),
            hooks: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// Register one callback hook under its exact JavaScript property name.
    ///
    /// # Errors
    /// Returns [`BuildError`] for an unknown or duplicate hook name.
    pub fn hook<F, Fut>(mut self, name: impl Into<String>, handler: F) -> Result<Self, BuildError>
    where
        F: Fn(HookCall) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<HookCall>> + Send + 'static,
    {
        let name = name.into();
        if !HOOK_NAMES.contains(&name.as_str())
            || matches!(name.as_str(), "tool" | "auth" | "provider")
        {
            return Err(BuildError::UnknownHook { name });
        }
        if self.hook_order.iter().any(|existing| existing == &name) {
            return Err(BuildError::DuplicateHook { name });
        }
        self.hook_order.push(name.clone());
        self.hooks.push((name, Arc::new(HookFn(handler))));
        Ok(self)
    }

    /// Register one remotely executable tool.
    ///
    /// # Errors
    /// Returns [`BuildError`] when the id is empty or already registered.
    pub fn tool<F, Fut>(
        mut self,
        definition: ToolDefinition,
        handler: F,
    ) -> Result<Self, BuildError>
    where
        F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<ToolOutput>> + Send + 'static,
    {
        if definition.id.trim().is_empty() {
            return Err(BuildError::EmptyToolId);
        }
        if self
            .tools
            .iter()
            .any(|tool| tool.definition.id == definition.id)
        {
            return Err(BuildError::DuplicateTool { id: definition.id });
        }
        self.tools.push(RegisteredTool {
            definition,
            handler: Arc::new(ToolFn(handler)),
        });
        if !self.hook_order.iter().any(|hook| hook == "tool") {
            self.hook_order.insert(0, "tool".to_owned());
        }
        Ok(self)
    }

    pub(crate) fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: self.id.clone(),
            hooks: self.hook_order.clone(),
            tools: self
                .tools
                .iter()
                .map(|tool| tool.definition.clone())
                .collect(),
        }
    }

    pub(crate) async fn call_hook(&self, call: HookCall) -> HandlerResult<HookCall> {
        let Some((_, handler)) = self.hooks.iter().find(|(name, _)| name == &call.hook) else {
            return Err(HandlerError::new(format!(
                "hook `{}` is not registered",
                call.hook
            )));
        };
        handler.call(call).await
    }

    pub(crate) async fn call_tool(&self, call: ToolCall) -> HandlerResult<ToolOutput> {
        let Some(tool) = self
            .tools
            .iter()
            .find(|tool| tool.definition.id == call.tool)
        else {
            return Err(HandlerError::new(format!(
                "tool `{}` is not registered",
                call.tool
            )));
        };
        tool.handler.call(call).await
    }

    pub(crate) fn validate(&self) -> Result<(), BuildError> {
        if self.id.trim().is_empty() {
            return Err(BuildError::EmptyPluginId);
        }
        let hooks = self.hook_order.iter().collect::<HashSet<_>>();
        if hooks.len() != self.hook_order.len() {
            return Err(BuildError::DuplicateManifestHook);
        }
        Ok(())
    }
}

/// A plugin definition is ambiguous or cannot be represented on the wire.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    #[error("plugin id must not be empty")]
    EmptyPluginId,
    #[error("tool id must not be empty")]
    EmptyToolId,
    #[error("unknown callback hook `{name}`")]
    UnknownHook { name: String },
    #[error("callback hook `{name}` was registered twice")]
    DuplicateHook { name: String },
    #[error("tool `{id}` was registered twice")]
    DuplicateTool { id: String },
    #[error("plugin manifest contains a duplicate hook")]
    DuplicateManifestHook,
}

/// Convenience constructor for object-shaped tool arguments.
#[must_use]
pub fn object(value: Value) -> Value {
    value
}
