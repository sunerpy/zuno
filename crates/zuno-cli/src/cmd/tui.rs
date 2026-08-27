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
//! A prompt leaves the screen as a typed submission on a bounded channel; a task with the
//! only [`super::turn::TurnHost`] picks it up and drives the turn, publishing
//! [`zuno_engine::r#loop::TurnEvent`]s on the channel the application already
//! consumes. Nothing about that is an optimisation: the loop is the only consumer of
//! terminal input, engine events **and** the terminal-lease wake, so a turn awaited
//! inside a component handler would stop all three and deadlock against a requester's
//! terminal lease.
//!
//! The prompt channel is only a bounded handoff into the durable session inbox. A
//! submission made while a turn is running is admitted there before it can steer at a
//! safe point or remain FIFO-queued for the next turn.
//!
//! # The initial composition is resolved before raw mode
//!
//! [`super::turn::TurnPlan::resolve`] and [`super::turn::TurnHost::open`] both run
//! before [`zuno_tui::app::TerminalSession::start`]. An error printed into a raw-mode
//! alternate screen that is about to be torn down is an error nobody reads.
//! Session changes remount that composition while the same terminal session remains
//! active; if a remount fails, unwinding restores the terminal before the error is
//! reported.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::IsTerminal as _;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt as _};
use tokio::sync::{mpsc, watch};
use zuno_engine::r#loop::{TurnEvent, TurnEventSender, event_channel};
use zuno_engine::status::SessionRunRegistry;
use zuno_engine::terminal_lease::{TerminalLease, TerminalLeaseCleanup};
use zuno_llm::event::StreamEvent;
use zuno_tool::PermissionAsker;
use zuno_tools::question::QuestionAsker;
use zuno_tui::app::{App, CrosstermDrawTarget, CrosstermLifecycle, TerminalEvent, TerminalSession};
use zuno_tui::config::{ResolveOptions, ResolvedTuiConfig};
use zuno_tui::keybind::{KeyDispatcher, Keymap};
use zuno_tui::theme::{
    EnvironmentPalette, HostTerminalPalette, Mode, SYSTEM_THEME, SystemThemeOutcome, ThemeRegistry,
};
use zuno_tui::views::ViewContext;
use zuno_tui::views::ambient::{SessionTitle, WorkState};
use zuno_tui::views::dialog::DialogHost;
use zuno_tui::views::external::{
    EditorCancellation, EditorProcess, EditorProcessLauncher, EditorRequest, ExternalEditor,
    ExternalError, SystemEditor,
};
use zuno_tui::views::live_session::{LiveSessionOpen, LiveSessions};
use zuno_tui::views::message::Message;
use zuno_tui::views::picker::{
    McpProjection, McpServer, McpState, McpToggleRequest, QueuedInputDelivery, QueuedInputEntry,
    QueuedInputNotice, QueuedInputNoticeKind, QueuedInputProjection,
};
use zuno_tui::views::session::{
    PromptSubmission, PromptTarget, QueuedInputMutation, SessionScreen, TargetedPromptSubmission,
    scopes,
};
use zuno_tui::views::slash::{CatalogCommand, HostCommand};

use super::child_turn::{
    ChildSessionOpened, ChildTurnObserver, InteractiveChildInput, InteractiveChildInputContext,
};
use super::tui_permission::{AutoApproval, PermissionBridge, PermissionBroker};
use super::tui_question::{QuestionBridge, QuestionBroker};
use super::turn::{
    SessionChoice, SessionTitleSink, TurnHost, TurnOptions, TurnPlan,
    background_execution_projections, persisted_session_agent,
};
use crate::command::TuiArgs;
use crate::environment::StartupEnvironment;

/// How many prompts may wait for durable admission.
///
/// This is not the session queue: accepted inputs are copied into `SessionInbox`.
/// The bound only prevents a terminal producer from allocating without limit while
/// SQLite is unavailable.
const PROMPT_CHANNEL_CAPACITY: usize = 32;

const MCP_TOGGLE_CHANNEL_CAPACITY: usize = 1;

/// How many language-server reports may be queued.
const LSP_CHANNEL_CAPACITY: usize = super::tui_lsp::REPORT_CHANNEL_CAPACITY;

/// How many "files are waiting" nudges may be queued.
///
/// One, because the message is a signal and not the work: the paths themselves live in
/// [`zuno_tui::views::lsp::PendingEdits`], so a nudge that finds the queue full is
/// redundant rather than lost. Sizing this like a batch queue is what let a whole edit
/// set be dropped.
const EDIT_SIGNAL_CHANNEL_CAPACITY: usize = 1;

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

/// Queue edits and cancellations remain responsive while a provider stream is active.
const QUEUE_MUTATION_CHANNEL_CAPACITY: usize = 8;

const EDITOR_CHANNEL_CAPACITY: usize = 1;

const WORKER_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How many submitted prompts may be waiting to be written down.
///
/// A user cannot submit faster than a turn accepts, and `PROMPT_CHANNEL_CAPACITY` is
/// one, so this is never contended in practice. Sixteen is headroom for the one case
/// that can burst — a stalled filesystem while a user submits, cancels and resubmits —
/// and the sink refuses the newest rather than blocking, because a lost recall is a far
/// smaller loss than a frozen prompt.
const PROMPT_HISTORY_CHANNEL_CAPACITY: usize = 16;

struct ContainedEditorLauncher;

struct ContainedEditorProcess {
    child: tokio::process::Child,
}

impl EditorProcess for ContainedEditorProcess {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    fn request_termination(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let pid = self
                .child
                .id()
                .ok_or_else(|| std::io::Error::other("the editor supervisor has no PID"))?;
            let status = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "signalling the editor supervisor failed with {status}"
                )))
            }
        }
        #[cfg(windows)]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "external editing is disabled on Windows because the process guard has no active cancellation signal",
            ))
        }
    }
}

impl EditorProcessLauncher for ContainedEditorLauncher {
    fn spawn(
        &self,
        invocation: &zuno_tui::views::external::EditorInvocation,
        cwd: Option<&str>,
    ) -> std::io::Result<Box<dyn EditorProcess>> {
        #[cfg(unix)]
        {
            let (program, arguments) =
                zuno_process::guarded_foreground_argv(&invocation.program, &invocation.args);
            if !zuno_process::is_active_guard(std::path::Path::new(&program)) {
                return Err(std::io::Error::other(
                    "external editing is disabled because process-tree containment is not active",
                ));
            }
            let mut command = tokio::process::Command::new(program);
            command.args(arguments);
            if let Some(cwd) = cwd {
                command.current_dir(cwd);
            }
            command
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());
            command
                .spawn()
                .map(|child| Box::new(ContainedEditorProcess { child }) as Box<dyn EditorProcess>)
        }
        #[cfg(windows)]
        {
            let _invocation = invocation;
            let _cwd = cwd;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "external editing is disabled on Windows because the process guard has no active cancellation signal",
            ))
        }
    }
}

pub(super) fn execute(args: &TuiArgs, environment: &StartupEnvironment) -> Result<(), String> {
    if !std::io::stdout().is_terminal() {
        return Err(
            "the interactive TUI requires a terminal; use `run <message>` for a \
             non-interactive turn"
                .to_owned(),
        );
    }

    let session = SessionChoice::resolve(args.session.as_deref(), args.r#continue);
    let options = TurnOptions {
        directory: None,
        model: args.model.clone(),
        agent: args
            .agent
            .clone()
            .or_else(|| persisted_session_agent(&session)),
        preset: None,
        session,
        title: None,
        effort: None,
        tool_authority: None,
        extension_composition: super::turn::ExtensionComposition::Active,
    };
    let mut terminal = None;
    let mut request = RemountRequest::initial(options, args.prompt.clone());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(to_string)?;

    loop {
        match execute_once(args, environment, &runtime, request, &mut terminal) {
            Err(error) => {
                drop(terminal.take());
                return match shutdown_tui_background_jobs(&runtime, environment) {
                    Ok(()) => Err(error),
                    Err(shutdown) => Err(format!(
                        "{error}; background job shutdown also failed: {shutdown}"
                    )),
                };
            }
            Ok(TuiRunOutcome::Exit(identity)) => {
                // The resume command belongs in the primary screen's scrollback. Keep the
                // terminal mounted across composition changes, but restore it before this
                // final line exactly as a one-composition run does.
                drop(terminal.take());
                shutdown_tui_background_jobs(&runtime, environment)?;
                if identity.is_materialized() {
                    println!("{}", resume_hint(identity.id()));
                }
                return Ok(());
            }
            Ok(TuiRunOutcome::Remount(next)) => {
                request = next;
            }
        }
    }
}

fn shutdown_tui_background_jobs(
    runtime: &tokio::runtime::Runtime,
    environment: &StartupEnvironment,
) -> Result<(), String> {
    environment.cancel_background_jobs();
    runtime.block_on(async {
        tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, environment.wait_background_jobs())
            .await
            .map_err(|_| {
                format!(
                    "background jobs did not stop within {} seconds",
                    WORKER_SHUTDOWN_TIMEOUT.as_secs()
                )
            })
    })
}

enum TuiRunOutcome {
    Exit(super::turn::PreparedSessionIdentity),
    Remount(RemountRequest),
}

#[derive(Clone, Copy)]
enum RemountDialog {
    Sessions,
}

struct RemountRequest {
    options: TurnOptions,
    launch_prompt: Option<String>,
    initial_dialog: Option<RemountDialog>,
    show_welcome: bool,
}

impl RemountRequest {
    fn initial(options: TurnOptions, launch_prompt: Option<String>) -> Self {
        Self {
            options,
            launch_prompt,
            initial_dialog: None,
            show_welcome: true,
        }
    }

    fn plain(options: TurnOptions) -> Self {
        Self {
            options,
            launch_prompt: None,
            initial_dialog: None,
            show_welcome: true,
        }
    }

    fn fresh_conversation(options: TurnOptions) -> Self {
        Self {
            options,
            launch_prompt: None,
            initial_dialog: None,
            show_welcome: false,
        }
    }

    fn reopening_sessions(options: TurnOptions) -> Self {
        Self {
            options,
            launch_prompt: None,
            initial_dialog: Some(RemountDialog::Sessions),
            show_welcome: true,
        }
    }
}

/// The physical terminal activation shared by every session composition in one run.
///
/// A session switch still rebuilds all session-scoped services, projections and workers,
/// but dropping this guard only on final exit keeps raw mode and the alternate screen
/// continuous. The old complete frame remains visible while the replacement resolves,
/// then the new app paints over it in one frame.
struct MountedTerminal {
    lifecycle: Arc<CrosstermLifecycle>,
    _session: TerminalSession,
}

fn execute_once(
    args: &TuiArgs,
    environment: &StartupEnvironment,
    runtime: &tokio::runtime::Runtime,
    request: RemountRequest,
    terminal: &mut Option<MountedTerminal>,
) -> Result<TuiRunOutcome, String> {
    let RemountRequest {
        options,
        launch_prompt,
        initial_dialog,
        show_welcome,
    } = request;
    let (terminal_sender, terminal_receiver) = zuno_tui::app::terminal_event_channel();
    let (engine_sender, engine_receiver) = event_channel();
    let (prompt_sender, prompt_receiver) = mpsc::channel(PROMPT_CHANNEL_CAPACITY);

    let plan = runtime.block_on(TurnPlan::resolve(&options, environment))?;
    let concurrency = plan.config().resolved_concurrency();
    let layout = zuno_paths::Layout::resolve(environment.resolved());
    let snapshot_store = zuno_snapshot::Store::open(
        zuno_snapshot::Location::discover_in(layout.snapshot_root(), plan.directory())
            .with_enabled(plan.config().snapshot.unwrap_or(true)),
    );
    let config_paths = tui_config_paths(layout.config(), plan.directory(), plan.worktree());
    let config =
        ResolvedTuiConfig::discover(&config_paths, ResolveOptions::default()).map_err(to_string)?;
    let keymap = Keymap::from_config(&config).map_err(to_string)?;

    // Read here, before raw mode, for the reason `SessionFacts::resolve` is read here:
    // a slow disk must not delay the first frame of an already-entered alternate
    // screen. `PromptHistory::load` cannot fail into an error, so nothing about a
    // damaged file can stop the TUI from starting — it reports instead.
    let history_path = prompt_history_path(&layout);
    let history = zuno_tui::views::editor::PromptHistory::load(&history_path);
    let history_notice = history.notice().map(str::to_owned);

    let mut themes = ThemeRegistry::new();
    // Only `system` needs an OSC round trip. Other themes read `COLORFGBG` without
    // consuming input, which keeps a prompt typed as the first frame appears intact.
    let system_outcome = if config.theme == SYSTEM_THEME {
        themes.refresh_system_theme(&HostTerminalPalette::default(), None, Mode::Dark)
    } else {
        themes.refresh_system_theme(&EnvironmentPalette, None, Mode::Dark)
    };
    let mode = match system_outcome {
        SystemThemeOutcome::Derived(mode) => mode,
        SystemThemeOutcome::Unavailable => Mode::Dark,
    };
    let resolved_theme = themes.resolve(&config.theme, mode);
    let mut theme_diagnostics = themes.load_issues().to_vec();
    theme_diagnostics.extend(resolved_theme.diagnostics());
    let context = ViewContext::new(&resolved_theme, config.clone());

    // Cloned before `TurnHost::open` consumes the plan, because the language-server
    // probe is built after the host is open and would otherwise have nothing to read
    // its server list and workspace root from.
    let lsp_config = plan.config().clone();
    let lsp_workspace = plan
        .worktree()
        .unwrap_or_else(|| plan.directory())
        .to_path_buf();
    let reference_root = lsp_workspace.clone();
    let mcp_configs = plan
        .config()
        .mcp
        .as_ref()
        .map(|servers| {
            servers
                .iter()
                .map(|(name, server)| (name.to_owned(), server.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let initial_mcp_targets = mcp_configs
        .iter()
        .filter(|(_, server)| mcp_enabled(server))
        .map(|(name, _)| McpToggleRequest {
            server: name.clone(),
            desired_enabled: true,
        })
        .collect::<Vec<_>>();
    let mcp_catalog = zuno_mcp::Catalog::new(mcp_configs.keys().cloned());
    let mcp_controller = zuno_mcp::McpServerController::from_config(
        mcp_catalog.clone(),
        &lsp_workspace,
        mcp_configs,
        zuno_mcp::McpLifecycleOptions::default(),
    );
    let mcp_projection = McpProjection::new(project_mcp_snapshots(&mcp_controller.snapshots()));
    let mcp_dirty = Arc::new(AtomicBool::new(!initial_mcp_targets.is_empty()));
    let reference_source = super::tui_reference::ProjectFiles::build(&reference_root)?;
    // Read before `TurnHost::open` consumes the plan, and before raw mode, so a slow
    // skill scan cannot delay the first frame of an already-entered alternate screen.
    let facts = runtime.block_on(SessionFacts::resolve(&plan, environment));
    let mut catalog = runtime.block_on(session_catalog(&plan, environment));
    let broker = Arc::new(PermissionBroker::new(terminal_sender.clone()));
    let question_broker = Arc::new(QuestionBroker::new(terminal_sender.clone()));
    let question: Arc<dyn QuestionAsker> = Arc::clone(&question_broker) as Arc<dyn QuestionAsker>;
    let approval: Arc<dyn PermissionAsker> = if args.auto && !plan.config().strict_authorization() {
        Arc::new(AutoApproval)
    } else {
        Arc::clone(&broker) as Arc<dyn PermissionAsker>
    };
    let driver_approval = Arc::clone(&approval);
    let mut driver_options = options.clone();
    let driver_environment = environment.clone();
    let runs = SessionRunRegistry::new();
    let live_sessions = LiveSessions::default();
    let child_observer: Arc<dyn ChildTurnObserver> = Arc::new(TuiChildObserver {
        sessions: live_sessions.clone(),
        wake: terminal_sender.clone(),
    });
    let mut host = runtime.block_on(TurnHost::open_with_runtime_mcp_and_observer(
        plan,
        environment,
        approval,
        Some(Arc::clone(&question)),
        runs.clone(),
        Some(mcp_catalog.clone()),
        Some(Arc::clone(&child_observer)),
    ))?;
    if let Err(error) = host.activate_extension_composition() {
        let shutdown = runtime.block_on(host.shutdown());
        return Err(match shutdown {
            Ok(()) => error,
            Err(shutdown) => {
                format!("{error}; candidate host shutdown also failed: {shutdown}")
            }
        });
    }
    let child_restore_diagnostics = if host.is_session_materialized() {
        restore_child_sessions(&host.database_pool(), host.session_id(), &live_sessions)
    } else {
        Vec::new()
    };
    driver_options.extension_composition = super::turn::ExtensionComposition::Active;
    catalog.sessions = session_entries(&host)?;
    catalog.session = host
        .is_session_materialized()
        .then(|| host.session_id().to_owned());
    // Seeded from the row the host already read, so a session resumed with `-s` shows its
    // name on frame one instead of waiting for a turn that will never re-title it — the
    // generator declines outright once a session is named.
    let session_title = SessionTitle::new(host.session_title().map(str::to_owned));
    let title_sink: Arc<dyn SessionTitleSink> = Arc::new(TitleProjectionSink {
        projection: session_title.clone(),
        wake: terminal_sender.clone(),
    });
    host.set_title_sink(Arc::clone(&title_sink));
    let continuity = TuiHostContinuity::new(runs, title_sink, Some(child_observer));
    let interactive_children = InteractiveChildInput::new(InteractiveChildInputContext {
        database: host.database_pool(),
        environment: driver_environment.clone(),
        directory: reference_root.clone(),
        approval: Arc::clone(&driver_approval),
        question: Some(Arc::clone(&question)),
        runs: continuity.runs(),
        mcp: Some(mcp_catalog.clone()),
        observer: continuity.child_observer(),
        supervisor: environment.background_jobs(&reference_root),
    });
    let work_state = WorkState::new(host.work_state()?);
    let queued_inputs = QueuedInputProjection::new(project_queued_inputs(
        &host.session_inbox(),
        host.session_id(),
    )?);
    // Copied before the host is moved into the turn driver. The id is what the hint
    // printed after teardown has to name, and by then the host is gone — a driver task
    // owns it and is aborted, not joined, so nothing survives to be asked.
    let exit_identity = host.session_identity();
    let mut slash_commands = host
        .commands()
        .map(|command| CatalogCommand::new(command.name.clone(), command.description.clone()))
        .collect::<Vec<_>>();
    slash_commands.extend(
        host.slash_skills()
            .into_iter()
            .map(|skill| CatalogCommand::skill(skill.name, skill.description, skill.location)),
    );
    let (cancel_sender, cancel_receiver) = mpsc::channel(CANCEL_CHANNEL_CAPACITY);
    let (selection_sender, selection_receiver) = mpsc::channel(SELECTION_CHANNEL_CAPACITY);
    let (queue_mutation_sender, queue_mutation_receiver) =
        mpsc::channel(QUEUE_MUTATION_CHANNEL_CAPACITY);
    let (mcp_toggle_sender, mcp_toggle_receiver) = mpsc::channel(MCP_TOGGLE_CHANNEL_CAPACITY);
    let (editor_sender, editor_receiver) = mpsc::channel(EDITOR_CHANNEL_CAPACITY);
    let (editor_result_sender, editor_result_receiver) = mpsc::channel(EDITOR_CHANNEL_CAPACITY);
    let control = continuity.control(host.session_id());

    let (report_sender, report_receiver) = mpsc::channel(LSP_CHANNEL_CAPACITY);
    let (edit_sender, edit_receiver) = mpsc::channel(EDIT_SIGNAL_CHANNEL_CAPACITY);
    let pending_edits = zuno_tui::views::lsp::PendingEdits::new(edit_sender);
    // The reader holds no sender, so the screen dropping its handle really does close
    // the channel and end the checker task.
    let edit_reader = pending_edits.reader();
    let (history_sender, history_receiver) = mpsc::channel(PROMPT_HISTORY_CHANNEL_CAPACITY);
    let background_executions = host.background_executions();
    let background_session = host.session_id().to_owned();
    let background_projection_service = Arc::clone(&background_executions);
    let background_projection_session = background_session.clone();
    let background_projection_state = work_state.clone();
    let probe = super::tui_lsp::Probe::resolve(
        &lsp_config,
        lsp_workspace.as_path(),
        terminal_sender.clone(),
    );
    let mut screen = SessionScreen::new(context.clone(), terminal_sender.clone())
        .with_prompt_sink(prompt_sender)
        .with_slash_commands(slash_commands)
        .with_reference_source(Box::new(reference_source))
        .with_cancel_sink(cancel_sender)
        .with_selection_sink(selection_sender)
        .with_mcp_control(mcp_projection.clone(), mcp_toggle_sender)
        .with_queued_inputs(queued_inputs.clone(), queue_mutation_sender)
        .with_session_title(session_title)
        .with_work_state(work_state.clone())
        .with_live_sessions(live_sessions)
        .with_catalog(catalog)
        .with_diagnostics_source(report_receiver)
        .with_edit_sink(pending_edits)
        .with_prompt_history(history.into_entries(), history_sender)
        .with_external_editor(editor_sender, editor_result_receiver)
        .with_background_executions(background_executions, background_session)
        // A clone rather than a borrow: `KeyDispatcher` takes the keymap by value below,
        // and the keybinding reference has to list what the *user's* keymap resolved
        // rather than the shipped defaults.
        .with_keymap(keymap.clone());
    if !show_welcome {
        screen = screen.without_welcome();
    }
    facts.describe(
        &mut screen,
        host.tool_count(),
        RuntimeIdentity::resolve(&host, environment.resolved()),
    );
    screen
        .transcript_mut()
        .transcript_mut()
        .restore_loaded_skills(host.selected_skills().into_iter().map(|skill| {
            zuno_tui::views::message::LoadedSkillIdentity {
                name: skill.name,
                source: Some(skill.source),
            }
        }));
    if host.is_session_materialized() {
        screen
            .transcript_mut()
            .transcript_mut()
            .restore_usage(host.session_usage().snapshot());
    }
    // Before every notice below, and that order is load-bearing in both directions.
    //
    // Before, because `Transcript::replay` only acts on an empty transcript — a guard that
    // is what makes the replayed run a prefix, which is what lets the message menu tell a
    // turn this process ran from one it only read back.
    //
    // Before also because these notices are about *this* launch, and history is what came
    // first. A fresh session replays nothing, so PR #23's case — startup notices coexisting
    // with the welcome screen — is reached by exactly the same code and is unchanged:
    // `conversation_started` excludes `Role::System`.
    match host.resumed_history() {
        Ok(history) => {
            let replay = super::tui_replay::project(history);
            let omission = replay.omission_notice();
            screen
                .transcript_mut()
                .transcript_mut()
                .replay(replay.messages);
            if let Some(notice) = omission {
                screen.transcript_mut().transcript_mut().push(notice);
            }
        }
        // Reported, never fatal: a session whose stored parts cannot be decoded still opens,
        // because the alternative is refusing to open the one session a user needs to export
        // or prune. A blank transcript with no explanation is the defect this whole path
        // exists to remove, so the explanation is mandatory even when the transcript is not.
        Err(error) => {
            screen
                .transcript_mut()
                .transcript_mut()
                .push(super::tui_replay::failure_notice(host.session_id(), &error));
        }
    }
    for diagnostic in child_restore_diagnostics {
        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::notice(diagnostic));
    }
    // Theme fallback is recoverable, unlike an unreadable or malformed config file.
    // Put its diagnostic in the transcript rather than stderr: the alternate screen
    // would hide a pre-raw warning until shutdown, while this is visible on frame one.
    for diagnostic in theme_diagnostics {
        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::notice(format!("warning: {diagnostic}")));
    }
    // The same surface, for the same reason: an unusable history file is recoverable,
    // and a pre-raw-mode stderr warning would sit hidden behind the alternate screen
    // until the user quit. This is on screen from frame one.
    if let Some(notice) = history_notice {
        screen
            .transcript_mut()
            .transcript_mut()
            .push(Message::notice(format!("warning: {notice}")));
    }
    if let Some(prompt) = launch_prompt {
        let prompt = prompt.trim();
        if !prompt.is_empty() {
            screen.submit_prompt(prompt);
        }
    }
    // The waker is what makes a toast expire on its deadline rather than at the next
    // event. It is the terminal channel that already exists, not a new one; see
    // `zuno_tui::views::toast` for why one deadline and one wake was chosen over giving
    // the redraw scheduler a fourth tier.
    let initial_dialog = initial_dialog.map(|RemountDialog::Sessions| screen.session_picker());
    let mut dialogs =
        DialogHost::new(context.clone(), Box::new(screen)).with_waker(terminal_sender.clone());
    if let Some(dialog) = initial_dialog {
        dialogs.open(dialog);
    }
    let bridge = PermissionBridge::new(context.clone(), broker, dialogs)
        .with_question(QuestionBridge::new(context, question_broker));
    let root = KeyDispatcher::new(keymap, scopes(), Box::new(bridge));

    // Mouse capture is terminal-scoped rather than session-scoped. Session switching is
    // admitted only inside the same exact directory, so every remount resolves the same
    // merged TUI configuration and reuses the activation created by the first one.
    let lifecycle = terminal.as_ref().map_or_else(
        || Arc::new(CrosstermLifecycle::new(config.mouse)),
        |mounted| Arc::clone(&mounted.lifecycle),
    );
    let target = CrosstermDrawTarget::new().map_err(to_string)?;
    let (mut app, owner) = App::new(
        Box::new(root),
        Box::new(target),
        lifecycle.clone(),
        terminal_receiver,
        engine_receiver,
    );
    let input_control = owner.input_control();
    let editor_lease: Arc<dyn TerminalLease> = Arc::new(owner.broker());
    let external_editor: Arc<dyn ExternalEditor> = Arc::new(SystemEditor::host_with_launcher(
        Arc::new(ContainedEditorLauncher),
    ));
    let editor_wake = terminal_sender.clone();
    let mcp_wake = terminal_sender.clone();
    let work_wake = terminal_sender.clone();
    let background_wake = terminal_sender.clone();
    let queue_wake = terminal_sender.clone();
    let session_shutdown = terminal_sender.clone();
    let remount = CompositionRemount::default();
    let driver_remount = remount.clone();

    if terminal.is_none() {
        let session = TerminalSession::start(lifecycle.clone()).map_err(to_string)?;
        *terminal = Some(MountedTerminal {
            lifecycle,
            _session: session,
        });
    }
    let outcome = runtime.block_on(async move {
        let (worker_shutdown, worker_shutdown_source) = watch::channel(false);
        let input_shutdown = Arc::clone(&input_control);
        let mut input = tokio::spawn(zuno_tui::app::forward_terminal_input(
            terminal_sender,
            input_control,
        ));
        let mut turns = tokio::spawn(drive_turns(
            TurnDriver {
                host,
                options: driver_options,
                approval: driver_approval,
                question,
                reference_root,
                mcp_catalog,
                mcp_dirty: Arc::clone(&mcp_dirty),
                snapshots: SnapshotHistory::new(snapshot_store),
                work_state,
                work_wake,
                queued_inputs,
                queue_wake,
                continuity,
                interactive_children,
                remount: driver_remount,
                shutdown: session_shutdown,
            },
            prompt_receiver,
            selection_receiver,
            queue_mutation_receiver,
            driver_environment,
            engine_sender,
            worker_shutdown_source.clone(),
        ));
        let mut background = tokio::spawn(drive_background_projection(
            background_projection_service,
            background_projection_session,
            background_projection_state,
            background_wake,
            worker_shutdown_source.clone(),
        ));
        let mut mcp = tokio::spawn(drive_mcp_lifecycle(McpLifecycleWorker {
            controller: mcp_controller,
            requests: mcp_toggle_receiver,
            initial: initial_mcp_targets,
            concurrency: NonZeroUsize::new(usize::from(concurrency.mcp_connections))
                .expect("configuration validates MCP concurrency"),
            projection: mcp_projection,
            dirty: mcp_dirty,
            wake: mcp_wake,
            shutdown: worker_shutdown_source.clone(),
        }));
        let mut checks = tokio::spawn(super::tui_lsp::check_edits(
            probe,
            edit_reader,
            edit_receiver,
            report_sender,
            worker_shutdown_source.clone(),
        ));
        let shutdown_control = control.clone();
        let mut cancels = tokio::spawn(forward_cancellations(
            control,
            cancel_receiver,
            worker_shutdown_source.clone(),
        ));
        let mut history = tokio::spawn(record_prompt_history(
            history_path,
            history_receiver,
            worker_shutdown_source,
        ));
        let (editor_shutdown, editor_shutdown_source) = watch::channel(false);
        let mut editor = tokio::spawn(drive_external_editor(
            editor_lease,
            external_editor,
            editor_receiver,
            editor_result_sender,
            editor_wake,
            editor_shutdown_source,
        ));
        let outcome = app.run().await.map_err(to_string);
        // `App::run` returning is the logical end of the human-attached surface, even
        // though worker shutdown still follows. Drop the component tree now so the
        // permission bridge rejects root or child asks before the turn driver is joined;
        // retaining it until the async block ended would make an unseen approval wait for
        // the worker-shutdown timeout.
        drop(app);
        let _stopping = worker_shutdown.send(true);
        let _aborted = shutdown_control.abort();
        let _stopping = editor_shutdown.send(true);
        input_shutdown.stop();
        let (
            editor_shutdown,
            input_shutdown,
            cancellation_shutdown,
            history_shutdown,
            turn_shutdown,
            background_shutdown,
            mcp_shutdown,
            lsp_shutdown,
        ) = tokio::join!(
            await_worker("external editor", &mut editor),
            await_worker("terminal input", &mut input),
            await_worker("cancellation forwarder", &mut cancels),
            await_worker("prompt history", &mut history),
            await_turn_driver(&mut turns),
            await_worker("background projection", &mut background),
            await_worker("MCP lifecycle", &mut mcp),
            await_worker("LSP diagnostics", &mut checks),
        );
        finish_tui_shutdown(
            outcome,
            [
                editor_shutdown,
                input_shutdown,
                cancellation_shutdown,
                history_shutdown,
                turn_shutdown,
                background_shutdown,
                mcp_shutdown,
                lsp_shutdown,
            ],
        )
    });
    outcome?;
    if let Some(next) = remount.take() {
        return Ok(TuiRunOutcome::Remount(next));
    }
    Ok(TuiRunOutcome::Exit(exit_identity))
}

async fn await_turn_driver(
    worker: &mut tokio::task::JoinHandle<Result<(), String>>,
) -> Result<(), String> {
    match tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, &mut *worker).await {
        Ok(joined) => {
            joined.map_err(|error| format!("turn driver shutdown task failed: {error}"))?
        }
        Err(_elapsed) => {
            worker.abort();
            let _cancelled = worker.await;
            Err(format!(
                "turn driver did not reach quiescence within {} seconds",
                WORKER_SHUTDOWN_TIMEOUT.as_secs()
            ))
        }
    }
}

async fn await_worker(name: &str, worker: &mut tokio::task::JoinHandle<()>) -> Result<(), String> {
    match tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, &mut *worker).await {
        Ok(joined) => joined.map_err(|error| format!("{name} shutdown task failed: {error}")),
        Err(_elapsed) => {
            worker.abort();
            let _cancelled = worker.await;
            Err(format!(
                "{name} did not reach quiescence within {} seconds",
                WORKER_SHUTDOWN_TIMEOUT.as_secs()
            ))
        }
    }
}

fn finish_tui_shutdown<const N: usize>(
    outcome: Result<(), String>,
    shutdowns: [Result<(), String>; N],
) -> Result<(), String> {
    let mut failures = outcome.err().into_iter().collect::<Vec<_>>();
    failures.extend(shutdowns.into_iter().filter_map(Result::err));
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// The flag that reopens an existing session, as `command.rs` declares it.
///
/// Named rather than spelled at the call site so the hint and the parser cannot drift:
/// a hint advertising a flag `TuiArgs` does not accept is worse than no hint, because
/// the user pastes it and gets a parse error for a session that is intact.
///
/// `-s <id>` and not `-c`, although `-c` is shorter and reads like the obvious choice for
/// "the one you just left". It is not equivalent: `--continue` resolves to the most
/// recently updated *active session in the current directory*
/// (`resolve_session`'s `ListQuery::directory(..).active_only().with_limit(1)`), so it
/// names the session just left only while the user stays in that directory and nothing
/// else touches a session there. Both conditions fail in ordinary use — a second terminal,
/// or pasting the hint from a different `cd` — and when they fail `-c` silently reopens a
/// *different* conversation, which is worse than an error. An explicit id is unambiguous
/// from anywhere and cannot resolve to the wrong session.
const RESUME_FLAG: &str = "-s";

/// What to print once the alternate screen is gone, so the session can be reopened.
///
/// Written to stdout **after** teardown and never during the run. Inside raw mode the
/// alternate screen owns the viewport, so this line would be drawn into a buffer the
/// terminal discards on exit — visible for the frame it corrupts and gone afterwards.
/// After `LeaveAlternateScreen` the primary buffer is back and the line lands in the
/// user's scrollback, which is the only place a command they may want tomorrow is any
/// use.
///
/// Called only after [`PreparedSessionIdentity::is_materialized`] proves the stable
/// process-local id now names a durable row. A welcome screen that never submitted model
/// input deliberately has no row and therefore no resume command to print.
fn resume_hint(session_id: &str) -> String {
    format!("resume this session: zuno {RESUME_FLAG} {session_id}")
}

/// Publishes a generated session name into the panel's projection and asks for a frame.
///
/// The composition root's half of [`super::turn::SessionTitleSink`]: `turn.rs` declares the
/// trait so it stays free of view types, and this is the one place allowed to name both it
/// and [`SessionTitle`].
///
/// The wake is not optional and not belt-and-braces. `SessionTitle::replace` moves state
/// the render loop only reads when it draws, and the prelude runs *before* the model is
/// called — so on a slow first token there may be no other event for seconds, and the name
/// would sit in the projection unseen. The nudge is `try_send` with the result discarded,
/// like every other wake on this path: a full queue already means a frame is coming, and a
/// title must never block the turn that produced it.
struct TitleProjectionSink {
    projection: SessionTitle,
    wake: mpsc::Sender<TerminalEvent>,
}

/// Projects independently running child hosts into the mounted session screen.
struct TuiChildObserver {
    sessions: LiveSessions,
    wake: mpsc::Sender<TerminalEvent>,
}

/// Restore the durable child tree before the first frame of a resumed parent session.
///
/// A child created by an earlier process is still a real session. Keeping this projection
/// process-only made `ctrl+x down` report that the parent had never delegated even though
/// SQLite held the complete tree and transcript. Each history is hydrated through the same
/// compaction boundary the next model request uses; one corrupt child becomes a visible notice
/// inside that child instead of hiding its siblings.
fn restore_child_sessions(
    pool: &zuno_db::pool::Pool,
    root_session_id: &str,
    sessions: &LiveSessions,
) -> Vec<String> {
    let connection = match pool.get() {
        Ok(connection) => connection,
        Err(error) => {
            return vec![format!(
                "warning: child sessions could not be restored: {error}"
            )];
        }
    };
    let mut diagnostics = Vec::new();
    let mut pending = VecDeque::from([root_session_id.to_owned()]);
    let mut seen = BTreeSet::from([root_session_id.to_owned()]);

    while let Some(parent_session_id) = pending.pop_front() {
        let mut children = match zuno_db::session::children(&connection, &parent_session_id) {
            Ok(children) => children,
            Err(error) => {
                diagnostics.push(format!(
                    "warning: children of session `{parent_session_id}` could not be restored: \
                     {error}"
                ));
                continue;
            }
        };
        children.sort_by(|left, right| {
            (left.time_created, left.id.as_str()).cmp(&(right.time_created, right.id.as_str()))
        });
        for child in children {
            if !seen.insert(child.id.clone()) {
                diagnostics.push(format!(
                    "warning: child session cycle ignored at `{}`",
                    child.id
                ));
                continue;
            }
            pending.push_back(child.id.clone());
            let messages =
                match zuno_engine::r#loop::hydrate_retained_history(&connection, &child.id) {
                    Ok(history) => {
                        let replay = super::tui_replay::project(history);
                        let omission = replay.omission_notice();
                        let mut messages = replay.messages;
                        if let Some(notice) = omission {
                            messages.push(notice);
                        }
                        messages
                    }
                    Err(error) => vec![super::tui_replay::failure_notice(&child.id, &error)],
                };
            sessions.restore(LiveSessionOpen {
                session_id: child.id,
                parent_session_id: parent_session_id.clone(),
                title: child.title,
                agent: child.agent.unwrap_or_default(),
                model: persisted_model_label(child.model.as_deref()).unwrap_or_default(),
                effort: None,
                messages,
                usage: Some(child.usage.snapshot()),
            });
        }
    }

    diagnostics
}

fn persisted_model_label(raw: Option<&str>) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(raw?).ok()?;
    let provider = value.get("providerID")?.as_str()?;
    let model = value.get("id")?.as_str()?;
    Some(format!("{provider}/{model}"))
}

impl ChildTurnObserver for TuiChildObserver {
    fn opened(&self, opened: ChildSessionOpened) {
        self.sessions.open(LiveSessionOpen {
            session_id: opened.session_id,
            parent_session_id: opened.parent_session_id,
            title: opened.title,
            agent: opened.agent,
            model: opened.model,
            effort: opened.effort,
            messages: opened.messages,
            usage: opened.usage,
        });
        let _nudged = self.wake.try_send(TerminalEvent::Wake);
    }

    fn event(&self, session_id: &str, event: &TurnEvent) {
        if self.sessions.observe(session_id, event) {
            let _nudged = self.wake.try_send(TerminalEvent::Wake);
        }
    }
}

impl super::turn::SessionTitleSink for TitleProjectionSink {
    fn publish(&self, title: &str) {
        self.projection.replace(Some(title.to_owned()));
        let _nudged = self.wake.try_send(TerminalEvent::Wake);
    }
}

/// Process-local collaborators that survive a turn-host replacement.
///
/// Model, agent, effort, and MCP changes replace [`TurnHost`] without remounting the
/// session screen. Cancellation control and the title projection belong to that mounted
/// session, not to one host generation. Keeping both here prevents a replacement from
/// silently moving live work to a fresh registry while the UI still targets the old one,
/// or from dropping future title updates after the first host is gone.
#[derive(Clone)]
struct TuiHostContinuity {
    runs: SessionRunRegistry,
    title_sink: Arc<dyn SessionTitleSink>,
    child_observer: Option<Arc<dyn ChildTurnObserver>>,
}

impl TuiHostContinuity {
    fn new(
        runs: SessionRunRegistry,
        title_sink: Arc<dyn SessionTitleSink>,
        child_observer: Option<Arc<dyn ChildTurnObserver>>,
    ) -> Self {
        Self {
            runs,
            title_sink,
            child_observer,
        }
    }

    fn control(&self, session_id: &str) -> zuno_engine::status::SessionControl {
        self.runs.control(session_id)
    }

    fn runs(&self) -> SessionRunRegistry {
        self.runs.clone()
    }

    fn title_sink(&self) -> Arc<dyn SessionTitleSink> {
        Arc::clone(&self.title_sink)
    }

    fn child_observer(&self) -> Option<Arc<dyn ChildTurnObserver>> {
        self.child_observer.as_ref().map(Arc::clone)
    }

    async fn open_host(
        &self,
        plan: TurnPlan,
        environment: &StartupEnvironment,
        approval: Arc<dyn PermissionAsker>,
        question: Arc<dyn QuestionAsker>,
        mcp: zuno_mcp::Catalog,
    ) -> Result<TurnHost, String> {
        let mut host = TurnHost::open_with_runtime_mcp_and_observer(
            plan,
            environment,
            approval,
            Some(question),
            self.runs(),
            Some(mcp),
            self.child_observer(),
        )
        .await?;
        host.set_title_sink(self.title_sink());
        Ok(host)
    }
}

/// Where submitted prompts are remembered between runs.
///
/// `state()` and not `data()`, which is the distinction the XDG base-directory spec
/// draws and which this layout already follows: `data()` holds artifacts the
/// application cannot regenerate and a user would miss — `auth.json`, the session
/// database, snapshots — while `state()` is for state that should survive a restart
/// but is not important enough to back up. A shell-style history file is the spec's
/// own example of the latter, and losing it costs recall and nothing else.
///
/// Resolved here rather than inside `zuno-tui`, so that crate stays a leaf on
/// `zuno-engine`, `zuno-llm` and `zuno-permission`. It names the file and reads a path
/// it is given; it never learns where a home directory is. Same division as
/// `ResolvedTuiConfig::discover`, which takes a path list for the same reason.
///
/// Joined with [`zuno_paths::node_path::join`] rather than `PathBuf::push`, because
/// this layout's paths are Node-normalized: with `XDG_STATE_HOME=/tmp/x/../y` the two
/// disagree about which directory this is, and a `PathBuf` join would write history
/// somewhere `state()` does not point.
fn prompt_history_path(layout: &zuno_paths::Layout) -> PathBuf {
    PathBuf::from(zuno_paths::node_path::join(
        &layout.state().to_string_lossy(),
        zuno_tui::views::editor::PROMPT_HISTORY_FILE,
    ))
}

/// Append every prompt the editor records to the history file.
///
/// The only part of prompt history that runs while the TUI is live, and it runs here
/// rather than in the editor because the editor is called from inside
/// `App::handle_terminal`, which holds the `Mutex<UiState>` the render loop needs. An
/// append on that path is the F-1 defect in miniature: a hung filesystem would freeze
/// input, drawing and terminal restoration together. Crossing a bounded channel to
/// this task is what keeps a stalled write costing a recall instead of a session.
///
/// `spawn_blocking` for the write itself, because `std::fs` has no async form and
/// blocking a runtime worker would stall the render task that shares it.
///
/// A failure goes to `tracing`, not to stderr and not to the transcript: stderr would
/// paint over the alternate screen the render loop owns, and this task holds no handle
/// to the screen — deliberately, since a component reachable from here would be one
/// more thing racing the loop for `UiState`. Only the first failure is reported; the
/// cause is almost always permissions or a full disk, and one line per submitted
/// prompt would bury the log it is written to.
async fn record_prompt_history(
    path: PathBuf,
    mut records: mpsc::Receiver<String>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut reported = false;
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    records.close();
                    while let Some(entry) = records.recv().await {
                        append_prompt_history_entry(&path, entry, &mut reported).await;
                    }
                    return;
                }
            }
            entry = records.recv() => {
                let Some(entry) = entry else {
                    return;
                };
                append_prompt_history_entry(&path, entry, &mut reported).await;
            }
        }
    }
}

async fn append_prompt_history_entry(path: &Path, entry: String, reported: &mut bool) {
    let Some(line) = zuno_tui::views::editor::PromptHistory::encode(&entry) else {
        return;
    };
    let target = path.to_path_buf();
    let written = tokio::task::spawn_blocking(move || append_line(&target, &line)).await;
    if let Ok(Err(error)) = written
        && !*reported
    {
        *reported = true;
        tracing::warn!(
            path = %path.display(),
            %error,
            "failed to append to the prompt history; later failures are not repeated"
        );
    }
}

/// Append `line` to `path`, creating the directory and the file if needed.
///
/// `append` rather than a read-modify-write of the whole file: two Zuno processes in
/// two terminals share this path, and rewriting it would let the one that exits last
/// erase everything the other recorded. A single `write` of one short line is also
/// what makes a JSONL reader's worst case a truncated final line.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        // Created here because nothing in the TUI startup path calls
        // `zuno_paths::Layout::ensure`, and a getter in that crate is pure by contract.
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

async fn drive_external_editor(
    lease: Arc<dyn TerminalLease>,
    editor: Arc<dyn ExternalEditor>,
    mut requests: mpsc::Receiver<EditorRequest>,
    results: mpsc::Sender<Result<Option<String>, ExternalError>>,
    wake: mpsc::Sender<zuno_tui::app::TerminalEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let request = tokio::select! {
            request = requests.recv() => match request {
                Some(request) => request,
                None => return,
            },
            changed = shutdown.changed() => {
                let _changed = changed;
                return;
            }
        };
        let cancellation = EditorCancellation::new();
        let cleanup: Arc<dyn TerminalLeaseCleanup> = Arc::new(cancellation.clone());
        let outcome = match lease
            .acquire_with_cleanup(request.lease_reason(), cleanup)
            .await
        {
            Ok(guard) => {
                let editing = editor.edit(&request, cancellation.clone());
                tokio::pin!(editing);
                let edited = tokio::select! {
                    result = &mut editing => result,
                    changed = shutdown.changed() => {
                        let _changed = changed;
                        cancellation.cancel();
                        editing.await
                    }
                };
                guard.release();
                edited
            }
            Err(error) => Err(ExternalError::Failed(error.to_string())),
        };
        if *shutdown.borrow() {
            return;
        }
        if results.send(outcome).await.is_err() {
            return;
        }
        let _nudged = wake.try_send(zuno_tui::app::TerminalEvent::Wake);
    }
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
    effort: Option<String>,
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
        let directory = plan.directory();
        let worktree = plan.worktree();
        let config = plan.config();

        let resolved_lsp = zuno_catalog::lsp_config::ResolvedLsp::resolve(config.lsp.as_ref());
        // The same registry the diagnostics probe runs — `ResolvedLsp::servers()` is only
        // the *overrides*, so a census built from it reported `0 lsp` while the probe was
        // happily running all twenty-odd built-ins that `lsp: true` enables. Two
        // collectors over one fact is how a live feature comes to be advertised as absent.
        let lsp = zuno_lsp::registry::ServerRegistry::offline(&resolved_lsp)
            .servers()
            .iter()
            .map(lsp_service)
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

        let skills = plan
            .skills()
            .sorted()
            .into_iter()
            .map(|skill| zuno_tui::views::ambient::SkillSummary {
                name: skill.name,
                source: skill.location,
                description: skill.description.unwrap_or_default(),
                loaded: false,
            })
            .collect();

        Self {
            directory: abbreviate_home(directory, environment),
            branch: worktree.and_then(current_branch),
            agent: plan.agent_name().to_owned(),
            model: plan.qualified_model(),
            effort: plan.effort().map(|effort| effort.as_str().to_owned()),
            version: crate::version::RUST_PACKAGE_VERSION.to_owned(),
            context_window: plan.context_window(),
            lsp,
            mcp,
            skills,
        }
    }

    /// State them on the welcome surface, reply identity, and ambient panel.
    fn describe(self, screen: &mut SessionScreen, tools: usize, runtime: RuntimeIdentity) {
        // Built before the moves below, which hand `lsp` to the ambient panel. The MCP
        // group is deliberately absent: the screen reads that from its live projection at
        // open time, so the census cannot state a connection state the MCP dialog has
        // already moved on from.
        screen.set_diagnostics(
            vec![
                zuno_tui::views::diagnostics::Group::new("LSP servers", self.lsp.clone()),
                zuno_tui::views::diagnostics::Group::new("Harness", runtime.harness),
            ],
            zuno_tui::views::diagnostics::DebugFacts {
                build: Some(crate::version::BUILD_ID.to_owned()),
                version: Some(self.version.clone()),
                channel: Some(zuno_paths::files::installation_channel().to_owned()),
                os: Some(format!(
                    "{} {}",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )),
                terminal: runtime.terminal,
                session: Some(runtime.session),
                model: Some(self.model.clone()),
                agent: Some(self.agent.clone()),
                directory: (!self.directory.is_empty()).then(|| self.directory.clone()),
            },
        );
        screen
            .transcript_mut()
            .transcript_mut()
            .set_context_limit(self.context_window);
        screen.status_mut().describe(&self.agent, &self.model);

        let directory = (!self.directory.is_empty()).then(|| self.directory.clone());
        *screen.welcome_mut().facts_mut() = zuno_tui::views::welcome::WelcomeFacts {
            agent: Some(self.agent.clone()),
            model: Some(self.model.clone()),
            reasoning: self.effort,
            directory: directory.clone(),
            branch: self.branch.clone(),
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

/// The facts `§8.7`'s panels need that only exist once the turn host is open.
///
/// Separate from [`SessionFacts`] because that value is resolved *before* raw mode and
/// before the host is created, and the session id does not exist until then.
struct RuntimeIdentity {
    session: String,
    harness: Vec<zuno_tui::views::ambient::Service>,
    terminal: Option<String>,
}

impl RuntimeIdentity {
    /// Read the host-derived halves of the census and the debug report.
    ///
    /// `TERM_PROGRAM` before `TERM`: the former names the emulator (`tmux`,
    /// `WezTerm`), while the latter names its termcap entry (`xterm-256color`), and
    /// several unrelated emulators report the same `TERM`. Neither is guaranteed, so an
    /// absent value is omitted rather than guessed.
    fn resolve(host: &TurnHost, env: &zuno_paths::Env) -> Self {
        Self {
            session: host.session_id().to_owned(),
            harness: lifecycle_services(&host.lifecycle_snapshots()),
            terminal: env
                .value("TERM_PROGRAM")
                .or_else(|| env.value("TERM"))
                .map(str::to_owned),
        }
    }
}

fn lifecycle_services(
    snapshots: &[zuno_runtime::RuntimeSnapshot],
) -> Vec<zuno_tui::views::ambient::Service> {
    use zuno_runtime::LifecycleState;
    use zuno_tui::views::ambient::{Health, Service};

    fn health(state: LifecycleState, runtime: bool) -> Health {
        match state {
            LifecycleState::Active => Health::Ready,
            LifecycleState::Preparing | LifecycleState::Stopping => Health::Pending,
            LifecycleState::Stopped if runtime => Health::Ready,
            LifecycleState::Stopped | LifecycleState::Closed => Health::Disabled,
            LifecycleState::Failed | LifecycleState::Uncertain => Health::Faulted,
        }
    }

    let mut services = Vec::new();
    for runtime in snapshots {
        let profile = runtime.profile_id.as_deref().map_or_else(
            || "scope".to_owned(),
            |profile| format!("profile {profile}"),
        );
        services.push(
            Service::new(
                format!("runtime {}", runtime.name),
                health(runtime.state, true),
            )
            .detailed(format!("{} · {}", lifecycle_state(runtime.state), profile)),
        );
        services.extend(runtime.components.iter().map(|component| {
            Service::new(component.id.clone(), health(component.state, false)).detailed(format!(
                "{} · effects {} · provides {} · requires {}",
                lifecycle_state(component.state),
                component.effects.len(),
                component.provides.len(),
                component.requires.len()
            ))
        }));
        services.extend(runtime.diagnostics.iter().map(|diagnostic| {
            Service::new(
                format!("{}:{}", diagnostic.component_id, diagnostic.effect_id),
                Health::Faulted,
            )
            .detailed(format!(
                "{} {} · {}",
                diagnostic.phase, diagnostic.kind, diagnostic.message
            ))
        }));
    }
    services
}

const fn lifecycle_state(state: zuno_runtime::LifecycleState) -> &'static str {
    match state {
        zuno_runtime::LifecycleState::Preparing => "preparing",
        zuno_runtime::LifecycleState::Active => "active",
        zuno_runtime::LifecycleState::Stopping => "stopping",
        zuno_runtime::LifecycleState::Stopped => "stopped",
        zuno_runtime::LifecycleState::Failed => "failed",
        zuno_runtime::LifecycleState::Uncertain => "uncertain",
        zuno_runtime::LifecycleState::Closed => "closed",
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

/// TUI-only configuration layers, weakest first.
///
/// The filename is `tui.json`/`tui.jsonc` because these keys deliberately do not
/// belong to the main Zuno schema. The global pair under `$XDG_CONFIG_HOME/zuno`
/// is the base; project files then run from the outermost `.zuno` to the nearest,
/// with JSONC after JSON at each level. Since [`ResolvedTuiConfig::discover`]
/// gives later paths precedence, the most specific project layer wins. Building
/// the project target from [`zuno_paths::PROJECT_CONFIG_DIRECTORY`] also makes it
/// impossible for this path to drift back to an `opencode`-named directory.
fn tui_config_paths(config_root: &Path, directory: &Path, worktree: Option<&Path>) -> Vec<PathBuf> {
    const TUI_CONFIG_NAME: &str = "tui";

    let mut paths = zuno_paths::Layout::file_in_directory(config_root, TUI_CONFIG_NAME).to_vec();
    let project_target = Path::new(zuno_paths::PROJECT_CONFIG_DIRECTORY).join(TUI_CONFIG_NAME);
    paths.extend(zuno_paths::config_files(
        &project_target.to_string_lossy(),
        directory,
        worktree,
    ));
    paths
}

/// One enabled language server, as the ambient panel shows it.
///
/// The health distinction is the point. A server whose program is on `PATH` really will
/// start when a file it claims is first read, so `Pending` plus that promise is true. One
/// whose program is missing never will, and the old copy said "starts on first matching
/// file" for it too — a promise the process cannot keep, and indistinguishable from a
/// server that is merely idle. `Faulted` names it as something to fix, because the user
/// enabled it: a built-in nobody asked for is not in this list at all.
fn lsp_service(server: &zuno_lsp::registry::ServerSpec) -> zuno_tui::views::ambient::Service {
    use zuno_lsp::registry::Availability;
    use zuno_tui::views::ambient::{Health, Service};
    let program = server.command.first().map_or("", String::as_str);
    let (health, detail) = match server.availability() {
        Availability::Present => (Health::Pending, "starts on first matching file".to_owned()),
        Availability::Installable => (Health::Pending, format!("installs {program} on first use")),
        Availability::Missing => (Health::Faulted, format!("{program} not found on PATH")),
        Availability::NoCommand => (Health::Faulted, "no command configured".to_owned()),
    };
    Service::new(server.id.clone(), health).detailed(detail)
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
/// produces, so it must not be the thing waiting on the abort. The registry retains an
/// interrupt that arrives during the guard handoff, so an accepted follow-up turn cannot
/// escape a cancellation merely because the previous guard dropped first.
async fn forward_cancellations(
    control: zuno_engine::status::SessionControl,
    mut cancels: mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    cancels.close();
                    return;
                }
            }
            cancellation = cancels.recv() => {
                let Some(()) = cancellation else {
                    return;
                };
                match control.abort() {
                    zuno_engine::status::AbortDisposition::Active => tracing::info!(
                        target: "zuno::tui::cancellation",
                        session_id = %control.session_id(),
                        disposition = "active",
                        "TUI interrupt request fired for the active turn"
                    ),
                    zuno_engine::status::AbortDisposition::ArmedNext => tracing::info!(
                        target: "zuno::tui::cancellation",
                        session_id = %control.session_id(),
                        disposition = "armed_next",
                        "TUI interrupt request was retained across the turn handoff"
                    ),
                }
            }
        }
    }
}

/// Drive one turn per submitted prompt until the screen stops sending.
///
/// Failures are reported through the same channel the turn's own events travel on,
/// because the alternate screen is the only surface the user is looking at: an error
/// on stderr under raw mode is either invisible or corrupts the frame. The interrupt
/// event goes first so the live footer stops claiming a running turn, and the error
/// second so the durable transcript detail remains on screen.
/// What the model, agent and session pickers offer.
///
/// Models are **every provider's**, which is what [`zuno models`] already reports and
/// what this surface used to contradict. The previous bound — the session provider's
/// models alone — was justified as a correctness guard, on the grounds that a turn wires
/// exactly one credential and so another vendor's model could only fail by presenting
/// the wrong key. That reasoning was wrong about this program: a selection does not
/// mutate the live host, it goes back through [`TurnPlan::resolve`]
/// ([`apply_selection`]), which splits the provider off the `/` prefix and re-resolves
/// the credential, the token window and the tool set from *that* provider. Every
/// cross-provider pick a launch could make, the rebuild can make too — so withholding
/// them hid working choices rather than preventing broken ones.
///
/// One list, from [`TurnPlan::catalog_models`], which preserves
/// `Catalog::model_lines` ordering and carries the resolved display names. Two
/// enumerations is precisely how the surfaces came to disagree.
async fn session_catalog(
    plan: &TurnPlan,
    _environment: &StartupEnvironment,
) -> zuno_tui::views::session::SessionCatalog {
    // Reasoning support per row comes from the same `FixedFacts` a delegation resolves a
    // model through, so the picker and the `task` tool cannot disagree about which models
    // reason. An undeclared model is treated as not reasoning: that yields a key which
    // explains itself rather than one that sends a control the provider may reject.
    let model_choices = plan.catalog_models();
    let reasoning_efforts = model_choices
        .iter()
        .map(|choice| (choice.id.clone(), plan.model_reasoning_efforts(&choice.id)))
        .collect();
    let models = model_choices
        .into_iter()
        .map(|choice| {
            let reasoning = plan.model_reasons(&choice.id);
            zuno_tui::views::picker::ModelEntry {
                id: choice.id,
                name: choice.name,
                provider: choice.provider,
                reasoning,
            }
        })
        .collect();
    // Filtered here rather than in `agent::list`, which must keep returning everything: the
    // turn loop resolves a delegation by name and needs the subagents this drops. Both TUI
    // surfaces read this one list — the `<leader>a` picker and the cycling keys — so one
    // filter is what stops them disagreeing about what "the agents" are. A subagent is
    // reachable only by delegation and `hidden` is its author asking not to be offered, so
    // neither is a valid choice for the session's own agent.
    let agents = selectable_session_agents(plan.agents());
    zuno_tui::views::session::SessionCatalog {
        models,
        agents,
        sessions: Vec::new(),
        session: None,
        model: Some(plan.qualified_model()),
        agent: Some(plan.agent_name().to_owned()),
        presets: plan.preset_names(),
        preset: plan.preset_name().map(str::to_owned),
        councils: plan
            .council_choices()
            .into_iter()
            .map(|choice| zuno_tui::views::picker::CouncilEntry {
                name: choice.name,
                description: choice.description,
            })
            .collect(),
        reasoning: plan.reasoning_supported(),
        reasoning_efforts,
        effort: plan.effort(),
    }
}

fn selectable_session_agents(
    agents: &[zuno_catalog::agent::Agent],
) -> Vec<zuno_tui::views::picker::AgentEntry> {
    agents
        .iter()
        .filter(|agent| {
            !matches!(agent.mode, zuno_catalog::agent::AgentMode::Subagent)
                && agent.hidden != Some(true)
        })
        .map(|agent| zuno_tui::views::picker::AgentEntry {
            name: agent.name.clone(),
            description: agent.description.clone().unwrap_or_default(),
        })
        .collect()
}

fn session_entries(host: &TurnHost) -> Result<Vec<zuno_tui::views::picker::SessionEntry>, String> {
    host.recent_sessions(zuno_db::session::UPSTREAM_LIST_LIMIT)
        .map_err(to_string)?
        .into_iter()
        .map(|session| {
            Ok(zuno_tui::views::picker::SessionEntry {
                id: session.id,
                title: session.title,
                when: super::session_list::today_time_or_date_time(session.time_updated)?,
            })
        })
        .collect()
}

/// Apply a picker choice at the boundary between turns.
///
/// Model, agent, and effort changes rebuild only the turn host. A session change remounts
/// the whole TUI composition: transcript replay, cancellation ownership, permission
/// attribution, LSP/MCP workers, snapshot history, and the exit hint all belong to the
/// selected session and must move together. The physical terminal activation is retained
/// by [`MountedTerminal`], so this complete replacement does not flash the primary screen.
///
/// A new host rather than a mutated one is the credential-safety argument: moving a live
/// host to another provider's model in place could present one provider's credential to
/// another endpoint. Going back through [`TurnPlan::resolve`] and [`TurnHost::open`] keeps
/// every rebuilt combination reachable from an ordinary launch.
///
/// A failure leaves the previous host in place and says so on the transcript's own
/// channel. The alternative — tearing down a working host on a bad pick — would lose the
/// session over a keystroke.
struct TurnRebuild<'a> {
    options: &'a TurnOptions,
    environment: &'a StartupEnvironment,
    approval: &'a Arc<dyn PermissionAsker>,
    question: &'a Arc<dyn QuestionAsker>,
    continuity: &'a TuiHostContinuity,
    events: &'a TurnEventSender,
    mcp_catalog: &'a zuno_mcp::Catalog,
}

trait HostLifecycle {
    fn shutdown_host(&mut self) -> BoxFuture<'_, Result<(), String>>;
}

impl HostLifecycle for TurnHost {
    fn shutdown_host(&mut self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.shutdown())
    }
}

/// Stop the previous owner before constructing the replacement.
///
/// [`TurnPlan`] is the side-effect-free preparation result. The closure performs the
/// actual start, so no candidate worker, route, watcher, or provider can overlap the
/// old host's cleanup.
async fn replace_host<H, Start, Started>(current: &mut H, start: Start) -> Result<(), String>
where
    H: HostLifecycle,
    Start: FnOnce() -> Started,
    Started: std::future::Future<Output = Result<H, String>>,
{
    current.shutdown_host().await?;
    let candidate = start().await?;
    *current = candidate;
    Ok(())
}

enum SelectionOutcome {
    Rebuilt(TurnEventSender),
    Remount(RemountRequest),
    Shutdown(String),
    Unchanged,
}

async fn apply_selection(
    selection: zuno_tui::views::session::Selection,
    host: &mut TurnHost,
    rebuild: &TurnRebuild<'_>,
) -> SelectionOutcome {
    let selected_agent = match &selection {
        zuno_tui::views::session::Selection::Agent(agent) if agent != host.agent_name() => {
            Some(agent.clone())
        }
        _ => None,
    };
    let mut next = rebuild.options.clone();
    next.session = host.rebuild_session_choice();
    // Seeded from the live host, not from the launch options, for the reason
    // `refresh_mcp_host` does the same: the host is what the previous selection actually
    // produced. Reading the launch options alone made each pick discard the one before
    // it — choose a model, then an agent, and the model reverted to the launched one.
    next.model = host.model_override().map(str::to_owned);
    next.agent = Some(host.agent_name().to_owned());
    next.preset = host.preset_name().map(str::to_owned);
    next.effort = host.effort_override();
    next.extension_composition = super::turn::ExtensionComposition::Active;
    match selection {
        zuno_tui::views::session::Selection::Model(model) => next.model = Some(model),
        zuno_tui::views::session::Selection::Agent(agent) => next.agent = Some(agent),
        zuno_tui::views::session::Selection::Preset(preset) => {
            next.preset = Some(preset);
            next.model = None;
            next.effort = None;
        }
        // Through the same rebuild as a model change, rather than mutating the live host:
        // the level is resolved against the model's declared variants and capability, so
        // it has to be re-resolved by `TurnPlan::resolve` to become the right provider
        // shape. That is also what makes a level chosen here survive a later model
        // switch, and what silently drops it when the new model does not reason.
        zuno_tui::views::session::Selection::Effort(effort) => next.effort = Some(effort),
        zuno_tui::views::session::Selection::NewSession => {
            next.directory = Some(PathBuf::from(host.session_directory()));
            next.session = SessionChoice::New;
            next.title = None;
            return SelectionOutcome::Remount(RemountRequest::fresh_conversation(next));
        }
        zuno_tui::views::session::Selection::Session(session_id) => {
            if session_id == host.session_id() {
                return SelectionOutcome::Unchanged;
            }
            let target = match host.switchable_session(&session_id) {
                Ok(Some(target)) => target,
                Ok(None) => {
                    let _reported = rebuild
                        .events
                        .publish(TurnEvent::Provider {
                            step: 0,
                            event: StreamEvent::StatusDetail {
                                detail: format!(
                                    "warning: session {session_id} is no longer switchable here"
                                ),
                            },
                        })
                        .await;
                    return SelectionOutcome::Unchanged;
                }
                Err(error) => {
                    let _reported = rebuild
                        .events
                        .publish(TurnEvent::Provider {
                            step: 0,
                            event: StreamEvent::StatusDetail {
                                detail: format!(
                                    "warning: could not validate session {session_id}: {error}"
                                ),
                            },
                        })
                        .await;
                    return SelectionOutcome::Unchanged;
                }
            };
            if let Some(agent) = target.agent.clone() {
                next.agent = Some(agent);
            }
            next.directory = Some(PathBuf::from(target.directory));
            next.session = SessionChoice::Existing(target.id);
            return SelectionOutcome::Remount(RemountRequest::plain(next));
        }
        zuno_tui::views::session::Selection::SessionRename { id, title } => {
            let title = title.trim();
            if title.is_empty() {
                let _reported = rebuild
                    .events
                    .publish(TurnEvent::Provider {
                        step: 0,
                        event: StreamEvent::StatusDetail {
                            detail: String::from("warning: session title cannot be empty"),
                        },
                    })
                    .await;
                return SelectionOutcome::Unchanged;
            }
            let target = match host.switchable_session(&id) {
                Ok(Some(target)) => target,
                Ok(None) => {
                    let _reported = rebuild
                        .events
                        .publish(TurnEvent::Provider {
                            step: 0,
                            event: StreamEvent::StatusDetail {
                                detail: format!("warning: session {id} is no longer editable here"),
                            },
                        })
                        .await;
                    return SelectionOutcome::Unchanged;
                }
                Err(error) => {
                    let _reported = rebuild
                        .events
                        .publish(TurnEvent::Provider {
                            step: 0,
                            event: StreamEvent::StatusDetail {
                                detail: format!(
                                    "warning: could not validate session {id}: {error}"
                                ),
                            },
                        })
                        .await;
                    return SelectionOutcome::Unchanged;
                }
            };
            if let Err(error) = host.rename_session(&id, title) {
                let _reported = rebuild
                    .events
                    .publish(TurnEvent::Provider {
                        step: 0,
                        event: StreamEvent::StatusDetail {
                            detail: format!("warning: could not rename session {id}: {error}"),
                        },
                    })
                    .await;
                return SelectionOutcome::Unchanged;
            }
            next.directory = Some(PathBuf::from(target.directory));
            return SelectionOutcome::Remount(RemountRequest::plain(next));
        }
        zuno_tui::views::session::Selection::SessionDelete(id) => {
            let target = match host.switchable_session(&id) {
                Ok(Some(target)) => target,
                Ok(None) => {
                    let _reported = rebuild
                        .events
                        .publish(TurnEvent::Provider {
                            step: 0,
                            event: StreamEvent::StatusDetail {
                                detail: format!(
                                    "warning: session {id} is no longer deletable here"
                                ),
                            },
                        })
                        .await;
                    return SelectionOutcome::Unchanged;
                }
                Err(error) => {
                    let _reported = rebuild
                        .events
                        .publish(TurnEvent::Provider {
                            step: 0,
                            event: StreamEvent::StatusDetail {
                                detail: format!(
                                    "warning: could not validate session {id}: {error}"
                                ),
                            },
                        })
                        .await;
                    return SelectionOutcome::Unchanged;
                }
            };
            let deleting_current = id == host.session_id();
            if deleting_current && host.has_running_background_tasks() {
                let _reported = rebuild
                    .events
                    .publish(TurnEvent::Provider {
                        step: 0,
                        event: StreamEvent::StatusDetail {
                            detail: format!(
                                "warning: session {id} still has background subagents running; \
                                 wait for them to finish before deleting it"
                            ),
                        },
                    })
                    .await;
                return SelectionOutcome::Unchanged;
            }
            let replacement = if deleting_current {
                match host.recent_sessions(zuno_db::session::UPSTREAM_LIST_LIMIT) {
                    Ok(sessions) => sessions.into_iter().find(|session| session.id != id),
                    Err(error) => {
                        let _reported = rebuild
                            .events
                            .publish(TurnEvent::Provider {
                                step: 0,
                                event: StreamEvent::StatusDetail {
                                    detail: format!(
                                        "warning: could not choose a session after deleting {id}: {error}"
                                    ),
                                },
                            })
                            .await;
                        return SelectionOutcome::Unchanged;
                    }
                }
            } else {
                None
            };
            if let Err(error) = host.delete_session(&id) {
                let _reported = rebuild
                    .events
                    .publish(TurnEvent::Provider {
                        step: 0,
                        event: StreamEvent::StatusDetail {
                            detail: format!("warning: could not delete session {id}: {error}"),
                        },
                    })
                    .await;
                return SelectionOutcome::Unchanged;
            }
            next.directory = Some(PathBuf::from(target.directory));
            if deleting_current {
                match replacement {
                    Some(session) => {
                        if let Some(agent) = session.agent {
                            next.agent = Some(agent);
                        }
                        next.session = SessionChoice::Existing(session.id);
                    }
                    None => next.session = SessionChoice::New,
                }
                next.title = None;
            }
            return SelectionOutcome::Remount(RemountRequest::reopening_sessions(next));
        }
        zuno_tui::views::session::Selection::JobCancel(job_id) => {
            let detail = match host.cancel_job(&job_id).await {
                Ok(outcome) => outcome.message,
                Err(error) => format!("warning: could not cancel job {job_id}: {error}"),
            };
            let _reported = rebuild
                .events
                .publish(TurnEvent::Provider {
                    step: 0,
                    event: StreamEvent::StatusDetail { detail },
                })
                .await;
            return SelectionOutcome::Unchanged;
        }
        zuno_tui::views::session::Selection::MemoryApply(id) => {
            report_memory_action(
                rebuild.events,
                host.memory_apply(&id),
                format!("memory candidate {id} approved"),
            )
            .await;
            return SelectionOutcome::Unchanged;
        }
        zuno_tui::views::session::Selection::MemoryReject(id) => {
            report_memory_action(
                rebuild.events,
                host.memory_reject(&id),
                format!("memory candidate {id} rejected"),
            )
            .await;
            return SelectionOutcome::Unchanged;
        }
        zuno_tui::views::session::Selection::MemoryUndo(id) => {
            report_memory_action(
                rebuild.events,
                host.memory_undo(&id),
                format!("memory candidate {id} undone"),
            )
            .await;
            return SelectionOutcome::Unchanged;
        }
        zuno_tui::views::session::Selection::MemoryEditApply { id, content } => {
            report_memory_action(
                rebuild.events,
                host.memory_edit_and_apply(&id, content),
                format!("edited memory candidate {id} approved"),
            )
            .await;
            return SelectionOutcome::Unchanged;
        }
        zuno_tui::views::session::Selection::MemoryRemove { scope, content } => {
            report_memory_action(
                rebuild.events,
                host.memory_remove(scope, content),
                format!("{} resident memory removed", scope.as_str()),
            )
            .await;
            return SelectionOutcome::Unchanged;
        }
        // A theme is owned and applied entirely by the view layer.
        zuno_tui::views::session::Selection::Theme(_) => return SelectionOutcome::Unchanged,
    }
    let plan = match TurnPlan::resolve(&next, rebuild.environment).await {
        Ok(plan) => plan,
        Err(message) => {
            let _reported = rebuild
                .events
                .publish(TurnEvent::Provider {
                    step: 0,
                    event: StreamEvent::StatusDetail {
                        detail: format!("warning: keeping the current turn host: {message}"),
                    },
                })
                .await;
            return SelectionOutcome::Unchanged;
        }
    };
    let continuity = rebuild.continuity.clone();
    match replace_host(host, || async move {
        continuity
            .open_host(
                plan,
                rebuild.environment,
                Arc::clone(rebuild.approval),
                Arc::clone(rebuild.question),
                rebuild.mcp_catalog.clone(),
            )
            .await
    })
    .await
    {
        Ok(()) => {
            if selected_agent.is_some()
                && let Err(error) = host.persist_active_agent()
            {
                return SelectionOutcome::Shutdown(format!(
                    "the collaboration mode changed in memory but could not be persisted: {error}"
                ));
            }
            SelectionOutcome::Rebuilt(rebuild.events.clone())
        }
        Err(message) => SelectionOutcome::Shutdown(format!(
            "turn host replacement could not establish a quiescent composition: {message}"
        )),
    }
}

async fn report_memory_action(
    events: &TurnEventSender,
    result: Result<(), String>,
    success: String,
) {
    let detail = result.map_or_else(
        |error| format!("warning: memory action failed: {error}"),
        |()| success,
    );
    let _reported = events
        .publish(TurnEvent::Provider {
            step: 0,
            event: StreamEvent::StatusDetail { detail },
        })
        .await;
}

struct TurnDriver {
    host: TurnHost,
    options: TurnOptions,
    approval: Arc<dyn PermissionAsker>,
    question: Arc<dyn QuestionAsker>,
    reference_root: PathBuf,
    mcp_catalog: zuno_mcp::Catalog,
    mcp_dirty: Arc<AtomicBool>,
    snapshots: SnapshotHistory,
    work_state: WorkState,
    work_wake: mpsc::Sender<TerminalEvent>,
    queued_inputs: QueuedInputProjection,
    queue_wake: mpsc::Sender<TerminalEvent>,
    continuity: TuiHostContinuity,
    interactive_children: InteractiveChildInput,
    remount: CompositionRemount,
    shutdown: mpsc::Sender<TerminalEvent>,
}

#[derive(Clone, Default)]
struct CompositionRemount(Arc<Mutex<Option<RemountRequest>>>);

impl CompositionRemount {
    fn request(&self, request: RemountRequest) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
    }

    fn take(&self) -> Option<RemountRequest> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

struct SnapshotHistory {
    store: zuno_snapshot::Store,
    undo: Vec<zuno_snapshot::TurnCheckpoint>,
    redo: Vec<zuno_snapshot::TurnCheckpoint>,
}

impl SnapshotHistory {
    fn new(store: zuno_snapshot::Store) -> Self {
        Self {
            store,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct DriverPrompt {
    submission: PromptSubmission,
    promoted_message_id: Option<String>,
}

impl DriverPrompt {
    fn direct(submission: PromptSubmission) -> Self {
        Self {
            submission,
            promoted_message_id: None,
        }
    }

    fn promoted(input_id: String, submission: PromptSubmission) -> Self {
        Self {
            submission,
            promoted_message_id: Some(input_id),
        }
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum PersistedTuiInput {
    TuiPrompt { submission: PromptSubmission },
}

fn dispatch_child_prompt(
    children: &InteractiveChildInput,
    session_id: &str,
    submission: PromptSubmission,
) -> Result<(), String> {
    let text = match &submission {
        PromptSubmission::Text(text) | PromptSubmission::Content { text, .. } => text.clone(),
        PromptSubmission::Steer(inner) => match inner.as_ref() {
            PromptSubmission::Text(text) | PromptSubmission::Content { text, .. } => text.clone(),
            _ => {
                return Err(
                    "an attached child session accepts text and image input only".to_owned(),
                );
            }
        },
        _ => {
            return Err("an attached child session accepts text and image input only".to_owned());
        }
    };
    let prompt =
        serde_json::to_value(PersistedTuiInput::TuiPrompt { submission }).map_err(to_string)?;
    children.submit_text(session_id, prompt, text)?;
    Ok(())
}

fn report_child_prompt_failure(
    observer: Option<Arc<dyn ChildTurnObserver>>,
    session_id: &str,
    message: String,
) {
    if let Some(observer) = observer {
        observer.event(
            session_id,
            &TurnEvent::TurnFailed {
                assistant_message_id: None,
                steps: 0,
                message: format!("input was not admitted: {message}"),
            },
        );
    }
}

fn route_targeted_prompt(
    children: &InteractiveChildInput,
    observer: Option<Arc<dyn ChildTurnObserver>>,
    prompt: TargetedPromptSubmission,
    root: &mut VecDeque<PromptSubmission>,
) {
    match prompt.target {
        PromptTarget::Root => root.push_back(prompt.submission),
        PromptTarget::Session(session_id) => {
            if let Err(error) = dispatch_child_prompt(children, &session_id, prompt.submission) {
                report_child_prompt_failure(observer, &session_id, error);
            }
        }
    }
}

fn project_queued_inputs(
    inbox: &zuno_db::inbox::SessionInbox,
    session_id: &str,
) -> Result<Vec<QueuedInputEntry>, String> {
    let mut projected = Vec::new();
    for input in inbox.pending(session_id).map_err(to_string)? {
        if input.prompt.get("kind").and_then(serde_json::Value::as_str) != Some("tuiPrompt") {
            continue;
        }
        let PersistedTuiInput::TuiPrompt { submission } =
            serde_json::from_value(input.prompt).map_err(to_string)?;
        let (text, editable) = queued_submission_display(&submission);
        projected.push(QueuedInputEntry {
            id: input.id,
            text,
            delivery: match input.delivery {
                zuno_db::inbox::InputDelivery::Queue => QueuedInputDelivery::Queue,
                zuno_db::inbox::InputDelivery::Steer => QueuedInputDelivery::Steer,
            },
            revision: input.revision,
            editable,
        });
    }
    Ok(projected)
}

fn queued_submission_display(submission: &PromptSubmission) -> (String, bool) {
    match submission {
        PromptSubmission::Text(text) | PromptSubmission::Content { text, .. } => {
            (text.clone(), true)
        }
        PromptSubmission::Command { name, arguments }
        | PromptSubmission::Skill {
            name, arguments, ..
        } => (
            format!(
                "/{name}{}",
                if arguments.is_empty() {
                    String::new()
                } else {
                    format!(" {arguments}")
                }
            ),
            false,
        ),
        PromptSubmission::Council { text, .. } => (text.clone(), false),
        PromptSubmission::Host(command) => (
            match command {
                HostCommand::Compact => "/compact".to_owned(),
                HostCommand::Undo => "/undo".to_owned(),
                HostCommand::Redo => "/redo".to_owned(),
                HostCommand::Goal(arguments) => format!("/goal {arguments}"),
                HostCommand::Preset(Some(preset)) => format!("/preset {preset}"),
                HostCommand::Preset(None) => "/preset".to_owned(),
                HostCommand::Council(arguments) => format!("/council {arguments}"),
                HostCommand::Plan => "/plan".to_owned(),
                HostCommand::StartPlan => "/start-plan".to_owned(),
                HostCommand::StartWork => "/start-work".to_owned(),
                HostCommand::Stop(Some(id)) => format!("/stop {id}"),
                HostCommand::Stop(None) => "/stop".to_owned(),
            },
            false,
        ),
        PromptSubmission::Queue(inner) | PromptSubmission::Steer(inner) => {
            queued_submission_display(inner)
        }
    }
}

fn refresh_queued_input_projection(
    inbox: &zuno_db::inbox::SessionInbox,
    session_id: &str,
    projection: &QueuedInputProjection,
    wake: &mpsc::Sender<TerminalEvent>,
    notice: Option<QueuedInputNotice>,
) {
    match project_queued_inputs(inbox, session_id) {
        Ok(inputs) => projection.publish(inputs, notice),
        Err(error) => projection.publish(
            projection.snapshot(),
            Some(QueuedInputNotice {
                input_id: String::new(),
                kind: QueuedInputNoticeKind::Failed(format!(
                    "queued inputs changed but could not be refreshed: {error}"
                )),
            }),
        ),
    }
    let _nudged = wake.try_send(TerminalEvent::Wake);
}

async fn apply_queued_input_mutation(
    inbox: zuno_db::inbox::SessionInbox,
    control: zuno_engine::status::SessionControl,
    session_id: String,
    reference_root: PathBuf,
    projection: QueuedInputProjection,
    wake: mpsc::Sender<TerminalEvent>,
    mutation: QueuedInputMutation,
) {
    let (input_id, outcome) = match mutation {
        QueuedInputMutation::Edit {
            id,
            expected_revision,
            text,
        } => {
            let outcome: Result<QueuedInputNoticeKind, String> = async {
                let current = inbox
                    .get(&session_id, &id)
                    .map_err(to_string)?
                    .ok_or_else(|| format!("queued input `{id}` no longer exists"))?;
                let delivery = current.delivery;
                let submission = super::tui_reference::resolve_submission(
                    &reference_root,
                    PromptSubmission::Text(text),
                )
                .await?;
                let submission = if delivery == zuno_db::inbox::InputDelivery::Steer {
                    PromptSubmission::Steer(Box::new(submission))
                } else {
                    submission
                };
                inbox
                    .edit_pending(
                        &session_id,
                        &id,
                        expected_revision,
                        serde_json::to_value(PersistedTuiInput::TuiPrompt {
                            submission: submission.clone(),
                        })
                        .map_err(to_string)?,
                        zuno_db::message::now_millis(),
                    )
                    .map_err(to_string)?;
                if delivery == zuno_db::inbox::InputDelivery::Steer {
                    let _removed = control.cancel_soft_interrupt(&id);
                    if let Some(message) = soft_interrupt(&id, &submission) {
                        let _queued = control.queue_soft_interrupt(message);
                    }
                }
                Ok(QueuedInputNoticeKind::Edited)
            }
            .await;
            (id, outcome)
        }
        QueuedInputMutation::Cancel {
            id,
            expected_revision,
        } => {
            let outcome: Result<QueuedInputNoticeKind, String> = (|| {
                let cancelled = inbox
                    .cancel_pending(
                        &session_id,
                        &id,
                        expected_revision,
                        zuno_db::message::now_millis(),
                    )
                    .map_err(to_string)?;
                if cancelled.delivery == zuno_db::inbox::InputDelivery::Steer {
                    let _removed = control.cancel_soft_interrupt(&id);
                }
                Ok(QueuedInputNoticeKind::Cancelled)
            })();
            (id, outcome)
        }
    };
    let kind = outcome.unwrap_or_else(|error| {
        QueuedInputNoticeKind::Failed(format!(
            "queued input `{input_id}` was not changed: {error}"
        ))
    });
    refresh_queued_input_projection(
        &inbox,
        &session_id,
        &projection,
        &wake,
        Some(QueuedInputNotice { input_id, kind }),
    );
}

async fn drive_turns(
    mut driver: TurnDriver,
    mut prompts: mpsc::Receiver<TargetedPromptSubmission>,
    mut selections: mpsc::Receiver<zuno_tui::views::session::Selection>,
    mut queue_mutations: mpsc::Receiver<QueuedInputMutation>,
    environment: StartupEnvironment,
    mut events: TurnEventSender,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let mut work_changes = driver.host.work_state_changes();
    let mut queue_mutations_open = true;
    let mut root_prompts = VecDeque::new();
    'driver: loop {
        while let Ok(prompt) = prompts.try_recv() {
            route_targeted_prompt(
                &driver.interactive_children,
                driver.continuity.child_observer(),
                prompt,
                &mut root_prompts,
            );
        }
        loop {
            match queue_mutations.try_recv() {
                Ok(mutation) => {
                    apply_queued_input_mutation(
                        driver.host.session_inbox(),
                        driver.host.control(),
                        driver.host.session_id().to_owned(),
                        driver.reference_root.clone(),
                        driver.queued_inputs.clone(),
                        driver.queue_wake.clone(),
                        mutation,
                    )
                    .await;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    queue_mutations_open = false;
                    break;
                }
            }
        }
        let queued = if root_prompts.is_empty() && selections.is_empty() {
            zuno_goal::QueuedUserInput::Absent
        } else {
            zuno_goal::QueuedUserInput::Present
        };
        match driver
            .host
            .continue_goal_if_idle(queued, events.clone())
            .await
        {
            Ok(true) => {
                refresh_work_state(
                    &mut driver.host,
                    &driver.work_state,
                    &driver.work_wake,
                    &events,
                )
                .await;
                work_changes.borrow_and_update();
                continue;
            }
            Ok(false) => {}
            Err(message) => {
                report_turn_failure(&events, message).await;
            }
        }
        let pending =
            match promote_pending_prompt(&driver.host, &driver.queued_inputs, &driver.queue_wake) {
                Ok(pending) => pending,
                Err(message) => {
                    report_turn_failure(&events, message.clone()).await;
                    let shutdown = driver.host.shutdown().await;
                    return match shutdown {
                        Ok(()) => Err(message),
                        Err(error) => Err(format!(
                            "{message}; final turn host shutdown also failed: {error}"
                        )),
                    };
                }
            };
        // A selection is taken only between turns, never during one: rebuilding the host
        // mid-turn would drop the stream the loop is still reading.
        let prompt = match pending {
            Some(pending) => pending,
            None if !root_prompts.is_empty() => DriverPrompt::direct(
                root_prompts
                    .pop_front()
                    .expect("the non-empty root prompt queue has a front"),
            ),
            None => tokio::select! {
                biased;
                prompt = prompts.recv() => match prompt {
                    Some(TargetedPromptSubmission {
                        target: PromptTarget::Root,
                        submission: prompt @ PromptSubmission::Queue(_),
                    }) => {
                        if let Err(message) = admit_followup(
                            driver.host.session_inbox(),
                            driver.host.control(),
                            driver.reference_root.clone(),
                            driver.queued_inputs.clone(),
                            driver.queue_wake.clone(),
                            prompt,
                        ).await {
                            report_input_failure(&events, message).await;
                        }
                        continue 'driver;
                    }
                    Some(TargetedPromptSubmission {
                        target: PromptTarget::Root,
                        submission,
                    }) => DriverPrompt::direct(submission),
                    Some(TargetedPromptSubmission {
                        target: PromptTarget::Session(session_id),
                        submission,
                    }) => {
                        if let Err(error) = dispatch_child_prompt(
                            &driver.interactive_children,
                            &session_id,
                            submission,
                        ) {
                            report_child_prompt_failure(
                                driver.continuity.child_observer(),
                                &session_id,
                                error,
                            );
                        }
                        continue 'driver;
                    }
                    None => break 'driver,
                },
                mutation = queue_mutations.recv(), if queue_mutations_open => {
                    match mutation {
                        Some(mutation) => apply_queued_input_mutation(
                            driver.host.session_inbox(),
                            driver.host.control(),
                            driver.host.session_id().to_owned(),
                            driver.reference_root.clone(),
                            driver.queued_inputs.clone(),
                            driver.queue_wake.clone(),
                            mutation,
                        ).await,
                        None => queue_mutations_open = false,
                    }
                    continue;
                },
                selection = selections.recv() => {
                    let Some(selection) = selection else { break 'driver };
                    let rebuild = TurnRebuild {
                        options: &driver.options,
                        environment: &environment,
                        approval: &driver.approval,
                        question: &driver.question,
                        continuity: &driver.continuity,
                        events: &events,
                        mcp_catalog: &driver.mcp_catalog,
                    };
                    match apply_selection(
                        selection,
                        &mut driver.host,
                        &rebuild,
                    )
                    .await
                    {
                        SelectionOutcome::Rebuilt(rebuilt) => {
                            events = rebuilt;
                            work_changes = driver.host.work_state_changes();
                        }
                        SelectionOutcome::Remount(request) => {
                            driver.remount.request(request);
                            let _stopping = driver.shutdown.send(TerminalEvent::Shutdown).await;
                            break 'driver;
                        }
                        SelectionOutcome::Shutdown(message) => {
                            report_turn_failure(&events, message.clone()).await;
                            let _stopping = driver.shutdown.send(TerminalEvent::Shutdown).await;
                            let shutdown = driver.host.shutdown().await;
                            return match shutdown {
                                Ok(()) => Err(message),
                                Err(error) => Err(format!(
                                    "{message}; final turn host shutdown also failed: {error}"
                                )),
                            };
                        }
                        SelectionOutcome::Unchanged => {}
                    }
                    refresh_work_state(
                        &mut driver.host,
                        &driver.work_state,
                        &driver.work_wake,
                        &events,
                    )
                    .await;
                    work_changes.borrow_and_update();
                    continue;
                },
                changed = shutdown.changed() => {
                    let _changed = changed;
                    break 'driver;
                },
                changed = work_changes.changed() => {
                    if changed.is_err() {
                        work_changes = driver.host.work_state_changes();
                    }
                    refresh_work_state(
                        &mut driver.host,
                        &driver.work_state,
                        &driver.work_wake,
                        &events,
                    )
                    .await;
                    work_changes.borrow_and_update();
                    continue;
                }
            },
        };
        if driver.mcp_dirty.swap(false, Ordering::AcqRel) {
            match refresh_mcp_host(&mut driver, &environment, &events).await {
                Ok(Some(rebuilt)) => {
                    events = rebuilt;
                    work_changes = driver.host.work_state_changes();
                }
                Ok(None) => {}
                Err(message) => {
                    report_turn_failure(&events, message.clone()).await;
                    let _stopping = driver.shutdown.send(TerminalEvent::Shutdown).await;
                    let shutdown = driver.host.shutdown().await;
                    return match shutdown {
                        Ok(()) => Err(message),
                        Err(error) => Err(format!(
                            "{message}; final turn host shutdown also failed: {error}"
                        )),
                    };
                }
            }
        }
        drive_one(
            &mut driver,
            prompt,
            &mut prompts,
            &mut root_prompts,
            &mut queue_mutations,
            &mut queue_mutations_open,
            &events,
        )
        .await;
        refresh_work_state(
            &mut driver.host,
            &driver.work_state,
            &driver.work_wake,
            &events,
        )
        .await;
        work_changes.borrow_and_update();
        if environment
            .extensions()
            .desired_revision(driver.host.extension_scope())
            != driver.host.extension_revision()
        {
            let mut next = driver.options.clone();
            next.session = driver.host.rebuild_session_choice();
            next.model = driver.host.model_override().map(str::to_owned);
            next.agent = Some(driver.host.agent_name().to_owned());
            next.preset = driver.host.preset_name().map(str::to_owned);
            next.effort = driver.host.effort_override();
            next.extension_composition = super::turn::ExtensionComposition::Desired;
            driver.remount.request(RemountRequest::plain(next));
            let _stopping = driver.shutdown.send(TerminalEvent::Shutdown).await;
            break 'driver;
        }
    }
    driver.host.shutdown().await
}

async fn drive_background_projection(
    service: Arc<zuno_pty::BackgroundExecutionService>,
    session_id: String,
    projection: WorkState,
    wake: mpsc::Sender<TerminalEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut changes = service.subscribe();
    refresh_background_projection(&service, &session_id, &projection, &wake);
    loop {
        tokio::select! {
            changed = changes.recv() => {
                let refresh = match changed {
                    Ok(zuno_pty::BackgroundExecutionEvent::Created(info)
                        | zuno_pty::BackgroundExecutionEvent::Settled(info)) => {
                            info.session_id == session_id
                        }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if refresh {
                    refresh_background_projection(&service, &session_id, &projection, &wake);
                }
            }
            changed = shutdown.changed() => {
                let _changed = changed;
                break;
            }
        }
    }
}

fn refresh_background_projection(
    service: &zuno_pty::BackgroundExecutionService,
    session_id: &str,
    projection: &WorkState,
    wake: &mpsc::Sender<TerminalEvent>,
) {
    let generation = projection.generation();
    projection.replace_background_executions(background_execution_projections(
        service,
        session_id,
        zuno_db::message::now_millis(),
    ));
    if projection.generation() != generation {
        let _nudged = wake.try_send(TerminalEvent::Wake);
    }
}

async fn refresh_work_state(
    host: &mut TurnHost,
    projection: &WorkState,
    wake: &mpsc::Sender<TerminalEvent>,
    events: &TurnEventSender,
) {
    match host.work_state() {
        Ok(state) => {
            let generation = projection.generation();
            projection.replace(state);
            if projection.generation() != generation {
                let _nudged = wake.try_send(TerminalEvent::Wake);
            }
        }
        Err(error) => {
            let _reported = events
                .publish(TurnEvent::Provider {
                    step: 0,
                    event: StreamEvent::StatusDetail {
                        detail: format!(
                            "warning: durable work state could not be refreshed: {error}"
                        ),
                    },
                })
                .await;
        }
    }
}

async fn refresh_mcp_host(
    driver: &mut TurnDriver,
    environment: &StartupEnvironment,
    events: &TurnEventSender,
) -> Result<Option<TurnEventSender>, String> {
    let mut next = driver.options.clone();
    next.session = driver.host.rebuild_session_choice();
    next.model = Some(driver.host.qualified_model());
    next.agent = Some(driver.host.agent_name().to_owned());
    next.extension_composition = super::turn::ExtensionComposition::Active;
    let plan = match TurnPlan::resolve(&next, environment).await {
        Ok(plan) => plan,
        Err(message) => {
            driver.mcp_dirty.store(true, Ordering::Release);
            let _reported = events
                .publish(TurnEvent::Provider {
                    step: 0,
                    event: StreamEvent::StatusDetail {
                        detail: format!("warning: MCP tools were not refreshed: {message}"),
                    },
                })
                .await;
            return Ok(None);
        }
    };
    let approval = Arc::clone(&driver.approval);
    let question = Arc::clone(&driver.question);
    let mcp_catalog = driver.mcp_catalog.clone();
    let continuity = driver.continuity.clone();
    replace_host(&mut driver.host, || async move {
        continuity
            .open_host(plan, environment, approval, question, mcp_catalog)
            .await
    })
    .await
    .map(|()| Some(events.clone()))
    .map_err(|error| {
        format!("MCP host refresh could not establish a quiescent composition: {error}")
    })
}

fn project_mcp_snapshots(snapshots: &[zuno_mcp::McpServerSnapshot]) -> Vec<McpServer> {
    snapshots
        .iter()
        .map(|snapshot| McpServer {
            name: snapshot.server.clone(),
            state: match &snapshot.state {
                zuno_mcp::McpServerState::Disabled => McpState::Disabled,
                zuno_mcp::McpServerState::Connecting => McpState::Connecting,
                zuno_mcp::McpServerState::Connected => McpState::Connected,
                zuno_mcp::McpServerState::Disconnecting => McpState::Disconnecting,
                zuno_mcp::McpServerState::Failed { error } => McpState::Failed(error.clone()),
                zuno_mcp::McpServerState::NeedsAuth => McpState::NeedsAuth,
                zuno_mcp::McpServerState::NeedsClientRegistration { error } => {
                    McpState::NeedsClientRegistration(error.clone())
                }
            },
            desired_enabled: snapshot.desired_enabled,
        })
        .collect()
}

struct McpLifecycleWorker {
    controller: zuno_mcp::McpServerController,
    requests: mpsc::Receiver<McpToggleRequest>,
    initial: Vec<McpToggleRequest>,
    concurrency: NonZeroUsize,
    projection: McpProjection,
    dirty: Arc<AtomicBool>,
    wake: mpsc::Sender<TerminalEvent>,
    shutdown: watch::Receiver<bool>,
}

async fn drive_mcp_lifecycle(worker: McpLifecycleWorker) {
    type ToggleResult = Result<zuno_mcp::McpServerSnapshot, zuno_mcp::McpLifecycleError>;
    type ToggleFuture = BoxFuture<'static, (String, ToggleResult)>;

    let McpLifecycleWorker {
        controller,
        mut requests,
        initial,
        concurrency,
        projection,
        dirty,
        wake,
        mut shutdown,
    } = worker;
    let mut changes = controller.subscribe();
    let mut pending = VecDeque::from(initial);
    let mut active = FuturesUnordered::<ToggleFuture>::new();
    let mut active_servers = BTreeSet::new();
    let mut requests_open = true;
    loop {
        while active.len() < concurrency.get() {
            let Some(index) = pending
                .iter()
                .position(|request| !active_servers.contains(&request.server))
            else {
                break;
            };
            let request = pending
                .remove(index)
                .expect("eligible pending MCP request exists");
            let controller = controller.clone();
            let server = request.server.clone();
            active_servers.insert(server.clone());
            active.push(Box::pin(async move {
                let result = controller
                    .set_enabled(&server, request.desired_enabled)
                    .await;
                (server, result)
            }));
        }

        if !requests_open && pending.is_empty() && active.is_empty() {
            break;
        }
        tokio::select! {
            change = changes.recv() => match change {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    projection.replace(project_mcp_snapshots(&controller.snapshots()));
                    dirty.store(true, Ordering::Release);
                    let _nudged = wake.try_send(TerminalEvent::Wake);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            result = active.next(), if !active.is_empty() => {
                let (server, _completed) = result.expect("guarded active MCP operation");
                active_servers.remove(&server);
                projection.replace(project_mcp_snapshots(&controller.snapshots()));
                dirty.store(true, Ordering::Release);
                let _nudged = wake.try_send(TerminalEvent::Wake);
            },
            request = requests.recv(), if requests_open => match request {
                Some(request) => pending.push_back(request),
                None => requests_open = false,
            },
            changed = shutdown.changed() => {
                let _changed = changed;
                break;
            },
        }
    }
    let servers = controller
        .snapshots()
        .into_iter()
        .map(|snapshot| snapshot.server)
        .collect::<Vec<_>>();
    futures::stream::iter(servers.into_iter().map(|server| {
        let controller = controller.clone();
        async move {
            let _result = controller.set_enabled(&server, false).await;
        }
    }))
    .buffered(concurrency.get())
    .collect::<Vec<_>>()
    .await;
}

async fn drive_one(
    driver: &mut TurnDriver,
    prompt: DriverPrompt,
    prompts: &mut mpsc::Receiver<TargetedPromptSubmission>,
    root_prompts: &mut VecDeque<PromptSubmission>,
    queue_mutations: &mut mpsc::Receiver<QueuedInputMutation>,
    queue_mutations_open: &mut bool,
    events: &TurnEventSender,
) {
    let interactive_children = driver.interactive_children.clone();
    let child_observer = driver.continuity.child_observer();
    let TurnDriver {
        host,
        reference_root,
        queued_inputs,
        queue_wake,
        snapshots,
        ..
    } = driver;
    {
        // Counted for the memory sampler's session attribution, which is what tells
        // "one session leaking" from "many sessions, each fine". A guard rather than a
        // manual increment so an early `?` or a panic cannot leave the count high.
        let _session = zuno_observability::memory::SessionCount::enter();
        let outcome = async {
            let DriverPrompt {
                submission,
                promoted_message_id,
            } = prompt;
            let prompt =
                super::tui_reference::resolve_submission(reference_root, submission).await?;
            let prompt = match prompt {
                PromptSubmission::Queue(prompt) | PromptSubmission::Steer(prompt) => *prompt,
                prompt => prompt,
            };
            if let PromptSubmission::Host(command) = prompt {
                return execute_host_command(host, command, snapshots, events).await;
            }
            let capture = begin_snapshot(&snapshots.store, events).await;
            let inbox = host.session_inbox();
            let control = host.control();
            let admission_root = reference_root.to_path_buf();
            let admission_events = events.clone();
            let mut admissions: FuturesUnordered<BoxFuture<'static, Result<(), String>>> =
                FuturesUnordered::new();
            while let Some(followup) = root_prompts.pop_front() {
                admissions.push(Box::pin(admit_followup(
                    inbox.clone(),
                    control.clone(),
                    admission_root.clone(),
                    queued_inputs.clone(),
                    queue_wake.clone(),
                    followup,
                )));
            }
            let mut prompts_open = true;
            let mut turn = Box::pin(drive_submission(
                host,
                prompt,
                promoted_message_id.as_deref(),
                events.clone(),
            ));
            let turn_outcome = loop {
                tokio::select! {
                    biased;
                    outcome = &mut turn => break outcome,
                    Some(outcome) = admissions.next(), if !admissions.is_empty() => {
                        if let Err(message) = outcome {
                            report_input_failure(&admission_events, message).await;
                        }
                    }
                    mutation = queue_mutations.recv(), if *queue_mutations_open => {
                        match mutation {
                            Some(mutation) => apply_queued_input_mutation(
                                inbox.clone(),
                                control.clone(),
                                control.session_id().to_owned(),
                                admission_root.clone(),
                                queued_inputs.clone(),
                                queue_wake.clone(),
                                mutation,
                            ).await,
                            None => *queue_mutations_open = false,
                        }
                    }
                    followup = prompts.recv(), if prompts_open => {
                        match followup {
                            Some(TargetedPromptSubmission {
                                target: PromptTarget::Root,
                                submission,
                            }) => admissions.push(Box::pin(admit_followup(
                                    inbox.clone(),
                                    control.clone(),
                                    admission_root.clone(),
                                    queued_inputs.clone(),
                                    queue_wake.clone(),
                                    submission,
                                ))),
                            Some(TargetedPromptSubmission {
                                target: PromptTarget::Session(session_id),
                                submission,
                            }) => {
                                if let Err(error) = dispatch_child_prompt(
                                    &interactive_children,
                                    &session_id,
                                    submission,
                                ) {
                                    report_child_prompt_failure(
                                        child_observer.as_ref().map(Arc::clone),
                                        &session_id,
                                        error,
                                    );
                                }
                            }
                            None => prompts_open = false,
                        }
                    }
                }
            };
            drop(turn);
            while let Ok(followup) = prompts.try_recv() {
                match followup.target {
                    PromptTarget::Root => admissions.push(Box::pin(admit_followup(
                        inbox.clone(),
                        control.clone(),
                        admission_root.clone(),
                        queued_inputs.clone(),
                        queue_wake.clone(),
                        followup.submission,
                    ))),
                    PromptTarget::Session(session_id) => {
                        if let Err(error) = dispatch_child_prompt(
                            &interactive_children,
                            &session_id,
                            followup.submission,
                        ) {
                            report_child_prompt_failure(
                                child_observer.as_ref().map(Arc::clone),
                                &session_id,
                                error,
                            );
                        }
                    }
                }
            }
            while let Some(outcome) = admissions.next().await {
                if let Err(message) = outcome {
                    report_input_failure(&admission_events, message).await;
                }
            }
            while let Ok(mutation) = queue_mutations.try_recv() {
                apply_queued_input_mutation(
                    inbox.clone(),
                    control.clone(),
                    control.session_id().to_owned(),
                    admission_root.clone(),
                    queued_inputs.clone(),
                    queue_wake.clone(),
                    mutation,
                )
                .await;
            }
            refresh_queued_input_projection(
                &inbox,
                control.session_id(),
                queued_inputs,
                queue_wake,
                None,
            );
            turn_outcome?;
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
            if let Some(capture) = capture {
                finish_snapshot(capture, snapshots, events).await;
            } else {
                snapshots.redo.clear();
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(message) = outcome {
            report_turn_failure(events, message).await;
        }
    }
}

async fn drive_submission(
    host: &mut TurnHost,
    prompt: PromptSubmission,
    promoted_message_id: Option<&str>,
    events: TurnEventSender,
) -> Result<(), String> {
    let result = match (prompt, promoted_message_id) {
        (PromptSubmission::Text(prompt), None) => host.drive(&prompt, events).await,
        (PromptSubmission::Text(prompt), Some(message_id)) => {
            host.drive_promoted(&prompt, message_id, events).await
        }
        (PromptSubmission::Content { text, content }, None) => {
            host.drive_content(&text, &content, events).await
        }
        (PromptSubmission::Content { text, content }, Some(message_id)) => {
            host.drive_promoted_content(&text, &content, message_id, events)
                .await
        }
        (PromptSubmission::Command { name, arguments }, None) => {
            host.drive_command(&name, &arguments, events).await
        }
        (PromptSubmission::Command { name, arguments }, Some(message_id)) => {
            host.drive_promoted_command(&name, &arguments, message_id, events)
                .await
        }
        (
            PromptSubmission::Skill {
                name,
                source,
                arguments,
            },
            None,
        ) => host.drive_skill(&name, &source, &arguments, events).await,
        (
            PromptSubmission::Skill {
                name,
                source,
                arguments,
            },
            Some(message_id),
        ) => {
            host.drive_promoted_skill(&name, &source, &arguments, message_id, events)
                .await
        }
        (
            PromptSubmission::Council {
                text,
                preset,
                question,
            },
            None,
        ) => host.drive_council(&text, &preset, &question, events).await,
        (
            PromptSubmission::Council {
                text,
                preset,
                question,
            },
            Some(message_id),
        ) => {
            host.drive_promoted_council(&text, &preset, &question, message_id, events)
                .await
        }
        (PromptSubmission::Host(_), _) => {
            unreachable!("host submissions are handled before a turn is started")
        }
        (PromptSubmission::Queue(_), _) | (PromptSubmission::Steer(_), _) => {
            unreachable!("delivery marker is removed before a turn is driven")
        }
    };
    if let (Err(error), Some(message_id)) = (&result, promoted_message_id) {
        let _settled =
            host.session_inbox()
                .mark_failed(host.session_id(), message_id, error.clone());
    }
    result
}

fn followup_delivery(prompt: &PromptSubmission) -> zuno_db::inbox::InputDelivery {
    match prompt {
        PromptSubmission::Queue(_) => zuno_db::inbox::InputDelivery::Queue,
        PromptSubmission::Steer(inner)
            if matches!(
                inner.as_ref(),
                PromptSubmission::Text(_) | PromptSubmission::Content { .. }
            ) =>
        {
            zuno_db::inbox::InputDelivery::Steer
        }
        _ => zuno_db::inbox::InputDelivery::Queue,
    }
}

fn soft_interrupt(
    input_id: &str,
    prompt: &PromptSubmission,
) -> Option<zuno_engine::interrupt::SoftInterruptMessage> {
    let PromptSubmission::Steer(prompt) = prompt else {
        return None;
    };
    let (content, images) = match prompt.as_ref() {
        PromptSubmission::Text(text) => (text.clone(), Vec::new()),
        PromptSubmission::Content { content, .. } => {
            let mut text = Vec::new();
            let mut images = Vec::new();
            for block in content {
                match block {
                    zuno_llm::event::RequestContentBlock::Text { text: block } => {
                        text.push(block.clone());
                    }
                    zuno_llm::event::RequestContentBlock::ResourceLink { .. } => {
                        let Some(block) = block.provider_text() else {
                            unreachable!("resource links always have a provider text projection")
                        };
                        text.push(block.into_owned());
                    }
                    zuno_llm::event::RequestContentBlock::Image {
                        media_type, data, ..
                    } => {
                        images.push((media_type.clone(), data.clone()));
                    }
                    _ => return None,
                }
            }
            (text.join("\n\n"), images)
        }
        PromptSubmission::Command { .. }
        | PromptSubmission::Skill { .. }
        | PromptSubmission::Council { .. }
        | PromptSubmission::Host(_)
        | PromptSubmission::Queue(_)
        | PromptSubmission::Steer(_) => return None,
    };
    Some(zuno_engine::interrupt::SoftInterruptMessage {
        input_id: Some(input_id.to_owned()),
        content,
        images,
        urgent: false,
        source: zuno_engine::interrupt::SoftInterruptSource::User,
    })
}

async fn admit_followup(
    inbox: zuno_db::inbox::SessionInbox,
    control: zuno_engine::status::SessionControl,
    reference_root: PathBuf,
    queued_inputs: QueuedInputProjection,
    queue_wake: mpsc::Sender<TerminalEvent>,
    prompt: PromptSubmission,
) -> Result<(), String> {
    let prompt = super::tui_reference::resolve_submission(&reference_root, prompt).await?;
    let input_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let delivery = followup_delivery(&prompt);
    inbox
        .admit(zuno_db::inbox::NewSessionInput::new(
            input_id.clone(),
            control.session_id(),
            serde_json::to_value(PersistedTuiInput::TuiPrompt {
                submission: prompt.clone(),
            })
            .map_err(to_string)?,
            delivery,
            zuno_db::message::now_millis(),
        ))
        .map_err(to_string)?;
    refresh_queued_input_projection(
        &inbox,
        control.session_id(),
        &queued_inputs,
        &queue_wake,
        Some(QueuedInputNotice {
            input_id: input_id.clone(),
            kind: QueuedInputNoticeKind::Admitted(match delivery {
                zuno_db::inbox::InputDelivery::Queue => QueuedInputDelivery::Queue,
                zuno_db::inbox::InputDelivery::Steer => QueuedInputDelivery::Steer,
            }),
        }),
    );
    if delivery == zuno_db::inbox::InputDelivery::Steer
        && let Some(message) = soft_interrupt(&input_id, &prompt)
    {
        match control.queue_soft_interrupt(message) {
            Ok(()) => tracing::debug!(
                target: "zuno::tui::steering",
                session_id = %control.session_id(),
                input_id,
                "durable TUI input woke the active turn"
            ),
            Err(_) => tracing::debug!(
                target: "zuno::tui::steering",
                session_id = %control.session_id(),
                input_id,
                "turn ended before steering; durable TUI input remains pending"
            ),
        }
    }
    Ok(())
}

fn promote_pending_prompt(
    host: &TurnHost,
    queued_inputs: &QueuedInputProjection,
    queue_wake: &mpsc::Sender<TerminalEvent>,
) -> Result<Option<DriverPrompt>, String> {
    let inbox = host.session_inbox();
    let Some(input) = inbox
        .pending(host.session_id())
        .map_err(to_string)?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let submission = decode_pending_prompt(&input)?;
    let Some(promoted) = inbox
        .promote_id(host.session_id(), &input.id)
        .map_err(to_string)?
    else {
        return Ok(None);
    };
    refresh_queued_input_projection(&inbox, host.session_id(), queued_inputs, queue_wake, None);
    Ok(Some(DriverPrompt::promoted(promoted.id, submission)))
}

fn decode_pending_prompt(input: &zuno_db::inbox::SessionInput) -> Result<PromptSubmission, String> {
    if input.prompt.get("kind").and_then(serde_json::Value::as_str) == Some("tuiPrompt") {
        let PersistedTuiInput::TuiPrompt { submission } =
            serde_json::from_value(input.prompt.clone()).map_err(to_string)?;
        return Ok(submission);
    }
    if matches!(
        input.prompt.get("kind").and_then(serde_json::Value::as_str),
        Some("subagentReport" | "productAgentReport")
    ) {
        let text = input
            .prompt
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("pending report input `{}` has no string `text`", input.id))?;
        return Ok(PromptSubmission::Text(text.to_owned()));
    }
    Err(format!(
        "pending session input `{}` has an unsupported durable prompt shape",
        input.id
    ))
}

async fn report_input_failure(events: &TurnEventSender, message: String) {
    let _reported = events
        .publish(TurnEvent::Provider {
            step: 0,
            event: StreamEvent::Error {
                message: format!("input was not admitted: {message}"),
                retry_after: None,
            },
        })
        .await;
}

async fn report_turn_failure(events: &TurnEventSender, message: String) {
    let reported = events
        .publish(TurnEvent::TurnFailed {
            assistant_message_id: None,
            steps: 0,
            message,
        })
        .await;
    // A closed event channel means the render loop has gone; there is nothing
    // left to report a failure to.
    let _closed = reported.is_err();
}

async fn finish_snapshot(
    capture: zuno_snapshot::TurnCapture,
    snapshots: &mut SnapshotHistory,
    events: &TurnEventSender,
) {
    snapshots.redo.clear();
    match tokio::task::spawn_blocking(move || capture.finish()).await {
        Ok(Ok(checkpoint)) => snapshots.undo.push(checkpoint),
        Ok(Err(error)) => {
            publish_snapshot_detail(
                events,
                format!("warning: turn snapshot could not be completed: {error}"),
            )
            .await;
        }
        Err(error) => {
            publish_snapshot_detail(
                events,
                format!("warning: turn snapshot task failed: {error}"),
            )
            .await;
        }
    }
}

async fn begin_snapshot(
    store: &zuno_snapshot::Store,
    events: &TurnEventSender,
) -> Option<zuno_snapshot::TurnCapture> {
    let store = store.clone();
    match tokio::task::spawn_blocking(move || store.begin_turn()).await {
        Ok(Ok(capture)) => capture,
        Ok(Err(error)) => {
            publish_snapshot_detail(
                events,
                format!("warning: turn snapshot could not start: {error}"),
            )
            .await;
            None
        }
        Err(error) => {
            publish_snapshot_detail(
                events,
                format!("warning: turn snapshot task failed: {error}"),
            )
            .await;
            None
        }
    }
}

async fn restore_snapshot(
    command: HostCommand,
    snapshots: &mut SnapshotHistory,
    events: &TurnEventSender,
) -> Result<(), String> {
    let (source, restore) = match command {
        HostCommand::Undo => (&mut snapshots.undo, zuno_snapshot::TurnRestore::Undo),
        HostCommand::Redo => (&mut snapshots.redo, zuno_snapshot::TurnRestore::Redo),
        HostCommand::Compact => {
            return Err("context compaction must be handled by the turn host".to_owned());
        }
        HostCommand::Goal(_) => {
            return Err("goal commands must be handled by the turn host".to_owned());
        }
        HostCommand::Preset(_) => {
            return Err("preset controls must be handled by the TUI selection layer".to_owned());
        }
        HostCommand::Council(_) => {
            return Err("Council controls must be handled by the TUI submission layer".to_owned());
        }
        HostCommand::Plan | HostCommand::StartPlan | HostCommand::StartWork => {
            return Err("plan mode controls must be handled by the TUI selection layer".to_owned());
        }
        HostCommand::Stop(_) => {
            return Err("background stop must be handled by the TUI background service".to_owned());
        }
    };
    let Some(checkpoint) = source.last().cloned() else {
        return Err(format!("nothing to {}", restore_name(restore)));
    };
    let store = snapshots.store.clone();
    let report = tokio::task::spawn_blocking(move || store.restore_turn(&checkpoint, restore))
        .await
        .map_err(|error| format!("{} snapshot task failed: {error}", restore_name(restore)))?
        .map_err(|error| format!("{} refused: {error}", restore_name(restore)))?;
    let checkpoint = source
        .pop()
        .expect("the restored checkpoint remains at the top of its stack");
    match restore {
        zuno_snapshot::TurnRestore::Undo => snapshots.redo.push(checkpoint),
        zuno_snapshot::TurnRestore::Redo => snapshots.undo.push(checkpoint),
    }
    publish_restore_report(events, &report).await;
    Ok(())
}

async fn execute_host_command(
    host: &mut TurnHost,
    command: HostCommand,
    snapshots: &mut SnapshotHistory,
    events: &TurnEventSender,
) -> Result<(), String> {
    let detail = match command {
        HostCommand::Compact => {
            if !host.is_session_materialized() {
                return Err("nothing to compact; send a message first".to_owned());
            }
            host.compact(false).await?;
            "context compacted; older history was summarized".to_owned()
        }
        HostCommand::Goal(arguments) => {
            let was_materialized = host.is_session_materialized();
            let outcome = host.goal_command(&arguments);
            if !was_materialized && host.is_session_materialized() {
                events
                    .publish(TurnEvent::SessionMaterialized {
                        session_id: host.session_id().to_owned(),
                        title: host.session_title().unwrap_or("New session").to_owned(),
                    })
                    .await
                    .map_err(to_string)?;
            }
            outcome?
        }
        HostCommand::Undo | HostCommand::Redo => {
            return restore_snapshot(command, snapshots, events).await;
        }
        HostCommand::Plan | HostCommand::StartPlan | HostCommand::StartWork => {
            return Err("plan mode controls must be handled by the TUI selection layer".to_owned());
        }
        HostCommand::Preset(_) => {
            return Err("preset controls must be handled by the TUI selection layer".to_owned());
        }
        HostCommand::Council(_) => {
            return Err("Council controls must be handled by the TUI submission layer".to_owned());
        }
        HostCommand::Stop(_) => {
            return Err("background stop must be handled by the TUI background service".to_owned());
        }
    };
    events
        .publish(TurnEvent::Provider {
            step: 0,
            event: StreamEvent::StatusDetail { detail },
        })
        .await
        .map_err(to_string)
}

async fn publish_restore_report(
    events: &TurnEventSender,
    report: &zuno_snapshot::TurnRestoreReport,
) {
    let action = restore_name(report.restore());
    if report.files().is_empty() {
        publish_snapshot_detail(events, format!("{action}: no files changed")).await;
        return;
    }
    publish_snapshot_detail(
        events,
        format!("{action}: restored {} file(s)", report.files().len()),
    )
    .await;
    for file in report.files() {
        publish_snapshot_detail(
            events,
            format!("{action}: {:?} {}", file.operation, file.path),
        )
        .await;
    }
}

async fn publish_snapshot_detail(events: &TurnEventSender, detail: String) {
    let _reported = events
        .publish(TurnEvent::Provider {
            step: 0,
            event: StreamEvent::StatusDetail { detail },
        })
        .await;
}

fn restore_name(restore: zuno_snapshot::TurnRestore) -> &'static str {
    match restore {
        zuno_snapshot::TurnRestore::Undo => "undo",
        zuno_snapshot::TurnRestore::Redo => "redo",
    }
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::io::{Read as _, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex, PoisonError};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    #[cfg(target_os = "linux")]
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use zuno_engine::terminal_lease::TerminalBroker;
    use zuno_testkit::FakeTerminalOwner;
    use zuno_tui::config::ResolveOptions;
    use zuno_tui::keybind::{Chord, Resolution};

    use super::*;

    #[test]
    fn primary_agent_selector_includes_deep_and_excludes_subagent_only_roles() {
        let agents =
            zuno_catalog::agent::resolve(&zuno_config::schema::ordered::OrderedMap::new(), &[]);
        let names = selectable_session_agents(&agents)
            .into_iter()
            .map(|agent| agent.name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["orchestrator", "build", "plan", "deep"]);
    }

    #[test]
    fn durable_child_sessions_are_restored_before_navigation() {
        let pool = Arc::new(
            zuno_db::pool::Pool::open(&zuno_paths::DbLocation::Memory)
                .expect("open child projection database"),
        );
        {
            let mut connection = pool.get().expect("seed connection");
            zuno_db::migration::apply(&mut connection).expect("apply schema");
            connection
                .execute_batch(
                    "INSERT INTO project \
                       (id, worktree, time_created, time_updated, sandboxes) \
                     VALUES ('project-child-projection', '/workspace', 1, 1, '[]');
                     INSERT INTO session \
                       (id, project_id, parent_id, slug, directory, title, version, agent, model, \
                        time_created, time_updated) \
                     VALUES \
                       ('ses_parent', 'project-child-projection', NULL, 'parent', '/workspace', \
                        'parent', '1', 'build', \
                        '{\"id\":\"root-model\",\"providerID\":\"test\"}', 1, 1),
                       ('ses_child', 'project-child-projection', 'ses_parent', 'child', \
                        '/workspace', 'durable child', '1', 'explorer', \
                        '{\"id\":\"child-model\",\"providerID\":\"test\"}', 2, 2);",
                )
                .expect("seed parent and child");
        }
        let sessions = LiveSessions::default();

        let diagnostics = restore_child_sessions(&pool, "ses_parent", &sessions);

        assert!(
            diagnostics.is_empty(),
            "a valid durable child emitted diagnostics: {diagnostics:?}"
        );
        let snapshot = sessions
            .snapshot("ses_child")
            .expect("durable child is restored");
        assert_eq!(snapshot.parent_session_id, "ses_parent");
        assert_eq!(snapshot.agent, "explorer");
        assert_eq!(snapshot.model, "test/child-model");
        assert!(
            !snapshot.transcript.is_running(),
            "a historical child was restored as active"
        );
    }

    #[tokio::test]
    async fn cancellation_forwarder_fires_the_active_sessions_interrupt_signal() {
        let registry = SessionRunRegistry::new();
        let guard = registry
            .begin_turn("ses_cancel_from_tui")
            .expect("the fixture owns the only live turn");
        let signal = guard.interrupt_signal().clone();
        let control = registry.control("ses_cancel_from_tui");
        let (requests, receiver) = mpsc::channel(1);
        let (shutdown, shutdown_source) = watch::channel(false);
        let worker = tokio::spawn(forward_cancellations(control, receiver, shutdown_source));

        requests
            .send(())
            .await
            .expect("the cancellation bridge is listening");
        tokio::time::timeout(Duration::from_secs(1), signal.notified())
            .await
            .expect("the cancellation bridge never fired the turn signal");
        assert!(signal.is_set(), "the turn signal remained clear");

        shutdown.send(true).expect("the worker observes shutdown");
        worker.await.expect("the cancellation bridge exits cleanly");
    }

    #[tokio::test]
    async fn cancellation_forwarder_retains_an_interrupt_across_the_turn_handoff() {
        let registry = SessionRunRegistry::new();
        let control = registry.control("ses_cancel_handoff");
        let (requests, receiver) = mpsc::channel(1);
        let (shutdown, shutdown_source) = watch::channel(false);
        let worker = tokio::spawn(forward_cancellations(control, receiver, shutdown_source));

        requests
            .send(())
            .await
            .expect("the cancellation bridge is listening");
        tokio::task::yield_now().await;
        let guard = registry
            .begin_turn("ses_cancel_handoff")
            .expect("the admitted follow-up acquires its guard");
        tokio::time::timeout(Duration::from_secs(1), guard.interrupt_signal().notified())
            .await
            .expect("the handoff interrupt never reached the accepted follow-up");
        assert!(guard.interrupt_signal().is_set());

        shutdown.send(true).expect("the worker observes shutdown");
        worker.await.expect("the cancellation bridge exits cleanly");
    }

    #[test]
    fn tui_host_continuity_keeps_cancellation_bound_to_replacement_hosts() {
        let continuity = TuiHostContinuity::new(
            SessionRunRegistry::new(),
            Arc::new(RecordingTitleSink::default()),
            None,
        );
        let control = continuity.control("ses_rebuilt");
        let replacement_runs = continuity.runs();
        let guard = replacement_runs
            .begin_turn("ses_rebuilt")
            .expect("the replacement host owns the live turn");

        assert_eq!(
            control.abort(),
            zuno_engine::status::AbortDisposition::Active,
            "a control created before host replacement targeted an abandoned registry"
        );
        assert!(
            guard.interrupt_signal().is_set(),
            "the replacement host did not receive the user's interrupt"
        );
    }

    #[derive(Default)]
    struct RecordingTitleSink(Mutex<Vec<String>>);

    impl SessionTitleSink for RecordingTitleSink {
        fn publish(&self, title: &str) {
            self.0.lock().expect("title log").push(title.to_owned());
        }
    }

    #[test]
    fn tui_host_continuity_keeps_title_projection_bound_to_replacement_hosts() {
        let titles = Arc::new(RecordingTitleSink::default());
        let continuity = TuiHostContinuity::new(
            SessionRunRegistry::new(),
            Arc::clone(&titles) as Arc<dyn SessionTitleSink>,
            None,
        );

        continuity.title_sink().publish("Replacement title");

        assert_eq!(
            *titles.0.lock().expect("title log"),
            ["Replacement title"],
            "a replacement host lost the live sidebar title projection"
        );
    }

    #[test]
    fn fresh_conversation_remount_skips_the_launch_welcome_only() {
        let request = RemountRequest::fresh_conversation(TurnOptions::default());
        assert!(!request.show_welcome);
        assert!(request.initial_dialog.is_none());
        assert!(matches!(request.options.session, SessionChoice::New));
    }

    #[tokio::test]
    async fn prompt_history_shutdown_closes_the_live_sender_and_drains_queued_entries() {
        let directory = tempfile::tempdir().expect("history fixture");
        let path = directory.path().join("prompt-history.jsonl");
        let (records, receiver) = mpsc::channel(4);
        let (shutdown, shutdown_source) = watch::channel(false);
        records
            .send("persist before exit".to_owned())
            .await
            .expect("history worker is live");
        let worker = tokio::spawn(record_prompt_history(
            path.clone(),
            receiver,
            shutdown_source,
        ));

        shutdown.send(true).expect("history worker observes exit");
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("history worker must stop even while the sender remains live")
            .expect("history worker must not panic");

        let persisted = fs::read_to_string(path).expect("queued history is flushed");
        assert!(persisted.contains("persist before exit"));
    }

    #[test]
    fn tui_followup_delivery_queues_by_default_and_only_steers_explicit_overrides() {
        assert_eq!(
            followup_delivery(&PromptSubmission::Text(String::from("direct"))),
            zuno_db::inbox::InputDelivery::Queue
        );
        assert_eq!(
            followup_delivery(&PromptSubmission::Queue(Box::new(PromptSubmission::Text(
                String::from("queue")
            )))),
            zuno_db::inbox::InputDelivery::Queue
        );
        assert_eq!(
            followup_delivery(&PromptSubmission::Steer(Box::new(PromptSubmission::Text(
                String::from("steer")
            )))),
            zuno_db::inbox::InputDelivery::Steer
        );
        assert_eq!(
            followup_delivery(&PromptSubmission::Command {
                name: String::from("review"),
                arguments: String::new(),
            }),
            zuno_db::inbox::InputDelivery::Queue
        );
        assert_eq!(
            followup_delivery(&PromptSubmission::Skill {
                name: String::from("codegraph"),
                source: String::from("/skills/codegraph/SKILL.md"),
                arguments: String::new(),
            }),
            zuno_db::inbox::InputDelivery::Queue
        );
        assert_eq!(
            followup_delivery(&PromptSubmission::Host(HostCommand::Undo)),
            zuno_db::inbox::InputDelivery::Queue
        );
    }

    #[tokio::test]
    async fn tui_followup_is_durable_and_wakes_the_active_turn_without_aborting_it() {
        let pool = Arc::new(
            zuno_db::pool::Pool::open(&zuno_paths::DbLocation::Memory)
                .expect("open shared in-memory inbox"),
        );
        {
            let mut connection = pool.get().expect("seed connection");
            zuno_db::migration::apply(&mut connection).expect("apply schema");
            connection
                .execute_batch(
                    "INSERT INTO project \
                       (id, worktree, time_created, time_updated, sandboxes) \
                     VALUES ('project-tui-steer', '/workspace', 1, 1, '[]');
                     INSERT INTO session \
                       (id, project_id, slug, directory, title, version, \
                        time_created, time_updated) \
                     VALUES ('ses_tui_steer', 'project-tui-steer', 'steer', \
                             '/workspace', 'steer', '1', 1, 1);",
                )
                .expect("seed project and session");
        }
        let inbox = zuno_db::inbox::SessionInbox::new(pool);
        let registry = SessionRunRegistry::new();
        let guard = registry
            .begin_turn("ses_tui_steer")
            .expect("fixture owns the live turn");
        let reference_root = tempfile::tempdir().expect("reference root");
        let projection = QueuedInputProjection::default();
        let (wake, _wake_source) = zuno_tui::app::terminal_event_channel();

        admit_followup(
            inbox.clone(),
            registry.control("ses_tui_steer"),
            reference_root.path().to_path_buf(),
            projection.clone(),
            wake,
            PromptSubmission::Steer(Box::new(PromptSubmission::Text(
                "change direction now".to_owned(),
            ))),
        )
        .await
        .expect("admit follow-up");

        let pending = inbox.pending("ses_tui_steer").expect("read durable inbox");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].delivery, zuno_db::inbox::InputDelivery::Steer);
        assert_eq!(projection.snapshot().len(), 1);
        assert!(matches!(
            projection.observe().2.map(|notice| notice.kind),
            Some(QueuedInputNoticeKind::Admitted(QueuedInputDelivery::Steer))
        ));
        assert!(
            guard.soft_interrupt_signal().is_set(),
            "the durable admission did not wake the provider wait"
        );
        assert!(
            !guard.interrupt_signal().is_set(),
            "a steer accidentally became a hard turn cancellation"
        );
        let delivered = guard.take_soft_interrupts_at_safe_point();
        assert_eq!(delivered.messages.len(), 1);
        assert_eq!(
            delivered.messages[0].input_id.as_deref(),
            Some(pending[0].id.as_str())
        );
        assert_eq!(delivered.messages[0].content, "change direction now");
    }

    #[test]
    fn tui_soft_interrupt_keeps_resolved_text_and_images() {
        let submission = PromptSubmission::Steer(Box::new(PromptSubmission::Content {
            text: String::from("inspect @diagram.png"),
            content: vec![
                zuno_llm::event::RequestContentBlock::Text {
                    text: String::from("inspect @diagram.png"),
                },
                zuno_llm::event::RequestContentBlock::Text {
                    text: String::from("Referenced image: diagram.png"),
                },
                zuno_llm::event::RequestContentBlock::Image {
                    filename: Some(String::from("diagram.png")),
                    media_type: String::from("image/png"),
                    data: String::from("AAAA"),
                },
            ],
        }));

        let message =
            soft_interrupt("msg_followup", &submission).expect("content can steer safely");

        assert_eq!(message.input_id.as_deref(), Some("msg_followup"));
        assert_eq!(
            message.content,
            "inspect @diagram.png\n\nReferenced image: diagram.png"
        );
        assert_eq!(
            message.images,
            vec![(String::from("image/png"), String::from("AAAA"))]
        );
        assert!(!message.urgent);
    }

    #[test]
    fn tui_prompt_submission_has_a_durable_round_trip() {
        let submissions = [
            PromptSubmission::Text(String::from("change direction")),
            PromptSubmission::Content {
                text: String::from("inspect @diagram.png"),
                content: vec![zuno_llm::event::RequestContentBlock::Image {
                    filename: Some(String::from("diagram.png")),
                    media_type: String::from("image/png"),
                    data: String::from("AAAA"),
                }],
            },
            PromptSubmission::Command {
                name: String::from("review"),
                arguments: String::from("the queue"),
            },
            PromptSubmission::Skill {
                name: String::from("github-project-scaffold"),
                source: String::from("/skills/github-project-scaffold/SKILL.md"),
                arguments: String::from("audit the repository"),
            },
            PromptSubmission::Host(HostCommand::Undo),
            PromptSubmission::Queue(Box::new(PromptSubmission::Text(String::from(
                "queued follow-up",
            )))),
            PromptSubmission::Steer(Box::new(PromptSubmission::Text(String::from(
                "urgent correction",
            )))),
        ];

        for submission in submissions {
            let stored = serde_json::to_value(&submission).expect("serialize submission");
            let restored =
                serde_json::from_value::<PromptSubmission>(stored).expect("decode submission");
            assert_eq!(restored, submission);
        }
    }

    struct BlockingMcpConnector {
        started: AtomicUsize,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
        changed: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }

    impl Default for BlockingMcpConnector {
        fn default() -> Self {
            Self {
                started: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
                changed: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
            }
        }
    }

    impl BlockingMcpConnector {
        async fn wait_for_started(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let notified = self.changed.notified();
                    if self.started.load(Ordering::Acquire) >= count {
                        return;
                    }
                    notified.await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "only {} MCP connects started",
                    self.started.load(Ordering::Acquire)
                )
            });
        }

        fn release_one(&self) {
            self.release.add_permits(1);
        }

        fn release_all(&self, count: usize) {
            self.release.add_permits(count);
        }

        fn maximum_active(&self) -> usize {
            self.maximum_active.load(Ordering::Acquire)
        }

        fn record_maximum(&self, active: usize) {
            let mut observed = self.maximum_active.load(Ordering::Acquire);
            while active > observed {
                match self.maximum_active.compare_exchange_weak(
                    observed,
                    active,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return,
                    Err(current) => observed = current,
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl zuno_mcp::McpConnector for BlockingMcpConnector {
        async fn connect(&self, _server: &str) -> Result<zuno_mcp::McpConnectOutcome, String> {
            self.started.fetch_add(1, Ordering::AcqRel);
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.record_maximum(active);
            self.changed.notify_waiters();
            let permit = self
                .release
                .acquire()
                .await
                .expect("test MCP release semaphore stays open");
            permit.forget();
            self.active.fetch_sub(1, Ordering::AcqRel);
            Ok(zuno_mcp::McpConnectOutcome::NeedsAuth)
        }
    }

    struct LeaseObservingEditor {
        transcript: zuno_testkit::TerminalTranscript,
        observed: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl ExternalEditor for LeaseObservingEditor {
        async fn edit(
            &self,
            request: &EditorRequest,
            _cancellation: zuno_tui::views::external::EditorCancellation,
        ) -> Result<Option<String>, ExternalError> {
            self.observed
                .store(self.transcript.acquired_by("tui"), Ordering::SeqCst);
            Ok(Some(format!("{} edited", request.value)))
        }
    }

    #[test]
    fn tui_the_exit_hint_names_the_session_just_left_with_a_flag_the_parser_accepts() {
        // Given: the id a real host would have carried, in the shape `prefixed_id` mints.
        let session_id = "ses_4f9c1d2e8a7b6c5d4e3f2a1b0c9d8e7f";

        // When: the hint printed after teardown is composed.
        let hint = resume_hint(session_id);

        // Then: it names that exact session, not a placeholder or a truncation.
        assert!(
            hint.contains(session_id),
            "the hint must carry the id a user has to paste; got {hint:?}"
        );

        // And: the whole command is copy-pasteable as written.
        assert!(
            hint.contains(&format!("zuno {RESUME_FLAG} {session_id}")),
            "the hint must read as one runnable command; got {hint:?}"
        );

        // And: the flag is one `TuiArgs` really declares. Parsed rather than compared to
        // a literal, because a literal would agree with itself after the flag was
        // renamed — which is the only way this hint can fail while still looking right.
        use clap::Parser as _;
        let parsed = crate::command::Cli::try_parse_from(["zuno", RESUME_FLAG, session_id])
            .expect("the hint's flag must be one the top-level parser accepts");
        assert_eq!(
            parsed.tui.session.as_deref(),
            Some(session_id),
            "the flag parsed, but not into the session slot"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_editor_worker_holds_the_terminal_lease_during_editing() {
        let owner = Arc::new(FakeTerminalOwner::new());
        let transcript = owner.transcript();
        let lease: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
        let observed = Arc::new(AtomicBool::new(false));
        let editor: Arc<dyn ExternalEditor> = Arc::new(LeaseObservingEditor {
            transcript: transcript.clone(),
            observed: Arc::clone(&observed),
        });
        let (requests, request_source) = mpsc::channel(1);
        let (results, mut result_source) = mpsc::channel(1);
        let (wake, mut wake_source) = zuno_tui::app::terminal_event_channel();
        let (_shutdown, shutdown_source) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(drive_external_editor(
            lease,
            editor,
            request_source,
            results,
            wake,
            shutdown_source,
        ));

        requests
            .send(EditorRequest::new("draft"))
            .await
            .expect("worker accepts the request");
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), result_source.recv())
            .await
            .expect("editor completes")
            .expect("result channel remains open")
            .expect("editor succeeds");

        assert_eq!(result, Some(String::from("draft edited")));
        assert!(observed.load(Ordering::SeqCst));
        assert!(transcript.released_by("tui"));
        // `drive_external_editor` sends the result and *then* nudges, so a delivered
        // result is a proxy for readiness rather than readiness itself: `try_recv` here
        // read an empty channel in 6 of 60 runs, the wake landing 8-28µs later across the
        // two-thread hand-off. Waiting for the nudge on a budget cannot be made wrong by
        // load, only slower, and each way it ends names a different failure.
        let nudged = tokio::time::timeout(std::time::Duration::from_secs(5), wake_source.recv())
            .await
            .expect("the worker published a result but never nudged the render loop")
            .expect("the worker dropped the nudge channel without waking the render loop");
        assert!(
            matches!(nudged, zuno_tui::app::TerminalEvent::Wake),
            "the worker nudged with {nudged:?}, which no render loop redraws on"
        );
        drop(requests);
        worker.await.expect("worker exits with its request channel");
    }

    struct HangingEditor {
        killed: Arc<AtomicBool>,
        reaped: Arc<AtomicBool>,
    }

    #[cfg(target_os = "linux")]
    static FOREGROUND_EDITOR_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[cfg(target_os = "linux")]
    fn contained_system_editor(spec: String) -> Arc<dyn ExternalEditor> {
        let test_executable = std::env::current_exe().expect("locate the zuno-cli test binary");
        let debug_directory = test_executable
            .parent()
            .and_then(std::path::Path::parent)
            .expect("the test binary is under target/debug/deps");
        let guard = debug_directory.join("zuno");
        assert!(guard.is_file(), "the zuno process guard was not built");
        zuno_process::activate_guard_executable(guard).expect("activate the zuno process guard");
        Arc::new(SystemEditor::configured_with_launcher(
            spec,
            Arc::new(ContainedEditorLauncher),
        ))
    }

    #[cfg(target_os = "linux")]
    fn hanging_system_editor() -> (
        tempfile::TempDir,
        Arc<dyn ExternalEditor>,
        std::path::PathBuf,
    ) {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("editor fixture directory");
        let script = directory.path().join("editor");
        let pid = directory.path().join("editor.pid");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$1\"\nexec sleep 3600\n",
        )
        .expect("write hanging editor fixture");
        let mut permissions = std::fs::metadata(&script)
            .expect("editor fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make editor fixture executable");
        let editor = contained_system_editor(format!("{} {}", script.display(), pid.display()));
        (directory, editor, pid)
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_editor_pid(
        path: &std::path::Path,
        results: &mut mpsc::Receiver<Result<Option<String>, ExternalError>>,
    ) -> u32 {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(path)
                    && let Ok(pid) = value.parse::<u32>()
                {
                    return pid;
                }
                tokio::select! {
                    result = results.recv() => {
                        panic!(
                            "the editor finished before writing {}: {result:?}",
                            path.display()
                        );
                    }
                    () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                }
            }
        })
        .await
        .expect("the editor writes its process id")
    }

    #[cfg(target_os = "linux")]
    fn wrapper_system_editor() -> (
        tempfile::TempDir,
        Arc<dyn ExternalEditor>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("editor fixture directory");
        let script = directory.path().join("wrapper-editor");
        let wrapper_pid = directory.path().join("wrapper.pid");
        let descendant_pid = directory.path().join("descendant.pid");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$1\"\nsleep 3600 &\nchild=$!\nprintf '%s' \"$child\" > \"$2\"\nwait \"$child\"\n",
        )
        .expect("write wrapper editor fixture");
        let mut permissions = std::fs::metadata(&script)
            .expect("wrapper editor fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions)
            .expect("make wrapper editor fixture executable");
        let editor = contained_system_editor(format!(
            "{} {} {}",
            script.display(),
            wrapper_pid.display(),
            descendant_pid.display()
        ));
        (directory, editor, wrapper_pid, descendant_pid)
    }

    #[cfg(target_os = "linux")]
    struct ProcessCleanup(u32);

    #[cfg(target_os = "linux")]
    impl Drop for ProcessCleanup {
        fn drop(&mut self) {
            if std::path::Path::new(&format!("/proc/{}", self.0)).exists() {
                let _status = std::process::Command::new("kill")
                    .args(["-KILL", &self.0.to_string()])
                    .status();
            }
        }
    }

    #[cfg(target_os = "linux")]
    struct DescendantObservingOwner {
        descendant_pid: std::path::PathBuf,
        alive_at_reclaim: AtomicBool,
        reclaimed: AtomicBool,
    }

    #[cfg(target_os = "linux")]
    #[async_trait::async_trait]
    impl zuno_engine::terminal_lease::TerminalOwner for DescendantObservingOwner {
        async fn yield_terminal(
            &self,
            _reason: &zuno_engine::terminal_lease::LeaseReason,
        ) -> Result<(), String> {
            Ok(())
        }

        fn reclaim_terminal(
            &self,
            _reason: &zuno_engine::terminal_lease::LeaseReason,
            _cause: zuno_engine::terminal_lease::ReclaimCause,
        ) {
            let alive = std::fs::read_to_string(&self.descendant_pid)
                .ok()
                .and_then(|pid| pid.parse::<u32>().ok())
                .is_some_and(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists());
            self.alive_at_reclaim.store(alive, Ordering::SeqCst);
            self.reclaimed.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl ExternalEditor for HangingEditor {
        async fn edit(
            &self,
            _request: &EditorRequest,
            cancellation: zuno_tui::views::external::EditorCancellation,
        ) -> Result<Option<String>, ExternalError> {
            cancellation.cancelled().await;
            self.killed.store(true, Ordering::SeqCst);
            tokio::task::yield_now().await;
            self.reaped.store(true, Ordering::SeqCst);
            Err(ExternalError::Failed(String::from(
                "scripted editor was terminated",
            )))
        }
    }

    #[cfg(target_os = "linux")]
    const EDITOR_TEST_LEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_editor_timeout_kills_and_reaps_before_forced_reclaim() {
        // These fixtures all contend for the test process's one foreground
        // terminal. Running them together can stop a sibling process group
        // before its script writes the PID that the assertion observes.
        let _foreground = FOREGROUND_EDITOR_TEST.lock().await;
        let owner = Arc::new(FakeTerminalOwner::new());
        let transcript = owner.transcript();
        let lease: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::with_timeout(
            owner,
            EDITOR_TEST_LEASE_TIMEOUT,
        ));
        let (_directory, editor, pid_path) = hanging_system_editor();
        let (requests, request_source) = mpsc::channel(1);
        let (results, mut result_source) = mpsc::channel(1);
        let (wake, _wake_source) = zuno_tui::app::terminal_event_channel();
        let (_shutdown, shutdown_source) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(drive_external_editor(
            lease,
            editor,
            request_source,
            results,
            wake,
            shutdown_source,
        ));

        requests
            .send(EditorRequest::new("draft"))
            .await
            .expect("worker accepts the request");
        let pid = wait_for_editor_pid(&pid_path, &mut result_source).await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), result_source.recv())
            .await
            .expect("the lease deadline cancels the editor")
            .expect("result channel remains open");

        assert!(result.is_err(), "a timed-out edit cannot produce text");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the timed-out editor still exists, including as an unreaped zombie"
        );
        transcript
            .wait_for_forced("tui", std::time::Duration::from_secs(5))
            .await
            .expect("the terminal is force-reclaimed after child cleanup");
        drop(requests);
        worker.await.expect("worker exits with its request channel");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_editor_wrapper_descendant_is_gone_before_terminal_reclaim() {
        let _foreground = FOREGROUND_EDITOR_TEST.lock().await;
        let (_directory, editor, wrapper_pid_path, descendant_pid_path) = wrapper_system_editor();
        let owner = Arc::new(DescendantObservingOwner {
            descendant_pid: descendant_pid_path.clone(),
            alive_at_reclaim: AtomicBool::new(false),
            reclaimed: AtomicBool::new(false),
        });
        let lease: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::with_timeout(
            Arc::clone(&owner) as Arc<dyn zuno_engine::terminal_lease::TerminalOwner>,
            EDITOR_TEST_LEASE_TIMEOUT,
        ));
        let (requests, request_source) = mpsc::channel(1);
        let (results, mut result_source) = mpsc::channel(1);
        let (wake, _wake_source) = zuno_tui::app::terminal_event_channel();
        let (_shutdown, shutdown_source) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(drive_external_editor(
            lease,
            editor,
            request_source,
            results,
            wake,
            shutdown_source,
        ));

        requests
            .send(EditorRequest::new("draft"))
            .await
            .expect("worker accepts the request");
        let wrapper_pid = wait_for_editor_pid(&wrapper_pid_path, &mut result_source).await;
        let descendant_pid = wait_for_editor_pid(&descendant_pid_path, &mut result_source).await;
        let wrapper_cleanup = ProcessCleanup(wrapper_pid);
        let descendant_cleanup = ProcessCleanup(descendant_pid);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), result_source.recv())
            .await
            .expect("the lease deadline cancels the wrapper editor")
            .expect("result channel remains open");

        assert!(result.is_err(), "a timed-out edit cannot produce text");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !owner.reclaimed.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the terminal is reclaimed after cleanup");
        assert!(
            !owner.alive_at_reclaim.load(Ordering::SeqCst),
            "the terminal was reclaimed while wrapper descendant {descendant_pid} still owned it"
        );
        assert!(
            !std::path::Path::new(&format!("/proc/{descendant_pid}")).exists(),
            "the wrapper descendant survived editor cleanup"
        );

        std::mem::forget(wrapper_cleanup);
        std::mem::forget(descendant_cleanup);
        drop(requests);
        worker.await.expect("worker exits with its request channel");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_editor_error_path_always_releases_the_lease() {
        let owner = Arc::new(FakeTerminalOwner::new());
        let transcript = owner.transcript();
        let lease: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
        let editor: Arc<dyn ExternalEditor> =
            Arc::new(zuno_tui::views::external::ScriptedEditor::failing());
        let (requests, request_source) = mpsc::channel(1);
        let (results, mut result_source) = mpsc::channel(1);
        let (wake, _wake_source) = zuno_tui::app::terminal_event_channel();
        let (_shutdown, shutdown_source) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(drive_external_editor(
            lease,
            editor,
            request_source,
            results,
            wake,
            shutdown_source,
        ));

        requests
            .send(EditorRequest::new("draft"))
            .await
            .expect("worker accepts the request");
        assert!(
            result_source
                .recv()
                .await
                .expect("result channel remains open")
                .is_err()
        );
        assert!(
            transcript.released_by("tui"),
            "an editor failure stranded the terminal lease"
        );
        drop(requests);
        worker.await.expect("worker exits with its request channel");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_editor_shutdown_kills_and_reaps_before_releasing_the_lease() {
        let owner = Arc::new(FakeTerminalOwner::new());
        let transcript = owner.transcript();
        let lease: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
        let killed = Arc::new(AtomicBool::new(false));
        let reaped = Arc::new(AtomicBool::new(false));
        let editor: Arc<dyn ExternalEditor> = Arc::new(HangingEditor {
            killed: Arc::clone(&killed),
            reaped: Arc::clone(&reaped),
        });
        let (requests, request_source) = mpsc::channel(1);
        let (results, _result_source) = mpsc::channel(1);
        let (wake, _wake_source) = zuno_tui::app::terminal_event_channel();
        let (shutdown, shutdown_source) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(drive_external_editor(
            lease,
            editor,
            request_source,
            results,
            wake,
            shutdown_source,
        ));
        requests
            .send(EditorRequest::new("draft"))
            .await
            .expect("worker accepts the request");
        assert!(
            transcript
                .wait_until(std::time::Duration::from_secs(5), |transitions| {
                    transitions
                        .iter()
                        .any(|transition| transition.requester() == "tui")
                })
                .await,
            "the editor did not acquire the lease"
        );

        shutdown.send(true).expect("the editor observes shutdown");
        worker.await.expect("the editor worker shuts down cleanly");

        assert!(
            killed.load(Ordering::SeqCst),
            "shutdown left the editor alive"
        );
        assert!(
            reaped.load(Ordering::SeqCst),
            "shutdown did not reap the editor"
        );
        assert!(
            transcript.released_by("tui"),
            "shutdown reclaimed the terminal before editor cleanup"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_editor_task_cancellation_kills_and_reaps_before_releasing_the_lease() {
        let _foreground = FOREGROUND_EDITOR_TEST.lock().await;
        let owner = Arc::new(FakeTerminalOwner::new());
        let transcript = owner.transcript();
        let lease: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
        let (_directory, editor, pid_path) = hanging_system_editor();
        let (requests, request_source) = mpsc::channel(1);
        let (results, mut result_source) = mpsc::channel(1);
        let (wake, _wake_source) = zuno_tui::app::terminal_event_channel();
        let (_shutdown, shutdown_source) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(drive_external_editor(
            lease,
            editor,
            request_source,
            results,
            wake,
            shutdown_source,
        ));
        requests
            .send(EditorRequest::new("draft"))
            .await
            .expect("worker accepts the request");
        assert!(
            transcript
                .wait_until(std::time::Duration::from_secs(5), |transitions| {
                    transitions
                        .iter()
                        .any(|transition| transition.requester() == "tui")
                })
                .await,
            "the editor did not acquire the lease"
        );
        let pid = wait_for_editor_pid(&pid_path, &mut result_source).await;

        worker.abort();
        let cancelled = worker.await.expect_err("the worker task was cancelled");
        assert!(cancelled.is_cancelled());

        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "task cancellation left the editor alive or unreaped"
        );
        assert!(
            transcript.released_by("tui"),
            "task cancellation reclaimed the terminal before editor cleanup"
        );
    }

    #[cfg(target_os = "linux")]
    const EDITOR_PTY_HELPER: &str = "cmd::tui::tests::external_editor_pty_helper";

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn external_editor_pty_helper() {
        if std::env::var_os("ZUNO_EDITOR_PTY_HELPER").is_none() {
            return;
        }

        let test_executable = std::env::current_exe().expect("locate the zuno-cli test binary");
        let guard = test_executable
            .parent()
            .and_then(std::path::Path::parent)
            .expect("the test binary is under target/debug/deps")
            .join("zuno");
        zuno_process::activate_guard_executable(guard).expect("activate the zuno process guard");
        let invocation = zuno_tui::views::external::EditorInvocation {
            program: String::from("/bin/sh"),
            args: vec![
                String::from("-c"),
                String::from("read value; printf 'PAYLOAD:%s\\n' \"$value\""),
            ],
        };
        let mut editor = ContainedEditorLauncher
            .spawn(&invocation, None)
            .expect("spawn the contained editor");
        println!("EDITOR-SPAWNED");
        let status = wait_for_editor(&mut *editor, "terminal input").await;
        assert!(status.success(), "contained editor failed: {status}");

        println!("FOREGROUND-RESTORED");
        let mut after = String::new();
        std::io::stdin()
            .read_line(&mut after)
            .expect("read from the restored foreground terminal");
        println!("AFTER:{}", after.trim_end());

        let failing = zuno_tui::views::external::EditorInvocation {
            program: String::from("/bin/sh"),
            args: vec![String::from("-c"), String::from("exit 7")],
        };
        let mut editor = ContainedEditorLauncher
            .spawn(&failing, None)
            .expect("spawn the failing contained editor");
        let status = wait_for_editor(&mut *editor, "the error-path editor").await;
        assert!(
            !status.success(),
            "the error-path editor unexpectedly succeeded"
        );
        println!("ERROR-FOREGROUND-RESTORED");
        let mut after_error = String::new();
        std::io::stdin()
            .read_line(&mut after_error)
            .expect("read after the error-path foreground restoration");
        println!("AFTER-ERROR:{}", after_error.trim_end());

        let hanging = zuno_tui::views::external::EditorInvocation {
            program: String::from("/bin/sh"),
            args: vec![
                String::from("-c"),
                String::from(
                    "trap 'exit 0' TERM; printf 'KILL-PAYLOAD-READY\\n'; while :; do sleep 1; done",
                ),
            ],
        };
        let mut editor = ContainedEditorLauncher
            .spawn(&hanging, None)
            .expect("spawn the cancellable contained editor");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        editor
            .request_termination()
            .expect("request contained editor termination");
        let _status = wait_for_editor(&mut *editor, "the cancellation-path editor").await;
        println!("KILL-FOREGROUND-RESTORED");
        let mut after_kill = String::new();
        std::io::stdin()
            .read_line(&mut after_kill)
            .expect("read after the kill-path foreground restoration");
        println!("AFTER-KILL:{}", after_kill.trim_end());
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_editor(
        editor: &mut dyn EditorProcess,
        context: &str,
    ) -> std::process::ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = editor.try_wait().expect("poll the contained editor") {
                return status;
            }
            if started.elapsed() >= std::time::Duration::from_secs(2) {
                let _terminated = editor.request_termination();
                panic!("contained editor did not exit after {context}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn contained_editor_launcher_reads_the_tty_and_restores_its_foreground_group() {
        let mut pty = EditorPty::spawn();
        assert!(
            pty.wait_for_output("EDITOR-SPAWNED"),
            "the launcher helper did not spawn the editor: {:?}",
            pty.output()
        );
        pty.write(b"hello\n");
        assert!(
            pty.wait_for_output("PAYLOAD:hello"),
            "the contained editor did not read terminal input: {:?}",
            pty.output()
        );
        assert!(
            pty.wait_for_output("FOREGROUND-RESTORED"),
            "the launcher did not restore the original foreground group: {:?}",
            pty.output()
        );
        pty.write(b"again\n");
        assert!(
            pty.wait_for_output("AFTER:again"),
            "the original foreground group could not read after editor exit: {:?}",
            pty.output()
        );
        assert!(
            pty.wait_for_output("ERROR-FOREGROUND-RESTORED"),
            "the failing editor did not restore the foreground group: {:?}",
            pty.output()
        );
        pty.write(b"after-error\n");
        assert!(
            pty.wait_for_output("AFTER-ERROR:after-error"),
            "the helper could not read after the editor error path: {:?}",
            pty.output()
        );
        assert!(
            pty.wait_for_output("KILL-PAYLOAD-READY"),
            "the cancellable editor did not start: {:?}",
            pty.output()
        );
        assert!(
            pty.wait_for_output("KILL-FOREGROUND-RESTORED"),
            "the cancelled editor did not restore the foreground group: {:?}",
            pty.output()
        );
        pty.write(b"after-kill\n");

        let (status, output) = pty.finish();
        assert!(
            status.success(),
            "PTY launcher helper failed: {status}; {output:?}"
        );
        assert!(
            output.contains("AFTER-KILL:after-kill"),
            "the helper could not read after the editor kill path: {output:?}"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn contained_editor_launcher_reports_windows_as_unsupported_before_spawning() {
        let invocation = zuno_tui::views::external::EditorInvocation {
            program: String::from("a-program-that-must-not-spawn"),
            args: Vec::new(),
        };
        let error = match ContainedEditorLauncher.spawn(&invocation, None) {
            Ok(_) => panic!("Windows external editing must be inert"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);

        let editor = SystemEditor::configured_with_launcher(
            "a-program-that-must-not-spawn",
            Arc::new(ContainedEditorLauncher),
        );
        let error = editor
            .edit(&EditorRequest::new("draft"), EditorCancellation::new())
            .await
            .expect_err("the caller must surface the unsupported launcher");
        assert!(
            error
                .to_string()
                .contains("external editing is disabled on Windows"),
            "the unsupported platform error was hidden: {error}"
        );
    }

    #[cfg(target_os = "linux")]
    struct EditorPty {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        writer: Option<Box<dyn Write + Send>>,
        output: Arc<Mutex<Vec<u8>>>,
        reader: Option<JoinHandle<std::io::Result<()>>>,
    }

    #[cfg(target_os = "linux")]
    impl EditorPty {
        fn spawn() -> Self {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .expect("open editor PTY");
            let mut command = CommandBuilder::new(
                std::env::current_exe().expect("locate the zuno-cli test binary"),
            );
            command.args(["--exact", EDITOR_PTY_HELPER, "--nocapture"]);
            command.env("ZUNO_EDITOR_PTY_HELPER", "1");
            let child = pair
                .slave
                .spawn_command(command)
                .expect("spawn editor PTY helper");
            drop(pair.slave);

            let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
            let writer = pair.master.take_writer().expect("take PTY writer");
            let output = Arc::new(Mutex::new(Vec::new()));
            let reader_output = Arc::clone(&output);
            let reader = std::thread::spawn(move || {
                let mut buffer = [0_u8; 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => return Ok(()),
                        Ok(read) => reader_output
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .extend_from_slice(&buffer[..read]),
                        Err(error) if error.raw_os_error() == Some(5) => return Ok(()),
                        Err(error) => return Err(error),
                    }
                }
            });

            Self {
                child,
                writer: Some(writer),
                output,
                reader: Some(reader),
            }
        }

        fn write(&mut self, input: &[u8]) {
            let writer = self.writer.as_mut().expect("PTY writer is open");
            writer.write_all(input).expect("write PTY input");
            writer.flush().expect("flush PTY input");
        }

        fn wait_for_output(&mut self, expected: &str) -> bool {
            let started = Instant::now();
            while started.elapsed() < std::time::Duration::from_secs(5) {
                if self.output().contains(expected) {
                    return true;
                }
                if self.child.try_wait().expect("poll editor PTY").is_some() {
                    return self.output().contains(expected);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            false
        }

        fn finish(&mut self) -> (portable_pty::ExitStatus, String) {
            let started = Instant::now();
            loop {
                if let Some(status) = self.child.try_wait().expect("poll editor PTY") {
                    self.writer.take();
                    self.join_reader();
                    return (status, self.output());
                }
                if started.elapsed() >= std::time::Duration::from_secs(5) {
                    let output = self.output();
                    self.stop();
                    panic!("editor PTY did not exit: {output:?}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        fn output(&self) -> String {
            String::from_utf8_lossy(&self.output.lock().unwrap_or_else(PoisonError::into_inner))
                .into_owned()
        }

        fn join_reader(&mut self) {
            if let Some(reader) = self.reader.take() {
                reader
                    .join()
                    .expect("PTY reader thread panicked")
                    .expect("read PTY output");
            }
        }

        fn stop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _killed = self.child.kill();
            }
            let _reaped = self.child.wait();
            self.writer.take();
            self.join_reader();
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for EditorPty {
        fn drop(&mut self) {
            self.stop();
        }
    }

    #[test]
    fn tui_config_path_discovery_applies_the_nearest_project_keybind_override() {
        let temp = tempfile::tempdir().expect("TUI config tempdir");
        let config_root = temp.path().join("config/zuno");
        let worktree = temp.path().join("worktree");
        let directory = worktree.join("packages/app");
        fs::create_dir_all(&config_root).expect("create global config root");
        fs::create_dir_all(worktree.join(".zuno")).expect("create outer project config root");
        fs::create_dir_all(directory.join(".zuno")).expect("create nearest project config root");

        fs::write(
            config_root.join("tui.json"),
            r#"{"keybinds":{"session_compact":"ctrl+alt+g"}}"#,
        )
        .expect("write global TUI config");
        fs::write(
            worktree.join(".zuno/tui.json"),
            r#"{"keybinds":{"session_compact":"ctrl+alt+v"}}"#,
        )
        .expect("write outer project TUI config");
        fs::write(
            directory.join(".zuno/tui.jsonc"),
            r#"{"keybinds":{"session_compact":"ctrl+alt+w"}}"#,
        )
        .expect("write nearest project TUI config");

        let paths = tui_config_paths(&config_root, &directory, Some(&worktree));
        assert_eq!(
            paths,
            vec![
                config_root.join("tui.json"),
                config_root.join("tui.jsonc"),
                worktree.join(".zuno/tui.json"),
                directory.join(".zuno/tui.jsonc"),
            ]
        );

        let config = ResolvedTuiConfig::discover(&paths, ResolveOptions::default())
            .expect("discover layered TUI config");
        let mut keymap = Keymap::from_config(&config).expect("build configured keymap");
        match keymap.resolve(
            &["session"],
            Chord::parse("ctrl+alt+w").expect("override chord parses"),
            Instant::now(),
        ) {
            Resolution::Action { definition, .. } => {
                assert_eq!(definition.name, "session_compact");
            }
            other => panic!("nearest on-disk override did not resolve: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_worker_applies_toggle_request_and_replaces_projection() {
        const SERVER: &str = "configured-toggle";
        let catalog = zuno_mcp::Catalog::new([SERVER]);
        let controller = zuno_mcp::McpServerController::from_config(
            catalog,
            ".",
            BTreeMap::from([(
                SERVER.to_owned(),
                zuno_config::schema::mcp::McpServerConfig::Toggle(
                    zuno_config::schema::mcp::McpToggle { enabled: false },
                ),
            )]),
            zuno_mcp::McpLifecycleOptions::default(),
        );
        let projection = McpProjection::new(project_mcp_snapshots(&controller.snapshots()));
        let observed = projection.clone();
        let dirty = Arc::new(AtomicBool::new(false));
        let observed_dirty = Arc::clone(&dirty);
        let (requests, request_source) = mpsc::channel(MCP_TOGGLE_CHANNEL_CAPACITY);
        let (wake, mut wake_source) = zuno_tui::app::terminal_event_channel();
        let (_shutdown, shutdown_source) = watch::channel(false);
        let worker = tokio::spawn(drive_mcp_lifecycle(McpLifecycleWorker {
            controller,
            requests: request_source,
            initial: Vec::new(),
            concurrency: NonZeroUsize::MIN,
            projection,
            dirty,
            wake,
            shutdown: shutdown_source,
        }));

        requests
            .send(McpToggleRequest {
                server: SERVER.to_owned(),
                desired_enabled: true,
            })
            .await
            .expect("worker accepts one toggle");

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    observed.snapshot().as_slice(),
                    [McpServer {
                        state: McpState::Failed(_),
                        desired_enabled: true,
                        ..
                    }]
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("toggle reaches the controller and projection");

        assert!(observed_dirty.load(Ordering::Acquire));
        assert!(matches!(wake_source.try_recv(), Ok(TerminalEvent::Wake)));
        drop(requests);
        worker
            .await
            .expect("worker exits after request channel closes");
    }

    #[tokio::test]
    async fn mcp_worker_overlaps_different_servers_up_to_the_configured_limit() {
        let connector = Arc::new(BlockingMcpConnector::default());
        let catalog = zuno_mcp::Catalog::new(["alpha", "beta"]);
        let controller = zuno_mcp::McpServerController::with_connector(
            catalog,
            ["alpha", "beta"],
            Arc::clone(&connector),
            zuno_mcp::McpLifecycleOptions::default(),
        );
        let projection = McpProjection::new(project_mcp_snapshots(&controller.snapshots()));
        let (requests, request_source) = mpsc::channel(MCP_TOGGLE_CHANNEL_CAPACITY);
        drop(requests);
        let (wake, _wake_source) = zuno_tui::app::terminal_event_channel();
        let (_shutdown, shutdown_source) = watch::channel(false);
        let worker = tokio::spawn(drive_mcp_lifecycle(McpLifecycleWorker {
            controller,
            requests: request_source,
            initial: vec![
                McpToggleRequest {
                    server: "alpha".to_owned(),
                    desired_enabled: true,
                },
                McpToggleRequest {
                    server: "beta".to_owned(),
                    desired_enabled: true,
                },
            ],
            concurrency: NonZeroUsize::new(2).expect("non-zero"),
            projection,
            dirty: Arc::new(AtomicBool::new(false)),
            wake,
            shutdown: shutdown_source,
        }));

        connector.wait_for_started(2).await;
        assert_eq!(
            connector.maximum_active(),
            2,
            "different MCP servers did not overlap"
        );
        connector.release_all(2);
        worker.await.expect("MCP worker exits");
    }

    #[tokio::test]
    async fn mcp_worker_serializes_repeated_operations_for_one_server() {
        let connector = Arc::new(BlockingMcpConnector::default());
        let catalog = zuno_mcp::Catalog::new(["same"]);
        let controller = zuno_mcp::McpServerController::with_connector(
            catalog,
            ["same"],
            Arc::clone(&connector),
            zuno_mcp::McpLifecycleOptions::default(),
        );
        let projection = McpProjection::new(project_mcp_snapshots(&controller.snapshots()));
        let (requests, request_source) = mpsc::channel(MCP_TOGGLE_CHANNEL_CAPACITY);
        drop(requests);
        let (wake, _wake_source) = zuno_tui::app::terminal_event_channel();
        let (_shutdown, shutdown_source) = watch::channel(false);
        let worker = tokio::spawn(drive_mcp_lifecycle(McpLifecycleWorker {
            controller,
            requests: request_source,
            initial: vec![
                McpToggleRequest {
                    server: "same".to_owned(),
                    desired_enabled: true,
                },
                McpToggleRequest {
                    server: "same".to_owned(),
                    desired_enabled: false,
                },
                McpToggleRequest {
                    server: "same".to_owned(),
                    desired_enabled: true,
                },
            ],
            concurrency: NonZeroUsize::new(8).expect("non-zero"),
            projection,
            dirty: Arc::new(AtomicBool::new(false)),
            wake,
            shutdown: shutdown_source,
        }));

        connector.wait_for_started(1).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            connector.started.load(Ordering::Acquire),
            1,
            "a second operation for the same MCP server started concurrently"
        );
        connector.release_one();
        connector.wait_for_started(2).await;
        assert_eq!(connector.maximum_active(), 1);
        connector.release_one();
        worker.await.expect("MCP worker exits");
    }

    #[tokio::test]
    async fn snapshot_history_moves_only_after_checked_undo_and_redo() {
        let temp = tempfile::tempdir().expect("snapshot fixture");
        let root = temp.path().join("snapshot");
        let worktree = temp.path().join("worktree");
        fs::create_dir_all(&root).expect("create snapshot root");
        fs::create_dir_all(&worktree).expect("create worktree");
        for args in [
            vec!["init", "-q", "."],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&worktree)
                .status()
                .expect("run git fixture command");
            assert!(status.success());
        }
        let file = worktree.join("turn.txt");
        fs::write(&file, "before\n").expect("seed tracked file");
        let status = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&worktree)
            .status()
            .expect("stage fixture");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(&worktree)
            .status()
            .expect("commit fixture");
        assert!(status.success());

        let store =
            zuno_snapshot::Store::open(zuno_snapshot::Location::new(root, "project", &worktree));
        let mut history = SnapshotHistory::new(store.clone());
        let (events, _event_source) = event_channel();
        let capture = store
            .begin_turn()
            .expect("capture before turn")
            .expect("snapshots enabled");
        fs::write(&file, "after\n").expect("simulate turn edit");
        finish_snapshot(capture, &mut history, &events).await;
        assert_eq!(history.undo.len(), 1);
        assert!(history.redo.is_empty());

        restore_snapshot(HostCommand::Undo, &mut history, &events)
            .await
            .expect("undo succeeds");
        assert_eq!(fs::read_to_string(&file).expect("read undo"), "before\n");
        assert!(history.undo.is_empty());
        assert_eq!(history.redo.len(), 1);

        restore_snapshot(HostCommand::Redo, &mut history, &events)
            .await
            .expect("redo succeeds");
        assert_eq!(fs::read_to_string(&file).expect("read redo"), "after\n");
        assert_eq!(history.undo.len(), 1);
        assert!(history.redo.is_empty());

        fs::write(&file, "manual drift\n").expect("introduce drift");
        let error = restore_snapshot(HostCommand::Undo, &mut history, &events)
            .await
            .expect_err("drift refuses the whole restore");
        assert!(error.contains("refused"), "{error}");
        assert_eq!(history.undo.len(), 1);
        assert!(history.redo.is_empty());
        assert_eq!(
            fs::read_to_string(&file).expect("read drifted file"),
            "manual drift\n"
        );
    }

    #[tokio::test]
    async fn successful_new_turn_clears_redo_history() {
        let temp = tempfile::tempdir().expect("snapshot fixture");
        let root = temp.path().join("snapshot");
        let worktree = temp.path().join("worktree");
        fs::create_dir_all(&root).expect("create snapshot root");
        fs::create_dir_all(&worktree).expect("create worktree");
        let status = std::process::Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(&worktree)
            .status()
            .expect("initialize git fixture");
        assert!(status.success());
        let file = worktree.join("turn.txt");
        fs::write(&file, "one\n").expect("seed file");
        let store =
            zuno_snapshot::Store::open(zuno_snapshot::Location::new(root, "project", &worktree));
        let mut history = SnapshotHistory::new(store.clone());
        let (events, _event_source) = event_channel();

        let first = store
            .begin_turn()
            .expect("first capture")
            .expect("snapshots enabled");
        fs::write(&file, "two\n").expect("first edit");
        finish_snapshot(first, &mut history, &events).await;
        restore_snapshot(HostCommand::Undo, &mut history, &events)
            .await
            .expect("undo first turn");
        assert_eq!(history.redo.len(), 1);

        let second = store
            .begin_turn()
            .expect("second capture")
            .expect("snapshots enabled");
        fs::write(&file, "three\n").expect("new edit after undo");
        finish_snapshot(second, &mut history, &events).await;

        assert!(history.redo.is_empty());
        assert_eq!(history.undo.len(), 1);
        assert_eq!(fs::read_to_string(file).expect("read new turn"), "three\n");
    }

    /// The owner's report, at the surface he saw it: `/model` offered only the session
    /// provider's models. The picker groups by provider and already could have shown more
    /// than one, so the failure was upstream of the view — every entry arrived carrying
    /// the same hard-bound provider. Asserting on **distinct** headings is what makes this
    /// fail for that cause rather than for a shorter list.
    #[test]
    fn the_model_picker_offers_every_provider_the_catalog_holds() {
        let entries = [
            (
                "amazon-bedrock/anthropic.claude-opus-4-6-v1",
                "Claude Opus 4.6",
                "Bedrock",
            ),
            (
                "amazon-bedrock/amazon.nova-lite-v1:0",
                "Nova Lite",
                "Bedrock",
            ),
            ("myopenai/gpt-5", "GPT-5", "My OpenAI"),
            ("myopenai/o4", "O4", "My OpenAI"),
        ];
        let entries = entries
            .into_iter()
            .map(|(id, name, provider)| zuno_tui::views::picker::ModelEntry {
                id: id.to_owned(),
                name: name.to_owned(),
                provider: provider.to_owned(),
                reasoning: false,
            })
            .collect::<Vec<_>>();

        let providers = entries
            .iter()
            .map(|entry| entry.provider.as_str())
            .collect::<BTreeSet<_>>();
        assert!(
            providers.len() >= 2,
            "every entry was bound to one provider, so no second vendor can reach the \
             picker: {providers:?}"
        );

        let picker = zuno_tui::views::picker::model_picker(
            zuno_tui::views::ViewContext::defaults(),
            entries,
        )
        .selecting("myopenai/o4");
        let offered = picker
            .visible()
            .iter()
            .map(|item| item.group.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            offered,
            BTreeSet::from(["Bedrock", "My OpenAI"]),
            "the picker's own rows do not span both providers"
        );
        // `selecting` must still land on the session's model now that it is one row among
        // hundreds rather than one among a single provider's list.
        assert_eq!(
            picker.selected().map(|item| item.value.as_str()),
            Some("myopenai/o4"),
            "the current model is no longer pre-selected"
        );
    }

    /// A model id may itself contain slashes, so only the first one is the provider.
    #[test]
    fn a_nested_model_id_keeps_every_segment_past_the_provider() {
        let entry = crate::cmd::turn::CatalogModelChoice {
            id: String::from("anyapi/openai/gpt"),
            name: String::from("OpenAI GPT"),
            provider: String::from("Any API"),
        };
        assert_eq!(entry.provider, "Any API");
        assert_eq!(entry.name, "OpenAI GPT");
        assert_eq!(
            entry.id, "anyapi/openai/gpt",
            "display metadata must not rewrite the exact value turn resolution accepts"
        );
    }

    #[test]
    fn runtime_inventory_projects_components_and_cleanup_failures() {
        use zuno_runtime::{
            ComponentSnapshot, LifecycleDiagnostic, LifecycleFailureKind, LifecyclePhase,
            LifecycleState, RuntimeSnapshot,
        };
        use zuno_tui::views::ambient::Health;

        let services = lifecycle_services(&[RuntimeSnapshot {
            name: "profile".to_owned(),
            state: LifecycleState::Uncertain,
            profile_id: Some("default".to_owned()),
            components: vec![ComponentSnapshot {
                id: "zuno.tools".to_owned(),
                state: LifecycleState::Active,
                effects: vec!["watch".to_owned()],
                provides: vec!["tools".to_owned()],
                requires: Vec::new(),
            }],
            capabilities: Vec::new(),
            diagnostics: vec![LifecycleDiagnostic {
                component_id: "zuno.mcp".to_owned(),
                effect_id: "remote".to_owned(),
                phase: LifecyclePhase::Stop,
                kind: LifecycleFailureKind::TimedOut,
                message: "close timed out".to_owned(),
            }],
        }]);

        assert_eq!(services[0].health, Health::Faulted);
        assert_eq!(services[1].name, "zuno.tools");
        assert_eq!(services[1].health, Health::Ready);
        assert!(services[1].detail.contains("effects 1"));
        assert_eq!(services[2].health, Health::Faulted);
        assert!(services[2].detail.contains("close timed out"));
    }

    struct FakeLifecycleHost {
        name: &'static str,
        log: Arc<Mutex<Vec<String>>>,
        fail_shutdown: bool,
    }

    impl HostLifecycle for FakeLifecycleHost {
        fn shutdown_host(&mut self) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .expect("lifecycle log")
                    .push(format!("stop:{}", self.name));
                if self.fail_shutdown {
                    Err(format!("{} uncertain", self.name))
                } else {
                    Ok(())
                }
            })
        }
    }

    #[tokio::test]
    async fn host_replacement_stops_the_old_host_before_installing_the_candidate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut current = FakeLifecycleHost {
            name: "old",
            log: Arc::clone(&log),
            fail_shutdown: false,
        };
        let candidate_log = Arc::clone(&log);
        replace_host(&mut current, || async move {
            candidate_log
                .lock()
                .expect("lifecycle log")
                .push("start:new".to_owned());
            Ok(FakeLifecycleHost {
                name: "new",
                log: candidate_log,
                fail_shutdown: false,
            })
        })
        .await
        .expect("replacement succeeds");

        assert_eq!(current.name, "new");
        assert_eq!(
            *log.lock().expect("lifecycle log"),
            ["stop:old", "start:new"]
        );
    }

    #[tokio::test]
    async fn failed_old_host_shutdown_never_starts_the_candidate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut current = FakeLifecycleHost {
            name: "old",
            log: Arc::clone(&log),
            fail_shutdown: true,
        };
        let candidate_log = Arc::clone(&log);
        let error = replace_host(&mut current, || async move {
            candidate_log
                .lock()
                .expect("lifecycle log")
                .push("start:new".to_owned());
            Ok(FakeLifecycleHost {
                name: "new",
                log: candidate_log,
                fail_shutdown: false,
            })
        })
        .await
        .expect_err("uncertain old host blocks replacement");

        assert!(error.contains("old uncertain"));
        assert_eq!(current.name, "old");
        assert_eq!(*log.lock().expect("lifecycle log"), ["stop:old"]);
    }

    struct LeasedHost {
        lease: Option<zuno_extension::CompositionLease>,
    }

    impl HostLifecycle for LeasedHost {
        fn shutdown_host(&mut self) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                self.lease.take();
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn extension_commit_happens_only_after_the_old_host_releases_its_lease() {
        let registry = Arc::new(zuno_extension::ExtensionRegistry::new());
        let scope = zuno_extension::Scope::new(std::path::Path::new("/workspace"));
        let package = serde_json::from_value(serde_json::json!({
            "apiVersion": zuno_extension::API_VERSION,
            "id": "review",
            "description": "review workflow",
            "workflows": {
                "review": {
                    "description": "review",
                    "prompt": "Review the change."
                }
            }
        }))
        .expect("valid extension package");
        registry.define(&scope, package).expect("define");
        let transaction = match registry.stage_run(&scope, "review", &[]).expect("stage") {
            zuno_extension::StageOutcome::Pending(transaction) => transaction,
            zuno_extension::StageOutcome::Unchanged { .. } => panic!("run must change"),
        };
        let mut current = LeasedHost {
            lease: Some(
                registry
                    .acquire_active(&scope, registry.active_revision(&scope))
                    .expect("old host lease"),
            ),
        };
        let candidate_registry = Arc::clone(&registry);
        let candidate_transaction = transaction.clone();

        replace_host(&mut current, || async move {
            let lease = candidate_registry
                .begin_transition(&candidate_transaction)
                .map_err(|error| error.to_string())?
                .commit()
                .map_err(|error| error.to_string())?;
            Ok(LeasedHost { lease: Some(lease) })
        })
        .await
        .expect("quiescent replacement");

        assert_eq!(
            registry.dynamic_statuses(&scope)[0].state,
            zuno_extension::DynamicState::Running
        );
        assert_eq!(registry.active_revision(&scope), transaction.revision());
    }
}
