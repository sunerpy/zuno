//! The watchdog has to be *driven*, not described.
//!
//! Its whole value is in a path that fires after 90 seconds of a busy process
//! going quiet, which no ordinary test will ever reach. So every test here builds
//! a millisecond-scale [`WatchdogConfig`], really sleeps, and asserts on reports
//! the watchdog actually emitted. Two rules follow from that:
//!
//! * **Real clock only.** The watchdog reads [`std::time::Instant`], which a
//!   virtual clock does not move. A `start_paused` runtime plus `advance` looks
//!   like it works and silently asserts nothing — the failure mode is "the fix is
//!   in place and the test is still red", which reads as the fix not working.
//! * **Every wait bounded.** Each `await`-for-a-report helper below polls with a
//!   hard deadline and fails naming what it was waiting for, so a watchdog that
//!   never reports fails the test instead of hanging the suite.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zuno_observability::watchdog::{
    ALIVE_EVERY, CHECK_EVERY, MAX_STALL_BACKOFF, MAX_THREADS_DUMPED, STALL_AFTER,
    STALL_BACKOFF_FACTOR, UNSTARTED, Watchdog, WatchdogConfig, WatchdogEvent, WatchdogReport,
    WatchdogSink,
};

/// Longest any test here waits for a report before failing.
///
/// Bounded on purpose: the defect class this repository keeps re-fixing is the
/// unbounded wait, and a liveness test that hangs is the worst possible instance
/// of it.
const DEADLINE: Duration = Duration::from_secs(10);

/// A sink that keeps every report so assertions can read them back.
#[derive(Debug, Clone, Default)]
struct Recorder {
    reports: Arc<Mutex<Vec<WatchdogReport>>>,
}

impl Recorder {
    fn events(&self) -> Vec<WatchdogEvent> {
        self.snapshot().into_iter().map(|r| r.event).collect()
    }

    fn snapshot(&self) -> Vec<WatchdogReport> {
        self.reports
            .lock()
            .expect("the recorder mutex is only locked for a clone")
            .clone()
    }

    fn count(&self, event: WatchdogEvent) -> usize {
        self.events().into_iter().filter(|e| *e == event).count()
    }

    /// Poll until `event` has been reported, or fail at [`DEADLINE`].
    fn wait_for(&self, event: WatchdogEvent) -> WatchdogReport {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            if let Some(report) = self.snapshot().into_iter().find(|r| r.event == event) {
                return report;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!(
            "no {} report within {DEADLINE:?}; observed {:?}",
            event.as_str(),
            self.events()
        );
    }
}

impl WatchdogSink for Recorder {
    fn report(&self, report: &WatchdogReport) {
        self.reports
            .lock()
            .expect("the recorder mutex is only locked for a push")
            .push(report.clone());
    }
}

/// Thresholds small enough to drive, keeping the same ordering as production:
/// one check is well under one stall, and alive is far enough out that the stall
/// tests never trip it.
fn driveable() -> WatchdogConfig {
    WatchdogConfig {
        stall_after: Duration::from_millis(60),
        check_every: Duration::from_millis(5),
        alive_every: Duration::from_secs(3_600),
        max_threads_dumped: MAX_THREADS_DUMPED,
        max_stall_backoff: MAX_STALL_BACKOFF,
    }
}

#[test]
fn watchdog_defaults_are_the_frozen_constants() {
    // Given/When: the configuration production code gets.
    let config = WatchdogConfig::default();

    // Then: it is exactly the frozen §11.2 triple, so the driveable configs used
    // by the tests below cannot quietly become the shipped thresholds.
    assert_eq!(config.stall_after, STALL_AFTER);
    assert_eq!(config.check_every, CHECK_EVERY);
    assert_eq!(config.alive_every, ALIVE_EVERY);
    assert_eq!(config.stall_after, Duration::from_secs(90));
    assert_eq!(config.check_every, Duration::from_secs(5));
    assert_eq!(config.alive_every, Duration::from_secs(300));
    assert_eq!(config.max_threads_dumped, MAX_THREADS_DUMPED);
    assert_eq!(config.max_stall_backoff, MAX_STALL_BACKOFF);
    assert!(
        config.check_every < config.stall_after,
        "a check interval at or above the stall threshold would report stalls a \
         whole threshold late"
    );
    assert!(
        config.stall_after < Duration::from_secs(120),
        "the stall threshold must stay under G4's frozen 120s progress timeout, so \
         a stalled turn is described before the gate that fails on it trips"
    );
    // The threshold alone does not establish that ordering. A stall is *noticed* at
    // the first check after it crosses `stall_after`, so the worst case is
    // `stall_after + check_every`. At the shipped 90s + 5s that is 95s, 25s ahead of
    // the gate; raising `check_every` to 40s would push it to 130s and invert the
    // ordering while the assertion above still passed. G4's review added this.
    let worst_case_detection = config.stall_after.saturating_add(config.check_every);
    assert!(
        worst_case_detection < Duration::from_secs(120),
        "a stall can go unnoticed for {worst_case_detection:?} ({:?} threshold plus one \
         {:?} check), which is past G4's frozen 120s progress timeout — the gate would \
         fail with nothing in the log to explain it",
        config.stall_after,
        config.check_every
    );
}

#[test]
fn watchdog_reports_a_stall_while_work_is_in_flight_and_no_beat_arrives() {
    // Given: a watchdog with a busy phase and no further heartbeats.
    let recorder = Recorder::default();
    let watchdog = Watchdog::spawn_with_sink(driveable(), recorder.clone());
    let phase = watchdog.phase("test.provider_request");
    let _work = watchdog.begin_work(phase);

    // When: the stall threshold passes with the guard still alive.
    let report = recorder.wait_for(WatchdogEvent::Stalled);

    // Then: the report attributes the stall to the phase that went quiet and
    // records the state needed to tell a deadlock from a busy loop.
    assert_eq!(report.phase, "test.provider_request");
    assert_eq!(report.busy, 1);
    assert!(
        report.since_beat >= Duration::from_millis(60),
        "{:?} is under the stall threshold it was reported for",
        report.since_beat
    );
    assert!(
        report.summary().contains("watchdog.stalled"),
        "{}",
        report.summary()
    );
    #[cfg(target_os = "linux")]
    {
        assert!(report.rss_kib.is_some_and(|kib| kib > 0));
        assert!(report.threads.is_some_and(|count| count >= 2));
        assert!(report.open_fds.is_some_and(|count| count > 0));
        assert!(
            !report.thread_rows.is_empty(),
            "a stall report without a thread dump cannot distinguish a futex \
             deadlock from a busy loop"
        );
        assert!(
            report.thread_rows.len() <= MAX_THREADS_DUMPED,
            "the dump cap is what keeps one stall from flooding the log"
        );
        let first = &report.thread_rows[0];
        assert_eq!(
            first.split(':').count(),
            4,
            "each row must be tid:name:state:wchan, got {first}"
        );
    }

    watchdog.shutdown();
}

#[test]
fn watchdog_stays_silent_while_nothing_is_in_flight() {
    // Given: a watchdog that is never given a work guard — the shape of a CLI
    // sitting at a prompt.
    let recorder = Recorder::default();
    let watchdog = Watchdog::spawn_with_sink(driveable(), recorder.clone());
    let phase = watchdog.phase("test.idle");
    watchdog.beat(phase);

    // When: far longer than the stall threshold passes in silence.
    std::thread::sleep(Duration::from_millis(200));

    // Then: nothing is reported. Without the BUSY gate this is the false positive
    // that would make every idle prompt look like a hang.
    assert_eq!(
        recorder.count(WatchdogEvent::Stalled),
        0,
        "idle silence was reported as a stall: {:?}",
        recorder.events()
    );
    assert_eq!(watchdog.busy(), 0);

    watchdog.shutdown();
}

#[test]
fn watchdog_reports_recovery_once_the_stalled_phase_beats_again() {
    // Given: a watchdog that has already reported a stall.
    let recorder = Recorder::default();
    let watchdog = Watchdog::spawn_with_sink(driveable(), recorder.clone());
    let phase = watchdog.phase("test.recovering");
    let work = watchdog.begin_work(phase);
    recorder.wait_for(WatchdogEvent::Stalled);

    // When: progress resumes.
    watchdog.beat(phase);
    let recovered = recorder.wait_for(WatchdogEvent::Recovered);

    // Then: recovery is recorded, so a log reader can bound the stall instead of
    // assuming the process never came back.
    assert_eq!(recovered.event.as_str(), "watchdog.recovered");
    assert_eq!(recovered.phase, "test.recovering");
    let events = recorder.events();
    let stalled_at = events
        .iter()
        .position(|e| *e == WatchdogEvent::Stalled)
        .expect("the stall was awaited above");
    let recovered_at = events
        .iter()
        .position(|e| *e == WatchdogEvent::Recovered)
        .expect("the recovery was awaited above");
    assert!(
        stalled_at < recovered_at,
        "recovery must follow the stall it ends: {events:?}"
    );

    drop(work);
    watchdog.shutdown();
}

#[test]
fn watchdog_dropping_the_work_guard_ends_the_stall_without_a_beat() {
    // Given: a stalled watchdog.
    let recorder = Recorder::default();
    let watchdog = Watchdog::spawn_with_sink(driveable(), recorder.clone());
    let phase = watchdog.phase("test.guard_drop");
    let work = watchdog.begin_work(phase);
    recorder.wait_for(WatchdogEvent::Stalled);

    // When: the work finishes rather than making progress — an early `return`, a
    // `?`, or a panic unwinding past the guard.
    drop(work);

    // Then: the stall ends. RAII, not a hand-written decrement, is what makes
    // that true on every exit path.
    let recovered = recorder.wait_for(WatchdogEvent::Recovered);
    assert_eq!(recovered.busy, 0);
    assert_eq!(watchdog.busy(), 0);

    watchdog.shutdown();
}

#[test]
fn watchdog_backs_off_instead_of_repeating_a_stall_every_check() {
    // Given: a stall that persists for many check intervals.
    let recorder = Recorder::default();
    let watchdog = Watchdog::spawn_with_sink(
        WatchdogConfig {
            stall_after: Duration::from_millis(20),
            check_every: Duration::from_millis(2),
            alive_every: Duration::from_secs(3_600),
            max_threads_dumped: 4,
            max_stall_backoff: Duration::from_millis(80),
        },
        recorder.clone(),
    );
    let phase = watchdog.phase("test.backoff");
    let _work = watchdog.begin_work(phase);
    recorder.wait_for(WatchdogEvent::Stalled);

    // When: 300ms of stall elapses — 150 check intervals.
    std::thread::sleep(Duration::from_millis(300));
    let reports = recorder.count(WatchdogEvent::Stalled);

    // Then: the count is bounded well under one-per-check. Doubling from 2ms and
    // capping at 80ms admits at most ~10 reports in that window; a watchdog
    // without backoff would emit around 150 and bury the context it exists to
    // preserve.
    assert!(
        (1..=20).contains(&reports),
        "{reports} stall reports in 300ms of a 2ms-check watchdog: backoff is not \
         limiting repeats"
    );
    const {
        assert!(
            STALL_BACKOFF_FACTOR >= 2,
            "a factor below 2 is not a backoff"
        )
    };

    watchdog.shutdown();
}

#[test]
fn watchdog_confirms_liveness_on_its_own_cadence() {
    // Given: a watchdog whose alive cadence is short and whose stall threshold is
    // long, so only the liveness path can fire.
    let recorder = Recorder::default();
    let watchdog = Watchdog::spawn_with_sink(
        WatchdogConfig {
            stall_after: Duration::from_secs(3_600),
            check_every: Duration::from_millis(2),
            alive_every: Duration::from_millis(10),
            ..WatchdogConfig::default()
        },
        recorder.clone(),
    );

    // When: the cadence elapses.
    let alive = recorder.wait_for(WatchdogEvent::Alive);

    // Then: a positive line is emitted, which is the only way a reader can tell
    // "nothing went wrong" from "the watchdog thread died".
    assert_eq!(alive.event.as_str(), "watchdog.alive");
    assert_eq!(
        recorder.count(WatchdogEvent::Stalled),
        0,
        "no work was in flight, so nothing may be reported as a stall"
    );
    assert!(
        alive.thread_rows.is_empty(),
        "routine liveness must not pay for a thread dump"
    );

    watchdog.shutdown();
}

#[test]
fn watchdog_shutdown_returns_promptly_rather_than_waiting_out_the_check_interval() {
    // Given: a watchdog whose check interval is far longer than any test may wait.
    let recorder = Recorder::default();
    let watchdog = Watchdog::spawn_with_sink(
        WatchdogConfig {
            stall_after: Duration::from_secs(3_600),
            check_every: Duration::from_secs(3_600),
            alive_every: Duration::from_secs(3_600),
            ..WatchdogConfig::default()
        },
        recorder.clone(),
    );

    // When: shutdown is requested while the thread is parked.
    let started = Instant::now();
    watchdog.shutdown();
    let elapsed = started.elapsed();

    // Then: the notify cut the park short. Without it, `shutdown` would join a
    // thread that sleeps for an hour — an unbounded wait in everything but name.
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown took {elapsed:?}; it waited out the park instead of being woken"
    );
    assert!(recorder.snapshot().is_empty());
}

#[test]
fn watchdog_phase_labels_are_interned_once_and_resolve_back() {
    // Given: a watchdog.
    let recorder = Recorder::default();
    let watchdog = Watchdog::spawn_with_sink(driveable(), recorder.clone());

    // When: the same label is registered twice and a second label once.
    let first = watchdog.phase("test.interned");
    let again = watchdog.phase("test.interned");
    let other = watchdog.phase("test.interned.other");

    // Then: interning is idempotent and each phase resolves to its own label.
    // That is what lets `beat` be two relaxed atomic stores instead of a string
    // copy on the hot path of every turn.
    assert_eq!(first, again);
    assert_ne!(first, other);
    assert_eq!(first.label(), "test.interned");
    assert_eq!(other.label(), "test.interned.other");

    watchdog.shutdown();
}

#[test]
fn watchdog_reports_the_unstarted_phase_before_anything_beats() {
    // Given: a watchdog that is made busy without ever naming a phase, which is
    // what a stall during early startup looks like.
    let recorder = Recorder::default();
    let watchdog = Watchdog::spawn_with_sink(driveable(), recorder.clone());
    let unstarted = watchdog.phase(UNSTARTED);
    let _work = watchdog.begin_work(unstarted);

    // When: it stalls.
    let report = recorder.wait_for(WatchdogEvent::Stalled);

    // Then: the phase is named rather than blank, so the report is still
    // attributable.
    assert_eq!(report.phase, UNSTARTED);

    watchdog.shutdown();
}
