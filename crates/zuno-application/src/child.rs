//! Native child Job admission. The host resolves a configuration grant; wire
//! intent cannot grant itself another model, owner or delegation limit.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zuno_types::{
    identity::{InvocationId, JobId, SessionId},
    wait::WaitRef,
};

use crate::{
    ApplicationError,
    runtime::{ConfigurationRef, ExecutionLease, JobInputSelection},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildDelivery {
    Foreground,
    NextStep,
    Quiet,
}

/// A normalized native delegation, bound to the original tool invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildInvocation {
    pub invocation_id: InvocationId,
    pub arguments_sha256: String,
    pub logical_key: String,
    pub prompt: String,
    pub description: String,
    pub delivery: ChildDelivery,
    pub resume_session_id: Option<SessionId>,
    /// Native task presentation, retained for the eventual original tool result.
    /// It is data, never permission or a checkpoint.
    pub presentation: Value,
}

impl ChildInvocation {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if self.arguments_sha256.len() != 64
            || !self
                .arguments_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.logical_key.is_empty()
            || self.logical_key.len() > 256
            || self.prompt.trim().is_empty()
            || self.prompt.len() > crate::MAX_INPUT_BYTES
            || self.prompt.contains('\0')
            || self.description.trim().is_empty()
            || self.description.len() > 4096
            || !self.presentation.is_object()
            || serde_json::to_vec(&self.presentation)
                .map_err(ApplicationError::storage)?
                .len()
                > 32768
        {
            return Err(ApplicationError::Invalid(
                "invalid bounded child invocation".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Constructed by the control plane from its installed immutable definitions.
/// Deliberately not a deserializable client grant.
#[derive(Debug, Clone)]
pub struct ChildDefinitionGrant {
    pub parent: ConfigurationRef,
    pub child: ConfigurationRef,
    pub selection: JobInputSelection,
    pub maximum_depth: u32,
    pub maximum_children: u32,
}
impl ChildDefinitionGrant {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.parent.validate()?;
        self.child.validate()?;
        self.selection.validate()?;
        if !(1..=16).contains(&self.maximum_depth) || !(1..=256).contains(&self.maximum_children) {
            return Err(ApplicationError::Invalid(
                "invalid configured child limits".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildDispatch {
    pub job_id: JobId,
    pub session_id: SessionId,
    pub wait: WaitRef,
    pub delivery: ChildDelivery,
}

#[async_trait]
pub trait ChildDispatchStore: Send + Sync {
    async fn delegation_depth(&self, lease: &ExecutionLease) -> Result<u32, ApplicationError>;
    /// Foreground requests stage intent; the parent's waiting checkpoint admits
    /// the executable child atomically. Background admission commits immediately.
    async fn dispatch_child(
        &self,
        lease: &ExecutionLease,
        invocation: ChildInvocation,
        grant: &ChildDefinitionGrant,
    ) -> Result<ChildDispatch, ApplicationError>;
}

/// The data owner installs immutable allowed targets. A Worker cannot resolve a
/// model or capability outside its parent's configured delegation map.
pub trait ChildDefinitionCatalog: Send + Sync {
    fn resolve(
        &self,
        parent: &ConfigurationRef,
        agent: &str,
        model: Option<&str>,
    ) -> Option<ChildDefinitionGrant>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ChildCommand {
    Depth,
    Dispatch {
        agent: String,
        model: Option<String>,
        invocation: Box<ChildInvocation>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChildReply {
    Depth { depth: u32 },
    Dispatch { dispatch: ChildDispatch },
}
