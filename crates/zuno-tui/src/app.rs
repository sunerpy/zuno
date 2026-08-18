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
#[cfg(test)]
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CrosstermEvent,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::{Frame, Terminal};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Instant as TokioInstant, Interval, MissedTickBehavior};
use zuno_engine::r#loop::{TURN_EVENT_CHANNEL_CAPACITY, TurnEvent};
use zuno_engine::terminal_lease::{
    DEFAULT_LEASE_TIMEOUT, LeaseReason, ReclaimCause, TerminalBroker, TerminalOwner,
};

/// Maximum queued terminal events before their producer is backpressured.
pub const TERMINAL_EVENT_CHANNEL_CAPACITY: usize = 64;

/// Capacity expected of the engine event stream consumed by [`App`].
pub const ENGINE_EVENT_CHANNEL_CAPACITY: usize = TURN_EVENT_CHANNEL_CAPACITY;

// The plan's 60 FPS starting point caps streaming redraws at one frame every
// 16.67 ms. On this project, 32 queued events fell from 32 frames to 1, while 120
// paced events produced 15 frames; five 25-keystroke runs retained median
// p50=8.572 us / p95=19.848 us because input bypasses this ceiling.
const ACTIVE_REDRAW_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);
// The plan's 250 ms idle starting point drops timer wakeups from 60/s to 4/s once
// a turn ends. This project's deep-idle wake fixture measured 250.199 ms from a
// new idle event to its frame, matching the intended quarter-second backoff.
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(250);
// The plan's 30 s starting point avoids treating short pauses as deep idle. This
// project's deterministic schedule fixture crosses at exactly 30 s and verifies
// that terminal or engine activity immediately resets the tier.
const DEEP_IDLE_AFTER: Duration = Duration::from_secs(30);
// The plan's 5 s deep-idle starting point cuts an inactive loop from 4 timer wakeups/s
// to 0.2/s. This project has no decorative animation; its wake fixture entered the
// 5 s tier, then activity selected the measured 250.199 ms idle frame instead.
const DEEP_IDLE_REDRAW_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
struct RedrawConfig {
    active: Duration,
    idle: Duration,
    deep_idle_after: Duration,
    deep_idle: Duration,
}

const REDRAW_CONFIG: RedrawConfig = RedrawConfig {
    active: ACTIVE_REDRAW_INTERVAL,
    idle: IDLE_REDRAW_INTERVAL,
    deep_idle_after: DEEP_IDLE_AFTER,
    deep_idle: DEEP_IDLE_REDRAW_INTERVAL,
};

struct RedrawSchedule {
    config: RedrawConfig,
    interval: Interval,
    cadence: Duration,
    last_activity: TokioInstant,
    turn_active: bool,
}

impl RedrawSchedule {
    fn new(config: RedrawConfig, now: TokioInstant) -> Self {
        let cadence = config.idle;
        Self {
            config,
            interval: redraw_interval(now, cadence),
            cadence,
            last_activity: now,
            turn_active: false,
        }
    }

    async fn tick(&mut self) {
        self.interval.tick().await;
        self.refresh(TokioInstant::now());
    }

    fn record_terminal_activity(&mut self, now: TokioInstant) {
        self.last_activity = now;
        self.refresh(now);
    }

    fn record_engine_activity(&mut self, event: &TurnEvent, now: TokioInstant) {
        self.last_activity = now;
        self.turn_active = !matches!(
            event,
            TurnEvent::TurnCompleted { .. } | TurnEvent::TurnInterrupted { .. }
        );
        self.refresh(now);
    }

    fn refresh(&mut self, now: TokioInstant) {
        let cadence = if self.turn_active {
            self.config.active
        } else if now.duration_since(self.last_activity) >= self.config.deep_idle_after {
            self.config.deep_idle
        } else {
            self.config.idle
        };
        if cadence != self.cadence {
            self.cadence = cadence;
            self.interval = redraw_interval(now, cadence);
        }
    }

    fn frame_drawn(&mut self, now: TokioInstant) {
        self.interval = redraw_interval(now, self.cadence);
    }

    #[cfg(test)]
    const fn cadence(&self) -> Duration {
        self.cadence
    }

    #[cfg(test)]
    fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.interval.missed_tick_behavior()
    }
}

fn redraw_interval(now: TokioInstant, cadence: Duration) -> Interval {
    let mut interval = tokio::time::interval_at(now + cadence, cadence);
    // A slow full frame can miss several deadlines. Replaying those deadlines would
    // render the same newest state repeatedly and can trap the UI in a catch-up loop.
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

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
    /// The physical input producer reached a pause barrier.
    ///
    /// Internal to the event loop. Its position in the bounded FIFO proves that every
    /// input event read before this barrier has been consumed and discarded before a
    /// terminal lease is granted.
    InputPaused(u64),
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

/// Write the escape sequences that put a terminal into the TUI.
///
/// Split out from [`CrosstermLifecycle::enter`] so the sequences are assertable
/// without a TTY: `enable_raw_mode` is a terminal-attribute call with nothing to
/// observe, while these are bytes a test can collect into a vector.
///
/// Bracketed paste is the one whose absence is invisible until somebody pastes.
/// Without it a multi-line paste arrives as individual key events, every newline
/// resolves to `input_submit`, and an eight-line paste starts eight turns — which is
/// exactly what a real terminal did, filling the transcript with
/// `not sent: a turn is already running`.
fn enter_terminal(output: &mut impl io::Write, mouse_capture: bool) -> io::Result<()> {
    execute!(output, EnterAlternateScreen)?;
    // Each step unwinds only the steps before it, so a partial failure never leaves
    // the terminal in a mode the paired teardown will not reach.
    if let Err(error) = execute!(output, EnableBracketedPaste) {
        let _ = execute!(output, LeaveAlternateScreen);
        return Err(error);
    }
    if mouse_capture && let Err(error) = execute!(output, EnableMouseCapture) {
        let _ = execute!(output, DisableBracketedPaste);
        let _ = execute!(output, LeaveAlternateScreen);
        return Err(error);
    }
    Ok(())
}

/// Undo [`enter_terminal`], reporting the first failure but running every step.
///
/// A readable cooked terminal matters more than returning early, which is why none of
/// these are `?`. Every mode enabled above is disabled here: a terminal left in
/// bracketed-paste mode wraps every later paste *in the user's shell* with
/// `\e[200~`/`\e[201~`, which the shell then shows literally — a visible bug in a
/// program the user has already exited.
fn restore_terminal(output: &mut impl io::Write, mouse_capture: bool) -> Option<io::Error> {
    let mut first_error = execute!(output, LeaveAlternateScreen).err();
    if let Err(error) = execute!(output, DisableBracketedPaste) {
        first_error.get_or_insert(error);
    }
    if mouse_capture && let Err(error) = execute!(output, DisableMouseCapture) {
        first_error.get_or_insert(error);
    }
    first_error
}

impl TerminalLifecycle for CrosstermLifecycle {
    fn enter(&self) -> io::Result<()> {
        let _operation = locked(&self.operation);
        if self.active.load(Ordering::SeqCst) {
            return Ok(());
        }

        enable_raw_mode()?;
        if let Err(error) = enter_terminal(&mut io::stdout(), self.mouse_capture) {
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

        let mut first_error = restore_terminal(&mut io::stdout(), self.mouse_capture);
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
    input: Arc<TerminalInputControl>,
    yielded_pause: Mutex<Option<u64>>,
    wake: Arc<Notify>,
    diagnostics: Mutex<Vec<TerminalDiagnostic>>,
    #[cfg(test)]
    reclaim_resume_probe: Mutex<Option<Arc<ReclaimResumeProbe>>>,
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

    /// The pause controller the one physical input producer must use.
    #[must_use]
    pub fn input_control(&self) -> Arc<TerminalInputControl> {
        Arc::clone(&self.input)
    }

    #[cfg(test)]
    fn probe_reclaim_resume(&self, probe: Arc<ReclaimResumeProbe>) {
        *locked(&self.reclaim_resume_probe) = Some(probe);
    }

    #[cfg(test)]
    fn stop_before_resume_if_probed(&self) {
        let probe = locked(&self.reclaim_resume_probe).take();
        if let Some(probe) = probe {
            probe.stop_before_resume();
        }
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

        // Setting `suspended` stops dispatch first. The producer then finishes any
        // in-flight bounded poll/read and places a FIFO barrier after every event it
        // already sent. The loop acknowledges only after discarding through that
        // barrier, so neither an active reader nor a stale key can cross the handoff.
        let pause = self.input.request_pause();
        self.wake.notify_one();
        if let Err(error) = self.input.wait_for_pause(pause).await {
            self.suspended.store(false, Ordering::SeqCst);
            self.input.resume(pause);
            self.wake.notify_one();
            return Err(format!(
                "failed to pause terminal input before yielding for {reason}: {error}"
            ));
        }

        // Wait for any in-progress component handler or frame to finish only after the
        // reader is parked. Taking this lock before waiting for the producer would
        // deadlock: the loop needs the same lock to drain and acknowledge the barrier.
        let ui = locked(&self.ui);
        if let Err(error) = self.lifecycle.restore() {
            drop(ui);
            self.suspended.store(false, Ordering::SeqCst);
            self.input.resume(pause);
            self.wake.notify_one();
            return Err(format!(
                "failed to yield the terminal for {reason}: {error}"
            ));
        }
        let previous_pause = locked(&self.yielded_pause).replace(pause);
        assert!(
            previous_pause.is_none(),
            "a granted terminal lease must reclaim its pause before another yield completes"
        );
        drop(ui);
        Ok(())
    }

    fn reclaim_terminal(&self, reason: &LeaseReason, cause: ReclaimCause) {
        // Move the granted lease's epoch into this reclaim before publishing
        // `suspended = false`. A successor may request a newer epoch as soon as that
        // value is visible; keeping the old epoch local makes it impossible for the
        // successor to change which generation this reclaim resumes.
        let pause = locked(&self.yielded_pause)
            .take()
            .expect("every granted terminal lease owns one pause epoch");
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

        // Input resumes last. In particular, it cannot enter another `poll` until raw
        // mode, alternate-screen state, clear and repaint have all completed.
        self.suspended.store(false, Ordering::SeqCst);
        self.wake.notify_one();
        #[cfg(test)]
        self.stop_before_resume_if_probed();
        self.input.resume(pause);
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

/// Maximum time a lease acquisition waits for the live input producer to park.
///
/// A poll is bounded by [`INPUT_POLL_INTERVAL`], so four intervals allow a loaded
/// blocking worker to return without turning a broken acknowledgement into a hang.
pub const INPUT_PAUSE_TIMEOUT: Duration = Duration::from_millis(400);

#[cfg(test)]
struct ReclaimResumeProbe {
    observed: Barrier,
    proceed: Barrier,
}

#[cfg(test)]
impl Default for ReclaimResumeProbe {
    fn default() -> Self {
        Self {
            observed: Barrier::new(2),
            proceed: Barrier::new(2),
        }
    }
}

#[cfg(test)]
impl ReclaimResumeProbe {
    fn stop_before_resume(&self) {
        self.observed.wait();
        self.proceed.wait();
    }

    fn wait_until_observed(&self) {
        self.observed.wait();
    }

    fn allow_resume(&self) {
        self.proceed.wait();
    }
}

#[cfg(test)]
#[derive(Default)]
struct WaitRegistrationProbe {
    observed: Notify,
    proceed: Notify,
}

#[cfg(test)]
impl WaitRegistrationProbe {
    async fn wait_until_observed(&self) {
        self.observed.notified().await;
    }

    fn allow_wait(&self) {
        self.proceed.notify_one();
    }
}

#[derive(Clone, Copy, Default)]
struct TerminalInputState {
    requested: u64,
    acknowledged: u64,
    resumed: u64,
    producer_attached: bool,
    producer_seen: bool,
}

/// Coordinates the one physical TTY reader with terminal ownership transitions.
pub struct TerminalInputControl {
    // Epochs are retained state, not transient notifications. A `watch` receiver that
    // observes an old value before `changed().await` still sees an intervening update,
    // eliminating the check-then-register window that `Notify::notify_waiters` cannot fill.
    state: watch::Sender<TerminalInputState>,
    #[cfg(test)]
    wait_registration_probe: Mutex<Option<Arc<WaitRegistrationProbe>>>,
}

impl TerminalInputControl {
    fn new() -> Self {
        Self {
            state: watch::Sender::new(TerminalInputState::default()),
            #[cfg(test)]
            wait_registration_probe: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn probe_wait_registration(&self, probe: Arc<WaitRegistrationProbe>) {
        *locked(&self.wait_registration_probe) = Some(probe);
    }

    #[cfg(test)]
    async fn stop_before_wait_if_probed(&self) {
        let probe = locked(&self.wait_registration_probe).clone();
        if let Some(probe) = probe {
            probe.observed.notify_one();
            probe.proceed.notified().await;
        }
    }

    fn attach(self: &Arc<Self>) -> InputProducerGuard {
        self.state.send_modify(|state| {
            state.producer_seen = true;
            state.producer_attached = true;
        });
        InputProducerGuard {
            control: Arc::clone(self),
        }
    }

    fn request_pause(&self) -> u64 {
        let mut epoch = 0;
        self.state.send_modify(|state| {
            state.requested += 1;
            epoch = state.requested;
        });
        epoch
    }

    async fn wait_for_pause(&self, epoch: u64) -> Result<(), &'static str> {
        let mut state = self.state.subscribe();
        tokio::time::timeout(INPUT_PAUSE_TIMEOUT, async {
            loop {
                let snapshot = *state.borrow_and_update();
                if snapshot.acknowledged >= epoch {
                    return Ok(());
                }
                if !snapshot.producer_attached {
                    return if snapshot.producer_seen {
                        Err("the input producer stopped before acknowledging the pause")
                    } else {
                        Ok(())
                    };
                }
                #[cfg(test)]
                self.stop_before_wait_if_probed().await;
                if state.changed().await.is_err() {
                    return Err("the input state channel closed before acknowledging the pause");
                }
            }
        })
        .await
        .map_err(|_| "the input producer did not acknowledge the pause")?
    }

    fn acknowledge(&self, epoch: u64) {
        self.state
            .send_modify(|state| state.acknowledged = state.acknowledged.max(epoch));
    }

    fn resume(&self, epoch: u64) {
        self.state
            .send_modify(|state| state.resumed = state.resumed.max(epoch));
    }

    fn pending_pause(&self) -> Option<u64> {
        let state = *self.state.borrow();
        (state.requested > state.resumed).then_some(state.requested)
    }

    async fn wait_for_resume(&self, epoch: u64) {
        let mut state = self.state.subscribe();
        loop {
            let snapshot = *state.borrow_and_update();
            if snapshot.resumed >= epoch {
                return;
            }
            #[cfg(test)]
            self.stop_before_wait_if_probed().await;
            if state.changed().await.is_err() {
                return;
            }
        }
    }
}

struct InputProducerGuard {
    control: Arc<TerminalInputControl>,
}

impl Drop for InputProducerGuard {
    fn drop(&mut self) {
        self.control
            .state
            .send_modify(|state| state.producer_attached = false);
    }
}

trait TerminalInput: Send + Sync + 'static {
    fn poll(&self, timeout: Duration) -> io::Result<bool>;
    fn read(&self) -> io::Result<CrosstermEvent>;
}

struct CrosstermInput;

impl TerminalInput for CrosstermInput {
    fn poll(&self, timeout: Duration) -> io::Result<bool> {
        crossterm::event::poll(timeout)
    }

    fn read(&self) -> io::Result<CrosstermEvent> {
        crossterm::event::read()
    }
}

/// Read the physical terminal and forward every event until the consumer is gone.
///
/// This is the producer half of [`App`]'s terminal channel, and it lives here
/// rather than in a host because it is the only code that has to know crossterm's
/// reader is synchronous: the poll and the read run on blocking threads, while the
/// send is **awaited**, so a burst of input applies backpressure instead of being
/// dropped. Returning on a closed channel is what lets the task end promptly once
/// the application has exited.
pub async fn forward_terminal_input(
    sender: mpsc::Sender<TerminalEvent>,
    control: Arc<TerminalInputControl>,
) {
    forward_terminal_input_from(Arc::new(CrosstermInput), sender, control).await;
}

async fn forward_terminal_input_from(
    input: Arc<dyn TerminalInput>,
    sender: mpsc::Sender<TerminalEvent>,
    control: Arc<TerminalInputControl>,
) {
    let _attached = control.attach();
    while !sender.is_closed() {
        if pause_input_if_requested(&sender, &control).await.is_err() {
            return;
        }
        let polling = Arc::clone(&input);
        match tokio::task::spawn_blocking(move || polling.poll(INPUT_POLL_INTERVAL)).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                if pause_input_if_requested(&sender, &control).await.is_err() {
                    return;
                }
                continue;
            }
            Ok(Err(_)) | Err(_) => return,
        }
        if pause_input_if_requested(&sender, &control).await.is_err() {
            return;
        }
        let reading = Arc::clone(&input);
        let Ok(Ok(event)) = tokio::task::spawn_blocking(move || reading.read()).await else {
            return;
        };
        let event = match event {
            CrosstermEvent::Resize(width, height) => TerminalEvent::Resize { width, height },
            other => TerminalEvent::Input(other),
        };
        if sender.send(event).await.is_err() {
            return;
        }
        // A pause may have raced the blocking read. Queue that completed read first,
        // then put the pause marker behind it in the same FIFO. The suspended event
        // loop discards the event before acknowledging the marker, so no old-epoch
        // input can be replayed after reclaim.
        if pause_input_if_requested(&sender, &control).await.is_err() {
            return;
        }
    }
}

async fn pause_input_if_requested(
    sender: &mpsc::Sender<TerminalEvent>,
    control: &TerminalInputControl,
) -> Result<(), ()> {
    let Some(epoch) = control.pending_pause() else {
        return Ok(());
    };
    // This marker is sent by the same producer as physical input. FIFO ordering makes
    // it a drain barrier: once the loop sees it, every old key is before it and can be
    // dropped. The producer remains parked until reclaim has repainted the TUI.
    sender
        .send(TerminalEvent::InputPaused(epoch))
        .await
        .map_err(|_| ())?;
    control.wait_for_resume(epoch).await;
    Ok(())
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
    redraw_config: RedrawConfig,
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
        let input = Arc::new(TerminalInputControl::new());
        let owner = Arc::new(TerminalLeaseOwner {
            lifecycle,
            ui: Arc::clone(&ui),
            suspended: AtomicBool::new(false),
            input,
            yielded_pause: Mutex::new(None),
            wake,
            diagnostics: Mutex::new(Vec::new()),
            #[cfg(test)]
            reclaim_resume_probe: Mutex::new(None),
        });
        (
            Self {
                ui,
                owner: Arc::clone(&owner),
                terminal_events,
                engine_events,
                pending_terminal: VecDeque::new(),
                pending_engine: VecDeque::new(),
                redraw_config: REDRAW_CONFIG,
            },
            owner,
        )
    }

    #[cfg(test)]
    fn with_redraw_config(mut self, redraw_config: RedrawConfig) -> Self {
        self.redraw_config = redraw_config;
        self
    }

    /// Run until an explicit terminal shutdown event arrives.
    pub async fn run(&mut self) -> Result<(), AppError> {
        self.draw_if_owned()?;
        let mut redraw = RedrawSchedule::new(self.redraw_config, TokioInstant::now());
        // This is the loop's single dirty bit. Engine events only set it; terminal
        // input can satisfy and clear it with the immediate frame that preserves typing latency.
        let mut needs_redraw = false;
        loop {
            if !self.owner.suspended.load(Ordering::SeqCst) {
                if let Some(event) = self.pending_terminal.pop_front() {
                    match self.dispatch_terminal(event, &mut redraw, &mut needs_redraw)? {
                        Dispatch::Deferred(event) => self.pending_terminal.push_front(event),
                        Dispatch::Continue => {}
                        Dispatch::Shutdown => return Ok(()),
                    }
                    continue;
                }
                if let Some(event) = self.pending_engine.pop_front() {
                    if let Some(event) =
                        self.dispatch_engine(event, &mut redraw, &mut needs_redraw)?
                    {
                        self.pending_engine.push_front(event);
                    }
                    continue;
                }
            }

            let suspended = self.owner.suspended.load(Ordering::SeqCst);
            tokio::select! {
                biased;
                () = self.owner.wake.notified() => {}
                event = self.terminal_events.recv() => {
                    let event = event.ok_or(AppError::TerminalEventsClosed)?;
                    if self.owner.suspended.load(Ordering::SeqCst) {
                        match event {
                            TerminalEvent::InputPaused(epoch) => {
                                // Discard is deliberate: a key read before the handoff
                                // belongs to neither the editor nor the resumed TUI.
                                // Replaying it could submit a prompt or exit the app.
                                self.pending_terminal.clear();
                                self.owner.input.acknowledge(epoch);
                            }
                            TerminalEvent::Shutdown => return Ok(()),
                            TerminalEvent::Input(_) | TerminalEvent::Wake => {}
                            TerminalEvent::Resize { .. } => {
                                // Safe to discard at the lease boundary: ratatui checks the
                                // backend size and autoresizes on the next complete draw.
                            }
                        }
                    } else {
                        match self.dispatch_terminal(event, &mut redraw, &mut needs_redraw)? {
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
                    } else if let Some(event) =
                        self.dispatch_engine(event, &mut redraw, &mut needs_redraw)?
                    {
                        self.pending_engine.push_back(event);
                    }
                }
                () = redraw.tick() => {
                    if needs_redraw && self.draw_if_owned()? {
                        needs_redraw = false;
                        // Skip prevents multiple stale ticks; resetting from frame end
                        // also removes the one already-due tick left behind by a slow draw.
                        redraw.frame_drawn(TokioInstant::now());
                    }
                }
            }
        }
    }

    fn draw_if_owned(&self) -> Result<bool, AppError> {
        let mut ui = locked(&self.ui);
        if self.owner.suspended.load(Ordering::SeqCst) {
            return Ok(false);
        }
        ui.draw()?;
        Ok(true)
    }

    fn dispatch_terminal(
        &mut self,
        event: TerminalEvent,
        redraw: &mut RedrawSchedule,
        needs_redraw: &mut bool,
    ) -> Result<Dispatch, AppError> {
        let records_activity = matches!(
            event,
            TerminalEvent::Input(_) | TerminalEvent::Resize { .. } | TerminalEvent::Wake
        );
        let outcome = self.handle_terminal(event)?;
        if !matches!(outcome.dispatch, Dispatch::Deferred(_)) {
            if records_activity {
                redraw.record_terminal_activity(TokioInstant::now());
            }
            if outcome.redraw && self.draw_if_owned()? {
                // The immediate input frame includes every engine state mutation already
                // handled under the same UiState mutex, so it also satisfies prior dirt.
                *needs_redraw = false;
                redraw.frame_drawn(TokioInstant::now());
            }
        }
        Ok(outcome.dispatch)
    }

    fn dispatch_engine(
        &mut self,
        event: TurnEvent,
        redraw: &mut RedrawSchedule,
        needs_redraw: &mut bool,
    ) -> Result<Option<TurnEvent>, AppError> {
        redraw.record_engine_activity(&event, TokioInstant::now());
        let outcome = self.handle_engine(event)?;
        if outcome.deferred.is_none() {
            *needs_redraw |= outcome.redraw;
        }
        Ok(outcome.deferred)
    }

    fn handle_terminal(&mut self, event: TerminalEvent) -> Result<TerminalDispatch, AppError> {
        if let TerminalEvent::InputPaused(epoch) = event {
            self.pending_terminal.clear();
            self.owner.input.acknowledge(epoch);
            return Ok(TerminalDispatch {
                dispatch: Dispatch::Continue,
                redraw: false,
            });
        }
        let shutdown = matches!(event, TerminalEvent::Shutdown);
        let mut ui = locked(&self.ui);
        if self.owner.suspended.load(Ordering::SeqCst) {
            return Ok(TerminalDispatch {
                dispatch: Dispatch::Deferred(event),
                redraw: false,
            });
        }
        if let TerminalEvent::Resize { width, height } = &event {
            ui.target.resize(*width, *height)?;
        }
        let result = ui.root.handle_event(&AppEvent::Terminal(event));
        Ok(TerminalDispatch {
            dispatch: if shutdown {
                Dispatch::Shutdown
            } else {
                Dispatch::Continue
            },
            redraw: result.redraw || shutdown,
        })
    }

    fn handle_engine(&mut self, event: TurnEvent) -> Result<EngineDispatch, AppError> {
        let mut ui = locked(&self.ui);
        if self.owner.suspended.load(Ordering::SeqCst) {
            return Ok(EngineDispatch {
                deferred: Some(event),
                redraw: false,
            });
        }
        let result = ui.root.handle_event(&AppEvent::Engine(event));
        Ok(EngineDispatch {
            deferred: None,
            redraw: result.redraw,
        })
    }
}

enum Dispatch {
    Deferred(TerminalEvent),
    Continue,
    Shutdown,
}

struct TerminalDispatch {
    dispatch: Dispatch,
    redraw: bool,
}

struct EngineDispatch {
    deferred: Option<TurnEvent>,
    redraw: bool,
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
