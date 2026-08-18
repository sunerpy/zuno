//! The two decisions this module makes that a wrong answer would be dangerous for:
//! whether a probe exists at all, and whether a position is one-based.

use super::*;

/// Why a real language server cannot be exercised, phrased to complete
/// `SKIPPED {test}: rust-analyzer {reason}`.
///
/// Two variants rather than one boolean, for the reason spelled out on
/// [`usable_server`]: "absent" and "present but not a working server" look identical to a
/// `PATH` lookup and have to be told apart before a skip can be trusted.
enum Unusable {
    /// Nothing by that name resolves on `PATH`.
    Missing,
    /// A path resolved, but the program behind it is not a usable server.
    Broken(String),
}

impl std::fmt::Display for Unusable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("was not found on PATH"),
            Self::Broken(reason) => formatter.write_str(reason),
        }
    }
}

/// Resolve `name` on `PATH` and prove the program can at least run.
///
/// This used to be a `PATH` walk that answered `is_file()`, and that is precisely how this
/// test failed on CI while passing locally. `rustup` installs a *proxy* at
/// `~/.cargo/bin/rust-analyzer` whether or not the `rust-analyzer` component was ever
/// added, so the file exists on a runner that has no language server at all; the proxy
/// then exits non-zero with `Unknown binary 'rust-analyzer' in official toolchain`, the
/// manager never brings a server up, and [`report`] flattens that into
/// [`Report::unchecked`] — a red assertion that said nothing about the missing component.
/// `crates/zuno-lsp/tests/live_rust_analyzer.rs` already models the same three states for
/// the same binary, and this is the same probe, so the two cannot drift.
///
/// [`which::which`] rather than a hand-rolled walk because it is what
/// [`zuno_lsp::registry::ServerRegistry`] itself uses to resolve `argv[0]`: the probe now
/// answers with the same mechanism, and the same executable, that production will launch.
/// An `is_file()` walk also accepts a file with no executable bit, which `which` does not.
fn usable_server(name: &str) -> Result<PathBuf, Unusable> {
    let path = which::which(name).map_err(|_| Unusable::Missing)?;
    match std::process::Command::new(&path).arg("--version").output() {
        Err(error) => Err(Unusable::Broken(format!(
            "at {} could not be executed: {error}",
            path.display()
        ))),
        Ok(output) if !output.status.success() => Err(Unusable::Broken(format!(
            "at {} exited with {} for `--version` (a rustup proxy for an uninstalled \
             component does exactly this): {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
        Ok(_) => Ok(path),
    }
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

/// The built-ins, with `rust` pinned to `server` rather than re-resolved.
///
/// `{"lsp":true}` leaves the registry to look `rust-analyzer` up on `PATH` again, so a probe
/// that validated one executable could hand the assertions a different one. Pinning the
/// path [`usable_server`] actually ran makes the thing probed and the thing launched the
/// same file.
fn pinned_rust_config(server: &Path) -> zuno_config::schema::Config {
    serde_json::from_value(serde_json::json!({
        "lsp": { "rust": { "command": [server.to_string_lossy()] } }
    }))
    .expect("valid configuration")
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
    let (wake_sender, edit_receiver) = mpsc::channel(1);
    let pending = zuno_tui::views::lsp::PendingEdits::new(wake_sender);
    let reader = pending.reader();
    let (reports, mut report_receiver) = mpsc::channel(4);
    let task = tokio::spawn(check_edits(None, reader, edit_receiver, reports));
    for _ in 0..4 {
        pending.merge([String::from("src/lib.rs")]);
    }
    drop(pending);
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
    let (wake_sender, edit_receiver) = mpsc::channel(1);
    let pending = zuno_tui::views::lsp::PendingEdits::new(wake_sender);
    let reader = pending.reader();
    let (reports, mut report_receiver) = mpsc::channel(4);
    let task = tokio::spawn(check_edits(Some(probe), reader, edit_receiver, reports));
    pending.merge([String::from("does/not/exist.rs")]);
    drop(pending);
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
    // Skipped rather than failed when no usable server exists, because a machine without
    // one cannot prove or disprove anything here. The skip names the reason and says the
    // claim went unverified: a skip nobody can see is indistinguishable from a pass.
    let server = match usable_server("rust-analyzer") {
        Ok(path) => path,
        Err(reason) => {
            eprintln!(
                "SKIPPED tui_lsp_reports_a_real_diagnostic_from_a_real_language_server: \
                 rust-analyzer {reason}; the end-to-end LSP report path was NOT exercised"
            );
            return;
        }
    };
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

    let probe = Probe::resolve(&pinned_rust_config(&server), root.path(), wake()).expect("a probe");
    let (wake_sender, edit_receiver) = mpsc::channel(1);
    let pending = zuno_tui::views::lsp::PendingEdits::new(wake_sender);
    let reader = pending.reader();
    let (reports, mut report_receiver) = mpsc::channel(4);
    let task = tokio::spawn(check_edits(Some(probe), reader, edit_receiver, reports));
    pending.merge([String::from("src/lib.rs")]);
    drop(pending);

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
        "rust-analyzer claims .rs, so the file must not be reported as unchecked: {} \
         (the server probed and launched was {})",
        report.summary(),
        server.display()
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
