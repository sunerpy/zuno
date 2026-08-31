//! The hourly garbage-collection schedule.
//!
//! `packages/opencode/src/snapshot/index.ts:761-766` forks one fiber per store:
//!
//! ```text
//! cleanup()
//!   .pipe(Effect.repeat(Schedule.spaced(Duration.hours(1))),
//!         Effect.delay(Duration.minutes(1)),
//!         Effect.forkScoped)
//! ```
//!
//! One minute after the store comes up, then once an hour, `git gc --prune=7.days`
//! runs. A failed pass is logged and the schedule continues, because a store that
//! cannot be compacted is still a store.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::Result;
use crate::store::{GcOutcome, Store};

/// The timing policy. Split out from the loop so the cadence can be asserted
/// without waiting an hour, and so tests can drive the loop at millisecond speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcSchedule {
    /// How long to wait before the first pass — `Effect.delay(1 minute)`.
    pub initial_delay: Duration,
    /// The gap between passes — `Schedule.spaced(1 hour)`.
    pub period: Duration,
}

impl GcSchedule {
    /// The oracle's cadence: first pass after one minute, then hourly.
    #[must_use]
    pub const fn hourly() -> Self {
        Self {
            initial_delay: Duration::from_secs(60),
            period: Duration::from_secs(60 * 60),
        }
    }

    /// A schedule with a custom period and no startup delay, for tests.
    #[must_use]
    pub const fn every(period: Duration) -> Self {
        Self {
            initial_delay: Duration::ZERO,
            period,
        }
    }
}

impl Default for GcSchedule {
    fn default() -> Self {
        Self::hourly()
    }
}

/// Something the schedule can collect. Implemented by [`Store`]; a trait so the
/// loop can be tested against a counter instead of a real repository.
pub trait Collect: Send + Sync + 'static {
    /// Run one garbage-collection pass.
    fn collect(&self) -> Result<GcOutcome>;
}

impl Collect for Store {
    fn collect(&self) -> Result<GcOutcome> {
        self.gc()
    }
}

/// A running schedule.
#[derive(Debug)]
pub struct GcHandle {
    stop: Arc<Notify>,
    task: JoinHandle<()>,
}

impl GcHandle {
    /// Signal the loop and wait for it to finish the pass it may be in.
    pub async fn shutdown(self) {
        self.stop.notify_one();
        let _ = self.task.await;
    }

    /// Stop the loop without waiting.
    pub fn abort(&self) {
        self.stop.notify_one();
        self.task.abort();
    }
}

/// Start collecting `target` on `schedule`.
///
/// Each pass runs on the blocking pool: `git gc` is a subprocess that can take
/// seconds on a large store, and it must not sit on a runtime worker thread.
pub fn spawn<T: Collect>(target: T, schedule: GcSchedule) -> GcHandle {
    let target = Arc::new(target);
    let stop = Arc::new(Notify::new());
    let task = tokio::spawn({
        let stop = Arc::clone(&stop);
        async move {
            if !wait(&stop, schedule.initial_delay).await {
                return;
            }
            loop {
                let pass = Arc::clone(&target);
                match tokio::task::spawn_blocking(move || pass.collect()).await {
                    Ok(Ok(outcome)) => tracing::debug!(?outcome, "snapshot gc"),
                    Ok(Err(error)) => tracing::warn!(%error, "snapshot gc failed"),
                    Err(error) => tracing::warn!(%error, "snapshot gc task failed"),
                }
                if !wait(&stop, schedule.period).await {
                    break;
                }
            }
        }
    });
    GcHandle { stop, task }
}

/// Sleep for `duration`, returning `false` if a stop was signalled first.
///
/// `biased` matters: the stop branch is polled first, so a `notify_one` permit that
/// is already waiting wins even when `duration` is zero.
async fn wait(stop: &Notify, duration: Duration) -> bool {
    tokio::select! {
        biased;
        () = stop.notified() => false,
        () = tokio::time::sleep(duration) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Counter {
        passes: Arc<AtomicUsize>,
    }

    impl Collect for Counter {
        fn collect(&self) -> Result<GcOutcome> {
            self.passes.fetch_add(1, Ordering::SeqCst);
            Ok(GcOutcome::Collected)
        }
    }

    #[test]
    fn the_default_cadence_is_the_oracle_cadence() {
        let schedule = GcSchedule::default();
        assert_eq!(schedule, GcSchedule::hourly());
        assert_eq!(schedule.initial_delay, Duration::from_secs(60));
        assert_eq!(schedule.period, Duration::from_secs(3600));
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_pass_waits_a_minute_then_repeats_hourly() {
        let passes = Arc::new(AtomicUsize::new(0));
        let handle = spawn(
            Counter {
                passes: Arc::clone(&passes),
            },
            GcSchedule::hourly(),
        );

        tokio::time::sleep(Duration::from_secs(59)).await;
        assert_eq!(passes.load(Ordering::SeqCst), 0, "not before one minute");

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(passes.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(passes.load(Ordering::SeqCst), 2, "then once an hour");

        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(passes.load(Ordering::SeqCst), 3);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_stops_the_loop() {
        let passes = Arc::new(AtomicUsize::new(0));
        let handle = spawn(
            Counter {
                passes: Arc::clone(&passes),
            },
            GcSchedule::every(Duration::from_millis(5)),
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            while passes.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the loop repeats twice even under a loaded test runner");
        handle.shutdown().await;
        let observed = passes.load(Ordering::SeqCst);
        assert!(observed >= 2, "the loop repeats: {observed} passes");

        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            observed,
            "no pass runs after shutdown"
        );
    }
}
