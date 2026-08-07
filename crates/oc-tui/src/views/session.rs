//! The composed session screen: transcript above, status strip, prompt below.
//!
//! Todo 76 built every view and left this composition to whoever booted the TUI,
//! and nothing did — so the views were reachable only from their own tests. This
//! is the one type a host needs to construct in order to have a screen, and it
//! lives here rather than in the CLI so that rendering stays inside this crate and
//! the host only wires channels.
//!
//! # Shutdown travels back through the terminal channel
//!
//! [`crate::app::App`] ends its loop on [`crate::app::TerminalEvent::Shutdown`] and
//! on nothing else, so a screen that resolves the `app_exit` action has to *send*
//! that event rather than return a flag. The alternative — teaching the input
//! producer which key means exit — would put a key spelling back above the keymap,
//! which is the one thing the view layer's discipline forbids. The sender is
//! therefore a collaborator of the screen, and `try_send` is deliberate: a full
//! terminal channel already has 64 events queued, and blocking here would stall the
//! render loop that has to drain them.

use crate::app::{AppEvent, Component, EventResult, TerminalEvent};
use crate::keybind::{ActionComponent, Definition};
use crate::views::ViewContext;
use crate::views::editor::{EditorSignal, InputEditor};
use crate::views::message::{Message, StatusView, TranscriptView};
use crate::views::permission::typed_character;
use crossterm::event::{Event as CrosstermEvent, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use tokio::sync::mpsc;

/// Rows reserved for the status strip and the prompt.
const STATUS_ROWS: u16 = 1;
const PROMPT_ROWS: u16 = 2;

/// The transcript, the status strip and the prompt as one screen.
pub struct SessionScreen {
    transcript: TranscriptView,
    status: StatusView,
    editor: InputEditor,
    shutdown: mpsc::Sender<TerminalEvent>,
    submissions: Vec<String>,
}

impl SessionScreen {
    /// A screen that requests shutdown through `shutdown` when `app_exit` resolves.
    #[must_use]
    pub fn new(context: ViewContext, shutdown: mpsc::Sender<TerminalEvent>) -> Self {
        Self {
            transcript: TranscriptView::new(context.clone()),
            status: StatusView::new(context.clone()),
            editor: InputEditor::new(context),
            shutdown,
            submissions: Vec::new(),
        }
    }

    /// The transcript, for a host that appends locally composed messages.
    pub const fn transcript_mut(&mut self) -> &mut TranscriptView {
        &mut self.transcript
    }

    /// Every text the user has submitted, oldest first.
    ///
    /// Retained rather than handed to a callback because a turn driver does not
    /// exist yet: a host that gains one reads them from here, and a host that has
    /// none can still show that the submission was received.
    #[must_use]
    pub fn submissions(&self) -> &[String] {
        &self.submissions
    }
}

impl Component for SessionScreen {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let [transcript, status, prompt] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(STATUS_ROWS),
            Constraint::Length(PROMPT_ROWS),
        ])
        .areas(area);
        self.transcript.render(frame, transcript);
        self.status.render(frame, status);
        self.editor.render(frame, prompt);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        // A printable key resolves to no action, so the dispatcher forwards it here
        // and the screen is what routes it into the prompt. Without this the editor
        // could not be typed into at all — see `permission::typed_character`, the
        // same seam the reject box uses.
        if let AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Key(key))) = event
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && let Some(character) = typed_character(key)
        {
            self.editor.insert_char(character);
            return EventResult::REDRAW;
        }
        self.transcript
            .handle_event(event)
            .merge(self.status.handle_event(event))
    }
}

impl ActionComponent for SessionScreen {
    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> EventResult {
        // `ctrl+c` and `ctrl+d` are each bound twice in the shipped table: once in
        // the `input` scope and once as `app_exit`. The input scope wins, so a
        // screen that only watched for `app_exit` could never be left — the two
        // editor actions have to fall through when there is nothing to act on,
        // which is also the behaviour the keys have in the reference TUI.
        let empty = self.editor.text().is_empty();
        if action.name == "app_exit"
            || (empty && matches!(action.name, "input_clear" | "input_delete"))
        {
            let _requested = self.shutdown.try_send(TerminalEvent::Shutdown);
            return EventResult::REDRAW;
        }
        match self.editor.handle_action(action) {
            EditorSignal::None => EventResult::IGNORED,
            EditorSignal::Submit(text) => {
                self.transcript
                    .transcript_mut()
                    .push(Message::user(text.clone()));
                self.submissions.push(text);
                EventResult::REDRAW
            }
            EditorSignal::Changed
            | EditorSignal::OpenExternalEditor
            | EditorSignal::Paste
            | EditorSignal::Copy(_) => EventResult::REDRAW,
        }
    }
}

/// The scope chain a session screen resolves keys in, outermost last.
///
/// `input` and `prompt` before `app` so a binding the editor claims wins over an
/// application-wide one on the same chord, and `app` last so `app_exit` still
/// resolves while the prompt has focus.
#[must_use]
pub fn scopes() -> Vec<String> {
    vec![
        String::from("input"),
        String::from("prompt"),
        String::from("messages"),
        String::from("app"),
    ]
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
