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

/// A wake sink **and its receiver**, which the caller must keep alive.
///
/// This used to return the sender alone and drop the receiver, on the reasoning that a
/// nudge nobody listens for costs nothing. That stopped being true when the nudge became
/// awaited: a closed wake channel now means "no event loop remains", which ends the
/// checker on purpose. A test that dropped the receiver would therefore be asserting
/// against the give-up path while looking like it asserted against the healthy one — the
/// nudge would never be attempted, and the very loss these tests exist to catch would be
/// invisible to them.
///
/// Returning the pair makes that unrepresentable: the receiver has to be bound to be
/// ignored, and `_wake` in a caller is a visible decision rather than an accident.
fn wake() -> (
    mpsc::Sender<zuno_tui::app::TerminalEvent>,
    mpsc::Receiver<zuno_tui::app::TerminalEvent>,
) {
    zuno_tui::app::terminal_event_channel()
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
    assert!(Probe::resolve(&config("{}"), root.path(), wake().0).is_none());
    assert!(Probe::resolve(&config(r#"{"lsp":false}"#), root.path(), wake().0).is_none());
}

#[test]
fn tui_lsp_probe_exists_once_a_server_is_enabled() {
    let root = tempfile::tempdir().expect("a temporary directory");
    assert!(
        Probe::resolve(&config(r#"{"lsp":true}"#), root.path(), wake().0).is_some(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_lsp_no_report_is_lost_when_a_turn_writes_more_files_than_the_channel_holds() {
    // The dropped-report defect. `report` used `try_send` and discarded the outcome, so a
    // turn whose edit set is larger than the channel silently lost the overflow: with only
    // a Rust server configured, seventeen `.txt` writes take the `has_server == false`
    // branch seventeen times, the seventeenth `try_send` fails against a sixteen-slot
    // channel, and the screen shows sixteen results as if the check were complete.
    //
    // That is not a display nicety. This module's own contract is that a file no server
    // claims must be *reported* as unchecked, because silence on it reads as a clean bill
    // of health — a false claim about correctness. A lost report makes the same false
    // claim, and makes it invisibly.
    //
    // Driven through `check_edits` — the production task, with a real `Probe` and the real
    // `PendingEdits` inlet — rather than by calling `report` or the sender directly. The
    // first round's fix was tested at the sender and that is exactly how this survived.
    let root = tempfile::tempdir().expect("a temporary directory");
    let written = REPORT_CHANNEL_CAPACITY + 1;
    let mut names = Vec::new();
    for index in 0..written {
        let name = format!("note{index}.txt");
        std::fs::write(root.path().join(&name), "text\n").expect("a written file");
        names.push(name);
    }

    // A Rust-only configuration, so every `.txt` is legitimately unclaimed and no language
    // server has to start for this to be a faithful run of the branch.
    //
    // The nudge receiver is *kept*, because when the screen drains is the whole defect. The
    // render loop drains reports only when something wakes it, so a test that drained
    // concurrently would keep the channel empty and the sixteenth slot would never be
    // reached — it would pass against the broken code. Nothing is read here until the
    // checker says there is something to read.
    let (nudges, mut woken) = zuno_tui::app::terminal_event_channel();
    let probe = Probe::resolve(&config(r#"{"lsp":true}"#), root.path(), nudges).expect("a probe");
    let (wake_sender, edit_receiver) = mpsc::channel(1);
    let pending = zuno_tui::views::lsp::PendingEdits::new(wake_sender);
    let reader = pending.reader();
    let (reports, mut report_receiver) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
    let _task = tokio::spawn(check_edits(Some(probe), reader, edit_receiver, reports));
    pending.merge(names.clone());
    // Closes the signal channel, so the task ends after this batch and the drain below
    // terminates on its own rather than on a timeout.
    drop(pending);

    let mut received = Vec::new();
    let drained = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        // Wait to be told, exactly as the loop does, and only then read.
        let _woken = woken.recv().await;
        while let Some(report) = report_receiver.recv().await {
            received.push(report);
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "the checker never finished; it delivered {} of {written} reports",
        received.len()
    );

    assert_eq!(
        received.len(),
        written,
        "{written} files were written and {} reports reached the screen, so {} unchecked \
         file(s) were silently dropped against a {REPORT_CHANNEL_CAPACITY}-slot channel",
        received.len(),
        written - received.len()
    );
    let mut reported: Vec<String> = received.iter().map(|report| report.path.clone()).collect();
    reported.sort();
    names.sort();
    assert_eq!(
        reported, names,
        "the reports name different files than were written"
    );
    assert!(
        received.iter().all(|report| !report.is_checked()),
        "a `.txt` file cannot have been checked by a Rust server"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_lsp_a_truncated_check_says_so_on_screen_and_not_only_in_the_log() {
    // A codemod that writes past `PENDING_EDIT_LIMIT` gets its overflow refused, and the
    // only record of that used to be a `tracing::warn!`. The default TUI displays no log,
    // so the screen showed a check over a subset and looked complete — the same "silence
    // reads as a clean bill of health" this module refuses for a file no server claims.
    //
    // Driven through `check_edits` so the notice has to survive the real path: the set's
    // own bound refuses the paths, and the checker has to turn that count into something
    // the screen is fed.
    let root = tempfile::tempdir().expect("a temporary directory");
    let limit = zuno_tui::views::lsp::PENDING_EDIT_LIMIT;
    let overflow = 3;
    // One real file, so the batch is not empty and the notice travels beside a report
    // rather than instead of one.
    std::fs::write(root.path().join("real.txt"), "text\n").expect("a written file");

    let (nudges, mut woken) = zuno_tui::app::terminal_event_channel();
    let probe = Probe::resolve(&config(r#"{"lsp":true}"#), root.path(), nudges).expect("a probe");
    let (wake_sender, edit_receiver) = mpsc::channel(1);
    let pending = zuno_tui::views::lsp::PendingEdits::new(wake_sender);
    let reader = pending.reader();
    let (reports, mut report_receiver) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
    let _task = tokio::spawn(check_edits(Some(probe), reader, edit_receiver, reports));

    pending.merge(std::iter::once(String::from("real.txt")).chain(
        // Names, not files: these are refused before anything looks at the disk.
        (0..limit + overflow - 1).map(|index| format!("ghost{index}.rs")),
    ));
    drop(pending);

    let mut received = Vec::new();
    let drained = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let _woken = woken.recv().await;
        while let Some(report) = report_receiver.recv().await {
            received.push(report);
        }
    })
    .await;
    assert!(drained.is_ok(), "the checker never finished");

    let notice = received
        .iter()
        .find(|report| report.dropped() > 0)
        .unwrap_or_else(|| {
            panic!(
                "{overflow} paths were refused for space and no report said so, so the \
                 screen shows a check over a subset as if it were complete. Reports: {:?}",
                received
                    .iter()
                    .map(|report| report.summary())
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(notice.dropped(), overflow);
    let summary = notice.summary();
    assert!(
        summary.contains("unchecked") && summary.contains(&overflow.to_string()),
        "the notice must name how many files have no result: [{summary}]"
    );
    assert!(
        received.iter().any(|report| report.path == "real.txt"),
        "the notice replaced the report it was meant to qualify"
    );
}

#[tokio::test]
async fn tui_lsp_a_nudge_waits_for_room_instead_of_being_dropped_when_the_terminal_queue_is_full() {
    // The third place this lossiness moved to. Awaiting the *report* only guarantees the
    // data is in its channel; the screen drains reports when it handles an event, and the
    // nudge is what makes an event happen. `try_send` for the nudge was justified by "a full
    // queue already holds a look again the loop has not read", and that is false: the
    // sixty-four slots are shared with every keystroke and resize, so a burst of typing
    // fills them with events that say nothing about diagnostics. The report was queued and
    // the only thing that would have drawn it was thrown away.
    //
    // Deterministic rather than timed: the queue is filled to exactly capacity with ordinary
    // printable keys, so the nudge provably has nowhere to go until a slot is freed here.
    let (wake_tx, mut wake_rx) = wake();
    for index in 0..zuno_tui::app::TERMINAL_EVENT_CHANNEL_CAPACITY {
        let key = char::from(b'a' + u8::try_from(index % 26).expect("index fits a byte"));
        wake_tx
            .try_send(zuno_tui::app::TerminalEvent::Input(
                zuno_tui::crossterm::event::Event::Key(zuno_tui::crossterm::event::KeyEvent::from(
                    zuno_tui::crossterm::event::KeyCode::Char(key),
                )),
            ))
            .expect("the queue accepts events up to its capacity");
    }
    assert_eq!(
        wake_tx.capacity(),
        0,
        "the queue must be full for this test to mean anything"
    );

    let (reports, mut report_receiver) = mpsc::channel(REPORT_CHANNEL_CAPACITY);
    let delivering = tokio::spawn({
        let wake_tx = wake_tx.clone();
        async move {
            deliver(
                zuno_tui::views::lsp::Report::unchecked("src/lib.rs"),
                &reports,
                &wake_tx,
            )
            .await
        }
    });

    // The report lands immediately — its channel has room — and then the task must be
    // parked on the nudge. Asserted by making no room and confirming it has not finished.
    let report = tokio::time::timeout(std::time::Duration::from_secs(5), report_receiver.recv())
        .await
        .expect("the report is queued without waiting")
        .expect("a report arrives");
    assert_eq!(report.path, "src/lib.rs");
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        !delivering.is_finished(),
        "the nudge did not wait for room: it was dropped, and with it the only thing that \
         would have made the screen read the report already sitting in its channel"
    );

    // One slot, which is all a coalescing wake needs.
    let displaced = wake_rx.recv().await.expect("a queued key");
    assert!(
        matches!(displaced, zuno_tui::app::TerminalEvent::Input(_)),
        "the first event out must be the oldest key, not the nudge"
    );

    let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), delivering)
        .await
        .expect("the nudge is sent once a slot frees")
        .expect("the delivering task does not panic");
    assert!(delivered, "delivery reported failure with both ends open");

    let mut sent = Vec::new();
    while let Ok(event) = wake_rx.try_recv() {
        sent.push(event);
    }
    assert!(
        sent.iter()
            .any(|event| matches!(event, zuno_tui::app::TerminalEvent::Wake)),
        "no wake ever reached the queue, so the report would never be drawn"
    );
    assert_eq!(
        sent.iter()
            .filter(|event| matches!(event, zuno_tui::app::TerminalEvent::Wake))
            .count(),
        1,
        "one report owes one nudge"
    );
}

#[tokio::test]
async fn tui_lsp_delivery_gives_up_when_the_event_loop_is_gone() {
    // The one case where dropping a nudge is right, and it must be reported rather than
    // ignored: with no loop left there is nothing to draw anything, so the caller should
    // stop querying language servers instead of working for a screen that no longer exists.
    let (wake_tx, wake_rx) = wake();
    drop(wake_rx);
    let (reports, _report_receiver) = mpsc::channel(REPORT_CHANNEL_CAPACITY);

    assert!(
        !deliver(
            zuno_tui::views::lsp::Report::unchecked("src/lib.rs"),
            &reports,
            &wake_tx,
        )
        .await,
        "a closed event loop must end delivery, not be silently ignored"
    );
}

#[tokio::test]
async fn tui_lsp_check_edits_skips_a_path_that_is_not_a_file() {
    // A model may report a write to a path it then removed, or to a directory. Asking a
    // language server about it would fail per file and produce a row per failure.
    let root = tempfile::tempdir().expect("a temporary directory");
    let (wake_tx, _wake_rx) = wake();
    let probe = Probe::resolve(&config(r#"{"lsp":true}"#), root.path(), wake_tx).expect("a probe");
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

    // The receiver is held for the whole test: a dropped one now ends the checker, so the
    // report below would be racing a shutdown instead of arriving normally.
    let (wake_tx, _wake_rx) = wake();
    let probe =
        Probe::resolve(&pinned_rust_config(&server), root.path(), wake_tx).expect("a probe");
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
