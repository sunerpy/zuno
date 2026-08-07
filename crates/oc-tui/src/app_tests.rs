use std::convert::Infallible;
use std::io;
use std::panic::{AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crossterm::event::Event as CrosstermEvent;
use oc_engine::r#loop::{TURN_EVENT_CHANNEL_CAPACITY, TurnEvent};
use oc_engine::terminal_lease::{LeaseReason, TerminalLease, TerminalLeaseError};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::Paragraph;
use tokio::sync::mpsc;

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
}

struct SharedTestTarget {
    screen: Arc<Mutex<Screen>>,
}

impl SharedTestTarget {
    fn new(width: u16, height: u16) -> (Self, Arc<Mutex<Screen>>) {
        let screen = Arc::new(Mutex::new(Screen {
            width,
            height,
            buffer: Buffer::empty(Rect::new(0, 0, width, height)),
            draws: 0,
            clears: 0,
        }));
        (
            Self {
                screen: Arc::clone(&screen),
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
    let producer = tokio::spawn(forward_terminal_input(sender));
    drop(receiver);

    tokio::time::timeout(INPUT_POLL_INTERVAL * 4, producer)
        .await
        .expect("the producer must notice a closed channel within a few polls")
        .expect("the producer must not panic");
}
