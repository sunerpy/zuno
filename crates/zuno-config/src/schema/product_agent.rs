//! Configuration for process-owned Codex and Claude Code subagents.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A supported external coding-agent product.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProductAgentKind {
    /// OpenAI Codex CLI through its app-server protocol.
    Codex,
    /// Anthropic Claude Code through its non-interactive stream-json CLI.
    ClaudeCode,
}

impl ProductAgentKind {
    /// The executable name inherited from the user's native installation.
    #[must_use]
    pub const fn default_command(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude",
        }
    }

    /// The default static tool name.
    #[must_use]
    pub const fn default_tool_name(self) -> &'static str {
        match self {
            Self::Codex => "subagent_codex",
            Self::ClaudeCode => "subagent_claude_code",
        }
    }
}

/// Native permission vocabulary accepted by either supported product.
///
/// Validation keeps product-specific values from crossing the boundary. One enum
/// keeps the JSON schema useful while [`ProductAgentConfig::validate`] enforces the
/// actual Codex/Claude Code split.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProductAgentPermissionMode {
    /// Codex: never ask Zuno for approval.
    #[serde(rename = "never")]
    Never,
    /// Codex: ask only for commands outside its trusted set.
    #[serde(rename = "unlessTrusted")]
    UnlessTrusted,
    /// Codex: request approval when its policy requires it.
    #[serde(rename = "onRequest")]
    OnRequest,
    /// Codex: explicit full-access bypass.
    #[serde(rename = "dangerouslyBypassApprovals")]
    DangerouslyBypassApprovals,
    /// Claude Code asks before protected operations. Zuno still disables interactive questions.
    #[serde(rename = "manual")]
    Manual,
    /// Claude Code selects its own non-interactive policy.
    #[serde(rename = "auto")]
    Auto,
    /// Claude Code may apply edits without asking.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// Claude Code refuses operations that would require a prompt.
    #[serde(rename = "dontAsk")]
    DontAsk,
    /// Claude Code's explicit permission bypass.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    /// Claude Code's read-only planning policy.
    #[serde(rename = "plan")]
    Plan,
}

impl ProductAgentPermissionMode {
    /// The exact native CLI value for Claude Code.
    #[must_use]
    pub const fn as_claude_code(self) -> Option<&'static str> {
        match self {
            Self::Manual => Some("manual"),
            Self::Auto => Some("auto"),
            Self::AcceptEdits => Some("acceptEdits"),
            Self::DontAsk => Some("dontAsk"),
            Self::BypassPermissions => Some("bypassPermissions"),
            Self::Plan => Some("plan"),
            Self::Never
            | Self::UnlessTrusted
            | Self::OnRequest
            | Self::DangerouslyBypassApprovals => None,
        }
    }

    /// The Codex app-server approval-policy value, when this is a Codex mode.
    #[must_use]
    pub const fn as_codex_approval_policy(self) -> Option<&'static str> {
        match self {
            Self::Never | Self::DangerouslyBypassApprovals => Some("never"),
            Self::UnlessTrusted => Some("unlessTrusted"),
            Self::OnRequest => Some("onRequest"),
            Self::Manual
            | Self::Auto
            | Self::AcceptEdits
            | Self::DontAsk
            | Self::BypassPermissions
            | Self::Plan => None,
        }
    }

    /// Whether this mode deliberately bypasses the product's safety boundary.
    #[must_use]
    pub const fn is_dangerous(self) -> bool {
        matches!(
            self,
            Self::DangerouslyBypassApprovals | Self::BypassPermissions
        )
    }
}

/// One named product-agent instance.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductAgentConfig {
    /// Which native product implements this instance.
    pub kind: ProductAgentKind,
    /// Disabled unless explicitly true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Executable or executable path. Defaults to the product's normal command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Static model-visible tool name.
    #[serde(default, rename = "toolName", skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Native product permission policy.
    #[serde(
        default,
        rename = "permissionMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<ProductAgentPermissionMode>,
    /// Additional child-process environment values.
    ///
    /// Ordinary process variables, including proxy variables, are inherited even when
    /// this map is absent. These entries only overlay that inherited environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
}

impl ProductAgentConfig {
    /// Whether this configured instance contributes a tool.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled == Some(true)
    }

    /// The executable after applying the native default.
    #[must_use]
    pub fn resolved_command(&self) -> &str {
        self.command
            .as_deref()
            .unwrap_or_else(|| self.kind.default_command())
    }

    /// The tool name after applying the native default.
    #[must_use]
    pub fn resolved_tool_name(&self) -> &str {
        self.tool_name
            .as_deref()
            .unwrap_or_else(|| self.kind.default_tool_name())
    }

    /// The permission mode after applying the non-dangerous product default.
    #[must_use]
    pub fn resolved_permission_mode(&self) -> ProductAgentPermissionMode {
        self.permission_mode.unwrap_or(match self.kind {
            ProductAgentKind::Codex => ProductAgentPermissionMode::Never,
            ProductAgentKind::ClaudeCode => ProductAgentPermissionMode::DontAsk,
        })
    }

    /// Validate values whose meaning depends on the selected product.
    pub fn validate(&self, instance: &str) -> Result<(), String> {
        if instance.trim().is_empty() {
            return Err("product-agent instance names must not be empty".to_owned());
        }
        if self.resolved_command().trim().is_empty() {
            return Err(format!("productAgent.{instance}.command must not be empty"));
        }
        let tool = self.resolved_tool_name();
        let mut chars = tool.chars();
        if chars
            .next()
            .is_none_or(|first| !(first == '_' || first.is_ascii_alphabetic()))
            || chars.any(|character| !(character == '_' || character.is_ascii_alphanumeric()))
        {
            return Err(format!(
                "productAgent.{instance}.toolName `{tool}` must start with an ASCII letter or \
                 underscore and contain only ASCII letters, digits, or underscores"
            ));
        }
        let permission = self.resolved_permission_mode();
        let valid = match self.kind {
            ProductAgentKind::Codex => permission.as_codex_approval_policy().is_some(),
            ProductAgentKind::ClaudeCode => permission.as_claude_code().is_some(),
        };
        if !valid {
            return Err(format!(
                "productAgent.{instance}.permissionMode is not valid for {:?}",
                self.kind
            ));
        }
        Ok(())
    }
}
