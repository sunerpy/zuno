//! External-surface tests. No clipboard program and no interactive `$EDITOR`.

use super::*;

// ---------------------------------------------------------------------------
// The editor seam
// ---------------------------------------------------------------------------

#[test]
fn views_external_editor_spec_prefers_visual() {
    let lookup = |name: &str| match name {
        "VISUAL" => Some(String::from("code --wait")),
        "EDITOR" => Some(String::from("vi")),
        _ => None,
    };
    assert_eq!(editor_spec(lookup), Some(String::from("code --wait")));
}

#[test]
fn views_external_editor_spec_falls_back_to_editor() {
    let lookup = |name: &str| (name == "EDITOR").then(|| String::from("nvim"));
    assert_eq!(editor_spec(lookup), Some(String::from("nvim")));
}

#[test]
fn views_external_editor_spec_treats_a_blank_value_as_absent() {
    // An exported-but-empty `VISUAL` is common and must not shadow `EDITOR`.
    let lookup = |name: &str| match name {
        "VISUAL" => Some(String::from("   ")),
        "EDITOR" => Some(String::from("vi")),
        _ => None,
    };
    assert_eq!(editor_spec(lookup), Some(String::from("vi")));
    assert_eq!(editor_spec(|_| None), None);
}

#[test]
fn views_external_invocation_splits_a_spec_with_arguments() {
    // `EDITOR="code --wait"` is common; treating the whole string as a program name
    // fails with a confusing "not found".
    let invocation = invocation("code --wait", "/tmp/x.md").expect("a program");
    assert_eq!(invocation.program, "code");
    assert_eq!(invocation.args, vec!["--wait", "/tmp/x.md"]);
}

#[test]
fn views_external_invocation_appends_the_file_last() {
    let invocation = invocation("vi", "/tmp/a.md").expect("a program");
    assert_eq!(invocation.args, vec!["/tmp/a.md"]);
}

#[test]
fn views_external_invocation_rejects_an_empty_spec() {
    assert_eq!(invocation("", "/tmp/x"), None);
    assert_eq!(invocation("   ", "/tmp/x"), None);
}

#[test]
fn views_external_editor_request_names_the_tui_as_the_lease_holder() {
    // The forced-reclaim diagnostic has to name a culprit, and for this path the
    // culprit is the TUI rather than a requester.
    let reason = EditorRequest::new("draft").lease_reason();
    assert_eq!(reason.requester, "tui");
    assert!(reason.purpose.contains("editor"));
    assert!(
        reason.to_string().contains("tui"),
        "the diagnostic does not name the holder: {reason}"
    );
}

#[tokio::test]
async fn views_external_scripted_editor_records_what_it_was_asked() {
    let editor = ScriptedEditor::returning("edited body");
    let request = EditorRequest::new("original");
    assert_eq!(
        editor
            .edit(&request, EditorCancellation::new())
            .await
            .expect("the double succeeds"),
        Some(String::from("edited body"))
    );
    assert_eq!(editor.requests(), vec![request]);
}

#[tokio::test]
async fn views_external_scripted_editor_can_fail() {
    let editor = ScriptedEditor::failing();
    let error = editor
        .edit(&EditorRequest::new("x"), EditorCancellation::new())
        .await
        .expect_err("the double fails");
    assert!(matches!(error, ExternalError::Failed(_)));
    assert_eq!(editor.requests().len(), 1);
}

#[tokio::test]
async fn views_external_editor_returning_nothing_means_no_change() {
    let editor = ScriptedEditor::default();
    assert_eq!(
        editor
            .edit(&EditorRequest::new("keep"), EditorCancellation::new())
            .await
            .expect("ok"),
        None
    );
}

#[cfg(unix)]
#[tokio::test]
async fn views_external_system_editor_returns_the_edited_file() {
    let directory = tempfile::tempdir().expect("editor fixture directory");
    let script = directory.path().join("editor");
    std::fs::write(&script, "#!/bin/sh\nprintf 'edited body' > \"$1\"\n")
        .expect("write editor fixture");
    let editor = SystemEditor::configured(format!("sh {}", script.display()));
    assert_eq!(
        editor
            .edit(
                &EditorRequest::new("original body"),
                EditorCancellation::new(),
            )
            .await
            .expect("editor round trip"),
        Some(String::from("edited body"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn views_external_system_editor_treats_an_empty_file_as_no_change() {
    let directory = tempfile::tempdir().expect("editor fixture directory");
    let script = directory.path().join("editor");
    std::fs::write(&script, "#!/bin/sh\n: > \"$1\"\n").expect("write editor fixture");
    let editor = SystemEditor::configured(format!("sh {}", script.display()));
    assert_eq!(
        editor
            .edit(
                &EditorRequest::new("original body"),
                EditorCancellation::new(),
            )
            .await
            .expect("editor round trip"),
        None
    );
}

// ---------------------------------------------------------------------------
// The clipboard ladder
// ---------------------------------------------------------------------------

fn nothing(_: &str) -> bool {
    false
}

fn everything(_: &str) -> bool {
    true
}

#[test]
fn views_external_copy_command_ladder_matches_the_oracle() {
    assert_eq!(
        copy_command(Platform::Macos, false, everything),
        Some(vec![String::from("osascript")])
    );
    assert_eq!(
        copy_command(Platform::Linux, true, everything),
        Some(vec![String::from("wl-copy")]),
        "Wayland must be preferred when a Wayland display is present"
    );
    assert_eq!(
        copy_command(Platform::Linux, false, everything),
        Some(vec![
            String::from("xclip"),
            String::from("-selection"),
            String::from("clipboard"),
        ]),
        "xclip must come before xsel"
    );
    assert_eq!(
        copy_command(Platform::Linux, false, |name| name == "xsel"),
        Some(vec![
            String::from("xsel"),
            String::from("--clipboard"),
            String::from("--input"),
        ])
    );
    let windows = copy_command(Platform::Windows, false, everything).expect("powershell");
    assert_eq!(windows[0], "powershell.exe");
    assert!(
        windows.last().expect("a script").contains("Set-Clipboard"),
        "the PowerShell fallback lost its script"
    );
}

#[test]
fn views_external_copy_command_is_none_when_nothing_is_installed() {
    for platform in [Platform::Macos, Platform::Linux, Platform::Windows] {
        assert_eq!(
            copy_command(platform, true, nothing),
            None,
            "{platform:?} claimed a program that is not installed"
        );
    }
}

#[test]
fn views_external_copy_command_falls_through_wayland_without_wl_copy() {
    // A Wayland session with only xclip installed still has to copy.
    assert_eq!(
        copy_command(Platform::Linux, true, |name| name == "xclip"),
        Some(vec![
            String::from("xclip"),
            String::from("-selection"),
            String::from("clipboard"),
        ])
    );
}

#[test]
fn views_external_image_read_command_covers_the_two_expressible_arms() {
    assert_eq!(
        image_read_command(Platform::Linux, true, everything),
        Some(vec![
            String::from("wl-paste"),
            String::from("-t"),
            String::from("image/png"),
        ])
    );
    assert!(image_read_command(Platform::Linux, false, everything).is_some());
    // macOS and Windows need a script rather than a command; the real
    // implementation handles those, so this must not claim one.
    assert_eq!(image_read_command(Platform::Macos, false, everything), None);
    assert_eq!(
        image_read_command(Platform::Windows, false, everything),
        None
    );
    assert_eq!(image_read_command(Platform::Linux, true, nothing), None);
}

#[test]
fn views_external_host_platform_is_one_of_the_three() {
    assert!(matches!(
        Platform::host(),
        Platform::Macos | Platform::Linux | Platform::Windows
    ));
}

// ---------------------------------------------------------------------------
// OSC 52
// ---------------------------------------------------------------------------

#[test]
fn views_external_osc52_carries_base64_and_the_right_terminators() {
    let sequence = osc52("hi", false);
    assert_eq!(sequence, "\u{1b}]52;c;aGk=\u{7}");
}

#[test]
fn views_external_osc52_wraps_for_a_multiplexer() {
    // tmux and screen swallow an unwrapped sequence, so the copy silently fails.
    let wrapped = osc52("hi", true);
    assert!(wrapped.starts_with("\u{1b}Ptmux;\u{1b}"));
    assert!(wrapped.ends_with("\u{1b}\\"));
    assert!(wrapped.contains("aGk="));
}

#[test]
fn views_external_multiplexer_detection_reads_tmux_and_sty() {
    assert!(is_multiplexed(
        |name| (name == "TMUX").then(|| String::from("/tmp/tmux-1000/default,1,0"))
    ));
    assert!(is_multiplexed(
        |name| (name == "STY").then(|| String::from("1234.pts-0.host"))
    ));
    assert!(!is_multiplexed(|_| None));
    assert!(
        !is_multiplexed(|name| (name == "TMUX").then(String::new)),
        "an empty TMUX was treated as being inside tmux"
    );
}

#[test]
fn views_external_base64_matches_the_reference_vectors() {
    // RFC 4648 §10.
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foob"), "Zm9vYg==");
    assert_eq!(base64(b"fooba"), "Zm9vYmE=");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");
}

#[test]
fn views_external_base64_handles_high_bytes_and_multibyte_text() {
    assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    assert_eq!(base64("日".as_bytes()), "5pel");
}

// ---------------------------------------------------------------------------
// The clipboard double
// ---------------------------------------------------------------------------

#[test]
fn views_external_memory_clipboard_round_trips_text() {
    let clipboard = MemoryClipboard::default();
    assert_eq!(clipboard.read().expect("readable"), None);
    clipboard.write("copied").expect("writable");
    assert_eq!(
        clipboard.read().expect("readable"),
        Some(ClipboardContent::text("copied"))
    );
}

#[test]
fn views_external_clipboard_image_is_distinguished_from_text() {
    // The prompt turns an image into an attachment, not into typed characters.
    let image = ClipboardContent {
        data: String::from("aGk="),
        mime: String::from("image/png"),
    };
    assert!(image.is_image());
    assert!(!ClipboardContent::text("hi").is_image());
    let clipboard = MemoryClipboard::holding(image.clone());
    assert_eq!(clipboard.read().expect("readable"), Some(image));
}

#[test]
fn views_external_error_messages_name_what_is_missing() {
    assert!(ExternalError::NoEditor.to_string().contains("EDITOR"));
    assert!(ExternalError::NoClipboard.to_string().contains("clipboard"));
}

// ---------------------------------------------------------------------------
// The real clipboard: OSC 52 first, native command only as fallback
// ---------------------------------------------------------------------------

/// A clipboard with both mechanisms available, wired to one ordered log.
fn both(log: &Arc<CopyLog>) -> SystemClipboard {
    SystemClipboard::new(
        Some(Box::new(RecordingSink::new(Arc::clone(log)))),
        false,
        copy_command(Platform::Linux, false, everything),
        Box::new(ScriptedRunner::new(Arc::clone(log))),
    )
}

#[test]
fn views_external_system_clipboard_osc52_success_skips_the_native_program() {
    // OSC 52 is the mechanism that reaches the user's terminal over SSH and tmux. Once
    // its flushed write succeeds, spawning a second helper adds only latency and a
    // process hazard; the native ladder is reserved for when that write fails.
    let log = CopyLog::shared();
    both(&log).write("hi").expect("either mechanism suffices");

    let entries = log.entries();
    assert_eq!(
        entries,
        vec![format!("osc52:{}", "\u{1b}]52;c;aGk=\u{7}")],
        "a successful terminal copy still spawned the native helper"
    );
}

#[test]
fn views_external_system_clipboard_emits_the_exact_escape_sequence() {
    let log = CopyLog::shared();
    both(&log).write("hi").expect("writable");
    assert_eq!(
        log.entries()[0],
        format!("osc52:{}", "\u{1b}]52;c;aGk=\u{7}"),
        "the sequence written to the terminal is not the one `osc52` builds"
    );
}

#[test]
fn views_external_system_clipboard_wraps_the_sequence_inside_a_multiplexer() {
    // An unwrapped sequence is swallowed by tmux, so the copy fails with nothing said.
    let log = CopyLog::shared();
    SystemClipboard::new(
        Some(Box::new(RecordingSink::new(Arc::clone(&log)))),
        true,
        None,
        Box::new(ScriptedRunner::new(Arc::clone(&log))),
    )
    .write("hi")
    .expect("writable");

    assert_eq!(
        log.entries(),
        vec![format!(
            "osc52:{}",
            "\u{1b}Ptmux;\u{1b}\u{1b}]52;c;aGk=\u{7}\u{1b}\\"
        )],
        "the tmux wrapper is missing or malformed"
    );
}

#[test]
fn views_external_system_clipboard_survives_a_failing_terminal_via_the_program() {
    let log = CopyLog::shared();
    SystemClipboard::new(
        Some(Box::new(RecordingSink::failing(Arc::clone(&log)))),
        false,
        copy_command(Platform::Linux, false, everything),
        Box::new(ScriptedRunner::new(Arc::clone(&log))),
    )
    .write("x")
    .expect("the program delivered it");
    assert_eq!(
        log.entries().len(),
        2,
        "a failed OSC 52 stopped the ladder from being tried"
    );
}

#[test]
fn views_external_system_clipboard_native_only_host_still_copies() {
    let log = CopyLog::shared();
    SystemClipboard::new(
        None,
        false,
        copy_command(Platform::Linux, false, everything),
        Box::new(ScriptedRunner::new(Arc::clone(&log))),
    )
    .write("native payload")
    .expect("the native fallback delivered it");

    assert_eq!(
        log.entries(),
        vec![String::from(
            "run:xclip -selection clipboard:native payload"
        )],
        "the native-only host did not feed the payload to its helper"
    );
}

#[test]
fn views_external_system_clipboard_survives_a_missing_program_via_the_terminal() {
    let log = CopyLog::shared();
    SystemClipboard::new(
        Some(Box::new(RecordingSink::new(Arc::clone(&log)))),
        false,
        None,
        Box::new(ScriptedRunner::failing(Arc::clone(&log))),
    )
    .write("x")
    .expect("OSC 52 delivered it");
}

#[test]
fn views_external_system_clipboard_reports_both_failures_rather_than_the_first() {
    // The two mechanisms fail for unrelated reasons, and a report naming one sends the
    // user looking in the wrong place.
    let log = CopyLog::shared();
    let error = SystemClipboard::new(
        Some(Box::new(RecordingSink::failing(Arc::clone(&log)))),
        false,
        copy_command(Platform::Linux, false, everything),
        Box::new(ScriptedRunner::failing(Arc::clone(&log))),
    )
    .write("x")
    .expect_err("both mechanisms failed");

    let message = error.to_string();
    assert!(
        message.contains("OSC 52"),
        "the terminal failure was swallowed: {message}"
    );
    assert!(
        message.contains("xclip"),
        "the program failure was swallowed: {message}"
    );
}

#[test]
fn views_external_system_clipboard_with_no_mechanism_says_so() {
    let clipboard = SystemClipboard::new(
        None,
        false,
        None,
        Box::new(ScriptedRunner::new(CopyLog::shared())),
    );
    assert!(!clipboard.is_available());
    assert!(matches!(
        clipboard.write("x").expect_err("nothing to write with"),
        ExternalError::NoClipboard
    ));
}

#[test]
fn views_external_system_clipboard_read_is_an_error_and_not_an_empty_clipboard() {
    // `Ok(None)` here would read to whoever wires `EditorSignal::Paste` as "the
    // clipboard is empty", which is the same silent no-op the write side had.
    let clipboard = SystemClipboard::new(
        None,
        false,
        None,
        Box::new(ScriptedRunner::new(CopyLog::shared())),
    );
    let error = clipboard.read().expect_err("reading is not wired");
    assert!(
        error.to_string().contains("not wired"),
        "the unimplemented read does not say so: {error}"
    );
}

#[test]
fn views_external_environment_resolution_picks_the_wayland_arm_and_the_wrapper() {
    // The boundary: platform, `PATH`, `WAYLAND_DISPLAY` and `TMUX` all read in one
    // place, so no branch below has to consult `cfg!` to decide what it does.
    let log = CopyLog::shared();
    let clipboard = SystemClipboard::for_environment(
        Platform::Linux,
        |name| match name {
            // A directory that cannot exist, so the probe finds no program and the
            // outcome does not depend on what the machine running the suite installed.
            "PATH" => Some(String::from("/nonexistent-bin")),
            "WAYLAND_DISPLAY" => Some(String::from("wayland-0")),
            "TMUX" => Some(String::from("/tmp/tmux-1000/default,1,0")),
            _ => None,
        },
        Some(Box::new(RecordingSink::new(Arc::clone(&log)))),
        Box::new(ScriptedRunner::new(Arc::clone(&log))),
    );

    // No program is installed under that `PATH`, so only the terminal remains — and it
    // must still be wrapped, because `TMUX` was set.
    clipboard.write("hi").expect("the terminal took it");
    assert_eq!(
        log.entries(),
        vec![format!(
            "osc52:{}",
            "\u{1b}Ptmux;\u{1b}\u{1b}]52;c;aGk=\u{7}\u{1b}\\"
        )],
        "the environment's TMUX did not reach the sequence"
    );
}

#[test]
fn views_external_a_redirected_stdout_has_no_osc52_destination() {
    // Not a test accommodation: an escape sequence written to a pipe corrupts that
    // stream instead of reaching a terminal. It is also what keeps the suite quiet —
    // `cargo test` captures stdout, so this is the branch every screen built without an
    // injected clipboard takes.
    assert!(
        terminal_destination(false).is_none(),
        "a process with no terminal was given somewhere to write escape sequences"
    );
    assert!(
        terminal_destination(true).is_some(),
        "a real terminal was refused the one mechanism that works over SSH"
    );
}

#[test]
fn views_external_path_candidates_join_every_entry_with_the_platform_separator() {
    assert_eq!(
        path_candidates("xclip", "/usr/bin:/bin", Platform::Linux),
        vec![
            std::path::PathBuf::from("/usr/bin/xclip"),
            std::path::PathBuf::from("/bin/xclip"),
        ]
    );
    assert_eq!(
        path_candidates("powershell.exe", r"C:\Windows;C:\Tools", Platform::Windows),
        vec![
            std::path::PathBuf::from(r"C:\Windows").join("powershell.exe"),
            std::path::PathBuf::from(r"C:\Tools").join("powershell.exe"),
        ],
        "a Windows PATH was split on the POSIX separator"
    );
    assert!(
        path_candidates("xclip", "", Platform::Linux).is_empty(),
        "an empty PATH produced a bare relative candidate"
    );
    assert!(
        path_candidates("xclip", "::", Platform::Linux).is_empty(),
        "empty PATH entries became candidates in the current directory"
    );
}

#[test]
fn views_external_child_process_runner_rejects_an_empty_command() {
    // The one arm of the real runner reachable without spawning anything.
    let error = ChildProcessRunner
        .run(&[], "x")
        .expect_err("there is no program to run");
    assert!(
        error.to_string().contains("empty"),
        "the empty command was not named: {error}"
    );
}

#[derive(Debug, Default)]
struct HangingChildState {
    killed: std::sync::atomic::AtomicBool,
    reaped: std::sync::atomic::AtomicBool,
}

struct BlockingStdin {
    state: Arc<HangingChildState>,
}

impl std::io::Write for BlockingStdin {
    fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
        while !self.state.killed.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the fake child was killed",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct HangingChild {
    state: Arc<HangingChildState>,
    stdin: Option<Box<dyn std::io::Write + Send>>,
}

impl HangingChild {
    fn non_consuming() -> (Self, Arc<HangingChildState>) {
        let state = Arc::new(HangingChildState::default());
        (
            Self {
                stdin: Some(Box::new(BlockingStdin {
                    state: Arc::clone(&state),
                })),
                state: Arc::clone(&state),
            },
            state,
        )
    }

    fn accepting_input() -> (Self, Arc<HangingChildState>) {
        let state = Arc::new(HangingChildState::default());
        (
            Self {
                stdin: Some(Box::new(std::io::sink())),
                state: Arc::clone(&state),
            },
            state,
        )
    }
}

impl ClipboardChild for HangingChild {
    fn take_stdin(&mut self) -> Option<Box<dyn std::io::Write + Send>> {
        self.stdin.take()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        Ok(None)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.state
            .killed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        while !self.state.killed.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.state
            .reaped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Err(std::io::Error::other(
            "fake reaped child has no exit status",
        ))
    }
}

#[test]
fn views_external_clipboard_child_timeout_bounds_a_non_consuming_child() {
    let (mut child, _state) = HangingChild::non_consuming();
    let bound = std::time::Duration::from_millis(20);
    let started = std::time::Instant::now();

    let error = run_clipboard_child("fake-clipboard", &mut child, "payload", bound)
        .expect_err("the hanging child must time out");

    assert!(
        started.elapsed() < std::time::Duration::from_millis(250),
        "the clipboard caller remained blocked after its deadline"
    );
    assert!(
        error.to_string().contains("timed out"),
        "the deadline failure was not surfaced: {error}"
    );
}

#[test]
fn views_external_clipboard_child_timeout_kills_and_reaps_the_child() {
    let (mut child, state) = HangingChild::accepting_input();

    run_clipboard_child(
        "fake-clipboard",
        &mut child,
        "payload",
        std::time::Duration::from_millis(20),
    )
    .expect_err("the hanging child must time out");

    assert!(
        state.killed.load(std::sync::atomic::Ordering::SeqCst),
        "the timed-out clipboard child was left running"
    );
    assert!(
        state.reaped.load(std::sync::atomic::Ordering::SeqCst),
        "the killed clipboard child was not waited on"
    );
}

#[derive(Clone, Copy)]
enum HostileClipboardMode {
    KillFails,
    DescendantHoldsPipe,
    TryWaitFails,
    WaitNeverReturns,
}

#[derive(Debug, Default)]
struct HostileClipboardState {
    kill_called: std::sync::atomic::AtomicBool,
    try_wait_called: std::sync::atomic::AtomicBool,
    wait_called: std::sync::atomic::AtomicBool,
}

struct NeverReturningStdin;

impl std::io::Write for NeverReturningStdin {
    fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct HostileClipboardChild {
    mode: HostileClipboardMode,
    state: Arc<HostileClipboardState>,
    stdin: Option<Box<dyn std::io::Write + Send>>,
}

impl HostileClipboardChild {
    fn new(mode: HostileClipboardMode, state: Arc<HostileClipboardState>) -> Self {
        let stdin: Box<dyn std::io::Write + Send> = match mode {
            HostileClipboardMode::KillFails | HostileClipboardMode::DescendantHoldsPipe => {
                Box::new(NeverReturningStdin)
            }
            HostileClipboardMode::TryWaitFails | HostileClipboardMode::WaitNeverReturns => {
                Box::new(std::io::sink())
            }
        };
        Self {
            mode,
            state,
            stdin: Some(stdin),
        }
    }
}

impl ClipboardChild for HostileClipboardChild {
    fn take_stdin(&mut self) -> Option<Box<dyn std::io::Write + Send>> {
        self.stdin.take()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.state
            .try_wait_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if matches!(self.mode, HostileClipboardMode::TryWaitFails) {
            return Err(std::io::Error::other("hostile try_wait failure"));
        }
        Ok(None)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.state
            .kill_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if matches!(self.mode, HostileClipboardMode::KillFails) {
            return Err(std::io::Error::other("hostile kill failure"));
        }
        Ok(())
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.state
            .wait_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

struct HostileClipboardRunner {
    mode: HostileClipboardMode,
    state: Arc<HostileClipboardState>,
}

impl CommandRunner for HostileClipboardRunner {
    fn run(&self, argv: &[String], input: &str) -> Result<(), ExternalError> {
        let mut child = HostileClipboardChild::new(self.mode, Arc::clone(&self.state));
        run_clipboard_child(
            argv.first().map_or("hostile-clipboard", String::as_str),
            &mut child,
            input,
            std::time::Duration::from_millis(20),
        )
    }
}

fn assert_hostile_native_copy_is_bounded(mode: HostileClipboardMode) -> Arc<HostileClipboardState> {
    let state = Arc::new(HostileClipboardState::default());
    let clipboard = SystemClipboard::new(
        None,
        false,
        Some(vec![String::from("hostile-clipboard")]),
        Box::new(HostileClipboardRunner {
            mode,
            state: Arc::clone(&state),
        }),
    );
    let (finished, outcome) = std::sync::mpsc::sync_channel(1);
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        let result = clipboard.write("payload");
        let _reported = finished.send(result);
    });

    let error = outcome
        .recv_timeout(std::time::Duration::from_millis(250))
        .expect("the component-facing clipboard call exceeded its hard bound")
        .expect_err("a hostile clipboard helper cannot report success");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(250),
        "the native fallback held the component path past its hard bound: {error}"
    );
    state
}

#[test]
fn views_external_native_clipboard_bound_survives_kill_failure() {
    let state = assert_hostile_native_copy_is_bounded(HostileClipboardMode::KillFails);
    assert!(
        state.kill_called.load(std::sync::atomic::Ordering::SeqCst),
        "the worker never attempted to kill the hostile helper"
    );
}

#[test]
fn views_external_native_clipboard_bound_survives_a_descendant_holding_stdin() {
    let state = assert_hostile_native_copy_is_bounded(HostileClipboardMode::DescendantHoldsPipe);
    assert!(
        state.kill_called.load(std::sync::atomic::Ordering::SeqCst),
        "the direct child was not killed before its inherited pipe stranded the writer"
    );
}

#[test]
fn views_external_native_clipboard_bound_survives_try_wait_failure() {
    let state = assert_hostile_native_copy_is_bounded(HostileClipboardMode::TryWaitFails);
    assert!(
        state
            .try_wait_called
            .load(std::sync::atomic::Ordering::SeqCst),
        "the hostile try_wait branch was not reached"
    );
}

#[test]
fn views_external_native_clipboard_bound_survives_a_nonreturning_wait() {
    let state = assert_hostile_native_copy_is_bounded(HostileClipboardMode::WaitNeverReturns);
    assert!(
        state.wait_called.load(std::sync::atomic::Ordering::SeqCst),
        "the hostile wait branch was not reached"
    );
}
