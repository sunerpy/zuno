//! The memory sampler has to be *driven*, not described.
//!
//! # Why these tests exist at all
//!
//! [`zuno_observability::memory`]'s four attribution levels were implemented and had
//! fifteen passing unit tests, and a repo-wide search found no production construction of
//! a `MemoryRing` and no production call to `observe` or `report`. Every level was correct
//! and none of it could run: the alert would not fire however far resident memory grew.
//!
//! So none of these tests calls `MemoryRing::incident` — that is what the unit tests do,
//! and it is exactly the shape that hid the defect. Each one starts a real
//! [`MemorySampler`], lets its thread run against a source that grows, and asserts on
//! incidents that reached a sink. If the loop is not wired, they fail.
//!
//! Two rules follow, the same two the watchdog's tests state:
//!
//! * **Real clock only.** The sampler parks on a `Condvar` and measures with
//!   [`std::time::Instant`], neither of which a virtual clock moves. Every test here uses
//!   a millisecond-scale interval and really sleeps.
//! * **Every wait bounded.** The helpers poll to a hard deadline and fail naming what they
//!   were waiting for, so a sampler that never reports fails rather than hanging the suite.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zuno_observability::memory::{
    Attribution, CRITICAL_RSS_KIB, MAX_REPEAT_BACKOFF, MemoryIncident, MemorySample, MemorySampler,
    MemorySink, MemorySource, REPEAT_BACKOFF_FACTOR, SAMPLE_EVERY, SessionCount, Severity,
    WARNING_ACTIVE_SESSIONS, WARNING_RSS_KIB, active_sessions,
};

/// Longest any test here waits before failing.
const DEADLINE: Duration = Duration::from_secs(10);

/// Fast enough that a test finishes, slow enough that the thread really parks.
const TICK: Duration = Duration::from_millis(20);

/// A sink that keeps everything so assertions can read it back.
#[derive(Clone, Default)]
struct Recorder {
    incidents: Arc<Mutex<Vec<MemoryIncident>>>,
    recoveries: Arc<Mutex<usize>>,
}

impl Recorder {
    fn incidents(&self) -> Vec<MemoryIncident> {
        self.incidents.lock().expect("recorder lock").clone()
    }

    fn recoveries(&self) -> usize {
        *self.recoveries.lock().expect("recorder lock")
    }

    /// Wait until `predicate` holds, or fail naming `what`.
    fn wait_for(&self, what: &str, predicate: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            if predicate(self) {
                return;
            }
            std::thread::sleep(TICK / 2);
        }
        panic!(
            "waited {DEADLINE:?} for {what}; the sampler recorded {} incident(s) and {} \
             recovery(ies). An empty list means nothing is driving the ring.",
            self.incidents().len(),
            self.recoveries()
        );
    }
}

impl MemorySink for Recorder {
    fn report(&self, incident: &MemoryIncident) {
        self.incidents
            .lock()
            .expect("recorder lock")
            .push(*incident);
    }

    fn recovered(&self, _latest: &MemorySample) {
        *self.recoveries.lock().expect("recorder lock") += 1;
    }
}

/// A source that replays a scripted series of samples, holding the last one.
///
/// Scripted rather than reading real memory: a test cannot make a process grow by 2 GiB,
/// and one that waited for it would be the unbounded wait this repository keeps removing.
/// The sampler's loop, ring, thresholds, attribution, rate limiting and sink are all the
/// production ones — only where the bytes come from differs.
struct Scripted {
    samples: Vec<MemorySample>,
    next: usize,
}

impl Scripted {
    fn new(samples: Vec<MemorySample>) -> Self {
        Self { samples, next: 0 }
    }
}

impl MemorySource for Scripted {
    fn sample(&mut self, elapsed: Duration) -> Option<MemorySample> {
        let index = self.next.min(self.samples.len() - 1);
        self.next += 1;
        let mut sample = self.samples[index];
        sample.elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        Some(sample)
    }
}

fn sample(anon_kib: u64, file_kib: u64, active_sessions: u32) -> MemorySample {
    MemorySample {
        elapsed_ms: 0,
        anon_kib,
        file_kib,
        shmem_kib: 0,
        active_sessions,
    }
}

#[test]
fn a_process_past_the_warning_size_produces_a_reported_incident() {
    // The claim the whole module exists for, and the one that was false: a running process
    // over the threshold gets an alert. Asserted through a sink the sampler pushed to, so
    // a sampler that does not exist cannot pass.
    let recorder = Recorder::default();
    let sampler = MemorySampler::spawn_with(
        TICK,
        Scripted::new(vec![sample(WARNING_RSS_KIB, 4_096, 1)]),
        recorder.clone(),
    );

    recorder.wait_for("the first incident", |recorder| {
        !recorder.incidents().is_empty()
    });
    sampler.shutdown();

    let first = recorder.incidents().remove(0);
    assert_eq!(first.severity, Severity::Warning);
    assert_eq!(first.attribution, Attribution::AnonymousHeap);
    assert!(
        first.total_rss_kib >= WARNING_RSS_KIB,
        "the incident must carry the size that triggered it: {first:?}"
    );
}

#[test]
fn a_healthy_process_produces_no_incident_at_all() {
    // The other half of the claim: an alert that fires on a healthy process is as useless
    // as one that never fires, and this sampler runs for every long-lived command.
    let recorder = Recorder::default();
    let sampler = MemorySampler::spawn_with(
        TICK,
        Scripted::new(vec![sample(64 * 1024, 8 * 1024, 1)]),
        recorder.clone(),
    );
    std::thread::sleep(TICK * 8);
    sampler.shutdown();

    assert!(
        recorder.incidents().is_empty(),
        "a 72 MiB process with one session was reported: {:?}",
        recorder.incidents()
    );
    assert_eq!(
        recorder.recoveries(),
        0,
        "nothing was ever wrong to recover"
    );
}

#[test]
fn a_persisting_incident_is_rate_limited_instead_of_logged_every_sample() {
    // Its own defect if missed: at the shipped two-second cadence an unattended process
    // past 2 GiB would write the same line 43,200 times a day. Eight sampler ticks must
    // therefore not produce eight reports.
    let recorder = Recorder::default();
    let sampler = MemorySampler::spawn_with(
        TICK,
        Scripted::new(vec![sample(WARNING_RSS_KIB, 4_096, 1)]),
        recorder.clone(),
    );

    recorder.wait_for("the first incident", |recorder| {
        !recorder.incidents().is_empty()
    });
    // Long enough for eight samples; the backoff doubles from one tick, so a correct
    // sampler reports at ticks 1, 2, 4 and 8 — four at the very most, never eight.
    std::thread::sleep(TICK * 9);
    sampler.shutdown();

    let reports = recorder.incidents().len();
    assert!(
        (1..=5).contains(&reports),
        "an unchanged incident produced {reports} reports across roughly nine samples; \
         the repeat interval is not backing off"
    );
    for incident in recorder.incidents() {
        assert_eq!(incident.severity, Severity::Warning);
    }
}

#[test]
fn an_escalation_is_reported_at_once_rather_than_waiting_out_the_backoff() {
    // The backoff must not swallow new information. A warning that becomes critical is the
    // moment a human most needs the line, and it can arrive while the interval is long.
    let recorder = Recorder::default();
    let sampler = MemorySampler::spawn_with(
        TICK,
        Scripted::new(vec![
            sample(WARNING_RSS_KIB, 4_096, 1),
            sample(WARNING_RSS_KIB, 4_096, 1),
            sample(CRITICAL_RSS_KIB, 4_096, 1),
        ]),
        recorder.clone(),
    );

    recorder.wait_for("a critical incident", |recorder| {
        recorder
            .incidents()
            .iter()
            .any(|incident| incident.severity == Severity::Critical)
    });
    sampler.shutdown();

    let severities: Vec<Severity> = recorder
        .incidents()
        .iter()
        .map(|incident| incident.severity)
        .collect();
    assert_eq!(
        severities.first(),
        Some(&Severity::Warning),
        "the warning came first: {severities:?}"
    );
    assert!(
        severities.contains(&Severity::Critical),
        "the escalation was never reported: {severities:?}"
    );
}

#[test]
fn dropping_back_under_every_threshold_is_reported_once() {
    // Without this a log shows a warning and then silence, which reads exactly like a
    // sampler that died — and the reader cannot tell recovery from failure.
    let recorder = Recorder::default();
    let sampler = MemorySampler::spawn_with(
        TICK,
        Scripted::new(vec![
            sample(WARNING_RSS_KIB, 4_096, 1),
            sample(WARNING_RSS_KIB, 4_096, 1),
            sample(32 * 1024, 4_096, 1),
        ]),
        recorder.clone(),
    );

    recorder.wait_for("a recovery", |recorder| recorder.recoveries() > 0);
    std::thread::sleep(TICK * 4);
    sampler.shutdown();

    assert_eq!(
        recorder.recoveries(),
        1,
        "recovery is a transition, not a state: it must be reported once"
    );
}

#[test]
fn the_session_count_the_sampler_reads_is_the_one_a_guard_maintains() {
    // `Attribution::SessionCount` is the level that tells a busy server from a leak, and it
    // is only meaningful against a live count. The sampler reads the same process-wide
    // counter the TUI's turn driver and the server's session task increment.
    let before = active_sessions().load(Ordering::Relaxed);
    {
        let _first = SessionCount::enter();
        let _second = SessionCount::enter();
        assert_eq!(
            active_sessions().load(Ordering::Relaxed),
            before + 2,
            "two in-flight sessions must be visible to the sampler"
        );
    }
    assert_eq!(
        active_sessions().load(Ordering::Relaxed),
        before,
        "the guards must give the count back when the sessions end"
    );
}

#[test]
fn many_sessions_are_attributed_to_the_session_count_through_the_running_sampler() {
    // The level reachable only with a real count. Asserted through the sampler rather than
    // by calling `incident` directly, because a count that nothing maintains would make
    // this level unreachable in production while passing a direct unit test.
    let recorder = Recorder::default();
    let sampler = MemorySampler::spawn_with(
        TICK,
        Scripted::new(vec![sample(
            WARNING_RSS_KIB,
            4_096,
            WARNING_ACTIVE_SESSIONS,
        )]),
        recorder.clone(),
    );

    recorder.wait_for("an incident", |recorder| !recorder.incidents().is_empty());
    sampler.shutdown();

    assert_eq!(
        recorder.incidents().remove(0).attribution,
        Attribution::SessionCount,
        "with {WARNING_ACTIVE_SESSIONS} sessions the count is the explanation, not the heap"
    );
}

#[test]
fn a_sampler_that_is_shut_down_stops_reporting() {
    // A sampler outliving its process's logging is how a shutdown turns into a panic in a
    // closed writer, so `shutdown` joins rather than detaching.
    let recorder = Recorder::default();
    let sampler = MemorySampler::spawn_with(
        TICK,
        Scripted::new(vec![sample(WARNING_RSS_KIB, 4_096, 1)]),
        recorder.clone(),
    );
    recorder.wait_for("the first incident", |recorder| {
        !recorder.incidents().is_empty()
    });
    sampler.shutdown();

    let after_shutdown = recorder.incidents().len();
    std::thread::sleep(TICK * 6);
    assert_eq!(
        recorder.incidents().len(),
        after_shutdown,
        "the sampler kept reporting after it was joined"
    );
}

#[test]
fn the_shipped_cadence_and_backoff_bound_how_stale_and_how_loud_an_alert_can_be() {
    // Both directions of the rate limit, pinned against the shipped constants rather than
    // the millisecond ones the tests above use.
    assert_eq!(
        SAMPLE_EVERY,
        Duration::from_secs(2),
        "the runtime cadence matches the perf gates' process-tree sampler, so a runtime \
         trace and a gate trace describe growth at the same resolution"
    );
    assert!(
        MAX_REPEAT_BACKOFF <= Duration::from_secs(600),
        "an unchanged incident must still prove the sampler is alive at least every ten \
         minutes, or silence becomes indistinguishable from a dead thread"
    );
    let doublings_to_cap = (0..)
        .scan(SAMPLE_EVERY, |interval, _| {
            *interval = interval.saturating_mul(REPEAT_BACKOFF_FACTOR);
            Some(*interval)
        })
        .position(|interval| interval >= MAX_REPEAT_BACKOFF)
        .expect("doubling reaches the cap");
    assert!(
        doublings_to_cap <= 10,
        "reaching the {MAX_REPEAT_BACKOFF:?} cap takes {doublings_to_cap} doublings from \
         {SAMPLE_EVERY:?}; that is {doublings_to_cap} lines before the log settles"
    );
}
