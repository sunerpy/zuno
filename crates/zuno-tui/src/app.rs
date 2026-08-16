//! Terminal lifecycle, rendering, and the interface-neutral TUI event loop.
//!
//! The engine supplies [`TurnEvent`] values; this module owns only presentation and
//! the physical terminal. Both terminal input and engine transitions arrive through
//! bounded channels. The consumer never uses `try_recv`, so a full channel applies
//! backpressure instead of silently dropping input or state transitions.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{self, Stdout};
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::{Frame, Terminal};
use tokio::sync::{Notify, mpsc};
use zuno_engine::r#loop::{TURN_EVENT_CHANNEL_CAPACITY, TurnEvent};
use zuno_engine::terminal_lease::{
    DEFAULT_LEASE_TIMEOUT, LeaseReason, ReclaimCause, TerminalBroker, TerminalOwner,
};

/// Maximum queued terminal events before their producer is backpressured.
pub const TERMINAL_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Capacity expected of the engine event stream consumed by [`App`].
pub const ENGINE_EVENT_CHANNEL_CAPACITY: usize = TURN_EVENT_CHANNEL_CAPACITY;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A terminal-side event delivered to the component tree.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// One event read by crossterm.
    Input(CrosstermEvent),
    /// The terminal's drawable area changed.
    Resize { width: u16, height: u16 },
    /// State a component shares with an out-of-loop producer changed.
    ///
    /// A turn driver running off the loop has news that is neither a key nor a
    /// [`TurnEvent`]: it needs a human to answer a permission ask. It parks the
    /// request in state the component tree already shares with it and sends this to
    /// say "look again". Carrying no payload is deliberate — a wake that described
    /// the change would be a second, racing copy of the state it announces.
    Wake,
    /// Stop the render loop after components observe the shutdown.
    Shutdown,
}

/// One event a component can observe.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Input, resize, or shutdown from the terminal side.
    Terminal(TerminalEvent),
    /// A state transition from the engine turn loop.
    Engine(TurnEvent),
}

/// What a component asks the event loop to do after handling an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventResult {
    /// Whether this component consumed the event.
    pub handled: bool,
    /// Whether the component changed visible state.
    pub redraw: bool,
}

impl EventResult {
    /// The event was irrelevant to this component.
    pub const IGNORED: Self = Self {
        handled: false,
        redraw: false,
    };

    /// The event changed visible state and requires a frame.
    pub const REDRAW: Self = Self {
        handled: true,
        redraw: true,
    };

    /// Combine results returned by child components.
    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        Self {
            handled: self.handled || other.handled,
            redraw: self.redraw || other.redraw,
        }
    }
}

/// A composable TUI node.
///
/// Components know ratatui and application events, but never depend on engine
/// execution. This keeps rendering above the interface-neutral turn loop.
pub trait Component: Send {
    /// Paint this component into `area` of the current frame.
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect);

    /// Observe one application event.
    fn handle_event(&mut self, event: &AppEvent) -> EventResult;
}

/// A vertical composition of independently renderable components.
#[derive(Default)]
pub struct Column {
    children: Vec<(Constraint, Box<dyn Component>)>,
}

impl Column {
    /// An empty column.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    /// Append one constrained child.
    #[must_use]
    pub fn push(mut self, constraint: Constraint, child: Box<dyn Component>) -> Self {
        self.children.push((constraint, child));
        self
    }
}

impl Component for Column {
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let constraints = self
            .children
            .iter()
            .map(|(constraint, _)| *constraint)
            .collect::<Vec<_>>();
        let areas = Layout::vertical(constraints).split(area);
        for ((_, child), child_area) in self.children.iter_mut().zip(areas.iter().copied()) {
            child.render(frame, child_area);
        }
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        self.children
            .iter_mut()
            .fold(EventResult::IGNORED, |result, (_, child)| {
                result.merge(child.handle_event(event))
            })
    }
}

fn impossible(error: Infallible) -> io::Error {
    match error {}
}

/// Render a component tree without a TTY.
///
/// This is also the stable test seam for layout and frame assertions.
pub fn render_offscreen(root: &mut dyn Component, width: u16, height: u16) -> io::Result<Buffer> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).map_err(impossible)?;
    terminal
        .draw(|frame| root.render(frame, frame.area()))
        .map_err(impossible)?;
    Ok(terminal.backend().buffer().clone())
}

/// The idempotent physical terminal transitions used by sessions and leases.
pub trait TerminalLifecycle: Send + Sync + 'static {
    /// Enter raw mode and the alternate-screen TUI.
    fn enter(&self) -> io::Result<()>;

    /// Leave the alternate screen and restore cooked mode.
    fn restore(&self) -> io::Result<()>;

    /// Whether the lifecycle currently considers the TUI active.
    fn is_active(&self) -> bool;
}

/// Crossterm's real raw-mode and alternate-screen lifecycle.
pub struct CrosstermLifecycle {
    mouse_capture: bool,
    active: AtomicBool,
    operation: Mutex<()>,
}

impl CrosstermLifecycle {
    /// Create a lifecycle, optionally enabling mouse capture while the TUI is active.
    #[must_use]
    pub const fn new(mouse_capture: bool) -> Self {
        Self {
            mouse_capture,
            active: AtomicBool::new(false),
            operation: Mutex::new(()),
        }
    }
}

impl TerminalLifecycle for CrosstermLifecycle {
    fn enter(&self) -> io::Result<()> {
        let _operation = locked(&self.operation);
        if self.active.load(Ordering::SeqCst) {
            return Ok(());
        }

        enable_raw_mode()?;
        let mut output = io::stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        if self.mouse_capture
            && let Err(error) = execute!(output, EnableMouseCapture)
        {
            let _ = execute!(output, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error);
        }
        self.active.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn restore(&self) -> io::Result<()> {
        let _operation = locked(&self.operation);
        if !self.active.swap(false, Ordering::SeqCst) {
            return Ok(());
        }

        // Run every restoration step even if an earlier write fails. A readable
        // cooked terminal is more important than returning the first error early.
        let mut output = io::stdout();
        let mut first_error = execute!(output, LeaveAlternateScreen).err();
        if self.mouse_capture
            && let Err(error) = execute!(output, DisableMouseCapture)
        {
            first_error.get_or_insert(error);
        }
        if let Err(error) = disable_raw_mode() {
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

/// Reports a panic after the terminal has been restored.
pub trait PanicReporter: Send + Sync + 'static {
    /// Emit a human-readable panic report. Implementations must not use stdout.
    fn report(&self, info: &PanicHookInfo<'_>);
}

type PanicHook = dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static;

struct PreviousHookReporter;

impl PanicReporter for PreviousHookReporter {
    fn report(&self, info: &PanicHookInfo<'_>) {
        if let Some(hook) = PREVIOUS_PANIC_HOOK.get() {
            hook(info);
        }
    }
}

// Panic hooks are process-global. Holding this guard for the entire session prevents
// two tests or nested launch paths from replacing and restoring one another's hooks.
static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());
static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();
static PREVIOUS_PANIC_HOOK: OnceLock<Arc<PanicHook>> = OnceLock::new();
static ACTIVE_PANIC_CONTEXT: Mutex<Option<PanicContext>> = Mutex::new(None);

#[derive(Clone)]
struct PanicContext {
    lifecycle: Arc<dyn TerminalLifecycle>,
    reporter: Arc<dyn PanicReporter>,
}

fn install_panic_dispatcher() {
    PANIC_HOOK_INSTALLED.get_or_init(|| {
        let previous: Arc<PanicHook> = Arc::from(panic::take_hook());
        let _ = PREVIOUS_PANIC_HOOK.set(previous);
        panic::set_hook(Box::new(|info| {
            if let Some(context) = locked(&ACTIVE_PANIC_CONTEXT).clone() {
                if let Err(error) = context.lifecycle.restore() {
                    eprintln!("failed to restore the terminal after a panic: {error}");
                }
                context.reporter.report(info);
            } else if let Some(previous) = PREVIOUS_PANIC_HOOK.get() {
                previous(info);
            }
        }));
    });
}

/// Owns terminal activation and process-global panic restoration for one TUI run.
pub struct TerminalSession {
    lifecycle: Arc<dyn TerminalLifecycle>,
    _hook_guard: MutexGuard<'static, ()>,
}

impl TerminalSession {
    /// Enter the terminal and delegate panic reporting to the previously installed
    /// process hook after restoring cooked mode.
    pub fn start(lifecycle: Arc<dyn TerminalLifecycle>) -> io::Result<Self> {
        Self::start_inner(lifecycle, None)
    }

    /// Enter the terminal with an injectable reporter for deterministic tests.
    pub fn start_with_reporter(
        lifecycle: Arc<dyn TerminalLifecycle>,
        reporter: Arc<dyn PanicReporter>,
    ) -> io::Result<Self> {
        Self::start_inner(lifecycle, Some(reporter))
    }

    fn start_inner(
        lifecycle: Arc<dyn TerminalLifecycle>,
        reporter: Option<Arc<dyn PanicReporter>>,
    ) -> io::Result<Self> {
        let hook_guard = locked(&PANIC_HOOK_LOCK);
        install_panic_dispatcher();
        let reporter = reporter.unwrap_or_else(|| Arc::new(PreviousHookReporter));
        *locked(&ACTIVE_PANIC_CONTEXT) = Some(PanicContext {
            lifecycle: Arc::clone(&lifecycle),
            reporter,
        });

        let session = Self {
            lifecycle,
            _hook_guard: hook_guard,
        };
        if let Err(error) = session.lifecycle.enter() {
            drop(session);
            return Err(error);
        }
        Ok(session)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if let Err(error) = self.lifecycle.restore() {
            eprintln!("failed to restore the terminal while leaving the TUI: {error}");
        }
        *locked(&ACTIVE_PANIC_CONTEXT) = None;
    }
}

/// A drawing backend shared by the event loop and lease reclaim path.
pub trait DrawTarget: Send {
    /// Draw one complete frame.
    fn draw(&mut self, root: &mut dyn Component) -> io::Result<()>;

    /// Clear stale terminal cells before a full repaint.
    fn clear(&mut self) -> io::Result<()>;

    /// Update the drawable terminal area.
    fn resize(&mut self, width: u16, height: u16) -> io::Result<()>;
}

/// The production ratatui backend over crossterm stdout.
pub struct CrosstermDrawTarget {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl CrosstermDrawTarget {
    /// Bind ratatui to stdout. Terminal mode is controlled separately by
    /// [`TerminalSession`].
    pub fn new() -> io::Result<Self> {
        let backend = CrosstermBackend::new(io::stdout());
        Ok(Self {
            terminal: Terminal::new(backend)?,
        })
    }
}

impl DrawTarget for CrosstermDrawTarget {
    fn draw(&mut self, root: &mut dyn Component) -> io::Result<()> {
        self.terminal
            .draw(|frame| root.render(frame, frame.area()))?;
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.terminal.clear()
    }

    fn resize(&mut self, width: u16, height: u16) -> io::Result<()> {
        self.terminal.resize(Rect::new(0, 0, width, height))
    }
}

struct UiState {
    root: Box<dyn Component>,
    target: Box<dyn DrawTarget>,
}

impl UiState {
    fn draw(&mut self) -> io::Result<()> {
        self.target.draw(self.root.as_mut())
    }
}

/// One surfaced terminal ownership or restoration diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalDiagnostic {
    /// The lease that caused this diagnostic, when applicable.
    pub reason: Option<LeaseReason>,
    /// The stderr-safe human-readable message.
    pub message: String,
    /// Whether a deadline forced terminal ownership back to the TUI.
    pub forced: bool,
}

/// The real [`TerminalOwner`] backed by the TUI lifecycle and render state.
pub struct TerminalLeaseOwner {
    lifecycle: Arc<dyn TerminalLifecycle>,
    ui: Arc<Mutex<UiState>>,
    suspended: AtomicBool,
    wake: Arc<Notify>,
    diagnostics: Mutex<Vec<TerminalDiagnostic>>,
}

impl TerminalLeaseOwner {
    /// Build a broker with the production human-interaction deadline.
    #[must_use]
    pub fn broker(self: &Arc<Self>) -> TerminalBroker {
        self.broker_with_timeout(DEFAULT_LEASE_TIMEOUT)
    }

    /// Build a broker with an explicit deadline.
    #[must_use]
    pub fn broker_with_timeout(self: &Arc<Self>, timeout: Duration) -> TerminalBroker {
        let owner: Arc<dyn TerminalOwner> = self.clone();
        TerminalBroker::with_timeout(owner, timeout)
    }

    /// Diagnostics emitted by reclaim and restoration paths so far.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<TerminalDiagnostic> {
        locked(&self.diagnostics).clone()
    }

    fn surface(&self, diagnostic: TerminalDiagnostic) {
        eprintln!("{}", diagnostic.message);
        locked(&self.diagnostics).push(diagnostic);
    }
}

#[async_trait]
impl TerminalOwner for TerminalLeaseOwner {
    async fn yield_terminal(&self, reason: &LeaseReason) -> Result<(), String> {
        if !self.lifecycle.is_active() {
            return Err("the TUI terminal is not active".to_owned());
        }
        if self.suspended.swap(true, Ordering::SeqCst) {
            return Err("the TUI terminal is already yielded".to_owned());
        }

        // Wait for any in-progress component handler or frame to finish. Once the
        // atomic is set, the loop will retain a concurrently received input event
        // rather than dispatching it while the child owns stdin.
        let ui = locked(&self.ui);
        if let Err(error) = self.lifecycle.restore() {
            drop(ui);
            self.suspended.store(false, Ordering::SeqCst);
            self.wake.notify_one();
            return Err(format!(
                "failed to yield the terminal for {reason}: {error}"
            ));
        }
        drop(ui);
        Ok(())
    }

    fn reclaim_terminal(&self, reason: &LeaseReason, cause: ReclaimCause) {
        if let ReclaimCause::Deadline(forced) = &cause {
            self.surface(TerminalDiagnostic {
                reason: Some(reason.clone()),
                message: forced.to_string(),
                forced: true,
            });
        }

        let mut failed = None;
        if let Err(error) = self.lifecycle.enter() {
            failed = Some(format!(
                "failed to reclaim the terminal for {reason}: {error}"
            ));
        } else {
            let mut ui = locked(&self.ui);
            if let Err(error) = ui.target.clear().and_then(|()| ui.draw()) {
                failed = Some(format!(
                    "reclaimed the terminal for {reason}, but repaint failed: {error}"
                ));
            }
        }

        self.suspended.store(false, Ordering::SeqCst);
        self.wake.notify_one();
        if let Some(message) = failed {
            self.surface(TerminalDiagnostic {
                reason: Some(reason.clone()),
                message,
                forced: cause.is_forced(),
            });
        }
    }
}

/// Create the lossless bounded transport accepted by [`App`].
#[must_use]
pub fn terminal_event_channel() -> (mpsc::Sender<TerminalEvent>, mpsc::Receiver<TerminalEvent>) {
    mpsc::channel(TERMINAL_EVENT_CHANNEL_CAPACITY)
}

/// How long the input producer waits for a key before re-checking for shutdown.
///
/// Short enough that leaving the TUI does not visibly stall on the last poll, long
/// enough that an idle terminal is not a busy loop.
pub const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Read the physical terminal and forward every event until the consumer is gone.
///
/// This is the producer half of [`App`]'s terminal channel, and it lives here
/// rather than in a host because it is the only code that has to know crossterm's
/// reader is synchronous: the poll and the read run on blocking threads, while the
/// send is **awaited**, so a burst of input applies backpressure instead of being
/// dropped. Returning on a closed channel is what lets the task end promptly once
/// the application has exited.
pub async fn forward_terminal_input(sender: mpsc::Sender<TerminalEvent>) {
    while !sender.is_closed() {
        match tokio::task::spawn_blocking(|| crossterm::event::poll(INPUT_POLL_INTERVAL)).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => continue,
            Ok(Err(_)) | Err(_) => return,
        }
        let Ok(Ok(event)) = tokio::task::spawn_blocking(crossterm::event::read).await else {
            return;
        };
        let event = match event {
            CrosstermEvent::Resize(width, height) => TerminalEvent::Resize { width, height },
            other => TerminalEvent::Input(other),
        };
        if sender.send(event).await.is_err() {
            return;
        }
    }
}

/// A TUI event-loop failure.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The terminal input producer disappeared without an explicit shutdown event.
    #[error("the terminal event producer closed without sending shutdown")]
    TerminalEventsClosed,
    /// The engine event producer disappeared while the application was running.
    #[error("the engine event producer closed while the TUI was running")]
    EngineEventsClosed,
    /// Drawing or resizing the terminal failed.
    #[error(transparent)]
    Terminal(#[from] io::Error),
}

/// The bounded terminal/engine event consumer and component renderer.
pub struct App {
    ui: Arc<Mutex<UiState>>,
    owner: Arc<TerminalLeaseOwner>,
    terminal_events: mpsc::Receiver<TerminalEvent>,
    engine_events: mpsc::Receiver<TurnEvent>,
    pending_terminal: VecDeque<TerminalEvent>,
    pending_engine: VecDeque<TurnEvent>,
}

impl App {
    /// Assemble an application and the terminal owner hosts use for leases.
    #[must_use]
    pub fn new(
        root: Box<dyn Component>,
        target: Box<dyn DrawTarget>,
        lifecycle: Arc<dyn TerminalLifecycle>,
        terminal_events: mpsc::Receiver<TerminalEvent>,
        engine_events: mpsc::Receiver<TurnEvent>,
    ) -> (Self, Arc<TerminalLeaseOwner>) {
        let ui = Arc::new(Mutex::new(UiState { root, target }));
        let wake = Arc::new(Notify::new());
        let owner = Arc::new(TerminalLeaseOwner {
            lifecycle,
            ui: Arc::clone(&ui),
            suspended: AtomicBool::new(false),
            wake,
            diagnostics: Mutex::new(Vec::new()),
        });
        (
            Self {
                ui,
                owner: Arc::clone(&owner),
                terminal_events,
                engine_events,
                pending_terminal: VecDeque::new(),
                pending_engine: VecDeque::new(),
            },
            owner,
        )
    }

    /// Run until an explicit terminal shutdown event arrives.
    pub async fn run(&mut self) -> Result<(), AppError> {
        {
            let mut ui = locked(&self.ui);
            if !self.owner.suspended.load(Ordering::SeqCst) {
                ui.draw()?;
            }
        }
        loop {
            if !self.owner.suspended.load(Ordering::SeqCst) {
                if let Some(event) = self.pending_terminal.pop_front() {
                    match self.handle_terminal(event)? {
                        Dispatch::Deferred(event) => self.pending_terminal.push_front(event),
                        Dispatch::Continue => {}
                        Dispatch::Shutdown => return Ok(()),
                    }
                    continue;
                }
                if let Some(event) = self.pending_engine.pop_front() {
                    if let Some(event) = self.handle_engine(event)? {
                        self.pending_engine.push_front(event);
                    }
                    continue;
                }
            }

            let suspended = self.owner.suspended.load(Ordering::SeqCst);
            tokio::select! {
                biased;
                () = self.owner.wake.notified() => {}
                event = self.terminal_events.recv(), if !suspended => {
                    let event = event.ok_or(AppError::TerminalEventsClosed)?;
                    if self.owner.suspended.load(Ordering::SeqCst) {
                        self.pending_terminal.push_back(event);
                    } else {
                        match self.handle_terminal(event)? {
                            Dispatch::Deferred(event) => self.pending_terminal.push_back(event),
                            Dispatch::Continue => {}
                            Dispatch::Shutdown => return Ok(()),
                        }
                    }
                }
                event = self.engine_events.recv(), if !suspended => {
                    let event = event.ok_or(AppError::EngineEventsClosed)?;
                    if self.owner.suspended.load(Ordering::SeqCst) {
                        self.pending_engine.push_back(event);
                    } else if let Some(event) = self.handle_engine(event)? {
                        self.pending_engine.push_back(event);
                    }
                }
            }
        }
    }

    fn handle_terminal(&mut self, event: TerminalEvent) -> Result<Dispatch, AppError> {
        let shutdown = matches!(event, TerminalEvent::Shutdown);
        let mut ui = locked(&self.ui);
        if self.owner.suspended.load(Ordering::SeqCst) {
            return Ok(Dispatch::Deferred(event));
        }
        if let TerminalEvent::Resize { width, height } = &event {
            ui.target.resize(*width, *height)?;
        }
        let result = ui.root.handle_event(&AppEvent::Terminal(event));
        if result.redraw || shutdown {
            ui.draw()?;
        }
        Ok(if shutdown {
            Dispatch::Shutdown
        } else {
            Dispatch::Continue
        })
    }

    fn handle_engine(&mut self, event: TurnEvent) -> Result<Option<TurnEvent>, AppError> {
        let mut ui = locked(&self.ui);
        if self.owner.suspended.load(Ordering::SeqCst) {
            return Ok(Some(event));
        }
        let result = ui.root.handle_event(&AppEvent::Engine(event));
        if result.redraw {
            ui.draw()?;
        }
        Ok(None)
    }
}

enum Dispatch {
    Deferred(TerminalEvent),
    Continue,
    Shutdown,
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
