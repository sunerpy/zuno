//! A transient notice, and why it costs the event loop exactly one wakeup.
//!
//! # Not a dialog, deliberately
//!
//! `§6.1` of the plan draws the line: a decision the user has to make is a dialog; a
//! fact that has already happened is a toast. A copy that succeeded is the second
//! kind, and rendering it as a modal would interrupt the typing it was meant to
//! confirm. So a toast never enters [`crate::views::dialog::DialogHost`]'s stack,
//! never takes the keyboard, and never draws a backdrop — and, because there is no
//! key to press, it has to remove itself.
//!
//! # One slot
//!
//! A single slot rather than a queue. Two toasts stacked in a corner is a log, and a
//! log already exists in the transcript; the newest fact is the one worth two rows of
//! the frame. Showing a second toast replaces the first, which also means the TTL
//! clock restarts — that is the intent, since the user has just been given something
//! newer to read.
//!
//! # How expiry reaches the screen
//!
//! The redraw scheduler ([`crate::app`]) paces frames in three tiers and only draws
//! when a component reported a change. Nothing about that machinery calls into the
//! component tree on a timer, so a toast cannot expire "by being drawn again": with no
//! further input the loop settles at 250 ms and then 5 s ticks that all decline to
//! draw, and a purely lazy toast would stay on screen until the user happened to press
//! something. Requiring a keypress to clear a notice is the behaviour this type exists
//! to avoid.
//!
//! Three shapes were weighed:
//!
//! * **Poll from the loop.** Give the schedule a "there is a toast" input and force a
//!   tier. That defeats deep idle for the toast's whole life and puts view state into
//!   the pacing decision, which is the one place it must not be — the measured tiers
//!   (`app.rs`: keystroke p50 8.572 µs, deep-idle wake 250.199 ms) are properties of
//!   input and engine activity, not of what is on screen.
//! * **Expire lazily only.** Free, and wrong for the reason above.
//! * **One deadline, one wake.** What this does: [`ToastLayer::show`] arms a single
//!   `tokio::time::sleep` for the notice's level-specific TTL that sends one
//!   [`crate::app::TerminalEvent::Wake`] on the terminal channel that already exists
//!   for out-of-loop producers. The host prunes on that event and reports a redraw, so
//!   the toast leaves the screen once and the loop returns to whatever tier it was in.
//!
//! The cost is exactly one extra loop wakeup per toast — no polling, no new channel,
//! and no interval that outlives the notice. The one honest side effect is that `Wake`
//! counts as terminal activity, so a toast expiring inside the deep-idle tier resets
//! the loop to the 250 ms idle tier for the usual 30 s. That is a timer at 4 Hz doing
//! nothing (a tick only draws when something is dirty), it happens once per toast, and
//! it is the same reset any keypress causes.
//!
//! Expiry is *also* evaluated lazily, on every event and every frame. The wake is what
//! guarantees promptness; the lazy check is what makes a dropped wake survivable. The
//! send is a `try_send` on a bounded channel, so a full queue drops the wake rather
//! than blocking a timer task — and a queue that full means events are arriving
//! anyway, which is precisely when the lazy check suffices.

use crate::app::TerminalEvent;
use crate::views::{ViewContext, display_width, message::wrap, padded};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Paragraph, Widget};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[cfg(test)]
#[path = "toast_tests.rs"]
mod tests;

/// How long a toast stays on screen.
///
/// `§6.1`'s five seconds, taken as written. Long enough to read a short sentence
/// without looking for it, short enough that it is gone before it becomes furniture.
pub const TOAST_TTL: Duration = Duration::from_secs(5);

/// How long warnings and failures stay visible.
///
/// These notices usually explain why an operation was refused and what the user can
/// change. They need enough time to read or select, unlike a short success
/// confirmation.
pub const TOAST_ATTENTION_TTL: Duration = Duration::from_secs(15);

/// The widest a toast is allowed to be.
///
/// The `medium` dialog tier ([`crate::views::dialog::DialogWidth::Medium`]) rather
/// than a number of its own, so a toast and the narrowest dialog agree about how wide
/// a short message is. Longer notices wrap into additional rows instead of losing
/// their actionable tail.
pub const TOAST_MAX_WIDTH: u16 = 60;

/// What kind of fact a toast is reporting.
///
/// `§11.5` assigns the colours: `success` for a completed operation, `warning` for a
/// refusal the user can act on, `error` for a failure, and muted text for a plain
/// statement. The four are a closed set for the same reason the permission prompt has
/// exactly three replies — a fifth state would need a fifth colour, and the palette
/// does not define one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToastLevel {
    /// A neutral statement of fact.
    Info,
    /// Something the user asked for happened.
    Success,
    /// Something was refused, and the user can do something about it.
    Warning,
    /// Something failed.
    Error,
}

impl ToastLevel {
    /// The glyph that carries the level when colour is unavailable.
    ///
    /// A colour alone is not a signal: a monochrome terminal, a colour-blind reader, and
    /// this crate's own row assertions all see the symbol and not the style. The same
    /// reason the MCP panel spells `✓ Enabled` rather than painting the row green.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Info => "·",
            Self::Success => "✓",
            Self::Warning => "!",
            Self::Error => "✗",
        }
    }

    /// How long this level remains visible.
    #[must_use]
    pub const fn ttl(self) -> Duration {
        match self {
            Self::Info | Self::Success => TOAST_TTL,
            Self::Warning | Self::Error => TOAST_ATTENTION_TTL,
        }
    }

    /// This level's style, on the inset-element background every footer uses.
    ///
    /// `background_element` rather than `background_panel`: a toast floats over whatever
    /// is behind it, and `§11.5` gives the inset shade to exactly that.
    #[must_use]
    fn style(self, context: &ViewContext) -> Style {
        let palette = context.palette();
        let foreground = match self {
            Self::Info => palette.text_muted,
            Self::Success => palette.success,
            Self::Warning => palette.warning,
            Self::Error => palette.error,
        };
        Style::new()
            .fg(foreground.into())
            .bg(palette.background_element.into())
    }
}

/// One transient notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    level: ToastLevel,
    text: String,
}

impl Toast {
    /// A notice at `level`.
    #[must_use]
    pub fn new(level: ToastLevel, text: impl Into<String>) -> Self {
        Self {
            level,
            text: text.into(),
        }
    }

    /// A neutral statement.
    #[must_use]
    pub fn info(text: impl Into<String>) -> Self {
        Self::new(ToastLevel::Info, text)
    }

    /// A completed operation.
    #[must_use]
    pub fn success(text: impl Into<String>) -> Self {
        Self::new(ToastLevel::Success, text)
    }

    /// A refusal the user can act on.
    #[must_use]
    pub fn warning(text: impl Into<String>) -> Self {
        Self::new(ToastLevel::Warning, text)
    }

    /// A failure.
    #[must_use]
    pub fn error(text: impl Into<String>) -> Self {
        Self::new(ToastLevel::Error, text)
    }

    /// This notice's level.
    #[must_use]
    pub const fn level(&self) -> ToastLevel {
        self.level
    }

    /// This notice's text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// How long this notice remains visible.
    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.level.ttl()
    }
}

/// The single-slot toast surface, drawn above everything else.
///
/// Owned by [`crate::views::dialog::DialogHost`] rather than by the base screen, and
/// that is what buys `§11.4`'s layer order — backdrop, dialog, toast. A toast the base
/// drew would be under the modal, so the copy the user just made while a picker was
/// open would be confirmed behind it.
pub struct ToastLayer {
    context: ViewContext,
    /// The notice and the instant it went up, or nothing.
    slot: Option<(Toast, Instant)>,
    /// Where a one-shot expiry wake is sent, when there is somewhere to send it.
    ///
    /// `Option` because a test renders without an event loop and must not need one to
    /// assert what a frame contains; expiry is still observable there through
    /// [`Self::prune`], which takes the instant rather than reading the clock.
    waker: Option<mpsc::Sender<TerminalEvent>>,
}

impl ToastLayer {
    /// An empty layer.
    #[must_use]
    pub const fn new(context: ViewContext) -> Self {
        Self {
            context,
            slot: None,
            waker: None,
        }
    }

    /// Send expiry wakes on `waker`.
    ///
    /// The channel is [`crate::app::terminal_event_channel`]'s existing bounded sender,
    /// not a new one: a second channel would need its own capacity and policy, and this
    /// one already exists to carry "look again" from a producer outside the loop.
    #[must_use]
    pub fn with_waker(mut self, waker: mpsc::Sender<TerminalEvent>) -> Self {
        self.waker = Some(waker);
        self
    }

    /// Put `toast` in the slot, replacing whatever was there, and arm its expiry.
    pub fn show(&mut self, toast: Toast) {
        self.show_at(toast, Instant::now());
    }

    /// [`Self::show`] with the clock supplied.
    ///
    /// The seam a test uses to place a toast in the past without sleeping. Production
    /// goes through [`Self::show`].
    pub fn show_at(&mut self, toast: Toast, now: Instant) {
        let ttl = toast.ttl();
        self.slot = Some((toast, now));
        self.arm(ttl);
    }

    /// Whether a toast is on screen.
    #[must_use]
    pub const fn is_showing(&self) -> bool {
        self.slot.is_some()
    }

    /// The toast in the slot, if any.
    #[must_use]
    pub const fn current(&self) -> Option<&Toast> {
        match &self.slot {
            Some((toast, _)) => Some(toast),
            None => None,
        }
    }

    /// Drop the toast if it has outlived its level-specific TTL by `now`, reporting
    /// whether the screen changed.
    ///
    /// `now` is a parameter rather than `Instant::now()` so expiry is assertable without
    /// a sleep. The comparison is `>=` so a toast shown at `t` is gone at `t + TTL`
    /// exactly, which is the boundary a test can name.
    pub fn prune(&mut self, now: Instant) -> bool {
        let expired = self
            .slot
            .as_ref()
            .is_some_and(|(toast, shown)| now.saturating_duration_since(*shown) >= toast.ttl());
        if expired {
            self.slot = None;
        }
        expired
    }

    /// Draw the toast in the top-right corner of `area`.
    ///
    /// Prunes first, so a frame drawn after the deadline never shows a stale notice even
    /// if the wake was dropped.
    pub fn render(&mut self, buffer: &mut ratatui::buffer::Buffer, area: Rect) {
        self.prune(Instant::now());
        let Some((toast, _)) = self.slot.as_ref() else {
            return;
        };
        if area.width == 0 || area.height == 0 {
            return;
        }
        let body = format!("{} {}", toast.level.glyph(), toast.text);
        let available = TOAST_MAX_WIDTH.min(area.width);
        let inner = available.saturating_sub(2).max(1);
        let rows = wrap(&body, inner);
        // Terminal columns, not characters: a notice naming a CJK path measured by
        // `chars().count()` would be placed one cell left of its own right edge per wide
        // glyph and paint over the frame's border.
        let wanted = rows
            .iter()
            .map(|row| display_width(row))
            .max()
            .unwrap_or(1)
            .saturating_add(2);
        let width = u16::try_from(wanted).unwrap_or(u16::MAX).min(available);
        let height = u16::try_from(rows.len())
            .unwrap_or(u16::MAX)
            .min(area.height);
        let style = toast.level.style(&self.context);
        let lines = rows
            .into_iter()
            .take(usize::from(height))
            .map(|row| padded(&format!(" {row}"), width, style))
            .collect::<Vec<_>>();
        let region = Rect {
            x: area.right() - width,
            y: area.top(),
            width,
            height,
        };
        Paragraph::new(lines).style(style).render(region, buffer);
    }

    /// Schedule the single wake that removes the current toast.
    ///
    /// Nothing happens without a waker or without a runtime — a test rendering offscreen
    /// has neither — and the task is one `sleep` that ends. Losing the send is safe: the
    /// lazy prune in [`Self::render`] and the host's per-event prune still clear the
    /// slot, just at the next event instead of on the deadline.
    fn arm(&self, ttl: Duration) {
        let Some(waker) = self.waker.clone() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            tokio::time::sleep(ttl).await;
            let _dropped_when_busy = waker.try_send(TerminalEvent::Wake);
        });
    }
}
