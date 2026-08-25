//! Traceable system-prompt assembly.
//!
//! A prompt is ordered data before it is a string. Each section retains a stable
//! identifier, its source, its exact model-visible content, and a digest. The
//! rendered prompt is the sections joined by two newlines. Session events persist
//! this data together with the post-hook system prompt, so a past request remains
//! inspectable after source files or configuration change.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// One ordered source of system-prompt text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    id: String,
    source: String,
    content: String,
    sha256: String,
    selected_skill_name: Option<String>,
}

impl PromptSection {
    /// Stable section identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Human-readable source identity.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Exact section bytes sent before hook transformation.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// SHA-256 of [`Self::content`].
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Semantic role, trust level, and precedence used by provider mappings.
    #[must_use]
    pub fn semantics(&self) -> PromptSemantics {
        semantics(&self.id)
    }

    /// Deterministic local token estimate used before provider accounting exists.
    #[must_use]
    pub fn estimated_tokens(&self) -> u64 {
        u64::try_from(self.content.len().saturating_add(3) / 4).unwrap_or(u64::MAX)
    }

    /// Selected skill name retained separately from its source locator.
    #[must_use]
    pub fn selected_skill_name(&self) -> Option<&str> {
        self.selected_skill_name.as_deref()
    }

    /// Serialized event representation.
    fn value(&self, order: usize) -> Value {
        let mut value = json!({
            "id": self.id,
            "order": order,
            "source": self.source,
            "bytes": self.content.len(),
            "estimatedTokens": self.estimated_tokens(),
            "sha256": self.sha256,
            "role": self.semantics().role,
            "trust": self.semantics().trust,
            "priority": self.semantics().priority,
            "truncated": false,
            "content": self.content,
        });
        if let Some(name) = &self.selected_skill_name {
            value["skillName"] = Value::String(name.clone());
        }
        value
    }
}

/// Backwards-compatible name for one typed block in a [`PromptEnvelope`].
pub type PromptBlock = PromptSection;

/// Provider-independent meaning attached to one prompt block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptSemantics {
    /// Logical envelope field used when mapping to a provider protocol.
    pub role: &'static str,
    /// Trust boundary shown by prompt diagnostics.
    pub trust: &'static str,
    /// Stable precedence; larger values are more authoritative.
    pub priority: u16,
}

/// Structured instruction context before any provider-specific mapping.
///
/// Vectors intentionally preserve every source block and its ordering. Provider
/// mappings keep every non-native block as its own developer-context item, so
/// provenance and trust boundaries are not erased by string concatenation. The
/// user input is deliberately not duplicated here: it remains the typed user
/// [`zuno_llm::event::Message`] carried by the same prepared request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptEnvelope {
    pub kernel: Vec<PromptBlock>,
    pub agent_role: Vec<PromptBlock>,
    pub collaboration_mode: Vec<PromptBlock>,
    pub runtime_policy: Vec<PromptBlock>,
    pub global_instructions: Vec<PromptBlock>,
    pub project_instructions: Vec<PromptBlock>,
    pub work_state: Vec<PromptBlock>,
    pub routing: Vec<PromptBlock>,
    pub selected_skills: Vec<PromptBlock>,
    pub skill_index: Vec<PromptBlock>,
    pub memory: Vec<PromptBlock>,
    ordered: Vec<PromptBlock>,
}

impl PromptEnvelope {
    fn from_sections(sections: &[PromptSection]) -> Self {
        let mut envelope = Self {
            ordered: sections.to_vec(),
            ..Self::default()
        };
        for section in sections {
            let destination = match section.semantics().role {
                "kernel" => &mut envelope.kernel,
                "agent_role" => &mut envelope.agent_role,
                "collaboration_mode" => &mut envelope.collaboration_mode,
                "global_instructions" => &mut envelope.global_instructions,
                "project_instructions" => &mut envelope.project_instructions,
                "work_state" => &mut envelope.work_state,
                "routing" => &mut envelope.routing,
                "selected_skill" => &mut envelope.selected_skills,
                "skill_index" => &mut envelope.skill_index,
                "memory" => &mut envelope.memory,
                _ => &mut envelope.runtime_policy,
            };
            destination.push(section.clone());
        }
        envelope
    }

    fn join(blocks: impl IntoIterator<Item = PromptBlock>) -> String {
        blocks
            .into_iter()
            .map(|block| block.content)
            .filter(|content| !content.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Provider-neutral system messages.
    ///
    /// The first item combines only the native kernel and configured agent role;
    /// every remaining block stays independent and becomes one provider developer
    /// context item. Kernel blocks precede agent-role blocks regardless of source
    /// discovery order because their semantic authority is higher.
    #[must_use]
    pub fn system_messages(&self) -> Vec<String> {
        let instructions = Self::join(self.kernel.iter().chain(&self.agent_role).cloned());
        let mut messages = Vec::with_capacity(1 + self.ordered.len());
        if !instructions.is_empty() {
            messages.push(instructions);
        }
        messages.extend(
            self.ordered
                .iter()
                .filter(|block| !matches!(block.semantics().role, "kernel" | "agent_role"))
                .map(|block| block.content.clone())
                .filter(|content| !content.is_empty()),
        );
        messages
    }
}

/// Validation failure while constructing an assembly.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PromptAssemblyError {
    /// A section id cannot be empty or contain a line break.
    #[error("invalid prompt section id `{id}`")]
    InvalidId { id: String },
    /// Stable ids are unique within one prompt.
    #[error("duplicate prompt section id `{id}`")]
    DuplicateId { id: String },
}

/// Ordered system-prompt sections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptAssembly {
    sections: Vec<PromptSection>,
}

impl PromptAssembly {
    /// An empty assembly.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sections: Vec::new(),
        }
    }

    /// Add a non-empty section.
    ///
    /// Empty content is ignored so a disabled capability does not create a phantom
    /// section in the trace.
    ///
    /// # Errors
    ///
    /// Returns [`PromptAssemblyError`] when `id` is invalid or already present.
    pub fn push(
        &mut self,
        id: impl Into<String>,
        source: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<(), PromptAssemblyError> {
        let id = id.into();
        if id.is_empty() || id.contains(['\r', '\n']) {
            return Err(PromptAssemblyError::InvalidId { id });
        }
        if self.sections.iter().any(|section| section.id == id) {
            return Err(PromptAssemblyError::DuplicateId { id });
        }
        let content = content.into();
        if content.is_empty() {
            return Ok(());
        }
        self.sections.push(PromptSection {
            id,
            source: source.into(),
            sha256: sha256(&content),
            content,
            selected_skill_name: None,
        });
        Ok(())
    }

    /// Add one fully selected skill with durable name and source provenance.
    ///
    /// The section id is derived from the exact source locator. This makes loading
    /// idempotent per source while still allowing same-named skills from different
    /// locations to coexist after an explicit disambiguation.
    pub fn push_selected_skill(
        &mut self,
        name: impl Into<String>,
        source: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<(), PromptAssemblyError> {
        let name = name.into();
        let source = source.into();
        let content = content.into();
        if content.is_empty() {
            return Ok(());
        }
        let id = format!("skills.selected.{}", sha256(&source));
        if self.sections.iter().any(|section| section.id == id) {
            return Err(PromptAssemblyError::DuplicateId { id });
        }
        self.sections.push(PromptSection {
            id,
            source,
            sha256: sha256(&content),
            content,
            selected_skill_name: Some(name),
        });
        Ok(())
    }

    /// Ordered sections.
    #[must_use]
    pub fn sections(&self) -> &[PromptSection] {
        &self.sections
    }

    /// Typed, provider-independent view of these sections.
    #[must_use]
    pub fn envelope(&self) -> PromptEnvelope {
        PromptEnvelope::from_sections(&self.sections)
    }

    /// Provider-neutral system messages derived from [`Self::envelope`].
    #[must_use]
    pub fn system_messages(&self) -> Vec<String> {
        self.envelope().system_messages()
    }

    /// Canonical pre-hook text used for prompt receipts and cache identity.
    ///
    /// Message boundaries remain available through [`Self::system_messages`]; this
    /// joined form exists only for hashing and diagnostics.
    #[must_use]
    pub fn provider_projection(&self) -> String {
        self.system_messages().join("\n\n")
    }

    /// Exact pre-hook system prompt.
    #[must_use]
    pub fn render(&self) -> String {
        self.sections
            .iter()
            .map(|section| section.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// SHA-256 of [`Self::render`].
    #[must_use]
    pub fn sha256(&self) -> String {
        sha256(&self.render())
    }

    /// Durable properties for `session.prompt.assembled`.
    ///
    /// The ordered section contents reconstruct the pre-hook prompt. When hooks
    /// changed it, `actualSystemPrompt` stores the exact post-hook value as well.
    #[must_use]
    pub fn event_properties(
        &self,
        agent: &str,
        step: u32,
        actual_system_prompt: &str,
    ) -> Map<String, Value> {
        let assembled = self.provider_projection();
        let mut properties = Map::from_iter([
            ("agent".to_owned(), Value::String(agent.to_owned())),
            ("step".to_owned(), Value::from(step)),
            ("schemaVersion".to_owned(), Value::from(2)),
            (
                "assemblySha256".to_owned(),
                Value::String(sha256(&assembled)),
            ),
            (
                "actualSha256".to_owned(),
                Value::String(sha256(actual_system_prompt)),
            ),
            (
                "hookTransformed".to_owned(),
                Value::Bool(assembled != actual_system_prompt),
            ),
            (
                "sections".to_owned(),
                Value::Array(
                    self.sections
                        .iter()
                        .enumerate()
                        .map(|(order, section)| section.value(order))
                        .collect(),
                ),
            ),
        ]);
        if assembled != actual_system_prompt {
            properties.insert(
                "actualSystemPrompt".to_owned(),
                Value::String(actual_system_prompt.to_owned()),
            );
        }
        properties
    }
}

fn semantics(id: &str) -> PromptSemantics {
    let (role, trust, priority) = if id == "agent.policy" {
        ("kernel", "native", 1_000)
    } else if id == "collaboration.mode" {
        ("collaboration_mode", "runtime", 975)
    } else if id.starts_with("agent.") {
        ("agent_role", "configured", 950)
    } else if id.starts_with("instructions.global") {
        ("global_instructions", "user", 800)
    } else if id.starts_with("instructions.project")
        || id.starts_with("instructions.configured")
        || id.starts_with("instructions.nearby")
    {
        ("project_instructions", "user", 850)
    } else if id.starts_with("goal.")
        || id.starts_with("plan.")
        || id.starts_with("todo.")
        || id.starts_with("work_state.")
    {
        ("work_state", "runtime", 900)
    } else if id == "skills.policy" || id == "extensions" || id.starts_with("routing.") {
        ("routing", "native", 825)
    } else if id.starts_with("skills.selected") {
        ("selected_skill", "user", 800)
    } else if id == "skills.index" {
        ("skill_index", "discovered", 300)
    } else if id.starts_with("memory.") {
        ("memory", "stored", 650)
    } else {
        ("runtime_policy", "runtime", 700)
    };
    PromptSemantics {
        role,
        trust,
        priority,
    }
}

/// Track which actual prompts have already been persisted in one turn.
#[derive(Debug, Default)]
pub struct PromptTraceSet {
    digests: BTreeSet<String>,
}

impl PromptTraceSet {
    /// Whether this exact post-hook prompt needs a new durable snapshot.
    pub fn insert(&mut self, actual_system_prompt: &str) -> bool {
        self.digests.insert(sha256(actual_system_prompt))
    }
}

/// SHA-256 as lowercase hexadecimal.
#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_sections_render_exactly_and_reject_duplicate_ids() {
        let mut prompt = PromptAssembly::new();
        prompt
            .push("agent.base", "native:build", "BASE")
            .expect("base");
        prompt
            .push("instructions.0", "/repo/AGENTS.md", "RULES")
            .expect("instructions");

        assert_eq!(prompt.render(), "BASE\n\nRULES");
        assert_eq!(prompt.sections()[0].id(), "agent.base");
        assert_eq!(prompt.sections()[1].source(), "/repo/AGENTS.md");
        assert_eq!(
            prompt.push("agent.base", "duplicate", "NO"),
            Err(PromptAssemblyError::DuplicateId {
                id: "agent.base".to_owned()
            })
        );
    }

    #[test]
    fn envelope_preserves_sources_and_splits_native_from_developer_context() {
        let mut prompt = PromptAssembly::new();
        prompt
            .push("agent.base", "native:build", "BUILD ROLE")
            .expect("agent role");
        prompt
            .push("agent.policy", "zuno-agent::builtin:build", "KERNEL")
            .expect("kernel");
        prompt
            .push(
                "collaboration.mode",
                "zuno-runtime:collaboration-mode",
                "PLAN MODE",
            )
            .expect("collaboration mode");
        prompt
            .push("instructions.global.0", "/config/zuno/AGENTS.md", "GLOBAL")
            .expect("global instructions");
        prompt
            .push("instructions.project.1", "/repo/AGENTS.md", "PROJECT")
            .expect("project instructions");
        prompt
            .push_selected_skill("codegraph", "/skills/codegraph/SKILL.md", "FULL SKILL")
            .expect("selected skill");
        prompt
            .push("skills.index", "discovered skill index", "SKILLS")
            .expect("skill index");

        let envelope = prompt.envelope();
        assert_eq!(envelope.agent_role[0].content(), "BUILD ROLE");
        assert_eq!(envelope.kernel[0].content(), "KERNEL");
        assert_eq!(envelope.collaboration_mode[0].content(), "PLAN MODE");
        assert_eq!(envelope.global_instructions[0].content(), "GLOBAL");
        assert_eq!(envelope.project_instructions[0].content(), "PROJECT");
        assert_eq!(envelope.selected_skills[0].content(), "FULL SKILL");
        assert_eq!(
            envelope.selected_skills[0].selected_skill_name(),
            Some("codegraph")
        );
        assert_eq!(envelope.skill_index[0].content(), "SKILLS");
        assert_eq!(
            envelope.system_messages(),
            vec![
                "KERNEL\n\nBUILD ROLE",
                "PLAN MODE",
                "GLOBAL",
                "PROJECT",
                "FULL SKILL",
                "SKILLS"
            ]
        );
        assert_eq!(
            prompt.provider_projection(),
            "KERNEL\n\nBUILD ROLE\n\nPLAN MODE\n\nGLOBAL\n\nPROJECT\n\nFULL SKILL\n\nSKILLS"
        );
    }

    #[test]
    fn routing_prefix_enters_the_typed_routing_section() {
        let mut prompt = PromptAssembly::new();
        prompt
            .push(
                "routing.council",
                "zuno-tui:/council",
                "Invoke council_run exactly once.",
            )
            .expect("routing block");

        let envelope = prompt.envelope();
        assert_eq!(envelope.routing.len(), 1);
        assert_eq!(
            envelope.routing[0].content(),
            "Invoke council_run exactly once."
        );
        assert!(envelope.runtime_policy.is_empty());
    }

    #[test]
    fn event_properties_reconstruct_untransformed_and_transformed_prompts() {
        let mut prompt = PromptAssembly::new();
        prompt
            .push("agent.base", "native:build", "BASE")
            .expect("base");
        let unchanged = prompt.event_properties("build", 1, "BASE");
        assert_eq!(unchanged["hookTransformed"], false);
        assert!(unchanged.get("actualSystemPrompt").is_none());
        assert_eq!(unchanged["sections"][0]["content"], "BASE");
        assert!(unchanged["sections"][0].get("skillName").is_none());

        prompt
            .push_selected_skill("release", "/skills/release/SKILL.md", "SHIP")
            .expect("selected skill");
        let selected = prompt.event_properties("build", 2, "BASE\n\nSHIP");
        assert_eq!(selected["sections"][1]["role"], "selected_skill");
        assert_eq!(selected["sections"][1]["skillName"], "release");
        assert_eq!(
            selected["sections"][1]["source"],
            "/skills/release/SKILL.md"
        );

        let transformed = prompt.event_properties("build", 3, "BASE\nHOOK");
        assert_eq!(transformed["hookTransformed"], true);
        assert_eq!(transformed["actualSystemPrompt"], "BASE\nHOOK");
        assert_ne!(transformed["assemblySha256"], transformed["actualSha256"]);
    }

    #[test]
    fn trace_set_deduplicates_only_identical_actual_prompts() {
        let mut seen = PromptTraceSet::default();
        assert!(seen.insert("one"));
        assert!(!seen.insert("one"));
        assert!(seen.insert("two"));
    }
}
