//! One immutable Agent snapshot consumed by prompt, model, delegation, and tools.
//!
//! Discovery and configuration produce [`zuno_catalog::agent::Agent`]. Permission
//! composition produces the final ordered rules. Keeping those values separate at
//! every consumer let the prompt describe one role while the registry enforced
//! another. [`AgentProfile`] joins them once at the composition boundary.

use std::collections::BTreeSet;

use zuno_catalog::agent::Agent;
use zuno_permission::visibility::is_tool_hidden;
use zuno_permission::{PermissionAction, Rule};

use crate::builtin::{self, ExtensionTools};

/// Filesystem authority the OS sandbox must compile for Shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFilesystemAccess {
    /// The Agent may inspect through Shell but cannot change host files.
    ReadOnly,
    /// The Agent may change the workspace and explicitly approved roots.
    WorkspaceWrite,
}

/// Runtime-enforced capabilities for one Agent attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityPolicy {
    rules: Vec<Rule>,
    delegation_targets: Option<Vec<String>>,
    tool_authority: Option<BTreeSet<String>>,
}

impl CapabilityPolicy {
    fn resolve(definition: &Agent, rules: Vec<Rule>, vision_available: bool) -> Self {
        Self {
            rules,
            delegation_targets: definition.delegates.as_ref().map(|targets| {
                targets
                    .iter()
                    .filter(|target| delegation_target_available(target, vision_available))
                    .cloned()
                    .collect()
            }),
            tool_authority: None,
        }
    }

    /// Ordered rules used both for model visibility and dispatch authorization.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Exact child-Agent allowlist, when the profile declares one.
    #[must_use]
    pub fn delegation_targets(&self) -> Option<&[String]> {
        self.delegation_targets.as_deref()
    }

    /// Exact parent-attempt tool upper bound, when this is a delegated turn.
    #[must_use]
    pub const fn tool_authority(&self) -> Option<&BTreeSet<String>> {
        self.tool_authority.as_ref()
    }

    /// Whether a tool survives the parent-attempt authority upper bound.
    #[must_use]
    pub fn within_tool_authority(&self, tool: &str) -> bool {
        self.tool_authority
            .as_ref()
            .is_none_or(|authority| authority.contains(tool))
    }

    /// Whether both the role rules and parent-attempt authority expose a tool.
    #[must_use]
    pub fn tool_available(&self, tool: &str) -> bool {
        self.within_tool_authority(tool) && !is_tool_hidden(tool, &self.rules)
    }

    /// Whether the final capability intersection exposes delegation.
    #[must_use]
    pub fn can_delegate(&self) -> bool {
        self.tool_available("task")
    }

    fn can_edit(&self) -> bool {
        ["apply_patch", "write", "edit"]
            .into_iter()
            .any(|tool| self.tool_available(tool))
    }

    /// Filesystem authority derived from the effective edit capability.
    ///
    /// This uses the frozen rules and parent-attempt authority, so a custom or
    /// delegated Agent cannot regain write access merely by retaining Shell.
    #[must_use]
    pub fn shell_filesystem_access(&self) -> ShellFilesystemAccess {
        if self.can_edit() {
            ShellFilesystemAccess::WorkspaceWrite
        } else {
            ShellFilesystemAccess::ReadOnly
        }
    }
}

/// Catalog definition plus the exact capabilities enforced for one attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentProfile {
    definition: Agent,
    capabilities: CapabilityPolicy,
    vision_available: bool,
    extension_rule_index: Option<usize>,
}

impl AgentProfile {
    /// Freeze a resolved catalog entry and permission rules into one profile.
    #[must_use]
    pub fn resolve(definition: Agent, rules: Vec<Rule>, vision_available: bool) -> Self {
        Self::resolve_inner(definition, rules, vision_available, None)
    }

    /// Freeze a profile and remember where extension grants precede user overrides.
    #[must_use]
    pub fn resolve_with_extension_boundary(
        definition: Agent,
        rules: Vec<Rule>,
        extension_rule_index: usize,
        vision_available: bool,
    ) -> Self {
        assert!(
            extension_rule_index <= rules.len(),
            "extension rule boundary must be inside the resolved ruleset"
        );
        Self::resolve_inner(
            definition,
            rules,
            vision_available,
            Some(extension_rule_index),
        )
    }

    fn resolve_inner(
        definition: Agent,
        rules: Vec<Rule>,
        vision_available: bool,
        extension_rule_index: Option<usize>,
    ) -> Self {
        let capabilities = CapabilityPolicy::resolve(&definition, rules, vision_available);
        Self {
            definition,
            capabilities,
            vision_available,
            extension_rule_index,
        }
    }

    /// Restrict this profile to the tools visible in the delegating attempt.
    ///
    /// Role rules remain intact for auditability, but every runtime and prompt
    /// capability query observes the intersection. A child can therefore reduce
    /// authority further and can never regain a tool omitted from its parent request.
    #[must_use]
    pub fn with_tool_authority(mut self, tools: impl IntoIterator<Item = String>) -> Self {
        self.capabilities.tool_authority = Some(tools.into_iter().collect());
        self
    }

    /// Resolve extension tools through the native role boundary before user rules.
    #[must_use]
    pub fn rules_with_extension_tools(&self, extension_tool_ids: &[&str]) -> Vec<Rule> {
        let mut rules = self.capabilities.rules.clone();
        let Some(index) = self.extension_rule_index else {
            return rules;
        };
        let inherits = builtin::get(&self.definition.name, self.vision_available)
            .is_some_and(|agent| agent.permissions.extension_tools == ExtensionTools::Inherit);
        if !inherits {
            return rules;
        }

        let grants = extension_tool_ids
            .iter()
            .copied()
            .filter(|tool| self.capabilities.within_tool_authority(tool))
            .map(|tool| Rule {
                permission: tool.to_owned(),
                pattern: "*".to_owned(),
                action: PermissionAction::Allow,
            })
            .collect::<Vec<_>>();
        rules.splice(index..index, grants);
        rules
    }

    /// Stable Agent identity.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.definition.name
    }

    /// The resolved catalog definition, including prompt and model policy.
    #[must_use]
    pub const fn definition(&self) -> &Agent {
        &self.definition
    }

    /// The exact runtime capability snapshot.
    #[must_use]
    pub const fn capabilities(&self) -> &CapabilityPolicy {
        &self.capabilities
    }

    /// Role-specific delegation boundary used by the provider-step runtime policy.
    ///
    /// Capability availability is deliberately not rendered here. The dispatcher can
    /// still remove tools after profile resolution, so the final model-visible policy
    /// is generated only after the provider-step tool snapshot is locked.
    #[must_use]
    pub fn delegation_guidance(&self) -> Option<String> {
        builtin::get(&self.definition.name, self.vision_available)
            .map(|native| native.prompt_policy())
    }
}

fn delegation_target_available(target: &str, vision_available: bool) -> bool {
    if vision_available {
        return true;
    }

    // Preserve configured/custom names for the composition root's explicit
    // validation. Only omit a target known to the native roster and known to be
    // absent solely because its capability gate is closed.
    builtin::get(target, false).is_some() || builtin::get(target, true).is_none()
}
