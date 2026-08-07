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

use crate::app::{AppEvent, Component, EventResult};
use crate::keybind::Definition;
use crate::views::{ViewContext, fill, padded};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;

/// The most recent prompts kept for `history_previous`.
///
/// A cap rather than unbounded growth: history lives for the process, and an
/// unbounded list of prompts is a slow memory leak in a long session.
pub const HISTORY_LIMIT: usize = 100;

/// Undo depth. Deep enough to recover a mis-keyed `input_delete_line`, shallow
/// enough that a snapshot-per-keystroke stack stays small.
pub const UNDO_LIMIT: usize = 200;

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
    /// What was being typed before history was entered.
    stashed: Option<Vec<String>>,
    /// First rendered line, for a buffer taller than its area.
    offset: usize,
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
            stashed: None,
            offset: 0,
        }
    }

    /// The buffer's text, lines joined with `\n`.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Replace the buffer, putting the cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.snapshot();
        self.lines = split(text);
        self.cursor = self.end();
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

    /// Insert text, honouring embedded newlines.
    ///
    /// The paste path. A pasted block keeps its line structure rather than being
    /// flattened, because a flattened shell script or patch is unusable.
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
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per binding; splitting it would only hide the table"
    )]
    pub fn handle_action(&mut self, action: &'static Definition) -> EditorSignal {
        match action.name {
            // -- submission and external surfaces ---------------------------
            "input_submit" | "prompt_submit" => {
                let text = self.text();
                if text.trim().is_empty() {
                    return EditorSignal::None;
                }
                self.remember(&text);
                self.lines = vec![String::new()];
                self.cursor = Position::default();
                self.anchor = None;
                self.undo.clear();
                self.redo.clear();
                EditorSignal::Submit(text)
            }
            "input_newline" => self.insert_char('\n'),
            "editor_open" => EditorSignal::OpenExternalEditor,
            "input_paste" => EditorSignal::Paste,
            "messages_copy" => match self.selection() {
                Some(text) => EditorSignal::Copy(text),
                None => EditorSignal::Copy(self.text()),
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
            "history_previous" => self.walk_history(1),
            "history_next" => self.walk_history(-1),
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
                        self.context.text()
                    };
                    spans.push(Span::styled(character.to_string(), style));
                }
                if index == self.cursor.line {
                    // A block glyph rather than a real terminal cursor: the frame is
                    // also what an off-screen assertion sees, and a hardware cursor
                    // leaves no trace in the buffer.
                    let at = self.cursor.column.min(spans.len());
                    spans.insert(at, Span::styled(String::from("▏"), self.context.accent()));
                }
                let rendered: String = spans.iter().map(|span| span.content.as_ref()).collect();
                let pad = usize::from(width).saturating_sub(rendered.chars().count());
                if pad > 0 {
                    spans.push(Span::styled(" ".repeat(pad), self.context.text()));
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
        fill(frame.buffer_mut(), area, self.context.text());
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
            .style(self.context.text())
            .render(area, frame.buffer_mut());
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        // Keys arrive as actions through `handle_action`; nothing else concerns the
        // editor. Returning `IGNORED` keeps engine events flowing past it.
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
        fill(frame.buffer_mut(), area, self.context.accent());
        Paragraph::new(vec![padded(&self.label, area.width, self.context.accent())])
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
