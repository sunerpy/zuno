//! Frame timing: the slow-frame threshold and the bounded history behind it.
//!
//! # What a slow frame is here
//!
//! One pass of the TUI's draw path, measured around the single call that renders
//! the component tree into the terminal. It is not a scheduling interval and not
//! a keystroke latency: it is how long the process spent inside `draw`.
//!
//! # Why a threshold at all, once the frames are fast
//!
//! `crates/zuno-tui/tests/render_cost.rs` measured an unchanged 931-message frame
//! at 9.905 ms and a streaming frame at 10.501 ms, both inside the 16.67 ms active
//! redraw interval. Those numbers are the reason a threshold is useful rather
//! than the reason it is unnecessary: the same frame cost 8.269 s before the
//! highlight memo and the row cache, and nothing in the process would have said
//! so. A frame that regresses past [`SLOW_FRAME_THRESHOLD`] now describes itself.
//!
//! # Bounded by construction
//!
//! [`SlowFrameHistory`] keeps [`SLOW_FRAME_HISTORY`] records and every record is
//! fixed-size, so the entry count *is* the byte bound. That is the opposite of
//! the transcript row cache, where an entry could be arbitrarily tall and the
//! bound had to be expressed in rows; the difference is worth naming because the
//! reference implementation's 512-entry ring hid exactly that distinction.

use std::collections::VecDeque;
use std::time::Duration;

/// How long one draw may take before it is reported.
///
/// Protects against: a rendering regression that is invisible because it is still
/// fast enough not to look broken — the shape of the 8.269 s frame that shipped
/// undetected until it was measured deliberately.
///
/// 40 ms is 2.4x the 16.67 ms active redraw interval in `zuno-tui`'s `app.rs`, so
/// a frame has to miss two consecutive active slots to trip it and normal jitter
/// cannot. Against the measured frames it is 4.04x the 9.905 ms unchanged frame
/// and 3.81x the 10.501 ms streaming frame, so today's rendering has to regress
/// nearly fourfold before the log says anything. It is also the reference
/// implementation's value, which is only a coincidence worth stating: the
/// derivation above stands on this project's own measurements.
pub const SLOW_FRAME_THRESHOLD: Duration = Duration::from_millis(40);

/// Environment variable overriding [`SLOW_FRAME_THRESHOLD`], in milliseconds.
///
/// Present so a slow frame can be provoked on a machine where rendering is
/// genuinely fast, without a build that ships a lowered threshold.
pub const SLOW_FRAME_THRESHOLD_ENV: &str = "ZUNO_SLOW_FRAME_MS";

/// How many slow frames are retained for inspection.
///
/// Protects against: an unbounded diagnostic buffer in a process that runs for
/// days — the failure class `.omo/plans/memory-perf-optimization.md` exists to
/// remove rather than relocate.
///
/// Every [`SlowFrame`] is two `u64`s and a `&'static str`, 32 bytes on a 64-bit
/// target, so 64 records cost 2,048 bytes: 0.00017% of M1's 1,198,872 KiB W-real
/// median. 64 is chosen over the reference implementation's 512 because these
/// records are read to answer "did rendering just regress", and a regression
/// produces slow frames continuously — the newest few are the informative ones,
/// and the oldest 448 would only be the same answer repeated.
pub const SLOW_FRAME_HISTORY: usize = 64;

/// One draw that exceeded the threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlowFrame {
    /// Microseconds spent inside the draw.
    pub elapsed_micros: u64,
    /// Which frame this was, counted from process start.
    pub sequence: u64,
    /// What triggered the draw. Interned so recording allocates nothing.
    pub cause: &'static str,
}

impl SlowFrame {
    /// A single line carrying every field, for a sink that only takes text.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "tui.slow_frame sequence={} cause={} elapsed_micros={}",
            self.sequence, self.cause, self.elapsed_micros
        )
    }
}

/// Frame causes, interned so [`SlowFrameHistory::record`] never allocates.
pub mod cause {
    /// The frame drawn before the event loop starts.
    pub const STARTUP: &str = "startup";
    /// A frame drawn immediately for terminal input, bypassing the redraw ceiling.
    pub const TERMINAL_INPUT: &str = "terminal_input";
    /// A frame drawn by the redraw schedule after engine events set the dirty bit.
    pub const SCHEDULED: &str = "scheduled";
    /// The repaint after the terminal was reclaimed from an external program.
    pub const RECLAIM: &str = "reclaim";
}

/// Counts every frame and retains the slow ones, bounded by construction.
#[derive(Debug)]
pub struct SlowFrameHistory {
    threshold: Duration,
    capacity: usize,
    drawn: u64,
    slow: u64,
    recent: VecDeque<SlowFrame>,
}

impl SlowFrameHistory {
    /// A history using [`SLOW_FRAME_THRESHOLD`], or the environment override.
    #[must_use]
    pub fn from_environment() -> Self {
        Self::with_threshold(resolve_threshold(
            SLOW_FRAME_THRESHOLD,
            std::env::var(SLOW_FRAME_THRESHOLD_ENV).ok().as_deref(),
        ))
    }

    /// A history using an explicit threshold and the frozen capacity.
    #[must_use]
    pub fn with_threshold(threshold: Duration) -> Self {
        Self {
            threshold,
            capacity: SLOW_FRAME_HISTORY,
            drawn: 0,
            slow: 0,
            recent: VecDeque::with_capacity(SLOW_FRAME_HISTORY),
        }
    }

    /// The threshold this history compares against.
    #[must_use]
    pub const fn threshold(&self) -> Duration {
        self.threshold
    }

    /// Record one completed draw, returning it when it was slow.
    ///
    /// The return value is what a caller reports; the history is what a later
    /// question reads. Both come from one call so a frame cannot be counted
    /// without being eligible for retention.
    pub fn record(&mut self, elapsed: Duration, cause: &'static str) -> Option<SlowFrame> {
        self.drawn += 1;
        if elapsed < self.threshold {
            return None;
        }
        self.slow += 1;
        let frame = SlowFrame {
            elapsed_micros: u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            sequence: self.drawn,
            cause,
        };
        if self.recent.len() == self.capacity {
            self.recent.pop_front();
        }
        self.recent.push_back(frame);
        Some(frame)
    }

    /// How many frames were drawn.
    #[must_use]
    pub const fn drawn(&self) -> u64 {
        self.drawn
    }

    /// How many frames were slow, including ones the bound has since dropped.
    ///
    /// Counted separately from [`Self::recent`] precisely because the bound drops
    /// records: a count that could only be derived from the retained records
    /// would silently stop growing at the bound.
    #[must_use]
    pub const fn slow(&self) -> u64 {
        self.slow
    }

    /// The retained slow frames, oldest first, at most [`SLOW_FRAME_HISTORY`].
    #[must_use]
    pub fn recent(&self) -> Vec<SlowFrame> {
        self.recent.iter().copied().collect()
    }
}

impl Default for SlowFrameHistory {
    fn default() -> Self {
        Self::with_threshold(SLOW_FRAME_THRESHOLD)
    }
}

/// Report one slow frame through `tracing`.
///
/// `warn` rather than `error`: a slow frame is a degradation a user may not even
/// notice, and reserving `error` for the watchdog's stall keeps the two
/// distinguishable in a log that is read after the fact.
pub fn report(frame: &SlowFrame) {
    tracing::warn!(target: "tui.frame", "{}", frame.summary());
}

fn resolve_threshold(default: Duration, environment: Option<&str>) -> Duration {
    environment
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map_or(default, Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_under_the_threshold_is_counted_and_not_reported() {
        let mut history = SlowFrameHistory::default();

        let reported = history.record(Duration::from_millis(39), cause::SCHEDULED);

        assert!(reported.is_none());
        assert_eq!(history.drawn(), 1);
        assert_eq!(history.slow(), 0);
        assert!(history.recent().is_empty());
    }

    #[test]
    fn a_frame_at_the_threshold_is_reported_with_its_cause_and_duration() {
        let mut history = SlowFrameHistory::default();
        history.record(Duration::from_millis(1), cause::STARTUP);

        let frame = history
            .record(Duration::from_millis(41), cause::TERMINAL_INPUT)
            .expect("41 ms exceeds the 40 ms threshold");

        assert_eq!(frame.sequence, 2);
        assert_eq!(frame.cause, cause::TERMINAL_INPUT);
        assert_eq!(frame.elapsed_micros, 41_000);
        assert_eq!(
            frame.summary(),
            "tui.slow_frame sequence=2 cause=terminal_input elapsed_micros=41000"
        );
    }

    #[test]
    fn the_history_never_holds_more_than_its_bound() {
        let mut history = SlowFrameHistory::default();
        let overrun = SLOW_FRAME_HISTORY * 3;
        for _ in 0..overrun {
            history
                .record(Duration::from_millis(50), cause::SCHEDULED)
                .expect("50 ms exceeds the threshold");
        }

        let retained = history.recent();
        assert_eq!(
            retained.len(),
            SLOW_FRAME_HISTORY,
            "the history grew to {} records against a {SLOW_FRAME_HISTORY}-record bound",
            retained.len()
        );
        assert_eq!(
            history.slow(),
            overrun as u64,
            "the count stopped at the bound, so a long regression would under-report"
        );
        assert_eq!(
            retained.first().map(|frame| frame.sequence),
            Some((overrun - SLOW_FRAME_HISTORY + 1) as u64),
            "the bound dropped the newest records instead of the oldest"
        );
        assert_eq!(
            retained.last().map(|frame| frame.sequence),
            Some(overrun as u64)
        );
    }

    #[test]
    fn the_shipped_threshold_leaves_the_measured_frames_silent() {
        let measured_unchanged = Duration::from_micros(9_905);
        let measured_streaming = Duration::from_micros(10_501);
        let mut history = SlowFrameHistory::default();

        assert!(
            history
                .record(measured_unchanged, cause::SCHEDULED)
                .is_none()
        );
        assert!(
            history
                .record(measured_streaming, cause::TERMINAL_INPUT)
                .is_none()
        );
        assert_eq!(history.slow(), 0);
    }

    /// The threshold has to sit above the redraw interval, or every missed slot logs.
    #[test]
    fn the_shipped_threshold_is_above_the_active_redraw_interval() {
        let active_redraw_interval = Duration::from_nanos(1_000_000_000 / 60);
        assert!(
            SLOW_FRAME_THRESHOLD > active_redraw_interval * 2,
            "a {SLOW_FRAME_THRESHOLD:?} threshold against a {active_redraw_interval:?} interval \
             would report frames that merely missed one slot"
        );
    }

    #[test]
    fn the_threshold_override_accepts_only_positive_millisecond_counts() {
        assert_eq!(
            resolve_threshold(SLOW_FRAME_THRESHOLD, Some("5")),
            Duration::from_millis(5)
        );
        for value in [None, Some(""), Some("soon"), Some("0"), Some("-1")] {
            assert_eq!(
                resolve_threshold(SLOW_FRAME_THRESHOLD, value),
                SLOW_FRAME_THRESHOLD
            );
        }
    }

    #[test]
    fn a_retained_record_is_fixed_size_so_the_entry_count_is_the_byte_bound() {
        assert_eq!(std::mem::size_of::<SlowFrame>(), 32);
    }
}
