use std::convert::Infallible;
use std::io;
use std::panic::{AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crossterm::event::{Event as CrosstermEvent, MouseButton, MouseEventKind};
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

    let enabled: Vec<&str> = [
        "EnableBracketedPaste",
        "NarrowMouseCapture",
        "AlternateScrollCapture",
    ]
    .into_iter()
    .filter(|command| entering.contains(command))
    .collect();
    assert_eq!(
        enabled.len(),
        3,
        "the scan did not find the commands it exists to pair, so it would pass \
         vacuously: {enabled:?}"
    );
    for command in enabled {
        let paired = command
            .replace("Enable", "Disable")
            .replace("Capture", "Release");
        assert!(
            leaving.contains(&paired),
            "`{command}` is enabled on the way in and `{paired}` never runs on the way \
             out, which leaves the terminal altered after the process ends"
        );
    }

    // The command-name half above cannot see inside a hand-written sequence, and this
    // build now writes DEC private modes directly. So the modes are paired too, derived
    // from whatever the source actually asks for rather than from a list: `?1000h` with no
    // `?1000l` leaves the user's shell reporting mouse clicks as garbage input, and a list
    // of names went silent the moment `EnableMouseCapture` stopped being the thing called.
    let private_modes = |source: &str, suffix: char| -> std::collections::BTreeSet<String> {
        let mut found = std::collections::BTreeSet::new();
        let mut rest = source;
        while let Some(at) = rest.find("[?") {
            rest = &rest[at + 2..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                continue;
            }
            if rest[digits.len()..].starts_with(suffix) {
                found.insert(digits);
            }
        }
        found
    };
    let whole = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app.rs"),
    )
    .expect("read this module's own source");
    let set = private_modes(&whole, 'h');
    let unset = private_modes(&whole, 'l');
    assert!(
        set.len() >= 2,
        "the mode scan found {} enabled modes, so it is inspecting nothing: {set:?}",
        set.len()
    );
    let orphans: Vec<&String> = set.difference(&unset).collect();
    assert!(
        orphans.is_empty(),
        "these DEC private modes are set and never unset, so they outlive the process: \
         {orphans:?}"
    );
}

#[test]
fn app_mouse_reporting_asks_only_for_the_events_a_screen_consumes() {
    // `EnableMouseCapture` requests `?1002` and `?1003`, which report drag and every
    // pointer motion. Nothing in this binary reads either, and each arriving event costs
    // two `spawn_blocking` hops and a slot in the bounded input channel — so requesting
    // them makes pointer movement delay keystrokes. This pins the narrowed request.
    let mut entered = Vec::new();
    enter_terminal(&mut entered, true).expect("a vector accepts every write");
    let entered = String::from_utf8(entered).expect("crossterm writes utf-8");

    assert!(
        entered.contains("\u{1b}[?1000h"),
        "press/release reporting is off, so the wheel sends nothing: {entered:?}"
    );
    assert!(
        entered.contains("\u{1b}[?1006h"),
        "SGR encoding is off, so a click past column 223 reports the wrong cell: \
         {entered:?}"
    );
    for (mode, what) in [("1002", "drag"), ("1003", "any pointer motion")] {
        assert!(
            !entered.contains(&format!("\u{1b}[?{mode}h")),
            "mode ?{mode} ({what}) is requested and nothing consumes it: {entered:?}"
        );
    }

    // And nothing is requested when the user turned the mouse off.
    let mut without = Vec::new();
    enter_terminal(&mut without, false).expect("a vector accepts every write");
    let without = String::from_utf8(without).expect("crossterm writes utf-8");
    assert!(
        !without.contains("\u{1b}[?1000h"),
        "`mouse = false` still grabbed the pointer, so native selection stays broken for \
         the user who opted out: {without:?}"
    );
    assert!(
        without.contains("\u{1b}[?1007h"),
        "native-selection mode did not ask the terminal to translate wheel notches: {without:?}"
    );
    let mut restored = Vec::new();
    assert!(restore_terminal(&mut restored, false).is_none());
    assert!(
        String::from_utf8(restored)
            .expect("crossterm writes utf-8")
            .contains("\u{1b}[?1007l"),
        "alternate scroll was not restored on exit"
    );
}

#[tokio::test]
async fn app_motion_is_dropped_at_the_boundary_while_the_wheel_and_a_press_still_arrive() {
    // The functional risk in narrowing the mouse pipeline: drop too much and the wheel
    // stops scrolling, or the ambient panel's disclosure triangles stop answering. Driven
    // through the real producer, so what is asserted is what the event loop would actually
    // receive.
    //
    // `Moved` and `Drag` are what must not survive. A press must, and `Drag(Left)` sitting
    // immediately before it in the input is deliberate: the two differ only in their
    // variant, so a filter widened to "any left-button event" would let the drag through
    // and be caught here rather than reintroducing the motion cost `?1002` was removed for.
    let mouse = |kind| {
        CrosstermEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column: 4,
            row: 4,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
    };
    let input = RecordingInput::new([
        mouse(MouseEventKind::Moved),
        mouse(MouseEventKind::Drag(MouseButton::Left)),
        mouse(MouseEventKind::Down(MouseButton::Left)),
        mouse(MouseEventKind::ScrollDown),
        mouse(MouseEventKind::Moved),
        mouse(MouseEventKind::ScrollUp),
    ]);
    let (sender, mut receiver) = terminal_event_channel();
    let control = Arc::new(TerminalInputControl::new());
    let producer = tokio::spawn(forward_terminal_input_from(
        Arc::clone(&input) as Arc<_>,
        sender,
        Arc::clone(&control),
    ));

    let mut delivered = Vec::new();
    for _ in 0..3 {
        let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("the consumable events arrive well inside the timeout")
            .expect("the producer is still running");
        delivered.push(event);
    }
    drop(receiver);
    let _ = producer.await;

    let kinds: Vec<MouseEventKind> = delivered
        .iter()
        .filter_map(|event| match event {
            TerminalEvent::Input(CrosstermEvent::Mouse(mouse)) => Some(mouse.kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollUp
        ],
        "the producer forwarded something other than the press and the two wheel notches, \
         so either motion reached the queue or a consumed event stopped arriving: \
         {delivered:?}"
    );
    assert_eq!(
        input.reads.load(Ordering::SeqCst),
        6,
        "the filter is meant to drop after the read, not to skip reading; a changed count \
         means this test is no longer exercising the path it describes"
    );
}

#[test]
fn app_the_input_filter_forwards_exactly_what_a_screen_consumes() {
    // Two hand-written match arms, in different files, that have to agree: the filter in
    // `is_consumable_mouse` and `SessionScreen::handle_mouse`'s own match. A source scan
    // rather than a shared constant because the screen's arms are what actually decide
    // behaviour, and the day a screen learns to drag this must fail rather than silently
    // drop the event before the screen ever sees it.
    let screen = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/views/session.rs"),
    )
    .expect("read the screen's source");
    let start = screen
        .find("fn handle_mouse(")
        .expect("handle_mouse is gone; this scan is checking nothing");
    let rest = &screen[start..];
    let end = rest.find("\n    }\n").expect("a method body ends");
    let consumed: std::collections::BTreeSet<&str> = rest[..end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter_map(|line| line.split("MouseEventKind::").nth(1))
        .map(|tail| {
            tail.split(|character: char| !character.is_alphanumeric())
                .next()
                .unwrap_or_default()
        })
        .filter(|name| !name.is_empty())
        .collect();
    assert!(
        consumed.len() >= 2,
        "the scan found {} consumed mouse kinds, so it is measuring nothing: {consumed:?}",
        consumed.len()
    );

    for name in &consumed {
        let kind = match *name {
            "ScrollUp" => MouseEventKind::ScrollUp,
            "ScrollDown" => MouseEventKind::ScrollDown,
            "ScrollLeft" => MouseEventKind::ScrollLeft,
            "ScrollRight" => MouseEventKind::ScrollRight,
            "Moved" => MouseEventKind::Moved,
            "Down" => MouseEventKind::Down(MouseButton::Left),
            other => panic!(
                "`{other}` is consumed by the screen and this test cannot construct it, so \
                 the filter's agreement with the screen is unproven"
            ),
        };
        assert!(
            is_consumable_mouse(kind),
            "the screen acts on `{name}` but the input filter drops it before the channel, \
             so that arm can never run"
        );
    }
    // The complement: a kind the screen ignores must not reach the queue.
    for kind in [
        MouseEventKind::Moved,
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Left),
        // A right press: the screen hit-tests the left button only, so a filter widened to
        // "any button" would be caught here rather than by a user wondering why a
        // context-menu attempt collapsed a panel section.
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        if consumable_name(kind).is_some_and(|name| consumed.contains(name)) {
            continue;
        }
        assert!(
            !is_consumable_mouse(kind),
            "{kind:?} is forwarded but no screen consumes it, so it only costs a channel \
             slot a keystroke needs"
        );
    }
}

/// The scan-comparable name of a mouse kind, for kinds this test enumerates.
const fn consumable_name(kind: MouseEventKind) -> Option<&'static str> {
    match kind {
        MouseEventKind::ScrollUp => Some("ScrollUp"),
        MouseEventKind::ScrollDown => Some("ScrollDown"),
        MouseEventKind::ScrollLeft => Some("ScrollLeft"),
        MouseEventKind::ScrollRight => Some("ScrollRight"),
        MouseEventKind::Moved => Some("Moved"),
        MouseEventKind::Down(MouseButton::Left) => Some("Down"),
        _ => None,
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

    assert_eq!(forced.requester, "kiro");
    assert!(lifecycle.is_active());
    assert_eq!(locked(&screen).draws, 1);
    let diagnostics = owner.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("requester `kiro`"));
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

/// A root that claims nothing, which is what every shipped view does with a resize.
///
/// [`EventRecorder`] answers `REDRAW` to everything, so it cannot be used to ask whether the
/// *host* forces a frame — it would report one either way. `TranscriptView` answers the whole
/// `Terminal` arm with `IGNORED` (`message.rs`), so this is the real production shape, and it
/// is what makes `draws` attributable: with no view ever setting the loop's dirty bit, the
/// scheduled tick draws nothing, and every frame after the startup one has exactly one cause.
struct IgnoringRoot {
    terminal_events: Arc<AtomicUsize>,
}

impl Component for IgnoringRoot {
    fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        frame.render_widget(
            Paragraph::new(format!("{}x{}", area.width, area.height)),
            area,
        );
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        if matches!(event, AppEvent::Terminal(_)) {
            self.terminal_events.fetch_add(1, Ordering::SeqCst);
        }
        EventResult::IGNORED
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
async fn app_a_resize_alone_repaints_without_waiting_for_another_event() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let terminal_events = Arc::new(AtomicUsize::new(0));
    let root = IgnoringRoot {
        terminal_events: Arc::clone(&terminal_events),
    };
    let (target, screen) = SharedTestTarget::new(10, 4);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(1);
    let (app, _owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let mut app = app.with_redraw_config(TEST_REDRAW_CONFIG);
    let task = tokio::spawn(async move { app.run().await });

    wait_until(|| locked(&screen).draws == 1).await;
    terminal_tx
        .send(TerminalEvent::Resize {
            width: 6,
            height: 3,
        })
        .await
        .expect("terminal event channel is open");
    wait_until(|| terminal_events.load(Ordering::SeqCst) == 1).await;
    // Two tiers of the scheduled cadence, so a frame that only arrived because the timer
    // eventually fired would be counted here and the equality below would read 3.
    tokio::time::sleep(TEST_REDRAW_CONFIG.deep_idle + TEST_REDRAW_CONFIG.deep_idle).await;
    let drawn = locked(&screen).draws;
    let painted = locked(&screen).buffer.clone();

    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");

    assert_eq!(
        drawn, 2,
        "the startup frame plus exactly one resize frame; a root that claims nothing leaves \
         the loop's dirty bit clear, so any other count means the resize either painted \
         nothing or painted twice"
    );
    assert_eq!(
        painted,
        Buffer::with_lines(["6x3   ", "      ", "      "]),
        "the frame the resize produced must already describe the new geometry"
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

/// Drives the *shipped* 40 ms threshold, not a lowered one, so what the test proves
/// is what a user's binary would report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_reports_a_frame_that_exceeds_the_shipped_slow_frame_threshold() {
    // 60 ms is above `SLOW_FRAME_THRESHOLD` and below nothing else in the loop, so the
    // only reason a frame here is slow is the draw itself.
    const FRAME_COST: Duration = Duration::from_millis(60);

    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let terminal_events = Arc::new(AtomicUsize::new(0));
    let root = EventRecorder {
        terminal_events: Arc::clone(&terminal_events),
        engine_events: Arc::new(AtomicUsize::new(0)),
    };
    let (target, screen) = SharedTestTarget::with_draw_delay(10, 2, FRAME_COST);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(ENGINE_EVENT_CHANNEL_CAPACITY);
    let (app, owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let ui = Arc::clone(&owner.ui);
    let mut app = app.with_redraw_config(TEST_REDRAW_CONFIG);
    let task = tokio::spawn(async move { app.run().await });

    terminal_tx
        .send(TerminalEvent::Input(CrosstermEvent::FocusGained))
        .await
        .expect("terminal event channel is open");
    wait_until(|| locked(&screen).draws >= 2).await;
    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");

    let frames = &locked(&ui).frames;
    assert_eq!(
        frames.threshold(),
        zuno_observability::frame::SLOW_FRAME_THRESHOLD,
        "the shipped threshold was not the one under test"
    );
    let recorded = frames.recent();
    assert!(
        !recorded.is_empty(),
        "{FRAME_COST:?} frames drawn against a {:?} threshold produced no report at all; \
         {} frames were measured",
        frames.threshold(),
        frames.drawn()
    );
    let startup = recorded
        .iter()
        .find(|frame| frame.cause == zuno_observability::frame::cause::STARTUP)
        .expect("the pre-loop frame must be attributed to startup");
    assert!(
        Duration::from_micros(startup.elapsed_micros) >= FRAME_COST,
        "a {FRAME_COST:?} draw was recorded as {} us",
        startup.elapsed_micros
    );
    assert!(
        recorded
            .iter()
            .any(|frame| frame.cause == zuno_observability::frame::cause::TERMINAL_INPUT),
        "the keystroke's immediate frame was not attributed to terminal input: {recorded:?}"
    );
    assert_eq!(
        frames.slow(),
        frames.drawn(),
        "every {FRAME_COST:?} frame should be slow, but {} of {} were",
        frames.slow(),
        frames.drawn()
    );
}

/// A frame that is fast must leave the log silent, or the threshold is decoration.
#[tokio::test]
async fn app_leaves_a_fast_frame_unreported() {
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let root = EventRecorder {
        terminal_events: Arc::new(AtomicUsize::new(0)),
        engine_events: Arc::new(AtomicUsize::new(0)),
    };
    let (target, screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(ENGINE_EVENT_CHANNEL_CAPACITY);
    let (app, owner) = App::new(
        Box::new(root),
        Box::new(target),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let ui = Arc::clone(&owner.ui);
    let mut app = app.with_redraw_config(TEST_REDRAW_CONFIG);
    let task = tokio::spawn(async move { app.run().await });

    terminal_tx
        .send(TerminalEvent::Input(CrosstermEvent::FocusGained))
        .await
        .expect("terminal event channel is open");
    wait_until(|| locked(&screen).draws >= 2).await;
    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("terminal event channel is open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");

    let frames = &locked(&ui).frames;
    assert!(
        frames.drawn() >= 2,
        "the fixture drew {} frames",
        frames.drawn()
    );
    assert_eq!(
        frames.slow(),
        0,
        "an offscreen 10x2 frame was reported slow: {:?}",
        frames.recent()
    );
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

/// Counts terminal events by kind, so a wake can be told from a keystroke.
///
/// [`EventRecorder`] counts every terminal event together, which cannot distinguish "the
/// wake was dispatched" from "a resize happened to arrive". This one names the kinds.
struct WakeRecorder {
    wakes: Arc<AtomicUsize>,
    inputs: Arc<AtomicUsize>,
}

impl Component for WakeRecorder {
    fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        frame.render_widget(Paragraph::new("ready"), area);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        match event {
            AppEvent::Terminal(TerminalEvent::Wake) => {
                self.wakes.fetch_add(1, Ordering::SeqCst);
            }
            AppEvent::Terminal(TerminalEvent::Input(_)) => {
                self.inputs.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        EventResult::REDRAW
    }
}

/// A suspended app, its terminal sender, and the counters its root increments.
///
/// # Why the pause barrier is sent by hand
///
/// The interleaving is the whole subject, and with a real input producer the barrier's
/// position in the queue is a race — the test would prove whichever order it happened to
/// win. Sending it here makes the order the assertion instead of the weather.
///
/// Everything else is production: the real [`TerminalOwner::yield_terminal`] publishes
/// `suspended`, the real `reclaim_terminal` resumes, and the epoch is the one the real
/// handshake allocated, so the loop's `acknowledge` is fed the value the producer would
/// have sent. With no producer attached `wait_for_pause` returns immediately, which is why
/// the lease can be taken without one.
struct SuspendedApp {
    terminal_tx: mpsc::Sender<TerminalEvent>,
    wakes: Arc<AtomicUsize>,
    inputs: Arc<AtomicUsize>,
    owner: Arc<TerminalLeaseOwner>,
    /// Held for the whole fixture: dropping it closes the engine inlet, and the loop
    /// treats that as a fatal `EngineEventsClosed` rather than as an idle app.
    _engine_tx: mpsc::Sender<TurnEvent>,
    lease: Option<zuno_engine::terminal_lease::TerminalLeaseGuard>,
    task: tokio::task::JoinHandle<Result<(), AppError>>,
    epoch: u64,
}

impl SuspendedApp {
    async fn new() -> Self {
        let lifecycle = Arc::new(FakeLifecycle::default());
        lifecycle.enter().expect("fake terminal enters");
        let wakes = Arc::new(AtomicUsize::new(0));
        let inputs = Arc::new(AtomicUsize::new(0));
        let root = WakeRecorder {
            wakes: Arc::clone(&wakes),
            inputs: Arc::clone(&inputs),
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
        wait_until(move || locked(&screen).draws == 1).await;

        let lease = owner
            .broker_with_timeout(Duration::from_secs(3_600))
            .acquire(LeaseReason::new("tui", "external editor"))
            .await
            .expect("the lease is granted with no producer attached");
        wait_until(|| owner.suspended.load(Ordering::SeqCst)).await;
        let epoch = owner.input.state.borrow().requested;

        Self {
            terminal_tx,
            wakes,
            inputs,
            owner,
            _engine_tx: engine_tx,
            lease: Some(lease),
            task,
            epoch,
        }
    }

    async fn send(&self, event: TerminalEvent) {
        self.terminal_tx
            .send(event)
            .await
            .expect("the terminal channel is open");
    }

    /// The barrier the input producer places after the last key it read.
    async fn send_pause_barrier(&self) {
        self.send(TerminalEvent::InputPaused(self.epoch)).await;
        // The loop acknowledges the barrier as it consumes it, so this is the observable
        // proof that the suspended branch really ran — no sleep required.
        wait_until(|| self.owner.input.state.borrow().acknowledged >= self.epoch).await;
    }

    /// Wait until the loop has taken every event sent so far, then prove it is still
    /// suspended.
    ///
    /// Without this the tests are races rather than interleavings. An event still sitting
    /// in the channel when the lease releases is received by the *resumed* loop and
    /// dispatched normally, so a test that released the lease straight after sending would
    /// pass against a build that discards wakes while suspended — which is exactly what one
    /// of these did before this probe existed.
    ///
    /// A restored channel capacity is the observable: the sender reports a slot free only
    /// once the receiver has moved the value out, and the loop yields only at its `select!`,
    /// so a test task that sees an empty channel is running after the loop's match arm for
    /// that event has already executed.
    async fn drained(&self) {
        wait_until(|| self.terminal_tx.capacity() == TERMINAL_EVENT_CHANNEL_CAPACITY).await;
        assert!(
            self.owner.suspended.load(Ordering::SeqCst),
            "the lease ended before the loop consumed the events under test, so this \
             interleaving was never actually exercised"
        );
    }

    /// Release the lease and wait for the resumed loop to finish what it owes.
    async fn resume(&mut self) {
        self.lease
            .take()
            .expect("the lease is still held")
            .release();
        wait_until(|| !self.owner.suspended.load(Ordering::SeqCst)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    async fn shutdown(self) {
        self.send(TerminalEvent::Shutdown).await;
        self.task
            .await
            .expect("the event loop task does not panic")
            .expect("the event loop exits cleanly");
    }
}

#[tokio::test]
async fn app_dispatches_a_wake_that_arrived_before_the_pause_barrier() {
    // The interleaving that actually tests the rule: the wake is remembered *first*, and
    // then `InputPaused` clears the deferral queue. A fix that parked the wake in that
    // queue would pass the other ordering and fail here.
    let mut app = SuspendedApp::new().await;

    app.send(TerminalEvent::Wake).await;
    app.send_pause_barrier().await;
    app.drained().await;
    assert_eq!(
        app.wakes.load(Ordering::SeqCst),
        0,
        "a wake must not be dispatched while the editor owns the terminal"
    );

    app.resume().await;

    assert_eq!(
        app.wakes.load(Ordering::SeqCst),
        1,
        "the wake raised during the lease never reached the screen, so a diagnostics \
         batch that finished while the editor was open would sit in its channel forever"
    );
    app.shutdown().await;
}

#[tokio::test]
async fn app_dispatches_a_wake_that_arrived_after_the_pause_barrier() {
    // The production ordering: the producer parks promptly and the checker finishes much
    // later, well inside a long editing session.
    let mut app = SuspendedApp::new().await;

    app.send_pause_barrier().await;
    app.send(TerminalEvent::Wake).await;
    app.drained().await;
    assert_eq!(
        app.wakes.load(Ordering::SeqCst),
        0,
        "a wake must not be dispatched while the editor owns the terminal"
    );

    app.resume().await;

    assert_eq!(
        app.wakes.load(Ordering::SeqCst),
        1,
        "a wake raised after the barrier but still during the lease was discarded"
    );
    app.shutdown().await;
}

#[tokio::test]
async fn app_collapses_many_wakes_raised_during_one_lease_into_a_single_dispatch() {
    // A wake carries no payload, so seventeen of them ask the same question once. This is
    // what makes a `bool` the right shape: it cannot grow across an editing session of any
    // length, and one dispatch drains every report the seventeen were raised for.
    let mut app = SuspendedApp::new().await;

    for _ in 0..17 {
        app.send(TerminalEvent::Wake).await;
    }
    app.send_pause_barrier().await;
    app.drained().await;
    app.resume().await;

    assert_eq!(
        app.wakes.load(Ordering::SeqCst),
        1,
        "seventeen wakes must coalesce into one dispatch, not seventeen"
    );
    app.shutdown().await;
}

#[tokio::test]
async fn app_still_discards_physical_input_buffered_across_the_lease() {
    // The other half of the distinction, asserted so the fix cannot be "remember
    // everything". A key read before the handoff belongs to neither the editor nor the
    // resumed TUI, and replaying it could submit a prompt or exit the app.
    let mut app = SuspendedApp::new().await;

    app.send(TerminalEvent::Input(CrosstermEvent::FocusGained))
        .await;
    app.send(TerminalEvent::Wake).await;
    app.send_pause_barrier().await;
    app.drained().await;
    app.resume().await;

    assert_eq!(
        app.inputs.load(Ordering::SeqCst),
        0,
        "input buffered at the lease boundary must still be discarded"
    );
    assert_eq!(
        app.wakes.load(Ordering::SeqCst),
        1,
        "the wake beside it must still survive"
    );
    app.shutdown().await;
}

/// An app that is never run, for asserting on its deferral bookkeeping directly.
fn idle_app() -> (App, Arc<TerminalLeaseOwner>, mpsc::Sender<TerminalEvent>) {
    let lifecycle = Arc::new(FakeLifecycle::default());
    lifecycle.enter().expect("fake terminal enters");
    let (target, _screen) = SharedTestTarget::new(10, 2);
    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (_engine_tx, engine_rx) = mpsc::channel(1);
    let (app, owner) = App::new(
        Box::new(Label("ready")),
        Box::new(target),
        lifecycle as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    (app, owner, terminal_tx)
}

#[test]
fn app_a_deferred_wake_is_remembered_as_a_bit_and_survives_the_next_pause_barrier() {
    // The resume race, asserted where it is decidable. The lease can be taken again between
    // the loop's `suspended` check and `handle_terminal`'s own, which yields
    // `Dispatch::Deferred(Wake)`. Routing that into `pending_terminal` compiles and looks
    // right, and the very next `InputPaused` then erases it — so the wake would be lost
    // exactly when a second lease follows the first.
    //
    // Driven through `defer_terminal`, the one funnel every deferral passes, rather than by
    // racing two leases: the interleaving that produces `Deferred` is not schedulable on
    // demand, and a test that waited for it would be a flake rather than a proof. The
    // outcome across two real leases is covered by
    // [`app_a_wake_survives_a_second_lease_taken_immediately_after_the_first`].
    let (mut app, owner, _terminal_tx) = idle_app();

    app.defer_terminal(TerminalEvent::Wake, Placement::Back);
    assert!(app.pending_wake, "a deferred wake must be remembered");
    assert!(
        app.pending_terminal.is_empty(),
        "a deferred wake must not enter the queue that `InputPaused` clears"
    );

    // A real key beside it, so the assertion below distinguishes "the barrier cleared
    // everything" from "the barrier cleared the right thing".
    app.defer_terminal(
        TerminalEvent::Input(CrosstermEvent::FocusGained),
        Placement::Back,
    );
    assert_eq!(app.pending_terminal.len(), 1);

    let epoch = owner.input.state.borrow().requested + 1;
    app.handle_terminal(TerminalEvent::InputPaused(epoch))
        .expect("the barrier is handled");

    assert!(
        app.pending_terminal.is_empty(),
        "the barrier must still retire stale physical input"
    );
    assert!(
        app.pending_wake,
        "the barrier erased the owed wake, so a wake deferred by the resume race would be \
         lost and the reports it was raised for would never be drawn"
    );
}

#[tokio::test]
async fn app_a_wake_survives_a_second_lease_taken_immediately_after_the_first() {
    // The same claim over the production path: two leases back to back, with the wake
    // raised during the first. Whichever way the loop and the successor interleave — the
    // wake dispatched in the gap, or deferred and re-owed — it must reach the screen
    // exactly once, and the successor's own barrier must not erase it.
    //
    // Honest about its own reach: which interleaving occurs is the scheduler's choice, so
    // this passes when the wake is dispatched in the gap even against a build that routes a
    // deferred wake into the queue. It is a guard on the outcome, and the sibling test
    // above is the proof of the routing rule. Both are kept because they fail to different
    // mistakes: that one to wrong routing, this one to a wake lost across two real leases.
    let mut app = SuspendedApp::new().await;
    app.send(TerminalEvent::Wake).await;
    app.drained().await;

    // Resume, then immediately take the terminal again before asserting anything.
    app.resume().await;
    let second = app
        .owner
        .broker_with_timeout(Duration::from_secs(3_600))
        .acquire(LeaseReason::new("tui", "a second external editor"))
        .await
        .expect("the second lease is granted");
    wait_until(|| app.owner.suspended.load(Ordering::SeqCst)).await;
    let second_epoch = app.owner.input.state.borrow().requested;
    app.send(TerminalEvent::InputPaused(second_epoch)).await;
    wait_until(|| app.owner.input.state.borrow().acknowledged >= second_epoch).await;

    second.release();
    wait_until(|| !app.owner.suspended.load(Ordering::SeqCst)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    assert_eq!(
        app.wakes.load(Ordering::SeqCst),
        1,
        "the wake raised during the first lease must be dispatched exactly once across \
         both leases, never erased by the second lease's pause barrier"
    );
    app.shutdown().await;
}
