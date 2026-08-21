//! The `grep` tool: content search.
//!
//! Output shape, title and metadata are `packages/opencode/src/tool/grep.ts:30-111`
//! line for line, including two things that look like defects and are not ours to
//! fix: a `path` argument naming a *file* searches that file's whole directory
//! (`grep.ts:62` takes `dirname` and hands it to `rg` as the cwd), and each rendered
//! match keeps the line's terminator, so the output has a blank line after every
//! match.

use crate::search_common::{
    InterruptCancellation, RESULT_LIMIT, SearchTooling, TargetKind, assert_external_directory,
    map_search_error,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use zuno_error::ToolError;
use zuno_search::GrepRequest;
use zuno_tool::{PermissionAsk, ToolContext, ToolOutput, ToolReplayPolicy, TypedTool};

/// The description the model reads, verbatim from `tool/grep.txt`.
pub const DESCRIPTION: &str = include_str!("description/grep.txt");

/// What the tool emits when nothing matched.
///
/// The oracle's wording (`grep.ts:33`) says "No files found" even though the search
/// was over contents. Reproduced because the model has been trained on it.
const EMPTY_OUTPUT: &str = "No files found";

/// `grep`'s arguments.
///
/// The field names and descriptions are the oracle's (`grep.ts:10-18`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepParams {
    /// The regex pattern to search for in file contents
    pub pattern: String,
    /// The directory to search in. Defaults to the current working directory.
    #[serde(default)]
    pub path: Option<String>,
    /// File pattern to include in the search (e.g. "*.js", "*.{ts,tsx}")
    #[serde(default)]
    pub include: Option<String>,
}

/// Content search over the project.
#[derive(Debug, Clone)]
pub struct GrepTool {
    tooling: SearchTooling,
}

impl GrepTool {
    /// A tool over `tooling`.
    #[must_use]
    pub fn new(tooling: SearchTooling) -> Self {
        Self { tooling }
    }
}

#[async_trait]
impl TypedTool for GrepTool {
    type Params = GrepParams;

    fn id(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    async fn run(&self, params: GrepParams, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        // `if (!params.pattern) throw` (`grep.ts:35-37`). A required `String` field
        // still admits `""`, so the check is explicit here too.
        if params.pattern.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.id().to_owned(),
                source: Box::new(PatternRequired),
            });
        }

        let mut ask = PermissionAsk::new("grep", params.pattern.clone());
        ask.always = vec!["*".to_owned()];
        ask.metadata = json!({
            "pattern": params.pattern,
            "path": params.path,
            "include": params.include,
        })
        .as_object()
        .cloned()
        .unwrap_or_default();
        ctx.ask(self.id(), ask).await?;

        let requested = self.tooling.scope.resolve(params.path.as_deref());
        let requested_is_directory = requested.is_dir();
        assert_external_directory(
            &ctx,
            self.id(),
            &self.tooling.scope,
            &requested,
            if requested_is_directory {
                TargetKind::Directory
            } else {
                TargetKind::File
            },
        )
        .await?;

        let cwd = search_root(&requested, requested_is_directory);
        let request = GrepRequest::new(&cwd, &params.pattern, RESULT_LIMIT)
            .with_include(params.include.clone());
        let cancel = InterruptCancellation::from_context(&ctx);
        let results = self
            .tooling
            .backend
            .grep(&request, &cancel)
            .map_err(|error| map_search_error(self.id(), error))?;

        if results.items.is_empty() {
            return Ok(empty(&params.pattern));
        }

        // The base the oracle resolves each row against (`grep.ts:72-75`) is the
        // *requested* path, not the cwd the search ran in. For a file argument those
        // differ: the search ran in the parent, so a row's relative path is joined to
        // that same parent.
        let base = search_root(&requested, requested_is_directory);
        let rows: Vec<Row> = results
            .items
            .iter()
            .map(|found| Row {
                path: base.join(&found.entry.path),
                line: found.line,
                text: found.text.clone(),
            })
            .collect();

        let truncated = rows.len() == RESULT_LIMIT;
        let total = rows.len();
        let files: Vec<String> = rows
            .iter()
            .map(|row| row.path.to_string_lossy().into_owned())
            .fold(Vec::new(), |mut files, path| {
                if files.last() != Some(&path) {
                    files.push(path);
                }
                files
            });

        let mut lines = vec![format!(
            "Found {total} matches{}",
            if truncated {
                " (more matches available)"
            } else {
                ""
            }
        )];

        let mut current: Option<&Path> = None;
        for row in &rows {
            if current != Some(row.path.as_path()) {
                if current.is_some() {
                    lines.push(String::new());
                }
                current = Some(row.path.as_path());
                lines.push(format!("{}:", row.path.display()));
            }
            lines.push(format!("  Line {}: {}", row.line, row.text));
        }

        if truncated {
            lines.push(String::new());
            lines.push(
                "(Results truncated. Consider using a more specific path or pattern.)".to_owned(),
            );
        }

        Ok(ToolOutput::text(&params.pattern, lines.join("\n"))
            .with_metadata("matches", total)
            .with_metadata("truncated", truncated)
            .with_metadata("files", json!(files)))
    }
}

struct Row {
    path: PathBuf,
    line: u64,
    text: String,
}

/// The directory the search runs in.
///
/// `grep.ts:62`: a directory is searched itself, and anything else — a file, or a
/// path that does not exist — is searched by way of its parent. That is why a `grep`
/// aimed at one file returns matches from its siblings.
fn search_root(requested: &Path, is_directory: bool) -> PathBuf {
    if is_directory {
        requested.to_path_buf()
    } else {
        requested.parent().unwrap_or(requested).to_path_buf()
    }
}

fn empty(pattern: &str) -> ToolOutput {
    ToolOutput::text(pattern, EMPTY_OUTPUT)
        .with_metadata("matches", 0)
        .with_metadata("truncated", false)
        .with_metadata("files", json!([]))
}

/// The failure for an empty `pattern`.
#[derive(Debug, thiserror::Error)]
#[error("pattern is required")]
struct PatternRequired;

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

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir(dir.path().join("src")).expect("a subdirectory");
        std::fs::write(dir.path().join("src/a.ts"), "alpha needle here\nbeta\n")
            .expect("a fixture file");
        std::fs::write(dir.path().join("src/b.ts"), "gamma\nneedle in b\n")
            .expect("a fixture file");
        dir
    }

    #[test]
    fn the_schema_is_derived_and_names_the_oracles_parameters() {
        let tool = erase(GrepTool::new(SearchTooling::new("/tmp")));
        let definition = tool.definition();

        assert_eq!(definition.id, "grep");
        assert_eq!(tool.replay_policy(), ToolReplayPolicy::Safe);
        assert_eq!(
            definition.parameters["properties"]["pattern"]["description"],
            "The regex pattern to search for in file contents"
        );
        assert_eq!(
            definition.parameters["properties"]["include"]["description"],
            "File pattern to include in the search (e.g. \"*.js\", \"*.{ts,tsx}\")"
        );
        assert_eq!(
            definition.parameters["properties"]["path"]["description"],
            "The directory to search in. Defaults to the current working directory."
        );
    }

    #[tokio::test]
    async fn an_empty_pattern_is_rejected_with_the_oracles_message() {
        let error = erase(GrepTool::new(SearchTooling::new("/tmp")))
            .execute(json!({ "pattern": "" }), context())
            .await
            .expect_err("an empty pattern is rejected");

        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        assert_eq!(
            std::error::Error::source(&error)
                .expect("the cause chains")
                .to_string(),
            "pattern is required"
        );
    }

    #[tokio::test]
    async fn no_matches_renders_the_oracles_empty_output_and_is_not_an_error() {
        let dir = fixture();

        let output = erase(GrepTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "zzzznomatchzzzz" }), context())
            .await
            .expect("a pattern that matches nothing is not an error");

        assert_eq!(output.output, "No files found");
        assert_eq!(output.title, "zzzznomatchzzzz");
        assert_eq!(output.metadata["matches"], 0);
        assert_eq!(output.metadata["truncated"], false);
        assert_eq!(output.metadata["files"], json!([]));
    }

    #[tokio::test]
    async fn matches_are_grouped_by_path_and_keep_the_line_terminator() {
        let dir = fixture();

        let output = erase(GrepTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "needle" }), context())
            .await
            .expect("the grep succeeds");

        let expected = format!(
            "Found 2 matches\n{a}:\n  Line 1: alpha needle here\n\n\n{b}:\n  Line 2: needle in b\n",
            a = dir.path().join("src/a.ts").display(),
            b = dir.path().join("src/b.ts").display(),
        );
        assert_eq!(
            output.output, expected,
            "the terminator inside each match text is the oracle's, and produces the blank lines"
        );
        assert_eq!(output.metadata["matches"], 2);
        assert_eq!(
            output.metadata["files"],
            json!([dir.path().join("src/a.ts"), dir.path().join("src/b.ts")])
        );
    }

    #[tokio::test]
    async fn an_include_pattern_narrows_the_search() {
        let dir = fixture();
        std::fs::write(dir.path().join("src/c.js"), "needle in js\n").expect("a fixture file");

        let output = erase(GrepTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "needle", "include": "*.js" }), context())
            .await
            .expect("the grep succeeds");

        assert_eq!(output.metadata["matches"], 1);
        assert!(output.output.contains("src/c.js"));
        assert!(!output.output.contains("src/a.ts"));
    }

    #[tokio::test]
    async fn a_file_path_argument_searches_its_whole_directory_as_the_oracle_does() {
        let dir = fixture();

        let output = erase(GrepTool::new(SearchTooling::new(dir.path())))
            .execute(
                json!({ "pattern": "needle", "path": "src/a.ts" }),
                context(),
            )
            .await
            .expect("the grep succeeds");

        assert_eq!(
            output.metadata["matches"], 2,
            "grep.ts:62 hands rg the file's parent directory, so the sibling matches too"
        );
    }

    #[tokio::test]
    async fn the_permission_gate_is_consulted_before_the_filesystem_is_touched() {
        let ctx = ToolContext::new(
            "ses_1",
            "msg_1",
            "call_1",
            "build",
            Arc::new(zuno_tool::DenyAll),
            Arc::new(NeverInterrupted),
        );

        let error = erase(GrepTool::new(SearchTooling::new("/tmp")))
            .execute(json!({ "pattern": "needle" }), ctx)
            .await
            .expect_err("a denied grep does not run");

        assert!(matches!(error, ToolError::Denied { .. }));
    }

    #[tokio::test]
    async fn an_invalid_regex_is_model_correctable() {
        let dir = fixture();

        let error = erase(GrepTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "(unclosed" }), context())
            .await
            .expect_err("an unclosed group cannot compile");

        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        assert!(error.is_model_correctable());
    }

    #[tokio::test]
    async fn exactly_one_hundred_matches_are_reported_as_truncated_as_the_oracle_does() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        for index in 0..RESULT_LIMIT {
            std::fs::write(dir.path().join(format!("f{index:04}.txt")), "needle\n")
                .expect("a fixture file");
        }

        let output = erase(GrepTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "needle" }), context())
            .await
            .expect("the grep succeeds");

        assert_eq!(output.metadata["matches"], RESULT_LIMIT);
        assert_eq!(output.metadata["truncated"], true);
        assert!(
            output
                .output
                .starts_with("Found 100 matches (more matches available)")
        );
        assert!(
            output
                .output
                .ends_with("(Results truncated. Consider using a more specific path or pattern.)")
        );
    }
}
