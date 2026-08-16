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

/// How many cancellation requests may be queued.
///
/// One, because aborting a turn is idempotent: a second request for the same turn
/// would abort nothing new, and a full channel is what makes the screen fall through
/// to shutdown rather than swallow the key.
const CANCEL_CHANNEL_CAPACITY: usize = 1;

/// How many picker choices may be queued.
///
/// A few, not one: a user can pick a model and an agent in quick succession, and a full
/// channel would make the second choice a visible refusal for no reason.
const SELECTION_CHANNEL_CAPACITY: usize = 8;

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
    // Read before `TurnHost::open` consumes the plan, and before raw mode, so a slow
    // skill scan cannot delay the first frame of an already-entered alternate screen.
    let facts = runtime.block_on(SessionFacts::resolve(&plan, environment));
    let catalog = runtime.block_on(session_catalog(&plan, environment));
    let broker = Arc::new(PermissionBroker::new(terminal_sender.clone()));
    let approval: Arc<dyn PermissionAsker> = if args.auto {
        Arc::new(AutoApproval)
    } else {
        Arc::clone(&broker) as Arc<dyn PermissionAsker>
    };
    let driver_approval = Arc::clone(&approval);
    let driver_options = options.clone();
    let driver_environment = environment.clone();
    let host = TurnHost::open(plan, environment, approval)?;
    let engine_sender = host.with_event_hooks(engine_sender);
    let plugins = host.plugin_runtime();
    broker.bind_session(host.session_id());

    let (cancel_sender, cancel_receiver) = mpsc::channel(CANCEL_CHANNEL_CAPACITY);
    let (selection_sender, selection_receiver) = mpsc::channel(SELECTION_CHANNEL_CAPACITY);
    let control = host.control();

    let mut screen = SessionScreen::new(context.clone(), terminal_sender.clone())
        .with_prompt_sink(prompt_sender)
        .with_cancel_sink(cancel_sender)
        .with_selection_sink(selection_sender)
        .with_catalog(catalog)
        // A clone rather than a borrow: `KeyDispatcher` takes the keymap by value below,
        // and the keybinding reference has to list what the *user's* keymap resolved
        // rather than the shipped defaults.
        .with_keymap(keymap.clone());
    facts.describe(&mut screen, host.tool_count());
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
        let turns = tokio::spawn(drive_turns(
            TurnDriver {
                host,
                options: driver_options,
                approval: driver_approval,
            },
            prompt_receiver,
            selection_receiver,
            driver_environment,
            engine_sender,
        ));
        let cancels = tokio::spawn(forward_cancellations(control, cancel_receiver));
        let outcome = app.run().await;
        input.abort();
        turns.abort();
        cancels.abort();
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

/// What the welcome screen and the ambient panel state about this session.
///
/// Resolved once, before raw mode, from the plan's already-merged configuration. Every
/// field is a string or a count: `zuno-tui` depends on none of `zuno-lsp`, `zuno-mcp`
/// or `zuno-catalog`, and translating here is what keeps rendering above execution.
struct SessionFacts {
    directory: String,
    branch: Option<String>,
    agent: String,
    model: String,
    version: String,
    context_window: u64,
    lsp: Vec<zuno_tui::views::ambient::Service>,
    mcp: Vec<zuno_tui::views::ambient::Service>,
    skills: Vec<zuno_tui::views::ambient::SkillSummary>,
}

impl SessionFacts {
    /// Read every fact off `plan`, whose configuration is already merged.
    ///
    /// Nothing here can fail into an error: a fact that cannot be resolved is omitted,
    /// because a placeholder would be indistinguishable from a fact that failed to
    /// load — the one ambiguity a surface like this must not have.
    async fn resolve(plan: &TurnPlan, environment: &StartupEnvironment) -> Self {
        let env = environment.resolved();
        let directory = plan.directory();
        let worktree = plan.worktree();
        let config = plan.config();

        let resolved_lsp = zuno_catalog::lsp_config::ResolvedLsp::resolve(config.lsp.as_ref());
        let lsp = resolved_lsp
            .servers()
            .map(|server| {
                // `Pending`, never `Ready`: a server is spawned lazily when a file it
                // handles is first read, so claiming it is up would name a process that
                // does not exist yet.
                zuno_tui::views::ambient::Service::new(
                    server.id.clone(),
                    zuno_tui::views::ambient::Health::Pending,
                )
                .detailed("starts on first matching file")
            })
            .collect();

        let mut mcp = config
            .mcp
            .as_ref()
            .map(|servers| {
                servers
                    .iter()
                    .map(|(name, server)| {
                        let enabled = mcp_enabled(server);
                        let health = if enabled {
                            zuno_tui::views::ambient::Health::Pending
                        } else {
                            zuno_tui::views::ambient::Health::Disabled
                        };
                        zuno_tui::views::ambient::Service::new(name.to_owned(), health)
                            .detailed(if enabled { "configured" } else { "disabled" })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        mcp.sort_by(|left, right| left.name.cmp(&right.name));

        let options = zuno_catalog::skill::discovery::SkillOptions::from_config(
            directory, worktree, env, config,
        );
        let skills = zuno_catalog::skill::load(&options)
            .await
            .sorted()
            .into_iter()
            .map(|skill| zuno_tui::views::ambient::SkillSummary {
                name: skill.name,
                description: skill.description.unwrap_or_default(),
            })
            .collect();

        Self {
            directory: abbreviate_home(directory, environment),
            branch: worktree.and_then(current_branch),
            agent: plan.agent_name().to_owned(),
            model: plan.qualified_model(),
            version: crate::version::RUST_PACKAGE_VERSION.to_owned(),
            context_window: plan.context_window(),
            lsp,
            mcp,
            skills,
        }
    }

    /// State them on the screen's welcome surface, status strip and ambient panel.
    fn describe(self, screen: &mut SessionScreen, tools: usize) {
        screen
            .transcript_mut()
            .transcript_mut()
            .set_context_limit(self.context_window);
        screen.status_mut().describe(&self.agent, &self.model);

        let directory = (!self.directory.is_empty()).then(|| self.directory.clone());
        *screen.welcome_mut().facts_mut() = zuno_tui::views::welcome::WelcomeFacts {
            directory: directory.clone(),
            branch: self.branch.clone(),
            model: Some(self.model.clone()),
            agent: Some(self.agent.clone()),
            version: Some(self.version.clone()),
            tools: Some(tools),
            mcp: Some(self.mcp.len()),
            lsp: Some(self.lsp.len()),
            skills: Some(self.skills.len()),
        };

        let ambient = screen.sidebar_mut().ambient_mut();
        ambient.directory = directory;
        ambient.branch = self.branch;
        ambient.agent = Some(self.agent);
        ambient.model = Some(self.model);
        ambient.version = Some(self.version);
        ambient.lsp = self.lsp;
        ambient.mcp = self.mcp;
        ambient.skills = self.skills;
    }
}

/// `~/rest` rather than the absolute path, which is the form a user recognises.
fn abbreviate_home(path: &std::path::Path, environment: &StartupEnvironment) -> String {
    let display = path.display().to_string();
    let Some(home) = environment
        .resolved()
        .value("HOME")
        .filter(|home| !home.is_empty())
    else {
        return display;
    };
    match display.strip_prefix(home) {
        Some(rest) => format!("~{rest}"),
        None => display,
    }
}

/// The checked-out branch, read from `git` itself.
///
/// Asking `git` rather than parsing `.git/HEAD` because a worktree, a detached head and
/// a packed ref are three different files and `git` already knows which. A failure is
/// `None`: the branch is an ornament here, and guessing one would be worse than
/// omitting it.
fn current_branch(worktree: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(worktree)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// Whether a configured MCP server is switched on, defaulting to on.
fn mcp_enabled(server: &zuno_config::schema::mcp::McpServerConfig) -> bool {
    use zuno_config::schema::mcp::McpServerConfig;
    match server {
        McpServerConfig::Local(local) => local.enabled.unwrap_or(true),
        McpServerConfig::Remote(remote) => remote.enabled.unwrap_or(true),
        McpServerConfig::Toggle(toggle) => toggle.enabled,
    }
}

/// Abort the live turn each time the screen asks, until the screen stops asking.
///
/// A task rather than a call inside the component handler for the reason the turn
/// driver is one: the render loop is the only consumer of the events an aborted turn
/// produces, so it must not be the thing waiting on the abort. `abort` returning
/// `false` means the turn had already finished, which is not a failure — the screen's
/// next press leaves instead.
async fn forward_cancellations(
    control: zuno_engine::status::SessionControl,
    mut cancels: mpsc::Receiver<()>,
) {
    while cancels.recv().await.is_some() {
        let _aborted = control.abort();
    }
}

/// Drive one turn per submitted prompt until the screen stops sending.
///
/// Failures are reported through the same channel the turn's own events travel on,
/// because the alternate screen is the only surface the user is looking at: an error
/// on stderr under raw mode is either invisible or corrupts the frame. The interrupt
/// event goes first so the status strip stops claiming a running turn, and the error
/// second so the strip's detail is what remains on screen.
/// What the model, agent and session pickers offer.
///
/// Models are limited to the **session provider's own**, which is a correctness bound
/// rather than a shortcut: a turn wires exactly one provider credential, so offering
/// another vendor's model would offer a choice that could only fail — and it would fail
/// by presenting this provider's key to that vendor's endpoint. Switching provider is a
/// relaunch, and a picker that said otherwise would be lying about what it can do.
async fn session_catalog(
    plan: &TurnPlan,
    environment: &StartupEnvironment,
) -> zuno_tui::views::session::SessionCatalog {
    let env = environment.resolved();
    let provider = plan.provider_id().to_owned();
    let models = plan
        .provider_model_ids()
        .into_iter()
        .map(|id| zuno_tui::views::picker::ModelEntry {
            id: format!("{provider}/{id}"),
            name: id,
            provider: provider.clone(),
        })
        .collect();
    let agents = zuno_catalog::agent::load(plan.directory(), plan.worktree(), env)
        .map(|agents| {
            agents
                .into_iter()
                .map(|agent| zuno_tui::views::picker::AgentEntry {
                    name: agent.name,
                    description: agent.description.unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    zuno_tui::views::session::SessionCatalog {
        models,
        agents,
        // Sessions are deliberately absent: the picker would list them, and this task
        // cannot switch session without discarding the turn it may be running. An empty
        // list makes the key report "nothing to choose from" rather than open a surface
        // whose selection would be silently ignored.
        sessions: Vec::new(),
        model: Some(plan.qualified_model()),
        agent: Some(plan.agent_name().to_owned()),
    }
}

/// Rebuild the turn host whenever the user picks a different model or agent.
///
/// A new host rather than a mutated one, and that is the safety argument rather than a
/// convenience: a host wires exactly one provider credential, so moving it to another
/// provider's model in place would present that credential to a different vendor's
/// endpoint. Going back through [`TurnPlan::resolve`] and [`TurnHost::open`] — the same
/// path the launch takes — re-resolves the credential, the tool set and the token window
/// together, so there is no combination reachable here that a launch could not produce.
///
/// The session id is carried over, so the conversation continues rather than restarting.
///
/// A failure leaves the previous host in place and says so on the transcript's own
/// channel. The alternative — tearing down a working host on a bad pick — would lose the
/// session over a keystroke.
async fn apply_selection(
    selection: zuno_tui::views::session::Selection,
    host: &mut TurnHost,
    options: &TurnOptions,
    environment: &StartupEnvironment,
    approval: &Arc<dyn PermissionAsker>,
    events: &TurnEventSender,
) -> Option<TurnEventSender> {
    let mut next = options.clone();
    next.session = SessionChoice::Existing(host.session_id().to_owned());
    match selection {
        zuno_tui::views::session::Selection::Model(model) => next.model = Some(model),
        zuno_tui::views::session::Selection::Agent(agent) => next.agent = Some(agent),
        // A theme is the view layer's own business and a session change is not something
        // this task can honour without discarding the turn it may be running.
        zuno_tui::views::session::Selection::Session(_)
        | zuno_tui::views::session::Selection::Theme(_) => return None,
    }
    let rebuilt = async {
        let plan = TurnPlan::resolve(&next, environment).await?;
        TurnHost::open(plan, environment, Arc::clone(approval))
    }
    .await;
    match rebuilt {
        Ok(replacement) => {
            let hooked = replacement.with_event_hooks(events.clone());
            *host = replacement;
            Some(hooked)
        }
        Err(message) => {
            let _reported = events
                .publish(TurnEvent::Provider {
                    step: 0,
                    event: StreamEvent::StatusDetail {
                        detail: format!("warning: keeping the previous model: {message}"),
                    },
                })
                .await;
            None
        }
    }
}

struct TurnDriver {
    host: TurnHost,
    options: TurnOptions,
    approval: Arc<dyn PermissionAsker>,
}

async fn drive_turns(
    mut driver: TurnDriver,
    mut prompts: mpsc::Receiver<String>,
    mut selections: mpsc::Receiver<zuno_tui::views::session::Selection>,
    environment: StartupEnvironment,
    mut events: TurnEventSender,
) {
    loop {
        // A selection is taken only between turns, never during one: rebuilding the host
        // mid-turn would drop the stream the loop is still reading.
        let prompt = tokio::select! {
            biased;
            prompt = prompts.recv() => match prompt {
                Some(prompt) => prompt,
                None => return,
            },
            selection = selections.recv() => {
                let Some(selection) = selection else { return };
                if let Some(rebuilt) = apply_selection(
                    selection,
                    &mut driver.host,
                    &driver.options,
                    &environment,
                    &driver.approval,
                    &events,
                )
                .await
                {
                    events = rebuilt;
                }
                continue;
            }
        };
        drive_one(&mut driver.host, prompt, &mut prompts, &events).await;
    }
}

async fn drive_one(
    host: &mut TurnHost,
    prompt: String,
    prompts: &mut mpsc::Receiver<String>,
    events: &TurnEventSender,
) {
    {
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
            // A closed event channel means the render loop has gone; there is nothing
            // left to report a failure to.
            let _closed = reported.is_err();
        }
    }
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}
