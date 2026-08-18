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
//! besides shutdown, and it is deliberately as thin as one: a typed submission out, and
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
use crate::views::autocomplete::{AutocompleteStep, AutocompleteView, SlashSource};
use crate::views::editor::{EditorSignal, InputEditor};
use crate::views::external::{Clipboard, EditorRequest, ExternalError, SystemClipboard};
use crate::views::message::{Message, StatusView, TranscriptView};
use crate::views::permission::typed_character;
use crate::views::scroll::Scroller;
use crate::views::slash::{CatalogCommand, HostCommand, SlashRouter, SlashSubmission};
use crossterm::event::{
    Event as CrosstermEvent, KeyEvent, KeyEventKind, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// The dialog id the skill browser reports under.
pub const SKILL_DIALOG_ID: &str = "prompt_skills";

/// Rows reserved for the status strip.
const STATUS_ROWS: u16 = 1;

/// The prompt's floor, and the share of the screen it may grow to.
///
/// Two rows is what the prompt occupied when its height was fixed, so a single-line
/// buffer keeps the proportions a user already knows rather than shrinking to one.
/// The cap is a third because the prompt is only ever half of a conversation: a
/// pasted diff allowed to take the whole height would evict the transcript it is
/// about to be sent against, and a prompt the user has to scroll is a smaller loss
/// than a reply they cannot see at all.
const PROMPT_MIN_ROWS: u16 = 2;
const PROMPT_MAX_SHARE: u16 = 3;

/// Rows the prompt gets for `content_lines` of typed text on a `height`-row screen.
///
/// One row more than the content so the line the cursor is about to open is already
/// on screen; below the floor that extra row is what the floor supplies anyway.
///
/// The floor is raised over the cap *before* clamping, and that ordering is the whole
/// reason this is a function. `height / PROMPT_MAX_SHARE` falls under
/// `PROMPT_MIN_ROWS` for any viewport shorter than six rows, and `u16::clamp` panics
/// when its minimum exceeds its maximum — so a naive
/// `wanted.clamp(PROMPT_MIN_ROWS, height / PROMPT_MAX_SHARE)` aborts the process on a
/// 20x10 terminal, which is a size a real pane reaches.
fn prompt_rows(content_lines: usize, height: u16) -> u16 {
    let wanted = u16::try_from(content_lines)
        .unwrap_or(u16::MAX)
        .saturating_add(1);
    let cap = (height / PROMPT_MAX_SHARE).max(PROMPT_MIN_ROWS);
    wanted.clamp(PROMPT_MIN_ROWS, cap)
}

/// The transcript, the status strip and the prompt as one screen.
pub struct SessionScreen {
    transcript: TranscriptView,
    status: StatusView,
    editor: InputEditor,
    autocomplete: AutocompleteView,
    slash: SlashRouter,
    welcome: crate::views::welcome::WelcomeView,
    sidebar: crate::views::ambient::SidebarView,
    shutdown: mpsc::Sender<TerminalEvent>,
    prompts: Option<mpsc::Sender<PromptSubmission>>,
    mcp_toggles: Option<mpsc::Sender<crate::views::picker::McpToggleRequest>>,
    mcp: crate::views::picker::McpProjection,
    cancels: Option<mpsc::Sender<()>>,
    /// Language-server reports produced beside the loop.
    ///
    /// Drained with `try_recv` inside `handle_event`, which is the same non-blocking
    /// shape the permission bridge uses: a receiver awaited here would stop the one loop
    /// that consumes terminal input, engine events and the lease wake.
    reports: Option<mpsc::Receiver<crate::views::lsp::Report>>,
    /// Where the set of files a finished turn wrote is sent for checking.
    edits: Option<mpsc::Sender<Vec<String>>>,
    /// The files the running turn has written so far.
    ///
    /// Accumulated from the same `ToolDispatchCompleted` events the transcript renders,
    /// so what is checked is exactly what the user was shown. A second listener wired
    /// separately could disagree with the screen about what happened.
    touched: Vec<String>,
    submissions: Vec<String>,
    cancellations: usize,
    cancel_requested: bool,
    sidebar_visible: bool,
    /// The resolved palette and configuration, for the pickers this screen builds.
    context: ViewContext,
    /// The theme showing when the theme picker opened, for escape to put back.
    ///
    /// A whole [`crate::theme::Resolved`] rather than a name, so restoring costs no
    /// second walk of the theme's colour references and cannot fall back to something
    /// else than what was on screen.
    ///
    /// Held here and not in the picker because the picker is gone by the time its
    /// cancellation is routed: [`crate::views::dialog::DialogHost`] pops the dialog and
    /// *then* tells the base.
    theme_restore: Option<Arc<crate::theme::Resolved>>,
    /// The user's resolved keymap, for the keybinding reference.
    ///
    /// Optional because every view test builds a screen without one, and a help view
    /// built from the shipped table instead would list the default spellings rather
    /// than the ones the user actually has.
    keymap: Option<crate::keybind::Keymap>,
    /// What the pickers offer, stated by the host.
    catalog: SessionCatalog,
    /// Dialogs asked for but not yet opened by the host.
    requested: Vec<Box<dyn crate::views::dialog::Dialog>>,
    /// Selections the user made, for a host that applies them to the next turn.
    selections: Option<mpsc::Sender<Selection>>,
    /// The dialog currently over this screen, as [`Self::observe_modal`] last saw it.
    ///
    /// Recorded only so a bracketed paste can be refused while a modal is up.
    /// [`crate::views::dialog::DialogHost`] forwards every *non-key* event to the base
    /// unconditionally — that single line is what keeps an open dialog from stalling
    /// the loop — and a paste is a non-key event, so without this the text would land
    /// in the prompt hidden behind a picker. That is the defect the host's own comment
    /// describes for keys: a modal owns the keyboard.
    modal: Option<&'static str>,
    /// The user's `scroll_speed` and `scroll_acceleration`, applied to wheel input.
    ///
    /// Held for the life of the screen rather than built per event, and that is the
    /// whole reason either key works. The curve is a function of the intervals
    /// *between* notches and the fractional carry is what survives a sub-row multiplier,
    /// so a scroller constructed inside `handle_event` would measure its first notch
    /// every time — reporting a multiplier of one forever, and rounding every
    /// `scroll_speed` under 1.0 to no movement at all. Nothing would fail loudly: a
    /// constant multiplier is a legal answer, so the defect would be invisible to any
    /// test that only asked whether the wheel moved something.
    scroller: Scroller,
    /// The monotonic origin wheel timestamps are measured from.
    ///
    /// A baseline plus an explicit `now_ms` parameter, rather than a clock read inside
    /// the curve, for the reason `KeyDispatcher::dispatch_key` takes its `Instant`: a
    /// streak that read the clock itself could only be tested by sleeping.
    started: Instant,
    /// Where [`EditorSignal::Copy`] puts the text.
    ///
    /// Injected, and `Arc` rather than `Box`, so a test can hold the same
    /// [`crate::views::external::MemoryClipboard`] the screen writes through and read it
    /// back afterwards. A process-global would make the assertion "did the copy land"
    /// order-dependent across the suite, which is the reason every other collaborator
    /// here is a field too.
    clipboard: Arc<dyn Clipboard>,
    editor_requests: Option<mpsc::Sender<EditorRequest>>,
    editor_results: Option<mpsc::Receiver<Result<Option<String>, ExternalError>>>,
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

/// A prompt-channel message. Catalog invocations stay typed until the CLI host
/// resolves their templates; plain text goes directly to the turn driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSubmission {
    /// Ordinary model input.
    Text(String),
    /// Model input after the host resolved one or more `@` references.
    ///
    /// Kept on the prompt channel rather than a parallel attachment channel so the text
    /// and its blocks cannot be reordered across turns. `text` is the user-authored form
    /// retained for hooks and diagnostics; `content` is what the provider receives.
    Content {
        text: String,
        content: Vec<zuno_llm::event::RequestContentBlock>,
    },
    /// A catalog command plus its still-unexpanded argument tail.
    Command { name: String, arguments: String },
    /// A session-local operation executed by the runtime host.
    Host(HostCommand),
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
        let slash = SlashRouter::default();
        Self {
            transcript: TranscriptView::new(context.clone()),
            status: StatusView::new(context.clone()),
            welcome: crate::views::welcome::WelcomeView::new(context.clone()),
            sidebar: crate::views::ambient::SidebarView::new(context.clone()),
            editor: InputEditor::new(context.clone()),
            autocomplete: AutocompleteView::new(
                context.clone(),
                Box::new(SlashSource::new(slash.clone())),
            ),
            slash,
            shutdown,
            prompts: None,
            mcp_toggles: None,
            mcp: crate::views::picker::McpProjection::default(),
            cancels: None,
            reports: None,
            edits: None,
            touched: Vec::new(),
            submissions: Vec::new(),
            cancellations: 0,
            cancel_requested: false,
            sidebar_visible: true,
            keymap: None,
            catalog: SessionCatalog::default(),
            requested: Vec::new(),
            selections: None,
            theme_restore: None,
            modal: None,
            scroller: Scroller::new(&context.config),
            started: Instant::now(),
            // The real host clipboard, so a copy works in production without the CLI
            // constructing anything: `SystemClipboard::host` resolves the platform, the
            // installed programs and whether stdout is a terminal, and yields a
            // clipboard with no mechanisms when there is no terminal — which is also
            // what keeps the suite from spawning `xclip` or painting escape sequences
            // into captured test output. `with_clipboard` replaces it.
            clipboard: Arc::new(SystemClipboard::host()),
            editor_requests: None,
            editor_results: None,
            // Last, because the two fields above borrow it and a struct literal
            // evaluates its fields in written order.
            context,
        }
    }

    /// Install prompts a previous run submitted, and record new ones to `records`.
    ///
    /// The entries and the sink arrive together because they are two halves of one
    /// feature, and the host supplies both: `zuno-tui` names the file
    /// ([`crate::views::editor::PROMPT_HISTORY_FILE`]) but resolves no directory, so
    /// the reading and the writing both live in `crates/zuno-cli/src/cmd/tui.rs`.
    ///
    /// Only prompts typed into this editor are recorded. A prompt supplied on the
    /// command line goes through [`Self::submit_prompt`], which never touches the
    /// editor — it was not typed here, and treating it as though it were would put an
    /// unattended invocation into the list a user walks back with.
    #[must_use]
    pub fn with_prompt_history(
        mut self,
        entries: Vec<String>,
        records: mpsc::Sender<String>,
    ) -> Self {
        self.editor.load_history(entries);
        self.editor.record_history_to(records);
        self
    }

    /// Send copied text somewhere other than the host's own clipboard.
    ///
    /// Optional for the reason every other collaborator here is: the default already
    /// works, and a test needs a clipboard it can read back.
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Arc<dyn Clipboard>) -> Self {
        self.clipboard = clipboard;
        self
    }

    /// Connect external-editor requests to a host worker and receive its results.
    #[must_use]
    pub fn with_external_editor(
        mut self,
        requests: mpsc::Sender<EditorRequest>,
        results: mpsc::Receiver<Result<Option<String>, ExternalError>>,
    ) -> Self {
        self.editor_requests = Some(requests);
        self.editor_results = Some(results);
        self
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
    pub fn with_prompt_sink(mut self, prompts: mpsc::Sender<PromptSubmission>) -> Self {
        self.prompts = Some(prompts);
        self
    }

    /// Install the live MCP projection and non-blocking lifecycle request sink.
    #[must_use]
    pub fn with_mcp_control(
        mut self,
        projection: crate::views::picker::McpProjection,
        toggles: mpsc::Sender<crate::views::picker::McpToggleRequest>,
    ) -> Self {
        self.mcp = projection;
        self.mcp_toggles = Some(toggles);
        self
    }

    /// Install host-projected catalog metadata without importing the catalog crate.
    #[must_use]
    pub fn with_slash_commands(
        mut self,
        commands: impl IntoIterator<Item = CatalogCommand>,
    ) -> Self {
        self.slash = SlashRouter::new(commands);
        self.autocomplete
            .set_source(Box::new(SlashSource::new(self.slash.clone())));
        self
    }

    /// Install the host's `@` candidates without teaching this leaf crate about filesystems.
    ///
    /// Completion is called from the keystroke path while the UI state is locked, so the
    /// implementation supplied here must already be bounded and must not perform a walk.
    /// The production CLI satisfies that contract with a capped index built before raw mode;
    /// tests keep using [`crate::views::autocomplete::StaticSource`].
    #[must_use]
    pub fn with_reference_source(
        mut self,
        source: Box<dyn crate::views::autocomplete::CompletionSource>,
    ) -> Self {
        self.autocomplete.set_reference_source(source);
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

    /// Report the files each finished turn wrote, for checking.
    #[must_use]
    pub fn with_edit_sink(mut self, edits: mpsc::Sender<Vec<String>>) -> Self {
        self.edits = Some(edits);
        self
    }

    /// The tools whose completion means a file on disk changed.
    ///
    /// `read` is deliberately absent: reporting the pre-existing diagnostics of a file
    /// the model only read would attribute somebody else's problem to this turn.
    pub const WRITING_TOOLS: [&'static str; 3] = ["edit", "write", "patch"];

    /// Note a written file, and hand the batch over when the turn ends.
    fn observe_edits(&mut self, event: &AppEvent) {
        let AppEvent::Engine(turn) = event else {
            return;
        };
        match turn {
            zuno_engine::r#loop::TurnEvent::ToolDispatchCompleted {
                name,
                title,
                is_error,
                ..
            } => {
                // A failed write changed nothing, so its diagnostics would describe the
                // file as it already was.
                if !*is_error
                    && Self::WRITING_TOOLS.contains(&name.as_str())
                    && !title.trim().is_empty()
                    && !self.touched.iter().any(|seen| seen == title.trim())
                {
                    self.touched.push(title.trim().to_owned());
                }
            }
            zuno_engine::r#loop::TurnEvent::TurnCompleted { .. }
            | zuno_engine::r#loop::TurnEvent::TurnInterrupted { .. } => {
                if self.touched.is_empty() {
                    return;
                }
                let batch = std::mem::take(&mut self.touched);
                if let Some(edits) = self.edits.as_ref() {
                    // `try_send` for the reason every sink here uses it: a full channel
                    // costs a check, never a stalled render loop.
                    let _sent = edits.try_send(batch);
                }
            }
            _ => {}
        }
    }

    /// Take language-server reports from `reports` as they arrive.
    ///
    /// Optional for the same reason the other two sinks are: a screen with no host has
    /// nothing querying language servers, and a receiver it could never be fed would be
    /// worse than none.
    #[must_use]
    pub fn with_diagnostics_source(
        mut self,
        reports: mpsc::Receiver<crate::views::lsp::Report>,
    ) -> Self {
        self.reports = Some(reports);
        self
    }

    /// Drain every report that has arrived.
    fn drain_reports(&mut self) -> EventResult {
        let mut drained = Vec::new();
        if let Some(reports) = self.reports.as_mut() {
            while let Ok(report) = reports.try_recv() {
                drained.push(report);
            }
        }
        if drained.is_empty() {
            return EventResult::IGNORED;
        }
        for report in drained {
            self.report_diagnostics(report);
        }
        EventResult::REDRAW
    }

    fn drain_editor_results(&mut self) -> EventResult {
        let mut drained = Vec::new();
        if let Some(results) = self.editor_results.as_mut() {
            while let Ok(result) = results.try_recv() {
                drained.push(result);
            }
        }
        if drained.is_empty() {
            return EventResult::IGNORED;
        }
        for result in drained {
            match result {
                Ok(Some(text)) => self.editor.set_text(&text),
                Ok(None) => {}
                Err(error) => self
                    .transcript
                    .transcript_mut()
                    .push(Message::notice(format!("external editor failed: {error}"))),
            }
        }
        EventResult::REDRAW
    }

    fn request_external_editor(&mut self) -> EventResult {
        let Some(requests) = self.editor_requests.as_ref() else {
            self.transcript
                .transcript_mut()
                .push(Message::notice("external editor is unavailable"));
            return EventResult::REDRAW;
        };
        let request = EditorRequest::new(self.editor.text());
        if let Err(error) = requests.try_send(request) {
            let reason = match error {
                mpsc::error::TrySendError::Full(_) => "an external editor is already running",
                mpsc::error::TrySendError::Closed(_) => "the external editor worker has stopped",
            };
            self.transcript
                .transcript_mut()
                .push(Message::notice(reason));
        }
        EventResult::REDRAW
    }

    /// Append a language-server report to the transcript.
    ///
    /// A method rather than letting the host reach through `transcript_mut` because the
    /// report should also reach the status strip, and a host that pushed the message
    /// itself would have to remember to do both.
    pub fn report_diagnostics(&mut self, report: crate::views::lsp::Report) {
        self.status.set_diagnostics(report.summary());
        self.transcript
            .transcript_mut()
            .push(Message::diagnostics(report));
    }

    /// The transcript, for a host that appends locally composed messages.
    pub const fn transcript_mut(&mut self) -> &mut TranscriptView {
        &mut self.transcript
    }

    /// The welcome screen, for the host that resolves the facts it states.
    pub const fn welcome_mut(&mut self) -> &mut crate::views::welcome::WelcomeView {
        &mut self.welcome
    }

    /// Supply the resolved keymap the keybinding reference is built from.
    #[must_use]
    pub fn with_keymap(mut self, keymap: crate::keybind::Keymap) -> Self {
        self.keymap = Some(keymap);
        self
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
        match self.slash.resolve(&text) {
            SlashSubmission::Prompt(prompt) => {
                self.submit_to_driver(prompt.clone(), PromptSubmission::Text(prompt))
            }
            SlashSubmission::UiAction(action) => {
                self.dispatch_action(action);
            }
            SlashSubmission::Catalog { command, arguments } => self.submit_to_driver(
                text,
                PromptSubmission::Command {
                    name: command,
                    arguments,
                },
            ),
            SlashSubmission::Host(command) => {
                self.submit_to_driver(text, PromptSubmission::Host(command));
            }
            SlashSubmission::Unknown(name) => {
                let shown = if name.is_empty() {
                    String::from("/")
                } else {
                    format!("/{name}")
                };
                self.transcript
                    .transcript_mut()
                    .push(Message::notice(format!(
                        "unknown command `{shown}`; type `/` to browse commands or press ctrl+p"
                    )));
            }
        }
    }

    fn submit_to_driver(&mut self, shown: String, submission: PromptSubmission) {
        self.transcript
            .transcript_mut()
            .push(Message::user(shown.clone()));
        if let Some(prompts) = self.prompts.as_ref() {
            match prompts.try_send(submission) {
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
        self.submissions.push(shown);
    }

    fn refresh_autocomplete(&mut self) {
        let text = self.editor.text();
        let cursor = self.editor.cursor();
        let before = text
            .split('\n')
            .take(cursor.line)
            .map(|line| line.chars().count() + 1)
            .sum::<usize>()
            .saturating_add(cursor.column);
        self.autocomplete.refresh(&text, before);
    }

    fn complete_autocomplete(&mut self) -> EventResult {
        let text = self.editor.text();
        let Some((completed, cursor)) = self.autocomplete.complete(&text) else {
            return EventResult::IGNORED;
        };
        self.editor.apply_completion(&completed, cursor);
        self.refresh_autocomplete();
        EventResult::REDRAW
    }

    fn autocomplete_step(&mut self, action: &'static str) -> EventResult {
        let Some(definition) = crate::keybind::definition(action) else {
            return EventResult::IGNORED;
        };
        match self.autocomplete.handle_action(definition) {
            AutocompleteStep::Ignored => EventResult::IGNORED,
            AutocompleteStep::Redraw => EventResult::REDRAW,
            AutocompleteStep::Complete => self.complete_autocomplete(),
        }
    }

    /// Put `text` on the clipboard, and say in the transcript what happened.
    ///
    /// Both outcomes are reported, not just the failure. A copy key that paints nothing
    /// teaches the user the binding is broken, so "it worked" and "it did not" have to
    /// be told apart on screen — the same reason [`Self::submit`] reports a refused
    /// prompt and [`Self::adopt`] reports a selection nothing listened to.
    ///
    /// The transcript rather than the status strip: a copy is one event, and the strip
    /// carries state that persists — a notice pinned there would still be claiming a
    /// copy minutes later.
    /// Put `text` into the prompt, and submit nothing.
    ///
    /// Submitting nothing is the whole behaviour being bought here. A real-terminal
    /// session before this existed turned an eight-line paste into eight turns and
    /// filled the transcript with `not sent: a turn is already running`, because
    /// without bracketed paste each newline was a separate key that resolved to
    /// `input_submit`.
    fn paste(&mut self, text: &str) -> EventResult {
        if let Some(dialog) = self.modal {
            // Refused rather than swallowed silently: a picker's filter box cannot take
            // pasted text — `Dialog::handle_typed` receives a key, not a string — and a
            // paste that vanished with nothing said is indistinguishable from a broken
            // terminal. The notice is behind the dialog and reads once it closes, which
            // is when the user can act on it.
            self.transcript
                .transcript_mut()
                .push(Message::notice(format!(
                    "paste ignored: `{dialog}` is open and owns the keyboard"
                )));
            return EventResult::REDRAW;
        }
        if self.editor.insert_paste(text) == EditorSignal::None {
            return EventResult::IGNORED;
        }
        self.refresh_autocomplete();
        EventResult::REDRAW
    }

    /// Insert whatever the clipboard holds, or say why it could not be read.
    ///
    /// The `input_paste` binding, for terminals that deliver a paste chord as an
    /// ordinary key rather than as a bracketed paste. A bracketed paste arrives as an
    /// event and never reaches here.
    ///
    /// Reporting the refusal is the point, and it is what makes
    /// [`Clipboard::read`]'s deliberate error worth returning: the binding used to fall
    /// into a bare redraw, so pressing it did nothing and said nothing.
    fn paste_from_clipboard(&mut self) -> EventResult {
        let notice = match self.clipboard.read() {
            Ok(Some(content)) if content.is_image() => String::from(
                "the clipboard holds an image; pasting an attachment is not supported yet",
            ),
            Ok(Some(content)) => return self.paste(&content.data),
            Ok(None) => String::from("nothing to paste: the clipboard is empty"),
            Err(error) => format!("paste failed: {error}"),
        };
        self.transcript
            .transcript_mut()
            .push(Message::notice(notice));
        EventResult::REDRAW
    }

    fn copy(&mut self, text: String) -> EventResult {
        // An empty buffer with nothing selected is not a copy, and writing the empty
        // string would destroy whatever the user already had on their clipboard.
        let notice = if text.is_empty() {
            String::from("nothing to copy: the prompt is empty and no text is selected")
        } else {
            match self.clipboard.write(&text) {
                Ok(()) => format!(
                    "copied {} characters to the clipboard",
                    text.chars().count()
                ),
                Err(error) => format!("copy failed: {error}"),
            }
        };
        self.transcript
            .transcript_mut()
            .push(Message::notice(notice));
        EventResult::REDRAW
    }
}

impl Component for SessionScreen {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.sidebar.ambient_mut().mcp = self
            .mcp
            .snapshot()
            .iter()
            .map(crate::views::picker::McpServer::service)
            .collect();
        let [body, status, prompt] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(STATUS_ROWS),
            Constraint::Length(prompt_rows(self.editor.height(), area.height)),
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
        if self.autocomplete.is_open() {
            let height = self.autocomplete.height().min(main.height);
            let overlay = Rect::new(
                main.x,
                main.y + main.height.saturating_sub(height),
                main.width,
                height,
            );
            self.autocomplete.render(frame, overlay);
        }
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        // A bracketed paste is one event carrying the whole block, so it goes straight
        // to the editor and resolves to no action at all. That is the point: before
        // bracketed paste was enabled the same paste arrived as individual keys, and
        // every newline among them resolved to `input_submit`.
        if let AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Paste(text))) = event {
            return self.paste(text);
        }
        // A printable key resolves to no action, so the dispatcher forwards it here
        // and the screen is what routes it into the prompt. Without this the editor
        // could not be typed into at all — see `permission::typed_character`, the
        // same seam the reject box uses.
        if let AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Key(key))) = event
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && let Some(character) = typed_character(key)
        {
            self.editor.insert_char(character);
            self.refresh_autocomplete();
            return EventResult::REDRAW;
        }
        // A wheel notch is the one terminal event the transcript acts on. Merged rather
        // than returned early, so the drain below still runs on a scroll — see its
        // comment: an event that skips it can be the last event the loop ever sees.
        let wheel = match event {
            AppEvent::Terminal(TerminalEvent::Input(CrosstermEvent::Mouse(mouse))) => {
                self.handle_wheel(mouse, self.now_ms())
            }
            _ => EventResult::IGNORED,
        };
        // Drained on every event rather than only on a wake, for the reason the
        // permission bridge pumps on every event: a dropped nudge must not leave a
        // verdict the user is waiting for sitting in a channel forever.
        self.observe_edits(event);
        wheel
            .merge(self.drain_editor_results())
            .merge(self.drain_reports())
            .merge(self.transcript.handle_event(event))
            .merge(self.status.handle_event(event))
    }
}

impl SessionScreen {
    fn mark_turn_accepted(&mut self) {
        self.cancel_requested = false;
        self.status.mark_running();
    }

    /// Milliseconds since this screen was built.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Scroll the transcript by one wheel notch observed at `now_ms`.
    ///
    /// Wheel input only. The `messages_*` actions in [`Self::handle_view_action`] keep
    /// moving whole rows, unaccelerated, because a line the user asked for by name must
    /// not become four just because they pressed the key quickly — acceleration is a
    /// property of a continuous gesture, not of a deliberate step.
    ///
    /// No hit-testing against the pointer's position: the transcript is the only
    /// scrollable region on this screen, so a notch anywhere means the transcript, the
    /// same way `messages_line_up` does not care where the pointer is.
    fn handle_wheel(&mut self, mouse: &MouseEvent, now_ms: u64) -> EventResult {
        let notches = match mouse.kind {
            MouseEventKind::ScrollUp => -1.0,
            MouseEventKind::ScrollDown => 1.0,
            // Horizontal wheels, buttons and drags: the transcript has one axis, and a
            // screen that claimed the rest would stop a later surface from seeing them.
            _ => return EventResult::IGNORED,
        };
        // Re-stated per notch from the transcript, which measured all three on its last
        // render and is the only thing that owns them. This is what keeps the wheel from
        // drifting away from the view while a live turn grows the content underneath it.
        self.scroller.total = self.transcript.content_height();
        self.scroller.viewport = self.transcript.viewport_height();
        self.scroller.sync_offset(self.transcript.offset());
        if self.scroller.wheel(notches, now_ms) == 0 {
            // A notch whose multiplier has not yet accumulated a whole row moved
            // nothing, so repainting would cost a frame to redraw identical rows.
            return EventResult::IGNORED;
        }
        self.transcript.set_offset(self.scroller.offset());
        EventResult::REDRAW
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
            // Two statements because opening the theme picker also records the theme to
            // put back on escape, which needs `&mut self` while `request` does too.
            "theme_list" => {
                let dialog = self.theme_picker();
                self.request(dialog)
            }
            "mcp_list" => self.request(self.mcp_list()),
            "prompt_skills" => self.request(self.skill_list()),
            "diff_open" => self.request(self.diff_view()),
            "help_show" => self.request(self.help_view()),
            "command_list" => self.request(self.command_palette()),
            _ => EventResult::IGNORED,
        }
    }

    /// The command palette.
    ///
    /// Always available, and that is the point: forty-three rows of the binding table ship
    /// with `keys: "none"`, faithfully to upstream, and upstream's answer for reaching one
    /// is the palette. Without it a third of the table is unreachable by any means —
    /// `command_list` was itself on the welcome screen's hint list, bound to `ctrl+p`, and
    /// reached nothing, which is why it was removed from that list rather than wired.
    fn command_palette(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        // The keymap rather than the shipped table, so the spelling each row shows is the
        // one the user would actually press. Without a keymap there is nothing honest to
        // print, and `request` says so instead of guessing.
        let keymap = self.keymap.as_ref()?;
        Some(Box::new(crate::views::palette::palette(
            self.context.clone(),
            keymap,
        )))
    }

    /// Run the action a palette row named.
    ///
    /// Guarded against the palette naming itself, which would push a second palette over
    /// the first and leave one behind on every later choice.
    fn dispatch_action(&mut self, action: &str) -> EventResult {
        if action == "command_list" {
            return EventResult::IGNORED;
        }
        let Some(definition) = crate::keybind::definition(action) else {
            return EventResult::IGNORED;
        };
        // A synthetic event with no key: the two readers both fall back to the action name.
        // `handle_action` checks `APP_EXIT` before asking whether the chord is an exit
        // chord, and `typed_character` yields nothing for a null key — correct here,
        // because a palette choice is not a typed character.
        let event = KeyEvent::new(
            crossterm::event::KeyCode::Null,
            crossterm::event::KeyModifiers::NONE,
        );
        self.handle_action(definition, &event)
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

    fn mcp_list(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        if self.mcp.is_empty() {
            return None;
        }
        Some(Box::new(crate::views::picker::mcp_list(
            self.context.clone(),
            self.mcp.clone(),
        )))
    }

    fn skill_list(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        let skills = self.sidebar.ambient().skills.clone();
        if skills.is_empty() {
            return None;
        }
        Some(Box::new(crate::views::picker::skill_list(
            self.context.clone(),
            skills,
        )))
    }

    /// The most recent patch a tool reported, as a scrollable diff.
    ///
    /// Read back out of the transcript rather than accumulated separately: the transcript
    /// already recognises a unified diff in tool output — see
    /// [`crate::views::message::looks_like_diff`] — so a second collector could disagree
    /// with what is on screen. Absent when no tool has produced one, which the caller
    /// reports rather than opening an empty viewer.
    fn diff_view(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        let patch = self.transcript.transcript().latest_diff()?;
        Some(Box::new(crate::views::diff::DiffDialog::new(
            self.context.clone(),
            &patch,
        )))
    }

    /// The keybinding reference, when the host supplied the keymap to build it from.
    ///
    /// A help view lists what the *user's* keymap resolved, so it cannot be built from
    /// the shipped table alone; without the keymap it would advertise defaults the user
    /// may have rebound. Absent rather than wrong: the key then reports "nothing to show"
    /// instead of printing a table of keys that do not work.
    fn help_view(&self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        let keymap = self.keymap.as_ref()?;
        Some(Box::new(crate::views::help::HelpView::new(
            self.context.clone(),
            keymap,
        )))
    }

    /// The theme picker, and the restore point escape needs.
    ///
    /// Moving the cursor in this picker re-themes the screen immediately — see
    /// [`crate::views::ViewContext::set_theme`] — so the theme showing when it opened is
    /// recorded here first. Without it, cancelling would leave the user in whichever
    /// theme they happened to be scrolling past, which is the one outcome they did not
    /// ask for.
    fn theme_picker(&mut self) -> Option<Box<dyn crate::views::dialog::Dialog>> {
        // The registry is built here rather than held, because the picker resolves every
        // theme once at construction for its preview and then never consults it again.
        let registry = crate::theme::ThemeRegistry::new();
        let active = self.context.theme();
        self.theme_restore = Some(Arc::clone(&active));
        Some(Box::new(crate::views::picker::theme_picker(
            self.context.clone(),
            &registry,
            // The mode the host resolved at startup, carried on the active theme rather
            // than re-decided here. A second mode policy in this crate would preview
            // dark variants on a terminal the CLI had already found to be light.
            active.mode,
        )))
    }

    /// Put back the theme the picker opened over.
    fn restore_theme(&mut self) -> EventResult {
        let Some(previous) = self.theme_restore.take() else {
            return EventResult::IGNORED;
        };
        self.context.set_theme(&previous);
        EventResult::REDRAW
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
            // No [`Selection::Theme`] is sent, and that is the change. The variant stays
            // because the host still matches on it, but a theme is the view layer's own
            // state now: the palette on screen is already the chosen one — the picker's
            // highlight hook applied it as the cursor arrived — so committing only has to
            // drop the restore point. Sending it would put a colour change through the
            // channel that rebuilds the turn host, and would earn the "not applied:
            // nothing is listening" notice from a host that deliberately discards it,
            // which would be a lie about a theme that visibly did apply.
            crate::views::picker::THEME_DIALOG_ID => {
                self.theme_restore = None;
                // The resolved name, not `value`: a theme that fell back is showing the
                // fallback, and the notice should say what the user is looking at.
                let name = self.context.theme().name.clone();
                self.transcript
                    .transcript_mut()
                    .push(Message::notice(format!("theme set to {name}")));
                return EventResult::REDRAW;
            }
            // The palette resolves to *another action's name*, so it re-enters the same
            // routing a key press takes. That is what makes an unbound action reachable
            // without a second copy of the routing table. Re-entry is bounded because the
            // palette is excluded from what it can dispatch.
            crate::views::palette::DIALOG_ID => return self.dispatch_action(value),
            SKILL_DIALOG_ID => {
                self.transcript
                    .transcript_mut()
                    .push(Message::notice(format!(
                        "skill `{value}` — name it in a prompt to invoke it"
                    )));
                return EventResult::REDRAW;
            }
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
    fn focused_scopes(&self) -> Vec<&'static str> {
        if self.autocomplete.is_open() {
            vec!["prompt.autocomplete"]
        } else if self.editor.cursor().line == 0
            || self.editor.cursor().line + 1 == self.editor.height()
        {
            // Scope ordering cannot vary by chord, so both history arrows are promoted at
            // either vertical edge. `InputEditor` then applies the directional half of the
            // rule: an arrow pointing into a multi-line buffer still moves the cursor, while
            // one pointing out past its first/last line walks history. Promoting everywhere
            // would shadow `input_move_up/down` throughout pasted blocks; never promoting is
            // the original bug that made persisted history unreachable.
            vec!["history"]
        } else {
            Vec::new()
        }
    }

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
            // Escape arrives as a cancelled outcome through the same routing a selection
            // takes, so no key is named here — which is the discipline this layer keeps.
            crate::views::dialog::DialogOutcome::Cancelled
                if dialog == crate::views::picker::THEME_DIALOG_ID =>
            {
                self.restore_theme()
            }
            crate::views::dialog::DialogOutcome::McpToggle(request) => {
                let delivered = self
                    .mcp_toggles
                    .as_ref()
                    .is_some_and(|sink| sink.try_send(request.clone()).is_ok());
                if !delivered {
                    self.transcript
                        .transcript_mut()
                        .push(Message::notice(format!(
                            "MCP server `{}` was not toggled: lifecycle worker is busy or unavailable",
                            request.server
                        )));
                }
                EventResult::REDRAW
            }
            _ => EventResult::IGNORED,
        }
    }

    fn observe_modal(&mut self, active: Option<&'static str>) {
        self.modal = active;
        // Only a permission prompt makes the turn wait on the user; a picker or the help
        // view is something the user opened *while* work continued, and suppressing the
        // spinner behind those would claim the turn had stopped when it had not.
        let awaiting = active == Some(crate::views::permission::DIALOG_ID);
        // Both surfaces, from one answer. The transcript's spinner is only on screen once
        // a turn has produced a message — before that the welcome surface has the area and
        // the strip is the only row saying anything about state, so fixing one and not the
        // other leaves the claim on whichever surface the user is actually looking at.
        self.transcript
            .transcript_mut()
            .set_awaiting_permission(awaiting);
        self.status.set_awaiting_permission(awaiting);
    }

    fn handle_action(&mut self, action: &'static Definition, event: &KeyEvent) -> EventResult {
        if self.autocomplete.is_open() {
            let autocomplete_action = match action.name {
                "prompt.autocomplete.prev"
                | "prompt.autocomplete.next"
                | "prompt.autocomplete.hide"
                | "prompt.autocomplete.select"
                | "prompt.autocomplete.complete" => Some(action.name),
                "input_submit" | "prompt_submit" => Some("prompt.autocomplete.select"),
                "input_move_up" | "command_list" => Some("prompt.autocomplete.prev"),
                "input_move_down" => Some("prompt.autocomplete.next"),
                "session_interrupt" => Some("prompt.autocomplete.hide"),
                _ => None,
            };
            if let Some(autocomplete_action) = autocomplete_action {
                return self.autocomplete_step(autocomplete_action);
            }
        }
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
                self.autocomplete.hide();
                EventResult::REDRAW
            }
            EditorSignal::Copy(text) => self.copy(text),
            EditorSignal::OpenExternalEditor => self.request_external_editor(),
            EditorSignal::Changed => {
                self.refresh_autocomplete();
                EventResult::REDRAW
            }
            EditorSignal::Paste => self.paste_from_clipboard(),
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
        "input", "prompt",
        // `history` stays after `input` in the static chain, preserving the rule above that
        // editor bindings win ordinary collisions. Its complete scope is only `up` and
        // `down`, so registering it cannot consume a typeable character. At a buffer's
        // vertical edge `focused_scopes` temporarily promotes it; the editor then decides
        // from direction whether that arrow moves inward or crosses into history.
        "history",
        // `editor` with them, because `editor_open` *is* a prompt action — its command is
        // `prompt.editor` and it opens `$EDITOR` on the buffer the prompt owns — so it
        // belongs beside the family above rather than among the surfaces below.
        //
        // Safe at any position, which is the part worth stating. The scope carries exactly
        // one row, `editor_open` on `<leader>e`, and no other row in the table spells
        // `<leader>e`, so this cannot take a chord from a scope before or after it. It also
        // cannot do what `diff` below does: a leader sequence opens with `ctrl+x`, which no
        // text entry produces, so registering this scope costs no typeable character.
        //
        // Unregistered it was the quietest possible dead key: `ctrl+x` resolved to
        // `Pending`, the `e` then matched nothing, fell through to the editor and was
        // inserted — `ctrl+x e` left `beforee` in the prompt, and the contained-editor
        // stack behind it could not be opened by any means.
        "editor", "messages", "model", "agent", "session", "theme", "sidebar", "mcp", "tool",
        "display", "tips", "command", "help",
        // `diff` after `input` and `messages`, and only for `diff_open`'s sake. The scope
        // also carries the viewer's own bare letters — `q`, `n`, `p`, `d`, `v`, `s`, `b`,
        // `[`, `]` — which resolve here whether or not the viewer is open. That is
        // survivable, and only because of two facts together: this screen returns
        // `IGNORED` for every diff action except `diff_open`, and an unhandled action
        // falls through to the editor, which inserts the character. Give this screen an
        // arm for one of those letters and the letter stops being typeable.
        "diff", // `app` last, so `app_exit` still resolves while the prompt has focus.
        "app",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
