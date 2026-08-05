use async_trait::async_trait;
use oc_error::ToolError;
use oc_tool::{InterruptHandle, NeverInterrupted, PermissionAsk, PermissionAsker, ToolContext};
use oc_tools::{FileFormatter, FileTools, NoopFormatter, uses_apply_patch};
use serde_json::json;
use std::error::Error as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

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
    async fn ask(&self, _tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
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
    assert!(edit["properties"].get("oldString").is_some());
    assert!(edit["properties"].get("newString").is_some());
    assert!(edit["properties"].get("replaceAll").is_some());

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

    let outside_permission = Arc::new(RecordingPermission::default());
    tools
        .read
        .execute(
            json!({ "filePath": "/etc/hosts" }),
            normal_context(outside_permission.clone()),
        )
        .await
        .expect("outside read");

    assert_eq!(
        outside_permission.permissions(),
        vec!["external_directory", "read"]
    );
    let asks = outside_permission.asks();
    assert_eq!(asks[0].patterns, vec!["/etc/*"]);
    assert_eq!(asks[0].always, vec!["/etc/*"]);
    assert_eq!(asks[0].metadata["filepath"], "/etc/hosts");
    assert_eq!(asks[0].metadata["parentDir"], "/etc");
    assert_eq!(asks[1].patterns, vec!["/etc/hosts"]);
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
                "oldString": "before",
                "newString": "after"
            }),
            normal_context(permission),
        )
        .await
        .expect_err("editing an unread file must fail");

    assert_eq!(
        source_message(&error),
        format!(
            "File must be read before editing. Use the read tool on {}, then retry the edit.",
            path.display()
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
                "oldString": "beta",
                "newString": "gamma"
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
    assert_eq!(formatter.paths(), vec![path]);
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
                "oldString": "same",
                "newString": "changed"
            }),
            normal_context(permission),
        )
        .await
        .expect_err("ambiguous edit must fail");

    assert_eq!(
        source_message(&error),
        "Found multiple matches for oldString; provide more context or use replaceAll."
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
                "oldString": "same",
                "newString": "changed",
                "replaceAll": true
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
async fn file_apply_patch_adds_updates_moves_and_deletes_files() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let formatter = Arc::new(RecordingFormatter::default());
    let tools = FileTools::with_formatter(workspace.path(), formatter.clone()).expect("file tools");
    let permission = Arc::new(RecordingPermission::default());
    std::fs::write(workspace.path().join("source.txt"), "old\n").expect("source fixture");
    std::fs::write(workspace.path().join("delete.txt"), "gone\n").expect("delete fixture");

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
fn file_model_condition_matches_the_oracle_registry_rule() {
    assert!(uses_apply_patch("gpt-5"));
    assert!(uses_apply_patch("openai/gpt-5.2-codex"));
    assert!(!uses_apply_patch("gpt-4.1"));
    assert!(!uses_apply_patch("openai/gpt-oss-120b"));
    assert!(!uses_apply_patch("claude-sonnet-4"));
}
