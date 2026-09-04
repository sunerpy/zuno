//! The `glob` tool: path matching.
//!
//! Output shape, title and metadata are `packages/opencode/src/tool/glob.ts:49-72`
//! line for line, because a change to any of them changes what the model reads.

use crate::search_common::{
    InterruptCancellation, RESULT_LIMIT, SearchTooling, TargetKind, assert_external_directory,
    display_relative, map_search_error, one_line,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use zuno_error::ToolError;
use zuno_search::GlobRequest;
use zuno_tool::{
    PermissionAsk, ToolConcurrencyPolicy, ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy,
    TypedTool,
};

/// The description the model reads, verbatim from `tool/glob.txt`.
pub const DESCRIPTION: &str = include_str!("description/glob.txt");

/// What the tool emits when nothing matched (`glob.ts:56`).
const EMPTY_OUTPUT: &str = "No files found";

/// What the tool emits when the engine matched files but could report none of them.
///
/// The engine drops a path it cannot spell as text and says so through `truncated`;
/// answering [`EMPTY_OUTPUT`] for that case told the model, with `truncated: false`,
/// that a tree it was enumerating held nothing.
const UNREPORTABLE_OUTPUT: &str = "Found files that cannot be reported: every matching \
                                   file has a name that cannot be spelled as text, so no \
                                   path can be shown. (Results truncated.)";

/// The trailer when a path was dropped for its name while the list is under the limit.
const UNNAMEABLE_TRAILER: &str = "(Results truncated: at least one matching file has a \
                                  name that cannot be spelled as text and cannot be \
                                  shown.)";

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

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Safe
    }

    fn concurrency_policy(&self) -> ToolConcurrencyPolicy {
        ToolConcurrencyPolicy::ParallelSafe
    }

    fn effect(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
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
        // Same reason as `grep`: `rg --files` over a large tree blocks its caller for
        // seconds, and the reactor a current-thread runtime needs to deliver the
        // interrupt is the very thread that would be blocked.
        let engine = self.tooling.ripgrep.clone();
        let results = tokio::task::spawn_blocking(move || engine.glob(&request, &cancel))
            .await
            .map_err(|error| ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(error),
            })?
            .map_err(|error| map_search_error(self.id(), error))?;

        // The engine's own flag first — it is set when a path was dropped for a name
        // the engine cannot spell, which no length test can see — then the oracle's
        // weaker test (`glob.ts:51`), which claims truncation for exactly 100 matches
        // and puts that claim in the output the model reads.
        let at_limit = results.items.len() == RESULT_LIMIT;
        let truncated = results.truncated || at_limit;

        let mut lines = Vec::new();
        if results.items.is_empty() {
            lines.push(
                if truncated {
                    UNREPORTABLE_OUTPUT
                } else {
                    EMPTY_OUTPUT
                }
                .to_owned(),
            );
        } else {
            for entry in &results.items {
                // One file, one line: see `one_line` for why a name with a line break
                // is spelled rather than printed raw.
                lines.push(one_line(&search.join(&entry.path)));
            }
            if truncated {
                lines.push(String::new());
                lines.push(if at_limit {
                    format!(
                        "(Results are truncated: showing first {RESULT_LIMIT} results. Consider using a more specific path or pattern.)"
                    )
                } else {
                    UNNAMEABLE_TRAILER.to_owned()
                });
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
        let tool = erase(GlobTool::new(SearchTooling::new("/tmp")));
        let definition = tool.definition();

        assert_eq!(definition.id, "glob");
        assert_eq!(tool.replay_policy(), ToolReplayPolicy::Safe);
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

        let tooling = SearchTooling::with_ripgrep(
            crate::search_common::SearchScope::with_worktree(
                dir.path().join("packages/app"),
                dir.path(),
            ),
            zuno_search::Ripgrep::new("rg"),
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

    /// A file whose name is not valid UTF-8; see the same helper in `grep` for why it
    /// is Unix-only.
    #[cfg(unix)]
    fn unnameable_file(dir: &std::path::Path, name: &[u8]) {
        use std::os::unix::ffi::OsStrExt as _;

        let path = dir.join(std::ffi::OsStr::from_bytes(name));
        std::fs::write(&path, "x").expect("a fixture file with a non-UTF-8 name");
        assert!(
            std::fs::read_dir(dir)
                .expect("the fixture directory lists")
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().as_encoded_bytes() == name),
            "the non-UTF-8 name must really have landed on disk"
        );
    }

    /// The engine answers `Ok(items: [], truncated: true)` for a directory whose only
    /// match it cannot name. Before the fix the tool rendered "No files found" with
    /// `truncated: false`, computed from the list length alone.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_glob_whose_only_match_is_unnameable_says_so_instead_of_no_files_found() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        unnameable_file(dir.path(), b"bad\xff.ts");
        std::fs::write(dir.path().join("other.js"), "x").expect("a fixture file");

        let output = erase(GlobTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "**/*.ts" }), context())
            .await
            .expect("a dropped path is not an error");

        assert_ne!(output.output, EMPTY_OUTPUT, "a dropped path is not absence");
        assert_eq!(output.output, UNREPORTABLE_OUTPUT);
        assert_eq!(output.metadata["count"], 0);
        assert_eq!(output.metadata["truncated"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_nameable_and_an_unnameable_match_render_one_path_and_a_truncation_note() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        unnameable_file(dir.path(), b"bad\xff.ts");
        std::fs::write(dir.path().join("good.ts"), "x").expect("a fixture file");

        let output = erase(GlobTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "**/*.ts" }), context())
            .await
            .expect("the glob succeeds");

        assert_eq!(output.metadata["count"], 1);
        assert_eq!(output.metadata["truncated"], true);
        assert_eq!(
            output.output,
            format!(
                "{}\n\n{UNNAMEABLE_TRAILER}",
                dir.path().join("good.ts").display()
            )
        );
    }

    /// The engine returns a newline-bearing file name as one path; the tool renders
    /// one result per line, so that name must be spelled or the model reads two files.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_name_with_a_line_break_is_one_rendered_line() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(dir.path().join("good.ts"), "x").expect("a fixture file");
        std::fs::write(dir.path().join("two\nlines.ts"), "x").expect("a fixture file");

        let output = erase(GlobTool::new(SearchTooling::new(dir.path())))
            .execute(json!({ "pattern": "**/*.ts" }), context())
            .await
            .expect("the glob succeeds");

        assert_eq!(output.metadata["count"], 2);
        assert_eq!(
            output.output.lines().count(),
            2,
            "one file, one line: {}",
            output.output
        );
        assert_eq!(
            output.output,
            format!(
                "{}\n{}",
                dir.path().join("good.ts").display(),
                one_line(&dir.path().join("two\nlines.ts"))
            )
        );
        assert!(output.output.contains(r"two\nlines.ts"));
        assert_eq!(output.metadata["truncated"], false);
    }

    /// A stand-in for `rg` that never finishes on its own.
    ///
    /// `exec` so the process the engine kills is the one that sleeps, leaving nothing
    /// behind; exit 1 is ripgrep's "no matches", which is what makes the regressed
    /// behaviour a clean `Ok` rather than a second kind of failure.
    ///
    /// Unix-only because the fake is a shell script. What it pins is not
    /// platform-specific — the change under test is one `spawn_blocking` — so the
    /// Windows arm rides the same call site rather than a second fake.
    #[cfg(unix)]
    fn never_finishing_rg(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let program = dir.join("never-finishing-rg");
        std::fs::write(&program, "#!/bin/sh\nexec sleep 30\n").expect("the fake engine");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("the fake engine is executable");
        program
    }

    /// An interrupt a spawned task raises after the search is already in flight.
    ///
    /// This is the whole point: an interrupt that is already set when the call starts
    /// is caught by the engine's pre-check and proves nothing about the reactor.
    #[cfg(unix)]
    struct DelayedInterrupt(std::sync::Arc<std::sync::atomic::AtomicBool>);

    #[cfg(unix)]
    #[async_trait]
    impl zuno_tool::InterruptHandle for DelayedInterrupt {
        fn is_set(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn notified(&self) {}
    }

    /// `zuno run`, `zuno acp` and `zuno serve` all drive a `new_current_thread`
    /// runtime, so a search that runs on the reactor holds the only thread that could
    /// deliver the interrupt the user just pressed — and the engine's own 10 ms
    /// cancellation poll then never observes it. The fake engine never exits, so the
    /// only way this call can return at all is if the timer task ran while the search
    /// was in flight.
    ///
    /// Before the fix the timer never fires: the call blocks until the fake's own
    /// `sleep` ends and then reports "no matches", so `expect_err` fails.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_glob_leaves_the_reactor_free_to_deliver_the_interrupt() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().expect("a temporary directory");
        let fired = Arc::new(AtomicBool::new(false));
        let tooling = SearchTooling {
            scope: crate::search_common::SearchScope::new(dir.path()),
            ripgrep: zuno_search::Ripgrep::new(never_finishing_rg(dir.path())),
        };
        let ctx = ToolContext::new(
            "ses_1",
            "msg_1",
            "call_1",
            "build",
            Arc::new(AllowAll),
            Arc::new(DelayedInterrupt(Arc::clone(&fired))),
        );
        tokio::spawn({
            let fired = Arc::clone(&fired);
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                fired.store(true, Ordering::SeqCst);
            }
        });

        let started = Instant::now();
        let error = erase(GlobTool::new(tooling))
            .execute(json!({ "pattern": "**/*" }), ctx)
            .await
            .expect_err("an interrupt raised during the search must cancel it");
        let elapsed = started.elapsed();

        assert_eq!(
            std::error::Error::source(&error)
                .expect("the cause chains")
                .to_string(),
            "search was cancelled"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the interrupt was delivered while the search ran, not after it: {elapsed:?}"
        );
    }
}
