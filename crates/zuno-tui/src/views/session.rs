//! The composed session screen: transcript above, status strip, prompt below.
//!
//! Todo 76 built every view and left this composition to whoever booted the TUI,
//! and nothing did — so the views were reachable only from their own tests. This
//! is the one type a host needs to construct in order to have a screen, and it
//! lives here rather than in the CLI so that rendering stays inside this crate and
//! the host only wires channels.
//!
//! # A submitted prompt leaves through a channel, and the turn comes back as events
//!
//! [`SessionScreen::with_prompt_sink`] is the only outward edge this screen has
//! besides shutdown, and it is deliberately as thin as one: a `String` out, and
//! [`zuno_engine::r#loop::TurnEvent`]s back in through
//! [`crate::app::AppEvent::Engine`]. The screen therefore knows nothing about
//! sessions, providers, databases or tools — a turn driver is not a collaborator it
//! holds, it is a reader on the far side of a bounded channel. That is what keeps
//! this crate above the turn loop even though a keystroke here now starts one.
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
//!
//! # An exit chord during a running turn cancels the turn, but only once
//!
//! Tearing the application down mid-turn discards work the user is waiting for, so
//! the first exit chord asks the driver to cancel rather than leaving.
//!
//! The second one leaves unconditionally, and that is the load-bearing part. Reading
//! "has a turn been cancelled already" off the status strip's running state looks
//! equivalent and is not: a turn parked on a permission ask never reaches the
//! engine's interrupt check, so it stays running after an abort and the strip never
//! clears. A screen that re-derived its answer from the strip would cancel forever
//! and never leave — the same trap in a politer form. One press is therefore
//! remembered explicitly, and cleared when a new turn starts.
//!
//! For the same reason cancellation never gets to swallow the key: with no sink
//! attached, or a sink that refuses, the chord falls straight through to shutdown. A
//! user must always have a way out, so a broken collaborator costs a cancelled turn,
//! never the ability to leave.

use crate::app::{AppEvent, Component, EventResult, TerminalEvent};
use crate::keybind::{APP_EXIT, ActionComponent, Definition, is_exit_request};
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
    prompts: Option<mpsc::Sender<String>>,
    cancels: Option<mpsc::Sender<()>>,
    submissions: Vec<String>,
    cancellations: usize,
    cancel_requested: bool,
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
            prompts: None,
            cancels: None,
            submissions: Vec::new(),
            cancellations: 0,
            cancel_requested: false,
        }
    }

    /// Forward every submitted prompt to a turn driver.
    ///
    /// A channel and not a callback for the reason the dialog set has one: a
    /// callback would run inside `handle_action`, which is the one frame a turn must
    /// not be started from — the loop that has to draw the turn's events is the
    /// caller. `try_send` for the same reason the shutdown sender uses it.
    ///
    /// Optional because a screen with no driver is still a legitimate screen — every
    /// view test builds one — and a `Sender` it could not answer would be worse.
    #[must_use]
    pub fn with_prompt_sink(mut self, prompts: mpsc::Sender<String>) -> Self {
        self.prompts = Some(prompts);
        self
    }

    /// Let an exit chord cancel a running turn instead of leaving the application.
    ///
    /// Optional for the same reason [`Self::with_prompt_sink`] is: a screen with no
    /// driver has no turn to cancel. Without it, an exit chord leaves immediately.
    #[must_use]
    pub fn with_cancel_sink(mut self, cancels: mpsc::Sender<()>) -> Self {
        self.cancels = Some(cancels);
        self
    }

    /// How many times an exit chord has cancelled a running turn.
    ///
    /// Retained for the same reason [`Self::submissions`] is: a test should be able
    /// to tell "cancelled the turn" from "left the application" without owning the
    /// far side of either channel.
    #[must_use]
    pub const fn cancellations(&self) -> usize {
        self.cancellations
    }

    /// The transcript, for a host that appends locally composed messages.
    pub const fn transcript_mut(&mut self) -> &mut TranscriptView {
        &mut self.transcript
    }

    /// Every text the user has submitted, oldest first.
    ///
    /// Retained as well as forwarded: a screen with no driver attached still has to
    /// show that the submission was received, and a test asserting what the user
    /// sent should not have to own the other end of a channel to read it.
    #[must_use]
    pub fn submissions(&self) -> &[String] {
        &self.submissions
    }

    /// Submit `text` as though the user had typed and sent it.
    ///
    /// The one path a host needs for a prompt supplied on the command line. It goes
    /// through the same code an interactive submission does so that an unattended
    /// invocation and a typed one cannot diverge — which is exactly the divergence a
    /// host that pushed to the transcript itself would introduce.
    pub fn submit_prompt(&mut self, text: impl Into<String>) {
        self.submit(text.into());
    }

    /// Hand `text` to the driver, or say in the transcript that nobody took it.
    ///
    /// Reporting the refusal is the point. A prompt that vanished because the driver
    /// had gone away, rendered identically to one accepted, is the defect where "no
    /// results" and "cannot see the data" look the same.
    fn submit(&mut self, text: String) {
        self.transcript
            .transcript_mut()
            .push(Message::user(text.clone()));
        if let Some(prompts) = self.prompts.as_ref() {
            match prompts.try_send(text.clone()) {
                Ok(()) => self.mark_turn_accepted(),
                Err(error) => {
                    let reason = match error {
                        mpsc::error::TrySendError::Full(_) => "a turn is already running",
                        mpsc::error::TrySendError::Closed(_) => "the turn driver has stopped",
                    };
                    self.transcript
                        .transcript_mut()
                        .push(Message::user(format!("not sent: {reason}")));
                }
            }
        }
        self.submissions.push(text);
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

impl SessionScreen {
    fn mark_turn_accepted(&mut self) {
        self.cancel_requested = false;
        self.status.mark_running();
    }

    /// Cancel a running turn, or leave the application when none is running.
    ///
    /// Falling through to shutdown when the sink is missing or refuses is what keeps
    /// this from becoming the trap described in the module docs.
    fn request_exit(&mut self) -> EventResult {
        if self.status.is_running()
            && !self.cancel_requested
            && let Some(cancels) = self.cancels.as_ref()
            && cancels.try_send(()).is_ok()
        {
            self.cancel_requested = true;
            self.cancellations += 1;
            self.transcript
                .transcript_mut()
                .push(Message::user(String::from(
                    "cancelling the turn; press the same key again to exit",
                )));
            return EventResult::REDRAW;
        }
        let _requested = self.shutdown.try_send(TerminalEvent::Shutdown);
        EventResult::REDRAW
    }
}

impl ActionComponent for SessionScreen {
    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> EventResult {
        // `ctrl+c` and `ctrl+d` are each claimed by the `input` scope before `app`,
        // so a screen that only watched for `app_exit` could never be left. Asking
        // the keymap whether the *chord* is an exit chord — rather than matching the
        // action names the resolution happened to produce — is what makes this
        // independent of which scope won, and it is why `delete`, the other spelling
        // of `input_delete`, no longer quits an application it was never bound to.
        let editor_owns_chord =
            !self.editor.text().is_empty() && matches!(action.name, "input_clear" | "input_delete");
        if action.name == APP_EXIT || (is_exit_request(event) && !editor_owns_chord) {
            return self.request_exit();
        }
        match self.editor.handle_action(action) {
            EditorSignal::None => EventResult::IGNORED,
            EditorSignal::Submit(text) => {
                self.submit(text);
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
