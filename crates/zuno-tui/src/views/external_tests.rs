//! External-surface tests. No subprocess, no clipboard program, no `$EDITOR`.

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
    // culprit is the TUI rather than a plugin.
    let reason = EditorRequest::new("draft").lease_reason();
    assert_eq!(reason.plugin, "tui");
    assert!(reason.purpose.contains("editor"));
    assert!(
        reason.to_string().contains("tui"),
        "the diagnostic does not name the holder: {reason}"
    );
}

#[test]
fn views_external_scripted_editor_records_what_it_was_asked() {
    let editor = ScriptedEditor::returning("edited body");
    let request = EditorRequest::new("original");
    assert_eq!(
        editor.edit(&request).expect("the double succeeds"),
        Some(String::from("edited body"))
    );
    assert_eq!(editor.requests(), vec![request]);
}

#[test]
fn views_external_scripted_editor_can_fail() {
    let editor = ScriptedEditor::failing();
    let error = editor
        .edit(&EditorRequest::new("x"))
        .expect_err("the double fails");
    assert!(matches!(error, ExternalError::Failed(_)));
    assert_eq!(editor.requests().len(), 1);
}

#[test]
fn views_external_editor_returning_nothing_means_no_change() {
    let editor = ScriptedEditor::default();
    assert_eq!(editor.edit(&EditorRequest::new("keep")).expect("ok"), None);
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
