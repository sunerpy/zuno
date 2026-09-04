use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use zuno_error::ToolError;
use zuno_tool::{
    ToolConcurrencyPolicy, ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy, TypedTool,
};

use crate::ContinuityError;

/// Stable model-facing notes tool id.
pub const NOTES_TOOL_ID: &str = "notes";

/// Trusted scope injected from [`ToolContext`].
#[derive(Debug, Clone, Copy)]
pub struct NoteScope<'a> {
    pub session_id: &'a str,
    pub agent: &'a str,
    pub call_id: &'a str,
}

/// Provider interface kept separate from the mixed-effect model tool.
pub trait NotesProvider: Send + Sync {
    fn list_files_by_prefix(
        &self,
        scope: NoteScope<'_>,
        prefix: Option<&str>,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError>;

    fn read_file(&self, scope: NoteScope<'_>, name: &str) -> Result<Value, ContinuityError>;

    fn search_contents(
        &self,
        scope: NoteScope<'_>,
        query: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Value, ContinuityError>;

    fn append_to_file(
        &self,
        scope: NoteScope<'_>,
        name: &str,
        content: &str,
        expected_revision: u64,
    ) -> Result<Value, ContinuityError>;

    fn write_file(
        &self,
        scope: NoteScope<'_>,
        name: &str,
        content: &str,
        expected_revision: u64,
    ) -> Result<Value, ContinuityError>;
}

/// Actions accepted by the native notes tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum NotesParams {
    /// List logical document names in lexical order.
    ListFilesByPrefix {
        /// Optional logical-name prefix.
        #[serde(default)]
        prefix: Option<String>,
        /// Opaque cursor returned by the previous page.
        #[serde(default)]
        cursor: Option<String>,
        /// Page size, 1 through 50.
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Read one logical document and its current revision.
    ReadFile {
        /// Logical document name, never a host path.
        name: String,
    },
    /// Search current Agent notes case-insensitively.
    SearchContents {
        /// Non-empty query, at most 1000 UTF-8 bytes.
        query: String,
        /// Opaque cursor returned by the previous page.
        #[serde(default)]
        cursor: Option<String>,
        /// Page size, 1 through 20.
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Append bytes to a document using optimistic concurrency.
    AppendToFile {
        /// Logical document name, never a host path.
        name: String,
        /// Text appended exactly as supplied.
        content: String,
        /// Current revision; use 0 only to create a new document.
        expected_revision: u64,
    },
    /// Replace a document using optimistic concurrency.
    WriteFile {
        /// Logical document name, never a host path.
        name: String,
        /// Complete replacement text.
        content: String,
        /// Current revision; use 0 only to create a new document.
        expected_revision: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotesActionClass {
    Read,
    Write,
    Invalid,
}

fn action_class(args: &Value) -> NotesActionClass {
    match args.get("action").and_then(Value::as_str) {
        Some("list_files_by_prefix" | "read_file" | "search_contents") => NotesActionClass::Read,
        Some("append_to_file" | "write_file") => NotesActionClass::Write,
        Some(_) | None => NotesActionClass::Invalid,
    }
}

/// Model consumer for one session-and-Agent scoped [`NotesProvider`].
pub struct NotesTool {
    provider: Arc<dyn NotesProvider>,
}

impl NotesTool {
    #[must_use]
    pub fn new(provider: Arc<dyn NotesProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl TypedTool for NotesTool {
    type Params = NotesParams;

    fn id(&self) -> &str {
        NOTES_TOOL_ID
    }

    fn description(&self) -> &str {
        include_str!("description/notes.txt").trim()
    }

    fn replay_policy_for(&self, args: &Value) -> ToolReplayPolicy {
        match action_class(args) {
            NotesActionClass::Read => ToolReplayPolicy::Safe,
            NotesActionClass::Write | NotesActionClass::Invalid => ToolReplayPolicy::Never,
        }
    }

    fn concurrency_policy_for(&self, args: &Value) -> ToolConcurrencyPolicy {
        match action_class(args) {
            NotesActionClass::Read => ToolConcurrencyPolicy::ParallelSafe,
            NotesActionClass::Write | NotesActionClass::Invalid => ToolConcurrencyPolicy::Exclusive,
        }
    }

    fn effect(&self, args: &Value) -> ToolEffect {
        match action_class(args) {
            NotesActionClass::Read => ToolEffect::ReadOnly,
            NotesActionClass::Write | NotesActionClass::Invalid => ToolEffect::SideEffecting,
        }
    }

    async fn run(
        &self,
        params: NotesParams,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let scope = NoteScope {
            session_id: &context.session_id,
            agent: &context.agent,
            call_id: &context.call_id,
        };
        let value = match params {
            NotesParams::ListFilesByPrefix {
                prefix,
                cursor,
                limit,
            } => self.provider.list_files_by_prefix(
                scope,
                prefix.as_deref(),
                cursor.as_deref(),
                limit,
            ),
            NotesParams::ReadFile { name } => self.provider.read_file(scope, &name),
            NotesParams::SearchContents {
                query,
                cursor,
                limit,
            } => self
                .provider
                .search_contents(scope, &query, cursor.as_deref(), limit),
            NotesParams::AppendToFile {
                name,
                content,
                expected_revision,
            } => self
                .provider
                .append_to_file(scope, &name, &content, expected_revision),
            NotesParams::WriteFile {
                name,
                content,
                expected_revision,
            } => self
                .provider
                .write_file(scope, &name, &content, expected_revision),
        }
        .map_err(|error| error.into_tool_error(NOTES_TOOL_ID))?;
        let output = serde_json::to_string_pretty(&value).map_err(|source| ToolError::Failed {
            tool: NOTES_TOOL_ID.to_owned(),
            source: Box::new(source),
        })?;
        Ok(ToolOutput::text(NOTES_TOOL_ID, output).with_metadata("continuityKind", "notes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_wire_schema_exposes_every_action() {
        // A root `#[serde(tag = "action")]` enum used to reach the provider as an empty
        // object schema, so the model had to infer the operation names from prose.
        let schema = zuno_tool::schema::params_schema::<NotesParams>();

        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["properties"]["action"]["enum"],
            serde_json::json!([
                "list_files_by_prefix",
                "read_file",
                "search_contents",
                "append_to_file",
                "write_file",
            ])
        );
        assert_eq!(schema["required"], serde_json::json!(["action"]));
        for field in [
            "prefix",
            "cursor",
            "limit",
            "name",
            "query",
            "content",
            "expected_revision",
            zuno_tool::schema::INTENT_KEY,
        ] {
            assert!(
                schema["properties"][field].is_object(),
                "{field} must reach the provider"
            );
        }
        let description = schema["properties"]["action"]["description"]
            .as_str()
            .expect("action explains each operation");
        assert!(
            description.contains("write_file")
                && description.contains("name, content, expected_revision"),
            "{description}"
        );
    }
}
