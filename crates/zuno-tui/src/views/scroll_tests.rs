//! Scroll tests: the configuration precedence, the ported curve, and the
//! accumulator.

use super::*;
use crate::config::{ResolvedTuiConfig, ScrollAcceleration};

fn config(speed: Option<f64>, acceleration: Option<bool>) -> ResolvedTuiConfig {
    ResolvedTuiConfig {
        scroll_speed: speed,
        scroll_acceleration: acceleration.map(|enabled| ScrollAcceleration { enabled }),
        ..ResolvedTuiConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Precedence
// ---------------------------------------------------------------------------

#[test]
fn views_scroll_default_is_precise_then_accelerates_during_a_fast_streak() {
    let mut accel = accel_for(&config(None, None));
    assert_eq!(
        accel.tick(0),
        DEFAULT_SCROLL_SPEED,
        "the first notch must remain a precise one-row movement"
    );
    assert!(
        accel.tick(50) > DEFAULT_SCROLL_SPEED,
        "rapid follow-up notches did not accelerate"
    );
}

#[test]
fn views_scroll_speed_is_used_when_configured() {
    let mut accel = accel_for(&config(Some(1.5), None));
    assert_eq!(accel.tick(0), 1.5);
}

#[test]
fn views_scroll_acceleration_wins_over_speed() {
    // `scroll.ts:19-24` checks acceleration first, so a user who set both gets it.
    let mut accel = accel_for(&config(Some(10.0), Some(true)));
    assert_eq!(
        accel.tick(0),
        1.0,
        "the constant speed was used even though acceleration is enabled"
    );
}

#[test]
fn views_scroll_acceleration_disabled_falls_through_to_speed() {
    let mut accel = accel_for(&config(Some(7.0), Some(false)));
    assert_eq!(accel.tick(0), 7.0);
}

#[test]
fn views_scroll_explicitly_disabled_acceleration_keeps_the_precise_default() {
    let mut accel = accel_for(&config(None, Some(false)));
    assert_eq!(accel.tick(0), DEFAULT_SCROLL_SPEED);
    assert_eq!(accel.tick(50), DEFAULT_SCROLL_SPEED);
}

// ---------------------------------------------------------------------------
// The curve
// ---------------------------------------------------------------------------

#[test]
fn views_scroll_accel_first_tick_is_unaccelerated() {
    let mut accel = MacOsAccel::new();
    assert_eq!(accel.tick(1_000), 1.0);
    assert!(accel.history().is_empty());
}

#[test]
fn views_scroll_accel_a_long_gap_resets_the_streak() {
    let mut accel = MacOsAccel::new();
    accel.tick(0);
    accel.tick(50);
    assert!(!accel.history().is_empty());
    assert_eq!(
        accel.tick(50 + STREAK_TIMEOUT_MS + 1),
        1.0,
        "a gap past the streak timeout still accelerated"
    );
    assert!(
        accel.history().is_empty(),
        "the streak history survived a reset"
    );
}

#[test]
fn views_scroll_accel_ignores_a_duplicate_tick_from_the_same_notch() {
    // Ghostty and others emit several events per physical notch; recording them
    // would accelerate a single notch straight to the cap.
    let mut accel = MacOsAccel::new();
    accel.tick(0);
    for offset in 1..MIN_TICK_INTERVAL_MS {
        assert_eq!(
            accel.tick(offset),
            1.0,
            "a {offset}ms duplicate was treated as a real notch"
        );
    }
    assert!(accel.history().is_empty());
}

#[test]
fn views_scroll_accel_accelerates_with_a_tighter_streak() {
    let slow = {
        let mut accel = MacOsAccel::new();
        accel.tick(0);
        accel.tick(100);
        accel.tick(200)
    };
    let fast = {
        let mut accel = MacOsAccel::new();
        accel.tick(0);
        accel.tick(20);
        accel.tick(40)
    };
    assert!(
        fast > slow,
        "a faster streak did not accelerate more ({fast} !> {slow})"
    );
    assert!(slow > 1.0, "a streak at all must accelerate above one");
}

#[test]
fn views_scroll_accel_matches_the_oracle_formula() {
    // One interval of 50 ms: velocity = 100/50 = 2, multiplier = 1 + 0.8·(e^(2/3) − 1).
    let mut accel = MacOsAccel::new();
    accel.tick(0);
    let measured = accel.tick(50);
    let expected = 1.0 + ACCEL_A * ((2.0 / ACCEL_TAU).exp() - 1.0);
    assert!(
        (measured - expected).abs() < 1e-9,
        "{measured} != {expected}"
    );
}

#[test]
fn views_scroll_accel_is_capped() {
    let mut accel = MacOsAccel::new();
    let mut now = 0;
    let mut last = 1.0;
    for _ in 0..30 {
        now += MIN_TICK_INTERVAL_MS;
        last = accel.tick(now);
    }
    assert_eq!(
        last, ACCEL_MAX_MULTIPLIER,
        "the multiplier exceeded or fell short of the ceiling"
    );
}

#[test]
fn views_scroll_accel_history_is_bounded() {
    let mut accel = MacOsAccel::new();
    let mut now = 0;
    for _ in 0..10 {
        now += 20;
        accel.tick(now);
    }
    assert_eq!(accel.history().len(), HISTORY_SIZE);
}

#[test]
fn views_scroll_accel_reset_clears_the_streak() {
    let mut accel = MacOsAccel::new();
    accel.tick(0);
    accel.tick(20);
    accel.reset();
    assert!(accel.history().is_empty());
    assert_eq!(accel.tick(40), 1.0);
}

// ---------------------------------------------------------------------------
// The accumulator
// ---------------------------------------------------------------------------

#[test]
fn views_scroll_fractional_speed_accumulates_instead_of_rounding_away() {
    // Three notches at 1.5 must move four rows. Truncating each notch would move
    // three, and every fractional speed between 1 and 2 would behave identically.
    let mut scroller = Scroller::new(&config(Some(1.5), None));
    scroller.resize(100, 10);
    scroller.scroll_to_top();
    let moved = (0..3).map(|_| scroller.wheel(1.0, 0)).sum::<isize>();
    assert_eq!(moved, 4, "the accumulator dropped a fractional remainder");
    assert_eq!(scroller.offset(), 4);
}

#[test]
fn views_scroll_a_notch_smaller_than_a_row_moves_nothing_yet() {
    let mut scroller = Scroller::new(&config(Some(0.4), None));
    scroller.resize(100, 10);
    scroller.scroll_to_top();
    assert_eq!(scroller.wheel(1.0, 0), 0);
    assert_eq!(scroller.wheel(1.0, 0), 0);
    assert_eq!(
        scroller.wheel(1.0, 0),
        1,
        "three tenths-of-a-row notches never added up to a row"
    );
}

#[test]
fn views_scroll_clamps_at_both_ends() {
    let mut scroller = Scroller::new(&config(Some(5.0), None));
    scroller.resize(30, 10);
    scroller.scroll_to_top();
    scroller.wheel(-10.0, 0);
    assert_eq!(scroller.offset(), 0, "scrolling up past the start moved");
    scroller.wheel(100.0, 0);
    assert_eq!(scroller.offset(), scroller.max_offset());
}

#[test]
fn views_scroll_max_offset_is_zero_when_everything_fits() {
    let mut scroller = Scroller::new(&config(None, None));
    scroller.resize(5, 10);
    assert_eq!(scroller.max_offset(), 0);
    assert!(scroller.is_at_bottom());
    assert_eq!(scroller.wheel(1.0, 0), 0);
}

#[test]
fn views_scroll_keyboard_movement_resets_the_streak() {
    let mut scroller = Scroller::new(&config(None, Some(true)));
    scroller.resize(200, 10);
    scroller.scroll_to_top();
    scroller.wheel(1.0, 0);
    scroller.wheel(1.0, 20);
    scroller.by_rows(1);
    // After a reset the next wheel event is the first of a new streak, so it moves
    // exactly one row rather than an accelerated amount.
    let moved = scroller.wheel(1.0, 40);
    assert_eq!(moved, 1, "the streak survived a keyboard scroll");
}

#[test]
fn views_scroll_pages_and_half_pages() {
    let mut scroller = Scroller::new(&config(None, None));
    scroller.resize(100, 20);
    scroller.scroll_to_top();
    assert_eq!(scroller.by_pages(1), 20);
    assert_eq!(scroller.offset(), 20);
    assert_eq!(scroller.by_half_pages(-1), -10);
    assert_eq!(scroller.offset(), 10);
}

#[test]
fn views_scroll_half_page_of_a_one_row_viewport_still_moves() {
    let mut scroller = Scroller::new(&config(None, None));
    scroller.resize(50, 1);
    scroller.scroll_to_top();
    assert_eq!(
        scroller.by_half_pages(1),
        1,
        "a one-row viewport made half-page scrolling a no-op"
    );
}

// ---------------------------------------------------------------------------
// Sticky bottom
// ---------------------------------------------------------------------------

#[test]
fn views_scroll_growing_content_keeps_a_pinned_view_at_the_bottom() {
    let mut scroller = Scroller::new(&config(None, None));
    scroller.resize(50, 10);
    assert!(scroller.is_at_bottom());
    scroller.resize(60, 10);
    assert!(
        scroller.is_at_bottom(),
        "a streaming transcript scrolled its newest line out of sight"
    );
    assert_eq!(scroller.offset(), 50);
}

#[test]
fn views_scroll_growing_content_leaves_a_scrolled_view_where_it_was() {
    let mut scroller = Scroller::new(&config(None, None));
    scroller.resize(50, 10);
    scroller.scroll_to_top();
    scroller.by_rows(5);
    scroller.resize(200, 10);
    assert_eq!(
        scroller.offset(),
        5,
        "growing content dragged a scrolled view along with it"
    );
    assert!(!scroller.is_at_bottom());
}

#[test]
fn views_scroll_shrinking_content_clamps_a_scrolled_view() {
    let mut scroller = Scroller::new(&config(None, None));
    scroller.resize(200, 10);
    scroller.scroll_to_top();
    scroller.by_rows(150);
    scroller.resize(40, 10);
    assert_eq!(scroller.offset(), 30);
}

#[test]
fn views_scroll_to_top_and_bottom_clear_the_accumulator() {
    let mut scroller = Scroller::new(&config(Some(0.4), None));
    scroller.resize(100, 10);
    scroller.scroll_to_top();
    scroller.wheel(1.0, 0);
    scroller.scroll_to_bottom();
    scroller.scroll_to_top();
    assert_eq!(
        scroller.wheel(1.0, 0),
        0,
        "a stale accumulator carried across a jump"
    );
}

#[test]
fn views_scroll_constant_speed_ignores_time() {
    let mut accel = ConstantSpeed(2.0);
    assert_eq!(accel.tick(0), 2.0);
    accel.reset();
    assert_eq!(accel.tick(u64::MAX), 2.0);
}
