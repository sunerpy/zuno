//! Stopping a child this crate spawned directly, inside a bound and off the reactor.
//!
//! Two surfaces spawn a process group of their own and then abandon it at a ceiling: the
//! `shell` tool's pre-flight `git` reads and `format`'s formatter children. Both need the
//! same three things, and both got them wrong in the same way when each wrote its own.
//!
//! 1. `kill_on_drop(true)` reaches the **direct child only**. Dropping the wait future
//!    `SIGKILL`s and reaps the leader; every process the leader started — a credential
//!    helper, `fsmonitor`, a `textconv` filter, a formatter's language server, the
//!    `sleep` a wrapper script left running — survives. `process_group(0)` makes that
//!    worse rather than better: the group is no longer Zuno's, so the terminal's `SIGINT`
//!    and Zuno's own group teardown no longer reach it either. Nothing is left to reap it.
//! 2. The kill has to happen **before** the wait future is dropped. `kill_on_drop` reaps
//!    the leader, and a reaped leader's pid no longer names a group, so a kill issued
//!    afterwards is aimed at nothing (or, worse, at whatever recycled the pid).
//! 3. On Windows the kill is `taskkill /pid N /f /t` waited on with a synchronous
//!    `std::process::Command::status()`. Running that inline on a current-thread runtime
//!    is the exact blocking spawn-and-wait these ceilings exist to remove, so it goes to a
//!    blocking thread — and the wait for *that* is bounded too, or the unbounded wait comes
//!    straight back in the teardown.
//!
//! Deliberately not [`zuno_process::request_contained_process_shutdown`]: that helper is
//! for a child launched through `guarded_argv` and reads the guard state to decide what to
//! do. With a guard active — which is every production Zuno, `crates/zuno-cli/src/main.rs`
//! activates one at startup — it sends a single `SIGTERM` to the pid and trusts the guard
//! to stop and reap the group it owns. No guard owns a group spawned here.

use std::time::Duration;
use zuno_process::DirectProcessGroup;

/// The share of a phase ceiling reserved for stopping a child that did not answer.
///
/// A tenth, so the ceiling an operator configures is the **total** the phase can take
/// rather than half of it. Reusing the full ceiling for the teardown made a documented and
/// configured 30s bound behave like 60s, which is not a rounding error to whoever set it.
const TEARDOWN_SHARE: u32 = 10;

/// The least the teardown may be given, whatever the ceiling is.
///
/// A tenth of a short test ceiling is tens of microseconds, and a `spawn_blocking` hop
/// cannot be scheduled in that; the reserve would expire before the kill was even
/// attempted and leave the orphan the reserve exists to prevent.
const TEARDOWN_FLOOR: Duration = Duration::from_millis(50);

/// The most the teardown may be given.
///
/// It is one `kill(2)` on Unix and one `taskkill /t` on Windows. Three seconds is far more
/// than either needs and short enough that the reads keep essentially the whole phase.
const TEARDOWN_CAP: Duration = Duration::from_secs(3);

/// What of `phase` is reserved for the teardown of a child that did not answer.
///
/// Never more than `phase` itself: a caller may configure a ceiling shorter than the floor,
/// and a reserve larger than the phase would make [`work_window`] zero and expire every
/// read before it started.
#[must_use]
pub(crate) fn teardown_ceiling(phase: Duration) -> Duration {
    (phase / TEARDOWN_SHARE)
        .clamp(TEARDOWN_FLOOR, TEARDOWN_CAP)
        .min(phase)
}

/// What of `phase` the work itself may spend, with the teardown reserve carved out.
///
/// The pair is what makes the advertised number true: work runs inside this window, the
/// teardown inside the reserve, and `window + reserve == phase`.
#[must_use]
pub(crate) fn work_window(phase: Duration) -> Duration {
    phase.saturating_sub(teardown_ceiling(phase))
}

/// Stop every member of `group`, off the reactor and inside `ceiling`.
///
/// `what` names the child in the log line, because a warning that a teardown did not
/// complete is only actionable if it says which one.
pub(crate) async fn stop_process_group(
    group: Option<DirectProcessGroup>,
    ceiling: Duration,
    what: &'static str,
) {
    let Some(group) = group else {
        return;
    };
    match tokio::time::timeout(
        ceiling,
        tokio::task::spawn_blocking(move || group.force_kill()),
    )
    .await
    {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => {
            tracing::warn!(%error, what, "could not stop the process group of {what}");
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, what, "the teardown of {what} did not run");
        }
        Err(_) => {
            tracing::warn!(
                what,
                "the teardown of {what} did not answer within its ceiling"
            );
        }
    }
}

/// Retain the group a live child leads, when the platform can prove it leads one.
///
/// Called while the child is certainly alive, which is what makes the answer meaningful:
/// on Unix the registration validates that the pid leads its own group, and after the child
/// has been reaped its pid names nothing at all. `None` when the child has already exited
/// or does not lead a group, and then there is nothing beside it to stop.
#[must_use]
pub(crate) fn group_of(child: &tokio::process::Child) -> Option<DirectProcessGroup> {
    child
        .id()
        .and_then(|pid| DirectProcessGroup::register(pid).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reserve is carved out of the phase, never added to it.
    ///
    /// The defect this pins is arithmetic: the teardown used to be given the full ceiling
    /// *after* the phase had already spent it, so a 30s bound admitted a 60s phase.
    #[test]
    fn a_phase_and_its_teardown_together_never_exceed_the_ceiling() {
        for phase in [
            Duration::from_millis(1),
            Duration::from_millis(200),
            Duration::from_millis(300),
            Duration::from_millis(500),
            Duration::from_secs(30),
            Duration::from_secs(600),
        ] {
            assert_eq!(
                work_window(phase) + teardown_ceiling(phase),
                phase,
                "the phase bound for {phase:?} is not the total"
            );
            assert!(
                teardown_ceiling(phase) <= phase,
                "the reserve for {phase:?} is larger than the phase"
            );
        }
    }

    /// The 30s default keeps essentially all of its phase for the reads.
    #[test]
    fn the_default_ceiling_spends_its_reserve_on_the_teardown_alone() {
        assert_eq!(
            teardown_ceiling(Duration::from_secs(30)),
            Duration::from_secs(3)
        );
        assert_eq!(
            work_window(Duration::from_secs(30)),
            Duration::from_secs(27)
        );
    }

    /// A ceiling short enough that a tenth cannot schedule a thread still gets a usable
    /// reserve, and a ceiling shorter than the floor still leaves the work a window.
    #[test]
    fn a_short_ceiling_keeps_a_schedulable_reserve() {
        assert_eq!(
            teardown_ceiling(Duration::from_millis(300)),
            Duration::from_millis(50)
        );
        assert_eq!(
            work_window(Duration::from_millis(300)),
            Duration::from_millis(250)
        );
        // Shorter than the floor: the reserve collapses to the phase rather than
        // overrunning it, and the window is zero rather than negative.
        assert_eq!(
            teardown_ceiling(Duration::from_millis(10)),
            Duration::from_millis(10)
        );
        assert_eq!(work_window(Duration::from_millis(10)), Duration::ZERO);
    }
}
