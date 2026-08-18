use std::convert::Infallible;
use std::io;
use std::panic::{AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crossterm::event::Event as CrosstermEvent;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::Paragraph;
use tokio::sync::mpsc;
use zuno_engine::r#loop::{TURN_EVENT_CHANNEL_CAPACITY, TurnEvent};
use zuno_engine::terminal_lease::{LeaseReason, TerminalLease, TerminalLeaseError};

use super::*;

fn locked<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct FakeLifecycle {
    active: AtomicBool,
    enters: AtomicUsize,
    restores: AtomicUsize,
    transitions: Mutex<Vec<&'static str>>,
}

impl FakeLifecycle {
    fn transitions(&self) -> Vec<&'static str> {
        locked(&self.transitions).clone()
    }

    fn restore_count(&self) -> usize {
        self.restores.load(Ordering::SeqCst)
    }
}

impl TerminalLifecycle for FakeLifecycle {
    fn enter(&self) -> io::Result<()> {
        if !self.active.swap(true, Ordering::SeqCst) {
            self.enters.fetch_add(1, Ordering::SeqCst);
            locked(&self.transitions).push("enter");
        }
        Ok(())
    }

    fn restore(&self) -> io::Result<()> {
        if self.active.swap(false, Ordering::SeqCst) {
            self.restores.fetch_add(1, Ordering::SeqCst);
            locked(&self.transitions).push("restore");
        }
        Ok(())
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

struct Label(&'static str);

impl Component for Label {
    fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        frame.render_widget(Paragraph::new(self.0), area);
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

#[test]
fn app_component_tree_renders_into_an_offscreen_buffer() {
    let branch = Column::new()
        .push(Constraint::Length(1), Box::new(Label("branch")))
        .push(Constraint::Min(1), Box::new(Label("leaf")));
    let mut root = Column::new()
        .push(Constraint::Length(1), Box::new(Label("root")))
        .push(Constraint::Min(2), Box::new(branch));

    let buffer = render_offscreen(&mut root, 12, 3).expect("off-screen render succeeds");

    assert_eq!(
        buffer,
        Buffer::with_lines(["root        ", "branch      ", "leaf        "])
    );
}

struct PanicOnEvent;

impl Component for PanicOnEvent {
    fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        frame.render_widget(Paragraph::new("before panic"), area);
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        panic!("component event panic")
    }
}

#[derive(Clone, Debug)]
struct PanicReport {
    terminal_was_active: bool,
    text: String,
}

struct RecordingReporter {
    lifecycle: Arc<FakeLifecycle>,
    reports: Arc<Mutex<Vec<PanicReport>>>,
}

impl PanicReporter for RecordingReporter {
    fn report(&self, info: &PanicHookInfo<'_>) {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic");
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_else(|| "unknown location".to_owned());
        locked(&self.reports).push(PanicReport {
            terminal_was_active: self.lifecycle.is_active(),
            text: format!("panic at {location}: {payload}"),
        });
    }
}

#[test]
fn app_panic_inside_the_event_loop_restores_before_reporting() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    let reports = Arc::new(Mutex::new(Vec::new()));
    let reporter = Arc::new(RecordingReporter {
        lifecycle: Arc::clone(&lifecycle),
        reports: Arc::clone(&reports),
    });
    let session =
        TerminalSession::start_with_reporter(Arc::clone(&lifecycle) as Arc<_>, reporter as Arc<_>)
            .expect("fake terminal enters");
    let (target, _screen) = SharedTestTarget::new(20, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(1);
    let (mut app, _owner) = App::new(
        Box::new(PanicOnEvent),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    terminal_tx
        .blocking_send(TerminalEvent::Input(CrosstermEvent::FocusGained))
        .expect("the event loop still owns its input channel");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds");

    let panic = catch_unwind(AssertUnwindSafe(|| runtime.block_on(app.run())));

    assert!(panic.is_err(), "the component panic must escape the loop");
    assert!(
        !lifecycle.is_active(),
        "the panic hook must restore while the session guard is still alive"
    );
    assert_eq!(
        lifecycle.restore_count(),
        1,
        "the hook restores once; the later guard drop is idempotent"
    );
    let reports = locked(&reports);
    assert_eq!(reports.len(), 1);
    assert!(
        !reports[0].terminal_was_active,
        "the readable report is emitted only after cooked mode is restored"
    );
    assert!(reports[0].text.contains("component event panic"));
    assert!(reports[0].text.contains("app_tests.rs"));
    drop(reports);
    drop(session);
}

#[test]
fn app_terminal_session_can_drop_while_a_panic_is_unwinding() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    let reports = Arc::new(Mutex::new(Vec::new()));
    let reporter = Arc::new(RecordingReporter {
        lifecycle: Arc::clone(&lifecycle),
        reports: Arc::clone(&reports),
    });

    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _session = TerminalSession::start_with_reporter(
            Arc::clone(&lifecycle) as Arc<_>,
            reporter as Arc<_>,
        )
        .expect("fake terminal enters");
        panic!("session-owned panic");
    }));

    assert!(panic.is_err());
    assert!(!lifecycle.is_active());
    assert_eq!(lifecycle.restore_count(), 1);
    assert_eq!(locked(&reports).len(), 1);
    let replacement = TerminalSession::start(Arc::clone(&lifecycle) as Arc<_>)
        .expect("the global session lock and context were released during unwind");
    drop(replacement);
}

// ---------------------------------------------------------------------------
// The escape sequences that enter and leave the TUI
// ---------------------------------------------------------------------------

/// `\e[?2004h` / `\e[?2004l`, the DEC private mode a terminal reads as bracketed
/// paste. Spelled out rather than derived from the crossterm command so that
/// exchanging one command for the other cannot make this test agree with itself.
const ENABLE_BRACKETED_PASTE: &str = "\u{1b}[?2004h";
const DISABLE_BRACKETED_PASTE: &str = "\u{1b}[?2004l";

/// Windows only: `execute!` falls back to a console API that has no bracketed-paste
/// call, so the bytes below exist only on an ANSI terminal. The paired-teardown
/// property is asserted on every platform by the source scan that follows.
#[cfg(not(windows))]
#[test]
fn app_entering_the_terminal_enables_bracketed_paste_and_leaving_disables_it() {
    let mut entered = Vec::new();
    enter_terminal(&mut entered, false).expect("a vector accepts every write");
    let entered = String::from_utf8(entered).expect("crossterm writes utf-8");

    let mut left = Vec::new();
    assert!(
        restore_terminal(&mut left, false).is_none(),
        "restoring into a vector reported a failure"
    );
    let left = String::from_utf8(left).expect("crossterm writes utf-8");

    assert!(
        entered.contains(ENABLE_BRACKETED_PASTE),
        "entering the TUI did not enable bracketed paste, so a multi-line paste \
         arrives as individual keys and every newline submits a turn: {entered:?}"
    );
    assert!(
        left.contains(DISABLE_BRACKETED_PASTE),
        "leaving the TUI did not disable bracketed paste, so the user's shell is left \
         wrapping every later paste in \\e[200~: {left:?}"
    );
}

#[test]
fn app_every_mode_entering_the_terminal_enables_is_disabled_on_the_way_out() {
    // A source scan rather than a byte comparison, and that is deliberate: it holds on
    // Windows too, where `execute!` may take a console-API path that writes no ANSI at
    // all. A terminal left in a mode the program enabled is a defect the user only sees
    // *after* quitting, so it needs a guard that cannot be skipped by platform.
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"),
    )
    .expect("read this module's own source");
    let body = |name: &str| -> String {
        let start = source
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("{name} is gone; this scan is checking nothing"));
        let rest = &source[start..];
        let end = rest.find("\n}\n").expect("a top-level function body ends");
        rest[..end].to_owned()
    };
    let entering = body("enter_terminal");
    let leaving = body("restore_terminal");

    let enabled: Vec<&str> = ["EnableBracketedPaste", "EnableMouseCapture"]
        .into_iter()
        .filter(|command| entering.contains(command))
        .collect();
    assert_eq!(
        enabled.len(),
        2,
        "the scan did not find the commands it exists to pair, so it would pass \
         vacuously: {enabled:?}"
    );
    for command in enabled {
        let paired = command.replace("Enable", "Disable");
        assert!(
            leaving.contains(&paired),
            "`{command}` is enabled on the way in and `{paired}` never runs on the way \
             out, which leaves the terminal altered after the process ends"
        );
    }
}

fn return_early(lifecycle: Arc<dyn TerminalLifecycle>) -> io::Result<()> {
    let _session = TerminalSession::start(lifecycle)?;
    Err(io::Error::other("early return"))
}

#[test]
fn app_terminal_guard_restores_on_an_early_error_return() {
    let lifecycle = Arc::new(FakeLifecycle::default());

    let error = return_early(Arc::clone(&lifecycle) as Arc<_>).expect_err("the body returns early");

    assert_eq!(error.to_string(), "early return");
    assert!(!lifecycle.is_active());
    assert_eq!(lifecycle.restore_count(), 1);
}

#[derive(Debug)]
struct Screen {
    width: u16,
    height: u16,
    buffer: Buffer,
    draws: usize,
    clears: usize,
    draw_started: Vec<Instant>,
}

struct SharedTestTarget {
    screen: Arc<Mutex<Screen>>,
    draw_delay: Duration,
}

impl SharedTestTarget {
    fn new(width: u16, height: u16) -> (Self, Arc<Mutex<Screen>>) {
        Self::with_draw_delay(width, height, Duration::ZERO)
    }

    fn with_draw_delay(
        width: u16,
        height: u16,
        draw_delay: Duration,
    ) -> (Self, Arc<Mutex<Screen>>) {
        let screen = Arc::new(Mutex::new(Screen {
            width,
            height,
            buffer: Buffer::empty(Rect::new(0, 0, width, height)),
            draws: 0,
            clears: 0,
            draw_started: Vec::new(),
        }));
        (
            Self {
                screen: Arc::clone(&screen),
                draw_delay,
            },
            screen,
        )
    }
}

fn impossible(error: Infallible) -> io::Error {
    match error {}
}

impl DrawTarget for SharedTestTarget {
    fn draw(&mut self, root: &mut dyn Component) -> io::Result<()> {
        locked(&self.screen).draw_started.push(Instant::now());
        std::thread::sleep(self.draw_delay);
        let (width, height) = {
            let screen = locked(&self.screen);
            (screen.width, screen.height)
        };
        let mut terminal = Terminal::new(TestBackend::new(width, height)).map_err(impossible)?;
        terminal
            .draw(|frame| root.render(frame, frame.area()))
            .map_err(impossible)?;
        let mut screen = locked(&self.screen);
        screen.buffer = terminal.backend().buffer().clone();
        screen.draws += 1;
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        let mut screen = locked(&self.screen);
        screen.buffer = Buffer::empty(Rect::new(0, 0, screen.width, screen.height));
        screen.clears += 1;
        Ok(())
    }

    fn resize(&mut self, width: u16, height: u16) -> io::Result<()> {
        let mut screen = locked(&self.screen);
        screen.width = width;
        screen.height = height;
        screen.buffer = Buffer::empty(Rect::new(0, 0, width, height));
        Ok(())
    }
}

#[test]
fn app_tty_yield_round_trip_reenters_and_repaints_the_tui() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    let session =
        TerminalSession::start(Arc::clone(&lifecycle) as Arc<_>).expect("fake terminal enters");
    let (target, screen) = SharedTestTarget::new(12, 1);
    let (_terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(1);
    let (_app, owner) = App::new(
        Box::new(Label("ready")),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let broker = owner.broker_with_timeout(Duration::from_secs(3_600));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds");

    let guard = runtime
        .block_on(broker.acquire(LeaseReason::new("kiro", "device-code prompt")))
        .expect("the TUI yields to the child");
    assert!(!lifecycle.is_active(), "the child receives cooked mode");
    assert_eq!(lifecycle.transitions(), vec!["enter", "restore"]);

    guard.release();

    assert!(
        lifecycle.is_active(),
        "guard drop returns ownership to the TUI"
    );
    assert_eq!(lifecycle.transitions(), vec!["enter", "restore", "enter"]);
    let screen = locked(&screen);
    assert_eq!(screen.clears, 1, "reclaim starts from a clean frame");
    assert_eq!(
        screen.draws, 1,
        "reclaim completes the repaint synchronously"
    );
    assert_eq!(screen.buffer, Buffer::with_lines(["ready       "]));
    drop(screen);
    drop(session);
}

#[test]
fn app_real_owner_keeps_the_brokers_refusal_policy() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    let session =
        TerminalSession::start(Arc::clone(&lifecycle) as Arc<_>).expect("fake terminal enters");
    let (target, _screen) = SharedTestTarget::new(12, 1);
    let (_terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(1);
    let (_app, owner) = App::new(
        Box::new(Label("ready")),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let broker = owner.broker_with_timeout(Duration::from_secs(3_600));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds");
    let first = runtime
        .block_on(broker.acquire(LeaseReason::new("kiro", "device-code prompt")))
        .expect("first holder wins");

    let error = runtime
        .block_on(broker.acquire(LeaseReason::new("other", "api key prompt")))
        .expect_err("a live holder is refused, never queued");

    assert_eq!(
        error,
        TerminalLeaseError::Busy {
            holder: "kiro".to_owned(),
            holder_purpose: "device-code prompt".to_owned(),
            requested_by: "other".to_owned(),
        }
    );
    drop(first);
    drop(session);
}

#[test]
fn app_force_reclaim_surfaces_the_brokers_diagnostic_and_repaints() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    let session =
        TerminalSession::start(Arc::clone(&lifecycle) as Arc<_>).expect("fake terminal enters");
    let (target, screen) = SharedTestTarget::new(12, 1);
    let (_terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(1);
    let (_app, owner) = App::new(
        Box::new(Label("ready")),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let broker = owner.broker_with_timeout(Duration::ZERO);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime builds");
    let leaked = runtime
        .block_on(broker.acquire(LeaseReason::new("kiro", "device-code prompt")))
        .expect("lease starts");

    let forced = broker
        .reclaim_if_expired()
        .expect("the elapsed lease is reclaimed");

    assert_eq!(forced.plugin, "kiro");
    assert!(lifecycle.is_active());
    assert_eq!(locked(&screen).draws, 1);
    let diagnostics = owner.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("plugin `kiro`"));
    assert!(diagnostics[0].forced);
    drop(leaked);
    assert_eq!(locked(&screen).draws, 1, "late guard drop must be inert");
    drop(session);
}

struct EventRecorder {
    terminal_events: Arc<AtomicUsize>,
    engine_events: Arc<AtomicUsize>,
}

impl Component for EventRecorder {
    fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        frame.render_widget(
            Paragraph::new(format!("{}x{}", area.width, area.height)),
            area,
        );
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        match event {
            AppEvent::Terminal(_) => {
                self.terminal_events.fetch_add(1, Ordering::SeqCst);
            }
            AppEvent::Engine(_) => {
                self.engine_events.fetch_add(1, Ordering::SeqCst);
            }
        }
        EventResult::REDRAW
    }
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the event loop makes progress under the test budget");
}

async fn wait_until_within(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(timeout, async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the event loop missed the latency budget");
}

const TEST_REDRAW_CONFIG: RedrawConfig = RedrawConfig {
    active: Duration::from_millis(10),
    idle: Duration::from_millis(30),
    deep_idle_after: Duration::from_millis(60),
    deep_idle: Duration::from_millis(100),
};

struct RecordingInput {
    polls: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
    events: Mutex<VecDeque<CrosstermEvent>>,
}

impl RecordingInput {
    fn new(events: impl IntoIterator<Item = CrosstermEvent>) -> Arc<Self> {
        Arc::new(Self {
            polls: Arc::new(AtomicUsize::new(0)),
            reads: Arc::new(AtomicUsize::new(0)),
            events: Mutex::new(events.into_iter().collect()),
        })
    }
}

impl TerminalInput for RecordingInput {
    fn poll(&self, timeout: Duration) -> io::Result<bool> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if locked(&self.events).is_empty() {
            std::thread::sleep(timeout.min(Duration::from_millis(2)));
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn read(&self) -> io::Result<CrosstermEvent> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        locked(&self.events)
            .pop_front()
            .ok_or_else(|| io::Error::other("the scripted terminal input is empty"))
    }
}

#[tokio::test]
async fn app_pause_wait_retains_acknowledgement_published_before_wait_registration() {
    let control = Arc::new(TerminalInputControl::new());
    let _producer = control.attach();
    let epoch = control.request_pause();
    let probe = Arc::new(WaitRegistrationProbe::default());
    control.probe_wait_registration(Arc::clone(&probe));
    let waiter = tokio::spawn({
        let control = Arc::clone(&control);
        async move { control.wait_for_pause(epoch).await }
    });
    probe.wait_until_observed().await;

    control.acknowledge(epoch);
    probe.allow_wait();

    tokio::time::timeout(Duration::from_millis(100), waiter)
        .await
        .expect("the retained acknowledgement must wake the pause waiter")
        .expect("the pause waiter must not panic")
        .expect("the pause waiter must observe the acknowledgement");
}

#[tokio::test]
async fn app_resume_wait_retains_resume_published_before_wait_registration() {
    let control = Arc::new(TerminalInputControl::new());
    let epoch = control.request_pause();
    let probe = Arc::new(WaitRegistrationProbe::default());
    control.probe_wait_registration(Arc::clone(&probe));
    let waiter = tokio::spawn({
        let control = Arc::clone(&control);
        async move { control.wait_for_resume(epoch).await }
    });
    probe.wait_until_observed().await;

    control.resume(epoch);
    probe.allow_wait();

    tokio::time::timeout(Duration::from_millis(100), waiter)
        .await
        .expect("the retained resume must wake the input producer")
        .expect("the resume waiter must not panic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_old_reclaim_cannot_resume_a_successor_pause_epoch() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(1);
    let (mut app, owner) = App::new(
        Box::new(Label("ready")),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let app_task = tokio::spawn(async move { app.run().await });
    wait_until(|| locked(&screen).draws == 1).await;
    let input = RecordingInput::new([]);
    let producer = tokio::spawn(forward_terminal_input_from(
        Arc::clone(&input) as Arc<_>,
        terminal_tx.clone(),
        owner.input_control(),
    ));
    wait_until(|| input.polls.load(Ordering::SeqCst) > 0).await;

    let broker = Arc::new(owner.broker_with_timeout(Duration::from_secs(3_600)));
    let first_lease = broker
        .acquire(LeaseReason::new("first", "old terminal lease"))
        .await
        .expect("the first lease acquires");
    let first_epoch = owner.input.state.borrow().requested;
    let probe = Arc::new(ReclaimResumeProbe::default());
    owner.probe_reclaim_resume(Arc::clone(&probe));
    let old_reclaim = std::thread::spawn(move || first_lease.release());
    probe.wait_until_observed();

    let successor = tokio::spawn({
        let broker = Arc::clone(&broker);
        async move {
            broker
                .acquire(LeaseReason::new("second", "successor terminal lease"))
                .await
        }
    });
    wait_until(|| owner.input.state.borrow().requested > first_epoch).await;
    let successor_epoch = owner.input.state.borrow().requested;
    probe.allow_resume();
    old_reclaim
        .join()
        .expect("the old reclaim thread does not panic");

    let successor_lease = tokio::time::timeout(INPUT_PAUSE_TIMEOUT / 2, successor)
        .await
        .expect("the successor pause is acknowledged before the acquisition timeout")
        .expect("the successor acquisition task does not panic")
        .expect("the successor lease acquires");
    let state = *owner.input.state.borrow();
    assert_eq!(
        state.acknowledged, successor_epoch,
        "the producer must emit and the loop must acknowledge the successor pause"
    );
    assert_eq!(
        state.resumed, first_epoch,
        "the old reclaim must not consume the successor epoch"
    );

    successor_lease.release();
    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    app_task
        .await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
    producer
        .await
        .expect("the input producer exits after its consumer");
}

#[tokio::test]
async fn app_event_loop_consumes_both_bounded_channels_and_resize_relays_out() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let terminal_events = Arc::new(AtomicUsize::new(0));
    let engine_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events: Arc::clone(&terminal_events),
        engine_events: Arc::clone(&engine_events),
    };
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(1);
    let (mut app, _owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let task = tokio::spawn(async move { app.run().await });

    engine_tx
        .send(TurnEvent::TurnStarted {
            session_id: "ses_1".to_owned(),
        })
        .await
        .expect("engine event channel is open");
    wait_until(|| engine_events.load(Ordering::SeqCst) == 1).await;

    terminal_tx
        .send(TerminalEvent::Resize {
            width: 6,
            height: 2,
        })
        .await
        .expect("terminal event channel is open");
    wait_until(|| locked(&screen).width == 6).await;

    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");

    assert_eq!(terminal_events.load(Ordering::SeqCst), 2);
    assert_eq!(engine_events.load(Ordering::SeqCst), 1);
    assert_eq!(
        locked(&screen).buffer,
        Buffer::with_lines(["6x2   ", "      "]),
        "the smaller frame has no stale cells from the old layout"
    );
}

#[tokio::test]
async fn app_coalesces_queued_engine_redraws_into_one_frame() {
    const EVENTS: usize = 32;

    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let engine_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events: Arc::new(AtomicUsize::new(0)),
        engine_events: Arc::clone(&engine_events),
    };
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(EVENTS);
    for index in 0..EVENTS {
        engine_tx
            .send(TurnEvent::TurnStarted {
                session_id: format!("ses_{index}"),
            })
            .await
            .expect("the queued burst fits the bounded engine channel");
    }
    let (mut app, _owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let task = tokio::spawn(async move { app.run().await });

    wait_until(|| engine_events.load(Ordering::SeqCst) == EVENTS).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let draws_after_burst = locked(&screen).draws;

    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
    eprintln!(
        "queued engine burst: events={EVENTS}, total_frames={draws_after_burst}, event_frames={}",
        draws_after_burst.saturating_sub(1)
    );
    assert_eq!(
        draws_after_burst, 2,
        "one initial frame plus one coalesced frame must cover the entire burst"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_streaming_burst_never_exceeds_the_frame_rate_ceiling() {
    const EVENTS: usize = 120;

    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let engine_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events: Arc::new(AtomicUsize::new(0)),
        engine_events: Arc::clone(&engine_events),
    };
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(ENGINE_EVENT_CHANNEL_CAPACITY);
    let (app, _owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let mut app = app.with_redraw_config(REDRAW_CONFIG);
    let task = tokio::spawn(async move { app.run().await });
    let burst_tx = engine_tx.clone();
    let producer = tokio::spawn(async move {
        for index in 0..EVENTS {
            burst_tx
                .send(TurnEvent::TurnStarted {
                    session_id: format!("ses_{index}"),
                })
                .await
                .expect("engine event channel is open");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    producer.await.expect("the burst producer does not panic");
    wait_until(|| engine_events.load(Ordering::SeqCst) == EVENTS).await;
    tokio::time::sleep(REDRAW_CONFIG.active * 2).await;
    let draw_started = locked(&screen).draw_started.clone();

    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
    let streaming_frames = &draw_started[1..];
    eprintln!(
        "paced streaming burst: events={EVENTS}, frames={}",
        streaming_frames.len()
    );
    assert!(
        streaming_frames.len() < EVENTS / 4,
        "{EVENTS} redraw requests produced {} frames",
        streaming_frames.len()
    );
    for pair in streaming_frames.windows(2) {
        assert!(
            pair[1].duration_since(pair[0]) >= REDRAW_CONFIG.active,
            "two streaming frames bypassed the {:?} ceiling: {:?}",
            REDRAW_CONFIG.active,
            pair[1].duration_since(pair[0])
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_slow_frame_does_not_trigger_a_catch_up_burst() {
    const EVENTS: usize = 80;
    const SLOW_FRAME: Duration = Duration::from_millis(35);

    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let engine_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events: Arc::new(AtomicUsize::new(0)),
        engine_events: Arc::clone(&engine_events),
    };
    let (target, screen) = SharedTestTarget::with_draw_delay(10, 2, SLOW_FRAME);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(ENGINE_EVENT_CHANNEL_CAPACITY);
    let (app, _owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let mut app = app.with_redraw_config(REDRAW_CONFIG);
    let task = tokio::spawn(async move { app.run().await });
    let burst_tx = engine_tx.clone();
    let producer = tokio::spawn(async move {
        for index in 0..EVENTS {
            burst_tx
                .send(TurnEvent::TurnStarted {
                    session_id: format!("ses_slow_{index}"),
                })
                .await
                .expect("engine event channel is open");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    producer.await.expect("the burst producer does not panic");
    wait_until(|| engine_events.load(Ordering::SeqCst) == EVENTS).await;
    wait_until(|| locked(&screen).draw_started.len() >= 4).await;
    let draw_started = locked(&screen).draw_started.clone();

    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
    let minimum_gap = draw_started[1..]
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]))
        .min()
        .expect("the fixture produced multiple scheduled frames");
    eprintln!("slow-frame cadence: frame_cost={SLOW_FRAME:?}, minimum_start_gap={minimum_gap:?}");
    for pair in draw_started[1..].windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        assert!(
            gap >= SLOW_FRAME + REDRAW_CONFIG.active - Duration::from_millis(2),
            "a {:?} frame was followed after {gap:?}; missed ticks were replayed",
            SLOW_FRAME
        );
    }
}

#[tokio::test]
async fn app_keystrokes_draw_immediately_instead_of_waiting_for_the_stream_cadence() {
    const SAMPLES: usize = 25;
    let input_budget = Duration::from_millis(50);
    let config = RedrawConfig {
        active: Duration::from_millis(100),
        ..TEST_REDRAW_CONFIG
    };
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let terminal_events = Arc::new(AtomicUsize::new(0));
    let engine_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events: Arc::clone(&terminal_events),
        engine_events: Arc::clone(&engine_events),
    };
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(1);
    let (app, _owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let mut app = app.with_redraw_config(config);
    let task = tokio::spawn(async move { app.run().await });
    wait_until(|| locked(&screen).draws == 1).await;
    engine_tx
        .send(TurnEvent::TurnStarted {
            session_id: "ses_typing".to_owned(),
        })
        .await
        .expect("engine event channel is open");
    wait_until(|| engine_events.load(Ordering::SeqCst) == 1).await;

    let mut latencies = Vec::with_capacity(SAMPLES);
    for sample in 1..=SAMPLES {
        let draws_before = locked(&screen).draws;
        let started = Instant::now();
        terminal_tx
            .send(TerminalEvent::Input(CrosstermEvent::FocusGained))
            .await
            .expect("terminal event channel is open");
        wait_until_within(input_budget, || {
            terminal_events.load(Ordering::SeqCst) == sample
                && locked(&screen).draws == draws_before + 1
        })
        .await;
        latencies.push(started.elapsed());
    }
    latencies.sort_unstable();
    let p50 = latencies[SAMPLES / 2];
    let p95 = latencies[(SAMPLES * 95).div_ceil(100) - 1];

    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
    eprintln!("keystroke latency over {SAMPLES} samples: p50={p50:?}, p95={p95:?}");
    assert!(p95 < input_budget, "p50={p50:?}, p95={p95:?}");
    assert!(
        p95 < config.active,
        "keystrokes waited for the streaming cadence: p95={p95:?}"
    );
}

#[tokio::test]
async fn app_idle_schedule_backs_off_and_activity_wakes_it() {
    let now = TokioInstant::now();
    let mut schedule = RedrawSchedule::new(REDRAW_CONFIG, now);
    assert_eq!(schedule.cadence(), REDRAW_CONFIG.idle);
    assert_eq!(
        schedule.missed_tick_behavior(),
        MissedTickBehavior::Skip,
        "slow frames must skip stale deadlines instead of replaying them"
    );

    schedule.refresh(now + REDRAW_CONFIG.deep_idle_after);
    assert_eq!(schedule.cadence(), REDRAW_CONFIG.deep_idle);

    let activity = now + REDRAW_CONFIG.deep_idle_after + Duration::from_millis(1);
    schedule.record_terminal_activity(activity);
    assert_eq!(schedule.cadence(), REDRAW_CONFIG.idle);
    schedule.record_engine_activity(
        &TurnEvent::TurnStarted {
            session_id: "ses_awake".to_owned(),
        },
        activity,
    );
    assert_eq!(schedule.cadence(), REDRAW_CONFIG.active);
}

#[tokio::test]
async fn app_deep_idle_wakes_at_the_idle_tier_when_new_work_arrives() {
    let config = RedrawConfig {
        deep_idle_after: Duration::from_millis(60),
        ..REDRAW_CONFIG
    };
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let engine_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events: Arc::new(AtomicUsize::new(0)),
        engine_events: Arc::clone(&engine_events),
    };
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(1);
    let (app, _owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let mut app = app.with_redraw_config(config);
    let task = tokio::spawn(async move { app.run().await });
    wait_until(|| locked(&screen).draws == 1).await;
    tokio::time::sleep(config.deep_idle_after + config.idle).await;

    let activity_started = Instant::now();
    engine_tx
        .send(TurnEvent::TurnCompleted {
            assistant_message_id: "msg_idle_wake".to_owned(),
            steps: 1,
        })
        .await
        .expect("engine event channel is open");
    wait_until(|| engine_events.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(config.idle / 2).await;
    assert_eq!(
        locked(&screen).draws,
        1,
        "idle work must back off instead of drawing as an active stream"
    );
    wait_until_within(config.idle * 2, || locked(&screen).draws == 2).await;
    let idle_wake_latency = activity_started.elapsed();

    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
    eprintln!(
        "deep-idle activity wake: idle_tier={:?}, deep_tier={:?}, frame_latency={idle_wake_latency:?}",
        config.idle, config.deep_idle
    );
}

#[tokio::test]
async fn app_dirty_timer_never_draws_while_a_terminal_lease_is_held() {
    let config = RedrawConfig {
        active: Duration::from_millis(40),
        ..TEST_REDRAW_CONFIG
    };
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let engine_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events: Arc::new(AtomicUsize::new(0)),
        engine_events: Arc::clone(&engine_events),
    };
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(1);
    let (app, owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let mut app = app.with_redraw_config(config);
    let task = tokio::spawn(async move { app.run().await });
    wait_until(|| locked(&screen).draws == 1).await;
    engine_tx
        .send(TurnEvent::TurnStarted {
            session_id: "ses_dirty_lease".to_owned(),
        })
        .await
        .expect("engine event channel is open");
    wait_until(|| engine_events.load(Ordering::SeqCst) == 1).await;
    let lease = owner
        .broker_with_timeout(Duration::from_secs(3_600))
        .acquire(LeaseReason::new("tui", "external editor"))
        .await
        .expect("the TUI yields before the dirty deadline");

    tokio::time::sleep(config.active * 2).await;
    assert_eq!(
        locked(&screen).draws,
        1,
        "a timer wrote to the TTY while the external editor owned it"
    );

    lease.release();
    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
}

#[tokio::test]
async fn app_event_loop_defers_engine_rendering_while_a_lease_owns_the_tty() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let terminal_events = Arc::new(AtomicUsize::new(0));
    let engine_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events,
        engine_events: Arc::clone(&engine_events),
    };
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(1);
    let (mut app, owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let task = tokio::spawn(async move { app.run().await });
    wait_until(|| locked(&screen).draws == 1).await;
    let broker = owner.broker_with_timeout(Duration::from_secs(3_600));
    let lease = broker
        .acquire(LeaseReason::new("kiro", "device-code prompt"))
        .await
        .expect("the TUI yields to the child");

    engine_tx
        .send(TurnEvent::TurnStarted {
            session_id: "ses_deferred".to_owned(),
        })
        .await
        .expect("engine event channel is open");
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(engine_events.load(Ordering::SeqCst), 0);
    assert_eq!(locked(&screen).draws, 1);

    lease.release();
    wait_until(|| engine_events.load(Ordering::SeqCst) == 1).await;
    wait_until(|| locked(&screen).draws == 3).await;
    assert_eq!(
        locked(&screen).draws,
        3,
        "reclaim and deferred event both redraw"
    );
    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
}

#[tokio::test]
async fn app_discards_terminal_input_that_was_buffered_at_the_lease_boundary() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let terminal_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events: Arc::clone(&terminal_events),
        engine_events: Arc::new(AtomicUsize::new(0)),
    };
    let (target, _screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(1);
    let (mut app, owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let input = RecordingInput::new([CrosstermEvent::FocusGained]);
    let producer = tokio::spawn(forward_terminal_input_from(
        Arc::clone(&input) as Arc<_>,
        terminal_tx.clone(),
        owner.input_control(),
    ));
    wait_until(|| input.reads.load(Ordering::SeqCst) == 1).await;
    let broker = Arc::new(owner.broker_with_timeout(Duration::from_secs(3_600)));
    let lease_task = tokio::spawn({
        let broker = Arc::clone(&broker);
        async move {
            broker
                .acquire(LeaseReason::new("tui", "external editor"))
                .await
        }
    });
    wait_until(|| owner.suspended.load(Ordering::SeqCst)).await;
    let task = tokio::spawn(async move { app.run().await });
    let lease = lease_task
        .await
        .expect("the lease task does not panic")
        .expect("the producer confirms its pause");

    lease.release();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(
        terminal_events.load(Ordering::SeqCst),
        0,
        "input read for the old TUI ownership epoch must not execute after the editor returns"
    );
    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
    producer
        .await
        .expect("the input producer exits after its consumer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_input_producer_performs_no_reads_while_a_lease_is_held() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(1);
    let (mut app, owner) = App::new(
        Box::new(Label("ready")),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let task = tokio::spawn(async move { app.run().await });
    wait_until(|| locked(&screen).draws == 1).await;
    let input = RecordingInput::new([]);
    let producer = tokio::spawn(forward_terminal_input_from(
        Arc::clone(&input) as Arc<_>,
        terminal_tx.clone(),
        owner.input_control(),
    ));
    wait_until(|| input.polls.load(Ordering::SeqCst) > 0).await;
    let lease = owner
        .broker_with_timeout(Duration::from_secs(3_600))
        .acquire(LeaseReason::new("kiro", "device-code prompt"))
        .await
        .expect("the producer acknowledges that it stopped reading");
    let polls_while_paused = input.polls.load(Ordering::SeqCst);

    tokio::time::sleep(INPUT_POLL_INTERVAL * 2).await;

    assert_eq!(
        input.polls.load(Ordering::SeqCst),
        polls_while_paused,
        "the parent entered another terminal poll while the lease holder owned stdin"
    );
    lease.release();
    wait_until(|| input.polls.load(Ordering::SeqCst) > polls_while_paused).await;
    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");
    producer
        .await
        .expect("the input producer exits after its consumer");
}

#[test]
fn app_declares_bounded_lossless_channel_capacities() {
    let (terminal_tx, _terminal_rx) = terminal_event_channel();

    assert_eq!(terminal_tx.capacity(), TERMINAL_EVENT_CHANNEL_CAPACITY);
    assert_eq!(
        ENGINE_EVENT_CHANNEL_CAPACITY, TURN_EVENT_CHANNEL_CAPACITY,
        "the TUI consumes the engine's declared lossless bounded channel"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_the_input_producer_returns_once_its_consumer_is_gone() {
    // The boot path aborts this task, but a producer that could only end by being
    // aborted would keep a blocking thread alive across a clean exit. One poll
    // interval is the whole budget.
    let (sender, receiver) = terminal_event_channel();
    let producer = tokio::spawn(forward_terminal_input(
        sender,
        Arc::new(TerminalInputControl::new()),
    ));
    drop(receiver);

    tokio::time::timeout(INPUT_POLL_INTERVAL * 4, producer)
        .await
        .expect("the producer must notice a closed channel within a few polls")
        .expect("the producer must not panic");
}
