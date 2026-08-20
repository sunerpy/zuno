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

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
use zuno_tui::theme::{EnvironmentPalette, Mode, SystemThemeOutcome, ThemeRegistry};
use zuno_tui::views::ViewContext;
use zuno_tui::views::dialog::DialogHost;
use zuno_tui::views::external::{
    EditorCancellation, EditorProcess, EditorProcessLauncher, EditorRequest, ExternalEditor,
    ExternalError, SystemEditor,
};
use zuno_tui::views::message::Message;
use zuno_tui::views::picker::{McpProjection, McpServer, McpState, McpToggleRequest};
use zuno_tui::views::session::{PromptSubmission, SessionScreen, scopes};
use zuno_tui::views::slash::{CatalogCommand, HostCommand};

use super::tui_permission::{AutoApproval, PermissionBridge, PermissionBroker};
use super::tui_question::{QuestionBridge, QuestionBroker};
use super::turn::{SessionChoice, TurnHost, TurnOptions, TurnPlan};
use crate::command::TuiArgs;
use crate::environment::StartupEnvironment;

/// How many prompts may be in flight. One, so a second is refused and not queued.
const PROMPT_CHANNEL_CAPACITY: usize = 1;

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

const EDITOR_CHANNEL_CAPACITY: usize = 1;

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
    // `COLORFGBG` is the only non-invasive mode signal available before
    // `TerminalSession` owns terminal I/O. A complete palette query needs an
    // stdin/stdout escape-sequence round trip; doing that here would violate this
    // module's rule that startup failures are resolved before raw mode. When the
    // environment has no usable signal, retain today's dark-mode behaviour.
    let mode = match themes.refresh_system_theme(&EnvironmentPalette, None, Mode::Dark) {
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
    let tui_plugins = runtime.block_on(plan.load_tui_plugins(environment));
    // Read before `TurnHost::open` consumes the plan, and before raw mode, so a slow
    // skill scan cannot delay the first frame of an already-entered alternate screen.
    let facts = runtime.block_on(SessionFacts::resolve(&plan, environment));
    let catalog = runtime.block_on(session_catalog(&plan, environment));
    let broker = Arc::new(PermissionBroker::new(terminal_sender.clone()));
    let question_broker = Arc::new(QuestionBroker::new(terminal_sender.clone()));
    let question: Arc<dyn QuestionAsker> = Arc::clone(&question_broker) as Arc<dyn QuestionAsker>;
    let approval: Arc<dyn PermissionAsker> = if args.auto {
        Arc::new(AutoApproval)
    } else {
        Arc::clone(&broker) as Arc<dyn PermissionAsker>
    };
    let driver_approval = Arc::clone(&approval);
    let driver_options = options.clone();
    let driver_environment = environment.clone();
    let host = TurnHost::open_with_runtime_and_mcp(
        plan,
        environment,
        approval,
        Some(Arc::clone(&question)),
        SessionRunRegistry::new(),
        Some(mcp_catalog.clone()),
    )?;
    let engine_sender = host.with_event_hooks(engine_sender);
    let plugins = host.plugin_runtime();
    let slash_commands = host
        .commands()
        .map(|command| CatalogCommand::new(command.name.clone(), command.description.clone()))
        .collect::<Vec<_>>();
    broker.bind_session(host.session_id());

    let (cancel_sender, cancel_receiver) = mpsc::channel(CANCEL_CHANNEL_CAPACITY);
    let (selection_sender, selection_receiver) = mpsc::channel(SELECTION_CHANNEL_CAPACITY);
    let (mcp_toggle_sender, mcp_toggle_receiver) = mpsc::channel(MCP_TOGGLE_CHANNEL_CAPACITY);
    let (editor_sender, editor_receiver) = mpsc::channel(EDITOR_CHANNEL_CAPACITY);
    let (editor_result_sender, editor_result_receiver) = mpsc::channel(EDITOR_CHANNEL_CAPACITY);
    let control = host.control();

    let (report_sender, report_receiver) = mpsc::channel(LSP_CHANNEL_CAPACITY);
    let (edit_sender, edit_receiver) = mpsc::channel(EDIT_SIGNAL_CHANNEL_CAPACITY);
    let pending_edits = zuno_tui::views::lsp::PendingEdits::new(edit_sender);
    // The reader holds no sender, so the screen dropping its handle really does close
    // the channel and end the checker task.
    let edit_reader = pending_edits.reader();
    let (history_sender, history_receiver) = mpsc::channel(PROMPT_HISTORY_CHANNEL_CAPACITY);
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
        .with_catalog(catalog)
        .with_diagnostics_source(report_receiver)
        .with_edit_sink(pending_edits)
        .with_prompt_history(history.into_entries(), history_sender)
        .with_external_editor(editor_sender, editor_result_receiver)
        // A clone rather than a borrow: `KeyDispatcher` takes the keymap by value below,
        // and the keybinding reference has to list what the *user's* keymap resolved
        // rather than the shipped defaults.
        .with_keymap(keymap.clone());
    facts.describe(
        &mut screen,
        host.tool_count(),
        RuntimeIdentity::resolve(host.session_id(), plugins.as_ref(), environment.resolved()),
    );
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
    if let Some(prompt) = args
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
    {
        screen.submit_prompt(prompt);
    }
    // The waker is what makes a toast expire on its deadline rather than at the next
    // event. It is the terminal channel that already exists, not a new one; see
    // `zuno_tui::views::toast` for why one deadline and one wake was chosen over giving
    // the redraw scheduler a fourth tier.
    let dialogs =
        DialogHost::new(context.clone(), Box::new(screen)).with_waker(terminal_sender.clone());
    let bridge = PermissionBridge::new(context.clone(), broker, dialogs)
        .with_question(QuestionBridge::new(context, question_broker));
    let root = KeyDispatcher::new(keymap, scopes(), Box::new(bridge));

    let lifecycle = Arc::new(CrosstermLifecycle::new(config.mouse));
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

    let session = TerminalSession::start(lifecycle).map_err(to_string)?;
    let outcome = runtime.block_on(async move {
        let input = tokio::spawn(zuno_tui::app::forward_terminal_input(
            terminal_sender,
            input_control,
        ));
        let turns = tokio::spawn(drive_turns(
            TurnDriver {
                host,
                options: driver_options,
                approval: driver_approval,
                question,
                reference_root,
                mcp_catalog,
                mcp_dirty: Arc::clone(&mcp_dirty),
                snapshots: SnapshotHistory::new(snapshot_store),
            },
            prompt_receiver,
            selection_receiver,
            driver_environment,
            engine_sender,
        ));
        let mcp = tokio::spawn(drive_mcp_lifecycle(
            mcp_controller,
            mcp_toggle_receiver,
            initial_mcp_targets,
            mcp_projection,
            mcp_dirty,
            mcp_wake,
        ));
        let checks = tokio::spawn(super::tui_lsp::check_edits(
            probe,
            edit_reader,
            edit_receiver,
            report_sender,
        ));
        let cancels = tokio::spawn(forward_cancellations(control, cancel_receiver));
        let history = tokio::spawn(record_prompt_history(history_path, history_receiver));
        let (editor_shutdown, editor_shutdown_source) = watch::channel(false);
        let mut editor = tokio::spawn(drive_external_editor(
            editor_lease,
            external_editor,
            editor_receiver,
            editor_result_sender,
            editor_wake,
            editor_shutdown_source,
        ));
        let outcome = app.run().await;
        let _stopping = editor_shutdown.send(true);
        if tokio::time::timeout(std::time::Duration::from_secs(3), &mut editor)
            .await
            .is_err()
        {
            editor.abort();
            let _cancelled = tokio::time::timeout(std::time::Duration::from_secs(3), editor).await;
        }
        input.abort();
        turns.abort();
        checks.abort();
        cancels.abort();
        mcp.abort();
        // Aborted with the rest rather than awaited, because the sender lives inside the
        // render tree that several `Arc`s outlive here — the channel never closes, so a
        // wait would be a hang and a timed wait would tax every exit. The cost is one
        // narrow race: a prompt submitted in the instant before quitting may not have
        // been appended. `try_send` wakes this task immediately and an append is a single
        // write, so what is lost is a recall, never the turn, which has already run.
        history.abort();
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
async fn record_prompt_history(path: PathBuf, mut records: mpsc::Receiver<String>) {
    let mut reported = false;
    while let Some(entry) = records.recv().await {
        let Some(line) = zuno_tui::views::editor::PromptHistory::encode(&entry) else {
            continue;
        };
        let target = path.clone();
        let written = tokio::task::spawn_blocking(move || append_line(&target, &line)).await;
        if let Ok(Err(error)) = written
            && !reported
        {
            reported = true;
            tracing::warn!(
                path = %path.display(),
                %error,
                "failed to append to the prompt history; later failures are not repeated"
            );
        }
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
    fn describe(self, screen: &mut SessionScreen, tools: usize, runtime: RuntimeIdentity) {
        // Built before the moves below, which hand `lsp` to the ambient panel. The MCP
        // group is deliberately absent: the screen reads that from its live projection at
        // open time, so the census cannot state a connection state the MCP dialog has
        // already moved on from.
        screen.set_diagnostics(
            vec![
                zuno_tui::views::diagnostics::Group::new("LSP servers", self.lsp.clone()),
                zuno_tui::views::diagnostics::Group::new("Plugins", runtime.plugins),
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
        if let Some(branch) = self.branch.as_deref() {
            screen.status_mut().set_git_branch(branch);
        }

        let directory = (!self.directory.is_empty()).then(|| self.directory.clone());
        // No agent or model: `status_mut().describe` above already states both on the one
        // row that is never dropped at any width, and the welcome screen sat directly on
        // top of it repeating them verbatim.
        *screen.welcome_mut().facts_mut() = zuno_tui::views::welcome::WelcomeFacts {
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
    plugins: Vec<zuno_tui::views::ambient::Service>,
    terminal: Option<String>,
}

impl RuntimeIdentity {
    /// Read the host-derived halves of the census and the debug report.
    ///
    /// `TERM_PROGRAM` before `TERM`: the former names the emulator (`tmux`,
    /// `WezTerm`), while the latter names its termcap entry (`xterm-256color`), and
    /// several unrelated emulators report the same `TERM`. Neither is guaranteed, so an
    /// absent value is omitted rather than guessed.
    fn resolve(
        session: &str,
        plugins: Option<&std::sync::Arc<super::plugin_runtime::PluginRuntime>>,
        env: &zuno_paths::Env,
    ) -> Self {
        use zuno_tui::views::ambient::{Health, Service};
        let plugins = plugins
            .map(|runtime| {
                runtime
                    .census()
                    .into_iter()
                    .map(|(id, hooks)| {
                        let detail = if hooks.is_empty() {
                            String::from("no hooks")
                        } else {
                            hooks.join(", ")
                        };
                        // `Ready`, because a plugin in this list is loaded: the load either
                        // succeeded or the plugin is not here to be listed.
                        Service::new(id, Health::Ready).detailed(detail)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            session: session.to_owned(),
            plugins,
            terminal: env
                .value("TERM_PROGRAM")
                .or_else(|| env.value("TERM"))
                .map(str::to_owned),
        }
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
/// One list, from [`TurnPlan::catalog_model_ids`], which is filled by
/// `Catalog::model_lines` — the same enumeration `zuno models` prints. Two enumerations
/// is precisely how the surfaces came to disagree.
async fn session_catalog(
    plan: &TurnPlan,
    environment: &StartupEnvironment,
) -> zuno_tui::views::session::SessionCatalog {
    let env = environment.resolved();
    let models = plan
        .catalog_model_ids()
        .into_iter()
        .map(|qualified| model_entry(&qualified))
        .collect();
    // Filtered here rather than in `agent::list`, which must keep returning everything: the
    // turn loop resolves a delegation by name and needs the subagents this drops. Both TUI
    // surfaces read this one list — the `<leader>a` picker and the cycling keys — so one
    // filter is what stops them disagreeing about what "the agents" are. A subagent is
    // reachable only by delegation and `hidden` is its author asking not to be offered, so
    // neither is a valid choice for the session's own agent.
    let agents = zuno_catalog::agent::load(plan.directory(), plan.worktree(), env)
        .map(|agents| {
            agents
                .into_iter()
                .filter(|agent| {
                    !matches!(agent.mode, zuno_catalog::agent::AgentMode::Subagent)
                        && agent.hidden != Some(true)
                })
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

/// Split one `provider/model` line into the entry the picker groups by.
///
/// `split_once`, never `rsplit_once` or `split`: a model id may itself contain slashes —
/// `anyapi/openai/gpt` is a real catalog shape, pinned by `turn_tests.rs`'s
/// `model_selection_splits_only_the_provider_prefix`. This has to divide the string
/// exactly where `select_model` will divide it again, or a row would resolve to a
/// different model than the one it named.
///
/// A line with no slash cannot come from `Catalog::model_lines`, which formats every one
/// as `{provider}/{model}`. Should one arrive anyway the whole string becomes the name
/// under an empty heading rather than being dropped: a visibly odd row is debuggable, and
/// a silently missing model is the defect this function exists to fix.
fn model_entry(qualified: &str) -> zuno_tui::views::picker::ModelEntry {
    let (provider, name) = qualified.split_once('/').unwrap_or(("", qualified));
    zuno_tui::views::picker::ModelEntry {
        id: qualified.to_owned(),
        name: name.to_owned(),
        provider: provider.to_owned(),
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
struct TurnRebuild<'a> {
    options: &'a TurnOptions,
    environment: &'a StartupEnvironment,
    approval: &'a Arc<dyn PermissionAsker>,
    question: &'a Arc<dyn QuestionAsker>,
    events: &'a TurnEventSender,
    mcp_catalog: &'a zuno_mcp::Catalog,
}

async fn apply_selection(
    selection: zuno_tui::views::session::Selection,
    host: &mut TurnHost,
    rebuild: &TurnRebuild<'_>,
) -> Option<TurnEventSender> {
    let mut next = rebuild.options.clone();
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
        let plan = TurnPlan::resolve(&next, rebuild.environment).await?;
        TurnHost::open_with_runtime_and_mcp(
            plan,
            rebuild.environment,
            Arc::clone(rebuild.approval),
            Some(Arc::clone(rebuild.question)),
            SessionRunRegistry::new(),
            Some(rebuild.mcp_catalog.clone()),
        )
    }
    .await;
    match rebuilt {
        Ok(replacement) => {
            let hooked = replacement.with_event_hooks(rebuild.events.clone());
            *host = replacement;
            Some(hooked)
        }
        Err(message) => {
            let _reported = rebuild
                .events
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
    question: Arc<dyn QuestionAsker>,
    reference_root: PathBuf,
    mcp_catalog: zuno_mcp::Catalog,
    mcp_dirty: Arc<AtomicBool>,
    snapshots: SnapshotHistory,
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

async fn drive_turns(
    mut driver: TurnDriver,
    mut prompts: mpsc::Receiver<PromptSubmission>,
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
                let rebuild = TurnRebuild {
                    options: &driver.options,
                    environment: &environment,
                    approval: &driver.approval,
                    question: &driver.question,
                    events: &events,
                    mcp_catalog: &driver.mcp_catalog,
                };
                if let Some(rebuilt) = apply_selection(
                    selection,
                    &mut driver.host,
                    &rebuild,
                )
                .await
                {
                    events = rebuilt;
                }
                continue;
            }
        };
        if driver.mcp_dirty.swap(false, Ordering::AcqRel)
            && let Some(rebuilt) = refresh_mcp_host(&mut driver, &environment, &events).await
        {
            events = rebuilt;
        }
        drive_one(
            &mut driver.host,
            prompt,
            &mut prompts,
            &driver.reference_root,
            &events,
            &mut driver.snapshots,
        )
        .await;
    }
}

async fn refresh_mcp_host(
    driver: &mut TurnDriver,
    environment: &StartupEnvironment,
    events: &TurnEventSender,
) -> Option<TurnEventSender> {
    let mut next = driver.options.clone();
    next.session = SessionChoice::Existing(driver.host.session_id().to_owned());
    next.model = Some(driver.host.qualified_model());
    next.agent = Some(driver.host.agent_name().to_owned());
    let rebuilt = async {
        let plan = TurnPlan::resolve(&next, environment).await?;
        TurnHost::open_with_runtime_and_mcp(
            plan,
            environment,
            Arc::clone(&driver.approval),
            Some(Arc::clone(&driver.question)),
            SessionRunRegistry::new(),
            Some(driver.mcp_catalog.clone()),
        )
    }
    .await;
    match rebuilt {
        Ok(replacement) => {
            let hooked = replacement.with_event_hooks(events.clone());
            driver.host = replacement;
            Some(hooked)
        }
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
            None
        }
    }
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

async fn drive_mcp_lifecycle(
    controller: zuno_mcp::McpServerController,
    mut requests: mpsc::Receiver<McpToggleRequest>,
    initial: Vec<McpToggleRequest>,
    projection: McpProjection,
    dirty: Arc<AtomicBool>,
    wake: mpsc::Sender<TerminalEvent>,
) {
    type ToggleFuture = Pin<
        Box<
            dyn Future<Output = Result<zuno_mcp::McpServerSnapshot, zuno_mcp::McpLifecycleError>>
                + Send,
        >,
    >;

    let mut changes = controller.subscribe();
    let mut initial = VecDeque::from(initial);
    let mut active: Option<ToggleFuture> = None;
    loop {
        if active.is_none()
            && let Some(request) = initial.pop_front()
        {
            let controller = controller.clone();
            active = Some(Box::pin(async move {
                controller
                    .set_enabled(&request.server, request.desired_enabled)
                    .await
            }));
        }

        tokio::select! {
            change = changes.recv() => match change {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    projection.replace(project_mcp_snapshots(&controller.snapshots()));
                    dirty.store(true, Ordering::Release);
                    let _nudged = wake.try_send(TerminalEvent::Wake);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            result = async { active.as_mut().expect("guarded active MCP operation").await }, if active.is_some() => {
                let _completed = result;
                active = None;
                projection.replace(project_mcp_snapshots(&controller.snapshots()));
                dirty.store(true, Ordering::Release);
                let _nudged = wake.try_send(TerminalEvent::Wake);
            },
            request = requests.recv(), if active.is_none() && initial.is_empty() => match request {
                Some(request) => {
                    let controller = controller.clone();
                    active = Some(Box::pin(async move {
                        controller
                            .set_enabled(&request.server, request.desired_enabled)
                            .await
                    }));
                }
                None => return,
            },
        }
    }
}

async fn drive_one(
    host: &mut TurnHost,
    prompt: PromptSubmission,
    prompts: &mut mpsc::Receiver<PromptSubmission>,
    reference_root: &Path,
    events: &TurnEventSender,
    snapshots: &mut SnapshotHistory,
) {
    {
        // Counted for the memory sampler's session attribution, which is what tells
        // "one session leaking" from "many sessions, each fine". A guard rather than a
        // manual increment so an early `?` or a panic cannot leave the count high.
        let _session = zuno_observability::memory::SessionCount::enter();
        let outcome = async {
            let prompt = super::tui_reference::resolve_submission(reference_root, prompt).await?;
            if let PromptSubmission::Host(command) = prompt {
                return restore_snapshot(command, snapshots, events).await;
            }
            let capture = begin_snapshot(&snapshots.store, events).await;
            match prompt {
                PromptSubmission::Text(prompt) => host.drive(&prompt, events.clone()).await?,
                PromptSubmission::Content { text, content } => {
                    host.drive_content(&text, &content, events.clone()).await?
                }
                PromptSubmission::Command { name, arguments } => {
                    host.drive_command(&name, &arguments, events.clone())
                        .await?
                }
                PromptSubmission::Host(_) => unreachable!("host submissions return before driving"),
            }
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, PoisonError};
    use std::thread::JoinHandle;
    use std::time::Instant;

    #[cfg(target_os = "linux")]
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use zuno_engine::terminal_lease::TerminalBroker;
    use zuno_testkit::FakeTerminalOwner;
    use zuno_tui::config::ResolveOptions;
    use zuno_tui::keybind::{Chord, Resolution};

    use super::*;

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
        assert!(matches!(
            wake_source.try_recv(),
            Ok(zuno_tui::app::TerminalEvent::Wake)
        ));
        drop(requests);
        worker.await.expect("worker exits with its request channel");
    }

    struct HangingEditor {
        killed: Arc<AtomicBool>,
        reaped: Arc<AtomicBool>,
    }

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
    async fn wait_for_editor_pid(path: &std::path::Path) -> u32 {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(path)
                    && let Ok(pid) = value.parse::<u32>()
                {
                    return pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_editor_timeout_kills_and_reaps_before_forced_reclaim() {
        let owner = Arc::new(FakeTerminalOwner::new());
        let transcript = owner.transcript();
        let lease: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::with_timeout(
            owner,
            std::time::Duration::from_millis(100),
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
        let pid = wait_for_editor_pid(&pid_path).await;
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
        let (_directory, editor, wrapper_pid_path, descendant_pid_path) = wrapper_system_editor();
        let owner = Arc::new(DescendantObservingOwner {
            descendant_pid: descendant_pid_path.clone(),
            alive_at_reclaim: AtomicBool::new(false),
            reclaimed: AtomicBool::new(false),
        });
        let lease: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::with_timeout(
            Arc::clone(&owner) as Arc<dyn zuno_engine::terminal_lease::TerminalOwner>,
            std::time::Duration::from_millis(100),
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
        let wrapper_pid = wait_for_editor_pid(&wrapper_pid_path).await;
        let descendant_pid = wait_for_editor_pid(&descendant_pid_path).await;
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
                        .any(|transition| transition.plugin() == "tui")
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
        let owner = Arc::new(FakeTerminalOwner::new());
        let transcript = owner.transcript();
        let lease: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
        let (_directory, editor, pid_path) = hanging_system_editor();
        let (requests, request_source) = mpsc::channel(1);
        let (results, _result_source) = mpsc::channel(1);
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
                        .any(|transition| transition.plugin() == "tui")
                })
                .await,
            "the editor did not acquire the lease"
        );
        let pid = wait_for_editor_pid(&pid_path).await;

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
        let worker = tokio::spawn(drive_mcp_lifecycle(
            controller,
            request_source,
            Vec::new(),
            projection,
            dirty,
            wake,
        ));

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
        let lines = [
            "amazon-bedrock/anthropic.claude-opus-4-6-v1",
            "amazon-bedrock/amazon.nova-lite-v1:0",
            "myopenai/gpt-5",
            "myopenai/o4",
        ];
        let entries = lines
            .iter()
            .map(|line| model_entry(line))
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
            BTreeSet::from(["amazon-bedrock", "myopenai"]),
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
        let entry = model_entry("anyapi/openai/gpt");
        assert_eq!(entry.provider, "anyapi");
        assert_eq!(entry.name, "openai/gpt");
        assert_eq!(
            entry.id, "anyapi/openai/gpt",
            "the value the rebuild re-resolves must be the line verbatim"
        );
    }
}
