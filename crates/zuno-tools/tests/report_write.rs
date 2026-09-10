use std::sync::Arc;
use zuno_tool::{AllowAll, DenyAll, NeverInterrupted, ToolContext, TypedTool};
use zuno_tools::report_write::{METADATA_KEY, ReportArtifact, ReportWriteParams, ReportWriteTool};

fn context(call: &str) -> ToolContext {
    ToolContext::new(
        "ses_investigation",
        "msg_report",
        call,
        "explorer",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn report(name: &str, content: &str) -> ReportWriteParams {
    ReportWriteParams {
        name: name.to_owned(),
        content: content.to_owned(),
    }
}

#[tokio::test]
async fn an_investigator_publishes_complete_reports_without_editing_project_files() {
    let root = tempfile::tempdir().expect("workspace");
    let source = root.path().join("source.rs");
    std::fs::write(&source, "original source").expect("seed");
    let tool = ReportWriteTool::new(root.path());
    let content = "# 调查报告\n\nA complete, verifiable finding.\n";
    let first = tool
        .run(report("audit.md", content), context("call_one"))
        .await
        .expect("publish");
    let receipt: ReportArtifact =
        serde_json::from_value(first.metadata[METADATA_KEY].clone()).expect("receipt");
    assert_eq!(
        std::fs::read_to_string(&receipt.path).expect("read"),
        content
    );
    assert_eq!(receipt.bytes, content.len());
    assert_eq!(receipt.sha256.len(), 64);
    assert_eq!(receipt.session_id, "ses_investigation");
    assert!(
        std::path::Path::new(&receipt.path)
            .canonicalize()
            .expect("resolve report")
            .starts_with(
                root.path()
                    .canonicalize()
                    .expect("resolve workspace")
                    .join(".zuno/reports")
            )
    );
    assert_eq!(
        std::fs::read_to_string(&source).expect("source"),
        "original source"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join(".zuno/reports/.gitignore")).expect("exclusion"),
        "*\n"
    );

    let second = tool
        .run(report("audit.md", "Revised report"), context("call_two"))
        .await
        .expect("new artifact");
    assert_ne!(
        first.metadata[METADATA_KEY]["path"],
        second.metadata[METADATA_KEY]["path"]
    );
    assert_eq!(
        std::fs::read_to_string(&receipt.path).expect("original report"),
        content
    );
    assert!(
        tool.run(report("audit.md", "overwrite"), context("call_one"))
            .await
            .is_err(),
        "an existing receipt's file must never be overwritten"
    );
}

#[tokio::test]
async fn denied_report_authority_creates_no_directories_or_files() {
    let root = tempfile::tempdir().expect("workspace");
    let mut ctx = context("denied");
    ctx.permission = Arc::new(DenyAll);
    let result = ReportWriteTool::new(root.path())
        .run(report("audit.md", "findings"), ctx)
        .await;
    assert!(result.is_err());
    assert!(!root.path().join(".zuno").exists());
}

#[tokio::test]
async fn report_names_cannot_select_other_files_on_any_platform() {
    let root = tempfile::tempdir().expect("workspace");
    let tool = ReportWriteTool::new(root.path());
    for name in [
        "",
        "../source.rs",
        "/tmp/report.md",
        "C:\\report.md",
        "folder/report.md",
        "folder\\report.md",
        ".gitignore",
        "CON.txt",
        "LPT1.md",
        "COM².txt",
        "x:stream",
        "name.",
        "name ",
        "bad\nname",
    ] {
        assert!(
            tool.run(report(name, "findings"), context("invalid"))
                .await
                .is_err(),
            "{name:?} must be rejected"
        );
    }
    assert!(!root.path().join(".zuno").exists());
}

#[tokio::test]
async fn report_size_is_bounded_before_a_write_is_admitted() {
    let root = tempfile::tempdir().expect("workspace");
    assert!(
        ReportWriteTool::new(root.path())
            .run(
                report("large.md", &"x".repeat(1024 * 1024 + 1)),
                context("large")
            )
            .await
            .is_err()
    );
    assert!(!root.path().join(".zuno").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_report_ancestor_cannot_redirect_the_host_writer() {
    for ancestor in [".zuno", ".zuno/reports"] {
        let root = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let link = root.path().join(ancestor);
        std::fs::create_dir_all(link.parent().expect("parent")).expect("parent exists");
        std::os::unix::fs::symlink(outside.path(), &link).expect("link");
        let result = ReportWriteTool::new(root.path())
            .run(report("audit.md", "findings"), context("linked"))
            .await;
        assert!(result.is_err(), "{ancestor}");
        assert_eq!(
            std::fs::read_dir(outside.path()).expect("outside").count(),
            0
        );
    }
}
