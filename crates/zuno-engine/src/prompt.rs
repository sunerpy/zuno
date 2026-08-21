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

    /// Serialized event representation.
    fn value(&self, order: usize) -> Value {
        json!({
            "id": self.id,
            "order": order,
            "source": self.source,
            "bytes": self.content.len(),
            "sha256": self.sha256,
            "content": self.content,
        })
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
        });
        Ok(())
    }

    /// Ordered sections.
    #[must_use]
    pub fn sections(&self) -> &[PromptSection] {
        &self.sections
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
        let assembled = self.render();
        let mut properties = Map::from_iter([
            ("agent".to_owned(), Value::String(agent.to_owned())),
            ("step".to_owned(), Value::from(step)),
            ("schemaVersion".to_owned(), Value::from(1)),
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
    fn event_properties_reconstruct_untransformed_and_transformed_prompts() {
        let mut prompt = PromptAssembly::new();
        prompt
            .push("agent.base", "native:build", "BASE")
            .expect("base");
        let unchanged = prompt.event_properties("build", 1, "BASE");
        assert_eq!(unchanged["hookTransformed"], false);
        assert!(unchanged.get("actualSystemPrompt").is_none());
        assert_eq!(unchanged["sections"][0]["content"], "BASE");

        let transformed = prompt.event_properties("build", 2, "BASE\nHOOK");
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
