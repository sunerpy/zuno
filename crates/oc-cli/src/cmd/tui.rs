//! Booting the terminal application.
//!
//! `oc-tui` has had a working event loop and a full view layer since todos 73 and
//! 76, and nothing called [`oc_tui::app::App::run`]. This module is that call. It
//! owns only wiring: the terminal session, the two bounded channels, the input
//! producer, and the component tree's root. Every rendering decision stays in
//! `oc-tui`, and no engine call is reachable from here — the TUI consumes
//! [`oc_engine::r#loop::TurnEvent`] as data, which is what keeps rendering above
//! the turn loop.
//!
//! # Why a non-terminal invocation is refused rather than degraded
//!
//! Entering raw mode and the alternate screen on a pipe writes escape sequences
//! into whatever is reading it and leaves no way to type the key that exits. The
//! refusal names `run` because that is the surface a non-interactive caller wants,
//! and it is the same reason `run` refuses `--interactive`.
//!
//! # What this does not do yet
//!
//! Submitting a prompt does not start a turn. The turn driver needs a session, a
//! provider registry and a database resolved on the TUI's own thread, and giving
//! the screen a partial version of that would be worse than a screen that says so:
//! the prompt lands in the transcript, the status strip stays `idle`, and the
//! engine channel exists but nothing sends on it. `run` is the surface that
//! executes a turn today.

use std::io::IsTerminal as _;
use std::sync::Arc;

use oc_engine::r#loop::event_channel;
use oc_tui::app::{App, CrosstermDrawTarget, CrosstermLifecycle, TerminalSession};
use oc_tui::config::ResolvedTuiConfig;
use oc_tui::keybind::{KeyDispatcher, Keymap};
use oc_tui::views::ViewContext;
use oc_tui::views::dialog::DialogHost;
use oc_tui::views::message::Message;
use oc_tui::views::session::{SessionScreen, scopes};

use crate::command::TuiArgs;
use crate::environment::StartupEnvironment;

/// The greeting the transcript opens with, which states the surface's one limit.
const OPENING_NOTE: &str =
    "Interactive turns are not wired yet: use `opencode-rust run <message>` to execute one.";

pub(super) fn execute(_args: &TuiArgs, _environment: &StartupEnvironment) -> Result<(), String> {
    if !std::io::stdout().is_terminal() {
        return Err(
            "the interactive TUI requires a terminal; use `run <message>` for a \
             non-interactive turn"
                .to_owned(),
        );
    }

    let config = ResolvedTuiConfig::default();
    let keymap = Keymap::defaults().map_err(to_string)?;
    let context = ViewContext::defaults();
    let (terminal_sender, terminal_receiver) = oc_tui::app::terminal_event_channel();
    // Held for the application's lifetime: `App::run` treats a closed engine channel
    // as a producer that disappeared mid-run, which is a failure rather than an exit.
    let (engine_sender, engine_receiver) = event_channel();

    let mut screen = SessionScreen::new(context.clone(), terminal_sender.clone());
    screen
        .transcript_mut()
        .transcript_mut()
        .push(Message::user(OPENING_NOTE));
    let host = DialogHost::new(context, Box::new(screen));
    let root = KeyDispatcher::new(keymap, scopes(), Box::new(host));

    let lifecycle = Arc::new(CrosstermLifecycle::new(config.mouse));
    let target = CrosstermDrawTarget::new().map_err(to_string)?;
    let (mut app, _owner) = App::new(
        Box::new(root),
        Box::new(target),
        lifecycle.clone(),
        terminal_receiver,
        engine_receiver,
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(to_string)?;
    let session = TerminalSession::start(lifecycle).map_err(to_string)?;
    let outcome = runtime.block_on(async move {
        let input = tokio::spawn(oc_tui::app::forward_terminal_input(terminal_sender));
        let outcome = app.run().await;
        input.abort();
        outcome
    });
    drop(session);
    drop(engine_sender);
    outcome.map_err(to_string)
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}
