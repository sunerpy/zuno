//! External `$EDITOR` and clipboard, both behind seams.
//!
//! # Why these are traits and not functions
//!
//! Both spawn a process, and one of them takes over the terminal. Neither may run in
//! a test: a real `$EDITOR` would block on a human, and a real clipboard read depends
//! on which of `wl-paste`/`xclip`/`pbpaste` happens to exist on the machine running
//! the suite. So [`ExternalEditor`] and [`Clipboard`] are traits with a real
//! implementation and a recording double, exactly as todo 73 did for the terminal
//! lifecycle.
//!
//! # Opening an editor is a terminal lease, not a suspend
//!
//! Upstream calls `renderer.suspend()` around the child process
//! (`packages/tui/src/editor.ts:32-53`). This crate already has the right mechanism
//! for that — todo 97's [`zuno_engine::terminal_lease::TerminalBroker`], driven by
//! todo 73's [`crate::app::TerminalLeaseOwner`] — so [`EditorRequest`] carries the
//! lease reason and the caller acquires a lease before invoking the editor. That
//! keeps one exclusion policy in the process instead of two that can disagree, which
//! is what would deadlock against a plugin's OAuth prompt.
//!
//! # The clipboard's fallback ladder is data, not control flow
//!
//! `copy_command` (`packages/tui/src/clipboard.ts:75-91`) picks a program by platform
//! and by what is installed. It is ported as a pure function over
//! `(platform, wayland, has)` so the whole ladder is testable without any of those
//! programs being present — which is the only way to test it at all.
//!
//! # OSC 52 is always written, even when a native tool exists
//!
//! `clipboard.ts:120-124` writes the escape sequence *and* runs the native command.
//! That is deliberate: over SSH the native tool copies into the remote machine's
//! clipboard, which is not where the user is looking. The tmux/screen wrapper is part
//! of it — those multiplexers swallow an unwrapped sequence.

use std::io;

#[cfg(test)]
#[path = "external_tests.rs"]
mod tests;

/// Environment variables consulted for the editor, in order
/// (`packages/tui/src/editor.ts:27`).
pub const EDITOR_VARIABLES: [&str; 2] = ["VISUAL", "EDITOR"];

/// A request to edit text in an external editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorRequest {
    /// The text to open with.
    pub value: String,
    /// The working directory for the child, when one is wanted.
    pub cwd: Option<String>,
}

impl EditorRequest {
    /// A request carrying `value`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            cwd: None,
        }
    }

    /// The lease reason a caller should acquire before invoking the editor.
    ///
    /// Named here rather than at the call site so every path that opens an editor
    /// declares the same reason with the same spelling. `LeaseReason::plugin` names
    /// the culprit in a forced-reclaim diagnostic, and for this path the culprit is
    /// the TUI itself, so it says so instead of borrowing a plugin's name.
    #[must_use]
    pub fn lease_reason(&self) -> zuno_engine::terminal_lease::LeaseReason {
        zuno_engine::terminal_lease::LeaseReason::new("tui", "external editor")
    }
}

/// An external editor could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ExternalError {
    /// Neither `VISUAL` nor `EDITOR` is set.
    #[error("no external editor is configured; set $VISUAL or $EDITOR")]
    NoEditor,
    /// The child failed to start, or exited non-zero.
    #[error("the external editor failed: {0}")]
    Failed(String),
    /// The scratch file could not be written or read.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// No clipboard mechanism is available on this host.
    #[error("no clipboard program is available on this host")]
    NoClipboard,
}

/// The `$EDITOR` round trip.
pub trait ExternalEditor: Send + Sync {
    /// Open an editor on `request.value` and return the edited text.
    ///
    /// `None` means the user made no change worth taking — an empty file, which
    /// upstream also treats as "no result" (`editor.ts:48`) rather than as an
    /// instruction to clear the prompt.
    ///
    /// # Errors
    ///
    /// [`ExternalError`] when no editor is configured or the child fails.
    fn edit(&self, request: &EditorRequest) -> Result<Option<String>, ExternalError>;
}

/// The command an editor invocation would run, and the temporary file it would use.
///
/// Split out from the spawn so the argument construction — the part with the quoting
/// and the `.md` suffix that gives the editor its syntax mode — is testable without a
/// child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorInvocation {
    /// The program.
    pub program: String,
    /// Its arguments, ending with the scratch file.
    pub args: Vec<String>,
}

/// Build the invocation for `spec` (the value of `$VISUAL`/`$EDITOR`) over `file`.
///
/// The spec is split on spaces because a user's `EDITOR` is frequently
/// `code --wait` or `nvim -u NONE`; treating the whole string as a program name
/// would fail for those with a confusing "not found".
#[must_use]
pub fn invocation(spec: &str, file: &str) -> Option<EditorInvocation> {
    let mut parts = spec.split_whitespace();
    let program = parts.next()?.to_owned();
    let mut args = parts.map(str::to_owned).collect::<Vec<_>>();
    args.push(file.to_owned());
    Some(EditorInvocation { program, args })
}

/// The editor spec from the environment, `VISUAL` first.
#[must_use]
pub fn editor_spec(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    EDITOR_VARIABLES
        .iter()
        .find_map(|name| lookup(name).filter(|value| !value.trim().is_empty()))
}

/// A recording double.
///
/// Not `#[cfg(test)]`: the CLI's `--no-editor` mode and the ACP host both need an
/// editor that answers without a terminal, and one double is better than three.
#[derive(Debug, Default)]
pub struct ScriptedEditor {
    /// What [`ExternalEditor::edit`] returns.
    pub result: Option<String>,
    /// Whether it fails instead.
    pub fail: bool,
    requests: std::sync::Mutex<Vec<EditorRequest>>,
}

impl ScriptedEditor {
    /// An editor that returns `result`.
    #[must_use]
    pub fn returning(result: impl Into<String>) -> Self {
        Self {
            result: Some(result.into()),
            fail: false,
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// An editor that fails.
    #[must_use]
    pub fn failing() -> Self {
        Self {
            result: None,
            fail: true,
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// What it was asked to edit.
    #[must_use]
    pub fn requests(&self) -> Vec<EditorRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ExternalEditor for ScriptedEditor {
    fn edit(&self, request: &EditorRequest) -> Result<Option<String>, ExternalError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        if self.fail {
            return Err(ExternalError::Failed(String::from("scripted failure")));
        }
        Ok(self.result.clone())
    }
}

/// What the clipboard holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardContent {
    /// The payload: text, or base64 for an image.
    pub data: String,
    /// Its MIME type.
    pub mime: String,
}

impl ClipboardContent {
    /// Plain text.
    #[must_use]
    pub fn text(data: impl Into<String>) -> Self {
        Self {
            data: data.into(),
            mime: String::from("text/plain"),
        }
    }

    /// Whether this is an image, which the prompt turns into an attachment rather
    /// than into typed characters.
    #[must_use]
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// The clipboard round trip.
pub trait Clipboard: Send + Sync {
    /// Read the clipboard.
    ///
    /// # Errors
    ///
    /// [`ExternalError::NoClipboard`] when the host offers no mechanism.
    fn read(&self) -> Result<Option<ClipboardContent>, ExternalError>;

    /// Write text to the clipboard.
    ///
    /// # Errors
    ///
    /// [`ExternalError`] when every mechanism failed.
    fn write(&self, text: &str) -> Result<(), ExternalError>;
}

/// An in-memory clipboard.
#[derive(Debug, Default)]
pub struct MemoryClipboard {
    content: std::sync::Mutex<Option<ClipboardContent>>,
}

impl MemoryClipboard {
    /// A clipboard holding `content`.
    #[must_use]
    pub fn holding(content: ClipboardContent) -> Self {
        Self {
            content: std::sync::Mutex::new(Some(content)),
        }
    }
}

impl Clipboard for MemoryClipboard {
    fn read(&self) -> Result<Option<ClipboardContent>, ExternalError> {
        Ok(self
            .content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    fn write(&self, text: &str) -> Result<(), ExternalError> {
        *self
            .content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(ClipboardContent::text(text));
        Ok(())
    }
}

/// The platforms the clipboard ladder distinguishes.
///
/// Its own enum rather than `cfg!` so every branch is reachable from a test on any
/// host — the whole point of making the ladder a pure function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS.
    Macos,
    /// Linux, and WSL.
    Linux,
    /// Windows.
    Windows,
}

impl Platform {
    /// The platform this binary was built for.
    #[must_use]
    pub const fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// The copy command for a host, or `None` when nothing is available.
///
/// Verbatim from `clipboard.ts:75-91`, including the order: Wayland before X11 on
/// Linux, and `xclip` before `xsel`.
#[must_use]
pub fn copy_command(
    platform: Platform,
    wayland: bool,
    has: impl Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let owned = |parts: &[&str]| Some(parts.iter().map(|part| (*part).to_owned()).collect());
    match platform {
        Platform::Macos if has("osascript") => owned(&["osascript"]),
        Platform::Linux if wayland && has("wl-copy") => owned(&["wl-copy"]),
        Platform::Linux if has("xclip") => owned(&["xclip", "-selection", "clipboard"]),
        Platform::Linux if has("xsel") => owned(&["xsel", "--clipboard", "--input"]),
        Platform::Windows if has("powershell.exe") => owned(&[
            "powershell.exe",
            "-NonInteractive",
            "-NoProfile",
            "-Command",
            "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
        ]),
        _ => None,
    }
}

/// The read command for a host, when an image-capable one exists.
///
/// `clipboard.ts:31-72` reaches for an image first on every platform and falls back
/// to text. Only the Linux arms are expressible as a plain command; macOS needs an
/// AppleScript that writes a file and Windows a PowerShell script, so those return
/// `None` here and the real implementation handles them.
#[must_use]
pub fn image_read_command(
    platform: Platform,
    wayland: bool,
    has: impl Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let owned = |parts: &[&str]| Some(parts.iter().map(|part| (*part).to_owned()).collect());
    match platform {
        Platform::Linux if wayland && has("wl-paste") => owned(&["wl-paste", "-t", "image/png"]),
        Platform::Linux if has("xclip") => {
            owned(&["xclip", "-selection", "clipboard", "-t", "image/png", "-o"])
        }
        _ => None,
    }
}

/// The OSC 52 sequence that copies `text` through the terminal itself.
///
/// `multiplexed` wraps the sequence for tmux or GNU screen, which otherwise consume
/// it instead of forwarding it (`clipboard.ts:24-28`).
#[must_use]
pub fn osc52(text: &str, multiplexed: bool) -> String {
    let encoded = base64(text.as_bytes());
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    if multiplexed {
        format!("\x1bPtmux;\x1b{sequence}\x1b\\")
    } else {
        sequence
    }
}

/// Whether the host is inside tmux or GNU screen (`clipboard.ts:27` — `TMUX`/`STY`).
#[must_use]
pub fn is_multiplexed(lookup: impl Fn(&str) -> Option<String>) -> bool {
    ["TMUX", "STY"]
        .iter()
        .any(|name| lookup(name).is_some_and(|value| !value.is_empty()))
}

/// Standard base64, no line breaks.
///
/// Hand-rolled rather than a dependency: this is the only base64 in the crate, and
/// adding an encoder to the render stack for twenty lines is a poor trade.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}
