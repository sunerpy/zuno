use async_trait::async_trait;
use serde_json::json;
use std::error::Error as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use zuno_error::{ToolError, ToolMutationConflict, ToolMutationConflictKind};
use zuno_tool::{InterruptHandle, NeverInterrupted, PermissionAsk, PermissionAsker, ToolContext};
use zuno_tools::{FileFormatter, FileTools, NoopFormatter};

#[derive(Default)]
struct RecordingPermission {
    asks: Mutex<Vec<PermissionAsk>>,
}

impl RecordingPermission {
    fn permissions(&self) -> Vec<String> {
        self.asks
            .lock()
            .expect("permission recording lock")
            .iter()
            .map(|ask| ask.permission.clone())
            .collect()
    }

    fn asks(&self) -> Vec<PermissionAsk> {
        self.asks.lock().expect("permission recording lock").clone()
    }
}

#[async_trait]
impl PermissionAsker for RecordingPermission {
    async fn ask(
        &self,
        _origin: zuno_tool::PermissionOrigin<'_>,
        _tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        self.asks
            .lock()
            .expect("permission recording lock")
            .push(ask);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingFormatter {
    paths: Mutex<Vec<PathBuf>>,
}

impl RecordingFormatter {
    fn paths(&self) -> Vec<PathBuf> {
        self.paths.lock().expect("formatter recording lock").clone()
    }
}

#[async_trait]
impl FileFormatter for RecordingFormatter {
    async fn format(&self, path: &Path) -> io::Result<bool> {
        self.paths
            .lock()
            .expect("formatter recording lock")
            .push(path.to_owned());
        Ok(false)
    }
}

struct FailingFormatter {
    fail_on_call: usize,
    calls: Mutex<usize>,
}

impl FailingFormatter {
    fn always() -> Self {
        Self {
            fail_on_call: 1,
            calls: Mutex::new(0),
        }
    }

    fn on_call(fail_on_call: usize) -> Self {
        Self {
            fail_on_call,
            calls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl FileFormatter for FailingFormatter {
    async fn format(&self, _path: &Path) -> io::Result<bool> {
        let mut calls = self.calls.lock().expect("formatter call lock");
        *calls += 1;
        if *calls == self.fail_on_call {
            Err(io::Error::other("formatter transport was lost"))
        } else {
            Ok(false)
        }
    }
}

/// A formatter that edits *another* file while formatting the one it was given.
///
/// It stands in for every writer that shares a worktree with the agent — a
/// background job, a second tool call, the user's editor — because a formatter is
/// the only such writer a test can schedule exactly inside the window between the
/// moment a patch verifies a file and the moment it writes it.
struct ConcurrentEditor {
    path: PathBuf,
    contents: &'static str,
}

#[async_trait]
impl FileFormatter for ConcurrentEditor {
    async fn format(&self, _path: &Path) -> io::Result<bool> {
        std::fs::write(&self.path, self.contents)?;
        Ok(false)
    }
}

#[derive(Debug)]
struct Interrupted;

#[async_trait]
impl InterruptHandle for Interrupted {
    fn is_set(&self) -> bool {
        true
    }

    async fn notified(&self) {}
}

fn context(
    permission: Arc<dyn PermissionAsker>,
    interrupt: Arc<dyn InterruptHandle>,
) -> ToolContext {
    ToolContext::new(
        "session-file",
        "message-file",
        "call-file",
        "build",
        permission,
        interrupt,
    )
}

fn normal_context(permission: Arc<dyn PermissionAsker>) -> ToolContext {
    context(permission, Arc::new(NeverInterrupted))
}

/// The typed conflict an uncertain outcome carries as its cause.
///
/// A refused write keeps its full [`ToolError`] in `#[source]` position rather than
/// being flattened to prose, so a caller that has to decide between re-reading and
/// revising can still read the kind off the chain.
fn mutation_conflict(source: &(dyn std::error::Error + 'static)) -> ToolMutationConflict {
    let Some(ToolError::MutationConflict { conflict, .. }) = source.downcast_ref::<ToolError>()
    else {
        panic!("expected a typed mutation conflict in the cause chain, got {source:?}");
    };
    (**conflict).clone()
}

fn source_message(error: &ToolError) -> String {
    error
        .source()
        .map(ToString::to_string)
        .unwrap_or_else(|| error.to_string())
}

fn setup() -> (TempDir, FileTools, Arc<RecordingPermission>) {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let permission = Arc::new(RecordingPermission::default());
    let tools = FileTools::new(workspace.path()).expect("file tools");
    (workspace, tools, permission)
}

async fn read_for_mutation(tools: &FileTools, path: &Path) {
    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(Arc::new(RecordingPermission::default())),
        )
        .await
        .unwrap_or_else(|error| panic!("read {} before mutation: {error}", path.display()));
}

#[test]
fn file_tool_schemas_are_derived_with_the_oracle_parameter_names() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tools = FileTools::new(workspace.path()).expect("file tools");

    let read = tools.read.definition().parameters;
    assert!(read["properties"].get("filePath").is_some());
    assert!(read["properties"].get("offset").is_some());
    assert!(read["properties"].get("limit").is_some());

    let write = tools.write.definition().parameters;
    assert!(write["properties"].get("content").is_some());
    assert!(write["properties"].get("filePath").is_some());

    let edit = tools.edit.definition().parameters;
    assert!(edit["properties"].get("filePath").is_some());
    assert!(edit["properties"].get("edits").is_some());
    let operation = &edit["properties"]["edits"]["items"]["properties"];
    assert!(operation.get("oldString").is_some());
    assert!(operation.get("newString").is_some());
    assert!(operation.get("replaceAll").is_some());

    let patch = tools.apply_patch.definition().parameters;
    assert!(patch["properties"].get("patchText").is_some());
}

#[tokio::test]
async fn file_read_permission_escalates_only_for_paths_outside_the_workspace() {
    let (workspace, tools, inside_permission) = setup();
    let inside = workspace.path().join("inside.txt");
    std::fs::write(&inside, "inside\n").expect("inside fixture");

    tools
        .read
        .execute(
            json!({ "filePath": inside }),
            normal_context(inside_permission.clone()),
        )
        .await
        .expect("inside read");

    assert_eq!(inside_permission.permissions(), vec!["read"]);
    assert_eq!(inside_permission.asks()[0].patterns, vec!["inside.txt"]);

    let outside_root = tempfile::tempdir().expect("temporary external directory");
    let outside = outside_root.path().join("outside.txt");
    std::fs::write(&outside, "outside\n").expect("outside fixture");
    let outside_permission = Arc::new(RecordingPermission::default());
    tools
        .read
        .execute(
            json!({ "filePath": outside }),
            normal_context(outside_permission.clone()),
        )
        .await
        .expect("outside read");

    assert_eq!(
        outside_permission.permissions(),
        vec!["external_directory", "read"]
    );
    let asks = outside_permission.asks();
    let outside = outside.canonicalize().expect("canonical test executable");
    let outside_parent = outside.parent().expect("test executable parent");
    let outside_pattern = format!("{}/*", zuno_paths::wire_path(outside_parent));
    assert_eq!(asks[0].patterns, vec![outside_pattern.clone()]);
    assert_eq!(asks[0].always, vec![outside_pattern]);
    assert_eq!(
        asks[0].metadata["filepath"],
        zuno_paths::wire_path(&outside)
    );
    assert_eq!(
        asks[0].metadata["parentDir"],
        zuno_paths::wire_path(outside_parent)
    );
    assert_eq!(asks[1].patterns, vec![zuno_paths::wire_path(&outside)]);
}

#[tokio::test]
async fn file_edit_without_a_prior_read_is_refused_with_an_actionable_message() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("unread.txt");
    std::fs::write(&path, "before\n").expect("fixture");

    let error = tools
        .edit
        .execute(
            json!({
                "filePath": path,
                "edits": [{
                    "oldString": "before",
                    "newString": "after"
                }]
            }),
            normal_context(permission),
        )
        .await
        .expect_err("editing an unread file must fail");

    assert_eq!(
        source_message(&error),
        format!(
            "File must be read before editing. Use the read tool on {}, then retry the edit.",
            slash(&path)
        )
    );
    assert_eq!(
        std::fs::read_to_string(path).expect("unchanged"),
        "before\n"
    );
}

#[tokio::test]
async fn file_edit_unique_match_succeeds_and_exercises_the_formatter_seam() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let formatter = Arc::new(RecordingFormatter::default());
    let tools = FileTools::with_formatter(workspace.path(), formatter.clone()).expect("file tools");
    let permission = Arc::new(RecordingPermission::default());
    let path = workspace.path().join("unique.txt");
    std::fs::write(&path, "alpha\nbeta\n").expect("fixture");

    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(permission.clone()),
        )
        .await
        .expect("read first");
    let output = tools
        .edit
        .execute(
            json!({
                "filePath": path,
                "edits": [{
                    "oldString": "beta",
                    "newString": "gamma"
                }]
            }),
            normal_context(permission),
        )
        .await
        .expect("unique edit");

    assert_eq!(
        std::fs::read_to_string(&path).expect("edited"),
        "alpha\ngamma\n"
    );
    assert_eq!(output.output, "Edit applied successfully.");
    assert_eq!(output.metadata["replacements"], 1);
    assert_eq!(
        formatter.paths(),
        vec![path.canonicalize().expect("canonical formatted path")]
    );
}

#[tokio::test]
async fn file_edit_keeps_a_written_change_when_the_formatter_service_fails() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tools = FileTools::with_formatter(workspace.path(), Arc::new(FailingFormatter::always()))
        .expect("file tools");
    let permission = Arc::new(RecordingPermission::default());
    let path = workspace.path().join("formatter-error.txt");
    std::fs::write(&path, "before\n").expect("fixture");

    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(permission.clone()),
        )
        .await
        .expect("read first");
    let output = tools
        .edit
        .execute(
            json!({
                "filePath": path,
                "edits": [{
                    "oldString": "before",
                    "newString": "after"
                }]
            }),
            normal_context(permission),
        )
        .await
        .expect("the edit landed, so formatter transport failure is diagnostic");

    assert_eq!(
        std::fs::read_to_string(&path).expect("written edit"),
        "after\n"
    );
    assert!(
        output
            .output
            .contains("formatter service failed: formatter transport was lost"),
        "{}",
        output.output
    );
    assert_eq!(output.written_paths(), vec![slash(&path)]);
}

#[tokio::test]
async fn file_edit_rejects_a_non_unique_match_with_replace_all_guidance() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("duplicate.txt");
    std::fs::write(&path, "same\nsame\n").expect("fixture");

    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(permission.clone()),
        )
        .await
        .expect("read first");
    let error = tools
        .edit
        .execute(
            json!({
                "filePath": path,
                "edits": [{
                    "oldString": "same",
                    "newString": "changed"
                }]
            }),
            normal_context(permission),
        )
        .await
        .expect_err("ambiguous edit must fail");

    assert_eq!(
        source_message(&error),
        "edits[0].oldString matched 2 locations; provide more context or set replaceAll."
    );
    assert_eq!(
        std::fs::read_to_string(path).expect("unchanged"),
        "same\nsame\n"
    );
}

#[tokio::test]
async fn file_edit_replace_all_changes_every_exact_match() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("all.txt");
    std::fs::write(&path, "same\nsame\n").expect("fixture");

    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(permission.clone()),
        )
        .await
        .expect("read first");
    let output = tools
        .edit
        .execute(
            json!({
                "filePath": path,
                "edits": [{
                    "oldString": "same",
                    "newString": "changed",
                    "replaceAll": true
                }]
            }),
            normal_context(permission),
        )
        .await
        .expect("replace all");

    assert_eq!(output.metadata["replacements"], 2);
    assert_eq!(
        std::fs::read_to_string(path).expect("edited"),
        "changed\nchanged\n"
    );
}

#[tokio::test]
async fn file_edit_applies_the_operation_list_atomically() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("atomic.txt");
    std::fs::write(&path, "alpha\n").expect("fixture");

    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(permission.clone()),
        )
        .await
        .expect("read first");
    let error = tools
        .edit
        .execute(
            json!({
                "filePath": path,
                "edits": [
                    {"oldString": "alpha", "newString": "beta"},
                    {"oldString": "missing", "newString": "gamma"}
                ]
            }),
            normal_context(permission),
        )
        .await
        .expect_err("a later invalid operation must reject the whole list");

    assert!(source_message(&error).contains("edits[1].oldString was not found"));
    assert_eq!(std::fs::read_to_string(path).expect("unchanged"), "alpha\n");
}

#[tokio::test]
async fn file_read_supports_directory_listing_and_line_windows() {
    let (workspace, tools, permission) = setup();
    std::fs::create_dir(workspace.path().join("folder")).expect("folder fixture");
    std::fs::write(
        workspace.path().join("folder/file.txt"),
        "one\ntwo\nthree\n",
    )
    .expect("file fixture");

    let directory = tools
        .read
        .execute(
            json!({ "filePath": workspace.path(), "limit": 10 }),
            normal_context(permission.clone()),
        )
        .await
        .expect("directory read");
    assert!(directory.output.contains("folder/"));

    let file = tools
        .read
        .execute(
            json!({
                "filePath": workspace.path().join("folder/file.txt"),
                "offset": 2,
                "limit": 1
            }),
            normal_context(permission),
        )
        .await
        .expect("windowed read");
    assert!(file.output.contains("2: two"));
    assert!(!file.output.contains("1: one"));
    assert!(file.output.contains("Use offset=3 to continue"));
}

#[tokio::test]
async fn file_read_returns_image_and_pdf_attachments() {
    let (workspace, tools, permission) = setup();
    let image = workspace.path().join("image.png");
    let pdf = workspace.path().join("document.pdf");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nfixture").expect("image fixture");
    std::fs::write(&pdf, b"%PDF-1.7\nfixture").expect("pdf fixture");

    let image_output = tools
        .read
        .execute(
            json!({ "filePath": image }),
            normal_context(permission.clone()),
        )
        .await
        .expect("image read");
    assert_eq!(image_output.output, "Image read successfully");
    assert_eq!(image_output.attachments[0].mime, "image/png");
    assert!(
        image_output.attachments[0]
            .url
            .starts_with("data:image/png;base64,")
    );

    let pdf_output = tools
        .read
        .execute(json!({ "filePath": pdf }), normal_context(permission))
        .await
        .expect("pdf read");
    assert_eq!(pdf_output.output, "PDF read successfully");
    assert_eq!(pdf_output.attachments[0].mime, "application/pdf");
}

#[tokio::test]
async fn file_write_creates_and_overwrites_after_read() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("write.txt");

    tools
        .write
        .execute(
            json!({ "filePath": path, "content": "created\n" }),
            normal_context(permission.clone()),
        )
        .await
        .expect("create");
    assert_eq!(
        std::fs::read_to_string(&path).expect("created"),
        "created\n"
    );

    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(permission.clone()),
        )
        .await
        .expect("read before overwrite");
    tools
        .write
        .execute(
            json!({ "filePath": path, "content": "overwritten\n" }),
            normal_context(permission),
        )
        .await
        .expect("overwrite");
    assert_eq!(
        std::fs::read_to_string(path).expect("overwritten"),
        "overwritten\n"
    );
}

#[tokio::test]
async fn file_write_keeps_a_created_file_when_the_formatter_service_fails() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tools = FileTools::with_formatter(workspace.path(), Arc::new(FailingFormatter::always()))
        .expect("file tools");
    let permission = Arc::new(RecordingPermission::default());
    let path = workspace.path().join("formatter-error.txt");

    let output = tools
        .write
        .execute(
            json!({ "filePath": path, "content": "written\n" }),
            normal_context(permission),
        )
        .await
        .expect("the write landed, so formatter transport failure is diagnostic");

    assert_eq!(
        std::fs::read_to_string(&path).expect("written file"),
        "written\n"
    );
    assert!(
        output
            .output
            .contains("formatter service failed: formatter transport was lost"),
        "{}",
        output.output
    );
    assert_eq!(output.written_paths(), vec![slash(&path)]);
}

#[tokio::test]
async fn file_apply_patch_adds_updates_moves_and_deletes_files() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let formatter = Arc::new(RecordingFormatter::default());
    let tools = FileTools::with_formatter(workspace.path(), formatter.clone()).expect("file tools");
    let permission = Arc::new(RecordingPermission::default());
    std::fs::write(workspace.path().join("source.txt"), "old\n").expect("source fixture");
    std::fs::write(workspace.path().join("delete.txt"), "gone\n").expect("delete fixture");
    read_for_mutation(&tools, &workspace.path().join("source.txt")).await;
    read_for_mutation(&tools, &workspace.path().join("delete.txt")).await;

    let output = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Add File: added.txt\n",
                    "+one\n",
                    "+two\n",
                    "*** Update File: source.txt\n",
                    "*** Move to: moved.txt\n",
                    "@@\n",
                    "-old\n",
                    "+new\n",
                    "*** Delete File: delete.txt\n",
                    "*** End Patch"
                )
            }),
            normal_context(permission),
        )
        .await
        .expect("apply patch");

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("added.txt")).expect("added"),
        "one\ntwo\n"
    );
    assert!(!workspace.path().join("source.txt").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("moved.txt")).expect("moved"),
        "new\n"
    );
    assert!(!workspace.path().join("delete.txt").exists());
    assert!(output.output.contains("A added.txt"));
    assert!(output.output.contains("M moved.txt"));
    assert!(output.output.contains("D delete.txt"));
    assert_eq!(formatter.paths().len(), 2);

    // One patch spanning every file the call touched, so the viewer scrolls the whole
    // change rather than whichever file happened to be applied last.
    let patch = output.metadata["diff"]
        .as_str()
        .expect("a multi-file patch carries its own patch");
    assert!(patch.contains("added.txt"), "{patch}");
    assert!(patch.contains("+one"), "{patch}");
    assert!(patch.contains("moved.txt"), "{patch}");
    assert!(patch.contains("+new"), "{patch}");
    assert!(
        patch.contains("delete.txt") && patch.contains("-gone"),
        "a delete's pre-image is captured before it is removed:\n{patch}"
    );
    let diffs = output.file_diffs();
    assert_eq!(diffs.len(), 4, "add, move source, move target, delete");
    assert!(diffs.iter().any(|diff| {
        diff.path() == slash(&workspace.path().join("added.txt"))
            && diff.old_text().is_none()
            && diff.new_text() == "one\ntwo\n"
    }));
    assert!(diffs.iter().any(|diff| {
        diff.path() == slash(&workspace.path().join("source.txt"))
            && diff.old_text() == Some("old\n")
            && diff.new_text().is_empty()
    }));
    assert!(diffs.iter().any(|diff| {
        diff.path() == slash(&workspace.path().join("moved.txt"))
            && diff.old_text().is_none()
            && diff.new_text() == "new\n"
    }));
    assert!(diffs.iter().any(|diff| {
        diff.path() == slash(&workspace.path().join("delete.txt"))
            && diff.old_text() == Some("gone\n")
            && diff.new_text().is_empty()
    }));
}

#[tokio::test]
async fn file_apply_patch_context_drift_tells_the_model_how_to_recover() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("drifted.txt");
    std::fs::write(&path, "current\n").expect("fixture");
    read_for_mutation(&tools, &path).await;

    let error = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: drifted.txt\n",
                    "@@\n",
                    "-stale\n",
                    "+updated\n",
                    "*** End Patch"
                )
            }),
            normal_context(permission),
        )
        .await
        .expect_err("stale hunk context must be rejected");

    let ToolError::MutationConflict {
        conflict, source, ..
    } = &error
    else {
        panic!("expected typed mutation conflict, got {error:?}");
    };
    assert_eq!(conflict.kind, ToolMutationConflictKind::ContextMismatch);
    assert_eq!(conflict.resource, "drifted.txt");
    assert_eq!(conflict.hunk_index, Some(1));
    assert_eq!(conflict.required_action(), "reread_and_revise");
    assert_eq!(conflict.operation_digest.len(), 64);
    assert_eq!(conflict.observed_digest.as_ref().map(String::len), Some(64));
    let message = source.to_string();
    assert!(message.contains("read the current file"), "{message}");
    assert!(message.contains("smaller patch"), "{message}");
    assert!(
        message.contains("do not resend the same patch"),
        "{message}"
    );
    assert_eq!(
        std::fs::read_to_string(path).expect("unchanged file"),
        "current\n"
    );
}

#[tokio::test]
async fn file_apply_patch_requires_a_current_read_before_touching_existing_files() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("unread.txt");
    std::fs::write(&path, "old\n").expect("fixture");

    let error = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: unread.txt\n",
                    "@@\n",
                    "-old\n",
                    "+new\n",
                    "*** End Patch"
                )
            }),
            normal_context(permission),
        )
        .await
        .expect_err("an existing file needs a read receipt");

    let ToolError::MutationConflict { conflict, .. } = error else {
        panic!("expected mutation conflict, got {error:?}");
    };
    assert_eq!(conflict.kind, ToolMutationConflictKind::ReadRequired);
    assert_eq!(conflict.resource, "unread.txt");
    assert_eq!(conflict.required_action(), "reread");
    assert_eq!(
        std::fs::read_to_string(path).expect("unchanged file"),
        "old\n"
    );
}

#[tokio::test]
async fn file_apply_patch_rejects_a_file_that_changed_after_it_was_read() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("stale.txt");
    std::fs::write(&path, "old\n").expect("fixture");
    read_for_mutation(&tools, &path).await;
    std::fs::write(&path, "changed elsewhere\n").expect("external change");

    let error = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: stale.txt\n",
                    "@@\n",
                    "-old\n",
                    "+new\n",
                    "*** End Patch"
                )
            }),
            normal_context(permission),
        )
        .await
        .expect_err("a stale read receipt must not authorize a write");

    let ToolError::MutationConflict { conflict, .. } = error else {
        panic!("expected mutation conflict, got {error:?}");
    };
    assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
    assert_eq!(conflict.resource, "stale.txt");
    assert_eq!(conflict.observed_digest.as_ref().map(String::len), Some(64));
    assert_eq!(
        std::fs::read_to_string(path).expect("external change remains"),
        "changed elsewhere\n"
    );
}

#[tokio::test]
async fn file_apply_patch_blocks_an_identical_failed_patch_until_the_patch_changes() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("replay.txt");
    std::fs::write(&path, "current\n").expect("fixture");
    read_for_mutation(&tools, &path).await;
    let stale_patch = json!({
        "patchText": concat!(
            "*** Begin Patch\n",
            "*** Update File: replay.txt\n",
            "@@\n",
            "-stale\n",
            "+updated\n",
            "*** End Patch"
        )
    });

    let first = tools
        .apply_patch
        .execute(stale_patch.clone(), normal_context(permission.clone()))
        .await
        .expect_err("the first stale context fails");
    assert!(matches!(
        first,
        ToolError::MutationConflict {
            conflict,
            ..
        } if conflict.kind == ToolMutationConflictKind::ContextMismatch
    ));

    read_for_mutation(&tools, &path).await;
    let replay = tools
        .apply_patch
        .execute(stale_patch, normal_context(permission))
        .await
        .expect_err("re-reading unchanged bytes must not permit an identical replay");
    let ToolError::MutationConflict { conflict, .. } = replay else {
        panic!("expected identical replay conflict, got {replay:?}");
    };
    assert_eq!(conflict.kind, ToolMutationConflictKind::IdenticalReplay);
    assert_eq!(conflict.required_action(), "reread_and_revise");

    tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: replay.txt\n",
                    "@@\n",
                    "-current\n",
                    "+updated\n",
                    "*** End Patch"
                )
            }),
            normal_context(Arc::new(RecordingPermission::default())),
        )
        .await
        .expect("a revised patch applies against the current image");
    assert_eq!(
        std::fs::read_to_string(path).expect("updated file"),
        "updated\n"
    );
}

#[tokio::test]
async fn file_apply_patch_identical_replay_preserves_the_actual_conflicted_file_and_hunk() {
    let (workspace, tools, permission) = setup();
    let first = workspace.path().join("first.txt");
    let second = workspace.path().join("second.txt");
    std::fs::write(&first, "one\n").expect("first fixture");
    std::fs::write(&second, "two\n").expect("second fixture");
    read_for_mutation(&tools, &first).await;
    read_for_mutation(&tools, &second).await;
    let patch = json!({
        "patchText": concat!(
            "*** Begin Patch\n",
            "*** Update File: first.txt\n",
            "@@\n",
            "-one\n",
            "+ONE\n",
            "*** Update File: second.txt\n",
            "@@ expected section\n",
            "-stale\n",
            "+TWO\n",
            "*** End Patch"
        )
    });

    let first_error = tools
        .apply_patch
        .execute(patch.clone(), normal_context(permission.clone()))
        .await
        .expect_err("the second file hunk must fail");
    let ToolError::MutationConflict {
        conflict: first_conflict,
        ..
    } = first_error
    else {
        panic!("expected context conflict");
    };
    assert_eq!(
        first_conflict.kind,
        ToolMutationConflictKind::ContextMismatch
    );
    assert_eq!(first_conflict.resource, "second.txt");
    assert_eq!(first_conflict.hunk_index, Some(1));
    assert_eq!(
        first_conflict.hunk_header.as_deref(),
        Some("expected section")
    );

    read_for_mutation(&tools, &first).await;
    read_for_mutation(&tools, &second).await;
    let replay = tools
        .apply_patch
        .execute(patch, normal_context(permission))
        .await
        .expect_err("an identical multi-file replay remains blocked");
    let ToolError::MutationConflict { conflict, .. } = replay else {
        panic!("expected identical replay conflict");
    };
    assert_eq!(conflict.kind, ToolMutationConflictKind::IdenticalReplay);
    assert_eq!(conflict.resource, "second.txt");
    assert_eq!(conflict.hunk_index, Some(1));
    assert_eq!(conflict.hunk_header.as_deref(), Some("expected section"));
    assert_eq!(std::fs::read_to_string(first).expect("first"), "one\n");
    assert_eq!(std::fs::read_to_string(second).expect("second"), "two\n");
}

#[tokio::test]
async fn file_apply_patch_preflights_every_file_before_the_first_write() {
    let (workspace, tools, permission) = setup();
    let first = workspace.path().join("first.txt");
    let second = workspace.path().join("second.txt");
    std::fs::write(&first, "one\n").expect("first fixture");
    std::fs::write(&second, "two\n").expect("second fixture");
    read_for_mutation(&tools, &first).await;

    let error = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: first.txt\n",
                    "@@\n",
                    "-one\n",
                    "+ONE\n",
                    "*** Update File: second.txt\n",
                    "@@\n",
                    "-two\n",
                    "+TWO\n",
                    "*** End Patch"
                )
            }),
            normal_context(permission),
        )
        .await
        .expect_err("the unread second file rejects the whole patch");

    assert!(matches!(
        error,
        ToolError::MutationConflict {
            conflict,
            ..
        } if conflict.kind == ToolMutationConflictKind::ReadRequired
    ));
    assert_eq!(std::fs::read_to_string(first).expect("first"), "one\n");
    assert_eq!(std::fs::read_to_string(second).expect("second"), "two\n");
}

#[tokio::test]
async fn file_apply_patch_preserves_crlf_bom_and_missing_final_newline() {
    let (workspace, tools, permission) = setup();
    let crlf = workspace.path().join("crlf.txt");
    let no_final = workspace.path().join("no-final.txt");
    std::fs::write(&crlf, b"\xef\xbb\xbfold\r\nkeep\r\n").expect("CRLF fixture");
    std::fs::write(&no_final, b"tail").expect("no-final fixture");
    read_for_mutation(&tools, &crlf).await;
    read_for_mutation(&tools, &no_final).await;

    tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: crlf.txt\n",
                    "@@\n",
                    "-old\n",
                    "+new\n",
                    "*** Update File: no-final.txt\n",
                    "@@\n",
                    "-tail\n",
                    "+done\n",
                    "*** End Patch"
                )
            }),
            normal_context(permission),
        )
        .await
        .expect("line-ending aware patch");

    assert_eq!(
        std::fs::read(crlf).expect("CRLF result"),
        b"\xef\xbb\xbfnew\r\nkeep\r\n"
    );
    assert_eq!(std::fs::read(no_final).expect("no-final result"), b"done");
}

#[tokio::test]
async fn file_apply_patch_reports_an_uncertain_multi_file_outcome_after_a_late_failure() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tools = FileTools::with_formatter(workspace.path(), Arc::new(FailingFormatter::on_call(2)))
        .expect("file tools");

    let error = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Add File: first.txt\n",
                    "+first\n",
                    "*** Add File: second.txt\n",
                    "+second\n",
                    "*** End Patch"
                )
            }),
            normal_context(Arc::new(RecordingPermission::default())),
        )
        .await
        .expect_err("the formatter connection was lost after both writes");

    let ToolError::Uncertain {
        tool,
        applied_paths,
        source,
    } = error
    else {
        panic!("expected uncertain outcome, got {error:?}");
    };
    assert_eq!(tool, "apply_patch");
    assert_eq!(
        applied_paths,
        vec![
            slash(&workspace.path().join("first.txt")),
            slash(&workspace.path().join("second.txt")),
        ]
    );
    assert_eq!(source.to_string(), "tool apply_patch failed");
    assert!(
        source
            .source()
            .is_some_and(|cause| cause.to_string().contains("formatter transport was lost")),
        "the uncertain outcome lost the formatter cause: {source:?}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("first.txt")).expect("first write"),
        "first\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("second.txt")).expect("second write"),
        "second\n"
    );
}

#[tokio::test]
async fn file_apply_patch_never_overwrites_a_file_changed_between_read_and_write() {
    // The formatter stands in for anything that can touch the worktree while a patch is
    // being applied — a background job, another tool call, the user's editor. It edits
    // the file the second half of the patch is about to write, so the bytes that were
    // verified during preparation are no longer the bytes on disk.
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let first = workspace.path().join("first.txt");
    let second = workspace.path().join("second.txt");
    std::fs::write(&first, "one\n").expect("first fixture");
    std::fs::write(&second, "two\n").expect("second fixture");
    let tools = FileTools::with_formatter(
        workspace.path(),
        Arc::new(ConcurrentEditor {
            path: second.clone(),
            contents: "somebody else\n",
        }),
    )
    .expect("file tools");
    read_for_mutation(&tools, &first).await;
    read_for_mutation(&tools, &second).await;

    let error = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: first.txt\n",
                    "@@\n",
                    "-one\n",
                    "+ONE\n",
                    "*** Update File: second.txt\n",
                    "@@\n",
                    "-two\n",
                    "+TWO\n",
                    "*** End Patch"
                )
            }),
            normal_context(Arc::new(RecordingPermission::default())),
        )
        .await
        .expect_err("a file that moved under the write must not be overwritten");

    // The first file's bytes really are on disk, so the call as a whole stays uncertain
    // rather than becoming a plain refusal; the refusal is its typed cause.
    let ToolError::Uncertain {
        applied_paths,
        source,
        ..
    } = &error
    else {
        panic!("expected an uncertain outcome after the first write landed, got {error:?}");
    };
    assert_eq!(applied_paths, &vec![slash(&first)]);
    let conflict = mutation_conflict(source.as_ref());
    assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
    assert_eq!(conflict.resource, "second.txt");
    assert_eq!(conflict.required_action(), "reread");
    assert_eq!(
        std::fs::read_to_string(&second).expect("the concurrent edit survives"),
        "somebody else\n",
        "the patch must not overwrite bytes it never read"
    );
    assert_eq!(
        std::fs::read_to_string(&first).expect("the first write landed"),
        "ONE\n"
    );
}

#[tokio::test]
async fn file_apply_patch_never_overwrites_a_path_that_appeared_after_preparation() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let first = workspace.path().join("first.txt");
    let added = workspace.path().join("added.txt");
    std::fs::write(&first, "one\n").expect("first fixture");
    let tools = FileTools::with_formatter(
        workspace.path(),
        Arc::new(ConcurrentEditor {
            path: added.clone(),
            contents: "somebody else\n",
        }),
    )
    .expect("file tools");
    read_for_mutation(&tools, &first).await;

    let error = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: first.txt\n",
                    "@@\n",
                    "-one\n",
                    "+ONE\n",
                    "*** Add File: added.txt\n",
                    "+added\n",
                    "*** End Patch"
                )
            }),
            normal_context(Arc::new(RecordingPermission::default())),
        )
        .await
        .expect_err("an add whose path appeared concurrently must not overwrite it");

    let ToolError::Uncertain { source, .. } = &error else {
        panic!("expected an uncertain outcome after the first write landed, got {error:?}");
    };
    let conflict = mutation_conflict(source.as_ref());
    assert_eq!(conflict.kind, ToolMutationConflictKind::StaleRead);
    assert_eq!(conflict.resource, "added.txt");
    assert_eq!(
        std::fs::read_to_string(&added).expect("the concurrent create survives"),
        "somebody else\n"
    );
}

#[tokio::test]
async fn file_every_writing_tool_a_model_can_see_reports_the_paths_it_wrote() {
    // The defect this pins: a downstream host — the TUI's language-server check — decided
    // which completions had changed a file by matching the tool's *name* against a
    // hand-kept `["edit", "write", "patch"]`. The registry's third id is `apply_patch`,
    // and a GPT model is shown only `read` and `apply_patch`, so on those models a
    // successful patch matched nothing and no file was ever checked.
    //
    // Driven off `model_visible` — the very function that decides what a model sees —
    // rather than a literal list here, so a tool added or renamed cannot quietly stop
    // reporting. Every exposed tool is invoked for real and answered against the disk: a
    // tool that wrote something must name it, and one that wrote nothing must name
    // nothing.
    for model in ["gpt-5", "claude-sonnet-4"] {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let tools = FileTools::with_formatter(workspace.path(), Arc::new(NoopFormatter))
            .expect("file tools");
        std::fs::write(workspace.path().join("existing.txt"), "old\n").expect("fixture");

        for tool in tools.model_visible() {
            let id = tool.definition().id.clone();
            let arguments = match id.as_str() {
                "read" => json!({ "filePath": workspace.path().join("existing.txt") }),
                "write" => json!({
                    "filePath": workspace.path().join("written.txt"),
                    "content": "fresh\n",
                }),
                "apply_patch" => json!({
                    "patchText": concat!(
                        "*** Begin Patch\n",
                        "*** Add File: patched-one.txt\n",
                        "+one\n",
                        "*** Add File: patched-two.txt\n",
                        "+two\n",
                        "*** End Patch"
                    )
                }),
                other => panic!(
                    "model {model} is shown a tool this test does not exercise: {other}. \
                     A new file tool must state which paths it writes, or the \
                     language-server check silently skips its edits."
                ),
            };
            let output = tool
                .execute(
                    arguments,
                    normal_context(Arc::new(RecordingPermission::default())),
                )
                .await
                .unwrap_or_else(|error| panic!("{id} on {model}: {error}"));

            let reported = output.written_paths();
            let expected: Vec<String> = match id.as_str() {
                "read" => Vec::new(),
                "write" => vec![slash(&workspace.path().join("written.txt"))],
                "apply_patch" => vec![
                    slash(&workspace.path().join("patched-one.txt")),
                    slash(&workspace.path().join("patched-two.txt")),
                ],
                _ => unreachable!("the arguments match arm already panicked"),
            };
            assert_eq!(
                reported, expected,
                "{id} on {model} reported {reported:?} as written; a host checking \
                 diagnostics reads exactly this list"
            );
            for path in &reported {
                assert!(
                    Path::new(path).is_file(),
                    "{id} reported {path} as written but nothing is there"
                );
            }
        }
    }
}

/// A path spelled the way the tools spell one in metadata.
fn slash(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| parent.canonicalize().ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_owned())
    });
    zuno_paths::wire_path(&canonical)
}

#[tokio::test]
async fn file_apply_patch_does_not_report_a_file_it_deleted_as_written() {
    // A deleted file has no diagnostics, and asking a language server about one produces
    // a report about a file that is not there — which reads as a problem with this turn.
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tools =
        FileTools::with_formatter(workspace.path(), Arc::new(NoopFormatter)).expect("file tools");
    std::fs::write(workspace.path().join("source.txt"), "old\n").expect("source fixture");
    std::fs::write(workspace.path().join("delete.txt"), "gone\n").expect("delete fixture");
    read_for_mutation(&tools, &workspace.path().join("source.txt")).await;
    read_for_mutation(&tools, &workspace.path().join("delete.txt")).await;

    let output = tools
        .apply_patch
        .execute(
            json!({
                "patchText": concat!(
                    "*** Begin Patch\n",
                    "*** Update File: source.txt\n",
                    "*** Move to: moved.txt\n",
                    "@@\n",
                    "-old\n",
                    "+new\n",
                    "*** Delete File: delete.txt\n",
                    "*** End Patch"
                )
            }),
            normal_context(Arc::new(RecordingPermission::default())),
        )
        .await
        .expect("apply patch");

    assert_eq!(
        output.written_paths(),
        vec![slash(&workspace.path().join("moved.txt"))],
        "only the surviving destination is written; neither the deleted file nor the \
         move's vacated source still exists"
    );
}

#[tokio::test]
async fn file_walks_honor_an_already_set_interrupt() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tools =
        FileTools::with_formatter(workspace.path(), Arc::new(NoopFormatter)).expect("file tools");
    std::fs::write(workspace.path().join("file.txt"), "one\ntwo\n").expect("fixture");

    let error = tools
        .read
        .execute(
            json!({ "filePath": workspace.path().join("file.txt") }),
            context(
                Arc::new(RecordingPermission::default()),
                Arc::new(Interrupted),
            ),
        )
        .await
        .expect_err("interrupted read");

    assert!(source_message(&error).contains("interrupted"));
}

#[test]
fn file_model_surface_has_one_structured_editor_and_one_full_write_fallback() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tools = FileTools::new(workspace.path()).expect("file tools");
    let ids = tools
        .model_visible()
        .into_iter()
        .map(|tool| tool.id().to_owned())
        .collect::<Vec<_>>();

    assert_eq!(ids, ["read", "write", "apply_patch"]);
}

#[test]
fn file_apply_patch_description_defines_selection_and_recovery_rules() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tools = FileTools::new(workspace.path()).expect("file tools");
    let description = tools.apply_patch.description();

    for clause in [
        "Call this tool directly",
        "generated files or bulk mechanical rewrites",
        "read the affected file",
        "smaller patch",
        "Do not resend the same patch",
        "uncertain",
        "Use `write` only",
        "`git apply --check`",
        "structured `edit`",
        "not Git metadata",
        "`git reset --hard`",
        "`git checkout --`",
    ] {
        assert!(
            description.contains(clause),
            "apply_patch description is missing `{clause}`:\n{description}"
        );
    }
}

#[test]
fn file_edit_and_write_descriptions_define_git_independent_stale_read_recovery() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let tools = FileTools::new(workspace.path()).expect("file tools");

    for (name, description) in [
        ("edit", tools.edit.description()),
        ("write", tools.write.description()),
    ] {
        for clause in [
            "exact bytes recorded by `read`",
            "does not depend on a Git repository",
            "re-read",
            "`git reset --hard`",
            "`git checkout --`",
        ] {
            assert!(
                description.contains(clause),
                "{name} description is missing `{clause}`:\n{description}"
            );
        }
    }
}

/// Defect: the TUI diff viewer was permanently empty for every tool that edits code,
/// because `edit` reports `"Edit applied successfully."` and the patch existed nowhere.
/// This pins the patch onto the result, which is the only place the viewer can read it.
#[tokio::test]
async fn file_edit_reports_the_patch_of_what_it_changed() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("edited.txt");
    std::fs::write(&path, "one\ntwo\nthree\n").expect("fixture");

    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(permission.clone()),
        )
        .await
        .expect("read first");
    let output = tools
        .edit
        .execute(
            json!({
                "filePath": path,
                "edits": [{
                    "oldString": "two",
                    "newString": "TWO"
                }]
            }),
            normal_context(permission),
        )
        .await
        .expect("edit");

    // The sentence stays the output: the patch must not displace what the model reads.
    assert_eq!(output.output, "Edit applied successfully.");
    let patch = output.metadata["diff"]
        .as_str()
        .expect("a mutation carries its patch as metadata");
    assert!(patch.lines().any(|line| line.starts_with("@@")), "{patch}");
    assert!(patch.contains("-two"), "{patch}");
    assert!(patch.contains("+TWO"), "{patch}");
    assert!(
        patch.contains("edited.txt"),
        "the patch names the file it changed:\n{patch}"
    );
    let diffs = output.file_diffs();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path(), slash(&path));
    assert_eq!(diffs[0].old_text(), Some("one\ntwo\nthree\n"));
    assert_eq!(diffs[0].new_text(), "one\nTWO\nthree\n");
}

#[tokio::test]
async fn file_write_reports_the_patch_for_both_a_creation_and_an_overwrite() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("written.txt");

    let created = tools
        .write
        .execute(
            json!({ "filePath": path, "content": "first\n" }),
            normal_context(permission.clone()),
        )
        .await
        .expect("create");
    let patch = created.metadata["diff"]
        .as_str()
        .expect("creating a file is a change");
    assert!(patch.contains("+first"), "{patch}");
    let diffs = created.file_diffs();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path(), slash(&path));
    assert_eq!(diffs[0].old_text(), None);
    assert_eq!(diffs[0].new_text(), "first\n");

    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(permission.clone()),
        )
        .await
        .expect("read before overwrite");
    let overwritten = tools
        .write
        .execute(
            json!({ "filePath": path, "content": "second\n" }),
            normal_context(permission),
        )
        .await
        .expect("overwrite");
    let patch = overwritten.metadata["diff"]
        .as_str()
        .expect("an overwrite is a change");
    assert!(patch.contains("-first"), "{patch}");
    assert!(patch.contains("+second"), "{patch}");
    let diffs = overwritten.file_diffs();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path(), slash(&path));
    assert_eq!(diffs[0].old_text(), Some("first\n"));
    assert_eq!(diffs[0].new_text(), "second\n");
}

/// A write of byte-identical content attaches nothing, so a present `diff` always means
/// "here is the change" rather than "something ran". An empty viewer beats a lying one.
#[tokio::test]
async fn file_write_reports_no_patch_when_the_content_is_unchanged() {
    let (workspace, tools, permission) = setup();
    let path = workspace.path().join("same.txt");
    std::fs::write(&path, "identical\n").expect("fixture");

    tools
        .read
        .execute(
            json!({ "filePath": path }),
            normal_context(permission.clone()),
        )
        .await
        .expect("read first");
    let output = tools
        .write
        .execute(
            json!({ "filePath": path, "content": "identical\n" }),
            normal_context(permission),
        )
        .await
        .expect("rewrite");

    assert!(
        output.metadata.get("diff").is_none(),
        "an unchanged file has no patch to show, got {:?}",
        output.metadata.get("diff")
    );
}
