//! Dialog host tests, including the one that proves an open dialog does not stall
//! event processing.

use super::*;
use crate::app::{
    App, DrawTarget, TerminalEvent, TerminalLifecycle, render_offscreen, terminal_event_channel,
};
use crate::views::message::TranscriptView;
use crate::views::testkit::{action, press, rows};
use crossterm::event::{Event as CrosstermEvent, KeyCode};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::mpsc;
use zuno_engine::r#loop::TurnEvent;

fn locked<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A dialog that records what it saw and resolves on submit.
struct Probe {
    id: &'static str,
    body: String,
    actions: Arc<Mutex<Vec<&'static str>>>,
}

impl Probe {
    fn new(id: &'static str, body: &str) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
        let actions = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                id,
                body: body.to_owned(),
                actions: Arc::clone(&actions),
            },
            actions,
        )
    }
}

impl Dialog for Probe {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> String {
        format!("probe {}", self.id)
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        vec![padded(&self.body, width, self.context_style())]
    }

    fn handle_action(&mut self, action: &'static Definition, _event: &KeyEvent) -> DialogStep {
        locked(&self.actions).push(action.name);
        match action.name {
            "dialog.select.submit" => DialogStep::Resolved(DialogOutcome::Selected {
                dialog: self.id,
                value: self.body.clone(),
            }),
            "app_exit" => DialogStep::Resolved(DialogOutcome::Cancelled),
            "dialog.select.next" => DialogStep::Redraw,
            _ => DialogStep::Ignored,
        }
    }
}

impl Probe {
    /// The probe paints from the default palette like every other view, so the
    /// palette-discipline scan has nothing special to say about it.
    fn context_style(&self) -> ratatui::style::Style {
        ViewContext::defaults().text()
    }
}

fn host() -> (DialogHost, ViewContext) {
    let context = ViewContext::defaults();
    let base = ObservedBase::new(TranscriptView::new(context.clone()));
    (DialogHost::new(context.clone(), Box::new(base)), context)
}

// ---------------------------------------------------------------------------
// The stack
// ---------------------------------------------------------------------------

#[test]
fn views_dialog_host_starts_closed_and_reports_its_stack() {
    let (mut host, _) = host();
    assert!(!host.is_open());
    assert_eq!(host.depth(), 0);
    assert_eq!(host.active(), None);

    let (first, _) = Probe::new("first", "one");
    host.open(Box::new(first));
    let (second, _) = Probe::new("second", "two");
    host.open(Box::new(second));
    assert_eq!(host.depth(), 2);
    assert_eq!(
        host.active(),
        Some("second"),
        "the newest dialog does not have the keyboard"
    );
}

#[test]
fn views_dialog_only_the_top_of_the_stack_receives_actions() {
    let (mut host, _) = host();
    let (lower, lower_actions) = Probe::new("lower", "one");
    host.open(Box::new(lower));
    let (upper, upper_actions) = Probe::new("upper", "two");
    host.open(Box::new(upper));

    host.handle_action(action("dialog.select.next"), &press(KeyCode::Down));
    assert_eq!(locked(&upper_actions).as_slice(), ["dialog.select.next"]);
    assert!(
        locked(&lower_actions).is_empty(),
        "a covered dialog received an action"
    );
}

#[test]
fn views_dialog_resolution_pops_the_stack_and_queues_the_outcome() {
    let (mut host, _) = host();
    let (probe, _) = Probe::new("probe", "value");
    host.open(Box::new(probe));

    let result = host.handle_action(action("dialog.select.submit"), &press(KeyCode::Enter));
    assert!(result.redraw);
    assert!(!host.is_open(), "the resolved dialog stayed on the stack");
    assert_eq!(
        host.drain_outcomes(),
        vec![(
            "probe",
            DialogOutcome::Selected {
                dialog: "probe",
                value: String::from("value"),
            }
        )]
    );
    assert!(
        host.drain_outcomes().is_empty(),
        "draining twice returned the same outcome twice"
    );
}

#[test]
fn views_dialog_an_unhandled_action_does_not_reach_the_base() {
    // A modal owns the keyboard. Forwarding an unhandled action would let a global
    // binding fire while a permission prompt is up.
    let (mut host, _) = host();
    let (probe, actions) = Probe::new("probe", "value");
    host.open(Box::new(probe));
    let result = host.handle_action(action("session_new"), &press(KeyCode::Char('n')));
    assert!(
        result.handled,
        "an action a dialog ignored was reported as unhandled, so the base would see it"
    );
    assert!(!result.redraw);
    assert_eq!(locked(&actions).as_slice(), ["session_new"]);
}

#[test]
fn views_dialog_actions_reach_the_base_once_the_stack_is_empty() {
    let (mut host, _) = host();
    let result = host.handle_action(action("session_new"), &press(KeyCode::Char('n')));
    assert_eq!(
        result,
        EventResult::IGNORED,
        "with no dialog open the base did not get the action"
    );
}

#[test]
fn views_dialog_dismiss_closes_without_an_outcome() {
    let (mut host, _) = host();
    let (probe, _) = Probe::new("probe", "value");
    host.open(Box::new(probe));
    assert!(host.dismiss());
    assert!(!host.is_open());
    assert!(host.drain_outcomes().is_empty());
    assert!(!host.dismiss(), "dismissing an empty stack claimed success");
}

// ---------------------------------------------------------------------------
// The base keeps receiving events
// ---------------------------------------------------------------------------

#[test]
fn views_dialog_base_still_receives_engine_events_while_a_dialog_is_open() {
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(TranscriptView::new(context))),
    );
    let (probe, _) = Probe::new("probe", "value");
    host.open(Box::new(probe));

    for index in 0..5 {
        host.handle_event(&AppEvent::Engine(TurnEvent::AssistantMessageCreated {
            step: index,
            message_id: format!("msg_{index}"),
        }));
    }
    // The host has no accessor for its base, so the observation is made through the
    // rendered frame: five assistant headers means five events were folded while the
    // dialog was up.
    let buffer = render_offscreen(&mut host, 40, 30).expect("infallible");
    let headers = rows(&buffer)
        .iter()
        .filter(|row| row.contains("Assistant"))
        .count();
    assert_eq!(
        headers, 5,
        "engine events were dropped while a dialog was open"
    );
}

#[test]
fn views_dialog_renders_over_a_live_base() {
    let (mut host, _) = host();
    host.handle_event(&AppEvent::Engine(TurnEvent::AssistantMessageCreated {
        step: 1,
        message_id: String::from("msg"),
    }));
    let (probe, _) = Probe::new("probe", "dialog body");
    host.open(Box::new(probe));
    let joined = rows(&render_offscreen(&mut host, 40, 12).expect("infallible")).join("\n");
    assert!(
        joined.contains("Assistant"),
        "the base vanished behind the dialog, so the prompt cannot be judged:\n{joined}"
    );
    assert!(
        joined.contains("dialog body"),
        "the dialog body is missing:\n{joined}"
    );
    assert!(
        joined.contains("probe probe"),
        "the dialog title is missing:\n{joined}"
    );
    assert!(
        joined.contains("move") && joined.contains("select") && joined.contains("cancel"),
        "the default footer hints are missing:\n{joined}"
    );
}

// ---------------------------------------------------------------------------
// The no-stall proof, against the real event loop
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CountingLifecycle {
    active: AtomicBool,
}

impl TerminalLifecycle for CountingLifecycle {
    fn enter(&self) -> io::Result<()> {
        self.active.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn restore(&self) -> io::Result<()> {
        self.active.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

/// A draw target that counts frames and never touches a terminal.
struct CountingTarget {
    frames: Arc<AtomicUsize>,
}

impl DrawTarget for CountingTarget {
    fn draw(&mut self, root: &mut dyn Component) -> io::Result<()> {
        // `render_offscreen` already owns the `TestBackend` plumbing and its
        // infallible-error conversion; reusing it keeps one seam.
        render_offscreen(root, 40, 10)?;
        self.frames.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn resize(&mut self, _width: u16, _height: u16) -> io::Result<()> {
        Ok(())
    }
}

/// A component that counts engine events, shared with the test.
struct SharedCounter {
    engine: Arc<AtomicUsize>,
    inner: TranscriptView,
}

impl Component for SharedCounter {
    fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect) {
        self.inner.render(frame, area);
    }

    fn handle_event(&mut self, event: &AppEvent) -> EventResult {
        if matches!(event, AppEvent::Engine(_)) {
            self.engine.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.handle_event(event)
    }
}

impl ActionComponent for SharedCounter {
    fn handle_action(&mut self, _action: &'static Definition, _event: &KeyEvent) -> EventResult {
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
    .expect("the event loop makes progress while a dialog is open");
}

/// The plan's failure scenario: "a dialog does not stall event processing".
///
/// Written so a blocking implementation fails it. Ten engine events are pushed while
/// a dialog is open; the assertion is that all ten were **observed by the base**, and
/// the five-second timeout inside [`wait_until`] is what turns a blocking
/// `handle_event` into a failure rather than a hang.
#[tokio::test]
async fn views_dialog_does_not_stall_event_processing() {
    let lifecycle = Arc::new(CountingLifecycle::default());
    lifecycle.enter().expect("the fake lifecycle enters");
    let observed = Arc::new(AtomicUsize::new(0));
    let frames = Arc::new(AtomicUsize::new(0));
    let context = ViewContext::defaults();

    let base = SharedCounter {
        engine: Arc::clone(&observed),
        inner: TranscriptView::new(context.clone()),
    };
    let mut host = DialogHost::new(context, Box::new(base));
    let (probe, _) = Probe::new("blocker", "still alive");
    host.open(Box::new(probe));
    assert!(host.is_open());

    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(4);
    let (mut app, _owner) = App::new(
        Box::new(host),
        Box::new(CountingTarget {
            frames: Arc::clone(&frames),
        }),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let task = tokio::spawn(async move { app.run().await });

    const EVENTS: usize = 10;
    for index in 0..EVENTS {
        engine_tx
            .send(TurnEvent::AssistantMessageCreated {
                step: u32::try_from(index).expect("small"),
                message_id: format!("msg_{index}"),
            })
            .await
            .expect("the engine channel stays open, which a stalled loop would close");
    }
    wait_until(|| observed.load(Ordering::SeqCst) == EVENTS).await;

    // Terminal input also keeps flowing: a key the dialog does not claim reaches the
    // loop and is dispatched rather than queued behind a modal.
    terminal_tx
        .send(TerminalEvent::Input(CrosstermEvent::Key(press(
            KeyCode::F(9),
        ))))
        .await
        .expect("the terminal channel stays open");

    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("the terminal channel stays open");
    task.await
        .expect("the event loop task does not panic")
        .expect("the event loop exits cleanly");

    assert_eq!(
        observed.load(Ordering::SeqCst),
        EVENTS,
        "the base did not observe every engine event delivered while a dialog was open"
    );
    assert!(
        frames.load(Ordering::SeqCst) >= EVENTS,
        "the loop drew {} frames for {EVENTS} redraw-worthy events, so rendering stalled",
        frames.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn views_dialog_stack_survives_a_resize_without_losing_events() {
    let lifecycle = Arc::new(CountingLifecycle::default());
    lifecycle.enter().expect("enters");
    let observed = Arc::new(AtomicUsize::new(0));
    let context = ViewContext::defaults();
    let base = SharedCounter {
        engine: Arc::clone(&observed),
        inner: TranscriptView::new(context.clone()),
    };
    let mut host = DialogHost::new(context, Box::new(base));
    let (probe, _) = Probe::new("probe", "body");
    host.open(Box::new(probe));

    let (terminal_tx, terminal_rx) = terminal_event_channel();
    let (engine_tx, engine_rx) = mpsc::channel(2);
    let (mut app, _owner) = App::new(
        Box::new(host),
        Box::new(CountingTarget {
            frames: Arc::new(AtomicUsize::new(0)),
        }),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let task = tokio::spawn(async move { app.run().await });

    terminal_tx
        .send(TerminalEvent::Resize {
            width: 20,
            height: 6,
        })
        .await
        .expect("open");
    engine_tx
        .send(TurnEvent::TurnStarted {
            session_id: String::from("ses"),
        })
        .await
        .expect("open");
    wait_until(|| observed.load(Ordering::SeqCst) == 1).await;

    terminal_tx
        .send(TerminalEvent::Shutdown)
        .await
        .expect("open");
    task.await.expect("no panic").expect("clean exit");
}

#[test]
fn views_dialog_pending_leader_sequence_is_recorded_for_which_key() {
    let (mut host, _) = host();
    let (probe, _) = Probe::new("probe", "body");
    host.open(Box::new(probe));
    let chord = crate::keybind::Chord::parse("ctrl+x").expect("a valid spelling");
    let result = host.pending_changed(&[chord]);
    assert!(result.redraw);
    assert_eq!(host.pending().len(), 1);
}

#[test]
fn views_observed_base_counts_both_event_families() {
    let mut base = ObservedBase::new(TranscriptView::new(ViewContext::defaults()));
    base.handle_event(&AppEvent::Engine(TurnEvent::TurnStarted {
        session_id: String::from("s"),
    }));
    base.handle_event(&AppEvent::Terminal(TerminalEvent::Resize {
        width: 1,
        height: 1,
    }));
    assert_eq!(base.engine_events(), 1);
    assert_eq!(base.terminal_events(), 1);
    assert!(base.inner().transcript().is_running());
}
