//! Effective authority an attempt may hand to a child.
//!
//! This is data, not a permission resolver. The host captures its final ordered
//! rules, actual resource contract, and authorized registry, including deferred
//! tools whose schemas have not yet been sent to the provider.

use serde::{Deserialize, Serialize};

use crate::snapshot::{SandboxCapabilityDescriptor, ToolSchemaIdentity};

/// Cross-cutting permission mode already resolved by the parent host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionModeSnapshot {
    Standard,
    Strict,
    AllowAll,
}

/// One closed permission decision, preserving the parent's rule precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionActionSnapshot {
    Allow,
    Ask,
    Deny,
}

/// A complete rule; sources are audit data and order remains significant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRuleSnapshot {
    pub permission: String,
    pub pattern: String,
    pub action: PermissionActionSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The upper bound for the next child invocation, not its previous invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParentAuthoritySnapshot {
    pub permission_mode: PermissionModeSnapshot,
    /// Absolute, resolved parent workspace anchoring the resource contract.
    pub workspace: String,
    /// Final effective rules, including dynamic resource constraints and overrides.
    pub rules: Vec<PermissionRuleSnapshot>,
    /// Actual parent Shell contract, with absolute roots and protected paths.
    /// A native backend retains the requested contract; it does not claim OS
    /// confinement. Network uses `deny`/`allow`, backend uses `auto`/`native`.
    pub sandbox: SandboxCapabilityDescriptor,
    /// Trusted parent fallback choice: `deny` or `run-unconfined`. The sandbox
    /// resolver still rejects fallback for a read-only child contract.
    pub sandbox_on_unavailable: String,
    /// All authorized tools, including deferred MCP schemas. Provider exposure is
    /// separately recorded by `AttemptSnapshot.tools` and never grants authority.
    pub tools: Vec<ToolSchemaIdentity>,
}
