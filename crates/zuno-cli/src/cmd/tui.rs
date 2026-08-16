//! Booting the terminal application, and driving a turn from it.
//!
//! `zuno-tui` has had a working event loop and a full view layer since todos 73 and
//! 76, and nothing called [`zuno_tui::app::App::run`]. This module is that call. It
//! owns only wiring: the terminal session, the channels, the input producer, the
//! component tree's root, and the task that turns a submitted prompt into a turn.
//! Every rendering decision stays in `zuno-tui`, and the turn's composition stays in
//! [`super::turn`] — this module resolves neither.
//!
//! # Why a non-terminal invocation is refused rather than degraded
//!
//! Entering raw mode and the alternate screen on a pipe writes escape sequences
//! into whatever is reading it and leaves no way to type the key that exits. The
//! refusal names `run` because that is the surface a non-interactive caller wants,
//! and it is the same reason `run` refuses `--interactive`.
//!
//! # The turn runs beside the loop, never inside it
//!
//! A prompt leaves the screen as a `String` on a bounded channel; a task with the
//! only [`super::turn::TurnHost`] picks it up and drives the turn, publishing
//! [`zuno_engine::r#loop::TurnEvent`]s on the channel the application already
//! consumes. Nothing about that is an optimisation: the loop is the only consumer of
//! terminal input, engine events **and** the terminal-lease wake, so a turn awaited
//! inside a component handler would stop all three and deadlock against a plugin's
//! terminal lease.
//!
//! The prompt channel holds exactly one message, which is what makes a second
//! submission while a turn is running a visible refusal in the transcript rather
//! than a silently queued turn.
//!
//! # Everything that can fail is resolved before raw mode
//!
//! [`super::turn::TurnPlan::resolve`] and [`super::turn::TurnHost::open`] both run
//! before [`zuno_tui::app::TerminalSession::start`]. An error printed into a raw-mode
//! alternate screen that is about to be torn down is an error nobody reads.

use std::io::IsTerminal as _;
use std::sync::Arc;

use tokio::sync::mpsc;
use zuno_engine::r#loop::{TurnEvent, TurnEventSender, event_channel};
use zuno_llm::event::StreamEvent;
use zuno_tool::PermissionAsker;
use zuno_tui::app::{App, CrosstermDrawTarget, CrosstermLifecycle, TerminalSession};
use zuno_tui::config::ResolvedTuiConfig;
use zuno_tui::keybind::{KeyDispatcher, Keymap};
use zuno_tui::views::ViewContext;
use zuno_tui::views::dialog::DialogHost;
use zuno_tui::views::session::{SessionScreen, scopes};

use super::tui_permission::{AutoApproval, PermissionBridge, PermissionBroker};
use super::turn::{SessionChoice, TurnHost, TurnOptions, TurnPlan};
use crate::command::TuiArgs;
use crate::environment::StartupEnvironment;

/// How many prompts may be in flight. One, so a second is refused and not queued.
const PROMPT_CHANNEL_CAPACITY: usize = 1;

pub(super) fn execute(args: &TuiArgs, environment: &StartupEnvironment) -> Result<(), String> {
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
    let (terminal_sender, terminal_receiver) = zuno_tui::app::terminal_event_channel();
    let (engine_sender, engine_receiver) = event_channel();
    let (prompt_sender, prompt_receiver) = mpsc::channel(PROMPT_CHANNEL_CAPACITY);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(to_string)?;
    let options = TurnOptions {
        directory: None,
        model: args.model.clone(),
        agent: args.agent.clone(),
        session: SessionChoice::resolve(args.session.as_deref(), args.r#continue),
        title: None,
    };
    let plan = runtime.block_on(TurnPlan::resolve(&options, environment))?;
    let tui_plugins = runtime.block_on(plan.load_tui_plugins(environment));
    let broker = Arc::new(PermissionBroker::new(terminal_sender.clone()));
    let approval: Arc<dyn PermissionAsker> = if args.auto {
        Arc::new(AutoApproval)
    } else {
        Arc::clone(&broker) as Arc<dyn PermissionAsker>
    };
    let host = TurnHost::open(plan, environment, approval)?;
    let engine_sender = host.with_event_hooks(engine_sender);
    let plugins = host.plugin_runtime();
    broker.bind_session(host.session_id());

    let mut screen = SessionScreen::new(context.clone(), terminal_sender.clone())
        .with_prompt_sink(prompt_sender);
    if let Some(prompt) = args
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
    {
        screen.submit_prompt(prompt);
    }
    let dialogs = DialogHost::new(context.clone(), Box::new(screen));
    let bridge = PermissionBridge::new(context, broker, dialogs);
    let root = KeyDispatcher::new(keymap, scopes(), Box::new(bridge));

    let lifecycle = Arc::new(CrosstermLifecycle::new(config.mouse));
    let target = CrosstermDrawTarget::new().map_err(to_string)?;
    let (mut app, _owner) = App::new(
        Box::new(root),
        Box::new(target),
        lifecycle.clone(),
        terminal_receiver,
        engine_receiver,
    );

    let session = TerminalSession::start(lifecycle).map_err(to_string)?;
    let outcome = runtime.block_on(async move {
        let input = tokio::spawn(zuno_tui::app::forward_terminal_input(terminal_sender));
        let turns = tokio::spawn(drive_turns(host, prompt_receiver, engine_sender));
        let outcome = app.run().await;
        input.abort();
        turns.abort();
        if let Some(plugins) = plugins {
            plugins.shutdown().await;
        }
        if let Some(plugins) = tui_plugins {
            plugins.shutdown().await;
        }
        outcome
    });
    drop(session);
    outcome.map_err(to_string)
}

/// Drive one turn per submitted prompt until the screen stops sending.
///
/// Failures are reported through the same channel the turn's own events travel on,
/// because the alternate screen is the only surface the user is looking at: an error
/// on stderr under raw mode is either invisible or corrupts the frame. The interrupt
/// event goes first so the status strip stops claiming a running turn, and the error
/// second so the strip's detail is what remains on screen.
async fn drive_turns(
    mut host: TurnHost,
    mut prompts: mpsc::Receiver<String>,
    events: TurnEventSender,
) {
    while let Some(prompt) = prompts.recv().await {
        let outcome = async {
            host.drive(&prompt, events.clone()).await?;
            loop {
                let queued = if prompts.is_empty() {
                    zuno_goal::QueuedUserInput::Absent
                } else {
                    zuno_goal::QueuedUserInput::Present
                };
                if !host.continue_goal_if_idle(queued, events.clone()).await? {
                    break;
                }
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(message) = outcome {
            let reported = events
                .publish(TurnEvent::TurnInterrupted {
                    assistant_message_id: None,
                    steps: 0,
                })
                .await
                .and(
                    events
                        .publish(TurnEvent::Provider {
                            step: 0,
                            event: StreamEvent::Error {
                                message,
                                retry_after: None,
                            },
                        })
                        .await,
                );
            if reported.is_err() {
                return;
            }
        }
    }
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}
