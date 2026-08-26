//! The input editor: a multi-line buffer, its history, and its undo stack.
//!
//! # Everything arrives as an action
//!
//! Sixty-odd `input_*` rows of the binding table
//! (`packages/tui/src/config/keybind.ts`) exist so that this editor never names a
//! key. [`InputEditor::handle_action`] is a match on
//! [`crate::keybind::Definition::name`] and nothing else; a printable character
//! reaches it through [`InputEditor::insert_char`], which the dispatcher calls for
//! keys no binding claimed.
//!
//! # Coordinates are (line, char), not byte offsets
//!
//! A cursor is `(line, column)` where the column counts **characters**. Byte
//! offsets would be faster and would also let a left-arrow land inside a multi-byte
//! grapheme, which panics on the next slice. Every mutation converts to a byte index
//! at the point of use.
//!
//! # Selection is an anchor, not a range
//!
//! `input_select_*` moves the cursor and keeps the anchor; an ordinary move drops
//! the anchor. That is one rule for eleven bindings, and it means a selection can
//! never be left inconsistent with the cursor — the invariant a stored range has to
//! be maintained against.
//!
//! Pointer selection uses the same invariant. The editor remembers the rectangle and
//! scroll offset from its most recent render, maps a left-button press into a character
//! position, and then clamps drags and release events to that visible rectangle. The
//! resulting anchor and cursor feed the same [`InputEditor::selection`] and
//! `messages_copy` path as a keyboard selection; the clipboard remains a host concern.
//!
//! # Undo snapshots whole states
//!
//! The stack holds `(lines, cursor)` snapshots rather than an operation log. A log
//! is smaller and is also where undo bugs live; the buffer here is a prompt, not a
//! file, so the memory argument does not apply.
//!
//! # History is separate from undo
//!
//! `history_previous`/`history_next` walk **submitted** prompts, and stepping into
//! history stashes whatever was being typed so returning past the newest entry
//! restores it. Losing a half-written prompt to an accidental up-arrow is the
//! failure this stash exists to prevent.
//!
//! # History survives the process, and the editor never touches the filesystem
//!
//! [`PromptHistory`] reads a file a *caller* names, and [`InputEditor`] forwards each
//! recorded prompt to a sink a caller supplies. Neither resolves a path: this crate
//! depends on `zuno-engine`, `zuno-llm` and `zuno-permission` and on nothing that
//! knows where a home directory is, the same division that makes
//! [`crate::config::ResolvedTuiConfig::discover`] take a path list. The host — see
//! `crates/zuno-cli/src/cmd/tui.rs` — resolves the path, loads before the first
//! frame, and drains the sink on a task off the render loop.
//!
//! An unreadable or corrupt file degrades to whatever lines *did* parse plus a
//! notice, and never to a startup failure. That is the whole reason the format is
//! one JSON object per line rather than one JSON document: a process killed
//! mid-append truncates only its final line, and every earlier prompt still loads.
//!
//! # A large paste is summarised in the prompt, not in the submission
//!
//! [`InputEditor::insert_paste`] replaces a paste over
//! [`PASTE_SUMMARY_LINES`]/[`PASTE_SUMMARY_CHARS`] with a one-line placeholder and
//! keeps the text beside it; [`InputEditor::submission_text`] puts it back. What the
//! model receives is therefore always the full paste — the summary is an affordance
//! for the prompt band, which [`crate::views::session`] caps at a third of the
//! screen, and never a truncation.

use crate::app::{AppEvent, Component, EventResult, TerminalEvent};
use crate::keybind::Definition;
use crate::views::{ViewContext, fill, padded};
use crossterm::event::{Event as CrosstermEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::sync::mpsc;

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;

/// The most recent prompts kept for `history_previous`.
///
/// A cap rather than unbounded growth: history lives for the process, and an
/// unbounded list of prompts is a slow memory leak in a long session. Enforced on
/// load as well as on record, because a file grown by an older build — or edited by
/// hand — would otherwise reintroduce the growth the cap exists to bound.
pub const HISTORY_LIMIT: usize = 100;

/// The file [`PromptHistory`] is written to, named here so the host and the loader
/// cannot disagree about it.
pub const PROMPT_HISTORY_FILE: &str = "prompt-history.jsonl";

/// The largest prompt persisted, in bytes.
///
/// Not a limit on what can be *submitted*: an over-long prompt still runs and still
/// walks back in this process's history, it is simply not written down. The bound
/// exists because a paste is now one keystroke, so `HISTORY_LIMIT` alone no longer
/// bounds the file — a hundred megabyte-sized pastes would make the startup read
/// this feature depends on the slowest thing the TUI does. 64 KiB is far above any
/// typed prompt, which keeps the worst case at a few megabytes.
pub const HISTORY_ENTRY_LIMIT: usize = 64 * 1024;

/// Lines and characters above which a paste is summarised rather than shown.
///
/// Tied to the prompt band, which is `clamp(lines + 1, 2, viewport / 3)` rows — see
/// [`crate::views::session`]. A paste that fits in the band needs no summary: it is
/// visible, editable, and that is what a user pasting a short snippet wants. Twenty
/// lines is roughly where a paste stops fitting on an ordinary terminal (a 60-row
/// screen yields a 20-row band), and 2 000 characters describes the same limit from
/// the other direction — about twenty rows of a 100-column terminal — so a single
/// very long line is caught too. The plan's §5.5 starting point of three lines was
/// deliberately not used: it would summarise almost every paste, including the ones
/// the user pasted in order to edit.
pub const PASTE_SUMMARY_LINES: usize = 20;
/// See [`PASTE_SUMMARY_LINES`].
pub const PASTE_SUMMARY_CHARS: usize = 2_000;

/// Undo depth. Deep enough to recover a mis-keyed `input_delete_line`, shallow
/// enough that a snapshot-per-keystroke stack stays small.
pub const UNDO_LIMIT: usize = 200;

/// One line of the history file.
///
/// An object rather than a bare JSON string so the plan's `parts` (attachments) can
/// be added later without invalidating files either build wrote: serde ignores
/// fields it does not know, so a new writer stays readable by an old binary.
///
/// The JSON string is also what makes a multi-line prompt survive a line-oriented
/// format at all — every newline inside `input` is escaped as `\n`, so one entry is
/// always exactly one line however many lines the user typed.
#[derive(Debug, Serialize, Deserialize)]
struct HistoryLine {
    input: String,
}

/// Submitted prompts read back from a file a caller named.
///
/// Carries its own diagnostic rather than returning a `Result`, because there is no
/// failure here that should stop a TUI from starting: a user must always be able to
/// open the prompt, so an unusable file becomes an empty history plus something to
/// say about it.
#[derive(Debug, Default)]
pub struct PromptHistory {
    entries: Vec<String>,
    notice: Option<String>,
}

impl PromptHistory {
    /// Read `path`, keeping every line that parses.
    ///
    /// A missing file is not a problem and produces no notice — that is every first
    /// run. Anything else that goes wrong is reported, including the partial case: a
    /// truncated final line costs one prompt, and saying so is what tells a user
    /// their history is shorter than they remember for a reason.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                return Self {
                    entries: Vec::new(),
                    notice: Some(format!(
                        "prompt history at {} could not be read ({error}); starting with an \
                         empty history",
                        path.display()
                    )),
                };
            }
        };

        let mut entries = Vec::new();
        let mut skipped = 0_usize;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            if line.len() > HISTORY_ENTRY_LIMIT {
                skipped += 1;
                continue;
            }
            match serde_json::from_str::<HistoryLine>(line) {
                Ok(entry) => entries.push(entry.input),
                Err(_) => skipped += 1,
            }
        }
        // The cap belongs at the tail: `history_previous` walks back from the newest,
        // so dropping the front keeps the entries a user is most likely to want.
        if entries.len() > HISTORY_LIMIT {
            entries.drain(..entries.len() - HISTORY_LIMIT);
        }
        let notice = (skipped > 0).then(|| {
            format!(
                "prompt history at {}: skipped {skipped} unreadable entr{}",
                path.display(),
                if skipped == 1 { "y" } else { "ies" }
            )
        });
        Self { entries, notice }
    }

    /// The prompts, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// The prompts, oldest first, for a caller that installs them.
    #[must_use]
    pub fn into_entries(self) -> Vec<String> {
        self.entries
    }

    /// What went wrong while loading, when anything did.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// One appendable line for `entry`, or `None` when it is too large to persist.
    ///
    /// Includes its own trailing newline, so an interrupted append leaves a partial
    /// line that [`Self::load`] skips rather than a run-together pair that would
    /// corrupt the entry before it too.
    #[must_use]
    pub fn encode(entry: &str) -> Option<String> {
        if entry.len() > HISTORY_ENTRY_LIMIT {
            return None;
        }
        let line = serde_json::to_string(&HistoryLine {
            input: entry.to_owned(),
        })
        .ok()?;
        Some(format!("{line}\n"))
    }
}

/// A paste held aside while a placeholder stands in for it in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Paste {
    placeholder: String,
    full: String,
}

/// A cursor position: line index and character column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Position {
    /// Zero-based line.
    pub line: usize,
    /// Zero-based character column.
    pub column: usize,
}

/// What the editor asks its host to do after an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorSignal {
    /// Nothing to report.
    None,
    /// The buffer changed and a frame is needed.
    Changed,
    /// The user asked to send this text.
    Submit(String),
    /// The user asked to open `$EDITOR` on the current text.
    OpenExternalEditor,
    /// The user asked to paste from the clipboard.
    Paste,
    /// The user asked to copy the selection, which is carried here.
    Copy(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Position,
}

/// The prompt editor.
pub struct InputEditor {
    context: ViewContext,
    lines: Vec<String>,
    cursor: Position,
    /// Selection anchor, set by the `input_select_*` family.
    anchor: Option<Position>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    history: Vec<String>,
    /// Where in history the editor is, counted back from the newest.
    history_index: Option<usize>,
    /// Where a recorded prompt goes to be written down.
    ///
    /// Optional and `try_send`, like every sink on the composed screen: an editor with
    /// no host is still a legitimate editor, and a full queue must cost one recalled
    /// prompt rather than the frame the user is waiting for.
    history_sink: Option<mpsc::Sender<String>>,
    /// What was being typed before history was entered.
    stashed: Option<Vec<String>>,
    /// Pastes standing behind a placeholder, oldest first.
    pastes: Vec<Paste>,
    /// How many pastes this editor has summarised.
    ///
    /// The placeholder carries this number, and that is the load-bearing part: two
    /// pastes of the same line count would otherwise produce identical placeholders,
    /// and [`InputEditor::submission_text`] would expand one of them with the other's
    /// text — sending the model content the user never pasted there.
    paste_counter: usize,
    /// First rendered line, for a buffer taller than its area.
    offset: usize,
    /// The editor rectangle from the most recently painted frame.
    ///
    /// Pointer events carry terminal coordinates but no layout identity. Retaining the
    /// actual rectangle here lets the editor reject presses aimed at neighbouring
    /// surfaces and map a captured drag through the same scroll offset it rendered.
    rendered_area: Option<Rect>,
    /// The cursor position whose caret cell was present in [`Self::rendered_area`].
    ///
    /// A caret occupies one terminal column without occupying one buffer character.
    /// Pointer mapping uses this rendered snapshot rather than the possibly newer live
    /// cursor, so queued drag events still refer to the frame the user actually saw.
    rendered_cursor: Position,
    /// Whether a left-button press inside [`Self::rendered_area`] owns the pointer.
    pointer_selecting: bool,
    /// Muted text standing where the buffer is empty, or empty for none.
    placeholder: String,
}

impl InputEditor {
    /// An empty editor.
    #[must_use]
    pub fn new(context: ViewContext) -> Self {
        Self {
            context,
            lines: vec![String::new()],
            cursor: Position::default(),
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            history: Vec::new(),
            history_index: None,
            history_sink: None,
            stashed: None,
            pastes: Vec::new(),
            paste_counter: 0,
            offset: 0,
            rendered_area: None,
            rendered_cursor: Position::default(),
            pointer_selecting: false,
            placeholder: String::new(),
        }
    }

    /// Show `text` in [`ViewContext::muted`] while the buffer is empty.
    ///
    /// Held here rather than drawn by the screen over the editor's area, because only the
    /// editor knows whether the buffer is empty *and* where the caret is: a hint painted
    /// from outside would either cover the caret or leave it stranded in the middle of a
    /// sentence the user cannot edit.
    #[must_use]
    pub fn with_placeholder(mut self, text: impl Into<String>) -> Self {
        self.placeholder = text.into();
        self
    }

    /// Install prompts a previous run submitted, oldest first.
    ///
    /// Capped here as well as in [`Self::remember`], so an over-long file cannot make
    /// the in-memory list larger than a session's own recording would.
    pub fn load_history(&mut self, entries: Vec<String>) {
        self.history = entries;
        if self.history.len() > HISTORY_LIMIT {
            self.history.drain(..self.history.len() - HISTORY_LIMIT);
        }
        self.history_index = None;
    }

    /// Send every prompt this editor records to `sink`.
    pub fn record_history_to(&mut self, sink: mpsc::Sender<String>) {
        self.history_sink = Some(sink);
    }

    /// The buffer's text, lines joined with `\n`.
    ///
    /// What is on screen, so a summarised paste reads as its placeholder. Use
    /// [`Self::submission_text`] for anything leaving the prompt.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The buffer with every summarised paste put back.
    ///
    /// The text that submission, copying and `$EDITOR` all use: a placeholder is an
    /// affordance for the prompt band, and letting one leave the editor would send the
    /// model a description of the paste instead of the paste.
    #[must_use]
    pub fn submission_text(&self) -> String {
        let mut text = self.text();
        for paste in &self.pastes {
            // One occurrence, not all: the ordinal makes the placeholder unique, so a
            // second copy of it can only be one the user duplicated themselves — and
            // expanding that too would silently double a large paste. A placeholder the
            // user deleted expands to nothing, which is what deleting it meant.
            text = text.replacen(&paste.placeholder, &paste.full, 1);
        }
        text
    }

    /// Replace the buffer, putting the cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.snapshot();
        self.lines = split(text);
        self.cursor = self.end();
        self.anchor = None;
        // `text` is now the whole truth. A retained placeholder would either expand a
        // string the caller happened to include or, worse, be submitted literally —
        // both of which lose the paste this editor was holding for it.
        self.pastes.clear();
    }

    /// Replace the buffer after completion and place the cursor at an absolute
    /// character offset, clamped to the new text.
    pub fn apply_completion(&mut self, text: &str, cursor: usize) {
        self.snapshot();
        self.lines = split(text);
        let mut remaining = cursor;
        self.cursor = self.end();
        for (line, value) in self.lines.iter().enumerate() {
            let width = chars(value);
            if remaining <= width {
                self.cursor = Position {
                    line,
                    column: remaining,
                };
                break;
            }
            remaining = remaining.saturating_sub(width + 1);
        }
        self.anchor = None;
    }

    /// The cursor.
    #[must_use]
    pub const fn cursor(&self) -> Position {
        self.cursor
    }

    /// The selection anchor, when a selection is active.
    #[must_use]
    pub const fn anchor(&self) -> Option<Position> {
        self.anchor
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(String::is_empty)
    }

    /// Rows the buffer occupies.
    #[must_use]
    pub fn height(&self) -> usize {
        self.lines.len()
    }

    /// The submitted prompts, newest last.
    #[must_use]
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// The selected text, when a selection is active.
    #[must_use]
    pub fn selection(&self) -> Option<String> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        let (start, end) = if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };
        if start.line == end.line {
            let line = &self.lines[start.line];
            return Some(slice(line, start.column, end.column));
        }
        let mut out = slice(
            &self.lines[start.line],
            start.column,
            chars(&self.lines[start.line]),
        );
        for line in &self.lines[start.line + 1..end.line] {
            out.push('\n');
            out.push_str(line);
        }
        out.push('\n');
        out.push_str(&slice(&self.lines[end.line], 0, end.column));
        Some(out)
    }

    /// Apply a terminal mouse event to the prompt's selection state.
    ///
    /// A press is accepted only inside the rectangle saved by the most recent render.
    /// Once accepted, a drag or release remains captured and clamps to that rectangle,
    /// which makes selecting past an edge deterministic without stealing a press from
    /// the transcript or sidebar. The returned [`EditorSignal::Changed`] asks the host
    /// for a frame; copying still travels through `messages_copy`, so the existing
    /// clipboard path receives exactly [`Self::selection`].
    pub fn handle_mouse(&mut self, mouse: &MouseEvent) -> EditorSignal {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(position) = self.pointer_position(mouse.column, mouse.row, false) else {
                    return EditorSignal::None;
                };
                self.cursor = position;
                self.anchor = Some(position);
                self.pointer_selecting = true;
                EditorSignal::Changed
            }
            MouseEventKind::Drag(MouseButton::Left) if self.pointer_selecting => {
                if let Some(position) = self.pointer_position(mouse.column, mouse.row, true) {
                    self.cursor = position;
                }
                EditorSignal::Changed
            }
            MouseEventKind::Up(MouseButton::Left) if self.pointer_selecting => {
                if let Some(position) = self.pointer_position(mouse.column, mouse.row, true) {
                    self.cursor = position;
                }
                self.pointer_selecting = false;
                if self.anchor == Some(self.cursor) {
                    self.anchor = None;
                }
                EditorSignal::Changed
            }
            _ => EditorSignal::None,
        }
    }

    /// Map one terminal cell through the last rendered rectangle and scroll offset.
    fn pointer_position(&self, column: u16, row: u16, clamp: bool) -> Option<Position> {
        let area = self.rendered_area?;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let right = area.x.saturating_add(area.width);
        let bottom = area.y.saturating_add(area.height);
        let inside = column >= area.x && column < right && row >= area.y && row < bottom;
        if !inside && !clamp {
            return None;
        }

        let visible_row = if row < area.y {
            0
        } else if row >= bottom {
            usize::from(area.height - 1)
        } else {
            usize::from(row - area.y)
        };
        let line = self
            .offset
            .saturating_add(visible_row)
            .min(self.lines.len() - 1);
        let text = &self.lines[line];
        let line_width = chars(text);
        let visual_column = if column < area.x {
            0
        } else if column >= right {
            line_width
        } else {
            character_column_at_cell(
                text,
                (line == self.rendered_cursor.line).then_some(self.rendered_cursor.column),
                usize::from(column - area.x),
            )
        };
        Some(Position {
            line,
            column: visual_column,
        })
    }

    /// Insert one character at the cursor.
    ///
    /// The path a printable key takes: no binding claims it, the dispatcher hands it
    /// to the component tree, and the host calls this.
    pub fn insert_char(&mut self, character: char) -> EditorSignal {
        self.snapshot();
        self.delete_selection();
        if character == '\n' {
            self.split_line();
            return EditorSignal::Changed;
        }
        let index = byte_index(&self.lines[self.cursor.line], self.cursor.column);
        self.lines[self.cursor.line].insert(index, character);
        self.cursor.column += 1;
        EditorSignal::Changed
    }

    /// Insert pasted text, summarising it when it would swamp the prompt.
    ///
    /// The bracketed-paste path, and the reason a paste no longer submits anything:
    /// the whole block arrives as one event and goes through [`Self::insert_text`],
    /// which keeps its newlines as newlines instead of letting each one resolve to
    /// `input_submit`.
    ///
    /// Above [`PASTE_SUMMARY_LINES`]/[`PASTE_SUMMARY_CHARS`] the buffer gets a
    /// placeholder and the text is kept for [`Self::submission_text`].
    pub fn insert_paste(&mut self, text: &str) -> EditorSignal {
        // A single line pasted from a terminal carries the newline the copy ended
        // with; inserting it would open an empty line under the cursor and leave the
        // cursor on it. A multi-line paste keeps its trailing newline, because there
        // the line structure is the user's.
        let content = normalize_prompt_content(text);
        if content.is_empty() {
            return EditorSignal::None;
        }
        // `//` is the slash router's literal escape (see `views/slash.rs`), and it is
        // applied only when this paste becomes the buffer's *first* character. A pasted
        // absolute path or diff is never a command, and without the escape submitting
        // `/etc/hosts` earns `unknown command /etc` and discards the paste — the buffer
        // has already been cleared by then. Leaving the doubled slash visible is
        // deliberate: it says the leading slash is literal, and deleting one character
        // restores command intent.
        let escape = self.insertion_point() == Position::default() && content.starts_with('/');
        let escaped = if escape {
            format!("/{content}")
        } else {
            content.to_owned()
        };
        let lines = content.split('\n').count();
        if lines < PASTE_SUMMARY_LINES && content.chars().count() <= PASTE_SUMMARY_CHARS {
            return self.insert_text(&escaped);
        }
        self.paste_counter += 1;
        let placeholder = format!("[Pasted #{} ~{lines} lines]", self.paste_counter);
        let signal = self.insert_text(&placeholder);
        self.pastes.push(Paste {
            placeholder,
            full: escaped,
        });
        signal
    }

    /// Where the next insertion lands: the selection's start, or the cursor.
    fn insertion_point(&self) -> Position {
        self.anchor
            .map_or(self.cursor, |anchor| anchor.min(self.cursor))
    }

    /// Insert text, honouring embedded newlines.
    ///
    /// A pasted block keeps its line structure rather than being flattened, because a
    /// flattened shell script or patch is unusable.
    pub fn insert_text(&mut self, text: &str) -> EditorSignal {
        self.snapshot();
        self.delete_selection();
        for (index, chunk) in text.split('\n').enumerate() {
            if index > 0 {
                self.split_line();
            }
            let at = byte_index(&self.lines[self.cursor.line], self.cursor.column);
            self.lines[self.cursor.line].insert_str(at, chunk);
            self.cursor.column += chars(chunk);
        }
        EditorSignal::Changed
    }

    /// Act on one resolved binding.
    pub fn handle_action(&mut self, action: &'static Definition) -> EditorSignal {
        self.handle_action_recording(action, true)
    }

    /// Act on one resolved binding without adding a submitted prompt to text history.
    ///
    /// Rich attachments are held outside the editor. Remembering only their visible
    /// `[Image #N]` token would create a history entry that cannot reconstruct the
    /// corresponding bytes, so attachment owners use this path for that submission.
    pub fn handle_action_without_history(&mut self, action: &'static Definition) -> EditorSignal {
        self.handle_action_recording(action, false)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per binding; splitting it would only hide the table"
    )]
    fn handle_action_recording(
        &mut self,
        action: &'static Definition,
        record_submission: bool,
    ) -> EditorSignal {
        match action.name {
            // -- submission and external surfaces ---------------------------
            "input_submit" | "input_force_submit" | "prompt_submit" => {
                let text = self.submission_text();
                if text.trim().is_empty() {
                    return EditorSignal::None;
                }
                if record_submission {
                    self.remember(&text);
                }
                self.lines = vec![String::new()];
                self.cursor = Position::default();
                self.anchor = None;
                self.undo.clear();
                self.redo.clear();
                self.pastes.clear();
                EditorSignal::Submit(text)
            }
            "input_newline" => self.insert_char('\n'),
            "editor_open" => EditorSignal::OpenExternalEditor,
            "input_paste" => EditorSignal::Paste,
            "messages_copy" => match self.selection() {
                // A selection copies exactly what is highlighted, placeholder included:
                // that is what the user pointed at. Copying the whole prompt expands,
                // because nobody means to put `[Pasted #1 ~40 lines]` on their clipboard.
                Some(text) => EditorSignal::Copy(text),
                None => EditorSignal::Copy(self.submission_text()),
            },

            // -- movement ---------------------------------------------------
            "input_move_left" => self.moved(false, Self::left),
            "input_move_right" => self.moved(false, Self::right),
            "input_move_up" => self.moved(false, Self::up),
            "input_move_down" => self.moved(false, Self::down),
            "input_select_left" => self.moved(true, Self::left),
            "input_select_right" => self.moved(true, Self::right),
            "input_select_up" => self.moved(true, Self::up),
            "input_select_down" => self.moved(true, Self::down),
            "input_line_home" | "input_visual_line_home" => self.moved(false, Self::line_home),
            "input_line_end" | "input_visual_line_end" => self.moved(false, Self::line_end),
            "input_select_line_home" | "input_select_visual_line_home" => {
                self.moved(true, Self::line_home)
            }
            "input_select_line_end" | "input_select_visual_line_end" => {
                self.moved(true, Self::line_end)
            }
            "input_buffer_home" => self.moved(false, Self::buffer_home),
            "input_buffer_end" => self.moved(false, Self::buffer_end),
            "input_select_buffer_home" => self.moved(true, Self::buffer_home),
            "input_select_buffer_end" => self.moved(true, Self::buffer_end),
            "input_word_forward" => self.moved(false, Self::word_forward),
            "input_word_backward" => self.moved(false, Self::word_backward),
            "input_select_word_forward" => self.moved(true, Self::word_forward),
            "input_select_word_backward" => self.moved(true, Self::word_backward),
            "input_select_all" => {
                self.anchor = Some(Position::default());
                self.cursor = self.end();
                EditorSignal::Changed
            }

            // -- deletion ---------------------------------------------------
            "input_backspace" => {
                self.snapshot();
                if self.delete_selection() {
                    return EditorSignal::Changed;
                }
                if self.cursor.column > 0 {
                    let index = byte_index(&self.lines[self.cursor.line], self.cursor.column - 1);
                    self.lines[self.cursor.line].remove(index);
                    self.cursor.column -= 1;
                } else if self.cursor.line > 0 {
                    let removed = self.lines.remove(self.cursor.line);
                    self.cursor.line -= 1;
                    self.cursor.column = chars(&self.lines[self.cursor.line]);
                    self.lines[self.cursor.line].push_str(&removed);
                }
                EditorSignal::Changed
            }
            "input_delete" => {
                self.snapshot();
                if self.delete_selection() {
                    return EditorSignal::Changed;
                }
                let width = chars(&self.lines[self.cursor.line]);
                if self.cursor.column < width {
                    let index = byte_index(&self.lines[self.cursor.line], self.cursor.column);
                    self.lines[self.cursor.line].remove(index);
                } else if self.cursor.line + 1 < self.lines.len() {
                    let next = self.lines.remove(self.cursor.line + 1);
                    self.lines[self.cursor.line].push_str(&next);
                }
                EditorSignal::Changed
            }
            "input_delete_line" => {
                self.snapshot();
                self.lines.remove(self.cursor.line);
                if self.lines.is_empty() {
                    self.lines.push(String::new());
                }
                self.cursor.line = self.cursor.line.min(self.lines.len() - 1);
                self.cursor.column = 0;
                self.anchor = None;
                EditorSignal::Changed
            }
            "input_delete_to_line_end" => {
                self.snapshot();
                let index = byte_index(&self.lines[self.cursor.line], self.cursor.column);
                self.lines[self.cursor.line].truncate(index);
                self.anchor = None;
                EditorSignal::Changed
            }
            "input_delete_to_line_start" => {
                self.snapshot();
                let index = byte_index(&self.lines[self.cursor.line], self.cursor.column);
                self.lines[self.cursor.line] = self.lines[self.cursor.line][index..].to_owned();
                self.cursor.column = 0;
                self.anchor = None;
                EditorSignal::Changed
            }
            "input_delete_word_backward" => {
                self.snapshot();
                let target = self.word_backward();
                self.delete_between(target, self.cursor);
                self.cursor = target;
                EditorSignal::Changed
            }
            "input_delete_word_forward" => {
                self.snapshot();
                let target = self.word_forward();
                self.delete_between(self.cursor, target);
                EditorSignal::Changed
            }
            "input_clear" => {
                self.snapshot();
                self.lines = vec![String::new()];
                self.cursor = Position::default();
                self.anchor = None;
                EditorSignal::Changed
            }

            // -- undo, redo, history ----------------------------------------
            "input_undo" => {
                if let Some(snapshot) = self.undo.pop() {
                    self.redo.push(Snapshot {
                        lines: self.lines.clone(),
                        cursor: self.cursor,
                    });
                    self.lines = snapshot.lines;
                    self.cursor = snapshot.cursor;
                    self.anchor = None;
                    return EditorSignal::Changed;
                }
                EditorSignal::None
            }
            "input_redo" => {
                if let Some(snapshot) = self.redo.pop() {
                    self.undo.push(Snapshot {
                        lines: self.lines.clone(),
                        cursor: self.cursor,
                    });
                    self.lines = snapshot.lines;
                    self.cursor = snapshot.cursor;
                    self.anchor = None;
                    return EditorSignal::Changed;
                }
                EditorSignal::None
            }
            "history_previous" => self.previous_line_or_history(),
            "history_next" => self.next_line_or_history(),
            _ => EditorSignal::None,
        }
    }

    /// Record a submitted prompt, de-duplicating an immediate repeat.
    fn remember(&mut self, text: &str) {
        if self.history.last().is_some_and(|last| last == text) {
            return;
        }
        self.history.push(text.to_owned());
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
        self.history_index = None;
        self.stashed = None;
        // Sent from here rather than from the submission path, so the file records
        // exactly what this list did. A host that wrote its own copy would keep the
        // repeat this method just dropped, and the next run's history would disagree
        // with the one the user was walking a moment ago.
        if let Some(sink) = self.history_sink.as_ref() {
            let _recorded = sink.try_send(text.to_owned());
        }
    }

    /// Move vertically inside a multi-line prompt before crossing into history.
    ///
    /// `history` temporarily outranks `input` at either vertical edge, so both arrow
    /// actions arrive here there. Moving toward the buffer must remain ordinary cursor
    /// movement; only moving outward from the first/last line walks history. This is what
    /// keeps a pasted block editable without making an empty one-line prompt unable to
    /// recall earlier submissions.
    fn previous_line_or_history(&mut self) -> EditorSignal {
        if self.cursor.line > 0 {
            self.moved(false, Self::up)
        } else {
            self.walk_history(1)
        }
    }

    fn next_line_or_history(&mut self) -> EditorSignal {
        if self.cursor.line + 1 < self.lines.len() {
            self.moved(false, Self::down)
        } else {
            self.walk_history(-1)
        }
    }

    fn walk_history(&mut self, direction: i32) -> EditorSignal {
        if self.history.is_empty() {
            return EditorSignal::None;
        }
        let next = match (self.history_index, direction) {
            (None, 1) => Some(0),
            (None, _) => None,
            (Some(index), 1) => Some((index + 1).min(self.history.len() - 1)),
            (Some(0), _) => {
                // Stepping forward past the newest entry restores what was being
                // typed, which is why it was stashed.
                self.history_index = None;
                self.lines = self.stashed.take().unwrap_or_else(|| vec![String::new()]);
                self.cursor = self.end();
                return EditorSignal::Changed;
            }
            (Some(index), _) => Some(index - 1),
        };
        let Some(index) = next else {
            return EditorSignal::None;
        };
        if self.history_index.is_none() {
            self.stashed = Some(self.lines.clone());
        }
        self.history_index = Some(index);
        let entry = &self.history[self.history.len() - 1 - index];
        self.lines = split(entry);
        self.cursor = self.end();
        self.anchor = None;
        EditorSignal::Changed
    }

    fn moved(&mut self, extend: bool, next: fn(&Self) -> Position) -> EditorSignal {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = next(self);
        EditorSignal::Changed
    }

    fn left(&self) -> Position {
        if self.cursor.column > 0 {
            return Position {
                line: self.cursor.line,
                column: self.cursor.column - 1,
            };
        }
        if self.cursor.line > 0 {
            return Position {
                line: self.cursor.line - 1,
                column: chars(&self.lines[self.cursor.line - 1]),
            };
        }
        self.cursor
    }

    fn right(&self) -> Position {
        if self.cursor.column < chars(&self.lines[self.cursor.line]) {
            return Position {
                line: self.cursor.line,
                column: self.cursor.column + 1,
            };
        }
        if self.cursor.line + 1 < self.lines.len() {
            return Position {
                line: self.cursor.line + 1,
                column: 0,
            };
        }
        self.cursor
    }

    fn up(&self) -> Position {
        if self.cursor.line == 0 {
            return Position { line: 0, column: 0 };
        }
        let line = self.cursor.line - 1;
        Position {
            line,
            column: self.cursor.column.min(chars(&self.lines[line])),
        }
    }

    fn down(&self) -> Position {
        if self.cursor.line + 1 >= self.lines.len() {
            return self.end();
        }
        let line = self.cursor.line + 1;
        Position {
            line,
            column: self.cursor.column.min(chars(&self.lines[line])),
        }
    }

    fn line_home(&self) -> Position {
        Position {
            line: self.cursor.line,
            column: 0,
        }
    }

    fn line_end(&self) -> Position {
        Position {
            line: self.cursor.line,
            column: chars(&self.lines[self.cursor.line]),
        }
    }

    fn buffer_home(&self) -> Position {
        Position::default()
    }

    fn buffer_end(&self) -> Position {
        self.end()
    }

    fn end(&self) -> Position {
        let line = self.lines.len() - 1;
        Position {
            line,
            column: chars(&self.lines[line]),
        }
    }

    fn word_forward(&self) -> Position {
        let line: Vec<char> = self.lines[self.cursor.line].chars().collect();
        let mut column = self.cursor.column;
        if column >= line.len() {
            return self.right();
        }
        while column < line.len() && !line[column].is_alphanumeric() {
            column += 1;
        }
        while column < line.len() && line[column].is_alphanumeric() {
            column += 1;
        }
        Position {
            line: self.cursor.line,
            column,
        }
    }

    fn word_backward(&self) -> Position {
        let line: Vec<char> = self.lines[self.cursor.line].chars().collect();
        let mut column = self.cursor.column;
        if column == 0 {
            return self.left();
        }
        while column > 0 && !line[column - 1].is_alphanumeric() {
            column -= 1;
        }
        while column > 0 && line[column - 1].is_alphanumeric() {
            column -= 1;
        }
        Position {
            line: self.cursor.line,
            column,
        }
    }

    fn split_line(&mut self) {
        let index = byte_index(&self.lines[self.cursor.line], self.cursor.column);
        let tail = self.lines[self.cursor.line].split_off(index);
        self.lines.insert(self.cursor.line + 1, tail);
        self.cursor.line += 1;
        self.cursor.column = 0;
    }

    fn delete_selection(&mut self) -> bool {
        let Some(anchor) = self.anchor.take() else {
            return false;
        };
        if anchor == self.cursor {
            return false;
        }
        let (start, end) = if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };
        self.delete_between(start, end);
        self.cursor = start;
        true
    }

    fn delete_between(&mut self, start: Position, end: Position) {
        if start == end {
            return;
        }
        if start.line == end.line {
            let line = &self.lines[start.line];
            let from = byte_index(line, start.column);
            let to = byte_index(line, end.column);
            self.lines[start.line].replace_range(from..to, "");
            return;
        }
        let head = {
            let line = &self.lines[start.line];
            line[..byte_index(line, start.column)].to_owned()
        };
        let tail = {
            let line = &self.lines[end.line];
            line[byte_index(line, end.column)..].to_owned()
        };
        self.lines.drain(start.line..=end.line);
        self.lines.insert(start.line, head + &tail);
    }

    fn snapshot(&mut self) {
        self.undo.push(Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        });
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// The rendered rows, cursor and selection included.
    #[must_use]
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let selection = self.selection_bounds();
        self.lines
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let mut spans = Vec::new();
                let characters: Vec<char> = text.chars().collect();
                for (column, character) in characters.iter().enumerate() {
                    let selected = selection.is_some_and(|(start, end)| {
                        let here = Position {
                            line: index,
                            column,
                        };
                        here >= start && here < end
                    });
                    let style = if selected {
                        self.context.selected()
                    } else {
                        self.context.on_element(self.context.text())
                    };
                    spans.push(Span::styled(character.to_string(), style));
                }
                if index == self.cursor.line {
                    // Keep the long-standing glyph because layout assertions and copied
                    // screenshots use it to prove the caret remains inside the prompt band.
                    // Reverse video makes the entire cell visible instead of relying on a
                    // one-pixel-looking stroke, while all colours still come from theme roles.
                    let at = self.cursor.column.min(spans.len());
                    spans.insert(
                        at,
                        Span::styled(
                            String::from("▏"),
                            self.context
                                .on_element(self.context.text())
                                .add_modifier(Modifier::REVERSED),
                        ),
                    );
                }
                // After the caret, so the hint sits beside it rather than under it, and only
                // on the first row of a wholly empty buffer — a placeholder repeated down a
                // multi-line buffer would claim every blank line was unwritten.
                if index == 0 && !self.placeholder.is_empty() && self.is_empty() {
                    // Truncated in columns, and it has to be: the caret glyph already holds
                    // one, so a hint measured against the full width would put its tail past
                    // the right inset the band reserved.
                    let room = usize::from(width).saturating_sub(1);
                    let hint = crate::views::truncate(&self.placeholder, room);
                    if !hint.is_empty() {
                        spans.push(Span::styled(
                            hint,
                            self.context.on_element(self.context.muted()),
                        ));
                    }
                }
                let rendered: String = spans.iter().map(|span| span.content.as_ref()).collect();
                let pad = usize::from(width).saturating_sub(rendered.chars().count());
                if pad > 0 {
                    // The row's own tail, and it is the widest span on the line — painting it in
                    // `text` put the transcript's surface across most of the composer and was
                    // what made a four-row band read as one row of text.
                    spans.push(Span::styled(
                        " ".repeat(pad),
                        self.context.on_element(self.context.text()),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }

    fn selection_bounds(&self) -> Option<(Position, Position)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }
}

impl Component for InputEditor {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.rendered_area = Some(area);
        self.rendered_cursor = self.cursor;
        // `element`, not `text`. The two differ only in background, and `text`'s is
        // `background_panel` — the surface the transcript and the welcome screen are filled
        // with, so an editor painted with it is invisible as a region and the band's four rows
        // read as one. Its owner fills the surrounding band with the same role for that reason;
        // a two-tone box would be worse than a flat one. See
        // `crate::views::session::PROMPT_GUTTER_COLS`.
        fill(frame.buffer_mut(), area, self.context.element());
        let height = usize::from(area.height);
        // Keep the cursor's line on screen; a prompt whose cursor has scrolled out
        // is unusable, and this is the only place that knows both numbers.
        if self.cursor.line < self.offset {
            self.offset = self.cursor.line;
        } else if height > 0 && self.cursor.line >= self.offset + height {
            self.offset = self.cursor.line + 1 - height;
        }
        let mut lines = self.lines(area.width);
        let visible = lines
            .drain(..)
            .skip(self.offset)
            .take(height)
            .collect::<Vec<_>>();
        Paragraph::new(visible)
            .style(self.context.element())
            .render(area, frame.buffer_mut());
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        if let AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Mouse(mouse))) = event {
            return match self.handle_mouse(mouse) {
                EditorSignal::Changed => EventResult::REDRAW,
                _ => EventResult::IGNORED,
            };
        }
        // Keys arrive as actions through `handle_action`. Returning `IGNORED` for every
        // other event keeps engine state flowing past the editor.
        EventResult::IGNORED
    }
}

/// The prompt's leading marker and its own row, drawn beside the editor.
pub struct PromptGutter {
    context: ViewContext,
    /// The marker shown, e.g. the active agent's initial.
    pub label: String,
}

impl PromptGutter {
    /// A gutter over `context`.
    #[must_use]
    pub const fn new(context: ViewContext, label: String) -> Self {
        Self { context, label }
    }
}

impl Component for PromptGutter {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        // Not `accent`: accent is the *marker's* colour, and filling the gutter's background
        // with it makes two columns a solid bar down the band that reads as a selection rather
        // than a margin. The two styles differ on purpose. `element` rather than `text` so the
        // gutter shares the band's surface — see `InputEditor::render`.
        fill(frame.buffer_mut(), area, self.context.element());
        Paragraph::new(vec![padded(
            &self.label,
            area.width,
            self.context.on_element(self.context.accent()),
        )])
        .render(area, frame.buffer_mut());
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

fn split(text: &str) -> Vec<String> {
    let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

/// Character boundary under one rendered terminal cell.
///
/// The buffer stores character columns while pointer events report terminal cells. A wide
/// glyph occupies more than one cell, a combining mark may occupy none, and the visible caret
/// inserts one extra cell at its character boundary. Walking the exact rendered sequence keeps
/// all three cases aligned.
fn character_column_at_cell(text: &str, caret: Option<usize>, target: usize) -> usize {
    let length = chars(text);
    let caret = caret.map(|column| column.min(length));
    let mut cell = 0usize;
    for (column, character) in text.chars().enumerate() {
        if caret == Some(column) {
            if target == cell {
                return column;
            }
            cell = cell.saturating_add(1);
        }
        let width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if target < cell.saturating_add(width) {
            return column;
        }
        cell = cell.saturating_add(width);
    }
    length
}

fn byte_index(text: &str, column: usize) -> usize {
    text.char_indices()
        .nth(column)
        .map_or_else(|| text.len(), |(index, _)| index)
}

fn slice(text: &str, from: usize, to: usize) -> String {
    text.chars()
        .skip(from)
        .take(to.saturating_sub(from))
        .collect()
}

/// Strip a trailing newline a paste added, when the content is a single line.
///
/// Verbatim from `packages/tui/src/editor.ts:12-24`. The condition matters: a
/// multi-line paste keeps its trailing newline, because the user's line structure is
/// meaningful, while a single line pasted from a terminal picks up a `\n` that would
/// submit the prompt.
#[must_use]
pub fn normalize_prompt_content(content: &str) -> &str {
    if let Some(body) = content.strip_suffix("\r\n")
        && !body.contains('\n')
        && !body.contains('\r')
    {
        return body;
    }
    if let Some(body) = content.strip_suffix('\n')
        && !body.contains('\n')
        && !body.contains('\r')
    {
        return body;
    }
    content
}
