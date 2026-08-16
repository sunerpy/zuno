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
    welcome: crate::views::welcome::WelcomeView,
    sidebar: crate::views::ambient::SidebarView,
    shutdown: mpsc::Sender<TerminalEvent>,
    prompts: Option<mpsc::Sender<String>>,
    cancels: Option<mpsc::Sender<()>>,
    submissions: Vec<String>,
    cancellations: usize,
    cancel_requested: bool,
    sidebar_visible: bool,
    /// The resolved palette and configuration, for the pickers this screen builds.
    context: ViewContext,
    /// What the pickers offer, stated by the host.
    catalog: SessionCatalog,
    /// Dialogs asked for but not yet opened by the host.
    requested: Vec<Box<dyn crate::views::dialog::Dialog>>,
    /// Selections the user made, for a host that applies them to the next turn.
    selections: Option<mpsc::Sender<Selection>>,
}

/// One choice the user made in a picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// A different `provider/model` for subsequent turns.
    Model(String),
    /// A different agent for subsequent turns.
    Agent(String),
    /// A different session to continue in.
    Session(String),
    /// A different theme.
    Theme(String),
}

/// What the pickers can offer, as the host resolved it.
///
/// Plain lists rather than a live query: a picker redraws on every keystroke, and a
/// surface that re-listed sessions per frame would put a database read in the render
/// path. The host states them once and restates them when they change.
#[derive(Debug, Clone, Default)]
pub struct SessionCatalog {
    /// Every model the catalog offers.
    pub models: Vec<crate::views::picker::ModelEntry>,
    /// Every agent discovery found.
    pub agents: Vec<crate::views::picker::AgentEntry>,
    /// Recent sessions.
    pub sessions: Vec<crate::views::picker::SessionEntry>,
    /// `provider/model` currently in use, so the picker opens on it.
    pub model: Option<String>,
    /// The agent currently in use.
    pub agent: Option<String>,
}

impl SessionScreen {
    /// A screen that requests shutdown through `shutdown` when `app_exit` resolves.
    #[must_use]
    pub fn new(context: ViewContext, shutdown: mpsc::Sender<TerminalEvent>) -> Self {
        Self {
            transcript: TranscriptView::new(context.clone()),
            status: StatusView::new(context.clone()),
            welcome: crate::views::welcome::WelcomeView::new(context.clone()),
            sidebar: crate::views::ambient::SidebarView::new(context.clone()),
            editor: InputEditor::new(context.clone()),
            shutdown,
            prompts: None,
            cancels: None,
            submissions: Vec::new(),
            cancellations: 0,
            cancel_requested: false,
            sidebar_visible: true,
            context,
            catalog: SessionCatalog::default(),
            requested: Vec::new(),
            selections: None,
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

    /// The welcome screen, for the host that resolves the facts it states.
    pub const fn welcome_mut(&mut self) -> &mut crate::views::welcome::WelcomeView {
        &mut self.welcome
    }

    /// State what the pickers offer.
    #[must_use]
    pub fn with_catalog(mut self, catalog: SessionCatalog) -> Self {
        self.catalog = catalog;
        self
    }

    /// Forward every picker choice to a host that can apply it.
    ///
    /// Optional and `try_send`, for the same reasons the prompt sink is: a screen with
    /// no host is still a legitimate screen, and blocking here would stall the loop
    /// that has to draw the choice.
    #[must_use]
    pub fn with_selection_sink(mut self, selections: mpsc::Sender<Selection>) -> Self {
        self.selections = Some(selections);
        self
    }

    /// What the pickers offer, mutably, for a host that restates it.
    pub const fn catalog_mut(&mut self) -> &mut SessionCatalog {
        &mut self.catalog
    }

    /// The status strip, for the host that states the configured agent and model.
    pub const fn status_mut(&mut self) -> &mut StatusView {
        &mut self.status
    }

    /// The ambient panel, for the host that resolves its services.
    pub const fn sidebar_mut(&mut self) -> &mut crate::views::ambient::SidebarView {
        &mut self.sidebar
    }

    /// Whether the ambient panel is drawn when the terminal is wide enough.
    #[must_use]
    pub const fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
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
        let [body, status, prompt] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(STATUS_ROWS),
            Constraint::Length(PROMPT_ROWS),
        ])
        .areas(area);

        // The sidebar is dropped rather than narrowed below the threshold: a panel
        // squeezed until its server names truncate says less than no panel while still
        // costing the reply the columns it needed.
        let (main, aside) = if self.sidebar_visible && area.width >= crate::views::SIDEBAR_MIN_WIDTH
        {
            let [main, aside] = Layout::horizontal([
                Constraint::Min(1),
                Constraint::Length(crate::views::ambient::SIDEBAR_WIDTH),
            ])
            .areas(body);
            (main, Some(aside))
        } else {
            (body, None)
        };

        // The transcript owns this region as soon as there is anything to show, so the
        // welcome screen can never hide content — it only fills rows that would
        // otherwise be blank.
        if self.transcript.transcript().messages().is_empty() {
            self.welcome.render(frame, main);
        } else {
            self.transcript.render(frame, main);
        }

        if let Some(aside) = aside {
            // Both the panel and the strip read the transcript's single accumulator
            // rather than folding the provider stream again, which is what keeps the
            // two token figures on screen from ever disagreeing.
            let ambient = self.sidebar.ambient_mut();
            ambient.tokens = self.transcript.transcript().tokens();
            ambient.context_used = self.transcript.transcript().context_used();
            self.sidebar.render(frame, aside);
        }

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

    /// Route the actions that change what is *shown* rather than what is typed.
    ///
    /// These were the largest class of built-but-unreachable behaviour in this crate.
    /// [`TranscriptView`] has had `toggle_thinking` and a clamped `set_offset` since the
    /// view layer was written, and no key press could reach either, because the composed
    /// screen forwarded keys only to the editor — and the editor answers
    /// [`EditorSignal::None`] for all of them, which the screen then reported as
    /// unhandled. A collapsible reasoning block nothing can collapse is
    /// indistinguishable from one that does not exist.
    fn handle_view_action(&mut self, action: &'static Definition) -> EventResult {
        let viewport = self.transcript.viewport_height().max(1);
        let max = self
            .transcript
            .content_height()
            .saturating_sub(self.transcript.viewport_height());
        let offset = self.transcript.offset();
        let moved = |delta: isize| -> usize {
            let target = isize::try_from(offset)
                .unwrap_or(isize::MAX)
                .saturating_add(delta);
            usize::try_from(target.max(0)).unwrap_or(0).min(max)
        };
        let half = isize::try_from(viewport / 2).unwrap_or(1).max(1);
        let page = isize::try_from(viewport).unwrap_or(1);
        match action.name {
            "display_thinking" => {
                self.transcript.toggle_thinking();
                EventResult::REDRAW
            }
            "tool_details" => {
                self.transcript.toggle_tool_output();
                EventResult::REDRAW
            }
            "sidebar_toggle" => {
                self.sidebar_visible = !self.sidebar_visible;
                EventResult::REDRAW
            }
            "tips_toggle" => {
                if self.welcome.tips_visible() {
                    self.welcome.hide_tips();
                } else {
                    self.welcome.next_tip();
                }
                EventResult::REDRAW
            }
            "messages_line_up" => {
                self.transcript.set_offset(moved(-1));
                EventResult::REDRAW
            }
            "messages_line_down" => {
                self.transcript.set_offset(moved(1));
                EventResult::REDRAW
            }
            "messages_page_up" => {
                self.transcript.set_offset(moved(-page));
                EventResult::REDRAW
            }
            "messages_page_down" => {
                self.transcript.set_offset(moved(page));
                EventResult::REDRAW
            }
            "messages_half_page_up" => {
                self.transcript.set_offset(moved(-half));
                EventResult::REDRAW
            }
            "messages_half_page_down" => {
                self.transcript.set_offset(moved(half));
                EventResult::REDRAW
            }
            "messages_first" => {
                self.transcript.set_offset(0);
                EventResult::REDRAW
            }
            "messages_last" => {
                self.transcript.set_offset(max);
                self.transcript.follow();
                EventResult::REDRAW
            }
            "model_list" => self.request(self.model_picker()),
            "agent_list" => self.request(self.agent_picker()),
            "session_list" => self.request(self.session_picker()),
            "theme_list" => self.request(self.theme_picker()),
            _ => EventResult::IGNORED,
        }
    }

    /// Ask the host to open `dialog`, or say why it cannot be opened.
    ///
    /// A picker with nothing in it is the failure mode that reads as a broken key: the
    /// dialog opens, says `no matches`, and the user cannot tell an empty catalog from
    /// a surface that did not load. Saying so in the transcript keeps the two apart.
    fn request(&mut self, dialog: Option<Box<dyn crate::views::dialog::Dialog>>) -> EventResult {
        match dialog {
            Some(dialog) => {
                self.requested.push(dialog);
                EventResult::REDRAW
            }
            None => {
                self.transcript
                    .transcript_mut()
                    .push(Message::notice("nothing to choose from here yet"));
                EventResult::REDRAW
            }
        }
    }

    fn model_picker(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        if self.catalog.models.is_empty() {
            return None;
        }
        let mut picker =
            crate::views::picker::model_picker(self.context.clone(), self.catalog.models.clone());
        if let Some(model) = &self.catalog.model {
            picker = picker.selecting(model);
        }
        Some(Box::new(picker))
    }

    fn agent_picker(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        if self.catalog.agents.is_empty() {
            return None;
        }
        let mut picker =
            crate::views::picker::agent_picker(self.context.clone(), self.catalog.agents.clone());
        if let Some(agent) = &self.catalog.agent {
            picker = picker.selecting(agent);
        }
        Some(Box::new(picker))
    }

    fn session_picker(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        if self.catalog.sessions.is_empty() {
            return None;
        }
        Some(Box::new(crate::views::picker::session_picker(
            self.context.clone(),
            self.catalog.sessions.clone(),
        )))
    }

    fn theme_picker(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        // The registry is built here rather than held, because the picker resolves every
        // theme once at construction for its preview and then never consults it again.
        let registry = crate::theme::ThemeRegistry::new();
        Some(Box::new(crate::views::picker::theme_picker(
            self.context.clone(),
            &registry,
            crate::theme::Mode::Dark,
        )))
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
            self.transcript.transcript_mut().push(Message::notice(
                "cancelling the turn; press the same key again to exit",
            ));
            return EventResult::REDRAW;
        }
        let _requested = self.shutdown.try_send(TerminalEvent::Shutdown);
        EventResult::REDRAW
    }
}

impl SessionScreen {
    /// Adopt a picker's answer, and forward it to whoever can act on it.
    ///
    /// The strip and the welcome facts are updated here so the choice is visible
    /// immediately, while the sink carries it to the host that can only apply it to the
    /// *next* turn. Saying so in the transcript is the point: a model that changed on
    /// screen but not in the running turn, with nothing said, is a surface that lies.
    fn adopt(&mut self, dialog: &'static str, value: &str) -> EventResult {
        let selection = match dialog {
            crate::views::picker::MODEL_DIALOG_ID => {
                self.catalog.model = Some(value.to_owned());
                self.status.set_configured_model(value);
                self.welcome.facts_mut().model = Some(value.to_owned());
                self.sidebar.ambient_mut().model = Some(value.to_owned());
                Selection::Model(value.to_owned())
            }
            crate::views::picker::AGENT_DIALOG_ID => {
                self.catalog.agent = Some(value.to_owned());
                self.status.set_configured_agent(value);
                self.welcome.facts_mut().agent = Some(value.to_owned());
                self.sidebar.ambient_mut().agent = Some(value.to_owned());
                Selection::Agent(value.to_owned())
            }
            crate::views::picker::SESSION_DIALOG_ID => Selection::Session(value.to_owned()),
            crate::views::picker::THEME_DIALOG_ID => Selection::Theme(value.to_owned()),
            _ => return EventResult::IGNORED,
        };
        let notice = match &selection {
            Selection::Model(model) => format!("model set to {model} for the next turn"),
            Selection::Agent(agent) => format!("agent set to {agent} for the next turn"),
            Selection::Session(id) => format!("session {id} selected"),
            Selection::Theme(theme) => format!("theme {theme} selected"),
        };
        let delivered = self
            .selections
            .as_ref()
            .is_some_and(|sink| sink.try_send(selection).is_ok());
        let text = if delivered {
            notice
        } else {
            // A refused sink is reported rather than swallowed. The alternative is the
            // defect this whole change is about: a picker that appears to work, a
            // selection that reached nothing, and no way for the user to tell.
            format!("{notice} (not applied: nothing is listening)")
        };
        self.transcript.transcript_mut().push(Message::notice(text));
        EventResult::REDRAW
    }
}

impl ActionComponent for SessionScreen {
    fn drain_dialogs(&mut self) -> Vec<Box<dyn crate::views::dialog::Dialog>> {
        std::mem::take(&mut self.requested)
    }

    fn apply_dialog_outcome(
        &mut self,
        dialog: &'static str,
        outcome: &crate::views::dialog::DialogOutcome,
    ) -> EventResult {
        match outcome {
            crate::views::dialog::DialogOutcome::Selected { value, .. } => {
                self.adopt(dialog, value)
            }
            _ => EventResult::IGNORED,
        }
    }

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
        if self.handle_view_action(action).handled {
            return EventResult::REDRAW;
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
/// Every scope whose actions this screen can act on, plus `app` last.
///
/// A scope missing from this list is the quietest possible dead key: the binding table
/// has the row, the chord is spelled, [`SessionScreen::handle_action`] has an arm for
/// it — and [`crate::keybind::KeyDispatcher`] never resolves the press, because
/// resolution is scoped. The four pickers were unreachable for two independent reasons
/// at once, and this was the second one; a screen that handles an action must therefore
/// list the scope that action lives in.
#[must_use]
pub fn scopes() -> Vec<String> {
    [
        // `input` and `prompt` first, so a chord the editor claims wins over an
        // application-wide one on the same keys.
        "input", "prompt", "messages", "model", "agent", "session", "theme", "sidebar", "tool",
        "display", "tips", "command", "help",
        // `app` last, so `app_exit` still resolves while the prompt has focus.
        "app",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
