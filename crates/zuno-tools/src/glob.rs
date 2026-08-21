//! The `glob` tool: path matching.
//!
//! Output shape, title and metadata are `packages/opencode/src/tool/glob.ts:49-72`
//! line for line, because a change to any of them changes what the model reads.

use crate::search_common::{
    InterruptCancellation, RESULT_LIMIT, SearchTooling, TargetKind, assert_external_directory,
    display_relative, map_search_error,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use zuno_error::ToolError;
use zuno_search::GlobRequest;
use zuno_tool::{PermissionAsk, ToolContext, ToolOutput, TypedTool};

/// The description the model reads, verbatim from `tool/glob.txt`.
pub const DESCRIPTION: &str = include_str!("description/glob.txt");

/// `glob`'s arguments.
///
/// The field names and descriptions are the oracle's (`glob.ts:10-15`); the model
/// sees them, so they are a compatibility surface, not documentation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobParams {
    /// The glob pattern to match files against
    pub pattern: String,
    /// The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter "undefined" or "null" - simply omit it for the default behavior. Must be a valid directory path if provided.
    #[serde(default)]
    pub path: Option<String>,
}

/// Path matching over the project.
#[derive(Debug, Clone)]
pub struct GlobTool {
    tooling: SearchTooling,
}

impl GlobTool {
    /// A tool over `tooling`.
    #[must_use]
    pub fn new(tooling: SearchTooling) -> Self {
        Self { tooling }
    }
}

#[async_trait]
impl TypedTool for GlobTool {
    type Params = GlobParams;

    fn id(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(&self, params: GlobParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        // Asked before anything is resolved or stat'd, which is the oracle's order
        // (`glob.ts:28-36`): the gate sees the call as the model wrote it.
        let mut ask = PermissionAsk::new("glob", params.pattern.clone());
        ask.always = vec!["*".to_owned()];
        ask.metadata = json!({ "pattern": params.pattern, "path": params.path })
            .as_object()
            .cloned()
            .unwrap_or_default();
        ctx.ask(self.id(), ask).await?;

        let search = self.tooling.scope.resolve(params.path.as_deref());
        if search.is_file() {
            return Err(ToolError::InvalidArgs {
                tool: self.id().to_owned(),
                source: Box::new(NotADirectory {
                    path: search.to_string_lossy().into_owned(),
                }),
            });
        }
        assert_external_directory(
            &ctx,
            self.id(),
            &self.tooling.scope,
            &search,
            TargetKind::Directory,
        )
        .await?;

        let request = GlobRequest::new(&search, &params.pattern, RESULT_LIMIT);
        let cancel = InterruptCancellation::from_context(&ctx);
        let results = self
            .tooling
            .backend
            .glob(&request, &cancel)
            .map_err(|error| map_search_error(self.id(), error))?;

        // `files.length === limit`, not the engine's own `truncated`. The oracle's
        // weaker test (`glob.ts:51`) claims truncation for a tree with exactly 100
        // matches, and that claim is in the output the model reads.
        let truncated = results.items.len() == RESULT_LIMIT;

        let mut lines = Vec::new();
        if results.items.is_empty() {
            lines.push("No files found".to_owned());
        } else {
            for entry in &results.items {
                lines.push(search.join(&entry.path).to_string_lossy().into_owned());
            }
            if truncated {
                lines.push(String::new());
                lines.push(format!(
                    "(Results are truncated: showing first {RESULT_LIMIT} results. Consider using a more specific path or pattern.)"
                ));
            }
        }

        Ok(ToolOutput::text(
            display_relative(&self.tooling.scope.worktree, &search),
            lines.join("\n"),
        )
        .with_metadata("count", results.items.len())
        .with_metadata("truncated", truncated))
    }
}

/// The failure for a `path` argument that names a file.
///
/// A named type rather than a formatted string so the message the model sees is
/// produced in one place, and so a caller can match on it.
#[derive(Debug, thiserror::Error)]
#[error("glob path must be a directory: {path}")]
struct NotADirectory {
    path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use zuno_tool::{AllowAll, NeverInterrupted, erase};

    fn context() -> ToolContext {
        ToolContext::new(
            "ses_1",
            "msg_1",
            "call_1",
            "build",
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        )
    }

    #[test]
    fn the_schema_is_derived_and_names_the_oracles_parameters() {
        let definition = erase(GlobTool::new(SearchTooling::new("/tmp"))).definition();

        assert_eq!(definition.id, "glob");
        assert_eq!(
            definition.parameters["properties"]["pattern"]["type"],
            "string"
        );
        assert_eq!(
            definition.parameters["properties"]["pattern"]["description"],
            "The glob pattern to match files against"
        );
        assert!(
            definition.parameters["properties"]["path"]["description"]
                .as_str()
                .expect("path carries a description")
                .contains("DO NOT enter \"undefined\" or \"null\""),
            "the description is a compatibility surface, not prose we may reword"
        );
        assert_eq!(
            definition.parameters["required"]
                .as_array()
                .expect("required is an array")[0],
            "pattern"
        );
    }

    #[tokio::test]
    async fn a_file_path_argument_is_rejected_with_the_oracles_message() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(dir.path().join("a.ts"), "x").expect("a fixture file");

        let tool = GlobTool::new(SearchTooling::new(dir.path()));
        let error = erase(tool)
            .execute(json!({ "pattern": "**/*", "path": "a.ts" }), context())
            .await
            .expect_err("a file is not a directory");

        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        assert!(
            std::error::Error::source(&error)
                .expect("the cause chains")
                .to_string()
                .starts_with("glob path must be a directory:")
        );
    }

    #[tokio::test]
    async fn no_matches_renders_the_oracles_empty_output() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(dir.path().join("a.ts"), "x").expect("a fixture file");

        let output = erase(GlobTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "**/*.nothing" }), context())
            .await
            .expect("an empty result is not an error");

        assert_eq!(output.output, "No files found");
        assert_eq!(output.metadata["count"], 0);
        assert_eq!(output.metadata["truncated"], false);
    }

    #[tokio::test]
    async fn matches_are_rendered_as_absolute_paths_one_per_line() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir(dir.path().join("src")).expect("a subdirectory");
        std::fs::write(dir.path().join("src/a.ts"), "x").expect("a fixture file");
        std::fs::write(dir.path().join("src/b.ts"), "x").expect("a fixture file");

        let output = erase(GlobTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "**/*.ts" }), context())
            .await
            .expect("the glob succeeds");

        assert_eq!(
            output.output,
            format!(
                "{}\n{}",
                dir.path().join("src/a.ts").display(),
                dir.path().join("src/b.ts").display()
            )
        );
        assert_eq!(output.metadata["count"], 2);
    }

    #[tokio::test]
    async fn exactly_one_hundred_results_are_reported_as_truncated_as_the_oracle_does() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        for index in 0..RESULT_LIMIT {
            std::fs::write(dir.path().join(format!("f{index:04}.ts")), "x")
                .expect("a fixture file");
        }

        let output = erase(GlobTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "**/*.ts" }), context())
            .await
            .expect("the glob succeeds");

        assert_eq!(output.metadata["count"], RESULT_LIMIT);
        assert_eq!(output.metadata["truncated"], true);
        assert!(output.output.ends_with(
            "(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)"
        ));
    }

    #[tokio::test]
    async fn the_title_is_the_search_root_relative_to_the_worktree() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir_all(dir.path().join("packages/app")).expect("a subdirectory");
        std::fs::write(dir.path().join("packages/app/a.ts"), "x").expect("a fixture file");

        let tooling = SearchTooling::with_backend(
            crate::search_common::SearchScope::with_worktree(
                dir.path().join("packages/app"),
                dir.path(),
            ),
            zuno_search::Backend::embedded(),
        );

        let output = erase(GlobTool::new(tooling))
            .execute(json!({ "pattern": "**/*.ts" }), context())
            .await
            .expect("the glob succeeds");

        assert_eq!(output.title, "packages/app");
    }

    #[tokio::test]
    async fn the_permission_gate_is_consulted_before_the_filesystem_is_touched() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let ctx = ToolContext::new(
            "ses_1",
            "msg_1",
            "call_1",
            "build",
            Arc::new(zuno_tool::DenyAll),
            Arc::new(NeverInterrupted),
        );

        let error = erase(GlobTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "**/*" }), ctx)
            .await
            .expect_err("a denied glob does not run");

        assert!(matches!(error, ToolError::Denied { .. }));
    }

    #[tokio::test]
    async fn a_fired_interrupt_fails_rather_than_returning_a_partial_list() {
        struct Fired;

        #[async_trait]
        impl zuno_tool::InterruptHandle for Fired {
            fn is_set(&self) -> bool {
                true
            }
            async fn notified(&self) {}
        }

        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(dir.path().join("a.ts"), "x").expect("a fixture file");
        let ctx = ToolContext::new(
            "ses_1",
            "msg_1",
            "call_1",
            "build",
            Arc::new(AllowAll),
            Arc::new(Fired),
        );

        let error = erase(GlobTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "**/*" }), ctx)
            .await
            .expect_err("a cancelled glob fails");

        assert!(matches!(error, ToolError::Failed { .. }));
    }
}
