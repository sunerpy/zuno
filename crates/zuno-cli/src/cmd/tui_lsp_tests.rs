//! The two decisions this module makes that a wrong answer would be dangerous for:
//! whether a probe exists at all, and whether a position is one-based.

use super::*;

/// Whether `command` is runnable, for a test that needs a real language server.
///
/// A `PATH` walk rather than spawning it with `--version`: several language servers have
/// no `--version` that exits, and this only has to answer "is it there".
fn on_path(command: &str) -> bool {
    if command.contains('/') {
        return Path::new(command).is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(command).is_file())
    })
}

/// A wake sink whose receiver is dropped, which is the shape a nudge has to survive.
///
/// `try_send` on a closed channel fails, and it must cost nothing: the reports are still
/// queued, and a nudge nobody is listening for is not an error.
fn wake() -> mpsc::Sender<zuno_tui::app::TerminalEvent> {
    zuno_tui::app::terminal_event_channel().0
}

fn config(json: &str) -> zuno_config::schema::Config {
    serde_json::from_str(json).expect("valid configuration")
}

fn lsp_diagnostic(
    line: u32,
    character: u32,
    severity: Option<u32>,
) -> zuno_lsp::client::Diagnostic {
    zuno_lsp::client::Diagnostic {
        range: zuno_lsp::client::Range {
            start: zuno_lsp::client::Position { line, character },
            end: zuno_lsp::client::Position {
                line,
                character: character + 4,
            },
        },
        severity,
        code: None,
        source: Some(String::from("rustc")),
        message: String::from("mismatched   types\n  expected String"),
        extra: serde_json::Map::new(),
    }
}

#[test]
fn tui_lsp_no_probe_when_configuration_enables_nothing() {
    // `if (!cfg.lsp)` disables every server. A probe here would answer every edit as
    // unchecked, putting a row in the transcript per write for the many users who have no
    // `lsp` key at all.
    let root = tempfile::tempdir().expect("a temporary directory");
    assert!(Probe::resolve(&config("{}"), root.path(), wake()).is_none());
    assert!(Probe::resolve(&config(r#"{"lsp":false}"#), root.path(), wake()).is_none());
}

#[test]
fn tui_lsp_probe_exists_once_a_server_is_enabled() {
    let root = tempfile::tempdir().expect("a temporary directory");
    assert!(
        Probe::resolve(&config(r#"{"lsp":true}"#), root.path(), wake()).is_some(),
        "`lsp: true` enables the built-in definitions, so a probe must exist"
    );
}

#[test]
fn tui_lsp_converts_zero_based_positions_to_one_based() {
    // The whole reason `convert` exists. LSP positions are zero-based and every editor a
    // user pastes a location into is one-based, so a report that passed them through would
    // be off by one on every row.
    let converted = convert(&lsp_diagnostic(41, 4, Some(1)));
    assert_eq!(converted.line, 42);
    assert_eq!(converted.column, 5);
}

#[test]
fn tui_lsp_conversion_never_underflows_at_the_origin() {
    let converted = convert(&lsp_diagnostic(0, 0, Some(1)));
    assert_eq!(converted.line, 1);
    assert_eq!(converted.column, 1);
}

#[test]
fn tui_lsp_conversion_flattens_a_multiline_message_onto_one_row() {
    let converted = convert(&lsp_diagnostic(0, 0, Some(1)));
    assert_eq!(converted.message, "mismatched types expected String");
    assert!(!converted.message.contains('\n'));
}

#[test]
fn tui_lsp_conversion_carries_the_severity_and_the_source() {
    assert_eq!(
        convert(&lsp_diagnostic(0, 0, Some(2))).severity,
        Severity::Warning
    );
    assert_eq!(
        convert(&lsp_diagnostic(0, 0, None)).severity,
        Severity::Error,
        "an unstated severity must not be downgraded"
    );
    assert_eq!(
        convert(&lsp_diagnostic(0, 0, Some(1))).source.as_deref(),
        Some("rustc")
    );
}

#[tokio::test]
async fn tui_lsp_check_edits_drains_its_inlet_when_there_is_no_probe() {
    // Dropping the receiver instead would make the screen's `try_send` fail, and a
    // failure reported nowhere is worse than a no-op.
    let (edits, edit_receiver) = mpsc::channel(4);
    let (reports, mut report_receiver) = mpsc::channel(4);
    let task = tokio::spawn(check_edits(None, edit_receiver, reports));
    for _ in 0..4 {
        edits
            .try_send(vec![String::from("src/lib.rs")])
            .expect("the inlet accepts a batch with no probe attached");
    }
    drop(edits);
    task.await.expect("the task finishes when the inlet closes");
    assert!(
        report_receiver.try_recv().is_err(),
        "a probe-less check produced a report"
    );
}

#[tokio::test]
async fn tui_lsp_check_edits_skips_a_path_that_is_not_a_file() {
    // A model may report a write to a path it then removed, or to a directory. Asking a
    // language server about it would fail per file and produce a row per failure.
    let root = tempfile::tempdir().expect("a temporary directory");
    let probe = Probe::resolve(&config(r#"{"lsp":true}"#), root.path(), wake()).expect("a probe");
    let (edits, edit_receiver) = mpsc::channel(4);
    let (reports, mut report_receiver) = mpsc::channel(4);
    let task = tokio::spawn(check_edits(Some(probe), edit_receiver, reports));
    edits
        .try_send(vec![String::from("does/not/exist.rs")])
        .expect("the inlet accepts the batch");
    drop(edits);
    tokio::time::timeout(std::time::Duration::from_secs(20), task)
        .await
        .expect("the check finishes rather than hanging on a missing file")
        .expect("the task does not panic");
    assert!(
        report_receiver.try_recv().is_err(),
        "a nonexistent path produced a report"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_lsp_reports_a_real_diagnostic_from_a_real_language_server() {
    // The end-to-end claim, against `rust-analyzer` itself rather than a fake: a file
    // that does not type-check must produce a report with a position and a message.
    // Skipped rather than failed when the binary is absent, because a machine without
    // `rust-analyzer` cannot prove or disprove anything here.
    if !on_path("rust-analyzer") {
        eprintln!("skipping: rust-analyzer is not on PATH");
        return;
    }
    let root = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"lspprobe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("a manifest");
    std::fs::create_dir(root.path().join("src")).expect("a source directory");
    let file = root.path().join("src").join("lib.rs");
    std::fs::write(
        &file,
        "pub fn answer() -> i32 {\n    let value: String = 42;\n    value\n}\n",
    )
    .expect("a source file");

    let probe = Probe::resolve(&config(r#"{"lsp":true}"#), root.path(), wake()).expect("a probe");
    let (edits, edit_receiver) = mpsc::channel(4);
    let (reports, mut report_receiver) = mpsc::channel(4);
    let task = tokio::spawn(check_edits(Some(probe), edit_receiver, reports));
    edits
        .try_send(vec![String::from("src/lib.rs")])
        .expect("the inlet accepts the batch");
    drop(edits);

    let report = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        report_receiver.recv().await
    })
    .await
    .expect("a report must arrive within the diagnostics timeout")
    .expect("the channel delivered a report");
    task.abort();

    assert_eq!(report.path, "src/lib.rs");
    assert!(
        report.is_checked(),
        "rust-analyzer claims .rs, so the file must not be reported as unchecked: {}",
        report.summary()
    );
    assert!(
        !report.diagnostics.is_empty(),
        "a file that does not type-check reported no problems: {}",
        report.summary()
    );
    let first = &report.diagnostics[0];
    assert_eq!(first.severity, Severity::Error, "{:?}", report.diagnostics);
    assert_eq!(first.line, 2, "the error is on the second line: {first:?}");
    assert!(first.column >= 1, "positions are one-based: {first:?}");
}
