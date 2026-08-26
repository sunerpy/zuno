//! One immutable Agent snapshot consumed by prompt, model, delegation, and tools.
//!
//! Discovery and configuration produce [`zuno_catalog::agent::Agent`]. Permission
//! composition produces the final ordered rules. Keeping those values separate at
//! every consumer let the prompt describe one role while the registry enforced
//! another. [`AgentProfile`] joins them once at the composition boundary.

use zuno_catalog::agent::Agent;
use zuno_permission::Rule;
use zuno_permission::visibility::is_tool_hidden;

use crate::builtin;

/// Runtime-enforced capabilities for one Agent attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityPolicy {
    rules: Vec<Rule>,
    delegation_targets: Option<Vec<String>>,
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

    /// Whether the final rules expose the delegation permission.
    #[must_use]
    pub fn can_delegate(&self) -> bool {
        !is_tool_hidden("task", &self.rules)
    }

    fn can_edit(&self) -> bool {
        ["apply_patch", "write", "edit"]
            .into_iter()
            .any(|tool| !is_tool_hidden(tool, &self.rules))
    }

    fn can_shell(&self) -> bool {
        !is_tool_hidden("shell", &self.rules)
    }

    fn can_research_externally(&self) -> bool {
        !is_tool_hidden("webfetch", &self.rules) || !is_tool_hidden("web_search", &self.rules)
    }
}

/// Catalog definition plus the exact capabilities enforced for one attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentProfile {
    definition: Agent,
    capabilities: CapabilityPolicy,
    prompt_policy: String,
}

impl AgentProfile {
    /// Freeze a resolved catalog entry and permission rules into one profile.
    #[must_use]
    pub fn resolve(definition: Agent, rules: Vec<Rule>, vision_available: bool) -> Self {
        let capabilities = CapabilityPolicy::resolve(&definition, rules, vision_available);
        let prompt_policy = render_prompt_policy(&definition, &capabilities, vision_available);
        Self {
            definition,
            capabilities,
            prompt_policy,
        }
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

    /// Model-visible routing advice plus an authoritative capability summary.
    #[must_use]
    pub fn prompt_policy(&self) -> &str {
        &self.prompt_policy
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

fn render_prompt_policy(
    definition: &Agent,
    capabilities: &CapabilityPolicy,
    vision_available: bool,
) -> String {
    let mut blocks = Vec::new();
    if let Some(native) = builtin::get(&definition.name, vision_available) {
        blocks.push(native.prompt_policy());
    }

    let delegation = if capabilities.can_delegate() {
        match capabilities.delegation_targets() {
            Some(targets) => format!("available (targets: {})", targets.join(", ")),
            None => "available".to_owned(),
        }
    } else {
        "unavailable".to_owned()
    };
    blocks.push(format!(
        "Enforced capability snapshot for this attempt:\n\
         - delegation: {delegation}\n\
         - workspace edits: {}\n\
         - shell: {}\n\
         - external research: {}\n\
         The runtime capability snapshot is authoritative when prose and available \
         tools appear to disagree.",
        availability(capabilities.can_edit()),
        availability(capabilities.can_shell()),
        availability(capabilities.can_research_externally()),
    ));
    blocks.join("\n\n")
}

fn availability(available: bool) -> &'static str {
    if available {
        "available"
    } else {
        "unavailable"
    }
}
