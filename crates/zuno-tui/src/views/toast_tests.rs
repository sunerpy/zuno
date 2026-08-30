//! The toast's contracts: it appears, it expires without a keypress, and it is on top.

use super::*;
use crate::app::{AppEvent, Component, EventResult, TerminalEvent, render_offscreen};
use crate::keybind::{ActionComponent, Definition};
use crate::views::basics::AlertDialog;
use crate::views::dialog::{DialogHost, ObservedBase};
use crate::views::message::TranscriptView;
use crate::views::testkit::rows;
use crossterm::event::KeyEvent;

/// A component that renders a toast layer and nothing else.
///
/// The layer is not a `Component` — it is painted by its host into a computed region, the
/// same arrangement a `Dialog` has — so a frame assertion needs something to hold it.
struct Bare(ToastLayer);

impl Component for Bare {
    fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect) {
        self.0.render(frame.buffer_mut(), area);
    }

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

fn frame_of(layer: ToastLayer, width: u16, height: u16) -> Vec<String> {
    let mut bare = Bare(layer);
    rows(&render_offscreen(&mut bare, width, height).expect("infallible"))
}

#[test]
fn views_toast_starts_empty_and_paints_nothing() {
    let layer = ToastLayer::new(ViewContext::defaults());
    assert!(!layer.is_showing());
    assert_eq!(layer.current(), None);
    assert!(
        frame_of(layer, 40, 4).iter().all(String::is_empty),
        "an empty toast layer painted something"
    );
}

#[test]
fn views_toast_paints_in_the_top_right_corner() {
    let mut layer = ToastLayer::new(ViewContext::defaults());
    layer.show(Toast::success("copied 41 characters to the clipboard"));
    let rendered = frame_of(layer, 60, 5);
    assert!(
        rendered[0].contains("copied 41 characters"),
        "the toast is not on the first row: {rendered:?}"
    );
    assert!(
        rendered[1..].iter().all(String::is_empty),
        "the toast spilled onto a second row: {rendered:?}"
    );
    // Right-aligned. Two halves, because either alone is satisfiable by a left-aligned
    // toast: nothing is painted after the notice's own trailing pad, and the notice starts
    // well into the right half of the frame. A toast at column zero would sit over
    // whatever the transcript is printing on its first row.
    let row = &rendered[0];
    assert!(
        row.trim_end().ends_with("clipboard"),
        "something is painted to the right of the notice: {row:?}"
    );
    let lead = row.len() - row.trim_start().len();
    assert!(
        lead > 15,
        "the notice is not right-aligned; it starts at column {lead}: {row:?}"
    );
}

#[test]
fn views_toast_carries_its_level_as_a_glyph_and_a_palette_colour() {
    let context = ViewContext::defaults();
    for (level, glyph, expected) in [
        (ToastLevel::Info, "·", context.muted().fg),
        (ToastLevel::Success, "✓", context.success().fg),
        (ToastLevel::Warning, "!", context.warning().fg),
        (ToastLevel::Error, "✗", context.error().fg),
    ] {
        let mut layer = ToastLayer::new(context.clone());
        layer.show(Toast::new(level, "message"));
        let buffer = {
            let mut bare = Bare(layer);
            render_offscreen(&mut bare, 40, 3).expect("infallible")
        };
        let row = (0..40).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
        assert!(
            row.contains(glyph),
            "{level:?} did not render {glyph}: {row}"
        );
        let cell = (0..40)
            .find(|x| buffer[(*x, 0)].symbol() == glyph)
            .map(|x| buffer[(x, 0)].clone())
            .expect("the glyph's cell");
        assert_eq!(
            Some(cell.fg),
            expected,
            "{level:?} did not use its palette colour"
        );
        assert_eq!(
            cell.bg,
            ratatui::style::Color::from(context.palette().background_element),
            "{level:?} did not sit on the inset-element background"
        );
        if level == ToastLevel::Success {
            let text = (0..40)
                .find(|x| buffer[(*x, 0)].symbol() == "m")
                .map(|x| buffer[(x, 0)].clone())
                .expect("the success toast body");
            assert_eq!(
                Some(text.fg),
                context.text().fg,
                "a success toast painted its whole sentence green instead of only the marker"
            );
        }
    }
}

#[test]
fn views_toast_expires_on_its_own_after_the_ttl_with_no_keypress() {
    // The property: nothing is pressed between showing and expiring.
    let mut layer = ToastLayer::new(ViewContext::defaults());
    let shown = std::time::Instant::now();
    layer.show_at(Toast::info("this goes away"), shown);
    assert!(
        !layer.prune(shown),
        "the toast expired the instant it opened"
    );
    assert!(
        !layer.prune(shown + TOAST_TTL - std::time::Duration::from_millis(1)),
        "the toast expired before its five seconds were up"
    );
    assert!(layer.is_showing());
    assert!(
        layer.prune(shown + TOAST_TTL),
        "the toast outlived its own TTL"
    );
    assert!(!layer.is_showing());
    assert!(
        !layer.prune(shown + TOAST_TTL),
        "pruning an already-empty slot claimed the screen changed"
    );
}

#[test]
fn views_toast_keeps_actionable_warnings_visible_longer() {
    let mut layer = ToastLayer::new(ViewContext::defaults());
    let shown = std::time::Instant::now();
    layer.show_at(Toast::warning("choose another model"), shown);

    assert!(
        !layer.prune(shown + TOAST_TTL),
        "an actionable warning inherited the short confirmation timeout"
    );
    assert!(
        !layer.prune(shown + TOAST_ATTENTION_TTL - std::time::Duration::from_millis(1)),
        "the warning disappeared before its reading and selection window ended"
    );
    assert!(layer.prune(shown + TOAST_ATTENTION_TTL));
}

#[test]
fn views_toast_can_keep_warning_semantics_with_the_short_notice_ttl() {
    let mut layer = ToastLayer::new(ViewContext::defaults());
    let shown = std::time::Instant::now();
    let toast = Toast::warning_for("unknown command", TOAST_TTL);
    assert_eq!(toast.level(), ToastLevel::Warning);
    layer.show_at(toast, shown);

    assert!(
        !layer.prune(shown + TOAST_TTL - std::time::Duration::from_millis(1)),
        "the short warning expired before its explicit TTL"
    );
    assert!(
        layer.prune(shown + TOAST_TTL),
        "an explicitly short warning inherited the attention timeout"
    );
}

#[test]
fn views_toast_wraps_a_long_notice_without_losing_its_actionable_tail() {
    let text = "myopenai/us.anthropic.claude-opus-5 does not support selectable reasoning \
                effort. Choose a reasoning-capable model to change the effort level.";
    let mut layer = ToastLayer::new(ViewContext::defaults());
    layer.show(Toast::warning(text));

    let rendered = frame_of(layer, 80, 8);
    let joined = rendered.join("\n");
    let normalized = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        rendered
            .iter()
            .take(4)
            .filter(|row| !row.is_empty())
            .count()
            >= 2,
        "the long notice was still forced through one clipped row: {rendered:?}"
    );
    for needle in [
        "myopenai/us.anthropic.claude-opus-5",
        "does not support selectable reasoning",
        "Choose a reasoning-capable model",
        "change the effort level.",
    ] {
        assert!(
            normalized.contains(needle),
            "the wrapped notice lost `{needle}`:\n{joined}"
        );
    }
}

#[test]
fn views_toast_that_has_expired_is_not_drawn_even_if_nothing_pruned_it() {
    // The belt to the wake's braces: a dropped `Wake` must not leave a stale notice on
    // screen, so the frame prunes too. `show_at` in the past is the same state a dropped
    // wake leaves behind.
    let mut layer = ToastLayer::new(ViewContext::defaults());
    layer.show_at(
        Toast::info("stale"),
        std::time::Instant::now() - TOAST_TTL - std::time::Duration::from_secs(1),
    );
    assert!(
        frame_of(layer, 40, 3).iter().all(String::is_empty),
        "an expired toast was still painted"
    );
}

#[test]
fn views_toast_replaces_the_previous_one_and_restarts_its_clock() {
    let mut layer = ToastLayer::new(ViewContext::defaults());
    let first = std::time::Instant::now();
    layer.show_at(Toast::info("first"), first);
    let second = first + TOAST_TTL - std::time::Duration::from_millis(1);
    layer.show_at(Toast::success("second"), second);
    assert_eq!(layer.current().map(Toast::text), Some("second"));
    assert!(
        !layer.prune(first + TOAST_TTL),
        "the replacement inherited the first toast's deadline"
    );
    assert!(layer.prune(second + TOAST_TTL));
}

#[test]
fn views_toast_is_clamped_to_the_medium_tier_and_then_to_the_frame() {
    let mut layer = ToastLayer::new(ViewContext::defaults());
    layer.show(Toast::error("e".repeat(400)));
    // Every wrapped row starts inside the rightmost 60 columns. The first row may
    // contain only the level glyph when the following unbreakable run fills later
    // rows, so no single row is required to occupy the whole region.
    let rendered = frame_of(layer, 200, 3);
    let boundary = 200 - usize::from(TOAST_MAX_WIDTH);
    let visible = rendered
        .iter()
        .filter(|row| !row.trim().is_empty())
        .collect::<Vec<_>>();
    assert!(
        visible.len() > 1,
        "the long notice did not wrap: {rendered:?}"
    );
    for row in visible {
        let lead = display_width(row) - display_width(row.trim_start());
        assert!(
            lead >= boundary,
            "the notice grew left of the medium tier: {row:?}"
        );
        assert!(
            display_width(row) <= 200,
            "the notice overflowed the frame: {row:?}"
        );
    }

    let mut narrow = ToastLayer::new(ViewContext::defaults());
    narrow.show(Toast::error("e".repeat(400)));
    let cramped = frame_of(narrow, 12, 3);
    assert!(
        cramped
            .iter()
            .all(|row| display_width(row) <= usize::from(12_u16)),
        "the notice overflowed a 12-column frame: {cramped:?}"
    );
}

#[test]
fn views_toast_survives_a_degenerate_frame() {
    for (width, height) in [(0_u16, 0_u16), (1, 1), (20, 10), (4, 2)] {
        let mut layer = ToastLayer::new(ViewContext::defaults());
        layer.show(Toast::warning("something happened"));
        let _no_panic = frame_of(layer, width.max(1), height.max(1));
    }
}

// ---------------------------------------------------------------------------
// Through the host, which is the only way production reaches it
// ---------------------------------------------------------------------------

/// A base that raises one toast when it sees any action.
struct Raiser(Option<Toast>);

impl Component for Raiser {
    fn render(&mut self, _frame: &mut ratatui::Frame<'_>, _area: ratatui::layout::Rect) {}

    fn handle_event(&mut self, _event: &AppEvent) -> EventResult {
        EventResult::IGNORED
    }
}

impl ActionComponent for Raiser {
    fn handle_action(&mut self, _action: &'static Definition, _event: &KeyEvent) -> EventResult {
        EventResult::IGNORED
    }

    fn drain_toasts(&mut self) -> Vec<Toast> {
        self.0.take().into_iter().collect()
    }
}

#[test]
fn views_toast_host_raises_what_the_base_asked_for_and_reports_a_redraw() {
    let context = ViewContext::defaults();
    let mut host = DialogHost::new(
        context,
        Box::new(Raiser(Some(Toast::success("copied to the clipboard")))),
    );
    let result = host.handle_action(
        crate::views::testkit::action("input_clear"),
        &crate::views::testkit::press(crossterm::event::KeyCode::Null),
    );
    assert!(
        result.redraw,
        "a raised toast did not ask for the frame that would show it"
    );
    let rendered = rows(&render_offscreen(&mut host, 60, 6).expect("infallible"));
    assert!(
        rendered[0].contains("copied to the clipboard"),
        "the base's toast never reached the screen: {rendered:?}"
    );
}

#[test]
fn views_toast_is_drawn_above_an_open_dialog() {
    // `§11.4`'s layer order, and the reason it is specified: a copy made while a picker
    // is open is confirmed *over* the modal, because the transcript behind it cannot be
    // read either. This is also the shape of the recorded false pass — a dialog covering
    // the row under test — so the assertion is that the toast covers the dialog and not
    // the other way round.
    let context = ViewContext::defaults();
    let base = ObservedBase::new(TranscriptView::new(context.clone()));
    let mut host = DialogHost::new(context.clone(), Box::new(base));
    host.open(Box::new(AlertDialog::new(
        context,
        "alert.test",
        "Under the toast",
        "aaaa ".repeat(200),
    )));
    let before = rows(&render_offscreen(&mut host, 70, 12).expect("infallible"));
    // The precondition, and it is not decoration: the recorded false pass in this project
    // was a frame assertion that a dialog had covered, so a layer-order test has to prove
    // the dialog owns the row *before* claiming the toast took it.
    assert!(
        before[0].contains("Under the toast"),
        "the dialog is not covering the top row, so this test would prove nothing: {before:?}"
    );

    host.toasts_mut().show(Toast::success("copied"));
    let after = rows(&render_offscreen(&mut host, 70, 12).expect("infallible"));
    assert!(
        after[0].contains("copied"),
        "the toast rendered under the dialog: {after:?}"
    );
    assert!(
        after[1..].iter().any(|row| row.contains("aaaa")),
        "the toast wiped out the dialog instead of sitting over one row of it: {after:?}"
    );
    assert!(
        after[0].contains("Under the toast"),
        "the toast is wider than it needs to be and covered the dialog's whole title row: \
         {after:?}"
    );
}

#[test]
fn views_toast_a_wake_after_the_ttl_clears_the_slot_and_asks_for_a_frame() {
    // How expiry actually reaches the screen in production: the armed deadline sends one
    // `Wake`, the host prunes on it, and the redraw it reports is the frame the toast
    // leaves on. Placing the toast in the past is the same state the loop is in when the
    // wake arrives.
    let context = ViewContext::defaults();
    let base = ObservedBase::new(TranscriptView::new(context.clone()));
    let mut host = DialogHost::new(context, Box::new(base));
    host.toasts_mut().show_at(
        Toast::info("expiring"),
        std::time::Instant::now() - TOAST_TTL,
    );
    let result = host.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
    assert!(
        result.redraw,
        "the wake did not ask for the frame that removes the toast"
    );
    assert!(
        !host.toasts_mut().is_showing(),
        "the wake did not clear the expired slot"
    );
}

#[test]
fn views_toast_a_wake_before_the_ttl_leaves_the_toast_alone() {
    // The complement: a wake sent for some other reason must not cut a toast short.
    let context = ViewContext::defaults();
    let base = ObservedBase::new(TranscriptView::new(context.clone()));
    let mut host = DialogHost::new(context, Box::new(base));
    host.toasts_mut().show(Toast::info("still fresh"));
    host.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
    assert!(
        host.toasts_mut().is_showing(),
        "an unrelated wake removed a toast that had not expired"
    );
}

#[tokio::test]
async fn views_toast_arms_exactly_one_wake_on_the_existing_terminal_channel() {
    // No new channel: the invariant is zero unbounded channels and no channel without a
    // registered capacity and policy, so the toast borrows the one that already carries
    // "look again" from producers outside the loop.
    tokio::time::pause();
    let (sender, mut receiver) = crate::app::terminal_event_channel();
    let mut layer = ToastLayer::new(ViewContext::defaults()).with_waker(sender);
    layer.show(Toast::info("armed"));
    // The spawned task has to be polled once before it registers its sleep; without this
    // `advance` moves the clock past a deadline that does not exist yet, and the sleep
    // then starts five seconds *after* the advance.
    tokio::task::yield_now().await;
    assert!(
        receiver.try_recv().is_err(),
        "the wake fired before the toast had been on screen at all"
    );
    tokio::time::advance(TOAST_TTL + std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(
        matches!(receiver.try_recv(), Ok(TerminalEvent::Wake)),
        "the armed deadline never woke the loop, so the toast would need a keypress"
    );
    assert!(
        receiver.try_recv().is_err(),
        "one toast armed more than one wake"
    );
}
