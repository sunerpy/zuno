//! Dialog host tests, including the one that proves an open dialog does not stall
//! event processing.

use super::*;
use crate::app::{
    App, DrawTarget, TerminalEvent, TerminalLifecycle, render_offscreen, terminal_event_channel,
};
use crate::keybind::Keymap;
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

struct FocusedBase;

impl Component for FocusedBase {
    fn render(&mut self, _frame: &mut ratatui::Frame<'_>, _area: ratatui::layout::Rect) {}

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

impl ActionComponent for FocusedBase {
    fn handle_action(&mut self, _action: &'static Definition, _event: &KeyEvent) -> EventResult {
        EventResult::IGNORED
    }

    fn focused_scopes(&self) -> Vec<&'static str> {
        vec!["history"]
    }
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
fn views_dialog_closed_host_forwards_the_bases_focused_scopes() {
    let context = ViewContext::defaults();
    let host = DialogHost::new(context, Box::new(FocusedBase));

    assert_eq!(host.focused_scopes(), ["history"]);
}

#[test]
fn views_dialog_open_host_replaces_the_bases_focused_scopes() {
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(context, Box::new(FocusedBase));
    let (dialog, _) = Probe::new("probe", "value");
    host.open(Box::new(dialog));

    assert_eq!(
        host.focused_scopes(),
        [
            "permission.prompt",
            "dialog.select",
            "dialog.prompt",
            "session"
        ]
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
        host.handle_event(&AppEvent::Engine(TurnEvent::Provider {
            step: index,
            event: zuno_llm::event::StreamEvent::TextDelta(format!("folded {index}")),
        }));
    }
    // The host has no accessor for its base, so the observation is made through the
    // rendered frame. Each event carries its own text rather than being counted by
    // message headers: consecutive assistant steps now share one header, so a header
    // count would measure the grouping rule instead of the property under test.
    let buffer = render_offscreen(&mut host, 40, 30).expect("infallible");
    let joined = rows(&buffer).join("\n");
    for index in 0..5 {
        assert!(
            joined.contains(&format!("folded {index}")),
            "engine event {index} was dropped while a dialog was open:\n{joined}"
        );
    }
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
    last_frame: Arc<Mutex<Option<ratatui::buffer::Buffer>>>,
}

impl DrawTarget for CountingTarget {
    fn draw(&mut self, root: &mut dyn Component) -> io::Result<()> {
        // `render_offscreen` already owns the `TestBackend` plumbing and its
        // infallible-error conversion; reusing it keeps one seam.
        let buffer = render_offscreen(root, 40, 30)?;
        *locked(&self.last_frame) = Some(buffer);
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
    let last_frame = Arc::new(Mutex::new(None));
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
            last_frame: Arc::clone(&last_frame),
        }),
        Arc::clone(&lifecycle) as Arc<_>,
        terminal_rx,
        engine_rx,
    );
    let task = tokio::spawn(async move { app.run().await });

    const EVENTS: usize = 10;
    for index in 0..EVENTS {
        let event = if index + 1 == EVENTS {
            TurnEvent::Provider {
                step: u32::try_from(index).expect("small"),
                event: zuno_llm::event::StreamEvent::TextDelta(format!("tail event {index}")),
            }
        } else {
            TurnEvent::AssistantMessageCreated {
                step: u32::try_from(index).expect("small"),
                message_id: format!("msg_{index}"),
            }
        };
        engine_tx
            .send(event)
            .await
            .expect("the engine channel stays open, which a stalled loop would close");
    }
    wait_until(|| observed.load(Ordering::SeqCst) == EVENTS).await;
    let tail_rendered_before_shutdown = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let tail_is_visible = locked(&last_frame)
                .as_ref()
                .is_some_and(|buffer| rows(buffer).join("\n").contains("tail event 9"));
            if tail_is_visible {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();

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
    // Per-event frame counting was only a liveness proxy; coalescing intentionally
    // retired it. The final buffer is stronger: it proves both that a frame happened
    // and that the coalesced frame includes the tail event rather than stale state.
    let rendered = locked(&last_frame);
    let joined = rendered.as_ref().map(rows).unwrap_or_default().join("\n");
    assert!(
        frames.load(Ordering::SeqCst) > 0
            && tail_rendered_before_shutdown
            && joined.contains("tail event 9"),
        "rendering stalled or lost the tail event: the final coalesced frame must contain \
         the last event's state before shutdown (frames={}, before_shutdown={}):\n{joined}",
        frames.load(Ordering::SeqCst),
        tail_rendered_before_shutdown
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
            last_frame: Arc::new(Mutex::new(None)),
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

#[test]
fn views_dialog_typed_keys_filter_the_dialog_and_never_reach_the_base() {
    // Observed on a real terminal: with the model picker open, typing a filter appended
    // to the *prompt behind it* and never filtered anything, so the visible selection
    // was whatever the unfiltered cursor happened to be on. Every keystroke did two
    // wrong things at once, and the suite could not see either.
    let context = ViewContext::defaults();
    let (shutdown, _receiver) = crate::app::terminal_event_channel();
    let mut screen = crate::views::session::SessionScreen::new(context.clone(), shutdown);
    *screen.catalog_mut() = crate::views::session::SessionCatalog {
        models: vec![
            crate::views::picker::ModelEntry {
                id: String::from("prov/alpha"),
                name: String::from("alpha"),
                provider: String::from("prov"),
            },
            crate::views::picker::ModelEntry {
                id: String::from("prov/beta"),
                name: String::from("beta"),
                provider: String::from("prov"),
            },
        ],
        ..crate::views::session::SessionCatalog::default()
    };
    let mut host = DialogHost::new(context, Box::new(screen));
    host.handle_action(
        crate::views::testkit::action("model_list"),
        &press(KeyCode::Null),
    );
    assert!(host.is_open(), "the picker did not open");

    for character in "beta".chars() {
        host.handle_event(&AppEvent::Terminal(crate::app::TerminalEvent::Input(
            crossterm::event::Event::Key(press(KeyCode::Char(character))),
        )));
    }
    let joined = rows(&render_offscreen(&mut host, 70, 20).expect("infallible")).join("\n");
    assert!(
        joined.contains("beta"),
        "the filter did not reach the dialog:\n{joined}"
    );
    assert!(
        !joined.contains("alpha"),
        "the filter did not narrow the list:\n{joined}"
    );
    // The base must not have been typed into. The dialog is drawn over the bottom of the
    // frame, so the prompt is only observable once the dialog is gone.
    assert!(host.dismiss());
    let after = rows(&render_offscreen(&mut host, 70, 20).expect("infallible")).join("\n");
    assert!(
        !after.contains("beta"),
        "typed text leaked into the prompt behind the dialog:\n{after}"
    );
}

#[test]
fn views_dialog_a_reject_box_can_still_be_typed_into() {
    // The complement: routing unclaimed keys to the dialog must not stop the one dialog
    // that legitimately collects free text from collecting it.
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(
            crate::views::message::TranscriptView::new(context.clone()),
        )),
    );
    host.open(Box::new(
        crate::views::permission::PermissionPrompt::new(
            context,
            zuno_permission::PermissionRequest {
                id: String::from("r"),
                session_id: String::from("s"),
                permission: String::from("bash"),
                patterns: Vec::new(),
                metadata: serde_json::Map::new(),
                always: Vec::new(),
                tool: None,
            },
            &serde_json::json!({"command": "make"}),
        )
        .with_reject_message(true),
    ));
    // Reach the reject stage, then type a reason.
    host.handle_action(
        crate::views::testkit::action("dialog.select.end"),
        &press(KeyCode::End),
    );
    host.handle_action(
        crate::views::testkit::action("dialog.select.submit"),
        &press(KeyCode::Enter),
    );
    for character in "nope".chars() {
        host.handle_event(&AppEvent::Terminal(crate::app::TerminalEvent::Input(
            crossterm::event::Event::Key(press(KeyCode::Char(character))),
        )));
    }
    let joined = rows(&render_offscreen(&mut host, 70, 20).expect("infallible")).join("\n");
    assert!(
        joined.contains("nope"),
        "the reject box no longer receives typed text:\n{joined}"
    );
}

// ---------------------------------------------------------------------------
// `§11.4`'s three width tiers, applied at this layer
// ---------------------------------------------------------------------------

/// A dialog that reports the tier it was built with and nothing else.
struct Tiered(&'static str, DialogWidth, ViewContext);

impl Dialog for Tiered {
    fn id(&self) -> &'static str {
        self.0
    }

    fn title(&self) -> String {
        String::from("tier")
    }

    fn width(&self) -> DialogWidth {
        self.1
    }

    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        // A full-width row of a single character, so the frame shows the dialog's own
        // measure rather than the length of whatever text happened to be in it.
        vec![padded(
            &"#".repeat(usize::from(width)),
            width,
            self.2.text(),
        )]
    }

    fn handle_action(&mut self, _action: &'static Definition, _event: &KeyEvent) -> DialogStep {
        DialogStep::Ignored
    }
}

/// The columns the body row actually occupied, from a rendered frame.
///
/// Derived from the painted `#`s rather than from `dialog_columns`, because a test that
/// recomputed the function under test would pass for any function.
fn measured_columns(host: &mut DialogHost, width: u16, height: u16) -> usize {
    let rendered = rows(&render_offscreen(host, width, height).expect("infallible"));
    rendered
        .iter()
        .map(|row| row.chars().filter(|character| *character == '#').count())
        .max()
        .unwrap_or(0)
}

#[test]
fn views_dialog_each_tier_gets_its_own_width_on_a_wide_terminal() {
    for (tier, expected) in [
        (DialogWidth::Medium, 60_u16),
        (DialogWidth::Large, 88),
        (DialogWidth::XLarge, 116),
    ] {
        assert_eq!(tier.columns(), expected, "{tier:?} moved off its tier");
        let (mut host, context) = host();
        host.open(Box::new(Tiered("tier", tier, context)));
        // Two columns of the tier are the left rule and the right inner margin, so the
        // body is the tier less two.
        assert_eq!(
            measured_columns(&mut host, 200, 12),
            usize::from(expected - 2),
            "{tier:?} did not lay out at {expected} columns on a 200-column terminal"
        );
    }
}

#[test]
fn views_dialog_a_tier_is_centred_rather_than_flush_left() {
    let (mut host, context) = host();
    host.open(Box::new(Tiered("tier", DialogWidth::Medium, context)));
    let rendered = rows(&render_offscreen(&mut host, 200, 12).expect("infallible"));
    let row = rendered
        .iter()
        .find(|row| row.contains('#'))
        .expect("a body row");
    // The first painted column is the left rule, so it is the dialog's own left edge.
    let lead = row.len() - row.trim_start().len();
    assert_eq!(
        lead,
        usize::from((200_u16 - 60) / 2),
        "a 60-column dialog is not centred in a 200-column frame: {row:?}"
    );
}

#[test]
fn views_dialog_a_terminal_narrower_than_the_tier_converges_to_the_gutter() {
    // `§11.4`: `min(tier, term_width - 4)`. Asserted for every tier at a width each one
    // cannot have, so the fallback is not being read off whichever tier happens to fit.
    for (tier, width) in [
        (DialogWidth::Medium, 50_u16),
        (DialogWidth::Large, 70),
        (DialogWidth::XLarge, 100),
    ] {
        let (mut host, context) = host();
        host.open(Box::new(Tiered("tier", tier, context)));
        assert_eq!(
            measured_columns(&mut host, width, 12),
            usize::from(width - DIALOG_GUTTER - 2),
            "{tier:?} did not converge to term_width - {DIALOG_GUTTER} at {width} columns"
        );
    }
}

#[test]
fn views_dialog_below_the_smallest_tier_the_tier_is_abandoned_entirely() {
    // The case `§11.4` does not name. There is no tier under 60, so every dialog takes
    // the gutter width — which is what keeps a 60-column `Rect` from being drawn into a
    // 20-column frame.
    for width in [20_u16, 30, 40, 59] {
        for tier in [DialogWidth::Medium, DialogWidth::Large, DialogWidth::XLarge] {
            assert_eq!(
                dialog_columns(tier, width),
                width - DIALOG_GUTTER,
                "{tier:?} kept its tier on a {width}-column terminal"
            );
            let (mut host, context) = host();
            host.open(Box::new(Tiered("tier", tier, context)));
            assert_eq!(
                measured_columns(&mut host, width, 10),
                usize::from(width - DIALOG_GUTTER - 2),
                "{tier:?} did not shrink to fit a {width}-column frame"
            );
        }
    }
}

#[test]
fn views_dialog_keeps_a_visible_body_when_the_gutter_would_take_everything() {
    // At four columns or fewer the gutter is abandoned too, because a zero-width dialog
    // is not a small dialog — it is an invisible modal that still owns the keyboard.
    for width in 1..=DIALOG_GUTTER {
        assert_eq!(
            dialog_columns(DialogWidth::Medium, width),
            width,
            "a {width}-column terminal produced an invisible dialog"
        );
    }
    assert_eq!(dialog_columns(DialogWidth::Medium, 5), 1);
}

#[test]
fn views_dialog_does_not_panic_on_a_degenerate_frame() {
    // `§11.6`'s acceptance case and the sizes around it. Before the height was clamped
    // back down after its `max(3)`, a two-row frame produced a three-row region and the
    // fill walked off the end of the buffer.
    for (width, height) in [(20_u16, 10_u16), (1, 1), (2, 2), (4, 3), (200, 1), (10, 2)] {
        let (mut host, context) = host();
        host.open(Box::new(Tiered("tier", DialogWidth::XLarge, context)));
        let _no_panic = render_offscreen(&mut host, width, height).expect("infallible");
    }
}

#[test]
fn views_dialog_the_default_tier_is_large_and_the_two_reference_panels_are_xlarge() {
    // The tier assignment is a table in the plan, and a dialog that silently took the
    // default would be a row of that table quietly changed. Asserted through the trait so
    // a new dialog inherits the check.
    let context = ViewContext::defaults();
    let (probe, _) = Probe::new("probe", "body");
    assert_eq!(probe.width(), DialogWidth::Large);
    assert_eq!(
        crate::views::help::HelpView::new(
            context.clone(),
            &Keymap::defaults().expect("the table builds")
        )
        .width(),
        DialogWidth::XLarge
    );
    assert_eq!(
        crate::views::permission::PermissionPrompt::new(
            context,
            zuno_permission::PermissionRequest {
                id: String::from("r"),
                session_id: String::from("s"),
                permission: String::from("bash"),
                patterns: Vec::new(),
                metadata: serde_json::Map::new(),
                always: Vec::new(),
                tool: None,
            },
            &serde_json::json!({"command": "make"}),
        )
        .width(),
        DialogWidth::XLarge
    );
}

// ---------------------------------------------------------------------------
// Scope reachability, for dialogs rather than for the base screen
// ---------------------------------------------------------------------------

/// One instance of every dialog form the base layer ships, plus the probe.
///
/// Rebuilt per action because `handle_action` takes `&mut self` and several of these
/// resolve, so one shared instance would answer the first action and then be a closed
/// dialog answering nothing.
fn base_forms(context: &ViewContext) -> Vec<Box<dyn Dialog>> {
    vec![
        Box::new(crate::views::basics::ConfirmDialog::new(
            context.clone(),
            "confirm",
            "heading",
            "body",
        )),
        Box::new(crate::views::basics::AlertDialog::new(
            context.clone(),
            "alert",
            "heading",
            "body",
        )),
        Box::new(crate::views::basics::PromptDialog::new(
            context.clone(),
            "prompt",
            "heading",
            "text",
        )),
        // The two `§8.7` panels join the derived guard rather than getting one of their
        // own. Both claim their *opening* action as a close — `status_view` and
        // `debug_view` — which only resolves while the panel is open if the screen's
        // static chain carries those scopes, and that registration is the exact thing
        // `editor_open` was missing.
        Box::new(crate::views::diagnostics::StatusPanel::new(
            context.clone(),
            Vec::new(),
        )),
        Box::new(crate::views::diagnostics::DebugPanel::new(
            context.clone(),
            crate::views::diagnostics::DebugFacts::default(),
        )),
    ]
}

#[test]
fn views_dialog_every_action_a_base_form_consumes_resolves_while_it_is_open() {
    // The dialog-side counterpart of `session_tests`'
    // `every_action_the_screen_consumes_lives_in_a_scope_it_resolves`, and it exists for
    // the same reason: that guard derives its set from `SessionScreen::handle_action`, so
    // an action a *dialog* claims is covered by neither it nor the two hand-kept lists.
    // A dialog whose scope `DialogHost::focused_scopes` does not carry is a footer
    // advertising a key that resolves to something else entirely — `picker.rs`'s missing
    // `session_interrupt` arm was exactly that, an `esc cancel` hint naming a way out
    // that did not exist.
    //
    // The set is derived from the dialog, not listed: whatever a form consumes has to be
    // pressable while that form is open.
    let mut keymap = Keymap::defaults().expect("the shipped table builds");
    let context = ViewContext::defaults();
    let mut offences = Vec::new();
    let mut checked = 0;

    for (index, id) in ["confirm", "alert", "prompt", "status", "debug"]
        .into_iter()
        .enumerate()
    {
        // The chain a key actually resolves against while this dialog owns the keyboard:
        // the host's own list first, then the screen's static one. Asked of the host so
        // the test cannot disagree with production about what is in scope.
        let scopes = {
            let mut host = DialogHost::new(context.clone(), Box::new(FocusedBase));
            host.open(base_forms(&context).into_iter().nth(index).expect("a form"));
            let mut scopes = ActionComponent::focused_scopes(&host)
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>();
            scopes.extend(crate::views::session::scopes());
            scopes
        };

        for definition in crate::keybind::DEFINITIONS {
            let sequences = keymap.sequences(definition.name);
            let Some(spelling) = sequences.first() else {
                continue;
            };
            let consumed = base_forms(&context)
                .into_iter()
                .nth(index)
                .expect("a form")
                .handle_action(definition, &press(KeyCode::Null))
                != DialogStep::Ignored;
            if !consumed {
                continue;
            }
            checked += 1;
            // `app_exit` is the documented exception, and it is the same one
            // `session_tests`' guard carves out. `ctrl+c` is claimed by the `input` scope
            // before `app`, so it resolves to `input_clear` and never to `app_exit` — and
            // that is *why* `DialogHost::handle_action` forwards exactly one class of
            // ignored action, the class whose chord the table binds to `APP_EXIT`. The
            // dialog's own arm is for a chain where the resolution does land on it. Held
            // to the resolution rule below, this row would demand the host stop
            // compensating from the physical chord, which is what once left a user unable
            // to leave a raw-mode terminal at all.
            if definition.name == crate::keybind::APP_EXIT {
                continue;
            }
            if !scopes.iter().any(|scope| scope == definition.scope) {
                offences.push(format!(
                    "`{id}` acts on {} but its scope `{}` is not in the chain a dialog \
                     resolves against",
                    definition.name, definition.scope
                ));
                continue;
            }
            // The shadowing half. Two dialog rows share `return` — `dialog.select.submit`
            // and `dialog.prompt.submit` — so demanding that each resolve to *itself*
            // would fail for a collision that is upstream's and cannot be edited away.
            // The property that matters is weaker and sufficient: the action the chord
            // really resolves to is one this same dialog also acts on, so the key does
            // what its footer says.
            let Ok(chord) = Chord::parse(spelling) else {
                continue;
            };
            let resolved = match keymap.resolve(
                &scopes.iter().map(String::as_str).collect::<Vec<_>>(),
                chord,
                std::time::Instant::now(),
            ) {
                crate::keybind::Resolution::Action { definition, .. } => Some(definition),
                _ => None,
            };
            let Some(resolved) = resolved else {
                offences.push(format!(
                    "`{id}` acts on {} (`{spelling}`) but that chord resolves to nothing",
                    definition.name
                ));
                continue;
            };
            let honoured = base_forms(&context)
                .into_iter()
                .nth(index)
                .expect("a form")
                .handle_action(resolved, &press(KeyCode::Null))
                != DialogStep::Ignored;
            if !honoured {
                offences.push(format!(
                    "`{id}` advertises {} on `{spelling}`, but that chord resolves to {} \
                     which the dialog ignores — the key does nothing",
                    definition.name, resolved.name
                ));
            }
        }
    }
    assert!(
        checked >= 24,
        "the dialog scope guard checked only {checked} actions across five forms, so it is \
         not finding what it exists to check"
    );
    assert!(offences.is_empty(), "{}", offences.join("\n"));
}
