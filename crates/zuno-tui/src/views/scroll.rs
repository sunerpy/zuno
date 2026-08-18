//! Scrolling: `scroll_speed`, `scroll_acceleration`, and the accumulator that makes
//! fractional multipliers usable on a character grid.
//!
//! # The two configuration keys are mutually exclusive, and acceleration wins
//!
//! `packages/tui/src/util/scroll.ts:18-27`:
//!
//! ```text
//! if (tuiConfig?.scroll_acceleration?.enabled) return new MacOSScrollAccel()
//! if (tuiConfig?.scroll_speed !== undefined)   return new CustomSpeedScroll(tuiConfig.scroll_speed)
//! return new CustomSpeedScroll(3)
//! ```
//!
//! So a user who set both gets acceleration, and the default is a **constant three
//! lines per notch** — not one. That default is why an unconfigured TUI feels
//! responsive, and getting it wrong is a difference every user notices.
//!
//! # The acceleration curve is ported exactly, because it is not derivable
//!
//! `MacOSScrollAccel` (`@opentui/core/lib/scroll-acceleration.ts`) keeps the last
//! three inter-event intervals, converts their mean into a velocity against a 100 ms
//! reference, and applies `1 + A·(e^(v/τ) − 1)` capped at `maxMultiplier`, with
//! `A = 0.8`, `τ = 3`, `cap = 6`. Two guards matter as much as the curve:
//!
//! - a gap over `streakTimeout` (150 ms) resets the streak and returns 1;
//! - a gap under `minTickInterval` (6 ms) returns 1 **without** recording, because
//!   some terminals emit several events per physical notch and recording them would
//!   accelerate a single notch to the cap.
//!
//! # Time is a parameter
//!
//! Every method takes the timestamp. A curve that read the clock could only be tested
//! by sleeping, which is both slow and flaky.

use crate::config::{ResolvedTuiConfig, ScrollAcceleration};

#[cfg(test)]
#[path = "scroll_tests.rs"]
mod tests;

/// Lines per notch when nothing is configured (`scroll.ts:26`).
pub const DEFAULT_SCROLL_SPEED: f64 = 3.0;

/// `A` in the exponential curve.
pub const ACCEL_A: f64 = 0.8;

/// `τ` in the exponential curve.
pub const ACCEL_TAU: f64 = 3.0;

/// The multiplier ceiling.
pub const ACCEL_MAX_MULTIPLIER: f64 = 6.0;

/// Interval, in milliseconds, that ends a streak.
pub const STREAK_TIMEOUT_MS: u64 = 150;

/// Interval, in milliseconds, below which an event is a duplicate of the same notch.
pub const MIN_TICK_INTERVAL_MS: u64 = 6;

/// Intervals kept in the moving average.
pub const HISTORY_SIZE: usize = 3;

/// The interval that maps to a velocity of one.
pub const REFERENCE_INTERVAL_MS: f64 = 100.0;

/// A per-notch multiplier.
pub trait ScrollAccel: Send {
    /// The multiplier for an event at `now_ms`.
    fn tick(&mut self, now_ms: u64) -> f64;

    /// Forget the streak, e.g. after a keyboard scroll.
    fn reset(&mut self);
}

/// A constant multiplier: `scroll_speed`, or the default of three.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConstantSpeed(pub f64);

impl ScrollAccel for ConstantSpeed {
    fn tick(&mut self, _now_ms: u64) -> f64 {
        self.0
    }

    fn reset(&mut self) {}
}

/// The macOS-style accelerating multiplier.
#[derive(Debug, Clone, Default)]
pub struct MacOsAccel {
    last_tick_ms: Option<u64>,
    history: Vec<u64>,
}

impl MacOsAccel {
    /// A fresh accelerator.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_tick_ms: None,
            history: Vec::new(),
        }
    }

    /// The intervals currently in the moving average, for tests.
    #[must_use]
    pub fn history(&self) -> &[u64] {
        &self.history
    }
}

impl ScrollAccel for MacOsAccel {
    fn tick(&mut self, now_ms: u64) -> f64 {
        let Some(last) = self.last_tick_ms else {
            self.last_tick_ms = Some(now_ms);
            self.history.clear();
            return 1.0;
        };
        let delta = now_ms.saturating_sub(last);
        if delta > STREAK_TIMEOUT_MS {
            self.last_tick_ms = Some(now_ms);
            self.history.clear();
            return 1.0;
        }
        if delta < MIN_TICK_INTERVAL_MS {
            // Deliberately does not update `last_tick_ms`: the duplicate event is
            // not part of the streak, so the *next* real notch is measured against
            // the notch before it.
            return 1.0;
        }
        self.last_tick_ms = Some(now_ms);
        self.history.push(delta);
        if self.history.len() > HISTORY_SIZE {
            self.history.remove(0);
        }
        let mean = self.history.iter().sum::<u64>() as f64 / self.history.len() as f64;
        let velocity = REFERENCE_INTERVAL_MS / mean;
        let multiplier = 1.0 + ACCEL_A * ((velocity / ACCEL_TAU).exp() - 1.0);
        multiplier.min(ACCEL_MAX_MULTIPLIER)
    }

    fn reset(&mut self) {
        self.last_tick_ms = None;
        self.history.clear();
    }
}

/// Build the accelerator the configuration asks for.
///
/// The precedence is the oracle's and is cited in the module docs.
#[must_use]
pub fn accel_for(config: &ResolvedTuiConfig) -> Box<dyn ScrollAccel> {
    if matches!(
        config.scroll_acceleration,
        Some(ScrollAcceleration { enabled: true })
    ) {
        return Box::new(MacOsAccel::new());
    }
    Box::new(ConstantSpeed(
        config.scroll_speed.unwrap_or(DEFAULT_SCROLL_SPEED),
    ))
}

/// A scroll position with the fractional accumulator.
///
/// A multiplier of 1.6 lines per notch cannot move a character grid by 1.6 rows. The
/// accumulator carries the remainder so three notches at 1.6 move 4 rows rather than
/// 3 — which is what `ScrollBoxRenderable.onMouseEvent` does with
/// `Math.trunc(accumulator)`, and without it every fractional `scroll_speed` would
/// silently round to the same integer.
pub struct Scroller {
    accel: Box<dyn ScrollAccel>,
    accumulator: f64,
    offset: usize,
    /// Total rows of content.
    pub total: usize,
    /// Rows visible at once.
    pub viewport: usize,
}

impl Scroller {
    /// A scroller configured from `config`.
    #[must_use]
    pub fn new(config: &ResolvedTuiConfig) -> Self {
        Self {
            accel: accel_for(config),
            accumulator: 0.0,
            offset: 0,
            total: 0,
            viewport: 0,
        }
    }

    /// The first visible row.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// The largest legal offset.
    #[must_use]
    pub const fn max_offset(&self) -> usize {
        self.total.saturating_sub(self.viewport)
    }

    /// Whether the view is pinned to the newest content.
    ///
    /// A transcript that is at the bottom should follow a stream; one the user
    /// scrolled up should not. This is the predicate that decides.
    #[must_use]
    pub const fn is_at_bottom(&self) -> bool {
        self.offset >= self.max_offset()
    }

    /// Jump to the newest content.
    pub const fn scroll_to_bottom(&mut self) {
        self.offset = self.max_offset();
        self.accumulator = 0.0;
    }

    /// Jump to the oldest content.
    pub const fn scroll_to_top(&mut self) {
        self.offset = 0;
        self.accumulator = 0.0;
    }

    /// Adopt an offset that something else owns.
    ///
    /// The wheel is not the only thing that moves a view: a keybind, a re-measure and
    /// following a live turn all move it with no notch involved, and the surface being
    /// scrolled is what observes those. A scroller holding a private copy of the offset
    /// would apply the next notch to a stale position and yank the view back to wherever
    /// the previous gesture ended — two sources of truth, in the one place a user would
    /// read as "scrolling is broken".
    ///
    /// Deliberately leaves the accumulator and the streak alone. Content growing under
    /// a gesture does not end the gesture, and discarding sub-row carry here would make
    /// every fractional `scroll_speed` round to zero. [`Self::by_rows`] is the method
    /// for the movement that *does* end a streak.
    pub const fn sync_offset(&mut self, offset: usize) {
        let max = self.max_offset();
        self.offset = if offset > max { max } else { offset };
    }

    /// Apply one wheel event of `notches` (negative is up) at `now_ms`.
    ///
    /// Returns the rows actually moved, which is zero while the accumulator is still
    /// under one row.
    pub fn wheel(&mut self, notches: f64, now_ms: u64) -> isize {
        let multiplier = self.accel.tick(now_ms);
        self.accumulator += notches * multiplier;
        let whole = self.accumulator.trunc();
        if whole == 0.0 {
            return 0;
        }
        self.accumulator -= whole;
        let before = self.offset;
        let target = self.offset as isize + whole as isize;
        self.offset = target.clamp(0, self.max_offset() as isize) as usize;
        self.offset as isize - before as isize
    }

    /// Move by whole rows, e.g. from a keybind. Resets the streak, as upstream does
    /// (`ScrollBoxRenderable.handleKeyPress`).
    pub fn by_rows(&mut self, rows: isize) -> isize {
        self.accel.reset();
        self.accumulator = 0.0;
        let before = self.offset;
        let target = self.offset as isize + rows;
        self.offset = target.clamp(0, self.max_offset() as isize) as usize;
        self.offset as isize - before as isize
    }

    /// Move by pages, the `messages_page_up`/`messages_page_down` actions.
    pub fn by_pages(&mut self, pages: isize) -> isize {
        self.by_rows(pages * self.viewport as isize)
    }

    /// Move by half pages, the `messages_half_page_*` actions.
    pub fn by_half_pages(&mut self, pages: isize) -> isize {
        self.by_rows(pages * (self.viewport as isize / 2).max(1))
    }

    /// Re-measure the content, keeping the bottom pinned if it was pinned.
    ///
    /// The sticky-scroll behaviour: content growing under a view that is following
    /// the stream must not scroll the newest line out of sight.
    pub const fn resize(&mut self, total: usize, viewport: usize) {
        let was_at_bottom = self.is_at_bottom();
        self.total = total;
        self.viewport = viewport;
        // Pinned views follow the new bottom; an unpinned view only has to stay
        // inside the new bounds. Both land on `max_offset()` here, but the two
        // reasons are different and the pinned case is the one that must not be
        // dropped when the clamp below is next edited.
        if was_at_bottom || self.offset > self.max_offset() {
            self.offset = self.max_offset();
        }
    }
}
